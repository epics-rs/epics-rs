//! The `tests/*.rs` of `epics-libcom-rs` that need no process of their own,
//! built as one binary: test `t` of the former binary `X` is `X::t` here, and
//! `binary(integration) & test(/^X::/)` selects what `X` ran.

mod console_lines_survive_a_concurrent_stdout;
mod errlog_console_strips_ansi_unless_a_terminal;
mod errlog_escapes_come_from_the_console_predicate;
mod errlog_exit_drains_the_queue;
mod errlog_listeners_follow_the_live_list;
mod errlog_no_console_reaches_only_listeners;
mod rt_priority_default;
mod rtems_exec_model_gate;
mod stdlib_strtol_base;
mod stdlib_strtol_overflow;
