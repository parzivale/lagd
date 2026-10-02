//! `lagd-ctl` — dial the three latency stages independently, while they run.
//!
//! Every stage has its own slot in the shared control plane, so dropping audio
//! and video never touches the input delay. That separation is the whole point:
//! it is what lets you hold input latency fixed and A/B the other two against
//! it.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use lagd_core::state::{Stage, StageId, State, StateMap, MAX_DELAY_US};

#[derive(Parser)]
#[command(
    name = "lagd-ctl",
    version,
    about = "Dial the lagd latency stages up and down at runtime"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show every stage's delay and whether it is in the signal path.
    Status,

    /// Set a stage's added delay, in milliseconds.
    Set {
        stage: Selector,
        /// Milliseconds; clamped to the 500 ms ceiling.
        ms: u64,
    },

    /// Nudge a stage's delay by a signed number of milliseconds.
    // The setting has to live on this subcommand, not on the root: clap does
    // not propagate it, so without it `adj input -5` is parsed as an unknown
    // flag rather than a negative number.
    #[command(allow_negative_numbers = true)]
    Adj { stage: Selector, delta_ms: i64 },

    /// Take stages out of the signal path entirely.
    ///
    /// Not the same as `set <stage> 0`: at zero delay the input stage still
    /// costs a grab/uinput round trip and the audio stage still costs a
    /// `PipeWire` quantum. Dropping removes that residue too.
    Drop {
        #[arg(required = true, num_args = 1..)]
        stages: Vec<Selector>,
    },

    /// Put dropped stages back, at the delay they still hold.
    Restore {
        #[arg(required = true, num_args = 1..)]
        stages: Vec<Selector>,
    },

    /// Flip a stage between zero and the last delay it held.
    Toggle { stage: Selector },

    /// Everything to zero and out of the path. The oh-shit key.
    Panic,
}

#[derive(Clone, Copy, ValueEnum)]
enum Selector {
    Input,
    Audio,
    Present,
    All,
}

impl Selector {
    fn stages(self) -> Vec<StageId> {
        match self {
            Selector::Input => vec![StageId::Input],
            Selector::Audio => vec![StageId::Audio],
            Selector::Present => vec![StageId::Present],
            Selector::All => StageId::ALL.to_vec(),
        }
    }

    fn expand(selectors: &[Selector]) -> Vec<StageId> {
        let mut out = Vec::new();
        for id in selectors.iter().copied().flat_map(Selector::stages) {
            if !out.contains(&id) {
                out.push(id);
            }
        }
        out
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let map = StateMap::open_or_create().with_context(|| {
        format!(
            "opening the lagd control plane at {}",
            StateMap::path().display()
        )
    })?;
    let st = map.state();

    match cli.cmd {
        Cmd::Status => print_status(st),

        Cmd::Set { stage, ms } => {
            let want = ms.saturating_mul(1000);
            for id in stage.stages() {
                let got = st.stage(id).set_delay_us(want);
                if got < want {
                    eprintln!(
                        "lagd-ctl: clamped {id} to {} ms (ceiling is {} ms)",
                        got / 1000,
                        MAX_DELAY_US / 1000
                    );
                }
                report(id, st.stage(id));
            }
        }

        Cmd::Adj { stage, delta_ms } => {
            for id in stage.stages() {
                st.stage(id).adjust_ms(delta_ms);
                report(id, st.stage(id));
            }
        }

        Cmd::Drop { stages } => {
            for id in Selector::expand(&stages) {
                st.stage(id).set_bypass(true);
                report(id, st.stage(id));
            }
        }

        Cmd::Restore { stages } => {
            for id in Selector::expand(&stages) {
                st.stage(id).set_bypass(false);
                report(id, st.stage(id));
            }
        }

        Cmd::Toggle { stage } => {
            for id in stage.stages() {
                st.stage(id).toggle();
                report(id, st.stage(id));
            }
        }

        Cmd::Panic => {
            for id in StageId::ALL {
                let stage = st.stage(id);
                stage.set_delay_us(0);
                stage.set_bypass(true);
            }
            println!("all stages zeroed and dropped out of the path");
        }
    }

    Ok(())
}

fn print_status(st: &State) {
    println!(
        "control plane {} (version {})",
        StateMap::path().display(),
        st.version()
    );
    println!("{:<9}{:>8}{:>9}  path", "stage", "delay", "resume");
    for id in StageId::ALL {
        let stage = st.stage(id);
        println!(
            "{:<9}{:>8}{:>9}  {}",
            id.as_str(),
            format_ms(stage.delay_us()),
            format_ms(stage.resume_us()),
            path_state(stage),
        );
    }
}

fn report(id: StageId, stage: &Stage) {
    println!(
        "{id}: {} ({})",
        format_ms(stage.delay_us()),
        path_state(stage)
    );
}

fn path_state(stage: &Stage) -> &'static str {
    if stage.is_bypassed() {
        "dropped"
    } else {
        "active"
    }
}

fn format_ms(us: u64) -> String {
    // Sub-millisecond settings are legitimate (a 500 us input delay is
    // perceptible in aggregate), so do not round them away to "0ms".
    if us > 0 && us < 1000 {
        format!("{us}us")
    } else {
        format!("{}ms", us / 1000)
    }
}
