// The bound-socket UDP responder stack (`socket2` shared-port setup, the
// `tokio::net::UdpSocket` recv loops, the `epics_base_rs::net` RX-overflow
// helpers) is the async reactor-borne front-end. Every reactor-free build
// (RTEMS, VxWorks, or a host `exec_backend` build) answers SEARCH through
// `server::blocking`'s `std::net` responder, reusing only the shared
// decode/shape logic (`SearchReplyBatch`, `parse_search_datagram`,
// `shape_search_reply_dg`) below. Those imports are gated to `tokio_backend`.
#[cfg(tokio_backend)]
use socket2::{Domain, Protocol, Socket, Type};
use std::net::SocketAddr;
// `Ipv4Addr`, `Arc`, and `CaResult` are used only by the bound-socket responder
// stack, which is `tokio_backend`; the shared decode path uses `SocketAddr` only.
#[cfg(tokio_backend)]
use std::net::Ipv4Addr;
#[cfg(tokio_backend)]
use std::sync::Arc;
#[cfg(tokio_backend)]
use tokio::net::UdpSocket;

use crate::protocol::*;
#[cfg(tokio_backend)]
use epics_base_rs::error::CaResult;
#[cfg(tokio_backend)]
use epics_base_rs::net::{
    PortOrigin, PortUse, enable_so_rxq_ovfl_for_socket, port_held, recv_from_with_drop_count_socket,
};
use epics_base_rs::server::database::PvDatabase;

/// Decide the UDP responder sockets to open: one `(bind_ip,
/// mcast_groups)` spec per CA `casIntfAddrList` interface entry.
///
/// **Invariant.** C `caservertask.c:622-669` opens exactly
/// ONE UDP socket per `casIntfAddrList` entry — `conf->udp`, bound
/// to that entry's address (a specific IP *or* `INADDR_ANY`) — and
/// joins every `casMCastAddrList` group on THAT SAME socket via
/// `IP_ADD_MEMBERSHIP`. C never opens a separate per-multicast-group
/// socket.
///
/// Pre-fix Rust kept C parity for specific interfaces but, for a
/// wildcard (`0.0.0.0`) interface, emptied the interface's group
/// list and spawned one EXTRA `0.0.0.0:port` socket per multicast
/// group. With `SO_REUSEADDR`/`SO_REUSEPORT` datagram fanout, an
/// ordinary broadcast (and, on stacks without reuseport
/// load-balancing, unicast) CA SEARCH reached the primary wildcard
/// responder AND every extra multicast responder — each emitting
/// its own `send_to(reply, src)`, so one request got duplicate
/// replies. `IP_MULTICAST_ALL=0` filters only multicast group
/// cross-talk, not unicast/broadcast.
///
/// This function enforces the invariant structurally: every
/// returned spec carries the FULL multicast group list, and there
/// is exactly one spec per interface entry — there is no code path
/// that produces an extra group-only responder. A wildcard
/// interface is therefore the single owner of ordinary
/// unicast/broadcast SEARCH traffic, matching C's `conf->udp`.
#[cfg(tokio_backend)]
fn plan_responder_specs(
    intf_addrs: Vec<Ipv4Addr>,
    mcast_addrs: &[Ipv4Addr],
) -> Vec<(Ipv4Addr, Vec<Ipv4Addr>)> {
    let intfs = if intf_addrs.is_empty() {
        vec![Ipv4Addr::UNSPECIFIED]
    } else {
        intf_addrs
    };
    intfs
        .into_iter()
        .map(|bind_ip| (bind_ip, mcast_addrs.to_vec()))
        .collect()
}

/// A UDP search responder whose sockets are already bound (and whose
/// multicast groups are already joined).
///
/// Counterpart of [`super::tcp::BoundTcp`]: only [`bind_udp_responders`]
/// can produce one, and every `CaServer` construction path does so
/// before returning the server. A bound UDP socket already queues
/// datagrams in the kernel, so a SEARCH that arrives before
/// `CaServer::run()` is polled is answered once the recv loop starts
/// rather than dropped.
#[cfg(tokio_backend)]
#[derive(Clone)]
pub struct BoundResponder {
    bind_ip: Ipv4Addr,
    /// Primary socket, bound to the interface entry's address.
    socket: Arc<UdpSocket>,
    /// Secondary socket bound to the interface's broadcast address —
    /// see the comment in [`bind_udp_responders`]. `None` without
    /// `ca_udp_bcast` (Windows, Cygwin) and for wildcard interfaces.
    bcast: Option<Arc<UdpSocket>>,
}

/// The UDP search responders of one server, all bound to one port.
#[cfg(tokio_backend)]
#[derive(Clone)]
pub struct BoundUdp {
    responders: Vec<BoundResponder>,
    port: u16,
}

#[cfg(tokio_backend)]
impl BoundUdp {
    /// The UDP port every responder is bound to. With a requested port
    /// of 0 this is the ephemeral port the kernel chose — the value a
    /// client must put in `EPICS_CA_ADDR_LIST` to reach this server.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The interfaces a responder is bound on.
    pub(crate) fn interfaces(&self) -> Vec<Ipv4Addr> {
        self.responders.iter().map(|r| r.bind_ip).collect()
    }
}

/// Bind one UDP search responder per `EPICS_CAS_INTF_ADDR_LIST`
/// interface, joining every configured multicast group on it.
///
/// Each interface gets its own socket — having a dedicated socket per
/// interface lets the OS keep the broadcast routing straight on multi-NIC
/// hosts (matching C EPICS osiSockDiscoverInterfaces behaviour).
///
/// A requested `port` of 0 means "any free UDP port": the first
/// responder's kernel-assigned port is then imposed on the remaining
/// interfaces, since a CA server answers SEARCHes on one port and its
/// clients are pointed at exactly one. This is the race-free way to
/// take a port — a caller that probes for a free port first and passes
/// the number can still lose it to another socket in between.
#[cfg(tokio_backend)]
pub fn bind_udp_responders(
    port: u16,
    intf_addrs: Vec<Ipv4Addr>,
    mcast_addrs: &[Ipv4Addr],
) -> CaResult<BoundUdp> {
    bind_udp_responders_with(port, intf_addrs, mcast_addrs, 0, &responder_broadcast_for)
}

/// `osiSockDiscoverBroadcastAddresses` for one responder interface: 127.0.0.1
/// is answered with itself (`osdNetIfAddrs.c:41-52`), so rsrv binds a second
/// socket on it; any other address gets its broadcast address, if it has one.
#[cfg(tokio_backend)]
pub(crate) fn responder_broadcast_for(interface: Ipv4Addr) -> Option<Ipv4Addr> {
    if interface == Ipv4Addr::LOCALHOST {
        Some(interface)
    } else {
        super::addr_list::broadcast_for_ip(interface)
    }
}

/// [`bind_udp_responders`] with the interface-to-broadcast-address lookup
/// (`osiSockDiscoverBroadcastAddresses`, `caservertask.c:680`) supplied, so a
/// test can name a broadcast address on a host that has none. `tcp_port` is
/// only the number C's interface name carries in its warnings (`:615`).
#[cfg(tokio_backend)]
pub(crate) fn bind_udp_responders_with(
    port: u16,
    intf_addrs: Vec<Ipv4Addr>,
    mcast_addrs: &[Ipv4Addr],
    tcp_port: u16,
    broadcast_of: &dyn Fn(Ipv4Addr) -> Option<Ipv4Addr>,
) -> CaResult<BoundUdp> {
    let specs = plan_responder_specs(intf_addrs, mcast_addrs);
    if port != 0 {
        return match bind_responder_set(
            port,
            PortOrigin::Configured,
            &specs,
            tcp_port,
            broadcast_of,
        )? {
            SetBind::Bound(b) => Ok(b),
            SetBind::NumberHeld(..) => unreachable!("a configured number is never refused whole"),
        };
    }
    // deliberate deviation, as for the PVA bundle (`docs/differences.md`):
    // the number is ours, so a later socket that finds a fanout stranger on
    // it restarts the set on a fresh one rather than joining the stranger or
    // dropping the interface. Refused sets stay bound until the end, so
    // `bind(0)` cannot hand the same number back. On Windows a stranger that
    // set `SO_REUSEADDR` is joined, as rsrv joins it there (`bind_fanout`
    // sets the option first); only a refusal, WSAEACCES, restarts the set.
    let mut refused = Vec::new();
    let mut last = None;
    for _ in 0..16 {
        match bind_responder_set(0, PortOrigin::Chosen, &specs, tcp_port, broadcast_of)? {
            SetBind::Bound(b) => return Ok(b),
            SetBind::NumberHeld(partial, e) => {
                refused.push(partial);
                last = Some(e);
            }
        }
    }
    Err(last.expect("sixteen refused sets"))
}

/// What one pass over the interface list bound.
#[cfg(tokio_backend)]
enum SetBind {
    Bound(BoundUdp),
    /// A [`PortOrigin::Chosen`] number was held by another fanout socket on
    /// a later interface or broadcast address; the responders bound so far
    /// are returned to keep it.
    NumberHeld(Vec<BoundResponder>, epics_base_rs::error::CaError),
}

#[cfg(tokio_backend)]
fn bind_responder_set(
    port: u16,
    origin: PortOrigin,
    specs: &[(Ipv4Addr, Vec<Ipv4Addr>)],
    tcp_port: u16,
    broadcast_of: &dyn Fn(Ipv4Addr) -> Option<Ipv4Addr>,
) -> CaResult<SetBind> {
    let mut responders = Vec::new();
    let mut actual_port = if port == 0 { None } else { Some(port) };
    let mut held: Option<epics_base_rs::error::CaError> = None;
    let mut own = Vec::new();
    // Wildcards first: a clean bind there proves the number free on every
    // address, so whatever follows is covered (`origin_at`). Bound in this
    // order, reported in list order.
    let mut order: Vec<usize> = (0..specs.len()).collect();
    order.sort_by_key(|&i| !specs[i].0.is_unspecified());
    for i in order {
        let (bind_ip, mcast_groups) = &specs[i];
        let responder = match bind_single_responder(
            *bind_ip,
            actual_port.unwrap_or(0),
            origin,
            &own,
            mcast_groups,
            tcp_port,
            broadcast_of,
        ) {
            Ok(r) => r,
            Err(epics_base_rs::error::CaError::Io(e))
                if bind_was_held(&e) && origin == PortOrigin::Chosen =>
            {
                return Ok(SetBind::NumberHeld(
                    responders.into_iter().map(|(_, r)| r).collect(),
                    epics_base_rs::error::CaError::Io(e),
                ));
            }
            // `tryBind` returns -1 without a word on EADDRINUSE and
            // `rsrv_init` drops that interface alone
            // (`caservertask.c:136`, `:630`, `:745-750`); the others serve.
            // Winsock reports a held number as WSAEACCES to a `SO_REUSEADDR`
            // bind, which C on WIN32 takes as fatal; here it is held too.
            Err(epics_base_rs::error::CaError::Io(e)) if bind_was_held(&e) => {
                held = Some(epics_base_rs::error::CaError::Io(e));
                continue;
            }
            Err(e) => return Err(e),
        };
        if actual_port.is_none() {
            actual_port = Some(responder.socket.local_addr()?.port());
        }
        own.push(responder.bind_ip);
        if let Some(b) = &responder.bcast {
            if let std::net::SocketAddr::V4(a) = b.local_addr()? {
                own.push(*a.ip());
            }
        }
        responders.push((i, responder));
    }
    responders.sort_by_key(|&(i, _)| i);
    let responders: Vec<BoundResponder> = responders.into_iter().map(|(_, r)| r).collect();
    // `plan_responder_specs` always yields at least one spec (an empty
    // interface list becomes the wildcard entry).
    // `cantProceed("CAS: No TCP server started")` only when none is left
    // (`caservertask.c:752`); here that is the error the last interface gave.
    if responders.is_empty() {
        return Err(held.expect("a spec that bound nothing was held"));
    }
    let port = actual_port.expect("a bound responder fixed the port");
    Ok(SetBind::Bound(BoundUdp { responders, port }))
}

/// Run the recv loops of responders already bound by
/// [`bind_udp_responders`].
///
/// `ignore_addrs` filters out source addresses that should never receive
/// search replies (EPICS_CAS_IGNORE_ADDR_LIST).
#[cfg(tokio_backend)]
pub async fn run_udp_search_responder(
    reactor: &epics_base_rs::runtime::task::Reactor,
    db: Arc<PvDatabase>,
    bound: BoundUdp,
    tcp_port: u16,
    ignore_addrs: Vec<Ipv4Addr>,
) -> CaResult<()> {
    /// Every per-socket responder this call spawned, aborted when the call's
    /// future goes away. The bare handles this replaces were dropped on
    /// cancellation, and a dropped handle DETACHES its task, so a cancelled
    /// `run_udp_search_responder` left its `recv_loop`s holding the search
    /// sockets — and, since a task that never ends never runs its
    /// `taskwd_insert` guard's drop, holding their watchdog rows too. C ends
    /// `cast_server` with the thread.
    struct RespondersOnDrop(Vec<epics_base_rs::runtime::task::TaskAbortHandle>);
    impl Drop for RespondersOnDrop {
        fn drop(&mut self) {
            for h in &self.0 {
                h.abort();
            }
        }
    }

    let responders = bound.responders;
    let mut handles = Vec::with_capacity(responders.len());
    let mut aborts = Vec::with_capacity(responders.len());

    for responder in responders {
        let db_t = db.clone();
        let ignore_t = ignore_addrs.clone();
        let handle = reactor
            .spawn(async move { run_single_responder(db_t, responder, tcp_port, ignore_t).await });
        aborts.push(handle.abort_handle());
        handles.push(handle);
    }
    let _abort_siblings = RespondersOnDrop(aborts);

    // Propagate the first error; the guard above stops the rest, whether this
    // returns or is cancelled.
    let mut handles_iter = handles.into_iter();
    if let Some(first) = handles_iter.next() {
        match first.await {
            Ok(inner) => inner,
            Err(e) => Err(epics_base_rs::error::CaError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                e.to_string(),
            ))),
        }
    } else {
        Ok(())
    }
}

/// What `rsrv_init` prints when an interface bound to a specific address has
/// no broadcast address (`caservertask.c:683-685`); `ifaceName` is the TCP
/// address `a.b.c.d:port` (`:615`). Not without `ca_udp_bcast`, which opens no broadcast
/// socket.
#[cfg(all(tokio_backend, ca_udp_bcast))]
fn missing_broadcast_warning(
    bind_ip: Ipv4Addr,
    tcp_port: u16,
    found: Option<Ipv4Addr>,
) -> Option<String> {
    (found.is_none() && !bind_ip.is_unspecified()).then(|| {
        format!(
            "Warning: Can't find broadcast address of interface {bind_ip}:{tcp_port}\n         \
             Name lookup may not work on this interface\n"
        )
    })
}

/// `tryBind`'s line for a bind error other than `EADDRINUSE`
/// (`caservertask.c:138-139`): `CAS: %s bind ERL_ERROR: %s`, the text being
/// `epicsSocketConvertErrnoToString`, which is `strerror` alone. Its only
/// caller is the broadcast socket, which only `ca_udp_bcast` opens.
#[cfg(all(tokio_backend, ca_udp_bcast))]
fn try_bind_error_line(name: &str, e: &std::io::Error) -> String {
    let os = bind_source(e);
    format!(
        "CAS: {name} bind {}: {}",
        epics_base_rs::runtime::log::erl_error(),
        crate::repeater_clients::sock_err_string(os)
    )
}

#[cfg(tokio_backend)]
fn bind_single_responder(
    bind_ip: Ipv4Addr,
    port: u16,
    origin: PortOrigin,
    own: &[Ipv4Addr],
    mcast_groups: &[Ipv4Addr],
    tcp_port: u16,
    broadcast_of: &dyn Fn(Ipv4Addr) -> Option<Ipv4Addr>,
) -> CaResult<BoundResponder> {
    let socket = bind_responder_socket(bind_ip, port, origin_at(bind_ip, own, origin))?;
    // The port the primary actually took — identical to `port`, except
    // for an ephemeral (0) request, where the kernel chose it. The only
    // consumer of the resolved port is the broadcast secondary responder
    // below, which exists only under `ca_udp_bcast`, so resolve it under
    // the same gate — otherwise the binding is unused without it.
    #[cfg(ca_udp_bcast)]
    let port = socket.local_addr()?.port();
    // Join each multicast group on this
    // responder's own socket via `IP_ADD_MEMBERSHIP` with
    // `imr_interface = bind_ip`. C `caservertask.c:634-666` joins
    // every `casMCastAddrList` group on the single `conf->udp`
    // socket of each `casIntfAddrList` entry, regardless of whether
    // that entry is a specific IP or `INADDR_ANY` — there is no
    // separate per-group socket. A wildcard `0.0.0.0` `bind_ip`
    // joins on the kernel default interface (matching C's
    // `imr_interface = INADDR_ANY` for a wildcard `conf->udpAddr`).
    // Joining here, on the one responder socket, keeps a single
    // owner for ordinary unicast/broadcast SEARCH traffic so a
    // request is answered exactly once. Per-(intf, group) failures
    // are logged and skipped — `caservertask.c:659-660`
    // `errlogPrintf`s and continues; the IOC stays up.
    for group in mcast_groups {
        match socket.join_multicast_v4(*group, bind_ip) {
            Ok(()) => tracing::debug!(
                target: "epics_ca_rs::server::udp",
                %bind_ip,
                group = %group,
                "joined multicast group on responder socket"
            ),
            Err(e) => tracing::warn!(
                target: "epics_ca_rs::server::udp",
                %bind_ip,
                group = %group,
                error = %e,
                "CA server IP_ADD_MEMBERSHIP failed — \
                 SEARCH on this group will not reach this NIC"
            ),
        }
    }
    let socket = Arc::new(socket);

    // C `caservertask.c::start_tcp_server_tasks` (lines 670-708) opens a
    // *second* UDP responder bound to the interface's broadcast address
    // whenever the primary socket is bound to a specific (non-INADDR_ANY)
    // interface IP. The comment at line 671 documents the BSD-sockets
    // oddity: a unicast-bound socket on POSIX does NOT receive UDP
    // datagrams whose destination is the interface's broadcast addr —
    // only the secondary socket bound to the broadcast addr will.
    // Without this second responder, every libca client SEARCH that
    // targets the broadcast network address (the default
    // `EPICS_CA_ADDR_LIST` fan-out shape) goes unanswered on a Rust IOC
    // configured with a specific `EPICS_CAS_INTF_ADDR_LIST` entry —
    // PVs become invisible to broadcast clients despite the server
    // running and accepting unicast searches.
    //
    // On Windows the kernel behaviour differs (a specific-IP-bound
    // socket receives broadcasts), so C `caservertask.c:671, 728`
    // guards the secondary socket with `#if !(_WIN32 || __CYGWIN__)`,
    // which is `ca_udp_bcast` (build.rs).
    let bcast: Option<Arc<UdpSocket>> = {
        #[cfg(not(ca_udp_bcast))]
        {
            let _ = (broadcast_of, tcp_port);
            None
        }
        #[cfg(ca_udp_bcast)]
        {
            let found = broadcast_of(bind_ip);
            if let Some(w) = missing_broadcast_warning(bind_ip, tcp_port, found) {
                eprint!("{w}");
            }
            let with_primary = [own, &[bind_ip]].concat();
            match found.map(|bcast_ip| {
                (
                    bcast_ip,
                    bind_responder_socket(
                        bcast_ip,
                        port,
                        origin_at(bcast_ip, &with_primary, origin),
                    ),
                )
            }) {
                None => None,
                Some((_, Ok(s))) => Some(Arc::new(s)),
                // `tryBind` on `udpbcast` failing with EADDRINUSE reaches
                // `goto cleanup` (`caservertask.c:703`): the interface is
                // dropped whole, as for its primary socket.
                Some((_, Err(epics_base_rs::error::CaError::Io(e)))) if bind_was_held(&e) => {
                    return Err(epics_base_rs::error::CaError::Io(e));
                }
                // Any other error: `tryBind` prints and suspends the init
                // thread (`:135-141`). Under the signed-off deviation of
                // 2026-10-14 the start returns the error instead.
                Some((_, Err(e))) => {
                    if let epics_base_rs::error::CaError::Io(io) = &e {
                        eprintln!("{}", try_bind_error_line("UDP Socket bcast", io));
                    }
                    return Err(e);
                }
            }
        }
    };

    Ok(BoundResponder {
        bind_ip,
        socket,
        bcast,
    })
}

#[cfg(tokio_backend)]
async fn run_single_responder(
    db: Arc<PvDatabase>,
    responder: BoundResponder,
    tcp_port: u16,
    ignore_addrs: Vec<Ipv4Addr>,
) -> CaResult<()> {
    let BoundResponder {
        bind_ip,
        socket,
        bcast,
    } = responder;
    let udp_rl = Arc::new(UdpRateLimiter::from_env());
    let primary = recv_loop(
        socket,
        db.clone(),
        bind_ip,
        tcp_port,
        ignore_addrs.clone(),
        udp_rl.clone(),
        "CAS-UDP",
    );

    match bcast {
        Some(bsock) => {
            let secondary = recv_loop(
                bsock,
                db,
                bind_ip,
                tcp_port,
                ignore_addrs,
                udp_rl,
                "CAS-UDP2",
            );
            // First task to error wins; the other is dropped when this
            // future returns. tokio::try_join is `Drop` on cancel, so the
            // surviving loop's recv() future cancels cleanly.
            tokio::try_join!(primary, secondary).map(|_| ())
        }
        None => primary.await,
    }
}

/// Build and configure the per-bind UDP socket. Centralised so the
/// primary (interface IP) and secondary (interface broadcast addr)
/// sockets share identical socket-option setup.
/// A bind error with the address it was for. Displays as `<what>: <source>`;
/// the source stays reachable so `tryBind`'s line can print `strerror` alone.
#[cfg(tokio_backend)]
#[derive(Debug)]
struct BindContext {
    what: String,
    source: std::io::Error,
}

#[cfg(tokio_backend)]
impl std::fmt::Display for BindContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.what, self.source)
    }
}

#[cfg(tokio_backend)]
impl std::error::Error for BindContext {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// The socket's own error under the [`BindContext`] it travels in, which
/// keeps only the kind: Winsock's `WSAEACCES` is a raw code.
#[cfg(tokio_backend)]
fn bind_source(e: &std::io::Error) -> &std::io::Error {
    e.get_ref()
        .and_then(|inner| inner.downcast_ref::<BindContext>())
        .map_or(e, |ctx| &ctx.source)
}

/// `tryBind`'s EADDRINUSE: another socket holds the number.
#[cfg(tokio_backend)]
fn bind_was_held(e: &std::io::Error) -> bool {
    port_held(bind_source(e))
}

/// The origin of a bind on the set's number at `addr`. Only an address an
/// earlier clean bind of the set covers — the same address, or the wildcard —
/// co-binds, as loopback's companion does its primary; anywhere else the
/// number must be free. An earlier specific address proves nothing about
/// another, which is why `bind_responder_set` binds wildcards first.
#[cfg(tokio_backend)]
fn origin_at(addr: Ipv4Addr, own: &[Ipv4Addr], origin: PortOrigin) -> PortOrigin {
    if own.iter().any(|h| *h == addr || h.is_unspecified()) {
        PortOrigin::Configured
    } else {
        origin
    }
}

#[cfg(tokio_backend)]
fn bind_responder_socket(bind_ip: Ipv4Addr, port: u16, origin: PortOrigin) -> CaResult<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    // This is a *datagram fanout* socket, so it mirrors libcom's
    // `epicsSocketEnableAddressUseForDatagramFanout`
    // (osdSockAddrReuse.cpp), which the C CA server applies to exactly
    // these sockets — the UDP name receiver and its broadcast companion
    // (caservertask.c:629, :698). That helper sets SO_REUSEPORT (where
    // it exists) followed by SO_REUSEADDR on *every* platform: it has no
    // `#ifdef _WIN32`. Windows must set SO_REUSEADDR too, or a second IOC
    // on the host cannot bind the CA port and never receives a broadcast
    // SEARCH.
    //
    // Do not confuse this with `epicsSocketEnableAddressReuseDuringTimeWaitState`
    // (the TCP time-wait helper), which *is* a Windows no-op because
    // WINSOCK's SO_REUSEADDR has port-hijack semantics there. That rule
    // governs the TCP listener and caRepeater's exclusive bind, not this
    // socket.
    //
    // Ephemeral ports included, as `rsrv_init` applies the helper whatever
    // the port (`caservertask.c:628`, `:698`) — but on a number this server
    // chooses, after the bind (`PortUse::bind`): set before, Linux may hand
    // `bind(0)` a number a same-user fanout socket holds, and that socket's
    // SO_REUSEPORT group then takes every SEARCH from a given client port.
    // libcom commit 51191e6: Linux defaults IP_MULTICAST_ALL=1, which makes
    // a socket bound to 0.0.0.0 receive multicast for groups joined on ANY
    // socket on this host. Clear it so per-NIC search responders don't see
    // foreign multicast traffic. No-op on non-Linux.
    #[cfg(target_os = "linux")]
    {
        let _ = sock.set_multicast_all_v4(false);
    }
    sock.set_nonblocking(true)?;
    // Name the socket in the error: a CA server binds several, and
    // "Address already in use" with no address is unactionable for an
    // operator whose UDP search port is taken by another process.
    PortUse::Shared
        .bind(&sock, SocketAddr::from((bind_ip, port)), origin)
        .map_err(|e| {
            std::io::Error::new(
                e.kind(),
                BindContext {
                    what: format!("CA server UDP search responder bind {bind_ip}:{port}"),
                    source: e,
                },
            )
        })?;
    let socket = UdpSocket::from_std(sock.into())?;
    socket.set_broadcast(true)?;
    // EPICS_CA_MCAST_TTL (epics-base 3.16, f2a1834d). Only consulted by
    // the OS for multicast destinations; safe to apply unconditionally.
    let _ = socket.set_multicast_ttl_v4(epics_base_rs::runtime::net::ca_mcast_ttl());
    // pvxs `udp_collector.cpp::UDPCollector::UDPCollector` parity
    // (commit a064677e3625): opt the kernel into SO_RXQ_OVFL so each
    // recvmsg surfaces the per-socket dropped-datagram counter as a
    // cmsg. No-op on non-Linux. Diagnostic-only; failure to enable
    // is logged at trace and the responder continues normally.
    if let Err(e) = enable_so_rxq_ovfl_for_socket(&socket) {
        tracing::trace!(
            target: "epics_ca_rs::server::udp",
            %bind_ip,
            error = %e,
            "SO_RXQ_OVFL enable failed (non-fatal)"
        );
    }
    Ok(socket)
}

// the standalone `run_multicast_responder` was
// removed. It bound a *second* `0.0.0.0:port` socket per multicast
// group; with `SO_REUSEADDR`/`SO_REUSEPORT` datagram fanout that
// extra socket also caught ordinary unicast/broadcast CA SEARCH
// traffic and emitted duplicate replies. C `caservertask.c:634-666`
// has no such per-group socket — it joins every `casMCastAddrList`
// group on the single `conf->udp` socket of each interface entry.
// `run_single_responder` now owns the joins for both specific and
// wildcard interfaces, matching C.

/// Cross-datagram accumulator for the UDP SEARCH responder: the reply
/// batch under construction plus the per-datagram VERSION-echo state
/// ([`shape_search_reply_dg`] consults both at flush time). Persisting it
/// across [`parse_search_datagram`] calls lets a caller coalesce several
/// same-source datagrams into one reply — C `cast_server.c:266-281` drains
/// the recv queue (FIONREAD) into a single `cas_send_dg_msg`.
#[derive(Default)]
pub(crate) struct SearchReplyBatch {
    /// One outbound datagram in progress. Byte 0 holds the seeded VERSION
    /// placeholder; SEARCH replies are appended after it.
    send_buf: Vec<u8>,
    /// Client sequence captured from a leading VERSION with
    /// `m_dataType == sequenceNoIsValid`; echoed in the reply VERSION.
    client_seq: Option<u32>,
    /// Largest VERSION minor seen this datagram; drives the CA_V411
    /// keep/strip decision at flush time.
    client_minor: Option<u16>,
}

impl SearchReplyBatch {
    /// Shape the trailing (post-last-flush) batch into its on-wire bytes,
    /// or `None` when empty. The blocking responder
    /// (`crate::server::blocking`) calls this to emit the final datagram
    /// after [`parse_search_datagram`] returns; the async responder does the
    /// equivalent through `flush_send_buf`. Encapsulates the private batch
    /// fields so the shaping stays in one place.
    pub(crate) fn shape_trailing(&self) -> Option<Vec<u8>> {
        shape_search_reply_dg(&self.send_buf, self.client_minor, self.client_seq)
    }

    /// Shape the current batch into its on-wire datagram (or `None` when
    /// empty), then reset the batch to empty. The blocking responder
    /// (`crate::server::blocking`) calls this to flush at a coalescing-group
    /// boundary — a recv-queue drain (FIONREAD == 0), a peer change, or
    /// shutdown — which is byte-equivalent to the async responder's
    /// `flush_send_buf` (shape + `send_buf.clear()`) followed by the
    /// peer-change `client_seq`/`client_minor` reset (`recv_loop`,
    /// udp.rs:846-849). Resetting the whole batch keeps the invariant "a
    /// non-empty batch's `client_*` echo state belongs to its own replies"
    /// true by construction for the next group.
    pub(crate) fn take_reply(&mut self) -> Option<Vec<u8>> {
        let dg = self.shape_trailing();
        *self = SearchReplyBatch::default();
        dg
    }
}

/// `log_header`'s context at each of C's three empty-name refusals,
/// `claim_ciu_action`, `search_reply_udp` and `search_reply_tcp`.
pub(crate) const EMPTY_PV_NAME: &str = "empty PV name in UDP search request?";

/// Parse one inbound UDP datagram's CA messages (VERSION + SEARCH),
/// appending SEARCH-reply bytes to `batch`. This is the shared
/// decode/respond core of the CA UDP name-search responder: the async
/// reactor front-end (`recv_loop`) and the blocking thread-per-client
/// front-end (`crate::server::blocking`, the embedded-target server) both
/// call it, so a search reply is byte-identical on either path.
///
/// `batch` persists across calls so the caller can coalesce several
/// same-source datagrams into one reply datagram (C FIONREAD drain). When
/// the accumulated batch would exceed the ~1 KB UDP flush threshold, the
/// current batch is shaped ([`shape_search_reply_dg`]) and pushed to
/// `ready`, and a fresh batch is begun — mirroring `cas_copy_in_header`'s
/// mid-batch flush (`caserverio.c:280-294`). The caller sends each `ready`
/// datagram; the trailing `batch.send_buf` is shaped and sent when the
/// caller's recv queue drains or the peer changes.
///
/// `lookup_src` is the datagram source threaded into `has_name_from` for
/// host-scoped gateway `.pvlist` admission (C `pvExistTest`).
///
/// Contains no socket I/O: the only await is the database name lookup,
/// which resolves on the first poll for a local record, so it is safe to
/// drive under `park_on` on a runtime-less embedded-target thread.
///
/// C: `search_reply_udp` / `udp_version_action` (`rsrv/camessage.c`).
pub(crate) async fn parse_search_datagram(
    input: &[u8],
    db: &PvDatabase,
    tcp_port: u16,
    lookup_src: SocketAddr,
    batch: &mut SearchReplyBatch,
    ready: &mut Vec<Vec<u8>>,
) {
    // match C's `MAX_UDP_SEND = 1024` (`caProto.h:66`).
    // `cas_copy_in_header` (`caserverio.c:280-294`) flushes when the next
    // message would push `stk > maxstk`, so C never builds a UDP reply
    // datagram larger than ~1024 bytes. Third-party CA implementations
    // (Java CAJ, asyncio-ca, embedded ports) may assume that contract and
    // truncate the tail of larger replies. libca peers pre-allocate
    // `recvBuf[MAX_UDP_RECV]` so they tolerate larger, but the wire-byte
    // parity argument favors matching the C constant.
    const UDP_FLUSH_THRESHOLD: usize = 1024;
    let SearchReplyBatch {
        send_buf,
        client_seq,
        client_minor,
    } = batch;
    let mut offset = 0;
    while offset + CaHeader::SIZE <= input.len() {
        let hdr = match CaHeader::from_bytes(&input[offset..]) {
            Ok(h) => h,
            Err(_) => break,
        };
        // C `rsrv/camessage.c:2452` rejects misaligned `m_postsize`.
        // UDP path drops silently (no error response). Without this
        // check, the `align8(postsize)` advancement would jump
        // into the next message's body, mis-parsing chained
        // SEARCH datagrams.
        if (hdr.postsize as usize) & 0x7 != 0 {
            break;
        }
        let payload_size = hdr.postsize as usize;
        let msg_len = CaHeader::SIZE + payload_size;

        if offset + msg_len > input.len() {
            break;
        }

        // C UDP dispatcher (camessage.c:2505-2516) allows only
        // udp_version_action (cmd 0) and search_reply_udp (cmd 6)
        // to succeed. Every other cmd index in the udpJumpTable
        // is bound to bad_udp_cmd_action which returns RSRV_ERROR
        // — the dispatcher loop then `break`s out, dropping the
        // rest of this datagram. Pre-fix Rust just advanced
        // `offset` and parsed the next message regardless;
        // a peer could chain a junk cmd before a SEARCH and the
        // chained SEARCH would still be processed even though
        // C IOC would have stopped parsing at the junk cmd.
        //
        // VERSION's UDP handler (udp_version_action, camessage.c:
        // 2094-2110) is a no-op for the stateless Rust responder:
        // it only stored per-client minor_version_number +
        // seqNoOfReq in C; Rust doesn't track UDP-per-datagram
        // state, so we just allow the VERSION header to pass and
        // continue.
        if hdr.cmmd != CA_PROTO_VERSION && hdr.cmmd != CA_PROTO_SEARCH {
            break;
        }
        if hdr.cmmd == CA_PROTO_VERSION {
            // C `udp_version_action` (rsrv/camessage.c:2094-2110)
            // stores `pclient->seqNoOfReq = m_cid` and the version
            // when the leading VERSION header marks the seq valid
            // (`m_dataType == sequenceNoIsValid`, caProto.h:128).
            // Capture it here so the SEARCH-reply branch can
            // populate its VERSION echo and match
            // `cas_send_dg_msg` byte-for-byte.
            //
            // C `udp_version_action` returns RSRV_ERROR on
            // `!CA_VSUPPORTED(m_count)` and the UDP dispatcher
            // breaks out of the current datagram on any non-OK
            // status. Pre-fix Rust accepted any VERSION and
            // happily kept parsing later messages in the same
            // datagram — a malformed VERSION-first datagram could
            // still elicit a Rust SEARCH reply where rsrv would
            // have dropped the rest. Mirror C: bad version
            // breaks the per-datagram parse.
            if !crate::protocol::declares_supported_version(&hdr) {
                break;
            }
            // track the largest VERSION minor seen so the
            // flush-time placeholder strip/keep decision matches
            // `CA_V411(minor_version_number)` regardless of
            // whether the inbound's leading frame is VERSION,
            // SEARCH, or chained.
            *client_minor = Some(client_minor.unwrap_or(0).max(hdr.count));
            if hdr.data_type == 1 {
                *client_seq = Some(hdr.cid);
            }
        }
        if hdr.cmmd == CA_PROTO_SEARCH {
            // C `search_reply_udp` (rsrv/camessage.c:2151-2153)
            // rejects unsupported minor versions BEFORE the
            // empty-name check. `CA_VSUPPORTED(minor) = minor >= 4`
            // (CA_MINIMUM_SUPPORTED_VERSION in caProto.h:34). C
            // returns RSRV_ERROR which skips the reply. Ancient
            // libca clients (pre-V4.4) parse search replies with a
            // different layout; emitting our V4.13 reply confuses
            // them or worse, fabricates a usable channel they
            // can't actually open.
            // C `search_reply_udp` (camessage.c:2151-2153)
            // returns RSRV_ERROR on unsupported minor version and
            // the UDP dispatcher breaks out of the datagram. Pre-
            // fix Rust skipped only the offending SEARCH and kept
            // parsing later messages in the same datagram, so a
            // malformed SEARCH-first datagram could still elicit
            // a Rust reply for a later message where rsrv would
            // have dropped the rest. Match C: bad-version SEARCH
            // ends the per-datagram parse.
            if !crate::protocol::declares_supported_version(&hdr) {
                break;
            }
            // C `search_reply_udp` (rsrv/camessage.c:2210-2214) rejects
            // SEARCH whose `m_postsize <= 1` with `log_header` ("empty PV
            // name in UDP search request?") and returns RSRV_OK. The
            // null-terminator alone is 1 byte; a usable PV name
            // needs at least one non-null byte plus the terminator
            // (postsize >= 2). Without this guard the Rust path
            // would parse `pv_name = ""` from an attacker's empty-
            // postsize SEARCH burst and call `db.has_name("")` on
            // every datagram — wasted lookups + a non-trivial
            // amplification vector if a record happened to be
            // named "" (impossible in practice, but the C side
            // documents the reject and we match it).
            if hdr.postsize <= 1 {
                crate::server::tcp::print_log_header(lookup_src, &hdr, EMPTY_PV_NAME, 0, None);
                offset += msg_len;
                continue;
            }
            let payload_start = offset + CaHeader::SIZE;
            let payload_end = payload_start + hdr.postsize as usize;
            let payload = &input[payload_start..payload_end];

            // Extract PV name (null-terminated)
            // C `search_reply_udp` forces
            // `pName[mp->m_postsize - 1] = '\0'`. Cap the
            // NUL search at `postsize - 1` so an unterminated
            // peer name is treated as a `postsize - 1` byte
            // name (matching rsrv) rather than the full
            // payload (Rust pre-fix).
            let scan_end = payload.len().saturating_sub(1);
            let pv_name_end = payload[..scan_end]
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(scan_end);
            {
                let pv_name = &payload[..pv_name_end];
                // thread the datagram source address
                // into the search resolver so the CA gateway can
                // apply host-scoped `.pvlist` `DENY FROM host`
                // admission at search time (parity with C
                // `pvExistTest` passing the client host to
                // `gateAs::findEntry`).
                if db.has_name_from(pv_name, Some(lookup_src)).await {
                    // C parity: `search_reply_udp`
                    // (`rsrv/camessage.c:2193-2197`) sets
                    // `sid = ~0U` (INADDR_BROADCAST), telling
                    // the client to use the UDP packet's source
                    // address as the server IP. The previous
                    // code embedded a probe-derived
                    // `local_ip_for(src)` which (a) diverged from
                    // C byte-for-byte and (b) could resolve to
                    // the wrong interface on multi-homed hosts —
                    // the probe binds 0.0.0.0:0 and `connect`s
                    // to the client, but the kernel's outgoing-
                    // interface choice may not match the
                    // interface the client used to reach us.
                    // Using the sentinel delegates the IP
                    // determination to the receiver, which gets
                    // it right by construction (the UDP source
                    // IP is whatever the client sees on the
                    // reply packet).
                    let mut resp = CaHeader::new(CA_PROTO_SEARCH);
                    resp.postsize = 8;
                    resp.data_type = tcp_port;
                    resp.count = 0;
                    resp.cid = u32::MAX; // ~0U — "use UDP source address"
                    resp.available = hdr.available;

                    let mut ver = CaHeader::new(CA_PROTO_VERSION);
                    ver.count = CA_MINOR_VERSION;
                    // Placeholder VERSION header — `cid` and
                    // `data_type` get patched at flush time once
                    // we know whether the inbound carried a
                    // CA_V411 VERSION.

                    let resp_bytes = resp.to_bytes();
                    let mut search_payload = [0u8; 8];
                    search_payload[0..2].copy_from_slice(&CA_MINOR_VERSION.to_be_bytes());

                    // accumulate into send_buf
                    // and ALWAYS pre-seed a VERSION placeholder
                    // at byte 0 of a fresh batch — matching
                    // `rsrv_version_reply`'s up-front seed.
                    // `cas_send_dg_msg` decides at flush time
                    // whether to keep (CA_V411 peer) or strip
                    // (pre-V4.11 peer) those 16 bytes. The
                    // placeholder always being present means a
                    // chained inbound that puts SEARCH before
                    // VERSION still gets a VERSION-led reply.
                    // Flush before append if the next reply
                    // would push us over the MTU; the post-
                    // flush re-seed (handled below) mirrors C's
                    // per-flush re-seed.
                    const SEARCH_REPLY_LEN: usize = CaHeader::SIZE + 8;
                    if !send_buf.is_empty()
                        && send_buf.len() + SEARCH_REPLY_LEN > UDP_FLUSH_THRESHOLD
                    {
                        // Over the ~1 KB MTU: shape the current batch, hand it
                        // to the caller to send, and begin a fresh batch. C
                        // `cas_copy_in_header` flushes mid-build the same way
                        // (`caserverio.c:280-294`).
                        if let Some(dg) =
                            shape_search_reply_dg(&send_buf[..], *client_minor, *client_seq)
                        {
                            ready.push(dg);
                        }
                        send_buf.clear();
                    }
                    if send_buf.is_empty() {
                        send_buf.extend_from_slice(&ver.to_bytes());
                    }
                    send_buf.extend_from_slice(&resp_bytes);
                    send_buf.extend_from_slice(&search_payload);
                }
                // C parity: `search_reply_udp` (rsrv/camessage.c:2167-2170)
                // silently returns on `dbChannelTest` failure for ALL
                // UDP searches — there is no DO_REPLY branch on the
                // UDP path. Only `search_reply_tcp` honours the flag
                // and emits CA_PROTO_NOT_FOUND. Emitting NOT_FOUND
                // here surprised C libca clients running through a
                // name-server-list iteration: a UDP NOT_FOUND from a
                // peer would short-circuit the broadcast search,
                // missing IOCs that hadn't responded yet.
            }
        }

        offset += msg_len;
    }
}

#[cfg(tokio_backend)]
async fn recv_loop(
    socket: Arc<UdpSocket>,
    db: Arc<PvDatabase>,
    bind_ip: Ipv4Addr,
    tcp_port: u16,
    ignore_addrs: Vec<Ipv4Addr>,
    udp_rl: Arc<UdpRateLimiter>,
    // C's own name for the thread this loop replaces: `CAS-UDP` for an
    // interface's primary socket, `CAS-UDP2` for its broadcast secondary
    // (`caservertask.c:723`, `:733`). It is the watchdog row's name, so the
    // two sockets of one interface are told apart in the report.
    role: &'static str,
) -> CaResult<()> {
    // C's UDP name-search thread registers with the watchdog through the same
    // `casAttachThreadToClient` its TCP siblings use — `cast_server.c:151`
    // builds a `client` for the datagram socket and attaches this thread to
    // it. Unbounded: the loop parks in `recv_from`, and an IOC nobody is
    // searching for is quiet rather than wedged.
    let _watched = epics_base_rs::runtime::taskwd::taskwd_insert(
        format!("{role} {bind_ip}"),
        epics_base_rs::runtime::taskwd::CheckIn::Unbounded,
        None,
    );
    // 64 KB receive buffer — IPv4 maximum datagram size. The previous
    // 4 KB cap silently truncated bursts of multi-PV searches in
    // active facilities (each search message is ~24 bytes inc. PV
    // name; 4 KB held ~150 PVs while a typical site burst is many
    // hundreds, especially during gateway restart storms). 64 KB
    // matches the kernel ceiling without risking truncation.
    // Heap-allocated because 64 KB on the per-task stack is large
    // and the `Box<[u8]>` cost is amortized over the listener's
    // lifetime — one allocation, reused on every recv.
    let mut buf = vec![0u8; 64 * 1024];

    // Tracks the previously-observed SO_RXQ_OVFL counter for this
    // socket. Logged on transitions only — pvxs `udp_collector.cpp:55-67`.
    let mut prev_drops: u32 = 0;

    // secondary buffer for peek-and-drain across queued
    // inbounds. Heap-allocated once and reused on every iteration.
    let mut peek_buf = vec![0u8; 64 * 1024];

    loop {
        // C `cast_server.c:171-179`: a UDP recv error never exits the cast
        // server thread — it logs and sleeps 1 s, then keeps receiving. On
        // Windows, `WSAECONNRESET` (os error 10054) surfaces here whenever an
        // earlier send from this socket drew an ICMP port-unreachable
        // (KB263823); on Linux a connected-socket ICMP shows as
        // `ECONNREFUSED`. Propagating the error instead killed the SEARCH
        // responder for the rest of the server's life.
        let (len, src, drops) = match recv_from_with_drop_count_socket(&socket, &mut buf).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    target: "epics_ca_rs::server::udp",
                    %bind_ip,
                    "CAS: UDP recv error: {e}"
                );
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };
        if drops != 0 && drops != prev_drops {
            tracing::debug!(
                target: "epics_ca_rs::server::udp",
                %bind_ip,
                prev = prev_drops,
                drops,
                "CA server UDP search responder buffer overflow"
            );
        }
        prev_drops = drops;
        if len < CaHeader::SIZE {
            continue;
        }

        // Apply ignore list (EPICS_CAS_IGNORE_ADDR_LIST). Any datagram
        // whose source IP appears in the list is silently dropped.
        if let SocketAddr::V4(v4) = src {
            if ignore_addrs.contains(v4.ip()) {
                continue;
            }
        }

        // Per-source-IP rate limit gate.
        if !udp_rl.allow(&src) {
            metrics::counter!("ca_server_udp_search_drops_total").increment(1);
            continue;
        }

        // per-(src, batch) state that survives across queued
        // same-src inbounds. Pre-fix Rust created a fresh send_buf
        // per inbound and flushed immediately, so a search storm of
        // N small datagrams from the same client yielded N reply
        // datagrams. C `cast_server.c:266-281` only flushes when
        // FIONREAD reports the recv queue is drained OR the peer
        // changes. Mirror that: accumulate same-src replies across
        // peeked inbounds; flush on peer change or queue drain.
        let mut current_src = src;
        // Owned copy of the current inbound so subsequent
        // `try_recv_from(&mut peek_buf)` peeks don't conflict with
        // the parse borrow. Reused via `clear()` + `extend_from_slice`.
        let mut current_buf: Vec<u8> = Vec::with_capacity(64 * 1024);
        current_buf.extend_from_slice(&buf[..len]);
        // Reply batch + per-datagram VERSION-echo state, persisted across
        // this inbound and any same-source datagrams drained below, so a
        // search storm of N same-peer datagrams yields ONE outbound — C
        // `cast_server.c:266-281` accumulates into one `cas_send_dg_msg`.
        let mut batch = SearchReplyBatch::default();
        'parse: loop {
            let mut ready: Vec<Vec<u8>> = Vec::new();
            parse_search_datagram(&current_buf, &db, tcp_port, src, &mut batch, &mut ready).await;
            // Send this datagram's over-threshold batches to its peer. C
            // `cas_send_dg_msg` flushes each full batch as it is built
            // (`caserverio.c:280-294`); the trailing partial batch is flushed
            // by the peer-change / queue-drain logic below.
            for dg in ready.drain(..) {
                send_reply_dg(&socket, current_src, &dg, &bind_ip).await;
            }
            // peek for queued inbounds. C `cast_server.c:266-281`
            // calls `socket_ioctl(FIONREAD, &nchars)` after each
            // `camessage()` and ONLY flushes `cas_send_dg_msg` when
            // `nchars == 0` or the peer changes. Mirror that — drain
            // queued same-src inbounds into the same `send_buf` so a
            // search storm of N small same-peer datagrams yields ONE
            // outbound, not N. Different-src peeks flush the current
            // batch and start a fresh one for the new src.
            //
            // a queued datagram that is rejected (short header,
            // ignore-list, rate limit) must NOT restart the parser
            // over the *previous* datagram's bytes still sitting in
            // `current_buf`, and must NOT leave `current_src` pointing
            // at the rejected peer. C `cast_server.c` always overwrites
            // `client->recv.buf` with `recvfrom` before `camessage()`
            // runs, and an ignored datagram sets `status = -1` so the
            // whole parse + `client->addr` update is skipped. Mirror
            // that: drain-and-discard rejected datagrams in this inner
            // loop without touching `current_src` / `current_buf`;
            // only an accepted datagram performs the peer-change flush,
            // replaces `current_buf`, and re-enters `'parse`.
            let next_datagram = loop {
                match socket.try_recv_from(&mut peek_buf) {
                    Ok((peek_len, peek_src)) => {
                        // C `cast_server.c` requires a full caHdr before
                        // `camessage()` will parse anything; a short
                        // datagram yields no SEARCH work. Discard it
                        // and try to drain the next queued datagram —
                        // do NOT re-parse `current_buf`.
                        if peek_len < CaHeader::SIZE {
                            continue;
                        }
                        // Ignore-list / rate-limit rejections discard
                        // the datagram without changing peer state —
                        // C skips `camessage()` for `casIgnoreAddrs`
                        // hits via `status = -1`.
                        if let SocketAddr::V4(v4) = peek_src {
                            if ignore_addrs.contains(v4.ip()) {
                                continue;
                            }
                        }
                        if !udp_rl.allow(&peek_src) {
                            metrics::counter!("ca_server_udp_search_drops_total").increment(1);
                            continue;
                        }
                        break Some((peek_len, peek_src));
                    }
                    Err(_) => break None, // recv queue drained
                }
            };
            match next_datagram {
                Some((peek_len, peek_src)) => {
                    if peek_src != current_src {
                        // Peer change: flush current batch to the old
                        // src, then reset batch state for the new src.
                        flush_send_buf(&socket, current_src, &mut batch, &bind_ip).await;
                        current_src = peek_src;
                        batch.client_seq = None;
                        batch.client_minor = None;
                    }
                    // Replace `current_buf` with the accepted
                    // datagram's bytes BEFORE re-entering `'parse` so
                    // the parser never reprocesses the previous
                    // datagram under a new `current_src`.
                    current_buf.clear();
                    current_buf.extend_from_slice(&peek_buf[..peek_len]);
                    continue 'parse;
                }
                None => break 'parse, // recv queue drained
            }
        } // 'parse
        // flush the accumulated SEARCH replies as a single outbound
        // datagram. `cas_send_dg_msg` does the same after each batch is
        // fully parsed.
        if !batch.send_buf.is_empty() {
            flush_send_buf(&socket, current_src, &mut batch, &bind_ip).await;
        }
    }
}

/// Shape the final on-wire bytes of one accumulated SEARCH-reply batch —
/// the pure, I/O-free core of `cas_send_dg_msg` (`caserverio.c:185-201`).
/// Shared by the async responder (`flush_send_buf`) and the blocking
/// thread-per-client responder (`crate::server::blocking`), so a reply is
/// byte-identical on either front-end.
///
/// If `client_minor >= 11` (CA_V411 peer), the seeded VERSION placeholder
/// at bytes 0..16 is patched with the final header (cid = client seq if
/// any, data_type = 1 when seq present). Otherwise the 16-byte placeholder
/// is stripped — pre-V4.11 peers must not see the VERSION header. Returns
/// `None` when there is nothing to send. Does not mutate `send_buf`; the
/// caller clears it after a flush.
pub(crate) fn shape_search_reply_dg(
    send_buf: &[u8],
    client_minor: Option<u16>,
    client_seq: Option<u32>,
) -> Option<Vec<u8>> {
    if send_buf.is_empty() {
        return None;
    }
    if client_minor.is_some_and(|m| m >= 11) {
        // Patch placeholder at bytes 0..16 with final seq/data_type.
        // The placeholder was seeded with cid=0, data_type=0.
        let mut out = send_buf.to_vec();
        if out.len() >= CaHeader::SIZE {
            let mut ver = CaHeader::new(CA_PROTO_VERSION);
            ver.count = CA_MINOR_VERSION;
            if let Some(seq) = client_seq {
                ver.cid = seq;
                ver.data_type = 1;
            }
            out[..CaHeader::SIZE].copy_from_slice(&ver.to_bytes());
        }
        Some(out)
    } else if send_buf.len() >= CaHeader::SIZE {
        // Pre-V4.11 peer: strip the placeholder.
        Some(send_buf[CaHeader::SIZE..].to_vec())
    } else {
        // Defensive: nothing past the placeholder, nothing to send.
        None
    }
}

/// Send one already-shaped reply datagram to `src`. On failure, log at warn
/// level instead of silently discarding (`caserverio.c:214-222`
/// `errlogPrintf` parity).
#[cfg(tokio_backend)]
async fn send_reply_dg(socket: &UdpSocket, src: SocketAddr, payload: &[u8], bind_ip: &Ipv4Addr) {
    if let Err(e) = socket.send_to(payload, src).await {
        tracing::warn!(
            target: "epics_ca_rs::server::udp",
            %bind_ip,
            dst = %src,
            payload_len = payload.len(),
            error = %e,
            "CA server UDP SEARCH-reply batch send failed"
        );
        metrics::counter!("ca_server_udp_search_reply_send_failures_total").increment(1);
    }
}

/// Shape the batch's trailing `send_buf` and send it as one datagram, then
/// clear the batch buffer. The async responder's flush path.
#[cfg(tokio_backend)]
async fn flush_send_buf(
    socket: &UdpSocket,
    src: SocketAddr,
    batch: &mut SearchReplyBatch,
    bind_ip: &Ipv4Addr,
) {
    if let Some(payload) =
        shape_search_reply_dg(&batch.send_buf, batch.client_minor, batch.client_seq)
    {
        send_reply_dg(socket, src, &payload, bind_ip).await;
    }
    batch.send_buf.clear();
}

/// Per-source-IP token bucket on the UDP search responder. Mitigates
/// amplification (a tiny SEARCH eliciting a much larger SEARCH_REPLY
/// across many records) and absurd loops from misconfigured clients.
///
/// Disabled when neither env var is set; the cost is one IP-equality
/// comparison per packet otherwise. The implementation is a fixed
/// 1-second sliding window — coarse but cheap; replace with
/// per-IP token buckets if a finer policy is ever needed.
#[cfg(tokio_backend)]
struct UdpRateLimiter {
    enabled: bool,
    cap_per_sec: u32,
    counts:
        std::sync::Mutex<std::collections::HashMap<std::net::IpAddr, (std::time::Instant, u32)>>,
}

#[cfg(tokio_backend)]
impl UdpRateLimiter {
    fn from_env() -> Self {
        let cap = epics_base_rs::runtime::env::get_lossy("EPICS_CAS_UDP_SEARCH_RATE_LIMIT")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0u32);
        Self {
            enabled: cap > 0,
            cap_per_sec: cap,
            counts: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn allow(&self, src: &SocketAddr) -> bool {
        if !self.enabled {
            return true;
        }
        let ip = src.ip();
        let now = std::time::Instant::now();
        let mut counts = self.counts.lock().unwrap();
        let entry = counts.entry(ip).or_insert((now, 0));
        if now.duration_since(entry.0) >= std::time::Duration::from_secs(1) {
            entry.0 = now;
            entry.1 = 0;
        }
        if entry.1 >= self.cap_per_sec {
            return false;
        }
        entry.1 += 1;
        // Periodic GC: prune stale entries every 1024 packets to keep
        // the map bounded under DDoS conditions where sources rotate.
        if counts.len() > 4096 {
            // Age forward (`now - t`) rather than a `now - 5s` cutoff:
            // subtracting a Duration from an Instant panics on Windows
            // (QPC-since-boot) when machine uptime is shorter than 5s.
            counts.retain(|_, (t, _)| {
                now.saturating_duration_since(*t) <= std::time::Duration::from_secs(5)
            });
        }
        true
    }
}

#[cfg(tokio_backend)]
#[cfg(test)]
mod mr_r8_responder_plan_tests {
    //! a wildcard interface configuration must produce exactly
    //! one responder socket — the single owner of ordinary
    //! unicast/broadcast SEARCH traffic — even when multicast groups
    //! are configured. No extra per-multicast-group `0.0.0.0:port`
    //! socket may be created.
    use super::plan_responder_specs;
    use std::net::Ipv4Addr;

    /// Pre-fix, a wildcard interface plus N multicast groups spawned
    /// 1 primary + N group-only responders, all bound `0.0.0.0:port`
    /// — duplicate-reply fanout. The plan must now be a single
    /// wildcard responder that itself carries every multicast group.
    #[test]
    fn mr_r8_wildcard_with_mcast_groups_yields_one_responder() {
        let groups = vec![Ipv4Addr::new(224, 0, 0, 100), Ipv4Addr::new(224, 0, 0, 101)];
        // Empty intf list → default wildcard interface.
        let specs = plan_responder_specs(Vec::new(), &groups);
        assert_eq!(
            specs.len(),
            1,
            "wildcard config must produce exactly ONE responder \
             socket, not one-per-multicast-group"
        );
        let (bind_ip, mcast) = &specs[0];
        assert_eq!(*bind_ip, Ipv4Addr::UNSPECIFIED);
        assert_eq!(
            mcast, &groups,
            "the single wildcard responder must own ALL multicast \
             group joins (C `conf->udp` parity)"
        );

        // An explicit `0.0.0.0` entry behaves identically.
        let specs2 = plan_responder_specs(vec![Ipv4Addr::UNSPECIFIED], &groups);
        assert_eq!(specs2.len(), 1);
        assert_eq!(specs2[0].1, groups);
    }

    /// A specific-interface configuration keeps one responder per
    /// interface entry, each carrying the full multicast group list.
    #[test]
    fn mr_r8_specific_intfs_each_own_all_mcast_groups() {
        let groups = vec![Ipv4Addr::new(224, 0, 0, 200)];
        let intfs = vec![Ipv4Addr::new(10, 0, 0, 1), Ipv4Addr::new(10, 0, 0, 2)];
        let specs = plan_responder_specs(intfs.clone(), &groups);
        assert_eq!(specs.len(), 2, "one responder per interface entry");
        for (i, (bind_ip, mcast)) in specs.iter().enumerate() {
            assert_eq!(*bind_ip, intfs[i]);
            assert_eq!(
                mcast, &groups,
                "each interface responder joins every multicast group \
                 on its own socket (C `conf->udp` parity)"
            );
        }
    }

    /// No multicast groups: exactly one wildcard responder with an
    /// empty group list — no spurious extra sockets.
    #[test]
    fn mr_r8_no_mcast_groups_yields_single_plain_responder() {
        let specs = plan_responder_specs(Vec::new(), &[]);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].0, Ipv4Addr::UNSPECIFIED);
        assert!(specs[0].1.is_empty());
    }
}

#[cfg(tokio_backend)]
#[cfg(test)]
mod responder_bind_tests {
    use super::*;

    /// `bind` until it stops answering EADDRINUSE, at most eight times. The
    /// port these tests take is chosen on one socket and then bound on
    /// another, so any other process on the box can hold it in between; that
    /// is a lost race of the rig, retried on a fresh number.
    fn bind_retrying_addr_in_use(mut bind: impl FnMut() -> CaResult<BoundUdp>) -> BoundUdp {
        for _ in 0..8 {
            match bind() {
                Err(epics_base_rs::error::CaError::Io(e)) if bind_was_held(&e) => {}
                r => return r.expect("bind responders"),
            }
        }
        panic!("eight chosen ports in a row were already in use");
    }

    /// The bundle-bind error path itself: a port another socket holds
    /// exclusively fails the bind, naming the address. Windows answers a
    /// `SO_REUSEADDR` bind on a held specific address with WSAEACCES, never
    /// WSAEADDRINUSE.
    #[test]
    fn a_held_port_fails_the_responder_bind() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _g = rt.enter();

        let held = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("hold a port");
        let port = held.local_addr().expect("held addr").port();
        match bind_udp_responders(port, vec![Ipv4Addr::LOCALHOST], &[]) {
            Err(epics_base_rs::error::CaError::Io(e)) => {
                let held = if cfg!(windows) {
                    std::io::ErrorKind::PermissionDenied
                } else {
                    std::io::ErrorKind::AddrInUse
                };
                assert_eq!(e.kind(), held, "{e}");
                assert!(
                    e.to_string().contains(&format!("127.0.0.1:{port}")),
                    "the error names the address: {e}"
                );
            }
            Err(e) => panic!("expected an I/O error, got {e}"),
            Ok(_) => panic!("a port held exclusively must not bind"),
        }
    }

    /// `tryBind` drops one held interface and `rsrv_init` serves on the rest
    /// (`caservertask.c:136`, `:630`, `:745-752`); only an empty set fails.
    #[test]
    fn a_held_interface_is_dropped_and_the_others_serve() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _g = rt.enter();

        // A stranger may hold the picked number on 127.0.0.1 as well; that
        // pick says nothing about the rule, so take another.
        let (held, bound) = (0..8)
            .find_map(|_| {
                let held =
                    std::net::UdpSocket::bind((Ipv4Addr::new(127, 0, 0, 2), 0)).expect("hold");
                let port = held.local_addr().expect("held addr").port();
                bind_udp_responders(
                    port,
                    vec![Ipv4Addr::LOCALHOST, Ipv4Addr::new(127, 0, 0, 2)],
                    &[],
                )
                .ok()
                .map(|bound| (held, bound))
            })
            .expect("the free interface serves");
        assert_eq!(bound.port(), held.local_addr().unwrap().port());
        assert_eq!(bound.responders.len(), 1);
    }

    /// A held broadcast-address socket drops the interface whole, as `tryBind`
    /// on `udpbcast` failing with EADDRINUSE does (`caservertask.c:703`).
    #[cfg(ca_udp_bcast)]
    #[test]
    fn a_held_broadcast_socket_drops_the_interface() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _g = rt.enter();

        // 127.0.0.2 stands in for the broadcast address, which loopback has
        // none of; plain std sockets set no reuse option, so ours cannot
        // co-bind it.
        let (ip, bcast) = (Ipv4Addr::new(127, 0, 0, 3), Ipv4Addr::new(127, 0, 0, 2));
        let lookup = |i: Ipv4Addr| (i == ip).then_some(bcast);

        // A stranger may hold the picked number on 127.0.0.1 as well; that
        // pick says nothing about the rule, so take another, the failed hold
        // parked so the kernel does not hand its number back.
        let mut parked = Vec::new();
        let (_held, port, bound) = (0..8)
            .find_map(|_| {
                let held =
                    std::net::UdpSocket::bind((bcast, 0)).expect("hold the broadcast address");
                let port = held.local_addr().unwrap().port();
                match bind_udp_responders_with(port, vec![Ipv4Addr::LOCALHOST, ip], &[], 0, &lookup)
                {
                    Ok(bound) => Some((held, port, bound)),
                    Err(_) => {
                        parked.push(held);
                        None
                    }
                }
            })
            .expect("the other interface serves");
        assert_eq!(bound.interfaces(), [Ipv4Addr::LOCALHOST]);
        assert!(bind_udp_responders_with(port, vec![ip], &[], 0, &lookup).is_err());
    }

    /// A broadcast-socket bind error other than EADDRINUSE is `tryBind`'s
    /// print-and-suspend path (`caservertask.c:135-141`); here the start
    /// returns the error, and the line is C's.
    #[cfg(ca_udp_bcast)]
    #[test]
    fn a_broadcast_bind_error_other_than_addr_in_use_fails_the_start() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _g = rt.enter();

        // 192.0.2.1 (TEST-NET-1) is no address of this host: EADDRNOTAVAIL.
        let lookup = |_: Ipv4Addr| Some(Ipv4Addr::new(192, 0, 2, 1));
        match bind_udp_responders_with(0, vec![Ipv4Addr::new(127, 0, 0, 3)], &[], 0, &lookup) {
            Err(epics_base_rs::error::CaError::Io(e)) => {
                assert_eq!(e.kind(), std::io::ErrorKind::AddrNotAvailable, "{e}");
                // strerror alone, not the address-carrying chain.
                #[cfg(target_os = "linux")]
                assert_eq!(
                    try_bind_error_line("UDP Socket bcast", &e),
                    "CAS: UDP Socket bcast bind ERROR: Cannot assign requested address"
                );
            }
            Err(e) => panic!("expected EADDRNOTAVAIL, got {e}"),
            Ok(_) => panic!("an unbindable broadcast address must fail the start"),
        }
    }

    #[cfg(ca_udp_bcast)]
    #[test]
    fn an_interface_without_a_broadcast_address_is_warned_about_as_c_does() {
        let ip = Ipv4Addr::new(192, 0, 2, 1);
        assert_eq!(
            missing_broadcast_warning(ip, 5064, None).as_deref(),
            Some(
                "Warning: Can't find broadcast address of interface 192.0.2.1:5064\n         \
                 Name lookup may not work on this interface\n"
            )
        );
        assert_eq!(missing_broadcast_warning(ip, 5064, Some(ip)), None);
        assert_eq!(
            missing_broadcast_warning(Ipv4Addr::UNSPECIFIED, 5064, None),
            None,
            "a wildcard bind has no second socket (`:663`)"
        );
    }

    /// Boundary — port 0: `rsrv_init` enables datagram fanout on every
    /// responder whatever the port (`caservertask.c:628`, `:698`), so a
    /// reuse-enabled socket can co-bind an ephemeral responder's port, as it
    /// can in C.
    #[test]
    fn ephemeral_responder_port_can_be_co_bound() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _g = rt.enter();

        let bound = bind_udp_responders(0, vec![Ipv4Addr::LOCALHOST], &[]).expect("bind ephemeral");
        assert_ne!(bound.port(), 0, "an ephemeral bind reports its real port");

        let peer = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).expect("socket");
        peer.set_reuse_address(true).expect("reuse addr");
        #[cfg(unix)]
        peer.set_reuse_port(true).expect("reuse port");
        let addr = std::net::SocketAddrV4::new(Ipv4Addr::LOCALHOST, bound.port());
        peer.bind(&addr.into())
            .expect("a fanout socket co-binds the responder's port, as in rsrv");
    }

    /// An ephemeral responder's number is never one a same-user fanout socket
    /// already holds. With the reuse options set before `bind(0)`, Linux hands
    /// out such a number about one bind in fifteen against 2000 holders, and
    /// the stranger's `SO_REUSEPORT` group then takes every SEARCH from one
    /// client source port — a client that never connects.
    #[cfg(unix)]
    #[test]
    fn an_ephemeral_responder_never_takes_a_strangers_port() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _g = rt.enter();

        let mut held = Vec::new();
        let mut theirs = std::collections::HashSet::new();
        for _ in 0..2000 {
            let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).expect("socket");
            s.set_reuse_address(true).expect("reuse addr");
            s.set_reuse_port(true).expect("reuse port");
            s.bind(&std::net::SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into())
                .expect("stranger bind");
            theirs.insert(s.local_addr().unwrap().as_socket().unwrap().port());
            held.push(s);
        }
        for _ in 0..200 {
            let bound =
                bind_udp_responders(0, vec![Ipv4Addr::UNSPECIFIED], &[]).expect("bind ephemeral");
            assert!(
                !theirs.contains(&bound.port()),
                "the responder took {}, a stranger's port",
                bound.port()
            );
        }
    }

    /// The number an ephemeral set chose is ours on every interface: a fanout
    /// stranger holding it on a later one restarts the set on a fresh number
    /// instead of being joined. `bind(0)` on 127.0.0.1 ignores sockets on
    /// 127.0.0.4, so about one set in fifteen meets one of 2000 there.
    #[cfg(unix)]
    #[test]
    fn a_stranger_on_a_later_interface_restarts_an_ephemeral_set() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _g = rt.enter();

        let later = Ipv4Addr::new(127, 0, 0, 4);
        let mut held = Vec::new();
        let mut theirs = std::collections::HashSet::new();
        for _ in 0..2000 {
            let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).expect("socket");
            s.set_reuse_address(true).expect("reuse addr");
            s.set_reuse_port(true).expect("reuse port");
            s.bind(&std::net::SocketAddrV4::new(later, 0).into())
                .expect("stranger bind");
            theirs.insert(s.local_addr().unwrap().as_socket().unwrap().port());
            held.push(s);
        }
        let no_bcast = |_: Ipv4Addr| None;
        for _ in 0..200 {
            let bound =
                bind_udp_responders_with(0, vec![Ipv4Addr::LOCALHOST, later], &[], 0, &no_bcast)
                    .expect("bind ephemeral set");
            assert_eq!(bound.interfaces(), [Ipv4Addr::LOCALHOST, later]);
            assert!(
                !theirs.contains(&bound.port()),
                "the set joined a stranger on {later}:{}",
                bound.port()
            );
        }
        // Every restarted set released what it bound: on a stranger's number
        // the only sockets of this process are the strangers.
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let link = |fd: i32| {
                std::fs::read_link(format!("/proc/self/fd/{fd}"))
                    .map(|l| l.to_string_lossy().into_owned())
                    .ok()
            };
            let strangers: std::collections::HashSet<String> =
                held.iter().filter_map(|s| link(s.as_raw_fd())).collect();
            let ours: std::collections::HashSet<String> = std::fs::read_dir("/proc/self/fd")
                .expect("list own fds")
                .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
                .map(|l| l.to_string_lossy().into_owned())
                .collect();
            for port in &theirs {
                let left: Vec<_> = named_port::port_holders::udp_holders(*port)
                    .expect("sock_diag dump")
                    .into_iter()
                    .map(|h| h.fd_link())
                    .filter(|l| ours.contains(l) && !strangers.contains(l))
                    .collect();
                assert!(
                    left.is_empty(),
                    "a refused set still holds {port}: {left:?}"
                );
            }
        }
    }

    /// Boundary — an ephemeral set whose interfaces overlap (the same address,
    /// or either one the wildcard) co-binds its own number, as rsrv's
    /// flags-first binds do; only a stranger's socket restarts the set.
    #[test]
    fn an_ephemeral_set_co_binds_its_own_overlapping_sockets() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _g = rt.enter();

        let (any, lo) = (Ipv4Addr::UNSPECIFIED, Ipv4Addr::LOCALHOST);
        for list in [vec![any, lo], vec![lo, any], vec![lo, lo], vec![any, any]] {
            let bound = bind_udp_responders(0, list.clone(), &[])
                .unwrap_or_else(|e| panic!("{list:?} failed to start: {e}"));
            assert_eq!(bound.interfaces(), list);
        }
    }

    /// A clean bind on 127.0.0.1 says nothing about 127.0.0.4, so a wildcard
    /// listed after it must not co-bind on that strength: a stranger holding
    /// 127.0.0.4 on the number would take every SEARCH sent there. The wildcard
    /// binds first and clean, which proves the number free on every address.
    #[cfg(unix)]
    #[test]
    fn a_wildcard_after_a_specific_interface_never_shares_a_strangers_port() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _g = rt.enter();

        let stranger_ip = Ipv4Addr::new(127, 0, 0, 4);
        let mut held = Vec::new();
        let mut theirs = std::collections::HashSet::new();
        for _ in 0..2000 {
            let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).expect("socket");
            s.set_reuse_address(true).expect("reuse addr");
            s.set_reuse_port(true).expect("reuse port");
            s.bind(&std::net::SocketAddrV4::new(stranger_ip, 0).into())
                .expect("stranger bind");
            theirs.insert(s.local_addr().unwrap().as_socket().unwrap().port());
            held.push(s);
        }
        let list = vec![Ipv4Addr::LOCALHOST, Ipv4Addr::UNSPECIFIED];
        for _ in 0..400 {
            let bound = bind_udp_responders(0, list.clone(), &[]).expect("bind ephemeral set");
            assert_eq!(bound.interfaces(), list);
            assert!(
                !theirs.contains(&bound.port()),
                "the wildcard shares {}, a stranger's port on {stranger_ip}",
                bound.port()
            );
        }
    }

    /// C reaches the watchdog for its UDP name-search thread through the same
    /// `casAttachThreadToClient` the TCP side uses (`cast_server.c:151`), so a
    /// `taskwdShow` on a C IOC lists `CAS-UDP` beside `CAS-TCP`. This port
    /// wired only the TCP caller and the name server ran unwatched.
    ///
    /// Asserted through `taskwd_show`'s own output, because that report is
    /// what an operator compares against C, and it is the thing that was
    /// missing a row.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_udp_name_server_is_listed_by_the_task_watchdog() {
        fn table() -> String {
            let out = std::cell::RefCell::new(String::new());
            epics_base_rs::runtime::taskwd::taskwd_show(1, &|line| {
                out.borrow_mut().push_str(line);
                out.borrow_mut().push('\n');
            });
            out.into_inner()
        }

        assert!(
            !table().contains("CAS-UDP"),
            "a name server was registered before this test started one"
        );

        let bound = bind_udp_responders(0, vec![Ipv4Addr::LOCALHOST], &[]).expect("bind ephemeral");
        let reactor = epics_base_rs::runtime::task::Reactor::current()
            .expect("a tokio test always has a reactor");
        let db = Arc::new(PvDatabase::new());
        let task = reactor.spawn({
            let reactor = reactor.clone();
            async move { run_udp_search_responder(&reactor, db, bound, 5064, Vec::new()).await }
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !table().contains("CAS-UDP") {
            assert!(
                std::time::Instant::now() < deadline,
                "the name server never reached the watchdog table:\n{}",
                table()
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        task.abort();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while table().contains("CAS-UDP") {
            assert!(
                std::time::Instant::now() < deadline,
                "an aborted name server stayed registered:\n{}",
                table()
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Boundary — a fixed port keeps the datagram-fanout reuse flags, so
    /// the well-known CA port can still be co-bound (multiple IOCs on one
    /// host must all receive a broadcast SEARCH).
    ///
    /// Runs on Windows too: libcom's fanout helper sets SO_REUSEADDR on
    /// every platform, so the co-bind must work there as well. This test
    /// is what pins that — it failed on Windows while
    /// `bind_responder_socket` was (wrongly) applying the TCP time-wait
    /// helper's Windows carve-out.
    #[test]
    fn fixed_responder_port_still_allows_datagram_fanout() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _g = rt.enter();

        // Take an ephemeral port first, then re-bind that same number as
        // a *fixed* port so the test never guesses at a free one.
        let mut port = 0;
        let bound = bind_retrying_addr_in_use(|| {
            let probe = bind_udp_responders(0, vec![Ipv4Addr::LOCALHOST], &[]).expect("probe bind");
            port = probe.port();
            drop(probe);
            bind_udp_responders(port, vec![Ipv4Addr::LOCALHOST], &[])
        });
        assert_eq!(bound.port(), port);

        let peer = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).expect("socket");
        peer.set_reuse_address(true).expect("reuse addr");
        #[cfg(unix)]
        peer.set_reuse_port(true).expect("reuse port");
        let addr = std::net::SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
        assert!(
            peer.bind(&addr.into()).is_ok(),
            "a well-known CA port must stay co-bindable (caRepeater fanout)"
        );
    }
}
