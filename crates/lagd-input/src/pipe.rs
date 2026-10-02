//! One device's worth of plumbing: grab the real device, mirror it onto a
//! virtual one, and move whole event frames between them on a delay.

use std::io;
use std::mem;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result};
use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{Device, EventType, InputEvent, SynchronizationCode, UinputAbsSetup};
use lagd_core::state::Stage;
use lagd_core::DelayLine;
use log::{debug, info, warn};

/// Prefix on every virtual device we create. Autodetection skips devices whose
/// name starts with it, because grabbing our own output would loop forever.
pub const VIRTUAL_PREFIX: &str = "lagd ";

/// How long the reader parks in `poll` between checks of the control plane.
/// This bounds how fast `lagd-ctl drop input` takes effect.
const POLL_MS: i32 = 20;

/// Frames in flight per device. A 1000 Hz mouse at the 500 ms ceiling needs
/// 500; the rest is headroom so a scheduling hiccup does not cost input.
const QUEUE_DEPTH: usize = 1024;

/// A delayed device: the grabbed original plus the virtual twin events come out
/// of.
pub struct Pipe {
    pub name: String,
    line: Arc<DelayLine<Vec<InputEvent>>>,
    reader: JoinHandle<()>,
    emitter: JoinHandle<()>,
}

impl Pipe {
    /// Mirrors `device`'s capabilities onto a new virtual device and starts the
    /// reader and emitter threads.
    ///
    /// `stage` is read live by both threads: the reader watches `bypass` to
    /// decide whether to hold the grab, the emitter watches the delay.
    pub fn start(
        mut device: Device,
        stage: &'static Stage,
        stop: &'static AtomicBool,
    ) -> Result<Self> {
        let name = device.name().unwrap_or("unnamed input device").to_owned();

        let virtual_device = mirror(&device, &name)
            .with_context(|| format!("building a virtual twin of {name:?}"))?;

        // Non-blocking, because the reader has to come back to the control
        // plane on a timer even when the device is idle. A blocking
        // `fetch_events` would sit on an idle keyboard forever and never
        // notice a bypass.
        device
            .set_nonblocking(true)
            .with_context(|| format!("setting {name:?} non-blocking"))?;

        let line = Arc::new(DelayLine::new(QUEUE_DEPTH));

        let reader = {
            let (line, name) = (Arc::clone(&line), name.clone());
            thread::Builder::new()
                .name(format!("lagd-read:{name}"))
                .spawn(move || read_loop(&mut device, &line, stage, stop, &name))?
        };

        let emitter = {
            let (line, name) = (Arc::clone(&line), name.clone());
            thread::Builder::new()
                .name(format!("lagd-emit:{name}"))
                .spawn(move || emit_loop(virtual_device, &line, stage, &name))?
        };

        Ok(Self {
            name,
            line,
            reader,
            emitter,
        })
    }

    /// How long the oldest undelivered frame has been queued, if any.
    #[must_use]
    pub fn oldest_age(&self) -> Option<Duration> {
        self.line.oldest_age()
    }

    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.line.dropped()
    }

    /// Lets both threads finish. The reader's `Device` drops with it, and the
    /// kernel releases the grab on close, so the real device is usable again
    /// whatever state we were in.
    pub fn join(self) {
        self.line.close();
        if self.reader.join().is_err() {
            warn!("reader thread for {:?} panicked", self.name);
        }
        if self.emitter.join().is_err() {
            warn!("emitter thread for {:?} panicked", self.name);
        }
    }
}

/// Builds a virtual device that libinput will classify the same way as the
/// original: same bus/vendor/product ids, same capability sets, same device
/// properties.
fn mirror(device: &Device, name: &str) -> Result<VirtualDevice> {
    let virtual_name = format!("{VIRTUAL_PREFIX}{name}");
    let mut builder: VirtualDeviceBuilder<'_> = VirtualDevice::builder()?
        .name(&virtual_name)
        // Carrying the original ids over matters: libinput's quirks and its
        // pointer-acceleration profiles are keyed on them, so a twin with
        // generic ids would feel different in ways unrelated to our delay.
        .input_id(device.input_id());

    if let Some(keys) = device.supported_keys() {
        builder = builder.with_keys(keys)?;
    }
    if let Some(axes) = device.supported_relative_axes() {
        builder = builder.with_relative_axes(axes)?;
    }
    if let Some(switches) = device.supported_switches() {
        builder = builder.with_switches(switches)?;
    }
    if let Some(misc) = device.misc_properties() {
        // MSC_SCAN rides along with key presses; dropping it would change what
        // clients see beyond the timing.
        builder = builder.with_msc(misc)?;
    }

    // Absolute axes have to be declared one at a time, each with its range,
    // resolution and flat/fuzz — a touchpad whose ABS_X range is wrong is
    // unusable in a way that has nothing to do with latency.
    if device.supported_absolute_axes().is_some() {
        for (axis, info) in device
            .get_absinfo()
            .context("reading absolute axis ranges")?
        {
            builder = builder.with_absolute_axis(&UinputAbsSetup::new(axis, info))?;
        }
    }

    let props = device.properties();
    if props.iter().next().is_some() {
        // INPUT_PROP_POINTER / _BUTTONPAD are what tell libinput "this is a
        // touchpad, not a tablet". Mirror them or a touchpad twin loses
        // tap-to-click and gets treated as an absolute pointer.
        builder = builder.with_properties(props)?;
    }

    // Deliberately not mirrored: LEDs and force feedback. Both flow *into* a
    // device rather than out of it, so forwarding them means a second pipe in
    // the opposite direction. The visible cost is that caps-lock and num-lock
    // indicators stop lighting while a keyboard is delayed.
    Ok(builder.build()?)
}

fn read_loop(
    device: &mut Device,
    line: &DelayLine<Vec<InputEvent>>,
    stage: &Stage,
    stop: &AtomicBool,
    name: &str,
) {
    let mut grabbed = false;
    let mut frame: Vec<InputEvent> = Vec::with_capacity(24);

    while !stop.load(Ordering::Relaxed) {
        // A bypassed stage must not merely stop delaying: it has to let go of
        // the grab so events reach the compositor by their normal path,
        // without the uinput round trip.
        let want_grab = !stage.is_bypassed();
        if want_grab != grabbed {
            match if want_grab {
                device.grab()
            } else {
                device.ungrab()
            } {
                Ok(()) => {
                    grabbed = want_grab;
                    frame.clear();
                    info!(
                        "{name:?}: {}",
                        if grabbed {
                            "grabbed, delaying"
                        } else {
                            "released, passing through untouched"
                        }
                    );
                }
                Err(err) => {
                    warn!(
                        "{name:?}: failed to {} the device: {err}",
                        if want_grab { "grab" } else { "release" }
                    );
                    // Back off rather than spin on a failing ioctl.
                    thread::sleep(Duration::from_millis(200));
                    continue;
                }
            }
        }

        match poll_readable(device.as_fd(), POLL_MS) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(err) => {
                warn!("{name:?}: poll failed: {err}");
                break;
            }
        }

        let events = match device.fetch_events() {
            Ok(events) => events,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
            Err(err) => {
                // ENODEV: the device was unplugged. Nothing to recover.
                warn!("{name:?}: read failed, giving up on this device: {err}");
                break;
            }
        };

        for event in events {
            if !grabbed {
                // Still drained, just discarded: while we are out of the path
                // the real device is delivering these itself, and re-emitting
                // them through the twin would double every keystroke.
                continue;
            }
            let end_of_frame = event.event_type() == EventType::SYNCHRONIZATION
                && event.code() == SynchronizationCode::SYN_REPORT.0;
            frame.push(event);
            if end_of_frame {
                // Whole frames are queued, never individual events: a
                // multitouch or absolute update split across two delays would
                // reach clients as a torn, self-contradictory state.
                line.push(mem::replace(&mut frame, Vec::with_capacity(24)));
            }
        }
    }

    debug!("{name:?}: reader stopped");
}

fn emit_loop(
    mut virtual_device: VirtualDevice,
    line: &DelayLine<Vec<InputEvent>>,
    stage: &Stage,
    name: &str,
) {
    // Frames already queued were captured under a grab, so they never reached
    // the compositor and must still be delivered even if the stage was dropped
    // in the meantime — `pop_due` just stops waiting on them.
    while let Some(frame) = line.pop_due(|| stage.effective()) {
        if let Err(err) = virtual_device.emit(&frame) {
            warn!("{name:?}: emitting a frame failed: {err}");
        }
    }
    debug!("{name:?}: emitter stopped");
}

/// Waits for the device to become readable, or for `timeout_ms` to pass.
fn poll_readable(fd: BorrowedFd<'_>, timeout_ms: i32) -> io::Result<bool> {
    let mut pfd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: `pfd` is a single valid pollfd and `fd` is borrowed for the
        // duration of the call.
        let rc = unsafe { libc::poll(&raw mut pfd, 1, timeout_ms) };
        if rc >= 0 {
            return Ok(rc > 0 && (pfd.revents & libc::POLLIN) != 0);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Whether a device is worth delaying, and not one of ours.
///
/// Keyboards and pointers are in; everything else (power buttons, lid
/// switches, audio jacks, the video bus) is left alone — grabbing those breaks
/// real functionality and none of it has a latency a human can feel.
#[must_use]
pub fn is_interesting(device: &Device) -> bool {
    if device.name().is_some_and(|n| n.starts_with(VIRTUAL_PREFIX)) {
        return false;
    }

    let events = device.supported_events();
    let pointing = events.contains(EventType::RELATIVE) || events.contains(EventType::ABSOLUTE);
    let typing = device.supported_keys().is_some_and(|keys| {
        // A real keyboard has letters. Power buttons and lid switches also
        // advertise EV_KEY, with one or two codes that are not these.
        keys.contains(evdev::KeyCode::KEY_A) || keys.contains(evdev::KeyCode::BTN_LEFT)
    });

    pointing || typing
}

/// The set of attributes we would hand to `with_keys`, used by `--list`.
#[must_use]
pub fn describe(device: &Device) -> String {
    let keys = device.supported_keys().map_or(0, |k| k.iter().count());
    let rel = device
        .supported_relative_axes()
        .map_or(0, |a| a.iter().count());
    let abs = device
        .supported_absolute_axes()
        .map_or(0, |a| a.iter().count());
    format!("{keys} keys, {rel} rel axes, {abs} abs axes")
}
