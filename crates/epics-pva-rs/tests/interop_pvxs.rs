//! PVA interop: Rust ↔ pvxs (`~/codes/pvxs`) `pvget`/`pvput`/`pvmonitor`.
//!
//! Opt-in via `cargo nextest run --profile interop`. Each test
//! shells out to a pvxs CLI binary; missing binaries → skip with a
//! SKIP-prefixed stderr line so a host without pvxs installed isn't
//! a hard fail.
//!
//! Targets the gaps that pure Rust↔Rust tests can't catch:
//!
//! - **Pipeline negotiation phase**: Rust `pvmonitor` with
//!   `pipeline_size > 0` against pvxs server, verified by inspecting
//!   the server's logged `record._options.pipeline` value (or — when
//!   we add a fault injection mode — the server's window emit /
//!   ACK protocol).
//! - **Typed pipeline options**: pvxs `pvget`-with-typed-
//!   pvRequest builder `.record("pipeline", true).record("queueSize",
//!   N)` against the Rust server. Verifies the Rust server's
//!   `monitor_pipeline_options` parser accepts the bool/int form, not
//!   only the parsed-string form.
//! - **TCP search on Rust server**: pvxs client configured
//!   with `EPICS_PVA_NAME_SERVERS=<rust>:port` sending a SEARCH on
//!   the established TCP circuit, expecting the Rust server to
//!   answer with SEARCH_RESPONSE on the same circuit.

mod interop_helpers;

#[cfg(unix)]
#[path = "interop_pvxs_mods/access_denied.rs"]
mod access_denied;
#[cfg(unix)]
#[path = "interop_pvxs_mods/asg_cross_impl.rs"]
mod asg_cross_impl;
#[cfg(unix)]
#[path = "interop_pvxs_mods/be_byte_order.rs"]
mod be_byte_order;
#[cfg(unix)]
#[path = "interop_pvxs_mods/beacon_udp.rs"]
mod beacon_udp;
#[cfg(unix)]
#[path = "interop_pvxs_mods/complex_types.rs"]
mod complex_types;
#[cfg(unix)]
#[path = "interop_pvxs_mods/field_projection.rs"]
mod field_projection;
#[cfg(unix)]
#[path = "interop_pvxs_mods/large_array.rs"]
mod large_array;
#[cfg(unix)]
#[path = "interop_pvxs_mods/monitor_stream.rs"]
mod monitor_stream;
#[cfg(unix)]
#[path = "interop_pvxs_mods/pipeline_r1.rs"]
mod pipeline_r1;
#[cfg(unix)]
#[path = "interop_pvxs_mods/pipeline_r20.rs"]
mod pipeline_r20;
#[cfg(unix)]
#[path = "interop_pvxs_mods/put_cross_impl.rs"]
mod put_cross_impl;
#[cfg(unix)]
#[path = "interop_pvxs_mods/reverse_complex_types.rs"]
mod reverse_complex_types;
#[cfg(unix)]
#[path = "interop_pvxs_mods/rpc_and_get_field.rs"]
mod rpc_and_get_field;
#[cfg(unix)]
#[path = "interop_pvxs_mods/tcp_search_r11.rs"]
mod tcp_search_r11;
#[cfg(all(unix, feature = "tls"))]
#[path = "interop_pvxs_mods/tls_interop.rs"]
mod tls_interop;
#[cfg(all(unix, feature = "tls"))]
#[path = "interop_pvxs_mods/tls_mtls.rs"]
mod tls_mtls;
#[cfg(unix)]
#[path = "interop_pvxs_mods/type_cache.rs"]
mod type_cache;
