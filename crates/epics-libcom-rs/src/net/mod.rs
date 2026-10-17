//! Cross-platform networking primitives shared by `epics-ca-rs`,
//! `epics-pva-rs`, and `epics-bridge-rs`.
//!
//! Two layers, split by whether they own a socket — the same split
//! `epics_pva_rs::server_native` makes for the same reason:
//!
//! * **Wire constants and interface facts.** Address literals the protocols
//!   embed in the datagrams they build ([`ORIGIN_TAG_MCAST_GROUP`]), and the
//!   IPv4 interface enumeration every UDP search destination list is derived
//!   from ([`iface_v4`]). No socket, no `tokio`, no `socket2` — compiles for
//!   every target, RTEMS and VxWorks included. [`iface_v4`] reaches the two
//!   embedded targets where `iface_map` cannot because it goes through
//!   `getifaddrs`, which both of them ship.
//! * **Socket-bearing modules** — `async_udp_v4`, `iface_map`,
//!   `loopback_mcast`. Built on `tokio::net` / `socket2`, neither
//!   of which cross to `armv7-rtems-eabihf` or the `*-wrs-vxworks*` triples.
//!   They take two different predicates, and the difference is not
//!   cosmetic: `async_udp_v4` awaits on `tokio::net`, so it is
//!   `tokio_backend` — absent from a host `exec_backend` build too, whose
//!   workers have no reactor to await on. `iface_map` and `loopback_mcast`
//!   own no socket they await: `socket2` is a *target* fact,
//!   so they stay `not(epics_embedded_target)` and a host `exec_backend`
//!   build keeps them (`loopback_mcast` there as its socket-option setup
//!   only; the tokio-wrapped bind is `tokio_backend`). Named in code spans rather than linked for exactly that
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

/// C `epicsSocketConvertErrorToString` / `epicsSocketConvertErrnoToString`
/// (`epicsSocketConvertErrnoToString.cpp:25-38`), which are `strerror` into
/// a 64-byte buffer. `io::Error`'s Display is the same sentence with
/// ` (os error N)` appended, so the suffix comes off rather than the text
/// being rebuilt from a table this port would have to keep in step.
pub fn sock_err_string(e: &std::io::Error) -> String {
    let text = e.to_string();
    match e.raw_os_error() {
        Some(errno) => text
            .strip_suffix(&format!(" (os error {errno})"))
            .unwrap_or(&text)
            .to_string(),
        None => text,
    }
}

#[cfg(tokio_backend)]
pub mod async_udp_v4;
pub mod fanout;
#[cfg(not(epics_embedded_target))]
pub mod iface_map;
pub mod iface_v4;
#[cfg(not(epics_embedded_target))]
pub mod loopback_mcast;
pub mod mcast_prep;
pub mod search_udp;
#[cfg(all(not(epics_embedded_target), any(unix, windows)))]
pub mod udp_rx;
#[cfg(windows)]
mod win_adapters;

/// pvxs ORIGIN_TAG forwarding multicast group. Mirrors the literal in
/// `udp_collector.cpp:127`: `"224.0.0.128,1@127.0.0.1"` — TTL=1,
/// iface=loopback. We expose the group separately so callers can
/// embed it in send addresses without re-parsing the pvxs string.
///
/// Lives here rather than in `loopback_mcast` because it is *wire data*, not a
/// socket: the PVA SEARCH decoder writes it into the destination of every
/// ORIGIN_TAG re-broadcast it decides on, and that decoder runs on
/// `epics_embedded_target` where the socket half of this module does not
/// exist. Gating a constant behind a socket module is what forced the PVA
/// decoder to stay host-only; the fix is to move the constant, not to copy
/// it.
pub const ORIGIN_TAG_MCAST_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 128);

pub use fanout::{PortOrigin, PortUse, bind_falls_back, bind_fanout, port_held};
pub use iface_v4::{IfaceV4, broadcast_addrs, local_addr};
pub use mcast_prep::{McastSocket, mcast_prep_v4, mcast_prep_v6};
pub use search_udp::{BeaconUdpSocket, SearchDatagram, SearchUdpSocket, SearchUdpSocketV6};

#[cfg(tokio_backend)]
pub use async_udp_v4::{
    AsyncUdpV4, RecvMeta, enable_so_rxq_ovfl_for_socket, recv_from_with_drop_count_socket,
};
#[cfg(not(epics_embedded_target))]
pub use iface_map::{ExternalIface, IfaceInfo, IfaceMap, all_external};
#[cfg(tokio_backend)]
pub use loopback_mcast::bind_loopback_mcast;
#[cfg(not(epics_embedded_target))]
pub use loopback_mcast::{deny_unjoined_multicast, enable_loopback_mcast};
