//! `mshim-rs` — beacon multicast shim mirroring pvxs `tools/mshim.cpp`.
//!
//! Listens on one or more `-L <ip[:port]>` endpoints and forwards
//! every received UDP datagram to one or more `-F <ip[:port]>`
//! destinations. Used to bridge IPv4 multicast to PVA clients /
//! servers that don't speak multicast natively.
//!
//! ```text
//! # 1. Forward local SEARCH packets to a multicast group:
//! mshim-rs -L 127.0.0.1:15076 -F 224.1.1.1:5076
//!
//! # 2. Forward multicast BEACONs back to local clients:
//! mshim-rs -L 224.1.1.1:5076 -F 127.0.0.1:15076
//!
//! # 3. Join / forward via a specific interface, custom TTL:
//! mshim-rs -L 224.1.1.1:5076@eth0 -F 224.1.1.1:5076,32@eth1
//! ```
//!
//! pvxs syntax (`tools/mshim.cpp`): a `-L`/`-F` entry may carry a
//! `,ttl#` TTL override and/or an `@iface` interface override.
//! `@iface` accepts either an interface name (`eth0`) or that
//! interface's IPv4 address; on the listen side it scopes the
//! multicast group join, on the forward side it selects the outbound
//! multicast interface. `,ttl#` sets `IP_MULTICAST_TTL` on forwarded
//! multicast packets.
// On `exec_backend` this program's `main` refuses instead of running, so
// everything below it is unreachable in that configuration by construction.
// The lint is reporting the intent, not dead code: the default build still
// lints this file in full.
#![cfg_attr(exec_backend, allow(dead_code, unused_imports))]

use std::collections::HashSet;
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket as StdUdpSocket};
use std::sync::Arc;

use clap::Parser;
use epics_base_rs::net::IfaceMap;
use epics_pva_rs::cli;
#[cfg(tokio_backend)]
use epics_pva_rs::server_native::udp::ForwardableDatagram;
use socket2::{Domain, Protocol, SockRef, Socket, Type};
use tokio::net::UdpSocket;

#[derive(Parser)]
#[command(
    name = "mshim-rs",
    about = "PVA beacon/search multicast shim",
    disable_version_flag = true
)]
struct Args {
    /// Print version information (pvxs `version_information`, tools
    /// `case 'V'`) and exit. Routed through `cli::version_information`
    /// so every PVA CLI reports the same library + protocol stack
    /// instead of clap's crate-only `<binary> <version>`.
    #[arg(short = 'V', long = "version")]
    version: bool,

    /// Listen endpoint `<ip>[:port][,ttl#][@iface]`. Repeat for
    /// multiple. Multicast groups are joined automatically; `@iface`
    /// scopes the join to one interface.
    #[arg(short = 'L', long = "listen", required_unless_present = "version")]
    listen: Vec<String>,

    /// Forward destination `<ip>[:port][,ttl#][@iface]`. Repeat for
    /// multiple. `,ttl#` / `@iface` apply to multicast destinations.
    #[arg(short = 'F', long = "forward", required_unless_present = "version")]
    forward: Vec<String>,

    /// Default UDP port if a `-L` / `-F` entry omits one.
    #[arg(short = 'p', long = "port")]
    port: Option<u16>,
}

#[derive(Debug)]
struct Endpoint {
    ip: IpAddr,
    port: u16,
    /// `,ttl#` override — multicast TTL for forwarded packets.
    ttl: Option<u32>,
    /// `@iface` override — interface name or IPv4 address. The join
    /// (listen side) / outbound interface (forward side) is scoped to
    /// this interface.
    iface: Option<String>,
}

/// Parse the pvxs `<ip>[:port][,ttl#][@iface]` endpoint syntax. The
/// `,ttl#` and `@iface` suffixes may appear in either order after the
/// `ip[:port]` head.
fn parse_endpoint(s: &str, default_port: u16) -> Result<Endpoint, String> {
    // Peel `@iface` and `,ttl#` suffixes off the end. Either may come
    // first; the head (`ip[:port]`) is whatever remains.
    let mut head = s;
    let mut ttl: Option<u32> = None;
    let mut iface: Option<String> = None;
    while let Some(idx) = head.rfind(['@', ',']) {
        let sep = &head[idx..idx + 1];
        let suffix = &head[idx + 1..];
        if sep == "@" {
            if suffix.is_empty() {
                return Err("empty @iface override".into());
            }
            iface = Some(suffix.to_string());
        } else {
            let v: u32 = suffix
                .parse()
                .map_err(|e| format!("ttl {suffix:?} invalid: {e}"))?;
            if v == 0 || v > 255 {
                return Err(format!("ttl {v} out of range 1..=255"));
            }
            ttl = Some(v);
        }
        head = &head[..idx];
    }
    let (ip_str, port) = if let Some((a, b)) = head.rsplit_once(':') {
        let port: u16 = b.parse().map_err(|e| format!("port {b:?} invalid: {e}"))?;
        (a, port)
    } else {
        (head, default_port)
    };
    // pvxs `mshim` routes each `-L`/`-F` endpoint through
    // `SockEndpoint(optarg, udp_port)`, whose body is parsed by
    // `SockAddr::setAddress` (mshim.cpp:60), so a DNS hostname like
    // `localhost` is resolved (IPv4-preferred) rather than rejected as a
    // non-literal-IP. `parseEP` then requires the resolved endpoint to be
    // AF_INET (mshim.cpp:66-68) — a non-IPv4 endpoint is an error — and
    // the port to be non-zero (mshim.cpp:70-72). The shared IPv4-only
    // resolver enforces AF_INET (a name with no IPv4 address fails); the
    // zero-port check stays here so a misconfigured `:0` fails at parse
    // time instead of surviving startup and then failing on every send.
    let ip = IpAddr::V4(cli::resolve_host_ipv4(ip_str)?);
    if port == 0 {
        return Err("non-zero port number required".into());
    }
    Ok(Endpoint {
        ip,
        port,
        ttl,
        iface,
    })
}

fn bind_listen(ep: &Endpoint) -> std::io::Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    #[cfg(unix)]
    {
        let _ = sock.set_reuse_port(true);
    }
    sock.set_broadcast(true)?;
    sock.set_nonblocking(true)?;
    // Multicast groups must bind 0.0.0.0; unicast/broadcast bind to
    // the actual address so packets only show up there.
    let bind_addr = if matches!(ep.ip, IpAddr::V4(v4) if v4.is_multicast()) {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), ep.port)
    } else {
        SocketAddr::new(ep.ip, ep.port)
    };
    sock.bind(&bind_addr.into())?;
    if let IpAddr::V4(v4) = ep.ip
        && v4.is_multicast()
    {
        // `@iface`: scope the group join to the named interface, by
        // its IPv4 address. Without it the kernel picks the default.
        let join_iface = match &ep.iface {
            Some(spec) => cli::resolve_iface_ipv4(spec)
                .map_err(|e| std::io::Error::new(ErrorKind::InvalidInput, e))?,
            None => Ipv4Addr::UNSPECIFIED,
        };
        sock.join_multicast_v4(&v4, &join_iface)?;
    }
    let std_sock: StdUdpSocket = sock.into();
    UdpSocket::from_std(std_sock)
}

/// A resolved forward destination plus its per-destination multicast
/// settings (TTL, outbound interface).
#[derive(Clone)]
struct ForwardTarget {
    addr: SocketAddr,
    ttl: Option<u32>,
    iface_v4: Option<Ipv4Addr>,
}

/// The `IP_MULTICAST_TTL` / `IP_MULTICAST_IF` values to assert on the
/// shared send socket before forwarding to `tgt`.
///
/// `None` for a unicast destination — no multicast setsockopt is
/// needed. For a multicast destination it is always `Some`: the
/// override when present, otherwise the platform defaults (TTL 1,
/// interface `INADDR_ANY`). Returning the defaults rather than `None`
/// is the fix — a multicast target WITHOUT overrides must still
/// reset the shared socket so a prior destination's state cannot
/// bleed through. Mirrors pvxs `mcast_prep_sendto`, which runs for
/// every multicast destination.
fn multicast_opts_for(tgt: &ForwardTarget) -> Option<(u32, Ipv4Addr)> {
    if !tgt.addr.ip().is_multicast() {
        return None;
    }
    Some((
        tgt.ttl.unwrap_or(1),
        tgt.iface_v4.unwrap_or(Ipv4Addr::UNSPECIFIED),
    ))
}

/// SEARCH `Unicast` flag classification for a forward destination,
/// mirroring pvxs `mshim.cpp:140-146`: the flag is SET for a true
/// unicast target and CLEARED for any multicast or broadcast target.
///
/// pvxs clears the flag when `dest.addr.isMCast() || ifmap.is_broadcast(dest.addr)`.
/// `IfaceMap::is_broadcast` (`evhelper.cpp:866`) recognizes a host
/// interface's *subnet* broadcast (e.g. `192.168.1.255`), not only the
/// limited broadcast `255.255.255.255` that `Ipv4Addr::is_broadcast()`
/// matches. `host_broadcasts` is the set of those subnet broadcasts,
/// snapshotted once at startup from the local interface map (pvxs holds
/// `IfaceMap::instance()` for the `App` lifetime, `mshim.cpp:79,87`).
/// Without that set a SEARCH forwarded to a subnet broadcast is
/// mislabeled unicast, and the limited-broadcast case is kept because
/// `255.255.255.255` is a broadcast target too.
fn dest_is_unicast(dest: SocketAddr, host_broadcasts: &HashSet<Ipv4Addr>) -> bool {
    match dest.ip() {
        IpAddr::V4(v4) => {
            !v4.is_multicast() && !v4.is_broadcast() && !host_broadcasts.contains(&v4)
        }
        IpAddr::V6(v6) => !v6.is_multicast(),
    }
}

/// Snapshot the host's interface subnet-broadcast addresses once at
/// startup (pvxs `IfaceMap::instance()`). Used by [`dest_is_unicast`].
fn host_broadcast_addrs() -> std::io::Result<HashSet<Ipv4Addr>> {
    Ok(IfaceMap::new()?
        .all()
        .into_iter()
        .filter_map(|i| i.broadcast)
        .collect())
}

#[cfg(tokio_backend)]
#[tokio::main]
async fn main() {
    // pvxs's mshim returns 1 from its bad-option arm (`tools/mshim.cpp`);
    // `Args::parse()` would exit with clap's 2.
    let args: Args =
        epics_pva_rs::cli::parse_or_exit_styled(epics_pva_rs::cli::UsageErrorStyle::Pvxs);

    // pvxs `-V` prints version_information and exits before binding any
    // socket (mshim `case 'V'`).
    if args.version {
        print!("{}", epics_pva_rs::cli::version_information());
        return;
    }

    let default_port = args
        .port
        .or_else(|| {
            std::env::var("EPICS_PVA_BROADCAST_PORT")
                .ok()
                // pvxs-compatible port parse (uint64 + low-16 truncate,
                // whitespace-tolerant) instead of a strict u16 parse.
                .and_then(|s| epics_pva_rs::config::env::parse_port_env(&s))
        })
        .unwrap_or(5076);

    // pvxs rejects a zero port for the fallback too — a 0 default
    // (`-p 0` or `EPICS_PVA_BROADCAST_PORT=0`) cannot be a meaningful
    // PVA broadcast/search port (mshim.cpp:70-72).
    if default_port == 0 {
        eprintln!("mshim-rs: non-zero port number required (got -p/EPICS_PVA_BROADCAST_PORT 0)");
        std::process::exit(2);
    }

    let listen: Vec<Endpoint> = match args
        .listen
        .iter()
        .map(|s| parse_endpoint(s, default_port))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("mshim-rs: {e}");
            std::process::exit(2);
        }
    };
    let forward: Vec<Endpoint> = match args
        .forward
        .iter()
        .map(|s| parse_endpoint(s, default_port))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("mshim-rs: {e}");
            std::process::exit(2);
        }
    };

    // Resolve each forward destination's `@iface` up-front so a bad
    // interface name fails fast rather than per-datagram.
    let forward_targets: Vec<ForwardTarget> = match forward
        .iter()
        .map(|e| {
            let iface_v4 = match &e.iface {
                Some(spec) => Some(cli::resolve_iface_ipv4(spec)?),
                None => None,
            };
            Ok(ForwardTarget {
                addr: SocketAddr::new(e.ip, e.port),
                ttl: e.ttl,
                iface_v4,
            })
        })
        .collect::<Result<Vec<_>, String>>()
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("mshim-rs: forward interface: {e}");
            std::process::exit(2);
        }
    };

    // Build the send socket via socket2 so we can set per-socket
    // multicast options (TTL / outbound interface). Tokio requires
    // nonblocking sockets when adopting via `from_std`.
    let send_socket = match Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("mshim-rs: create send socket: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = send_socket.bind(&SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0).into())
    {
        eprintln!("mshim-rs: bind send socket: {e}");
        std::process::exit(1);
    }
    if let Err(e) = send_socket.set_nonblocking(true) {
        eprintln!("mshim-rs: send socket set_nonblocking: {e}");
        std::process::exit(1);
    }
    if let Err(e) = send_socket.set_broadcast(true) {
        eprintln!("mshim-rs: set_broadcast: {e}");
    }
    // mshim uses a single shared send socket. `IP_MULTICAST_TTL` /
    // `IP_MULTICAST_IF` are setsockopt state on that socket, so the
    // recv loop re-asserts both for every multicast destination before
    // its send — overrides when present, platform defaults otherwise —
    // so one destination's settings never bleed into another's.
    let send_sock_std: StdUdpSocket = send_socket.into();
    let send_sock = match UdpSocket::from_std(send_sock_std) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("mshim-rs: send socket from_std: {e}");
            std::process::exit(1);
        }
    };
    let send_sock = std::sync::Arc::new(send_sock);

    // Snapshot the host's interface subnet-broadcast addresses once so a
    // SEARCH forwarded to e.g. `192.168.1.255` is classified broadcast,
    // not unicast — pvxs `IfaceMap::is_broadcast` parity (mshim.cpp:141).
    let host_broadcasts = match host_broadcast_addrs() {
        Ok(a) => Arc::new(a),
        Err(e) => {
            // Without the snapshot every forwarded broadcast would be
            // misclassified as unicast, so this is fatal rather than an
            // empty set that silently changes the forwarding decision.
            eprintln!("mshim-rs: interface enumeration: {e}");
            std::process::exit(1);
        }
    };

    eprintln!(
        "mshim-rs: listening on {} endpoint(s), forwarding to {} target(s)",
        listen.len(),
        forward.len()
    );
    for ep in &listen {
        let mut extras = Vec::new();
        if let Some(i) = &ep.iface {
            extras.push(format!("iface={i}"));
        }
        if extras.is_empty() {
            eprintln!("  listen {}:{}", ep.ip, ep.port);
        } else {
            eprintln!("  listen {}:{} [{}]", ep.ip, ep.port, extras.join(" "));
        }
    }
    for (ep, tgt) in forward.iter().zip(forward_targets.iter()) {
        let mut extras = Vec::new();
        if let Some(t) = tgt.ttl {
            extras.push(format!("ttl={t}"));
        }
        if let Some(i) = tgt.iface_v4 {
            extras.push(format!("iface={i}"));
        }
        if extras.is_empty() {
            eprintln!("  forward → {}:{}", ep.ip, ep.port);
        } else {
            eprintln!("  forward → {}:{} [{}]", ep.ip, ep.port, extras.join(" "));
        }
    }

    let mut handles = Vec::new();
    for ep in listen {
        let sock = match bind_listen(&ep) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("mshim-rs: bind {}:{}: {e}", ep.ip, ep.port);
                std::process::exit(1);
            }
        };
        let targets = forward_targets.clone();
        let send_sock = send_sock.clone();
        let host_broadcasts = host_broadcasts.clone();
        let h = tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            loop {
                match sock.recv_from(&mut buf).await {
                    Ok((n, peer)) => {
                        // pvxs `mshim` does NOT relay the raw datagram: it
                        // decodes the SEARCH/BEACON, drops anything else,
                        // and rebuilds a fresh wire body per destination —
                        // toggling the SEARCH `Unicast` flag and resolving
                        // an `isAny` reply/server address to the original
                        // source (mshim.cpp:95-200). pvxs decodes only the
                        // FIRST PVA header per datagram and ignores any
                        // trailing bytes (`UDPCollector::process_one`,
                        // udp_collector.cpp:329-352), so forward exactly
                        // one rebuilt message — never amplify a chained
                        // datagram into several forwarded packets.
                        let Some(message) = ForwardableDatagram::decode_first(&buf[..n]) else {
                            // Unrecognized / malformed: drop, never relay
                            // raw bytes (pvxs forwards only decoded msgs).
                            continue;
                        };
                        for tgt in &targets {
                            // Avoid an obvious feedback loop: don't
                            // forward back to the source endpoint of
                            // the same datagram.
                            if tgt.addr == peer {
                                continue;
                            }
                            // SEARCH `Unicast` flag: set for a unicast
                            // destination, cleared for multicast/broadcast
                            // (pvxs mshim.cpp:140-146). A subnet broadcast
                            // (e.g. 192.168.1.255) is recognized via the
                            // host interface map, not just 255.255.255.255.
                            // BEACON ignores the flag.
                            let dest_unicast = dest_is_unicast(tgt.addr, &host_broadcasts);
                            // Apply per-destination multicast options
                            // before the send — for EVERY multicast
                            // target, override or not (see
                            // `multicast_opts_for`). `set_multicast_*`
                            // operate on the shared send socket, so a
                            // destination without overrides must still
                            // reset to the defaults or it inherits the
                            // previous destination's settings.
                            if let Some((ttl, iface)) = multicast_opts_for(tgt) {
                                // `SockRef` borrows the tokio socket's
                                // fd so we can reach the setsockopt-
                                // backed multicast options tokio's
                                // `UdpSocket` doesn't expose directly
                                // (notably `IP_MULTICAST_IF`).
                                let sref = SockRef::from(send_sock.as_ref());
                                if let Err(e) = sref.set_multicast_ttl_v4(ttl) {
                                    eprintln!("mshim-rs: set multicast ttl for {}: {e}", tgt.addr);
                                }
                                if let Err(e) = sref.set_multicast_if_v4(&iface) {
                                    eprintln!(
                                        "mshim-rs: set multicast iface for {}: {e}",
                                        tgt.addr
                                    );
                                }
                            }
                            let frame = message.rebuild_for(dest_unicast, peer);
                            if let Err(e) = send_sock.send_to(&frame, tgt.addr).await
                                && e.kind() != ErrorKind::WouldBlock
                            {
                                eprintln!("mshim-rs: forward to {}: {e}", tgt.addr);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("mshim-rs: recv on {}:{}: {e}", ep.ip, ep.port);
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        });
        handles.push(h);
    }

    // Wait forever; tokio::signal::ctrl_c gives clean exit.
    tokio::select! {
        _ = futures_join_all(handles) => {}
        _ = tokio::signal::ctrl_c() => {
            eprintln!("mshim-rs: shutting down");
        }
    }
}

async fn futures_join_all(handles: Vec<tokio::task::JoinHandle<()>>) {
    for h in handles {
        let _ = h.await;
    }
}

/// The `exec_backend` arm. The shim's whole job is forwarding UDP between
/// interfaces and every collector waits on `server_native::udp`, which is
/// `tokio_backend`-only. Nothing replaces it here: an RTEMS image does not run
/// mshim.
#[cfg(exec_backend)]
fn main() -> std::process::ExitCode {
    eprintln!(
        "mshim-rs: this build selects the reactor-free execution backend \
         (EPICS_RS_BUILD_EXEC_BACKEND=thread), and the UDP forwarder needs a \
         tokio reactor. Unset that variable and rebuild."
    );
    std::process::ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_endpoint_ipv4_with_port() {
        let ep = parse_endpoint("127.0.0.1:5076", 9999).unwrap();
        assert_eq!(ep.ip.to_string(), "127.0.0.1");
        assert_eq!(ep.port, 5076);
        assert!(ep.ttl.is_none());
        assert!(ep.iface.is_none());
    }

    #[test]
    fn parse_endpoint_default_port_when_omitted() {
        let ep = parse_endpoint("224.1.1.1", 5076).unwrap();
        assert_eq!(ep.ip.to_string(), "224.1.1.1");
        assert_eq!(ep.port, 5076);
    }

    #[test]
    fn parse_endpoint_ttl_modifier_parsed() {
        let ep = parse_endpoint("224.1.1.1,255", 5076).unwrap();
        assert_eq!(ep.ip.to_string(), "224.1.1.1");
        assert_eq!(ep.port, 5076);
        assert_eq!(ep.ttl, Some(255));
        assert!(ep.iface.is_none());
    }

    #[test]
    fn parse_endpoint_iface_modifier_parsed() {
        let ep = parse_endpoint("224.1.1.1@eth0", 5076).unwrap();
        assert_eq!(ep.ip.to_string(), "224.1.1.1");
        assert_eq!(ep.iface.as_deref(), Some("eth0"));
        assert!(ep.ttl.is_none());
    }

    #[test]
    fn parse_endpoint_ipv4_port_with_iface() {
        // pvxs syntax: "224.1.1.1:5076@eth0"
        let ep = parse_endpoint("224.1.1.1:5076@eth0", 9999).unwrap();
        assert_eq!(ep.ip.to_string(), "224.1.1.1");
        assert_eq!(ep.port, 5076);
        assert_eq!(ep.iface.as_deref(), Some("eth0"));
    }

    #[test]
    fn parse_endpoint_ttl_and_iface_together() {
        // pvxs syntax: "<ip>:port,ttl#@iface"
        let ep = parse_endpoint("224.1.1.1:5076,32@eth1", 9999).unwrap();
        assert_eq!(ep.ip.to_string(), "224.1.1.1");
        assert_eq!(ep.port, 5076);
        assert_eq!(ep.ttl, Some(32));
        assert_eq!(ep.iface.as_deref(), Some("eth1"));
    }

    #[test]
    fn parse_endpoint_iface_then_ttl_order() {
        // suffixes accepted in either order
        let ep = parse_endpoint("224.1.1.1@eth0,8", 5076).unwrap();
        assert_eq!(ep.ttl, Some(8));
        assert_eq!(ep.iface.as_deref(), Some("eth0"));
    }

    #[test]
    fn parse_endpoint_rejects_bad_ip() {
        // A name that cannot resolve is an error (the `.invalid` TLD is
        // reserved by RFC 6761 to never resolve, so this is
        // deterministic regardless of the host's DNS).
        assert!(parse_endpoint("no.such.host.invalid", 5076).is_err());
    }

    /// pvxs `mshim` resolves the endpoint body through
    /// `SockEndpoint`/`SockAddr::setAddress` (mshim.cpp:60), so a DNS
    /// hostname like `localhost` is accepted (resolved IPv4-preferred),
    /// not rejected. `localhost:15076` must parse to an IPv4 loopback
    /// address with the explicit port.
    #[test]
    fn parse_endpoint_resolves_hostname() {
        let ep = parse_endpoint("localhost:15076", 5076).expect("localhost resolves");
        assert!(
            matches!(ep.ip, IpAddr::V4(v4) if v4.is_loopback()),
            "expected IPv4 loopback, got {:?}",
            ep.ip
        );
        assert_eq!(ep.port, 15076);
    }

    #[test]
    fn parse_endpoint_rejects_bad_port() {
        assert!(parse_endpoint("127.0.0.1:notaport", 5076).is_err());
    }

    #[test]
    fn parse_endpoint_rejects_ipv6() {
        // pvxs mshim is IPv4-only (mshim.cpp:56-68).
        assert!(parse_endpoint("::1", 5076).is_err());
        assert!(parse_endpoint("fe80::1", 5076).is_err());
        assert!(parse_endpoint("[::1]:5076", 5076).is_err());
    }

    #[test]
    fn parse_endpoint_rejects_zero_port() {
        // Explicit `:0` endpoint port (mshim.cpp:70-72).
        assert!(parse_endpoint("127.0.0.1:0", 5076).is_err());
        // Zero default port (the `-p 0` fallback) also yields port 0.
        assert!(parse_endpoint("127.0.0.1", 0).is_err());
    }

    #[test]
    fn parse_endpoint_rejects_ttl_out_of_range() {
        assert!(parse_endpoint("224.1.1.1,0", 5076).is_err());
        assert!(parse_endpoint("224.1.1.1,256", 5076).is_err());
    }

    #[test]
    fn parse_endpoint_rejects_bad_ttl() {
        assert!(parse_endpoint("224.1.1.1,abc", 5076).is_err());
    }

    #[test]
    fn parse_endpoint_rejects_empty_iface() {
        assert!(parse_endpoint("224.1.1.1@", 5076).is_err());
    }

    fn fwd(ip: &str, ttl: Option<u32>, iface: Option<Ipv4Addr>) -> ForwardTarget {
        ForwardTarget {
            addr: SocketAddr::new(ip.parse().unwrap(), 5076),
            ttl,
            iface_v4: iface,
        }
    }

    /// a unicast destination needs no multicast setsockopt.
    #[test]
    fn multicast_opts_none_for_unicast() {
        assert_eq!(multicast_opts_for(&fwd("192.168.1.10", None, None)), None);
        // An override on a unicast target is still irrelevant.
        assert_eq!(
            multicast_opts_for(&fwd("192.168.1.10", Some(64), None)),
            None
        );
    }

    /// a multicast destination WITH overrides reports them verbatim.
    #[test]
    fn multicast_opts_uses_override() {
        let iface = Ipv4Addr::new(192, 168, 1, 5);
        assert_eq!(
            multicast_opts_for(&fwd("224.1.1.1", Some(32), Some(iface))),
            Some((32, iface))
        );
    }

    /// Regression: a multicast destination WITHOUT overrides must
    /// still report the platform defaults (TTL 1, INADDR_ANY) so the
    /// shared send socket is reset — it must NOT report `None`, which
    /// would let a prior destination's TTL/IF bleed through.
    #[test]
    fn multicast_opts_resets_to_defaults_without_override() {
        assert_eq!(
            multicast_opts_for(&fwd("224.1.1.1", None, None)),
            Some((1, Ipv4Addr::UNSPECIFIED))
        );
    }

    /// mixed targets — an override target followed by a bare
    /// multicast target — each resolve to independent option sets, so
    /// forwarding to the bare target after the override target resets
    /// the socket instead of inheriting TTL 32 / the override iface.
    #[test]
    fn multicast_opts_mixed_targets_do_not_bleed() {
        let iface = Ipv4Addr::new(10, 0, 0, 1);
        let with_override = fwd("239.0.0.1", Some(32), Some(iface));
        let bare = fwd("239.0.0.2", None, None);

        assert_eq!(
            multicast_opts_for(&with_override),
            Some((32, iface)),
            "override target keeps its settings"
        );
        assert_eq!(
            multicast_opts_for(&bare),
            Some((1, Ipv4Addr::UNSPECIFIED)),
            "bare target resets to defaults, not the prior target's 32/{iface}"
        );
    }

    /// pvxs `mshim` clears the SEARCH `Unicast` flag for any destination
    /// `IfaceMap::is_broadcast` recognizes — a host interface's *subnet*
    /// broadcast (e.g. 192.168.1.255), not only the limited broadcast
    /// 255.255.255.255 (mshim.cpp:141, evhelper.cpp:866). The classifier
    /// must therefore consult the host broadcast set, not merely
    /// `Ipv4Addr::is_broadcast()`.
    #[test]
    fn dest_is_unicast_clears_for_subnet_broadcast() {
        let subnet_bcast = Ipv4Addr::new(192, 168, 1, 255);
        let host: HashSet<Ipv4Addr> = std::iter::once(subnet_bcast).collect();

        // Subnet broadcast known to the host map → NOT unicast (the regression).
        assert!(
            !dest_is_unicast(SocketAddr::new(IpAddr::V4(subnet_bcast), 5076), &host),
            "subnet broadcast must clear Unicast"
        );
        // Limited broadcast 255.255.255.255 → NOT unicast (existing case, kept).
        assert!(!dest_is_unicast(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), 5076),
            &host
        ));
        // Multicast → NOT unicast (existing case).
        assert!(!dest_is_unicast(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(224, 1, 1, 1)), 5076),
            &host
        ));
        // Ordinary unicast not in the host broadcast set → unicast.
        assert!(dest_is_unicast(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)), 5076),
            &host
        ));
        // The SAME subnet-broadcast address with an EMPTY host map (the
        // pre-fix `Ipv4Addr::is_broadcast()` behavior) is mislabeled
        // unicast — proves the host set, not a magic constant, drives it.
        assert!(dest_is_unicast(
            SocketAddr::new(IpAddr::V4(subnet_bcast), 5076),
            &HashSet::new()
        ));
    }

    #[cfg(tokio_backend)]
    /// End-to-end wire.
    ///
    /// A real SEARCH datagram forwarded to a `-F <subnet-broadcast>`
    /// target must be rebuilt with the wire `Unicast` flag (0x80)
    /// cleared, matching pvxs for `ifmap.is_broadcast(dest)`. The flags
    /// byte is the first payload byte after the 8-byte UDP header and
    /// the 4-byte sequence id (offset 12).
    #[test]
    fn rebuilt_search_to_subnet_broadcast_clears_unicast_flag() {
        use epics_pva_rs::codec::PvaCodec;

        const FLAGS_OFFSET: usize = 12;
        const UNICAST_FLAG: u8 = 0x80;

        let codec = PvaCodec::new();
        // A client SEARCH that announced itself unicast (flag set).
        let frame = codec.build_search(1, 7, "MY:PV", [10, 1, 2, 3], 5076, /*unicast=*/ true);
        let msgs = ForwardableDatagram::decode_all(&frame);
        assert_eq!(msgs.len(), 1, "one SEARCH message decoded");

        let subnet_bcast = Ipv4Addr::new(192, 168, 1, 255);
        let host: HashSet<Ipv4Addr> = std::iter::once(subnet_bcast).collect();
        let source: SocketAddr = "10.1.2.3:1111".parse().unwrap();

        // Forwarded to the subnet broadcast → Unicast cleared.
        let bcast_dest = SocketAddr::new(IpAddr::V4(subnet_bcast), 5076);
        let bcast_frame = msgs[0].rebuild_for(dest_is_unicast(bcast_dest, &host), source);
        assert_eq!(
            bcast_frame[FLAGS_OFFSET] & UNICAST_FLAG,
            0,
            "Unicast must be cleared for a SEARCH forwarded to a subnet broadcast"
        );

        // Contrast: an ordinary unicast dest keeps the flag set.
        let uni_dest = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)), 5076);
        let uni_frame = msgs[0].rebuild_for(dest_is_unicast(uni_dest, &host), source);
        assert_eq!(
            uni_frame[FLAGS_OFFSET] & UNICAST_FLAG,
            UNICAST_FLAG,
            "Unicast must be set for a SEARCH forwarded to an ordinary unicast dest"
        );
    }
}
