//! Test harness for the input stage. Not shipped in the default package.
//!
//! Measuring an input delay needs one process to own both ends: the uinput fd
//! that *emits* the event, and the twin that *receives* it. Split across two
//! processes the measurement would be comparing two clocks and racing the
//! device-creation order, so this binary does the whole thing and prints JSON
//! for a test driver to assert on.
//!
//! Sequence:
//!
//! 1. create a synthetic keyboard, and write its event-node path where the
//!    driver can read it;
//! 2. wait for `lagd-input` to build its twin;
//! 3. check the real device is actually grabbed;
//! 4. for each requested delay, emit and time events across the pair;
//! 5. drop the stage and check the grab is released, then restore it.

use std::fs;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, Device, EventType, InputEvent, KeyCode, SynchronizationCode};
use lagd_core::state::{self, StageId};

mod vulkan;

/// Long enough for the reader's 20 ms poll and the watchdog's 50 ms tick to
/// have both seen a control-plane change.
const SETTLE: Duration = Duration::from_millis(300);

/// Gap between samples, on top of the delay, so frames cannot overlap.
const GAP: Duration = Duration::from_millis(50);

/// Must match `lagd-input`'s own `VIRTUAL_PREFIX`. Duplicated because that
/// lives in a binary crate and cannot be imported; if the daemon ever changes
/// it, this test stops finding the twin and says so.
const TWIN_PREFIX: &str = "lagd ";

#[derive(Parser)]
#[command(
    name = "lagd-probe",
    about = "Exercise the lagd stages from inside a VM test"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Measure the input stage's real latency, and check it really grabs.
    Latency(Latency),

    /// Create a Vulkan instance, device and queue, and report what worked.
    ///
    /// Run with and without `LAGD_PRESENT=1`: the layer has to leave every
    /// field of the report identical.
    Vulkan,
}

#[derive(Parser)]
struct Latency {
    /// Name for the synthetic source device. The twin is `lagd <name>`.
    #[arg(long, default_value = "lagd-probe keyboard")]
    name: String,

    /// Where to write the source device's event-node path, so the driver can
    /// pass it to `lagd-input --device`.
    #[arg(long, value_name = "FILE")]
    path_file: PathBuf,

    /// Delays to measure, in milliseconds.
    #[arg(long, value_delimiter = ',', default_values_t = [0u64, 40, 80])]
    delays: Vec<u64>,

    /// Samples per delay. The first is discarded as warm-up.
    #[arg(long, default_value_t = 20)]
    count: usize,

    /// How long to wait for `lagd-input` to create the twin.
    #[arg(long, default_value_t = 30)]
    timeout_secs: u64,
}

struct Stats {
    delay_ms: u64,
    samples: Vec<u64>,
}

impl Stats {
    fn summary(&self) -> (u64, u64, u64) {
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        let min = sorted.first().copied().unwrap_or(0);
        let max = sorted.last().copied().unwrap_or(0);
        let median = sorted.get(sorted.len() / 2).copied().unwrap_or(0);
        (min, median, max)
    }
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Latency(args) => latency(&args),
        Cmd::Vulkan => {
            let report = vulkan::probe().context("exercising Vulkan")?;
            let mut stdout = std::io::stdout().lock();
            writeln!(stdout, "{}", report.to_json()).context("writing the report")?;
            stdout.flush().context("flushing the report")?;
            Ok(())
        }
    }
}

fn latency(cli: &Latency) -> Result<()> {
    let control = state::shared().context("mapping the lagd control plane")?;
    let stage = control.stage(StageId::Input);

    // A keyboard with real letter keys, so `lagd-input`'s autodetection would
    // also accept it — the test drives it explicitly, but a device the daemon
    // would refuse is not the device we want to be measuring.
    let mut keys = AttributeSet::<KeyCode>::new();
    for key in [
        KeyCode::KEY_A,
        KeyCode::KEY_B,
        KeyCode::KEY_C,
        KeyCode::KEY_LEFTSHIFT,
    ] {
        keys.insert(key);
    }
    let mut source = VirtualDevice::builder()
        .context("opening /dev/uinput")?
        .name(&cli.name)
        .with_keys(&keys)
        .context("declaring keys on the synthetic device")?
        .build()
        .context("creating the synthetic source device")?;

    let source_path = wait_for_named(&cli.name, Duration::from_secs(5))
        .context("the synthetic device never appeared under /dev/input")?;
    fs::write(&cli.path_file, source_path.to_string_lossy().as_bytes())
        .with_context(|| format!("writing {}", cli.path_file.display()))?;
    eprintln!("source device {} ({:?})", source_path.display(), cli.name);

    // The driver starts lagd-input only once the path file exists, so this is
    // where we hand over and wait.
    let twin_name = format!("{TWIN_PREFIX}{}", cli.name);
    let twin_path = wait_for_named(&twin_name, Duration::from_secs(cli.timeout_secs))
        .with_context(|| format!("lagd-input never created a twin named {twin_name:?}"))?;
    eprintln!("twin device {} ({twin_name:?})", twin_path.display());

    // Give the daemon a moment to finish grabbing before asserting on it.
    thread::sleep(SETTLE);
    let grabbed_when_active = is_grabbed(&source_path)?;

    let mut runs = Vec::new();
    for delay_ms in &cli.delays {
        stage.set_bypass(false);
        stage.set_delay_us(delay_ms.saturating_mul(1000));
        thread::sleep(SETTLE);

        let mut twin = Device::open(&twin_path)
            .with_context(|| format!("opening the twin {}", twin_path.display()))?;
        // Non-blocking for the whole run: reads are driven by `poll`, so that a
        // frame that never arrives times out instead of parking forever.
        twin.set_nonblocking(true)
            .context("setting the twin non-blocking")?;
        // Anything the twin emitted while we were settling is not a sample.
        drain(&mut twin)?;

        let mut samples = Vec::with_capacity(cli.count);
        for i in 0..cli.count {
            let measured = measure_once(&mut source, &mut twin, *delay_ms)?;
            // The first sample pays for page faults and the twin's first read.
            if i > 0 {
                samples.push(measured);
            }
            thread::sleep(Duration::from_millis(*delay_ms) + GAP);
        }
        eprintln!("delay {delay_ms} ms: {} samples", samples.len());
        runs.push(Stats {
            delay_ms: *delay_ms,
            samples,
        });
    }

    // Dropping the stage must release the grab, not merely stop delaying: that
    // distinction is the whole reason `drop` exists alongside `set 0`.
    stage.set_bypass(true);
    thread::sleep(SETTLE);
    let grabbed_when_dropped = is_grabbed(&source_path)?;

    stage.set_bypass(false);
    thread::sleep(SETTLE);
    let grabbed_after_restore = is_grabbed(&source_path)?;

    print_json(
        &source_path,
        &twin_path,
        grabbed_when_active,
        grabbed_when_dropped,
        grabbed_after_restore,
        &runs,
    )
}

/// Emits one key-down frame on `source` and waits for it to come out of `twin`.
fn measure_once(source: &mut VirtualDevice, twin: &mut Device, delay_ms: u64) -> Result<u64> {
    let down = [
        InputEvent::new(EventType::KEY.0, KeyCode::KEY_A.0, 1),
        InputEvent::new(
            EventType::SYNCHRONIZATION.0,
            SynchronizationCode::SYN_REPORT.0,
            0,
        ),
    ];
    let up = [
        InputEvent::new(EventType::KEY.0, KeyCode::KEY_A.0, 0),
        InputEvent::new(
            EventType::SYNCHRONIZATION.0,
            SynchronizationCode::SYN_REPORT.0,
            0,
        ),
    ];

    let started = Instant::now();
    source.emit(&down).context("emitting a key-down frame")?;

    // Generous: the delay itself, plus room for a slow VM, before calling it a
    // lost event rather than a slow one.
    //
    // The wait is a `poll` with a short timeout rather than a blocking read,
    // because a blocking `fetch_events` would sit here forever if the frame
    // never came and the deadline below could never fire. `poll` wakes as soon
    // as the twin has data, so the timestamp is no less precise for it.
    let deadline = started + Duration::from_millis(delay_ms) + Duration::from_secs(5);
    let arrived = loop {
        if Instant::now() > deadline {
            bail!(
                "a key-down frame never arrived at the twin within {delay_ms} ms + 5 s — \
                 is the input stage dropped, or the twin the wrong device?"
            );
        }
        if !poll_readable(twin, 50)? {
            continue;
        }
        let mut seen = None;
        match twin.fetch_events() {
            Ok(events) => {
                for event in events {
                    if event.event_type() == EventType::KEY
                        && event.code() == KeyCode::KEY_A.0
                        && event.value() == 1
                    {
                        seen = Some(Instant::now());
                    }
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(err) => return Err(err).context("reading the twin"),
        }
        if let Some(at) = seen {
            break at;
        }
    };

    source.emit(&up).context("emitting a key-up frame")?;
    // Let the key-up through so it cannot be mistaken for the next sample.
    thread::sleep(Duration::from_millis(delay_ms) + Duration::from_millis(10));
    drain(twin)?;

    Ok(u64::try_from(arrived.duration_since(started).as_micros()).unwrap_or(u64::MAX))
}

/// Reads and discards whatever the device has queued. Expects a non-blocking
/// device.
fn drain(device: &mut Device) -> Result<()> {
    loop {
        match device.fetch_events() {
            Ok(events) => {
                if events.count() == 0 {
                    return Ok(());
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(err) => return Err(err).context("draining the twin"),
        }
    }
}

/// Waits for `device` to have data, or for `timeout_ms` to pass.
fn poll_readable(device: &Device, timeout_ms: i32) -> Result<bool> {
    let mut pfd = libc::pollfd {
        fd: device.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: a single valid pollfd, over an fd the borrowed device owns for
        // the duration of the call.
        let rc = unsafe { libc::poll(&raw mut pfd, 1, timeout_ms) };
        if rc >= 0 {
            return Ok(rc > 0 && (pfd.revents & libc::POLLIN) != 0);
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(err).context("polling the twin");
        }
    }
}

/// Whether some other process holds an exclusive grab on `path`.
///
/// Asked by trying to take the grab ourselves: `EVIOCGRAB` fails with `EBUSY`
/// when someone else holds it, which is a direct observation rather than an
/// inference from whether events are flowing.
fn is_grabbed(path: &Path) -> Result<bool> {
    let mut device = Device::open(path)
        .with_context(|| format!("opening {} to test the grab", path.display()))?;
    match device.grab() {
        Ok(()) => {
            // Nobody held it. Give it straight back: holding it ourselves would
            // break the very daemon we are measuring.
            device.ungrab().context("releasing our probe grab")?;
            Ok(false)
        }
        Err(err) if err.raw_os_error() == Some(libc::EBUSY) => Ok(true),
        Err(err) => Err(err).context("testing the grab"),
    }
}

/// Polls `/dev/input` until a device reports exactly `name`.
fn wait_for_named(name: &str, timeout: Duration) -> Result<PathBuf> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(path) = evdev::enumerate()
            .find(|(_, device)| device.name() == Some(name))
            .map(|(path, _)| path)
        {
            return Ok(path);
        }
        if Instant::now() > deadline {
            bail!("timed out waiting for an input device named {name:?}");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Hand-rolled rather than via serde: one consumer, six fields, and no reason
/// to put a serialisation stack in the dependency tree for it.
fn print_json(
    source: &Path,
    twin: &Path,
    grabbed_when_active: bool,
    grabbed_when_dropped: bool,
    grabbed_after_restore: bool,
    runs: &[Stats],
) -> Result<()> {
    let measurements: Vec<String> = runs
        .iter()
        .map(|run| {
            let (min, median, max) = run.summary();
            format!(
                r#"{{"delay_ms":{},"samples":{},"min_us":{min},"median_us":{median},"max_us":{max}}}"#,
                run.delay_ms,
                run.samples.len()
            )
        })
        .collect();

    let out = format!(
        r#"{{"source":"{}","twin":"{}","grabbed_when_active":{grabbed_when_active},"grabbed_when_dropped":{grabbed_when_dropped},"grabbed_after_restore":{grabbed_after_restore},"measurements":[{}]}}"#,
        source.display(),
        twin.display(),
        measurements.join(",")
    );

    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{out}").context("writing the result")?;
    stdout.flush().context("flushing the result")?;
    Ok(())
}
