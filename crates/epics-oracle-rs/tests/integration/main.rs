//! The `tests/*.rs` of `epics-oracle-rs` that need no process of their own,
//! built as one binary: test `t` of the former binary `X` is `X::t` here, and
//! `binary(integration) & test(/^X::/)` selects what `X` ran.

mod calc_fetch_stop_oracle;
mod fetch_prologue_oracle;
mod oracle;
mod oracle_ioc_db_load_fails_as_softioc_d;
mod oracle_ioc_iocinit_line;
mod oracle_ioc_pini_order;
mod pva_oracle;
mod runner_pair;
mod scaler_oracle;
mod sseq_atof_oracle;
