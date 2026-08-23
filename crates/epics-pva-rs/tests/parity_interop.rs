//! Wire-level parity / interop matrix entry point.
//!
//! Cargo only picks up `tests/*.rs` automatically; this file exists so the
//! sub-modules under `tests/parity/` get compiled and registered.

#![allow(
    clippy::manual_async_fn,
    clippy::approx_constant,
    clippy::collapsible_if,
    clippy::single_char_add_str,
    clippy::nonstandard_macro_braces,
    non_snake_case
)]

#[path = "parity/interop.rs"]
mod interop;
#[path = "parity/stability_interop.rs"]
mod stability_interop;
#[path = "parity/test_client_server_lifecycle.rs"]
mod test_client_server_lifecycle;
#[path = "parity/test_monitor_finish.rs"]
mod test_monitor_finish;
#[path = "parity/test_monitor_reload_deny_composite.rs"]
mod test_monitor_reload_deny_composite;
#[path = "parity/test_put_get_process.rs"]
mod test_put_get_process;
#[path = "parity/test_pvrequest_filter.rs"]
mod test_pvrequest_filter;
#[path = "parity/test_requeue_on_destroy.rs"]
mod test_requeue_on_destroy;
#[path = "parity/test_sharedpv_lazy_open.rs"]
mod test_sharedpv_lazy_open;
#[path = "parity/testbitmask_port.rs"]
mod testbitmask_port;
#[path = "parity/testconfig_port.rs"]
mod testconfig_port;
#[path = "parity/testdata_port.rs"]
mod testdata_port;
#[path = "parity/testdiscover_port.rs"]
mod testdiscover_port;
#[path = "parity/testendian_port.rs"]
mod testendian_port;
#[path = "parity/testget_port.rs"]
mod testget_port;
#[path = "parity/testnt_port.rs"]
mod testnt_port;
#[path = "parity/testpvreq_port.rs"]
mod testpvreq_port;
#[path = "parity/testtype_port.rs"]
mod testtype_port;
#[path = "parity/testudp_port.rs"]
mod testudp_port;
#[path = "parity/testxcode_port.rs"]
mod testxcode_port;
#[cfg(feature = "tls")]
#[path = "parity/tls_interop.rs"]
mod tls_interop;
#[path = "parity/wire_dump.rs"]
mod wire_dump;
