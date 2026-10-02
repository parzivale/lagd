//! The shared-memory control plane.
//!
//! One [`State`] lives in a file (`$XDG_RUNTIME_DIR/lagd/state` by default,
//! overridable with `LAGD_STATE`) that every component maps. The layout is
//! `#[repr(C)]` with a version word first, so a stale mapping is rejected
//! rather than silently writing at the wrong offsets.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::mem;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use memmap2::MmapMut;

/// Bumped whenever the layout of [`State`] changes.
pub const STATE_VERSION: u32 = 1;

/// Upper bound on any single stage's delay. A typo'd `5000` should not make the
/// machine feel bricked, so writers clamp rather than accept it.
pub const MAX_DELAY_US: u64 = 500_000;

/// What `toggle` turns a stage back on to when it has no remembered value.
pub const DEFAULT_RESUME_US: u64 = 50_000;

/// Which injector a set of knobs belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum StageId {
    Input,
    Audio,
    Present,
}

impl StageId {
    pub const ALL: [StageId; 3] = [StageId::Input, StageId::Audio, StageId::Present];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            StageId::Input => "input",
            StageId::Audio => "audio",
            StageId::Present => "present",
        }
    }

    fn index(self) -> usize {
        match self {
            StageId::Input => 0,
            StageId::Audio => 1,
            StageId::Present => 2,
        }
    }
}

impl fmt::Display for StageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One injector's knobs. Each stage occupies a fixed, independent slot, so a
/// write to one stage can never disturb another.
#[repr(C)]
pub struct Stage {
    /// Added delay in microseconds. Read on every event, frame and audio
    /// callback, so it is a plain relaxed load.
    delay_us: AtomicU64,
    /// Last non-zero `delay_us`, so `toggle` can put back the value that was
    /// being tested rather than a default.
    resume_us: AtomicU64,
    /// `1` takes the stage out of the signal path entirely, which is not the
    /// same as a zero delay: at zero the input stage still costs a
    /// grab/uinput round trip and the audio stage still costs a `PipeWire`
    /// quantum. Changing this is a state transition, so daemons watch it on a
    /// control thread rather than in the hot path.
    bypass: AtomicU32,
    _pad: u32,
}

impl Stage {
    /// Raw added delay, in microseconds, ignoring `bypass`.
    #[must_use]
    pub fn delay_us(&self) -> u64 {
        self.delay_us.load(Ordering::Relaxed)
    }

    /// The delay to actually apply: `None` when the stage is bypassed, which
    /// callers must treat as "remove yourself from the path", not "sleep 0".
    #[must_use]
    pub fn effective(&self) -> Option<Duration> {
        if self.is_bypassed() {
            None
        } else {
            Some(Duration::from_micros(self.delay_us()))
        }
    }

    /// Sets the delay, clamped to [`MAX_DELAY_US`]. Returns the stored value.
    pub fn set_delay_us(&self, us: u64) -> u64 {
        let us = us.min(MAX_DELAY_US);
        if us > 0 {
            self.resume_us.store(us, Ordering::Relaxed);
        }
        self.delay_us.store(us, Ordering::Relaxed);
        us
    }

    /// Adds a signed millisecond offset, saturating at 0 and [`MAX_DELAY_US`].
    pub fn adjust_ms(&self, delta_ms: i64) -> u64 {
        let magnitude = delta_ms.unsigned_abs().saturating_mul(1000);
        let cur = self.delay_us();
        let next = if delta_ms >= 0 {
            cur.saturating_add(magnitude)
        } else {
            cur.saturating_sub(magnitude)
        };
        self.set_delay_us(next)
    }

    /// Flips between zero and the remembered value. Returns the new delay.
    pub fn toggle(&self) -> u64 {
        if self.delay_us() == 0 {
            let resume = match self.resume_us.load(Ordering::Relaxed) {
                0 => DEFAULT_RESUME_US,
                us => us,
            };
            self.set_delay_us(resume)
        } else {
            // Deliberately not `set_delay_us(0)`: that would not refresh
            // `resume_us`, and we want the value we are turning off remembered.
            self.resume_us.store(self.delay_us(), Ordering::Relaxed);
            self.delay_us.store(0, Ordering::Relaxed);
            0
        }
    }

    #[must_use]
    pub fn resume_us(&self) -> u64 {
        self.resume_us.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn is_bypassed(&self) -> bool {
        self.bypass.load(Ordering::Relaxed) != 0
    }

    pub fn set_bypass(&self, bypass: bool) {
        self.bypass.store(u32::from(bypass), Ordering::Relaxed);
    }

    fn reset(&self) {
        self.delay_us.store(0, Ordering::Relaxed);
        self.resume_us.store(0, Ordering::Relaxed);
        self.bypass.store(0, Ordering::Relaxed);
    }
}

/// The whole control plane. `version` is first so a mismatched mapping is
/// detected before anything reads a field at a moved offset.
#[repr(C)]
pub struct State {
    version: AtomicU32,
    _pad: u32,
    stages: [Stage; 3],
}

impl State {
    #[must_use]
    pub fn stage(&self, id: StageId) -> &Stage {
        &self.stages[id.index()]
    }

    #[must_use]
    pub fn version(&self) -> u32 {
        self.version.load(Ordering::Acquire)
    }

    /// Zeroes every stage and publishes the version last, so a concurrent
    /// reader either sees version 0 (and waits behind the creation lock) or a
    /// fully initialised struct.
    fn init(&self) {
        for stage in &self.stages {
            stage.reset();
        }
        self.version.store(STATE_VERSION, Ordering::Release);
    }
}

/// A live mapping of the control plane. Dropping it unmaps; the file persists
/// for the lifetime of the runtime directory.
pub struct StateMap {
    // Held to keep the mapping alive; `state` points into it.
    _mmap: MmapMut,
    state: *const State,
}

// SAFETY: `State` is nothing but atomics over integers, which is exactly the
// contract for sharing it across threads *and* processes.
unsafe impl Send for StateMap {}
unsafe impl Sync for StateMap {}

impl StateMap {
    /// Path of the state file: `$LAGD_STATE` if set, else
    /// `$XDG_RUNTIME_DIR/lagd/state`, else a per-uid directory under `/tmp`
    /// for the case where there is no session bus (a bare TTY, a test).
    #[must_use]
    pub fn path() -> PathBuf {
        if let Some(p) = std::env::var_os("LAGD_STATE") {
            return PathBuf::from(p);
        }
        let dir = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(
            || {
                // SAFETY: getuid is always safe; it cannot fail.
                let uid = unsafe { libc::getuid() };
                PathBuf::from(format!("/tmp/lagd-{uid}"))
            },
            |d| PathBuf::from(d).join("lagd"),
        );
        dir.join("state")
    }

    /// Maps the control plane, creating and initialising it if needed.
    ///
    /// Creation is serialised with `flock` on a sibling lock file: without it
    /// two components starting at once could each initialise a different inode
    /// and then disagree about every delay.
    ///
    /// # Errors
    ///
    /// Fails if the runtime directory cannot be created, if the state file
    /// cannot be opened or mapped, or if the file was written by a build that
    /// speaks a different [`STATE_VERSION`].
    pub fn open_or_create() -> io::Result<Self> {
        Self::open_at(&Self::path())
    }

    /// As [`Self::open_or_create`], but at an explicit path rather than the one
    /// [`Self::path`] derives from the environment.
    ///
    /// # Errors
    ///
    /// As [`Self::open_or_create`], plus a path with no parent directory.
    ///
    /// # Panics
    ///
    /// If the kernel returns a mapping that is not aligned for [`State`].
    /// `mmap` is page-aligned by definition, so this is a sanity check rather
    /// than a reachable condition — but it guards every atomic access that
    /// follows, so it is checked rather than assumed.
    pub fn open_at(path: &Path) -> io::Result<Self> {
        let dir = path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("state path {} has no parent directory", path.display()),
            )
        })?;
        fs::create_dir_all(dir)?;

        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(dir.join(".lock"))?;
        flock_exclusive(&lock)?;

        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;

        let want = mem::size_of::<State>() as u64;
        let fresh = file.metadata()?.len() < want;
        if fresh {
            // Growing a file zero-fills, which is a valid all-stages-off State.
            file.set_len(want)?;
        }

        let mmap = unsafe { MmapMut::map_mut(&file)? };
        // A mapping starts on a page boundary, which is far stricter than
        // `State` requires; the assertion below turns that from an assumption
        // into a check.
        #[allow(clippy::cast_ptr_alignment)]
        let ptr = mmap.as_ptr().cast::<State>();
        assert!(
            ptr.cast::<u8>().align_offset(mem::align_of::<State>()) == 0,
            "mmap returned a misaligned address"
        );

        // SAFETY: the mapping is at least `size_of::<State>()` bytes, is
        // correctly aligned (asserted above), and every field of `State` is an
        // atomic integer for which all bit patterns are valid values.
        let state = unsafe { &*ptr };

        if fresh || state.version() == 0 {
            state.init();
        }
        let found = state.version();
        if found != STATE_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} has control-plane version {found}, this build speaks {STATE_VERSION} \
                     — stop the lagd services and delete the file",
                    path.display()
                ),
            ));
        }

        // The flock drops here with `lock`; the mapping outlives it.
        Ok(Self {
            _mmap: mmap,
            state: ptr,
        })
    }

    #[must_use]
    pub fn state(&self) -> &State {
        // SAFETY: as established in `open_or_create`, and `_mmap` keeps the
        // mapping alive for at least as long as `self`.
        unsafe { &*self.state }
    }
}

static SHARED: OnceLock<StateMap> = OnceLock::new();

/// Process-wide control plane, mapped on first use.
///
/// The mapping is held in a `static` for the life of the process, which is what
/// lets this hand out a `&'static State` — the Vulkan layer in particular needs
/// one it can stash in its dispatch tables.
///
/// # Errors
///
/// As [`StateMap::open_or_create`]. A failure here is not retried: callers that
/// must stay transparent on failure (the Vulkan layer) should remember that and
/// stop asking.
pub fn shared() -> io::Result<&'static State> {
    if let Some(map) = SHARED.get() {
        return Ok(map.state());
    }
    let map = StateMap::open_or_create()?;
    // A racing thread may win; its mapping is equivalent and ours is dropped.
    Ok(SHARED.get_or_init(|| map).state())
}

fn flock_exclusive(file: &File) -> io::Result<()> {
    // SAFETY: `file` owns a valid fd for the duration of the call.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fresh mappings must land on the same offsets in every component, so the
    /// layout is pinned by assertion rather than by convention.
    #[test]
    fn layout_is_pinned() {
        assert_eq!(mem::size_of::<Stage>(), 24);
        assert_eq!(mem::align_of::<State>(), 8);
        assert_eq!(mem::size_of::<State>(), 8 + 3 * 24);
    }

    /// Tests map an explicit path rather than going through `LAGD_STATE`:
    /// nextest runs them in one process, and racing `set_var` calls would make
    /// two tests fight over the same mapping.
    fn scratch(name: &str) -> StateMap {
        let dir = std::env::temp_dir().join(format!("lagd-test-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        StateMap::open_at(&dir.join("state")).unwrap()
    }

    #[test]
    fn stages_are_independent() {
        let map = scratch("independent");
        let st = map.state();
        st.stage(StageId::Input).set_delay_us(40_000);
        st.stage(StageId::Audio).set_delay_us(10_000);
        st.stage(StageId::Present).set_delay_us(20_000);

        // Dropping audio and present must leave input exactly as it was.
        st.stage(StageId::Audio).set_bypass(true);
        st.stage(StageId::Present).set_bypass(true);

        assert_eq!(st.stage(StageId::Input).delay_us(), 40_000);
        assert_eq!(
            st.stage(StageId::Input).effective(),
            Some(Duration::from_millis(40))
        );
        assert_eq!(st.stage(StageId::Audio).effective(), None);
        assert_eq!(st.stage(StageId::Present).effective(), None);
        // A bypassed stage keeps its delay, so `restore` brings it back.
        assert_eq!(st.stage(StageId::Audio).delay_us(), 10_000);
    }

    #[test]
    fn toggle_restores_the_value_under_test() {
        let map = scratch("toggle");
        let stage = map.state().stage(StageId::Present);
        stage.set_delay_us(33_000);
        assert_eq!(stage.toggle(), 0);
        assert_eq!(stage.toggle(), 33_000);
    }

    #[test]
    fn delays_are_clamped() {
        let map = scratch("clamp");
        let stage = map.state().stage(StageId::Input);
        assert_eq!(stage.set_delay_us(5_000_000), MAX_DELAY_US);
        stage.set_delay_us(5_000);
        assert_eq!(stage.adjust_ms(-50), 0);
    }
}
