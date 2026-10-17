//! Who holds a port: the UDP sockets bound to it, the TCP sockets listening
//! on it.
//!
//! `/proc/net/udp` and `/proc/net/tcp` are not snapshots: a read that spans
//! more than one chunk resumes by row count, so a socket opened or closed
//! elsewhere meanwhile repeats or skips rows. Under a parallel suite a test
//! counting its own two responder sockets read 6 (the same two inodes three
//! times) or 0, and a skipped LISTEN row reads a healthy IOC as one that lost
//! its port. A `sock_diag` dump filtered on the port walks each hash bucket
//! under its lock, so a socket present for the whole dump is reported once —
//! as long as the port's rows fit one dump message (some 45 at the first
//! message's `NLMSG_GOODSIZE`). Past that the kernel drops the lock and
//! resumes by index as `/proc` does; every caller has a handful of sockets on
//! its port.

use std::io;
use std::mem::size_of;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// One socket on the port asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortHolder {
    pub local: SocketAddr,
    pub inode: u32,
}

impl PortHolder {
    /// The `/proc/<pid>/fd` link text of this socket.
    pub fn fd_link(&self) -> String {
        format!("socket:[{}]", self.inode)
    }
}

const SOCK_DIAG_BY_FAMILY: u16 = 20;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLM_F_DUMP_INTR: u16 = 0x10;

#[repr(C)]
struct NlMsgHdr {
    len: u32,
    kind: u16,
    flags: u16,
    seq: u32,
    pid: u32,
}

#[repr(C)]
struct InetDiagSockId {
    sport: [u8; 2],
    dport: [u8; 2],
    src: [u8; 16],
    dst: [u8; 16],
    ifindex: u32,
    cookie: [u32; 2],
}

#[repr(C)]
struct InetDiagReqV2 {
    family: u8,
    protocol: u8,
    ext: u8,
    pad: u8,
    states: u32,
    id: InetDiagSockId,
}

#[repr(C)]
struct InetDiagMsg {
    family: u8,
    state: u8,
    timer: u8,
    retrans: u8,
    id: InetDiagSockId,
    expires: u32,
    rqueue: u32,
    wqueue: u32,
    uid: u32,
    inode: u32,
}

/// Every IPv4 and IPv6 UDP socket bound to `port` in this network namespace.
pub fn udp_holders(port: u16) -> io::Result<Vec<PortHolder>> {
    both_families(libc::IPPROTO_UDP, !0, port)
}

/// Every IPv4 and IPv6 TCP socket listening on `port` in this network
/// namespace.
pub fn tcp_listeners(port: u16) -> io::Result<Vec<PortHolder>> {
    // TCPF_LISTEN: `1 << TCP_LISTEN`.
    both_families(libc::IPPROTO_TCP, 1 << 10, port)
}

fn both_families(protocol: libc::c_int, states: u32, port: u16) -> io::Result<Vec<PortHolder>> {
    let mut all = dump(libc::AF_INET as u8, protocol as u8, states, port)?;
    all.extend(dump(libc::AF_INET6 as u8, protocol as u8, states, port)?);
    Ok(all)
}

/// A dump interrupted by a concurrent change (`NLM_F_DUMP_INTR`) is retaken.
fn dump(family: u8, protocol: u8, states: u32, port: u16) -> io::Result<Vec<PortHolder>> {
    loop {
        if let Some(rows) = dump_once(family, protocol, states, port)? {
            return Ok(rows);
        }
    }
}

struct Fd(libc::c_int);

impl Drop for Fd {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a descriptor this value opened and owns.
        unsafe { libc::close(self.0) };
    }
}

fn dump_once(
    family: u8,
    protocol: u8,
    states: u32,
    port: u16,
) -> io::Result<Option<Vec<PortHolder>>> {
    // SAFETY: plain socket(2); the result is checked before use.
    let raw = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            libc::NETLINK_SOCK_DIAG,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = Fd(raw);

    #[repr(C)]
    struct Request {
        hdr: NlMsgHdr,
        req: InetDiagReqV2,
    }
    let request = Request {
        hdr: NlMsgHdr {
            len: size_of::<Request>() as u32,
            kind: SOCK_DIAG_BY_FAMILY,
            flags: (libc::NLM_F_REQUEST | libc::NLM_F_DUMP) as u16,
            seq: 1,
            pid: 0,
        },
        req: InetDiagReqV2 {
            family,
            protocol,
            ext: 0,
            pad: 0,
            states,
            id: InetDiagSockId {
                sport: port.to_be_bytes(),
                dport: [0; 2],
                src: [0; 16],
                dst: [0; 16],
                ifindex: 0,
                cookie: [!0; 2],
            },
        },
    };
    loop {
        // SAFETY: `request` is a live, fully initialised `repr(C)` value of
        // the length passed.
        let sent = unsafe {
            libc::send(
                fd.0,
                (&request as *const Request).cast(),
                size_of::<Request>(),
                0,
            )
        };
        if sent >= 0 {
            break;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }

    let mut rows = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        // SAFETY: `buf` is writable for its whole length.
        let n = unsafe { libc::recv(fd.0, buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        let mut at = 0usize;
        let n = n as usize;
        while at + size_of::<NlMsgHdr>() <= n {
            // SAFETY: a whole header lies at `at`; read unaligned.
            let hdr: NlMsgHdr = unsafe { std::ptr::read_unaligned(buf[at..].as_ptr().cast()) };
            let len = hdr.len as usize;
            if len < size_of::<NlMsgHdr>() || at + len > n {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "short netlink message",
                ));
            }
            if hdr.flags & NLM_F_DUMP_INTR != 0 {
                return Ok(None);
            }
            let body = &buf[at + size_of::<NlMsgHdr>()..at + len];
            match hdr.kind {
                NLMSG_DONE => return Ok(Some(rows)),
                NLMSG_ERROR => {
                    let Some(errno) = body.first_chunk::<4>().map(|b| i32::from_ne_bytes(*b))
                    else {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "short netlink error message",
                        ));
                    };
                    return Err(io::Error::from_raw_os_error(-errno));
                }
                SOCK_DIAG_BY_FAMILY if body.len() >= size_of::<InetDiagMsg>() => {
                    // SAFETY: `body` holds a whole `inet_diag_msg`.
                    let m: InetDiagMsg = unsafe { std::ptr::read_unaligned(body.as_ptr().cast()) };
                    let ip = if m.family == libc::AF_INET as u8 {
                        IpAddr::V4(Ipv4Addr::new(
                            m.id.src[0],
                            m.id.src[1],
                            m.id.src[2],
                            m.id.src[3],
                        ))
                    } else {
                        IpAddr::V6(Ipv6Addr::from(m.id.src))
                    };
                    rows.push(PortHolder {
                        local: SocketAddr::new(ip, u16::from_be_bytes(m.id.sport)),
                        inode: m.inode,
                    });
                }
                _ => {}
            }
            // NLMSG_ALIGN
            at += (len + 3) & !3;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;

    fn inode_of(s: &impl std::os::fd::AsRawFd) -> u32 {
        let link = std::fs::read_link(format!("/proc/self/fd/{}", s.as_raw_fd())).unwrap();
        let link = link.to_string_lossy();
        link.trim_start_matches("socket:[")
            .trim_end_matches(']')
            .parse()
            .unwrap()
    }

    /// The rows of `rows` whose inode is in `inodes`, by inode, once every row
    /// has been checked to appear once. A port picked by binding one address
    /// is free on that address only: another process may hold the number on
    /// another one (a caRepeater on the NIC's address did), and is rightly
    /// listed.
    fn own(rows: &[PortHolder], inodes: &[u32]) -> Vec<PortHolder> {
        let mut seen: Vec<u32> = rows.iter().map(|h| h.inode).collect();
        seen.sort_unstable();
        assert!(
            seen.windows(2).all(|w| w[0] != w[1]),
            "a repeated row: {rows:?}"
        );
        let mut mine: Vec<PortHolder> = rows
            .iter()
            .filter(|h| inodes.contains(&h.inode))
            .cloned()
            .collect();
        mine.sort_by_key(|h| h.inode);
        mine
    }

    /// The port's sockets, ours among them, each once, with its address and
    /// inode; a closed one is gone.
    #[test]
    fn a_port_lists_its_own_sockets_once() {
        let a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = a.local_addr().unwrap().port();
        let b = UdpSocket::bind(("127.0.0.2", port)).unwrap();
        let inodes = [inode_of(&a), inode_of(&b)];
        let got = own(&udp_holders(port).unwrap(), &inodes);
        let mut want = vec![
            PortHolder {
                local: a.local_addr().unwrap(),
                inode: inode_of(&a),
            },
            PortHolder {
                local: b.local_addr().unwrap(),
                inode: inode_of(&b),
            },
        ];
        want.sort_by_key(|h| h.inode);
        assert_eq!(got, want);
        drop(b);
        assert_eq!(
            own(&udp_holders(port).unwrap(), &inodes),
            vec![
                want.into_iter()
                    .find(|h| h.local.ip() == Ipv4Addr::LOCALHOST)
                    .unwrap()
            ]
        );
    }

    /// The boundary the `/proc/net/udp` scan failed at: while other threads
    /// open and close sockets as fast as they can, two held sockets read
    /// once each, every time.
    #[test]
    fn a_held_pair_reads_as_two_under_socket_churn() {
        let a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = a.local_addr().unwrap().port();
        let b = UdpSocket::bind(("127.0.0.2", port)).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let churn: Vec<_> = (0..4)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let v: Vec<_> = (0..64)
                            .map(|_| UdpSocket::bind("127.0.0.1:0").unwrap())
                            .collect();
                        drop(v);
                    }
                })
            })
            .collect();
        let inodes = [inode_of(&a), inode_of(&b)];
        for _ in 0..500 {
            assert_eq!(own(&udp_holders(port).unwrap(), &inodes).len(), 2);
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for t in churn {
            t.join().unwrap();
        }
        drop((a, b));
    }

    /// A TCP listener reads as one, v4 and v6 alike; a connection on its
    /// port, the connection's other end and a closed listener read as none.
    #[test]
    fn a_tcp_port_lists_its_listeners_only() {
        let v4 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = v4.local_addr().unwrap().port();
        let client = std::net::TcpStream::connect(v4.local_addr().unwrap()).unwrap();
        let (served, _) = v4.accept().unwrap();
        let ours = [inode_of(&v4), inode_of(&client), inode_of(&served)];
        let got = own(&tcp_listeners(port).unwrap(), &ours);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].local, v4.local_addr().unwrap());
        assert_eq!(
            own(
                &tcp_listeners(client.local_addr().unwrap().port()).unwrap(),
                &ours
            ),
            vec![]
        );
        drop(v4);
        assert_eq!(own(&tcp_listeners(port).unwrap(), &ours), vec![]);

        let v6 = std::net::TcpListener::bind("[::1]:0").unwrap();
        let got = own(
            &tcp_listeners(v6.local_addr().unwrap().port()).unwrap(),
            &[inode_of(&v6)],
        );
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].local, v6.local_addr().unwrap());
    }

    /// A listener on 127.0.0.1:`port` (0: any) with `SO_REUSEPORT`, so
    /// several share the port's one listening-hash bucket.
    fn reuseport_listener(port: u16) -> std::net::TcpListener {
        use std::os::fd::FromRawFd;
        // SAFETY: plain socket(2)/setsockopt(2); the fd is checked, then owned
        // by the returned listener.
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        assert!(fd >= 0, "{}", io::Error::last_os_error());
        let l = unsafe { std::net::TcpListener::from_raw_fd(fd) };
        let one: libc::c_int = 1;
        // SAFETY: `one` outlives the call and is the length passed.
        let rc = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEPORT,
                (&one as *const libc::c_int).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        assert_eq!(rc, 0, "{}", io::Error::last_os_error());
        let addr = libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: port.to_be(),
            sin_addr: libc::in_addr {
                s_addr: u32::from(Ipv4Addr::LOCALHOST).to_be(),
            },
            sin_zero: [0; 8],
        };
        // SAFETY: `addr` is a live sockaddr_in of the length passed.
        let rc = unsafe {
            libc::bind(
                fd,
                (&addr as *const libc::sockaddr_in).cast(),
                size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        assert_eq!(rc, 0, "{}", io::Error::last_os_error());
        // SAFETY: plain listen(2) on the bound socket.
        assert_eq!(
            unsafe { libc::listen(fd, 8) },
            0,
            "{}",
            io::Error::last_os_error()
        );
        l
    }

    /// The boundary `listens_on_tcp` failed at: other listeners opening and
    /// closing in the held one's own bucket, which is where a
    /// `/proc/net/tcp` read resumes by offset (it missed the held listener
    /// 17 times in 2000 reads under this churn). Kept under one dump's worth
    /// of rows, past which a sock_diag dump resumes by index too.
    #[test]
    fn a_held_listener_is_seen_once_under_churn_in_its_bucket() {
        let held = reuseport_listener(0);
        let port = held.local_addr().unwrap().port();
        let inode = {
            use std::os::fd::AsRawFd;
            std::fs::read_link(format!("/proc/self/fd/{}", held.as_raw_fd()))
                .unwrap()
                .to_string_lossy()
                .into_owned()
        };
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let churn: Vec<_> = (0..2)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let v: Vec<_> = (0..8).map(|_| reuseport_listener(port)).collect();
                        drop(v);
                    }
                })
            })
            .collect();
        for _ in 0..2000 {
            let seen = tcp_listeners(port)
                .unwrap()
                .iter()
                .filter(|h| h.fd_link() == inode)
                .count();
            assert_eq!(seen, 1);
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for t in churn {
            t.join().unwrap();
        }
    }
}
