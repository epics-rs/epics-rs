//! One UDP datagram with the ancillary data pvxs `recvfromx` reads: its
//! destination address and arrival interface (`os/default/osdSockExt.cpp:71-200`,
//! `os/WIN32/osdSockExt.cpp:85-140`), and on Linux the `SO_RXQ_OVFL` drop count.
//!
//! The one owner of that receive path for the client collector, both server
//! search sockets and the exec-backend SEARCH receive pump, so no platform can
//! have it on one and not the other.
//!
//! * Linux / Android: `IP_PKTINFO` (`in_pktinfo`: destination and interface).
//! * Other Unix: `IP_RECVDSTADDR` for the destination — the one v4 option
//!   `libc` defines across the BSD family, where pvxs uses `IP_PKTINFO` on
//!   Apple/NetBSD and `IP_ORIGDSTADDR` elsewhere — and, where the OS has it,
//!   `IP_RECVIF` for the interface (`sockaddr_dl.sdl_index`), as pvxs does.
//! * Windows: `IP_PKTINFO` through `WSARecvMsg`, an extension function
//!   resolved once (pvxs `oseDoOnce`).
//! * v6, everywhere: `IPV6_PKTINFO`.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

#[cfg(unix)]
pub use std::os::fd::AsFd as AsSock;
#[cfg(windows)]
pub use std::os::windows::io::AsSocket as AsSock;

/// One received datagram.
#[derive(Debug, Clone, Copy)]
pub struct Datagram {
    pub len: usize,
    pub src: SocketAddr,
    /// The destination the datagram was sent to: a unicast, broadcast or
    /// group address. `None` when the OS attached none.
    pub dest: Option<IpAddr>,
    /// The arrival interface's index, 0 when the OS reported none — the same
    /// "no index" as `IfaceInfo::index`.
    pub ifindex: u32,
    /// `SO_RXQ_OVFL`'s running drop count; 0 off Linux.
    pub drops: u32,
}

impl Datagram {
    /// The v4 `(destination, interface index)` pair the server's
    /// `SearchListeners::arrival` routes by.
    pub fn v4_pktinfo(&self) -> Option<(Ipv4Addr, u32)> {
        match self.dest {
            Some(IpAddr::V4(dest)) => Some((dest, self.ifindex)),
            _ => None,
        }
    }
}

/// Ask the OS to attach each datagram's destination and arrival interface
/// (pvxs `evsocket::enable_IP_PKTINFO`).
#[cfg(unix)]
pub fn enable(sock: &impl AsSock, is_v6: bool) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let fd = sock.as_fd().as_raw_fd();
    let set = |level: libc::c_int, name: libc::c_int| -> io::Result<()> {
        let on: libc::c_int = 1;
        // SAFETY: `fd` is borrowed from `sock` for this call; `on` outlives it.
        let rc = unsafe {
            libc::setsockopt(
                fd,
                level,
                name,
                &on as *const _ as *const libc::c_void,
                std::mem::size_of_val(&on) as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    };
    if is_v6 {
        return set(libc::IPPROTO_IPV6, libc::IPV6_RECVPKTINFO);
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    set(libc::IPPROTO_IP, libc::IP_PKTINFO)?;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    set(libc::IPPROTO_IP, libc::IP_RECVDSTADDR)?;
    #[cfg(any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    set(libc::IPPROTO_IP, libc::IP_RECVIF)?;
    Ok(())
}

/// One `recvmsg`. A socket with nothing queued returns `WouldBlock` when
/// non-blocking, as `recv_from` does.
#[cfg(unix)]
pub fn recv(sock: &impl AsSock, buf: &mut [u8]) -> io::Result<Datagram> {
    use std::mem::{size_of, zeroed};
    use std::os::fd::AsRawFd;
    // SAFETY: every pointer handed to `recvmsg` points at a local that
    // outlives the call, `control` is 8-aligned for `cmsghdr` and holds every
    // control message `enable` asks for, and the cmsg walk stays inside
    // `msg_controllen`.
    unsafe {
        let mut storage: libc::sockaddr_storage = zeroed();
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut libc::c_void,
            iov_len: buf.len(),
        };
        let mut control = [0u64; 16];
        let mut msg: libc::msghdr = zeroed();
        msg.msg_name = &mut storage as *mut _ as *mut libc::c_void;
        msg.msg_namelen = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = size_of::<[u64; 16]>() as _;
        let n = libc::recvmsg(sock.as_fd().as_raw_fd(), &mut msg, 0);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut out = Datagram {
            len: n as usize,
            src: sockaddr_to_socketaddr(&storage)?,
            dest: None,
            ifindex: 0,
            drops: 0,
        };
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            decode_cmsg(&*cmsg, libc::CMSG_DATA(cmsg), &mut out);
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
        Ok(out)
    }
}

/// Fold one control message into `out`.
///
/// # Safety
/// `data` is `CMSG_DATA` of `hdr`, which `recvmsg` filled.
#[cfg(unix)]
unsafe fn decode_cmsg(hdr: &libc::cmsghdr, data: *const libc::c_uchar, out: &mut Datagram) {
    use std::ptr::read_unaligned;
    // SAFETY (whole body): each read is of the payload type its
    // `(level, type)` carries, after checking the payload is that long.
    // `as _`: `cmsg_len` is `usize` on glibc but `socklen_t` on BSD and musl.
    let fits = |size: usize| hdr.cmsg_len >= unsafe { libc::CMSG_LEN(size as u32) } as _;
    match (hdr.cmsg_level, hdr.cmsg_type) {
        (libc::IPPROTO_IPV6, libc::IPV6_PKTINFO) if fits(size_of::<libc::in6_pktinfo>()) => {
            let info = unsafe { read_unaligned(data as *const libc::in6_pktinfo) };
            out.dest = Some(IpAddr::V6(Ipv6Addr::from(info.ipi6_addr.s6_addr)));
            out.ifindex = info.ipi6_ifindex as u32;
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        (libc::IPPROTO_IP, libc::IP_PKTINFO) if fits(size_of::<libc::in_pktinfo>()) => {
            let info = unsafe { read_unaligned(data as *const libc::in_pktinfo) };
            out.dest = Some(IpAddr::V4(Ipv4Addr::from(
                info.ipi_addr.s_addr.to_ne_bytes(),
            )));
            out.ifindex = info.ipi_ifindex as u32;
        }
        #[cfg(target_os = "linux")]
        (libc::SOL_SOCKET, libc::SO_RXQ_OVFL) if fits(size_of::<u32>()) => {
            out.drops = unsafe { read_unaligned(data as *const u32) };
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        (libc::IPPROTO_IP, libc::IP_RECVDSTADDR) if fits(size_of::<libc::in_addr>()) => {
            let addr = unsafe { read_unaligned(data as *const libc::in_addr) };
            out.dest = Some(IpAddr::V4(Ipv4Addr::from(addr.s_addr.to_ne_bytes())));
        }
        // `sdl_index` is the only field read, and it sits before the
        // variable-length tail, so a short `sockaddr_dl` is still read in bounds.
        #[cfg(any(
            target_vendor = "apple",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "netbsd",
            target_os = "openbsd"
        ))]
        (libc::IPPROTO_IP, libc::IP_RECVIF)
            if fits(
                std::mem::offset_of!(libc::sockaddr_dl, sdl_index) + size_of::<libc::c_ushort>(),
            ) =>
        {
            let off = std::mem::offset_of!(libc::sockaddr_dl, sdl_index);
            out.ifindex =
                u32::from(unsafe { read_unaligned(data.add(off) as *const libc::c_ushort) });
        }
        _ => {}
    }
}

#[cfg(unix)]
fn sockaddr_to_socketaddr(storage: &libc::sockaddr_storage) -> io::Result<SocketAddr> {
    match storage.ss_family as libc::c_int {
        libc::AF_INET => {
            // SAFETY: `ss_family == AF_INET` ⟹ the storage holds a `sockaddr_in`.
            let sin = unsafe { &*(storage as *const _ as *const libc::sockaddr_in) };
            let ip = Ipv4Addr::from(sin.sin_addr.s_addr.to_ne_bytes());
            Ok(SocketAddr::V4(SocketAddrV4::new(
                ip,
                u16::from_be(sin.sin_port),
            )))
        }
        libc::AF_INET6 => {
            // SAFETY: `ss_family == AF_INET6` ⟹ the storage holds a `sockaddr_in6`.
            let sin6 = unsafe { &*(storage as *const _ as *const libc::sockaddr_in6) };
            Ok(SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::from(sin6.sin6_addr.s6_addr),
                u16::from_be(sin6.sin6_port),
                sin6.sin6_flowinfo,
                sin6.sin6_scope_id,
            )))
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("recvmsg returned unsupported address family {other}"),
        )),
    }
}

/// Ask Winsock to attach each datagram's destination and arrival interface,
/// and resolve `WSARecvMsg` now so the receive path cannot fail on first use
/// (pvxs `enable_IP_PKTINFO` + `oseDoOnce`, `os/WIN32/osdSockExt.cpp`).
#[cfg(windows)]
pub fn enable(sock: &impl AsSock, is_v6: bool) -> io::Result<()> {
    use windows_sys::Win32::Networking::WinSock::{
        IP_PKTINFO, IPPROTO_IP, IPPROTO_IPV6, IPV6_PKTINFO, setsockopt,
    };
    let raw = win::raw(sock);
    let on: i32 = 1;
    let (level, name) = if is_v6 {
        (IPPROTO_IPV6, IPV6_PKTINFO)
    } else {
        (IPPROTO_IP, IP_PKTINFO)
    };
    // SAFETY: `raw` is borrowed from `sock` for this call; `on` outlives it.
    let rc = unsafe {
        setsockopt(
            raw,
            level,
            name,
            &on as *const i32 as *const u8,
            std::mem::size_of_val(&on) as i32,
        )
    };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(win::last_wsa_error()));
    }
    win::resolve_wsarecvmsg(raw)?;
    Ok(())
}

/// One `WSARecvMsg`. A socket with nothing queued returns `WouldBlock` when
/// non-blocking, as `recv_from` does.
#[cfg(windows)]
pub fn recv(sock: &impl AsSock, buf: &mut [u8]) -> io::Result<Datagram> {
    // SAFETY: `buf` is borrowed for the one call.
    unsafe { win::recvmsg_once(win::raw(sock), buf) }
}

/// The Winsock analog of the Unix `recvmsg`/cmsg path, pvxs
/// `os/WIN32/osdSockExt.cpp` (`recvfromx::call`). Scoped to 64-bit Windows
/// (the matrix builds x86_64 + aarch64), where the cmsg header and data
/// alignments both equal `align_of::<CMSGHDR>()`.
#[cfg(windows)]
mod win {
    use super::*;
    use core::ffi::c_void;
    use std::mem::size_of;
    use std::ptr::{null, null_mut, read_unaligned};
    use std::sync::OnceLock;
    use windows_sys::Win32::Networking::WinSock::{
        AF_INET, AF_INET6, CMSGHDR, IN_PKTINFO, IN6_PKTINFO, IP_PKTINFO, IPPROTO_IP, IPPROTO_IPV6,
        IPV6_PKTINFO, SIO_GET_EXTENSION_FUNCTION_POINTER, SOCKADDR_IN, SOCKADDR_IN6,
        SOCKADDR_STORAGE, SOCKET, WSABUF, WSAGetLastError, WSAID_WSARECVMSG, WSAIoctl, WSAMSG,
    };
    use windows_sys::core::GUID;

    pub(super) fn raw(sock: &impl AsSock) -> SOCKET {
        use std::os::windows::io::AsRawSocket;
        sock.as_socket().as_raw_socket() as SOCKET
    }

    /// `WSARecvMsg` extension fn. Its trailing `OVERLAPPED*` and completion
    /// routine are always null here, so they are typed as opaque pointers to
    /// avoid naming extra types.
    type WsaRecvMsgFn =
        unsafe extern "system" fn(SOCKET, *mut WSAMSG, *mut u32, *mut c_void, *mut c_void) -> i32;

    static WSARECVMSG: OnceLock<WsaRecvMsgFn> = OnceLock::new();

    pub(super) fn last_wsa_error() -> i32 {
        // SAFETY: `WSAGetLastError` has no preconditions.
        unsafe { WSAGetLastError() }
    }

    /// Resolve and cache the `WSARecvMsg` extension-fn pointer via
    /// `WSAIoctl(SIO_GET_EXTENSION_FUNCTION_POINTER)` (pvxs `oseDoOnce`).
    pub(super) fn resolve_wsarecvmsg(sock: SOCKET) -> io::Result<WsaRecvMsgFn> {
        if let Some(f) = WSARECVMSG.get() {
            return Ok(*f);
        }
        let guid: GUID = WSAID_WSARECVMSG;
        let mut func: Option<WsaRecvMsgFn> = None;
        let mut nout: u32 = 0;
        // SAFETY: `guid`/`func`/`nout` outlive the call; the sizes match.
        let rc = unsafe {
            WSAIoctl(
                sock,
                SIO_GET_EXTENSION_FUNCTION_POINTER,
                &guid as *const GUID as *const c_void,
                size_of::<GUID>() as u32,
                &mut func as *mut Option<WsaRecvMsgFn> as *mut c_void,
                size_of::<Option<WsaRecvMsgFn>>() as u32,
                &mut nout,
                null_mut(),
                None,
            )
        };
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(last_wsa_error()));
        }
        let func = func.ok_or_else(|| {
            io::Error::new(io::ErrorKind::Unsupported, "WSARecvMsg pointer unavailable")
        })?;
        let _ = WSARECVMSG.set(func);
        Ok(func)
    }

    /// One `WSARecvMsg`.
    ///
    /// # Safety
    /// `buf` must be writable for its full length; the call borrows it.
    pub(super) unsafe fn recvmsg_once(sock: SOCKET, buf: &mut [u8]) -> io::Result<Datagram> {
        let recv = resolve_wsarecvmsg(sock)?;

        // SAFETY: zeroed sockaddr/WSAMSG are valid initial states; every
        // pointer below refers to a stack local that outlives the call.
        let mut storage: SOCKADDR_STORAGE = unsafe { std::mem::zeroed() };
        let mut iov = WSABUF {
            len: buf.len() as u32,
            buf: buf.as_mut_ptr(),
        };
        // Room for one in6_pktinfo cmsg (header + 20 bytes, aligned).
        let mut cbuf = [0u64; 16];
        let mut msg: WSAMSG = unsafe { std::mem::zeroed() };
        msg.name = &mut storage as *mut SOCKADDR_STORAGE as *mut _;
        msg.namelen = size_of::<SOCKADDR_STORAGE>() as i32;
        msg.lpBuffers = &mut iov;
        msg.dwBufferCount = 1;
        msg.Control = WSABUF {
            len: size_of::<[u64; 16]>() as u32,
            buf: cbuf.as_mut_ptr() as *mut u8,
        };
        msg.dwFlags = 0;

        let mut nrx: u32 = 0;
        // SAFETY: `recv` is the resolved WSARecvMsg; pointers outlive it.
        let rc = unsafe { recv(sock, &mut msg, &mut nrx, null_mut(), null_mut()) };
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(last_wsa_error()));
        }

        let mut out = Datagram {
            len: nrx as usize,
            src: sockaddr_to_socketaddr(&storage)?,
            dest: None,
            ifindex: 0,
            drops: 0,
        };
        let mut cmsg = wsa_cmsg_firsthdr(&msg);
        while !cmsg.is_null() {
            // SAFETY: `cmsg` is non-null and within the control buffer
            // `WSARecvMsg` filled.
            let hdr = unsafe { &*cmsg };
            let data = unsafe { wsa_cmsg_data(cmsg) };
            if hdr.cmsg_level == IPPROTO_IPV6 && hdr.cmsg_type == IPV6_PKTINFO {
                // SAFETY: an IPV6_PKTINFO cmsg payload is an in6_pktinfo.
                let info = unsafe { read_unaligned(data as *const IN6_PKTINFO) };
                out.dest = Some(IpAddr::V6(Ipv6Addr::from(unsafe { info.ipi6_addr.u.Byte })));
                out.ifindex = info.ipi6_ifindex;
            } else if hdr.cmsg_level == IPPROTO_IP && hdr.cmsg_type == IP_PKTINFO {
                // SAFETY: an IP_PKTINFO cmsg payload is an in_pktinfo.
                let info = unsafe { read_unaligned(data as *const IN_PKTINFO) };
                out.dest = Some(IpAddr::V4(Ipv4Addr::from(unsafe {
                    info.ipi_addr.S_un.S_addr.to_ne_bytes()
                })));
                out.ifindex = info.ipi_ifindex;
            }
            // SAFETY: `cmsg` is a valid header from this same control buffer.
            cmsg = unsafe { wsa_cmsg_nxthdr(&msg, cmsg) };
        }
        Ok(out)
    }

    #[inline]
    fn cmsg_align(len: usize) -> usize {
        let a = std::mem::align_of::<CMSGHDR>();
        (len + a - 1) & !(a - 1)
    }

    /// `WSA_CMSG_DATA`: payload pointer for a cmsg header.
    ///
    /// # Safety
    /// `cmsg` must point at a valid `CMSGHDR` inside the control buffer.
    unsafe fn wsa_cmsg_data(cmsg: *const CMSGHDR) -> *const u8 {
        (cmsg as *const u8).wrapping_add(cmsg_align(size_of::<CMSGHDR>()))
    }

    /// `WSA_CMSG_FIRSTHDR`.
    fn wsa_cmsg_firsthdr(msg: &WSAMSG) -> *const CMSGHDR {
        if (msg.Control.len as usize) >= size_of::<CMSGHDR>() {
            msg.Control.buf as *const CMSGHDR
        } else {
            null()
        }
    }

    /// `WSA_CMSG_NXTHDR`. Bounds the walk by `Control.len` using integer
    /// arithmetic so no out-of-range pointer is ever formed.
    ///
    /// # Safety
    /// `cmsg`, when non-null, must point at a valid `CMSGHDR`.
    unsafe fn wsa_cmsg_nxthdr(msg: &WSAMSG, cmsg: *const CMSGHDR) -> *const CMSGHDR {
        if cmsg.is_null() {
            return wsa_cmsg_firsthdr(msg);
        }
        // SAFETY: caller guarantees `cmsg` is a valid header.
        let cmsg_len = unsafe { (*cmsg).cmsg_len };
        let base = msg.Control.buf as usize;
        let end = base + msg.Control.len as usize;
        let next = (cmsg as usize) + cmsg_align(cmsg_len);
        if next + size_of::<CMSGHDR>() > end {
            null()
        } else {
            next as *const CMSGHDR
        }
    }

    /// Convert a `WSARecvMsg`-filled `SOCKADDR_STORAGE` into a [`SocketAddr`].
    fn sockaddr_to_socketaddr(storage: &SOCKADDR_STORAGE) -> io::Result<SocketAddr> {
        match storage.ss_family {
            AF_INET => {
                // SAFETY: ss_family == AF_INET ⟹ storage is a SOCKADDR_IN.
                let sin = unsafe { &*(storage as *const SOCKADDR_STORAGE as *const SOCKADDR_IN) };
                let ip = Ipv4Addr::from(unsafe { sin.sin_addr.S_un.S_addr }.to_ne_bytes());
                Ok(SocketAddr::V4(SocketAddrV4::new(
                    ip,
                    u16::from_be(sin.sin_port),
                )))
            }
            AF_INET6 => {
                // SAFETY: ss_family == AF_INET6 ⟹ storage is a SOCKADDR_IN6.
                let sin6 = unsafe { &*(storage as *const SOCKADDR_STORAGE as *const SOCKADDR_IN6) };
                let ip = Ipv6Addr::from(unsafe { sin6.sin6_addr.u.Byte });
                let scope = unsafe { sin6.Anonymous.sin6_scope_id };
                Ok(SocketAddr::V6(SocketAddrV6::new(
                    ip,
                    u16::from_be(sin6.sin6_port),
                    sin6.sin6_flowinfo,
                    scope,
                )))
            }
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("WSARecvMsg returned unsupported address family {other}"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The arrival a server routes by: the destination and an interface
    /// index, on every platform this path exists for, not only Linux.
    #[test]
    fn a_datagram_carries_its_destination_and_interface() {
        let rx = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        enable(&rx, false).unwrap();
        rx.set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .unwrap();
        let port = rx.local_addr().unwrap().port();
        let tx = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        tx.send_to(b"arrival", (Ipv4Addr::LOCALHOST, port)).unwrap();

        let mut buf = [0u8; 64];
        let d = recv(&rx, &mut buf).unwrap();
        assert_eq!(&buf[..d.len], b"arrival");
        assert_eq!(d.src, tx.local_addr().unwrap());
        assert_eq!(d.dest, Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert_ne!(d.ifindex, 0, "no arrival interface reported");
    }
}
