//! pvxs `evsocket::mcast_prep_sendto` (`evhelper.cpp:573-609`): before each
//! send to a multicast destination, the TTL (hop limit) and the egress
//! interface that destination's endpoint names, `0.0.0.0` / index 0 when it
//! names none.
//!
//! One spelling for every socket that sends to an endpoint list — the PVA
//! client SEARCH and the PVA server beacon, on every backend and target — so
//! an arm cannot set the TTL and forget the interface.

use std::io;
use std::net::Ipv4Addr;

/// A socket these options can be set on.
#[cfg(unix)]
pub trait McastSocket: std::os::fd::AsFd {}
#[cfg(unix)]
impl<T: std::os::fd::AsFd> McastSocket for T {}
#[cfg(windows)]
pub trait McastSocket: std::os::windows::io::AsSocket {}
#[cfg(windows)]
impl<T: std::os::windows::io::AsSocket> McastSocket for T {}

/// `IP_MULTICAST_TTL` then `IP_MULTICAST_IF`. pvxs logs each failure and
/// sends anyway; the caller decides the same way.
pub fn mcast_prep_v4<S: McastSocket>(sock: &S, ttl: u32, iface: Ipv4Addr) -> io::Result<()> {
    let ttl = set_int(sock, sys::IPPROTO_IP, sys::IP_MULTICAST_TTL, ttl as i32);
    let ifr = sys::set_if_v4(sock, iface);
    ttl.and(ifr)
}

/// `IPV6_MULTICAST_HOPS` then `IPV6_MULTICAST_IF`.
pub fn mcast_prep_v6<S: McastSocket>(sock: &S, hops: u32, index: u32) -> io::Result<()> {
    let hops = set_int(
        sock,
        sys::IPPROTO_IPV6,
        sys::IPV6_MULTICAST_HOPS,
        hops as i32,
    );
    let ifr = set_int(
        sock,
        sys::IPPROTO_IPV6,
        sys::IPV6_MULTICAST_IF,
        index as i32,
    );
    hops.and(ifr)
}

fn set_int<S: McastSocket>(sock: &S, level: i32, name: i32, value: i32) -> io::Result<()> {
    sys::setsockopt(sock, level, name, &value.to_ne_bytes())
}

#[cfg(unix)]
mod sys {
    use super::McastSocket;
    use std::io;
    use std::net::Ipv4Addr;
    use std::os::fd::AsRawFd;

    pub(super) const IPPROTO_IP: i32 = libc::IPPROTO_IP;
    pub(super) const IP_MULTICAST_TTL: i32 = libc::IP_MULTICAST_TTL;
    pub(super) const IPPROTO_IPV6: i32 = libc::IPPROTO_IPV6;
    pub(super) const IPV6_MULTICAST_HOPS: i32 = libc::IPV6_MULTICAST_HOPS;
    pub(super) const IPV6_MULTICAST_IF: i32 = libc::IPV6_MULTICAST_IF;

    /// The `struct in_addr` form, which every BSD-derived stack (RTEMS
    /// libbsd, VxWorks, macOS) takes and Linux accepts beside `ip_mreqn`.
    pub(super) fn set_if_v4<S: McastSocket>(sock: &S, iface: Ipv4Addr) -> io::Result<()> {
        setsockopt(
            sock,
            libc::IPPROTO_IP,
            libc::IP_MULTICAST_IF,
            &iface.octets(),
        )
    }

    pub(super) fn setsockopt<S: McastSocket>(
        sock: &S,
        level: i32,
        name: i32,
        value: &[u8],
    ) -> io::Result<()> {
        // SAFETY: the fd is borrowed from `sock` for the call; `value` is a
        // live buffer and the length passed is its exact length.
        let rc = unsafe {
            libc::setsockopt(
                sock.as_fd().as_raw_fd(),
                level,
                name,
                value.as_ptr().cast(),
                value.len() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
mod sys {
    use super::McastSocket;
    use std::io;
    use std::net::Ipv4Addr;
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock;

    pub(super) const IPPROTO_IP: i32 = WinSock::IPPROTO_IP;
    pub(super) const IP_MULTICAST_TTL: i32 = WinSock::IP_MULTICAST_TTL;
    pub(super) const IPPROTO_IPV6: i32 = WinSock::IPPROTO_IPV6;
    pub(super) const IPV6_MULTICAST_HOPS: i32 = WinSock::IPV6_MULTICAST_HOPS;
    pub(super) const IPV6_MULTICAST_IF: i32 = WinSock::IPV6_MULTICAST_IF;

    pub(super) fn set_if_v4<S: McastSocket>(sock: &S, iface: Ipv4Addr) -> io::Result<()> {
        setsockopt(
            sock,
            WinSock::IPPROTO_IP,
            WinSock::IP_MULTICAST_IF,
            &iface.octets(),
        )
    }

    pub(super) fn setsockopt<S: McastSocket>(
        sock: &S,
        level: i32,
        name: i32,
        value: &[u8],
    ) -> io::Result<()> {
        // SAFETY: the socket is borrowed from `sock` for the call; `value` is
        // a live buffer and the length passed is its exact length.
        let rc = unsafe {
            WinSock::setsockopt(
                sock.as_socket().as_raw_socket() as WinSock::SOCKET,
                level,
                name,
                value.as_ptr(),
                value.len() as i32,
            )
        };
        if rc != 0 {
            // SAFETY: no preconditions.
            return Err(io::Error::from_raw_os_error(unsafe {
                WinSock::WSAGetLastError()
            }));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The options land: the kernel reads back what was set.
    #[cfg(not(epics_embedded_target))]
    #[test]
    fn prep_sets_ttl_and_interface() {
        let sock = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        mcast_prep_v4(&sock, 7, Ipv4Addr::LOCALHOST).unwrap();
        let s = socket2::SockRef::from(&sock);
        assert_eq!(s.multicast_ttl_v4().unwrap(), 7);
        assert_eq!(s.multicast_if_v4().unwrap(), Ipv4Addr::LOCALHOST);
        mcast_prep_v4(&sock, 1, Ipv4Addr::UNSPECIFIED).unwrap();
        assert_eq!(s.multicast_ttl_v4().unwrap(), 1);
        assert_eq!(s.multicast_if_v4().unwrap(), Ipv4Addr::UNSPECIFIED);
    }

    /// Darwin refuses index 0 with EINVAL (`in6p_set_multicast_if`:
    /// `IF_INDEX_IN_RANGE`), which pvxs logs before sending anyway; the hops
    /// are set first either way.
    #[cfg(not(epics_embedded_target))]
    #[test]
    fn prep_v6_sets_hops_and_index() {
        let Ok(sock) = std::net::UdpSocket::bind((std::net::Ipv6Addr::UNSPECIFIED, 0)) else {
            return; // no v6 stack on this host
        };
        let prep = mcast_prep_v6(&sock, 3, 0);
        let s = socket2::SockRef::from(&sock);
        assert_eq!(s.multicast_hops_v6().unwrap(), 3);
        if cfg!(target_vendor = "apple") {
            assert_eq!(prep.unwrap_err().kind(), io::ErrorKind::InvalidInput);
        } else {
            prep.unwrap();
            assert_eq!(s.multicast_if_v6().unwrap(), 0);
        }
    }
}
