//! The `tests/*.rs` of `scope-ioc` that need no process of their own, built as
//! one binary: test `t` of the former binary `X` is `X::t` here, and
//! `binary(integration) & test(/^X::/)` selects what `X` ran. Helpers several
//! modules share are declared once below.

mod common;

mod db_links_name_driver_params;
mod initial_values_are_c_s;
mod reference_db_is_c_s;
mod selects_are_c_s;
mod sim_task_posts_the_waveform;
mod statistics_are_unscaled;
mod write_float64_is_cs;
