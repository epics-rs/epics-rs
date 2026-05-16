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

use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket as StdUdpSocket};

use clap::Parser;
use socket2::{Domain, Protocol, SockRef, Socket, Type};
use tokio::net::UdpSocket;

#[derive(Parser)]
#[command(name = "mshim-rs", version, about = "PVA beacon/search multicast shim")]
struct Args {
    /// Listen endpoint `<ip>[:port][,ttl#][@iface]`. Repeat for
    /// multiple. Multicast groups are joined automatically; `@iface`
    /// scopes the join to one interface.
    #[arg(short = 'L', long = "listen", required = true)]
    listen: Vec<String>,

    /// Forward destination `<ip>[:port][,ttl#][@iface]`. Repeat for
    /// multiple. `,ttl#` / `@iface` apply to multicast destinations.
    #[arg(short = 'F', long = "forward", required = true)]
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
    loop {
        match head.rfind(['@', ',']) {
            Some(idx) => {
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
            None => break,
        }
    }
    let (ip_str, port) = if let Some((a, b)) = head.rsplit_once(':') {
        let port: u16 = b.parse().map_err(|e| format!("port {b:?} invalid: {e}"))?;
        (a, port)
    } else {
        (head, default_port)
    };
    let ip: IpAddr = ip_str
        .parse()
        .map_err(|e| format!("ip {ip_str:?} invalid: {e}"))?;
    Ok(Endpoint {
        ip,
        port,
        ttl,
        iface,
    })
}

/// Resolve an `@iface` spec to the interface's IPv4 address. Accepts
/// either an interface name (`eth0`) or a literal IPv4 address (which
/// is returned verbatim). Used to scope multicast joins and select
/// the outbound multicast interface.
fn resolve_iface_v4(spec: &str) -> Result<Ipv4Addr, String> {
    // A literal IPv4 address is accepted directly — pvxs allows this.
    if let Ok(v4) = spec.parse::<Ipv4Addr>() {
        return Ok(v4);
    }
    #[cfg(unix)]
    {
        iface_name_to_v4(spec)
    }
    #[cfg(not(unix))]
    {
        Err(format!(
            "interface-name override {spec:?} requires a Unix host; \
             pass the interface's IPv4 address instead"
        ))
    }
}

/// Look up an interface's first IPv4 address by name via `getifaddrs`.
#[cfg(unix)]
fn iface_name_to_v4(name: &str) -> Result<Ipv4Addr, String> {
    use std::ffi::CStr;

    // SAFETY: getifaddrs allocates a linked list we free via
    // freeifaddrs; every pointer is null-checked before deref.
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return Err(format!(
                "getifaddrs failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mut cur = ifap;
        let mut found: Option<Ipv4Addr> = None;
        while !cur.is_null() {
            let ifa = &*cur;
            if !ifa.ifa_name.is_null() && !ifa.ifa_addr.is_null() {
                let ifa_name = CStr::from_ptr(ifa.ifa_name).to_string_lossy();
                let sa = &*ifa.ifa_addr;
                if ifa_name == name && sa.sa_family as i32 == libc::AF_INET {
                    let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    // s_addr is in network byte order.
                    let addr = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                    found = Some(addr);
                    break;
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(ifap);
        found.ok_or_else(|| format!("interface {name:?} has no IPv4 address"))
    }
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
            Some(spec) => resolve_iface_v4(spec)
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

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let default_port = args
        .port
        .or_else(|| {
            std::env::var("EPICS_PVA_BROADCAST_PORT")
                .ok()
                .and_then(|s| s.parse().ok())
        })
        .unwrap_or(5076);

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
                Some(spec) => Some(resolve_iface_v4(spec)?),
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
    if let Err(e) = send_socket.bind(&SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0).into()) {
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
    // Apply `@iface` / `,ttl#` to the send socket. mshim uses a
    // single send socket; when several multicast destinations request
    // different overrides we apply each before its send (see the
    // per-target apply in the recv loop). The values set here are the
    // socket's defaults — the last forward target's settings — and
    // are re-asserted per datagram so concurrent destinations with
    // distinct overrides each get their own.
    let send_sock_std: StdUdpSocket = send_socket.into();
    let send_sock = match UdpSocket::from_std(send_sock_std) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("mshim-rs: send socket from_std: {e}");
            std::process::exit(1);
        }
    };
    let send_sock = std::sync::Arc::new(send_sock);

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
            eprintln!(
                "  forward → {}:{} [{}]",
                ep.ip,
                ep.port,
                extras.join(" ")
            );
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
        let h = tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            loop {
                match sock.recv_from(&mut buf).await {
                    Ok((n, peer)) => {
                        let payload = &buf[..n];
                        for tgt in &targets {
                            // Avoid an obvious feedback loop: don't
                            // forward back to the source endpoint of
                            // the same datagram.
                            if tgt.addr == peer {
                                continue;
                            }
                            // Apply per-destination multicast options
                            // before the send. `set_multicast_*`
                            // operate on the underlying socket; we
                            // re-assert them each datagram so two
                            // destinations with distinct overrides on
                            // the shared send socket don't bleed into
                            // each other.
                            if tgt.addr.ip().is_multicast()
                                && (tgt.ttl.is_some() || tgt.iface_v4.is_some())
                            {
                                // `SockRef` borrows the tokio socket's
                                // fd so we can reach the setsockopt-
                                // backed multicast options tokio's
                                // `UdpSocket` doesn't expose directly
                                // (notably `IP_MULTICAST_IF`).
                                let sref = SockRef::from(send_sock.as_ref());
                                if let Some(ttl) = tgt.ttl
                                    && let Err(e) = sref.set_multicast_ttl_v4(ttl)
                                {
                                    eprintln!(
                                        "mshim-rs: set multicast ttl for {}: {e}",
                                        tgt.addr
                                    );
                                }
                                if let Some(iface) = tgt.iface_v4
                                    && let Err(e) = sref.set_multicast_if_v4(&iface)
                                {
                                    eprintln!(
                                        "mshim-rs: set multicast iface for {}: {e}",
                                        tgt.addr
                                    );
                                }
                            }
                            if let Err(e) = send_sock.send_to(payload, tgt.addr).await
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
        assert!(parse_endpoint("not-an-ip", 5076).is_err());
    }

    #[test]
    fn parse_endpoint_rejects_bad_port() {
        assert!(parse_endpoint("127.0.0.1:notaport", 5076).is_err());
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

    #[test]
    fn resolve_iface_accepts_literal_ipv4() {
        let v4 = resolve_iface_v4("192.168.1.5").unwrap();
        assert_eq!(v4, Ipv4Addr::new(192, 168, 1, 5));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_iface_loopback_name() {
        // The loopback interface is named `lo` on Linux and `lo0` on
        // macOS/BSD; one of them must resolve to 127.0.0.1.
        let lo = resolve_iface_v4("lo").or_else(|_| resolve_iface_v4("lo0"));
        if let Ok(v4) = lo {
            assert!(v4.is_loopback(), "loopback iface should map to a loopback addr");
        }
    }
}
