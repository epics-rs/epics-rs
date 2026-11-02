//! Rust port of the EPICS sequencer R2-2-9: the run-time (`src/seq`,
//! `src/pv`, feature `runtime`, the default) and the SNL compiler snc-rs
//! (`src/snc`, module `snc` under feature `snc`), one crate as C's seq
//! module is one module.
//!
// The rest links run-time items, which a build without `runtime` lacks.
#![cfg_attr(feature = "runtime", doc = include_str!("runtime.md"))]

#[cfg(feature = "runtime")]
pub mod arith;
#[cfg(feature = "runtime")]
pub mod c;
#[cfg(feature = "runtime")]
mod ca;
#[cfg(feature = "runtime")]
mod cmd;
#[cfg(feature = "runtime")]
mod mac;
#[cfg(feature = "runtime")]
mod prog;
#[cfg(feature = "runtime")]
mod program;
#[cfg(feature = "runtime")]
pub mod pv;
#[cfg(feature = "runtime")]
#[doc(hidden)]
pub mod pvt;
#[cfg(feature = "runtime")]
mod qry;
#[cfg(feature = "runtime")]
mod queue;
#[cfg(feature = "runtime")]
mod seq;
#[cfg(feature = "snc")]
pub mod snc;
#[cfg(feature = "runtime")]
mod state_set;
#[cfg(feature = "runtime")]
mod task;
#[cfg(feature = "runtime")]
pub mod types;
#[cfg(feature = "runtime")]
pub mod var;

#[cfg(feature = "runtime")]
pub use cmd::{RegisteredProgram, seq_register_sequencer_commands, seq_register_sequencer_program};
#[cfg(feature = "runtime")]
pub use program::*;
#[cfg(feature = "runtime")]
pub use qry::{SeqStats, seq_chan_show, seq_gather_stats, seq_queue_show, seq_show, seqcar};
#[cfg(feature = "runtime")]
pub use seq::{SeqThreadId, seq};
#[cfg(feature = "runtime")]
pub use state_set::{CompleteFlags, StateSet};
#[cfg(feature = "runtime")]
pub use task::seq_stop;
#[cfg(feature = "runtime")]
pub use types::*;
#[cfg(feature = "runtime")]
pub use var::{
    Addr, At, ChanCell, ElemKind, Field, Indexed, Place, Prim, ProgramVars, Ptr, SeqVar,
    UnionBytes, UnionMember, View, Zeroed,
};

/// Include the program snc-rs wrote for `<name>.st` to `$OUT_DIR/<name>.rs`.
/// The companion's `seqg_escaped!` must be defined before this call.
#[cfg(feature = "runtime")]
#[macro_export]
macro_rules! include_program {
    ($name:literal) => {
        include!(concat!(env!("OUT_DIR"), "/", $name, ".rs"));
    };
}
