//! `lagd-input` — deliberate latency on evdev keyboards and pointers.
//!
//! Each selected device is grabbed (`EVIOCGRAB`, so the kernel stops
//! delivering it to the compositor) and mirrored onto a uinput twin. Event
//! frames move between the two through a delay line whose length is read live
//! from the shared control plane.
//!
//! The failure mode this guards hardest against is a live-but-silent keyboard:
//! the kernel releases a grab when the fd closes, so a crash is safe, but a
//! stalled emitter is not. A watchdog therefore drops the whole stage out of
//! the path if a frame sits past its deadline.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use evdev::Device;
use lagd_core::state::{self, StageId};
use log::{info, warn};

mod pipe;

use pipe::Pipe;

#[derive(Parser)]
#[command(
    name = "lagd-input",
    version,
    about = "Delay evdev input devices through uinput twins"
)]
struct Cli {
    /// Delay this device node. Repeatable. Omitted means autodetect every
    /// keyboard and pointer.
    #[arg(long, value_name = "PATH")]
    device: Vec<PathBuf>,

    /// Seed the input stage's delay in milliseconds. Without this the stage
    /// keeps whatever `lagd-ctl` last set.
    #[arg(long, value_name = "MS")]
    delay_ms: Option<u64>,

    /// Start out of the signal path. `lagd-ctl restore input` engages it.
    #[arg(long)]
    start_dropped: bool,

    /// List the devices autodetection would pick, then exit.
    #[arg(long)]
    list: bool,

    /// Fail open if a frame sits this far past its release deadline.
    #[arg(long, value_name = "MS", default_value_t = 250)]
    watchdog_grace_ms: u64,
}

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_signum: libc::c_int) {
    // A relaxed store to a static atomic is async-signal-safe; anything that
    // allocates or logs is not.
    STOP.store(true, Ordering::Relaxed);
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();

    if cli.list {
        list_devices();
        return Ok(());
    }

    let control = state::shared().with_context(|| {
        format!(
            "mapping the lagd control plane at {}",
            state::StateMap::path().display()
        )
    })?;
    let stage = control.stage(StageId::Input);

    if let Some(ms) = cli.delay_ms {
        stage.set_delay_us(ms.saturating_mul(1000));
    }
    stage.set_bypass(cli.start_dropped);

    install_signal_handlers();

    let devices = if cli.device.is_empty() {
        autodetect()
    } else {
        open_explicit(&cli.device)?
    };
    if devices.is_empty() {
        bail!("nothing to delay. {}", diagnose_no_devices());
    }

    let mut pipes = Vec::with_capacity(devices.len());
    for device in devices {
        match Pipe::start(device, stage, &STOP) {
            Ok(pipe) => {
                info!("delaying {:?}", pipe.name);
                pipes.push(pipe);
            }
            // One unmirrorable device must not take the others down with it —
            // losing the mouse because a touchpad has an odd axis would be a
            // poor trade.
            Err(err) => warn!("skipping a device: {err:#}"),
        }
    }
    if pipes.is_empty() {
        bail!("every candidate device failed to start; see the warnings above");
    }

    info!(
        "input stage up on {} device(s), delay {} ms{}",
        pipes.len(),
        stage.delay_us() / 1000,
        if stage.is_bypassed() {
            ", dropped out of the path"
        } else {
            ""
        }
    );

    watchdog(&pipes, stage, Duration::from_millis(cli.watchdog_grace_ms));

    for pipe in pipes {
        pipe.join();
    }
    info!("input stage stopped; every real device released");
    Ok(())
}

/// Polls for a stalled emitter and for overflow, until a signal arrives.
///
/// Running this on the main thread rather than inside the per-device threads is
/// deliberate: a thread cannot notice that it is itself wedged.
fn watchdog(pipes: &[Pipe], stage: &state::Stage, grace: Duration) {
    let mut reported_drops = vec![0u64; pipes.len()];

    while !STOP.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(50));

        for (i, pipe) in pipes.iter().enumerate() {
            let dropped = pipe.dropped();
            if dropped > reported_drops[i] {
                warn!(
                    "{:?}: dropped {} frame(s) total — the emitter is not keeping up",
                    pipe.name, dropped
                );
                reported_drops[i] = dropped;
            }
        }

        let Some(delay) = stage.effective() else {
            continue; // Already out of the path; nothing to fail open from.
        };
        let budget = delay + grace;

        for pipe in pipes {
            if let Some(age) = pipe.oldest_age() {
                if age > budget {
                    warn!(
                        "{:?}: a frame has been queued {age:?}, past the {budget:?} budget — \
                         dropping the input stage out of the path so the device keeps working. \
                         Use `lagd-ctl restore input` once you know why.",
                        pipe.name
                    );
                    stage.set_bypass(true);
                    break;
                }
            }
        }
    }
}

fn install_signal_handlers() {
    for signum in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: `on_signal` is an `extern "C"` fn that only does a relaxed
        // atomic store, which is permitted in a signal handler.
        unsafe {
            libc::signal(signum, on_signal as *const () as libc::sighandler_t);
        }
    }
}

fn autodetect() -> Vec<Device> {
    evdev::enumerate()
        .filter_map(|(path, device)| {
            if pipe::is_interesting(&device) {
                info!(
                    "autodetected {} ({:?}): {}",
                    path.display(),
                    device.name().unwrap_or("unnamed"),
                    pipe::describe(&device)
                );
                Some(device)
            } else {
                None
            }
        })
        .collect()
}

fn open_explicit(paths: &[PathBuf]) -> Result<Vec<Device>> {
    paths
        .iter()
        .map(|path| Device::open(path).with_context(|| format!("opening {}", path.display())))
        .collect()
}

fn list_devices() {
    let mut seen = 0usize;
    for (path, device) in evdev::enumerate() {
        seen += 1;
        println!(
            "{:<24} {:<40} {}{}",
            path.display(),
            device.name().unwrap_or("unnamed"),
            pipe::describe(&device),
            if pipe::is_interesting(&device) {
                ""
            } else {
                "  [skipped by autodetect]"
            }
        );
    }
    if seen == 0 {
        // `evdev::enumerate` skips nodes it cannot open, so an empty list looks
        // identical to a machine with no input devices. Say which it was.
        eprintln!("{}", diagnose_no_devices());
    }
}

/// Explains an empty device list: almost always a missing group, not a missing
/// device.
fn diagnose_no_devices() -> String {
    const DIR: &str = "/dev/input";
    match fs::read_dir(DIR) {
        Err(err) => format!("no input devices: {DIR} could not be read: {err}"),
        Ok(entries) => {
            let nodes = entries
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("event"))
                .count();
            if nodes == 0 {
                format!("no input devices: {DIR} contains no event nodes")
            } else {
                format!(
                    "no readable input devices, though {DIR} has {nodes} event node(s). \
                     This user cannot open them — add it to the `input` group, and to \
                     `uinput` for the virtual devices (on NixOS: services.lagd.users)."
                )
            }
        }
    }
}
