//! The `tests/*.rs` of `epics-seq-rs` that need no process of their own, built
//! as one binary: test `t` of the former binary `X` is `X::t` here, and
//! `binary(integration) & test(/^X::/)` selects what `X` ran. Helpers several
//! modules share are declared once below.

#[cfg(feature = "snc")]
mod common;

#[cfg(feature = "snc")]
mod analysis;
#[cfg(feature = "snc")]
mod cpp;
#[cfg(feature = "runtime")]
mod foreign_call;
#[cfg(feature = "runtime")]
mod golden_delay_do;
#[cfg(feature = "runtime")]
mod pv;
#[cfg(feature = "snc")]
mod reference_analysis;
#[cfg(feature = "snc")]
mod reference_cpp;
#[cfg(feature = "snc")]
mod reference_gen_code;
#[cfg(feature = "snc")]
mod reference_gen_tables;
#[cfg(feature = "snc")]
mod reference_snc_cli;
#[cfg(feature = "snc")]
mod reference_snc_parse;
#[cfg(feature = "snc")]
mod reference_snl_tables;
