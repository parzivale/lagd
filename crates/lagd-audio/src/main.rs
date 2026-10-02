//! `lagd-audio` — a `PipeWire` virtual sink that delays what you hear.
//!
//! pipewire-rs exposes `pw_stream` but not `pw_filter`, so the stage is built
//! from two streams sharing one main loop: a `media.class = Audio/Sink` node
//! that clients write into, and a playback node that feeds the real sink. A
//! ring buffer between them holds the delay.
//!
//! Dropping this stage is not a matter of setting the delay to zero — at zero
//! the audio still crosses a `PipeWire` quantum on its way through us. So
//! `bypass` disconnects both streams, which takes the virtual sink out of the
//! graph and lets `WirePlumber` move clients back to the real default sink.

mod ring;

use std::cell::{Cell, RefCell};
use std::io::Cursor;
use std::mem;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use lagd_core::state::{self, Stage, StageId};
use log::{info, warn};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use spa::pod::Pod;

use ring::DelayRing;

/// `f32` samples only: fixing the sample format removes a conversion path from
/// the delay stage, and the adapter `PipeWire` wraps an `Audio/Sink` node in
/// converts for clients that want something else.
const SAMPLE_BYTES: usize = mem::size_of::<f32>();

/// How often the non-real-time side re-reads the shared control plane.
const CONTROL_TICK: Duration = Duration::from_millis(50);

#[derive(Parser)]
#[command(
    name = "lagd-audio",
    version,
    about = "A PipeWire virtual sink that delays audio on its way to the real one"
)]
struct Cli {
    /// Sink to forward delayed audio to, by node name. Strongly recommended:
    /// without it the output autoconnects to the default sink, which is a
    /// feedback loop if that default is lagd itself.
    #[arg(long, value_name = "NODE_NAME")]
    target: Option<String>,

    /// Name the virtual sink appears under.
    #[arg(long, default_value = "lagd")]
    node_name: String,

    /// Description shown in volume mixers.
    #[arg(long, default_value = "lagd (delayed output)")]
    description: String,

    #[arg(long, default_value_t = 48_000, value_name = "HZ")]
    rate: u32,

    #[arg(long, default_value_t = 2)]
    channels: u32,

    /// Seed the audio stage's delay in milliseconds.
    #[arg(long, value_name = "MS")]
    delay_ms: Option<u64>,

    /// Start out of the graph. `lagd-ctl restore audio` brings the sink up.
    #[arg(long)]
    start_dropped: bool,

    /// Crossfade length applied when the delay changes. Shorter is tighter but
    /// more likely to click.
    #[arg(long, default_value_t = 10, value_name = "MS")]
    fade_ms: u64,
}

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_signum: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

/// The negotiated format plus the ring sized for it.
struct Audio {
    ring: DelayRing,
    rate: u32,
    channels: usize,
    fade_ms: u64,
}

impl Audio {
    fn new(rate: u32, channels: usize, fade_ms: u64) -> Self {
        let mut audio = Self {
            ring: DelayRing::new(channels, 1, 1),
            rate,
            channels,
            fade_ms,
        };
        audio.resize();
        audio
    }

    fn resize(&mut self) {
        let max_delay_frames = frames_for(state::MAX_DELAY_US, self.rate);
        let fade_frames = frames_for(self.fade_ms.saturating_mul(1000), self.rate).max(1);
        self.ring = DelayRing::new(self.channels, max_delay_frames, fade_frames);
    }

    /// Rebuilds the ring if the graph negotiated something other than what we
    /// asked for. Keeping the old ring would mean interleaving at the wrong
    /// stride, which is noise rather than delayed audio.
    fn reconfigure(&mut self, rate: u32, channels: usize) {
        if rate == self.rate && channels == self.channels {
            return;
        }
        info!(
            "format changed to {rate} Hz / {channels}ch (was {} Hz / {}ch); resizing the ring",
            self.rate, self.channels
        );
        self.rate = rate;
        self.channels = channels;
        self.resize();
    }

    fn stride(&self) -> usize {
        self.channels * SAMPLE_BYTES
    }
}

type Shared = Rc<RefCell<Audio>>;

/// Both streams, and whether they are currently in the graph.
struct Plumbing {
    sink: pw::stream::StreamRc,
    out: pw::stream::StreamRc,
    format: Vec<u8>,
    connected: Cell<bool>,
}

impl Plumbing {
    fn connect(&self) -> Result<()> {
        // The sink does not autoconnect: clients connect *to* it. The output
        // does, so delayed audio reaches the real device.
        let mut params = [pod(&self.format)?];
        self.sink
            .connect(
                spa::utils::Direction::Input,
                None,
                pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
                &mut params,
            )
            .context("connecting the virtual sink")?;

        let mut params = [pod(&self.format)?];
        self.out
            .connect(
                spa::utils::Direction::Output,
                None,
                pw::stream::StreamFlags::AUTOCONNECT
                    | pw::stream::StreamFlags::MAP_BUFFERS
                    | pw::stream::StreamFlags::RT_PROCESS,
                &mut params,
            )
            .context("connecting the playback output")?;

        self.connected.set(true);
        Ok(())
    }

    /// Takes both nodes out of the graph. Order matters: dropping the output
    /// first means nothing is still trying to pull from a sink that is going
    /// away.
    fn disconnect(&self) {
        if let Err(err) = self.out.disconnect() {
            warn!("disconnecting the playback output: {err}");
        }
        if let Err(err) = self.sink.disconnect() {
            warn!("disconnecting the virtual sink: {err}");
        }
        self.connected.set(false);
    }
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();

    let control = state::shared().with_context(|| {
        format!(
            "mapping the lagd control plane at {}",
            state::StateMap::path().display()
        )
    })?;
    let stage = control.stage(StageId::Audio);
    if let Some(ms) = cli.delay_ms {
        stage.set_delay_us(ms.saturating_mul(1000));
    }
    stage.set_bypass(cli.start_dropped);

    install_signal_handlers();

    if cli.target.is_none() {
        warn!(
            "no --target given: the delayed output will autoconnect to the default sink. \
             If {:?} is the default sink that is a feedback loop — pass the real device's \
             node name.",
            cli.node_name
        );
    }

    pw::init();
    let mainloop =
        pw::main_loop::MainLoopRc::new(None).context("creating the PipeWire main loop")?;
    let context =
        pw::context::ContextRc::new(&mainloop, None).context("creating a PipeWire context")?;
    let core = context.connect_rc(None).context("connecting to PipeWire")?;

    let audio: Shared = Rc::new(RefCell::new(Audio::new(
        cli.rate,
        cli.channels as usize,
        cli.fade_ms,
    )));

    let sink = pw::stream::StreamRc::new(core.clone(), &cli.node_name, sink_props(&cli))
        .context("creating the virtual sink stream")?;
    let out = pw::stream::StreamRc::new(core.clone(), "lagd-out", out_props(&cli))
        .context("creating the playback stream")?;

    // Listeners must outlive the loop, so they are bound here rather than in
    // the functions that register them.
    let _sink_listener = register_sink(&sink, &audio)?;
    let _out_listener = register_out(&out, &audio, stage)?;

    let plumbing = Rc::new(Plumbing {
        sink,
        out,
        format: format_pod(cli.rate, cli.channels)?,
        connected: Cell::new(false),
    });

    if stage.is_bypassed() {
        info!("audio stage starting dropped; `lagd-ctl restore audio` brings the sink up");
    } else {
        plumbing.connect()?;
    }

    let _control_timer = arm_control(&mainloop, &plumbing, &audio, stage);

    info!(
        "audio stage up: sink {:?} -> {}, {} Hz / {}ch, delay {} ms",
        cli.node_name,
        cli.target.as_deref().unwrap_or("(default sink)"),
        cli.rate,
        cli.channels,
        stage.delay_us() / 1000
    );

    mainloop.run();

    plumbing.disconnect();
    info!("audio stage stopped");
    Ok(())
}

fn sink_props(cli: &Cli) -> pw::properties::PropertiesBox {
    properties! {
        *pw::keys::MEDIA_CLASS => "Audio/Sink",
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::NODE_NAME => cli.node_name.as_str(),
        *pw::keys::NODE_DESCRIPTION => cli.description.as_str(),
        *pw::keys::NODE_VIRTUAL => "true",
        *pw::keys::AUDIO_RATE => cli.rate.to_string(),
        *pw::keys::AUDIO_CHANNELS => cli.channels.to_string(),
    }
}

fn out_props(cli: &Cli) -> pw::properties::PropertiesBox {
    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_ROLE => "Production",
        *pw::keys::MEDIA_CLASS => "Stream/Output/Audio",
        *pw::keys::NODE_DESCRIPTION => "lagd delayed output",
    };
    if let Some(target) = &cli.target {
        props.insert(*pw::keys::TARGET_OBJECT, target.as_str());
    }
    props
}

/// Registers the virtual sink: format negotiation, and the capture half of the
/// delay.
///
/// Everything clients write goes into the ring at a frame position rather than
/// against a clock, so the delay is exact in frames.
fn register_sink(
    sink: &pw::stream::StreamRc,
    audio: &Shared,
) -> Result<pw::stream::StreamListener<Shared>> {
    sink.add_local_listener_with_user_data(Rc::clone(audio))
        .param_changed(|_, audio, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = spa::param::format_utils::parse_format(param)
            else {
                return;
            };
            if media_type != spa::param::format::MediaType::Audio
                || media_subtype != spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            let mut info = spa::param::audio::AudioInfoRaw::new();
            if info.parse(param).is_err() {
                warn!("could not parse the negotiated audio format");
                return;
            }
            audio
                .borrow_mut()
                .reconfigure(info.rate(), info.channels() as usize);
        })
        .state_changed(|_, _, old, new| {
            info!("virtual sink: {old:?} -> {new:?}");
        })
        .process(|stream, audio| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                // Out of buffers means the graph is behind us; dropping this
                // cycle is the only option and beats blocking in RT context.
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let (offset, size) = {
                let chunk = data.chunk();
                (chunk.offset() as usize, chunk.size() as usize)
            };
            let Some(bytes) = data.data() else { return };
            let end = offset.saturating_add(size).min(bytes.len());
            if offset >= end {
                return;
            }
            audio.borrow_mut().ring.push_le_bytes(&bytes[offset..end]);
        })
        .register()
        .context("registering the virtual sink listener")
}

/// Registers the playback side, where audio comes back out `delay` behind.
fn register_out(
    out: &pw::stream::StreamRc,
    audio: &Shared,
    stage: &'static Stage,
) -> Result<pw::stream::StreamListener<(Shared, &'static Stage)>> {
    out.add_local_listener_with_user_data((Rc::clone(audio), stage))
        .state_changed(|_, _, old, new| {
            info!("delayed output: {old:?} -> {new:?}");
        })
        .process(|stream, (audio, stage)| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let requested = buffer.requested() as usize;
            let mut audio = audio.borrow_mut();
            let stride = audio.stride();

            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };

            let frames = {
                let Some(slice) = data.data() else { return };
                let capacity = slice.len() / stride;
                if requested == 0 {
                    capacity
                } else {
                    requested.min(capacity)
                }
            };

            if frames > 0 {
                // A bypassed stage still has to produce audio until the control
                // tick disconnects us; passing it through undelayed is the
                // honest thing to do in that window.
                let delay_frames = stage
                    .effective()
                    .map_or(0, |d| frames_for(d.as_micros() as u64, audio.rate));
                if let Some(slice) = data.data() {
                    audio
                        .ring
                        .pull_le_bytes(&mut slice[..frames * stride], frames, delay_frames);
                }
            }

            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = i32::try_from(stride).unwrap_or(i32::MAX);
            *chunk.size_mut() = u32::try_from(frames * stride).unwrap_or(u32::MAX);
        })
        .register()
        .context("registering the playback listener")
}

/// Arms the one timer that drives everything outside real-time context:
/// engaging and dropping the stage, republishing our latency, and noticing a
/// shutdown signal.
fn arm_control<'l>(
    mainloop: &'l pw::main_loop::MainLoopRc,
    plumbing: &Rc<Plumbing>,
    audio: &Shared,
    stage: &'static Stage,
) -> pw::loop_::TimerSource<'l> {
    let plumbing = Rc::clone(plumbing);
    let audio = Rc::clone(audio);
    let weak_loop = mainloop.downgrade();
    // The loop takes an `Fn` callback, so the timer's own bookkeeping lives in
    // cells rather than in captured `mut` locals.
    let last_reported_us = Cell::new(u64::MAX);
    let last_underruns = Cell::new(0u64);

    let timer = mainloop.loop_().add_timer(move |_| {
        if STOP.load(Ordering::Relaxed) {
            if let Some(mainloop) = weak_loop.upgrade() {
                mainloop.quit();
            }
            return;
        }

        let want_connected = !stage.is_bypassed();
        if want_connected != plumbing.connected.get() {
            if want_connected {
                match plumbing.connect() {
                    Ok(()) => info!("audio stage back in the graph"),
                    Err(err) => warn!("could not re-enter the graph: {err:#}"),
                }
            } else {
                plumbing.disconnect();
                info!("audio stage dropped out of the graph; clients will move to the real sink");
            }
        }

        // Tell the graph how much latency we add, so anything doing A/V sync
        // compensates instead of fighting us.
        let delay_us = stage.effective().map_or(0, |d| d.as_micros() as u64);
        if delay_us != last_reported_us.get() {
            last_reported_us.set(delay_us);
            if let Err(err) = publish_latency(&plumbing, delay_us) {
                warn!("could not publish our process latency: {err:#}");
            }
        }

        let underruns = audio.borrow().ring.underruns();
        if underruns > last_underruns.get() {
            warn!("ring underran {underruns} time(s) — the delay outran the buffer");
            last_underruns.set(underruns);
        }
    });
    timer.update_timer(Some(CONTROL_TICK), Some(CONTROL_TICK));
    timer
}

fn install_signal_handlers() {
    for signum in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: the handler performs only a relaxed atomic store, which is
        // async-signal-safe.
        unsafe {
            libc::signal(signum, on_signal as *const () as libc::sighandler_t);
        }
    }
}

/// Microseconds to whole frames at `rate`.
fn frames_for(us: u64, rate: u32) -> usize {
    (us.saturating_mul(u64::from(rate)) / 1_000_000) as usize
}

fn pod(bytes: &[u8]) -> Result<&Pod> {
    Pod::from_bytes(bytes).context("the serialised pod is malformed")
}

/// The one format we offer: interleaved `f32` at the requested rate.
fn format_pod(rate: u32, channels: u32) -> Result<Vec<u8>> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(rate);
    info.set_channels(channels);

    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    if channels >= 1 {
        position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    }
    if channels >= 2 {
        position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    }
    info.set_position(position);

    let values = spa::pod::serialize::PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::sys::SPA_TYPE_OBJECT_Format,
            id: spa::sys::SPA_PARAM_EnumFormat,
            properties: info.into(),
        }),
    )
    .context("serialising the audio format")?
    .0
    .into_inner();
    Ok(values)
}

/// Publishes our added latency as a `ProcessLatency` param on both nodes.
fn publish_latency(plumbing: &Plumbing, delay_us: u64) -> Result<()> {
    let nanoseconds = i64::try_from(delay_us.saturating_mul(1000)).unwrap_or(i64::MAX);
    let values = spa::pod::serialize::PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::sys::SPA_TYPE_OBJECT_ParamProcessLatency,
            id: spa::sys::SPA_PARAM_ProcessLatency,
            properties: vec![spa::pod::Property::new(
                spa::sys::SPA_PARAM_PROCESS_LATENCY_ns,
                spa::pod::Value::Long(nanoseconds),
            )],
        }),
    )
    .context("serialising the latency param")?
    .0
    .into_inner();

    let mut params = [pod(&values)?];
    plumbing
        .sink
        .update_params(&mut params)
        .context("updating the sink's latency param")?;
    let mut params = [pod(&values)?];
    plumbing
        .out
        .update_params(&mut params)
        .context("updating the output's latency param")?;
    Ok(())
}
