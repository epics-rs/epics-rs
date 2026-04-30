//! Cross-platform networking primitives shared by `epics-ca-rs`,
//! `epics-pva-rs`, and `epics-bridge-rs`.
//!
//! The two main exports:
//!
//! * [`iface_map`] — enumerate IPv4 network interfaces with their
//!   `ifindex`, broadcast/netmask info, and multicast capability.
//!   Used by every UDP search/beacon path to plan per-NIC fanout.
//!
//! * [`async_udp_v4`] — cross-platform async UDP socket with
//!   per-NIC TX/RX accuracy. On Unix (Linux + macOS) we use a single
//!   wildcard socket plus `IP_PKTINFO` cmsg (pvxs convention); on
//!   Windows we use a `Vec` of per-NIC bound sockets (libca
//!   convention). Both paths expose the same [`AsyncUdpV4`] surface.

pub mod async_udp_v4;
pub mod iface_map;
pub mod loopback_mcast;

pub use async_udp_v4::{AsyncUdpV4, RecvMeta};
pub use iface_map::{IfaceInfo, IfaceMap};
pub use loopback_mcast::{ORIGIN_TAG_MCAST_GROUP, bind_loopback_mcast};
