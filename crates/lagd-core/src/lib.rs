//! Shared plumbing for the `lagd` latency injectors.
//!
//! The three injectors ([`StageId::Input`], [`StageId::Audio`],
//! [`StageId::Present`]) run as separate processes, and the present stage runs
//! as a Vulkan layer *inside* every Vulkan client — so there is no single
//! process that can own a control socket. Instead every component maps one
//! small [`State`] struct out of shared memory and reads its own stage's
//! atomics. `lagd-ctl` writes them. No IPC, no startup ordering, and the
//! present hook stays allocation- and syscall-free.

pub mod delay;
pub mod state;

pub use delay::DelayLine;
pub use state::{Stage, StageId, State, StateMap, MAX_DELAY_US, STATE_VERSION};
