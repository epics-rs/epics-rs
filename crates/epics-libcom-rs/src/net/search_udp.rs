//! The client's UDP SEARCH socket, on **every** target.
//!
//! One type, two implementations, chosen by whether a tokio reactor exists:
//!
//! * `tokio_backend` — delegates to [`AsyncUdpV4`](super::async_udp_v4::AsyncUdpV4),
//!   the per-NIC bundle. Unchanged behaviour: the same sockets, the same
//!   `IP_PKTINFO` receive metadata, the same per-NIC fanout.
//! * `exec_backend` — **one wildcard socket** plus a receive pump thread.
//!
//! # Why the exec arm is one socket
//!
//! Because that is libca's own model. `udpiiu.cpp:174` creates a *single*
//! datagram socket bound to `INADDR_ANY:0` and reaches every subnet through
//! the address list, not through a socket per NIC — the per-NIC bundle is an
//! epics-rs elaboration over libca that buys accurate `RecvMeta.iface_ip` and
//! a `255.255.255.255` fanout. Neither is a parity requirement, and neither is
//! reachable on the embedded targets anyway: `tokio::net`, `socket2` and
//! `if-addrs` build for none of them.
//!
//! The one thing the bundle bought that the target still needs is *reaching
//! every subnet's broadcast address*. That comes from
//! [`super::iface_v4::broadcast_addrs`] instead — the same list C's
//! `osiSockDiscoverBroadcastAddresses` builds, which is what libca sends its
//! SEARCHes to. So the exec arm is closer to C here than the host arm is.
//!
//! # Why the exec arm needs a thread
//!
//! There is no reactor to register the socket with: `runtime::task::spawn` runs
//! futures on a callback-pool worker, and `tokio::net::UdpSocket` panics there
//! ("there is no reactor running"). A blocking `recv_from` on a dedicated
//! thread, feeding a channel the engine's `select!` can await, is the same seam
//! the blocking CA/PVA *servers* already use for their own SEARCH responders
//! (`epics_ca_rs::server::blocking::handle_udp_search_blocking`) and the same
//! one `runtime::blocking_io` uses for TCP circuit pumps.
//!
//! Sends are **not** pumped. A UDP `send_to` either fits the socket buffer or
//! fails with `ENOBUFS`; it does not block on a peer, so it runs inline on the
//! engine's worker, exactly as the blocking server's `send_udp_reply` does.
//! One thread per search engine, and a CA client has one.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};

/// One received SEARCH datagram, and what the receiving path knows about it.
///
/// The three things a client's SEARCH-receive arm reads, in one type that is
/// nameable on both backends — which is the whole reason it exists rather than
/// the callers reading `AsyncUdpV4`'s `RecvMeta` directly: `RecvMeta` is part
/// of the host-only UDP stack, and a `select!` branch cannot carry a `#[cfg]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchDatagram {
    /// Datagram length written into the caller's buffer. A datagram longer
    /// than the buffer is truncated, as `recvfrom(2)` truncates.
    pub n: usize,
    /// Sender address.
    pub src: SocketAddr,
    /// IPv4 address of the NIC that received it, when the receive path
    /// reports one.
    ///
    /// `None` on a single wildcard socket with no `IP_PKTINFO` receive path —
    /// libca's model, and the only one available on the embedded targets. It
    /// keys the per-NIC `SO_RXQ_OVFL` drop log, so `None` means "this
    /// datagram's drop counter is not per-NIC attributable" rather than
    /// naming a NIC the platform never told us about.
    pub iface_ip: Option<Ipv4Addr>,
    /// The receiving NIC's kernel drop counter (`SO_RXQ_OVFL`), or 0 where the
    /// platform has no such counter.
    pub drops: u32,
}

/// A client's UDP SEARCH socket.
pub struct SearchUdpSocket(sys::Sock);

impl SearchUdpSocket {
    /// Bind an ephemeral SEARCH socket.
    ///
    /// `pump_name` and `pump_priority` describe the receive thread the
    /// `exec_backend` arm creates; the host arm has no thread and ignores
    /// them. They are unconditional parameters and not `#[cfg]`-gated so that
    /// a caller states its band once, in its own C-derived terms, without a
    /// gate of its own — the CA client passes libca's `CAC-UDP`
    /// (`udpiiu.cpp:128-132`), the PVA client its own.
    pub fn bind_ephemeral(
        broadcast: bool,
        pump_name: &str,
        pump_priority: crate::runtime::task::ThreadPriority,
    ) -> io::Result<Self> {
        sys::Sock::bind_ephemeral(broadcast, pump_name, pump_priority).map(Self)
    }

    /// `SO_RCVBUF`. Best-effort on every platform: a kernel is free to clamp.
    pub fn set_recv_buffer_size(&self, size: usize) -> io::Result<()> {
        self.0.set_recv_buffer_size(size)
    }

    /// `IP_MULTICAST_TTL` — `EPICS_CA_MCAST_TTL` (epics-base 3.16, f2a1834d).
    pub fn set_multicast_ttl_v4(&self, ttl: u32) -> io::Result<()> {
        self.0.set_multicast_ttl_v4(ttl)
    }

    /// Opt into the kernel's per-socket drop counter where one exists
    /// (`SO_RXQ_OVFL`, Linux only). Diagnostic; failure is not fatal.
    pub fn enable_so_rxq_ovfl(&self) -> io::Result<()> {
        self.0.enable_so_rxq_ovfl()
    }

    /// Every local address this socket bound. One entry on the exec arm, one
    /// per NIC on the host arm.
    pub fn local_addrs(&self) -> Vec<SocketAddr> {
        self.0.local_addrs()
    }

    /// The next SEARCH reply datagram.
    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<SearchDatagram> {
        self.0.recv(buf).await
    }

    /// Send one datagram to one destination.
    pub async fn send_to(&self, buf: &[u8], dest: SocketAddr) -> io::Result<usize> {
        self.0.send_to(buf, dest).await
    }

    /// Send the same payload toward every eligible interface — for the
    /// limited broadcast `255.255.255.255` and for multicast groups, which a
    /// single socket would otherwise emit on one interface only.
    ///
    /// Returns how many sends succeeded; errors only when none did.
    pub async fn fanout_to(&self, buf: &[u8], dest: SocketAddr) -> io::Result<usize> {
        self.0.fanout_to(buf, dest).await
    }
}

// ---------------------------------------------------------------------------
// Host: the per-NIC bundle, unchanged.
// ---------------------------------------------------------------------------

#[cfg(tokio_backend)]
mod sys {
    use super::{SearchDatagram, SocketAddr, io};
    use crate::net::async_udp_v4::AsyncUdpV4;

    pub(super) struct Sock(AsyncUdpV4);

    impl Sock {
        pub(super) fn bind_ephemeral(
            broadcast: bool,
            pump_name: &str,
            pump_priority: crate::runtime::task::ThreadPriority,
        ) -> io::Result<Self> {
            // No pump: the reactor is the pump.
            let _ = (pump_name, pump_priority);
            AsyncUdpV4::bind(0, broadcast).map(Self)
        }

        pub(super) fn set_recv_buffer_size(&self, size: usize) -> io::Result<()> {
            self.0.set_recv_buffer_size(size)
        }

        pub(super) fn set_multicast_ttl_v4(&self, ttl: u32) -> io::Result<()> {
            self.0.set_multicast_ttl_v4(ttl)
        }

        pub(super) fn enable_so_rxq_ovfl(&self) -> io::Result<()> {
            self.0.enable_so_rxq_ovfl()
        }

        pub(super) fn local_addrs(&self) -> Vec<SocketAddr> {
            self.0.local_addrs()
        }

        pub(super) async fn recv(&self, buf: &mut [u8]) -> io::Result<SearchDatagram> {
            let (meta, drops) = self.0.recv_with_meta_with_drops(buf).await?;
            Ok(SearchDatagram {
                n: meta.n,
                src: meta.src,
                iface_ip: Some(meta.iface_ip),
                drops,
            })
        }

        pub(super) async fn send_to(&self, buf: &[u8], dest: SocketAddr) -> io::Result<usize> {
            self.0.send_to(buf, dest).await
        }

        pub(super) async fn fanout_to(&self, buf: &[u8], dest: SocketAddr) -> io::Result<usize> {
            self.0.fanout_to(buf, dest).await
        }
    }
}

// ---------------------------------------------------------------------------
// Exec backend: one wildcard socket + one receive pump.
// ---------------------------------------------------------------------------

#[cfg(exec_backend)]
mod sys {
    use super::{Ipv4Addr, SearchDatagram, SocketAddr, io};
    use crate::runtime::task::{StackSizeClass, ThreadPriority, spawn_dedicated_thread};
    use std::net::UdpSocket;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    /// How long a blocking `recv_from` may sit before the pump re-reads the
    /// stop flag. Only a clean-stop seam — the same 200 ms the blocking CA
    /// server's SEARCH responder uses, and invisible to the protocol.
    const PUMP_WAKE_INTERVAL: Duration = Duration::from_millis(200);

    /// IPv4's maximum datagram, matching every other SEARCH receive buffer in
    /// the workspace.
    const RECV_BUF: usize = 64 * 1024;

    pub(super) struct Sock {
        /// Shared with the pump thread. `Arc`, not `try_clone`: a `dup(2)` on
        /// a libbsd socket fails `ENXIO` on `armv7-rtems-eabihf`.
        socket: Arc<UdpSocket>,
        /// Datagrams the pump has read, awaiting the engine.
        rx: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<(Vec<u8>, SocketAddr)>>,
        /// Set by `Drop`; the pump observes it between `recv_from` calls.
        stop: Arc<AtomicBool>,
    }

    impl Sock {
        pub(super) fn bind_ephemeral(
            broadcast: bool,
            pump_name: &str,
            pump_priority: ThreadPriority,
        ) -> io::Result<Self> {
            let socket = Arc::new(UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?);
            if broadcast {
                socket.set_broadcast(true)?;
            }
            socket.set_read_timeout(Some(PUMP_WAKE_INTERVAL))?;

            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let stop = Arc::new(AtomicBool::new(false));
            let pump_socket = Arc::clone(&socket);
            let pump_stop = Arc::clone(&stop);
            // `epicsThreadStackMedium`, which is what libca asks for
            // (`udpiiu.cpp:129`). Ours does strictly less than C's CAC-UDP —
            // it copies bytes to a channel where C runs the whole response
            // callback on this stack — but there is one such thread per
            // process, so matching C costs nothing worth deviating for.
            spawn_dedicated_thread(
                pump_name.to_string(),
                pump_priority,
                StackSizeClass::Medium,
                move || pump(&pump_socket, &pump_stop, &tx),
            )?;

            Ok(Self {
                socket,
                rx: tokio::sync::Mutex::new(rx),
                stop,
            })
        }

        pub(super) fn set_recv_buffer_size(&self, size: usize) -> io::Result<()> {
            set_int_opt(
                &self.socket,
                sockopt::SOL_SOCKET,
                sockopt::SO_RCVBUF,
                size as _,
            )
        }

        pub(super) fn set_multicast_ttl_v4(&self, ttl: u32) -> io::Result<()> {
            set_int_opt(
                &self.socket,
                sockopt::IPPROTO_IP,
                sockopt::IP_MULTICAST_TTL,
                ttl as _,
            )
        }

        pub(super) fn enable_so_rxq_ovfl(&self) -> io::Result<()> {
            // `SO_RXQ_OVFL` is a Linux cmsg facility read via `recvmsg`, and
            // this arm's receive path is `recv_from`. Reporting Ok here with
            // `SearchDatagram::drops` always 0 would claim a counter that is
            // never read, so it reports the honest thing instead: the
            // diagnostic is unsupported, which every caller already treats as
            // non-fatal.
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "SO_RXQ_OVFL needs a recvmsg receive path; this SEARCH socket uses recv_from",
            ))
        }

        pub(super) fn local_addrs(&self) -> Vec<SocketAddr> {
            self.socket.local_addr().into_iter().collect()
        }

        pub(super) async fn recv(&self, buf: &mut [u8]) -> io::Result<SearchDatagram> {
            let Some((bytes, src)) = self.rx.lock().await.recv().await else {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "SEARCH receive pump stopped",
                ));
            };
            // Truncate as `recvfrom(2)` truncates.
            let n = bytes.len().min(buf.len());
            buf[..n].copy_from_slice(&bytes[..n]);
            Ok(SearchDatagram {
                n,
                src,
                iface_ip: None,
                drops: 0,
            })
        }

        pub(super) async fn send_to(&self, buf: &[u8], dest: SocketAddr) -> io::Result<usize> {
            self.socket.send_to(buf, dest)
        }

        pub(super) async fn fanout_to(&self, buf: &[u8], dest: SocketAddr) -> io::Result<usize> {
            // One socket cannot pick its egress NIC by binding, so the
            // destination does it: each interface's own directed broadcast
            // leaves via that interface by ordinary routing. This is what
            // libca sends to in the first place — `EPICS_CA_AUTO_ADDR_LIST`
            // is exactly this list — so the expansion is C's own, not a
            // substitute for one.
            let port = dest.port();
            let dests: Vec<SocketAddr> = match dest {
                SocketAddr::V4(v4) if v4.ip().is_broadcast() => {
                    crate::net::iface_v4::broadcast_addrs()
                        .into_iter()
                        .map(|ip| SocketAddr::from((ip, port)))
                        .collect()
                }
                // A multicast group has no per-interface rewrite: the group
                // address *is* the destination on every NIC, and one socket
                // emits on the routing table's choice. Sending it once is all
                // this arm can do.
                _ => vec![dest],
            };
            let mut ok = 0usize;
            let mut last_err: Option<io::Error> = None;
            for d in dests {
                match self.socket.send_to(buf, d) {
                    Ok(_) => ok += 1,
                    Err(e) => {
                        tracing::debug!(
                            target: "epics_base_rs::net",
                            dest = %d,
                            error = %e,
                            "SEARCH fanout send failed"
                        );
                        last_err = Some(e);
                    }
                }
            }
            if ok == 0 {
                return Err(last_err.unwrap_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::AddrNotAvailable,
                        "SEARCH fanout: no eligible broadcast destination",
                    )
                }));
            }
            Ok(ok)
        }
    }

    impl Drop for Sock {
        /// Ask the pump to stop. It exits within [`PUMP_WAKE_INTERVAL`] and
        /// releases the last `Arc<UdpSocket>` as it does.
        ///
        /// Not joined: `Drop` runs on the engine's worker, and blocking it for
        /// up to one wake interval to reclaim a thread that is already leaving
        /// trades a certain stall for no gain. The exit is unconditional — it
        /// is the pump loop's own condition, not a later cleanup step.
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
        }
    }

    /// Read datagrams until asked to stop or the receiver is gone.
    fn pump(
        socket: &UdpSocket,
        stop: &AtomicBool,
        tx: &tokio::sync::mpsc::UnboundedSender<(Vec<u8>, SocketAddr)>,
    ) {
        let mut buf = vec![0u8; RECV_BUF];
        while !stop.load(Ordering::Acquire) {
            match socket.recv_from(&mut buf) {
                Ok((n, src)) => {
                    if tx.send((buf[..n].to_vec(), src)).is_err() {
                        return;
                    }
                }
                Err(e) if is_wake_timeout(e.kind()) => continue,
                // libca `udpiiu.cpp:1090-1120` and C `cast_server.c:171-179`
                // both keep receiving after a UDP error: an earlier SEARCH
                // drawing an ICMP port-unreachable surfaces here as
                // ECONNREFUSED/ECONNRESET on the *next* recv and says nothing
                // about this socket's health.
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::ConnectionRefused
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::Interrupted
                    ) =>
                {
                    continue;
                }
                Err(e) => {
                    tracing::warn!(
                        target: "epics_base_rs::net",
                        error = %e,
                        "SEARCH receive pump stopping"
                    );
                    return;
                }
            }
        }
    }

    /// A read-timeout expiry, which is a wake and not an error.
    ///
    /// Two kinds because a timed-out socket read reports `EAGAIN` on some
    /// platforms and `EWOULDBLOCK` on others, and Rust maps them to different
    /// `ErrorKind`s.
    fn is_wake_timeout(kind: io::ErrorKind) -> bool {
        matches!(kind, io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
    }

    #[cfg(unix)]
    mod sockopt {
        pub(super) const SOL_SOCKET: libc::c_int = libc::SOL_SOCKET;
        pub(super) const SO_RCVBUF: libc::c_int = libc::SO_RCVBUF;
        pub(super) const IPPROTO_IP: libc::c_int = libc::IPPROTO_IP;
        pub(super) const IP_MULTICAST_TTL: libc::c_int = libc::IP_MULTICAST_TTL;
    }

    #[cfg(unix)]
    fn set_int_opt(
        socket: &UdpSocket,
        level: libc::c_int,
        name: libc::c_int,
        value: libc::c_int,
    ) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        // SAFETY: the fd is owned by `socket` and outlives the call; `value`
        // is a live `c_int` and the length matches it exactly.
        let rc = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                level,
                name,
                std::ptr::addr_of!(value).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    // A non-Unix exec backend is a host `--features rtems-exec-model` build on
    // Windows: it exists to run the target's *scheduling* shape on a
    // developer's machine, not its socket options. Both options this arm sets
    // are performance/reach tuning that the SEARCH protocol does not depend
    // on, so they report unsupported rather than pulling in a second
    // platform's setsockopt spelling.
    #[cfg(not(unix))]
    mod sockopt {
        pub(super) const SOL_SOCKET: i32 = 0;
        pub(super) const SO_RCVBUF: i32 = 0;
        pub(super) const IPPROTO_IP: i32 = 0;
        pub(super) const IP_MULTICAST_TTL: i32 = 0;
    }

    #[cfg(not(unix))]
    fn set_int_opt(_socket: &UdpSocket, _level: i32, _name: i32, _value: i32) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "socket options on a non-Unix exec-backend SEARCH socket",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::task::ThreadPriority;

    fn bind() -> SearchUdpSocket {
        SearchUdpSocket::bind_ephemeral(true, "test-CAC-UDP", ThreadPriority::Medium)
            .expect("bind ephemeral SEARCH socket")
    }

    /// The property the whole stage exists for: a SEARCH socket binds on this
    /// backend, whichever backend it is.
    #[epics_macros_rs::epics_test]
    async fn binds_and_reports_a_local_address() {
        let sock = bind();
        let addrs = sock.local_addrs();
        assert!(!addrs.is_empty(), "a bound SEARCH socket has an address");
        for a in &addrs {
            assert_ne!(a.port(), 0, "ephemeral bind assigns a real port: {a}");
        }
    }

    /// Round-trip one datagram through the arm this build selected — the
    /// receive path is a reactor registration on one backend and a pump thread
    /// on the other, and this is the assertion that does not care which.
    #[epics_macros_rs::epics_test]
    async fn round_trips_a_datagram_to_itself() {
        let sock = bind();
        let port = sock
            .local_addrs()
            .iter()
            .find(|a| a.is_ipv4())
            .expect("an IPv4 SEARCH address")
            .port();
        let dest = SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));

        sock.send_to(b"CA-SEARCH", dest).await.expect("send_to");
        let mut buf = [0u8; 64];
        let dg =
            crate::runtime::task::timeout(std::time::Duration::from_secs(5), sock.recv(&mut buf))
                .await
                .expect("a datagram sent to ourselves arrives")
                .expect("recv");
        assert_eq!(&buf[..dg.n], b"CA-SEARCH");
        assert_eq!(dg.drops, 0, "a single quiet datagram overflows nothing");
    }
}
