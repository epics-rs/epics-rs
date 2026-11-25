//! The `tests/*.rs` of `epics-stream-rs` that need no process of their own,
//! built as one binary: test `t` of the former binary `X` is `X::t` here, and
//! `binary(integration) & test(/^X::/)` selects what `X` ran. Helpers several
//! modules share are declared once below.

mod common;
#[path = "dbior/boot.rs"]
mod dbior_boot;
#[path = "dbior/harness.rs"]
mod dbior_harness;
mod redirect;

mod a_link_put_lands_before_the_record_completes;
mod asyn;
mod bo;
mod calcout_posts;
mod checksum_converter;
mod cstr_strtol_base;
mod dbior;
mod dbior_app;
mod dbior_bytes;
mod debug_interface;
mod dev_int64out;
mod dev_longout;
mod dev_mbbo;
mod dev_mbbo_direct;
mod device_posts;
mod drv_init;
mod dset;
mod dummy;
mod enum_converter;
mod epics;
mod field_redirect_addr;
mod field_redirect_debug_shows_the_field_buffer;
mod field_redirect_dollar;
mod field_redirect_format;
mod field_redirect_held;
mod field_redirect_link;
mod field_redirect_lockset;
mod field_redirect_match;
mod gate;
mod init_order;
mod lock_partners_join_with_no_set_held;
mod lso_posts;
mod mantissa_exponent_converter;
mod oracle;
mod pcre_internal;
mod protocol;
mod put_failed_line;
mod raw_converter;
mod raw_float_converter;
mod regexp_converter;
mod reload_stale_reply;
mod runtime;
mod std_converters;
mod stream_init_after;
mod stream_init_in_dev_sup_passes;
mod stringout_posts;
mod timestamp_converter;
