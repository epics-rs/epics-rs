//! Cross-platform networking primitives shared by `epics-ca-rs`,
//! `epics-pva-rs`, and `epics-bridge-rs`.
//!
//! Two layers, split by whether they own a socket — the same split
//! `epics_pva_rs::server_native` makes for the same reason:
//!
//! * **Wire constants.** Address literals the protocols embed in the
//!   datagrams they build ([`ORIGIN_TAG_MCAST_GROUP`]). No socket, no
//!   `tokio`, no `socket2` — compiles for every target, RTEMS and VxWorks
//!   included.
//! * **Socket-bearing modules** — `async_udp_v4`, `iface_map`,
//!   `loopback_mcast`. Built on `tokio::net` / `socket2` / `if-addrs`, none
//!   of which cross to `armv7-rtems-eabihf` or the `*-wrs-vxworks*` triples,
//!   so they are gated off `epics_embedded_target` and are otherwise
//!   host-only. Named in code spans rather than linked for exactly that
//!   reason: this module doc is compiled on every target, the three modules
//!   are not, and a link from the one to the other cannot resolve on an
//!   embedded doc build. The rendered host page still reaches them through
//!   its own Modules table.
//!
//! The socket modules:
//!
//! * `iface_map` — enumerate IPv4 network interfaces with their
//!   `ifindex`, broadcast/netmask info, and multicast capability.
//!   Used by every UDP search/beacon path to plan per-NIC fanout.
//!
//! * `async_udp_v4` — cross-platform async UDP socket with
//!   per-NIC TX/RX accuracy. On Unix (Linux + macOS) we use a single
//!   wildcard socket plus `IP_PKTINFO` cmsg (pvxs convention); on
//!   Windows we use a `Vec` of per-NIC bound sockets (libca
//!   convention). Both paths expose the same `AsyncUdpV4` surface.

use std::net::Ipv4Addr;

#[cfg(not(epics_embedded_target))]
pub mod async_udp_v4;
#[cfg(not(epics_embedded_target))]
pub mod iface_map;
#[cfg(not(epics_embedded_target))]
pub mod loopback_mcast;

/// pvxs ORIGIN_TAG forwarding multicast group. Mirrors the literal in
/// `udp_collector.cpp:127`: `"224.0.0.128,1@127.0.0.1"` — TTL=1,
/// iface=loopback. We expose the group separately so callers can
/// embed it in send addresses without re-parsing the pvxs string.
///
/// Lives here rather than in `loopback_mcast` because it is *wire data*,
/// not a socket: the PVA SEARCH decoder writes it into the destination of
/// every ORIGIN_TAG re-broadcast it decides on, and that decoder runs on
/// `epics_embedded_target` where the socket half of this module does not
/// exist. Gating a constant behind a socket module is what forced the PVA
/// decoder to stay host-only; the fix is to move the constant, not to copy
/// it.
pub const ORIGIN_TAG_MCAST_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 128);

#[cfg(not(epics_embedded_target))]
pub use async_udp_v4::{
    AsyncUdpV4, RecvMeta, enable_so_rxq_ovfl_for_socket, recv_from_with_drop_count_socket,
};
#[cfg(not(epics_embedded_target))]
pub use iface_map::{IfaceInfo, IfaceMap};
#[cfg(not(epics_embedded_target))]
pub use loopback_mcast::bind_loopback_mcast;
