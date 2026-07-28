//! IPv4 interface enumeration, on every target the workspace builds for —
//! RTEMS and VxWorks included.
//!
//! # Why this exists
//!
//! The UDP name-search destination list is derived from the machine's
//! interfaces: `EPICS_CA_AUTO_ADDR_LIST=YES` (C's default) expands to one
//! broadcast address per up, non-loopback interface. Until this module existed
//! there was no way to compute that list on `epics_embedded_target`, because
//! the one enumerator in the workspace went through the `if-addrs` crate, which
//! does not build for `armv7-rtems-eabihf` or the `*-wrs-vxworks*` triples. So
//! `epics-ca-rs::server::addr_list` carried three `#[cfg(epics_embedded_target)]`
//! stubs that returned "no interfaces", and a target IOC could reach a server
//! only through an explicitly configured name server.
//!
//! It lives in `epics-libcom-rs` for the reason `runtime::socket` does:
//! `epics-ca-rs` and `epics-pva-rs` both need it, neither may depend on the
//! other, and this is the one crate both reach. It is also the first entry in
//! `CRATES` in both `scripts/rtems-check.sh` and `scripts/vxworks-check.sh`, so
//! the `unsafe` below is compiled for both embedded triples by gates that
//! already run.
//!
//! # `getifaddrs`, not `SIOCGIFCONF`
//!
//! EPICS base has two implementations of this enumeration and picks between
//! them per OS:
//!
//! * `libcom/src/osi/osdNetIfAddrs.c` — `getifaddrs(3)`. Selected by Linux,
//!   Darwin, FreeBSD, iOS and cygwin, each of whose `os/*/osdNetIntf.c` is a
//!   one-line `#include` of it.
//! * `libcom/src/osi/osdNetIfConf.c` — `ioctl(SIOCGIFCONF)` walking a
//!   `struct ifreq` array. Reached through `os/default/osdNetIntf.c`, which is
//!   what RTEMS and vxWorks take: neither has an `osdNetIntf.c` of its own.
//!
//! The two compute the same list — same `IFF_UP` / `IFF_LOOPBACK` rejections,
//! same `IFF_BROADCAST`-else-`IFF_POINTOPOINT` destination choice — and this
//! module reproduces the `getifaddrs` one on all targets, including the two
//! where C takes the `SIOCGIFCONF` one.
//!
//! That is a deliberate deviation, and the reason is that both embedded targets
//! *do* ship `getifaddrs`; C's selection predates that rather than reflecting
//! it. Measured headers, not assumed:
//!
//! | target | header | declared | defined in |
//! |---|---|---|---|
//! | **RTEMS 6** (`armv7-rtems-eabihf`) | `arm-rtems6/<bsp>/lib/include/ifaddrs.h` | yes | `libbsd.a` |
//! | RTEMS 5 | `arm-rtems5/<bsp>/lib/include/ifaddrs.h` | yes | `libbsd.a` |
//! | RTEMS 7 | `arm-rtems7/<bsp>/lib/include/ifaddrs.h` | yes | `libbsd.a` |
//! | **VxWorks 7** (`*-wrs-vxworks`) | `vxsdk/sysroot/usr/h/public/net/ifaddrs.h` | yes | `libnet.a` |
//!
//! The two bold rows are the triples this workspace builds; the RTEMS 5 and 7
//! rows are there because the struct is unchanged across all three, so an RTEMS
//! version bump is not a layout risk. "defined in" is `nm --defined-only`, not
//! the header — a declaration the image cannot link is what a `cargo check`
//! gate would have missed.
//!
//! Taking it avoids re-deriving the BSD `_IOWR` request encoding — on VxWorks
//! that is a *different* encoding again (`VX_IOWR`, `net/ioctl.h`), so the
//! `SIOCGIFCONF` route would have been two hand-built ioctl number schemes plus
//! two `struct ifreq` layouts, where `getifaddrs` is one struct that all three
//! platforms agree on byte for byte (see [`sys`]).
//!
//! # What is *not* here
//!
//! No `ifindex`, no multicast capability, no IPv6. Those belong to
//! `net::iface_map`, which backs the per-NIC `AsyncUdpV4` bundle and stays
//! host-only: the embedded search transport is C's single wildcard socket
//! (libca `udpiiu.cpp:174` opens exactly one `SOCK_DGRAM`), so it needs the
//! destination list this module computes and nothing else about a NIC.

use std::io;
use std::net::Ipv4Addr;

/// Interface flags this module reads. Values are the BSD set, and are the same
/// three-way agreement the struct layout is: `libc`'s newlib module has
/// `IFF_UP = 0x1`, `IFF_BROADCAST = 0x2`, `IFF_LOOPBACK = 0x8`,
/// `IFF_POINTOPOINT = 0x10`, VxWorks' `net/if.h:192-196` has the same four
/// values, and so does Linux. They are restated here rather than taken from
/// `libc` because `libc` does not define them for VxWorks at all.
pub mod flags {
    /// Interface is administratively up. C skips everything without it
    /// (`osdNetIfAddrs.c:100`).
    pub const IFF_UP: u32 = 0x1;
    /// `ifa_broadaddr` is meaningful (`osdNetIfAddrs.c:130`).
    pub const IFF_BROADCAST: u32 = 0x2;
    /// Loopback. C skips it (`osdNetIfAddrs.c:108`).
    pub const IFF_LOOPBACK: u32 = 0x8;
    /// Point-to-point link; `ifa_dstaddr` is the peer
    /// (`osdNetIfAddrs.c:143`).
    pub const IFF_POINTOPOINT: u32 = 0x10;
}

/// One IPv4 interface, as `getifaddrs` reported it.
///
/// Flat rather than an enum over broadcast/point-to-point because the BSD
/// `struct ifaddrs` is flat: `ifa_broadaddr` and `ifa_dstaddr` are the *same*
/// pointer, and which one it means is decided by [`flags`]. Modelling that as
/// two variants here would invent a distinction the OS does not make and would
/// have to be un-made again by every caller that just wants "where do I send a
/// SEARCH through this interface" — which is [`Self::search_destination`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IfaceV4 {
    /// Interface name (`eth0`, `en0`, ...). Diagnostic only.
    pub name: String,
    /// The interface's own IPv4 address (`ifa_addr`).
    pub ip: Ipv4Addr,
    /// `ifa_netmask`, when the platform reported one.
    pub netmask: Option<Ipv4Addr>,
    /// `ifa_broadaddr` / `ifa_dstaddr` — one field, meaning selected by
    /// [`flags`]. `None` when the platform left it null.
    pub dest: Option<Ipv4Addr>,
    /// `ifa_flags`.
    pub flags: u32,
}

impl IfaceV4 {
    /// `IFF_UP`.
    pub fn is_up(&self) -> bool {
        self.flags & flags::IFF_UP != 0
    }

    /// `IFF_LOOPBACK`.
    pub fn is_loopback(&self) -> bool {
        self.flags & flags::IFF_LOOPBACK != 0
    }

    /// `IFF_BROADCAST`.
    pub fn is_broadcast(&self) -> bool {
        self.flags & flags::IFF_BROADCAST != 0
    }

    /// `IFF_POINTOPOINT`.
    pub fn is_point_to_point(&self) -> bool {
        self.flags & flags::IFF_POINTOPOINT != 0
    }

    /// Whether C would query through this interface at all: up, and not
    /// loopback (`osdNetIfAddrs.c:100`, `:108`).
    ///
    /// Note that C tests `IFF_UP` and not `IFF_RUNNING`, so an interface that
    /// is configured but has no carrier is still eligible. Reproduced rather
    /// than improved on: an operator who brings a NIC up before the cable is
    /// plugged in gets the same address list from both IOCs.
    pub fn is_eligible(&self) -> bool {
        self.is_up() && !self.is_loopback()
    }

    /// Where a UDP SEARCH addressed to "everyone on this interface" goes, or
    /// `None` if C would not query through it.
    ///
    /// The whole of C's destination choice, in C's order
    /// (`osdNetIfAddrs.c:130-151`):
    ///
    /// * `IFF_BROADCAST` → the broadcast address, *unless* it is `0.0.0.0`,
    ///   which C discards (`:133`);
    /// * else `IFF_POINTOPOINT` → the peer address, so a VPN/PPP tunnel is
    ///   queried through its far end;
    /// * else nothing — "CA will not query through the interface".
    ///
    /// The eligibility test is folded in so no caller can walk the list and
    /// forget it. That is the bug shape this returns `Option` to prevent: a
    /// down interface still carries an address, and a caller that only checked
    /// `dest.is_some()` would fan SEARCH at it.
    pub fn search_destination(&self) -> Option<Ipv4Addr> {
        if !self.is_eligible() {
            return None;
        }
        if self.is_broadcast() {
            return self.dest.filter(|b| !b.is_unspecified());
        }
        if self.is_point_to_point() {
            return self.dest;
        }
        None
    }
}

/// Enumerate the machine's IPv4 interfaces.
///
/// Every `AF_INET` interface the OS reports, filtered by nothing: eligibility
/// is [`IfaceV4::is_eligible`]'s job and the destination choice is
/// [`IfaceV4::search_destination`]'s, so a caller that wants the raw list for
/// diagnostics gets it and a caller that wants C's SEARCH destinations composes
/// the two.
pub fn enumerate() -> io::Result<Vec<IfaceV4>> {
    sys::enumerate()
}

/// C `osiSockDiscoverBroadcastAddresses` with a wildcard match address: the
/// deduplicated UDP SEARCH destination of every eligible interface.
///
/// The `pMatchAddr` argument C takes is not reproduced here because its two
/// non-wildcard uses are separate functions in this workspace
/// ([`local_addr`] and the per-interface lookup in
/// `epics-ca-rs::server::addr_list`), and a single-caller parameter that is
/// always the same value is the kind of surface a reviewer rightly asks about.
///
/// Deduplicated because two aliases on one subnet report the same broadcast
/// address and C's caller (`addAddrToChannelAccessAddressList`) drops
/// duplicates on insert.
pub fn broadcast_addrs() -> Vec<Ipv4Addr> {
    let Ok(ifaces) = enumerate() else {
        return Vec::new();
    };
    let mut out: Vec<Ipv4Addr> = Vec::new();
    for iface in ifaces {
        if let Some(dest) = iface.search_destination() {
            if !out.contains(&dest) {
                out.push(dest);
            }
        }
    }
    out
}

/// C `osiLocalAddr` (`osdNetIfAddrs.c:167-216`): the address of the FIRST
/// interface that is up, `AF_INET`, and not loopback.
///
/// Falls back to `INADDR_LOOPBACK` when enumeration fails or finds only
/// loopback, which is C's fallback (`:208-213`). Not cached here — C caches
/// behind `epicsThreadOnce`, and the caller that needs that caches it; making
/// this function itself remember would put a process-lifetime `OnceLock` in a
/// primitive whose whole job is to report what the OS says now.
pub fn local_addr() -> Ipv4Addr {
    let Ok(ifaces) = enumerate() else {
        return Ipv4Addr::LOCALHOST;
    };
    ifaces
        .into_iter()
        .find(|i| i.is_eligible())
        .map(|i| i.ip)
        .unwrap_or(Ipv4Addr::LOCALHOST)
}

#[cfg(unix)]
mod sys {
    //! Unix, hosted and embedded alike: `getifaddrs(3)`.
    //!
    //! # One struct for five platforms
    //!
    //! [`IfAddrs`] is declared here rather than taken from `libc` because
    //! `libc` does not have it on either embedded target: `src/unix/newlib`
    //! declares no `ifaddrs` at all, and `src/vxworks/mod.rs` declares neither
    //! the struct nor the function. Declaring it *only* there and using
    //! `libc::ifaddrs` elsewhere would mean two spellings of one walk — and two
    //! spellings is what the `libc` field name forces anyway, since Linux calls
    //! the sixth field `ifa_ifu` and BSD calls it `ifa_dstaddr`.
    //!
    //! So there is one declaration, and the platforms whose `libc` *does* have
    //! the struct check it rather than being trusted: [`LAYOUT_MATCHES_LIBC`]
    //! is a `const` assertion against `libc::ifaddrs`, so a wrong field order
    //! here fails to compile on Linux and macOS — the two hosted-Unix
    //! configurations CI builds — instead of silently misreading an address on
    //! a target no CI machine runs.
    //!
    //! Measured layouts, all seven fields in this order and `unsigned int` for
    //! the flags:
    //!
    //! | platform | source |
    //! |---|---|
    //! | Linux | `libc` `src/unix/linux_like/mod.rs:151` |
    //! | RTEMS 5 / 7 | `<bsp>/lib/include/ifaddrs.h` |
    //! | VxWorks 7 | `usr/h/public/net/ifaddrs.h` (sixth field is a union of `ifu_broadaddr`/`ifu_dstaddr`, which is one pointer) |

    use super::IfaceV4;
    use std::ffi::CStr;
    use std::io;
    use std::net::Ipv4Addr;

    /// The BSD `struct ifaddrs`. See the module note on why it is declared
    /// rather than imported.
    #[repr(C)]
    struct IfAddrs {
        ifa_next: *mut IfAddrs,
        ifa_name: *mut libc::c_char,
        ifa_flags: libc::c_uint,
        ifa_addr: *mut libc::sockaddr,
        ifa_netmask: *mut libc::sockaddr,
        /// `ifa_broadaddr` and `ifa_dstaddr` are this one pointer; the flags
        /// say which it is.
        ifa_dstaddr: *mut libc::sockaddr,
        ifa_data: *mut libc::c_void,
    }

    /// Does this build's `libc` agree with [`IfAddrs`], field for field?
    ///
    /// Only meaningful where `libc` has the struct — the embedded targets have
    /// nothing to compare against, which is why the declaration exists. There
    /// the assertion below is skipped and the header table in the module note
    /// is the evidence.
    #[cfg(not(epics_embedded_target))]
    const LAYOUT_MATCHES_LIBC: bool = size_of::<IfAddrs>() == size_of::<libc::ifaddrs>()
        && align_of::<IfAddrs>() == align_of::<libc::ifaddrs>()
        && core::mem::offset_of!(IfAddrs, ifa_next)
            == core::mem::offset_of!(libc::ifaddrs, ifa_next)
        && core::mem::offset_of!(IfAddrs, ifa_name)
            == core::mem::offset_of!(libc::ifaddrs, ifa_name)
        && core::mem::offset_of!(IfAddrs, ifa_flags)
            == core::mem::offset_of!(libc::ifaddrs, ifa_flags)
        && core::mem::offset_of!(IfAddrs, ifa_addr)
            == core::mem::offset_of!(libc::ifaddrs, ifa_addr)
        && core::mem::offset_of!(IfAddrs, ifa_netmask)
            == core::mem::offset_of!(libc::ifaddrs, ifa_netmask)
        && core::mem::offset_of!(IfAddrs, ifa_data)
            == core::mem::offset_of!(libc::ifaddrs, ifa_data);

    #[cfg(not(epics_embedded_target))]
    const _: () = assert!(
        LAYOUT_MATCHES_LIBC,
        "net::iface_v4's `struct ifaddrs` no longer matches this platform's \
         `libc::ifaddrs`. The declaration exists because neither embedded \
         target's `libc` has the struct; this assertion is what keeps it \
         honest on the platforms whose `libc` does. Re-derive the field order \
         from the platform header before changing it."
    );

    // `libc` declares `getifaddrs` for hosted Unix but not for newlib/RTEMS or
    // VxWorks. Declaring our own on every target would be a second, clashing
    // declaration of one symbol (`clashing_extern_declarations`, and this
    // workspace builds with `-D warnings`), so the hosted arm calls `libc`'s
    // and casts the out-pointer — sound precisely because the assertion above
    // proved the two structs are one layout.
    #[cfg(not(epics_embedded_target))]
    unsafe fn get_ifaddrs(head: *mut *mut IfAddrs) -> libc::c_int {
        // SAFETY: `head` is a valid out-pointer; the cast is between two
        // structs a `const` assertion has proved layout-identical.
        unsafe { libc::getifaddrs(head as *mut *mut libc::ifaddrs) }
    }

    #[cfg(not(epics_embedded_target))]
    unsafe fn free_ifaddrs(head: *mut IfAddrs) {
        // SAFETY: `head` came from `get_ifaddrs` above, i.e. from
        // `libc::getifaddrs`, and is freed exactly once.
        unsafe { libc::freeifaddrs(head as *mut libc::ifaddrs) }
    }

    #[cfg(epics_embedded_target)]
    unsafe extern "C" {
        #[link_name = "getifaddrs"]
        fn getifaddrs_raw(ifap: *mut *mut IfAddrs) -> libc::c_int;
        #[link_name = "freeifaddrs"]
        fn freeifaddrs_raw(ifa: *mut IfAddrs);
    }

    #[cfg(epics_embedded_target)]
    unsafe fn get_ifaddrs(head: *mut *mut IfAddrs) -> libc::c_int {
        // SAFETY: `head` is a valid out-pointer, and the declaration matches
        // `ifaddrs.h` on both embedded targets (module note).
        unsafe { getifaddrs_raw(head) }
    }

    #[cfg(epics_embedded_target)]
    unsafe fn free_ifaddrs(head: *mut IfAddrs) {
        // SAFETY: `head` came from `get_ifaddrs` above and is freed once.
        unsafe { freeifaddrs_raw(head) }
    }

    /// Read an `AF_INET` address out of a `struct sockaddr *`.
    ///
    /// Goes through `libc::sockaddr_in` rather than reading a family field off
    /// `sockaddr` directly, because the two families of platform disagree about
    /// where that field is: BSD-derived stacks (macOS, RTEMS, VxWorks) put a
    /// one-byte `sa_len` first and the family second, Linux starts with a
    /// two-byte family. `libc`'s per-target `sockaddr_in` already encodes which
    /// — and on RTEMS that it is the *right* one is a build-stopping guard, not
    /// a hope (`epics_rtems_boot`'s socket-layout refusal, whose doc names this
    /// address list as a consumer).
    ///
    /// Unaligned: `ifa_addr` points into an allocation the C library laid out,
    /// and nothing in the contract promises `sockaddr_in` alignment for a
    /// pointer typed as `sockaddr`.
    ///
    /// # Safety
    ///
    /// `sa` must be null or point to a readable `sockaddr` whose storage is at
    /// least `size_of::<libc::sockaddr_in>()` when its family is `AF_INET`.
    unsafe fn sockaddr_ipv4(sa: *const libc::sockaddr) -> Option<Ipv4Addr> {
        if sa.is_null() {
            return None;
        }
        // SAFETY: the caller guarantees `sa` is readable; `read_unaligned`
        // makes no alignment demand of its own.
        let sin: libc::sockaddr_in = unsafe { std::ptr::read_unaligned(sa as *const _) };
        if libc::c_int::from(sin.sin_family) != libc::AF_INET {
            return None;
        }
        Some(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)))
    }

    pub(super) fn enumerate() -> io::Result<Vec<IfaceV4>> {
        let mut head: *mut IfAddrs = std::ptr::null_mut();
        // SAFETY: `head` is a valid out-pointer for the call.
        if unsafe { get_ifaddrs(&mut head) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if head.is_null() {
            // A success return with no list is not an error — a target with no
            // configured interface reports exactly this — and C treats the
            // empty walk the same way.
            return Ok(Vec::new());
        }

        let mut out = Vec::new();
        let mut cur = head;
        // SAFETY: the list is `getifaddrs`-owned and stays valid until the
        // `free_ifaddrs` below; every pointer read is null-checked, and every
        // address is copied out before the free.
        unsafe {
            while !cur.is_null() {
                let node = &*cur;
                cur = node.ifa_next;

                let Some(ip) = sockaddr_ipv4(node.ifa_addr) else {
                    // Null `ifa_addr` (C `osdNetIfAddrs.c:65`) or a non-INET
                    // family (`:75`) — both are skips, not failures.
                    continue;
                };
                let name = if node.ifa_name.is_null() {
                    String::new()
                } else {
                    CStr::from_ptr(node.ifa_name).to_string_lossy().into_owned()
                };
                out.push(IfaceV4 {
                    name,
                    ip,
                    netmask: sockaddr_ipv4(node.ifa_netmask),
                    dest: sockaddr_ipv4(node.ifa_dstaddr),
                    flags: node.ifa_flags,
                });
            }
            free_ifaddrs(head);
        }
        Ok(out)
    }
}

#[cfg(not(unix))]
mod sys {
    //! Windows: no `getifaddrs`, so the `if-addrs` crate — which is what the
    //! host enumeration used before this module existed, and which C mirrors by
    //! giving Windows its own `os/WIN32/osdNetIntf.c` rather than including
    //! either shared file.
    //!
    //! `if-addrs` does not surface `ifa_flags`, so the flags are reconstructed
    //! from what it does report: an interface it lists is treated as up, and a
    //! reported broadcast address is what `IFF_BROADCAST` means. That is the
    //! same approximation the pre-existing host path made, and it is confined
    //! to the one platform that cannot do better.

    use super::{IfaceV4, flags};
    use std::io;
    use std::net::IpAddr;

    pub(super) fn enumerate() -> io::Result<Vec<IfaceV4>> {
        let ifs = if_addrs::get_if_addrs()?;
        let mut out = Vec::new();
        for iface in ifs {
            let if_addrs::IfAddr::V4(v4) = &iface.addr else {
                continue;
            };
            let IpAddr::V4(ip) = iface.ip() else {
                continue;
            };
            let mut f = flags::IFF_UP;
            if iface.is_loopback() {
                f |= flags::IFF_LOOPBACK;
            }
            if v4.broadcast.is_some() {
                f |= flags::IFF_BROADCAST;
            }
            out.push(IfaceV4 {
                name: iface.name.clone(),
                ip,
                netmask: Some(v4.netmask),
                dest: v4.broadcast,
                flags: f,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iface(flags: u32, dest: Option<Ipv4Addr>) -> IfaceV4 {
        IfaceV4 {
            name: "test0".to_string(),
            ip: Ipv4Addr::new(10, 0, 0, 5),
            netmask: Some(Ipv4Addr::new(255, 255, 255, 0)),
            dest,
            flags,
        }
    }

    /// C `osdNetIfAddrs.c:100` — a down interface is skipped even though it
    /// still carries an address and a broadcast address.
    #[test]
    fn a_down_interface_is_never_a_search_destination() {
        let down = iface(flags::IFF_BROADCAST, Some(Ipv4Addr::new(10, 0, 0, 255)));
        assert!(!down.is_eligible());
        assert_eq!(down.search_destination(), None);
    }

    /// C `osdNetIfAddrs.c:108`.
    #[test]
    fn loopback_is_never_a_search_destination() {
        let lo = iface(
            flags::IFF_UP | flags::IFF_LOOPBACK | flags::IFF_BROADCAST,
            Some(Ipv4Addr::new(127, 255, 255, 255)),
        );
        assert_eq!(lo.search_destination(), None);
    }

    /// C `osdNetIfAddrs.c:130-135`.
    #[test]
    fn a_broadcast_interface_yields_its_broadcast_address() {
        let up = iface(
            flags::IFF_UP | flags::IFF_BROADCAST,
            Some(Ipv4Addr::new(10, 0, 0, 255)),
        );
        assert_eq!(up.search_destination(), Some(Ipv4Addr::new(10, 0, 0, 255)));
    }

    /// C `osdNetIfAddrs.c:133` discards a `0.0.0.0` broadcast rather than
    /// fanning SEARCH at the wildcard address.
    #[test]
    fn an_unspecified_broadcast_address_is_discarded() {
        let up = iface(
            flags::IFF_UP | flags::IFF_BROADCAST,
            Some(Ipv4Addr::UNSPECIFIED),
        );
        assert_eq!(up.search_destination(), None);
    }

    /// C `osdNetIfAddrs.c:143-145` — the peer address, and note that C does
    /// *not* apply the `0.0.0.0` rejection on this branch.
    #[test]
    fn a_point_to_point_interface_yields_its_peer_address() {
        let p2p = iface(
            flags::IFF_UP | flags::IFF_POINTOPOINT,
            Some(Ipv4Addr::new(192, 168, 9, 1)),
        );
        assert_eq!(
            p2p.search_destination(),
            Some(Ipv4Addr::new(192, 168, 9, 1))
        );
    }

    /// C `osdNetIfAddrs.c:147-151` — "CA will not query through the interface".
    #[test]
    fn an_interface_that_is_neither_broadcast_nor_p2p_yields_nothing() {
        let odd = iface(flags::IFF_UP, Some(Ipv4Addr::new(10, 0, 0, 255)));
        assert_eq!(odd.search_destination(), None);
    }

    /// The enumeration itself must run on the host CI machines. It is not
    /// asserted to be non-empty — a container with only loopback is a valid
    /// machine — but every entry it does report must be self-consistent.
    #[test]
    fn enumeration_runs_and_reports_consistent_entries() {
        let ifaces = enumerate().expect("getifaddrs must succeed on the host");
        for iface in &ifaces {
            if iface.ip.is_loopback() {
                assert!(
                    iface.is_loopback() || iface.name.is_empty(),
                    "an interface with a loopback address must carry IFF_LOOPBACK: {iface:?}"
                );
            }
            if let Some(dest) = iface.search_destination() {
                assert!(iface.is_up(), "a destination came from a down interface");
                assert!(!iface.is_loopback(), "a destination came from loopback");
                assert!(!dest.is_unspecified() || iface.is_point_to_point());
            }
        }
    }

    /// Loopback-only machines exist (CI containers), so this asserts the
    /// fallback rather than a real address.
    #[test]
    fn local_addr_is_an_eligible_interface_or_loopback() {
        let addr = local_addr();
        if addr.is_loopback() {
            return;
        }
        let ifaces = enumerate().expect("getifaddrs must succeed on the host");
        assert!(
            ifaces.iter().any(|i| i.is_eligible() && i.ip == addr),
            "local_addr returned {addr}, which is not an eligible interface"
        );
    }

    /// Every address `broadcast_addrs` reports must be some eligible
    /// interface's destination, and the list must not repeat.
    #[test]
    fn broadcast_addrs_are_deduplicated_eligible_destinations() {
        let addrs = broadcast_addrs();
        let mut seen = Vec::new();
        for a in &addrs {
            assert!(!seen.contains(a), "broadcast_addrs repeated {a}");
            seen.push(*a);
        }
        let ifaces = enumerate().expect("getifaddrs must succeed on the host");
        for a in &addrs {
            assert!(
                ifaces.iter().any(|i| i.search_destination() == Some(*a)),
                "broadcast_addrs reported {a}, which no eligible interface names"
            );
        }
    }
}
