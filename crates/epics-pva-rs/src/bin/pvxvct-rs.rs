//! `pvxvct-rs` — PV Access Virtual Cable Tester. Mirrors pvxs
//! `tools/pvxvct.cpp`.
//!
//! Listens on the UDP broadcast port for SEARCH (client → server) and
//! BEACON (server → client) frames, decodes the headers + key
//! metadata, and prints to stdout. Useful for diagnosing network
//! configuration issues — replicates `pvxvct` at the operationally
//! relevant level (decoded frames, no raw hex dump).
//!
//! ```text
//! pvxvct-rs                       # listen for both SEARCH and BEACON
//! pvxvct-rs -C                    # only SEARCH
//! pvxvct-rs -S                    # only BEACON
//! pvxvct-rs -H 10.0.0.0/24        # filter by source subnet (repeatable)
//! pvxvct-rs -P somePv             # filter SEARCH by PV name (repeatable)
//! pvxvct-rs -B 192.168.1.5:5076   # bind a specific interface (repeatable)
//! pvxvct-rs -B 224.0.0.1@10.0.0.2 # join a multicast group on an iface
//! ```

// The sniffer half of this tool is `tokio_backend`-only, because
// `client_native::udp` is: its collector waits on `UdpSocket::readable` inside
// a future started through `runtime::task`, and `exec_backend` — selected on a
// host build by `EPICS_RS_BUILD_EXEC_BACKEND=thread` — starts it on a worker
// with no reactor. Cargo's `required-features` cannot name a build-script cfg,
// so the gate is here instead of in `Cargo.toml`. Everything above the socket
// — the `-H`/`-B`/`-P` grammars and their tests — is backend-neutral and stays
// compiled, so this file is linted and tested in both configurations.
#[cfg(tokio_backend)]
use std::io::Cursor;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
#[cfg(tokio_backend)]
use std::time::SystemTime;

use clap::Parser;
#[cfg(tokio_backend)]
use tokio::sync::mpsc::Receiver;

#[cfg(tokio_backend)]
use epics_pva_rs::client_native::udp::{CollectedDatagram, UdpManager};
use epics_pva_rs::config::Endpoint;
#[cfg(tokio_backend)]
use epics_pva_rs::decode::try_parse_frame;
#[cfg(tokio_backend)]
use epics_pva_rs::proto::{Command, ReadExt, decode_size_nonnull, decode_string, ip_from_bytes};

#[derive(Parser)]
#[command(
    name = "pvxvct-rs",
    about = "PVA Virtual Cable Tester",
    disable_version_flag = true
)]
struct Args {
    /// Print version information (pvxs `version_information`, tools
    /// `case 'V'`) and exit. Routed through `cli::version_information`
    /// so every PVA CLI reports the same library + protocol stack
    /// instead of clap's crate-only `<binary> <version>`. Distinct
    /// from `-v` (raise the sniffer logger to Debug).
    #[arg(short = 'V', long = "version")]
    version: bool,

    /// Show only client SEARCH packets.
    #[arg(short = 'C', conflicts_with = "server_only")]
    client_only: bool,

    /// Show only server BEACON packets.
    #[arg(short = 'S', conflicts_with = "client_only")]
    server_only: bool,

    /// Filter by source host. Accepts a hostname/IP, optionally with a
    /// CIDR prefix (`10.0.0.0/24`) or dotted netmask
    /// (`10.0.0.0/255.255.255.0`). Repeatable; entries OR-combined.
    /// pvxs `tools/pvxvct.cpp:36-78` (`parsePeer`) parses exactly these
    /// forms and matches `(peer & mask) == addr`.
    #[arg(short = 'H', long = "host")]
    hosts: Vec<String>,

    /// Filter SEARCH frames by PV name. Repeatable; a frame is shown if any
    /// of its names matches any `-P` value. pvxs `pvxvct` parity (commit
    /// bb53bb8 "fix pvxvct: actually apply PV name and host/network filters").
    #[arg(short = 'P', long = "pv")]
    pvnames: Vec<String>,

    /// Listen on the given interface(s). Repeatable; one listener per
    /// bind. Form `host[:port]` for a unicast/wildcard interface, or
    /// `mcast[,ttl][@iface][:port]` to join a multicast group. pvxs
    /// `tools/pvxvct.cpp:152-153,171-173,235-241` accepts repeated `-B`
    /// and binds `0.0.0.0:5076` only when none is given. The socket is
    /// always the shared wildcard collector for the port (pvxs `UDPManager`,
    /// `src/udp_collector.cpp:140-151`); the destination address selects
    /// which datagrams this listener is shown — it is never bound directly.
    #[arg(short = 'B', long = "bind")]
    binds: Vec<String>,

    /// Verbose: log bind endpoints and active filters to stderr. pvxs
    /// `tools/pvxvct.cpp:144-145,172` raises the `pvxvct` logger to
    /// Debug under `-v`; the decoded SEARCH/BEACON lines print
    /// regardless.
    #[arg(short = 'v')]
    verbose: bool,

    /// UDP port to bind (Rust-only explicit override). pvxs `pvxvct`
    /// has no `-p` and hard-codes 5076; this defaults to that same
    /// literal when unset and is only used as the default for the
    /// implicit wildcard bind and `-B` endpoints that omit `:port`.
    /// `EPICS_PVA_BROADCAST_PORT` is deliberately not consulted here, so
    /// an unrelated PVA env setting cannot move the cable tester off 5076.
    #[arg(short = 'p', long = "port")]
    port: Option<u16>,
}

/// A parsed `-H` source filter: an address and netmask, both as
/// host-order `u32`, with `addr` already masked. Matching mirrors pvxs
/// `opts.allowPeer` (`tools/pvxvct.cpp:115-126`): `(peer & mask) == addr`,
/// and any non-IPv4 peer is rejected when a filter is present.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PeerFilter {
    addr: u32,
    mask: u32,
}

/// A parsed `-B` bind endpoint. `iface` is the multicast join
/// interface (`UNSPECIFIED` lets the kernel choose); it is ignored for
/// unicast/wildcard binds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BindEndpoint {
    addr: Ipv4Addr,
    port: u16,
    iface: Ipv4Addr,
}

/// The frame-display filters shared by every listener task.
struct Filters {
    client_only: bool,
    server_only: bool,
    peers: Vec<PeerFilter>,
    pvnames: Vec<String>,
}

// `allow_peer` and `pv_filter_allows` are the display filters `run_listener`
// applies to each decoded frame, so they carry its gate rather than a second
// copy of the reasoning. Their unit tests below carry it for the same reason.
#[cfg(tokio_backend)]
impl Filters {
    /// pvxs `allowPeer` (`tools/pvxvct.cpp:115-126`): no filters → allow
    /// all; otherwise the peer must be IPv4 and match one stored
    /// `(addr, mask)` pair. A non-IPv4 peer is dropped when any filter
    /// is set (pvxs `peer.family()!=AF_INET → return false`).
    fn allow_peer(&self, ip: IpAddr) -> bool {
        if self.peers.is_empty() {
            return true;
        }
        match ip {
            IpAddr::V4(v4) => {
                let p = u32::from(v4);
                self.peers.iter().any(|f| (p & f.mask) == f.addr)
            }
            IpAddr::V6(_) => false,
        }
    }
}

/// pvxs `searchCB` PV-name gate (`tools/pvxvct.cpp:201-214`): with no
/// `-P` filter, show every SEARCH; with a filter, show only when some
/// frame name matches. A zero-name discovery SEARCH therefore yields
/// `false` (no name can match) and is hidden — it does NOT bypass the
/// filter. This is the structural rule, not a special case for the
/// empty-name frame.
#[cfg(tokio_backend)]
fn pv_filter_allows(pvnames: &[String], names: &[String]) -> bool {
    if pvnames.is_empty() {
        return true;
    }
    names.iter().any(|n| pvnames.iter().any(|p| p == n))
}

/// The default UDP port for binds: used for the implicit no-`-B`
/// wildcard listener and for any `-B` endpoint that omits `:port`.
///
/// pvxs `pvxvct` hard-codes the well-known PVA UDP port 5076 for both:
/// `-B optarg` builds `SockEndpoint(optarg, 5076)` and the implicit bind
/// is `SockAddr::any(AF_INET, 5076)` (`tools/pvxvct.cpp:155,172`). It has
/// no `-p` option and never reads `EPICS_PVA_BROADCAST_PORT`. Consulting
/// that variable would let an unrelated PVA env setting silently move the
/// cable tester off 5076, so a diagnostic command copied between the C
/// and Rust tools could miss the same SEARCH/BEACON traffic. The default
/// is therefore the literal 5076; the Rust-only `-p` (`port_arg`) is the
/// only override, and it is always explicit, never env-driven.
fn default_bind_port(port_arg: Option<u16>) -> u16 {
    port_arg.unwrap_or(5076)
}

/// Parse a `-H` value (`host[/bits | /dotted.mask]`) into a
/// [`PeerFilter`]. Mirrors pvxs `parsePeer` (`tools/pvxvct.cpp:36-78`):
/// no mask → exact host match (`INADDR_BROADCAST`); `/N` → high-N-bits
/// mask; `/a.b.c.d` → dotted mask. The address is masked at parse time
/// so `1.2.3.4/24` becomes `1.2.3.0/24`.
fn parse_peer(spec: &str) -> Result<PeerFilter, String> {
    let (host, mask_spec) = match spec.split_once('/') {
        Some((h, m)) => (h, Some(m)),
        None => (spec, None),
    };
    let addr = epics_pva_rs::cli::resolve_host_ipv4(host)?;
    let mask: u32 = match mask_spec {
        // pvxs default: INADDR_BROADCAST (all ones) → exact match.
        None => 0xffff_ffff,
        Some(m) if m.contains('.') => {
            let mv4: Ipv4Addr = m
                .parse()
                .map_err(|_| format!("invalid netmask {m:?} in -H {spec:?}"))?;
            u32::from(mv4)
        }
        Some(m) => {
            let nbit: u32 = m
                .parse()
                .map_err(|_| format!("invalid prefix length {m:?} in -H {spec:?}"))?;
            if nbit > 32 {
                return Err(format!("prefix length out of range in -H {spec:?}: {nbit}"));
            }
            // `0xffffffff << 32` panics in Rust (C leaves it UB); pvxs
            // computes the same value, so special-case /0 → match all.
            if nbit == 0 {
                0
            } else {
                0xffff_ffffu32 << (32 - nbit)
            }
        }
    };
    Ok(PeerFilter {
        addr: u32::from(addr) & mask,
        mask,
    })
}

/// Parse a `-B` value into a [`BindEndpoint`]. Grammar mirrors pvxs
/// `SockEndpoint` (`tools/pvxvct.cpp:152-153`): `addr[,ttl][@iface][:port]`.
/// The `ttl` is accepted for grammar compatibility but does not affect a
/// pure listener, so it is validated and discarded. The endpoint body and
/// the multicast `@iface` are resolved through the shared PVA-tool
/// resolvers: the body via DNS (IPv4-preferred), and `@iface` accepting
/// **either** an interface IPv4 address **or** an OS interface name
/// (`en0`, `lo0`) — the dual form pvxs's `SockEndpoint` ctor accepts
/// (`config.cpp:76-80`), so `-B 224.0.1.1@en0` works as it does under
/// pvxs instead of being misresolved as a DNS host.
fn parse_bind(spec: &str, default_port: u16) -> Result<BindEndpoint, String> {
    let mut rest = spec;
    let mut port = default_port;
    // `:port` — only consume the trailing colon segment if it parses as
    // a port, so a bare address is left intact.
    if let Some((head, tail)) = rest.rsplit_once(':')
        && let Ok(p) = tail.parse::<u16>()
    {
        port = p;
        rest = head;
    }
    let mut iface = Ipv4Addr::UNSPECIFIED;
    if let Some((head, tail)) = rest.rsplit_once('@') {
        iface = epics_pva_rs::cli::resolve_iface_ipv4(tail)?;
        rest = head;
    }
    if let Some((head, tail)) = rest.rsplit_once(',') {
        // Validate the TTL even though a listener does not use it.
        tail.parse::<u32>()
            .map_err(|_| format!("invalid ttl {tail:?} in -B {spec:?}"))?;
        rest = head;
    }
    let addr = epics_pva_rs::cli::resolve_host_ipv4(rest)?;
    Ok(BindEndpoint { addr, port, iface })
}

impl BindEndpoint {
    /// The collector destination this `-B` requests. pvxvct never binds the
    /// destination address itself: it hands the address to the shared
    /// wildcard collector (`UdpManager`), which binds `0.0.0.0:port` and
    /// fans datagrams out by their recovered original destination. This is
    /// pvxs's rule — "Always bind to wildcard to receive all
    /// uni/broad/multicast" (`src/udp_collector.cpp:140-151`) — so a
    /// broadcast or specific-interface `-B` is received portably instead of
    /// failing to bind a broadcast address (not portable) or silently
    /// missing broadcast traffic on a unicast bind. A multicast endpoint
    /// passes its `@iface` through (as a literal IPv4, which the collector's
    /// `resolve_iface_v4` round-trips) for the group join; a unicast or
    /// broadcast endpoint carries none — the wildcard bind already receives it.
    fn to_endpoint(self) -> Endpoint {
        Endpoint {
            addr: SocketAddr::new(IpAddr::V4(self.addr), self.port),
            ttl: None,
            iface: if self.iface.is_unspecified() {
                None
            } else {
                Some(self.iface.to_string())
            },
        }
    }
}

#[cfg(tokio_backend)]
fn now_iso() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let frac_us = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_micros())
        .unwrap_or(0);
    // Cheap formatter: "Tssss.uuuuuu" — fine for a debug tool.
    format!("T{secs}.{frac_us:06}")
}

#[cfg(tokio_backend)]
fn fmt_guid(g: &[u8]) -> String {
    let mut s = String::with_capacity(24);
    for b in g {
        use std::fmt::Write;
        write!(&mut s, "{b:02X}").unwrap();
    }
    s
}

/// Decode and print SEARCH/BEACON frames the wildcard collector delivers
/// for one `-B` destination, applying the shared display filters. One of
/// these runs per `-B` endpoint, fed by the collector's per-destination
/// fan-out (pvxs starts one search/beacon listener pair per bind,
/// `tools/pvxvct.cpp:235-241`; the collector routes each datagram by its
/// original destination, `src/udp_collector.cpp:451`).
#[cfg(tokio_backend)]
async fn run_listener(mut rx: Receiver<CollectedDatagram>, filters: Arc<Filters>) {
    while let Some(datagram) = rx.recv().await {
        let peer = datagram.src;
        if !filters.allow_peer(peer.ip()) {
            continue;
        }

        let bytes = datagram.data.as_slice();
        let Ok(Some((frame, _consumed))) = try_parse_frame(bytes) else {
            continue;
        };
        let cmd = Command::from_code(frame.header.command);
        let order = frame.header.flags.byte_order();

        match cmd {
            Some(Command::Beacon) if !filters.client_only => {
                // Re-decode the beacon body to surface the advertised
                // server address + GUID + proto string.
                let mut cur = Cursor::new(frame.payload.as_slice());
                let guid = cur.get_bytes(12).unwrap_or_default();
                let _flags = cur.get_u8().unwrap_or(0);
                let _seq = cur.get_u8().unwrap_or(0);
                let _change = cur.get_u16(order).unwrap_or(0);
                let addr = cur.get_bytes(16).unwrap_or_default();
                let server_port = cur.get_u16(order).unwrap_or(0);
                let proto = decode_string(&mut cur, order)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "tcp".into());

                let mut addr_arr = [0u8; 16];
                addr_arr[..addr.len().min(16)].copy_from_slice(&addr[..addr.len().min(16)]);
                let server_ip =
                    ip_from_bytes(&addr_arr).unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
                let server_disp = if server_ip.is_unspecified() {
                    peer.ip()
                } else {
                    server_ip
                };
                println!(
                    "{} BEACON   peer={peer:21} server={server_disp}:{server_port} proto={proto} guid={}",
                    now_iso(),
                    fmt_guid(&guid)
                );
            }
            Some(Command::Search) if !filters.server_only => {
                // Header + payload-tail decode: SEARCH carries
                // (seq:u32, flags:u8, reserved:u24, response_addr:16,
                // response_port:u16, n_protocols:u8, ...). For
                // operational debug we just surface seq + reply
                // address + first PV name.
                let mut cur = Cursor::new(frame.payload.as_slice());
                let seq = cur.get_u32(order).unwrap_or(0);
                let _flags = cur.get_u8().unwrap_or(0);
                let _ = cur.get_bytes(3); // reserved
                let resp_addr = cur.get_bytes(16).unwrap_or_default();
                let resp_port = cur.get_u16(order).unwrap_or(0);
                let mut addr_arr = [0u8; 16];
                addr_arr[..resp_addr.len().min(16)]
                    .copy_from_slice(&resp_addr[..resp_addr.len().min(16)]);
                let resp_ip = ip_from_bytes(&addr_arr).unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
                // pvxs decodes the protocol count as a non-null Size
                // (`allow_null=false`) and faults the whole SEARCH on `0xFF`.
                // A null count here means the rest of the frame can't be
                // parsed, so skip displaying this malformed frame rather
                // than silently treating it as zero protocols.
                let n_protos = match decode_size_nonnull(&mut cur, order, "search protocol count") {
                    Ok(n) => n,
                    Err(_) => continue,
                };
                for _ in 0..n_protos {
                    let _ = decode_string(&mut cur, order);
                }
                let n_search = cur.get_u16(order).unwrap_or(0);
                let mut names = Vec::new();
                for _ in 0..n_search {
                    let _cid = cur.get_u32(order).unwrap_or(0);
                    if let Ok(Some(name)) = decode_string(&mut cur, order) {
                        names.push(name);
                    }
                }
                // `-P` gate (pvxs `searchCB`): a zero-name discovery
                // SEARCH is hidden when a filter is set, since no name
                // can match — see `pv_filter_allows`.
                if !pv_filter_allows(&filters.pvnames, &names) {
                    continue;
                }
                println!(
                    "{} SEARCH   peer={peer:21} seq={seq} reply={resp_ip}:{resp_port} pvs={names:?}",
                    now_iso()
                );
            }
            _ => {
                if !filters.client_only && !filters.server_only {
                    println!(
                        "{} OTHER    peer={peer:21} cmd_code={}",
                        now_iso(),
                        frame.header.command
                    );
                }
            }
        }
    }
}

#[tokio::main]
async fn main() {
    // pvxs's pvxvct returns 1 from its bad-option arm (`tools/pvxvct.cpp`);
    // `Args::parse()` would exit with clap's 2.
    let args: Args =
        epics_pva_rs::cli::parse_or_exit_styled(epics_pva_rs::cli::UsageErrorStyle::Pvxs);

    // pvxs `-V` prints version_information and exits before binding any
    // sniffer socket (pvxvct `case 'V'`).
    if args.version {
        print!("{}", epics_pva_rs::cli::version_information());
        return;
    }

    let default_port = default_bind_port(args.port);

    let peers = match args
        .hosts
        .iter()
        .map(|h| parse_peer(h))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("pvxvct-rs: {e}");
            std::process::exit(1);
        }
    };

    // pvxs binds `0.0.0.0:<port>` only when no `-B` is given
    // (`tools/pvxvct.cpp:171-173`).
    let bind_specs: Vec<String> = if args.binds.is_empty() {
        vec!["0.0.0.0".to_string()]
    } else {
        args.binds.clone()
    };
    let endpoints = match bind_specs
        .iter()
        .map(|b| parse_bind(b, default_port))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(e) => e,
        Err(e) => {
            eprintln!("pvxvct-rs: {e}");
            std::process::exit(1);
        }
    };

    let filters = Arc::new(Filters {
        client_only: args.client_only,
        server_only: args.server_only,
        peers,
        pvnames: args.pvnames,
    });

    if args.verbose {
        for f in &filters.peers {
            eprintln!(
                "pvxvct-rs: peer filter {}/{}",
                Ipv4Addr::from(f.addr),
                Ipv4Addr::from(f.mask)
            );
        }
        for p in &filters.pvnames {
            eprintln!("pvxvct-rs: pv filter {p:?}");
        }
    }

    #[cfg(exec_backend)]
    refuse_without_a_reactor(endpoints, filters);

    #[cfg(tokio_backend)]
    sniff(endpoints, filters).await;
}

/// The `exec_backend` arm: everything up to here is backend-neutral, and this
/// is the first step that is not.
///
/// It refuses rather than proceeding because the failure would otherwise be a
/// panic from inside a background worker: `UdpManager::collect` starts its
/// receive loop through `runtime::task`, which on this backend is a
/// callback-pool thread with no tokio reactor entered, and the first
/// `UdpSocket::readable` there aborts the task with *"there is no reactor
/// running"*. Same shape as `realtime-ca-ioc`'s hosted arm: the binary is
/// still built and linted in this configuration, and says why it will not run.
#[cfg(exec_backend)]
fn refuse_without_a_reactor(endpoints: Vec<BindEndpoint>, filters: Arc<Filters>) -> ! {
    // A dry run of everything that did resolve, so an operator who reached
    // this arm by accident still learns whether their `-B`/`-H`/`-P` spellings
    // parsed the way they meant. Only the socket is missing.
    for ep in endpoints {
        let dest = ep.to_endpoint();
        match dest.iface {
            Some(iface) => eprintln!("pvxvct-rs: would collect {} on {iface}", dest.addr),
            None => eprintln!("pvxvct-rs: would collect {}", dest.addr),
        }
    }
    let shown = match (filters.client_only, filters.server_only) {
        (true, _) => "SEARCH only",
        (_, true) => "BEACON only",
        _ => "SEARCH and BEACON",
    };
    eprintln!(
        "pvxvct-rs: would show {shown}, {} peer filter(s), {} PV filter(s)",
        filters.peers.len(),
        filters.pvnames.len()
    );
    eprintln!(
        "pvxvct-rs: this build selects the reactor-free execution backend \
         (EPICS_RS_BUILD_EXEC_BACKEND=thread); the UDP collector needs a tokio reactor, so \
         there is nothing to sniff with. Rebuild without that feature."
    );
    std::process::exit(1);
}

/// The `tokio_backend` arm: bind one collector per `-B` endpoint and print
/// what arrives until every listener ends.
#[cfg(tokio_backend)]
async fn sniff(endpoints: Vec<BindEndpoint>, filters: Arc<Filters>) {
    // One shared wildcard collector per (family, port): it binds
    // `0.0.0.0:port` and fans each datagram out to the listeners whose `-B`
    // destination matches its recovered original destination — pvxs
    // `UDPManager` (`src/udp_collector.cpp:102-151`). The collector handles
    // must outlive their listeners: the background receive task runs only
    // while a handle (or another listener) keeps the collector alive.
    let reactor = epics_base_rs::runtime::task::Reactor::current()
        .expect("the collector loop is armed on the tool's runtime");
    let manager = UdpManager::new();
    let mut collectors = Vec::with_capacity(endpoints.len());
    let mut handles = Vec::with_capacity(endpoints.len());
    for ep in endpoints {
        let dest = ep.to_endpoint();
        let collector = match manager.collect(&reactor, &dest) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("pvxvct-rs: bind {}:{}: {e}", ep.addr, ep.port);
                std::process::exit(1);
            }
        };
        let rx = match collector.add_listener(&dest) {
            Ok(rx) => rx,
            Err(e) => {
                eprintln!("pvxvct-rs: listen {}:{}: {e}", ep.addr, ep.port);
                std::process::exit(1);
            }
        };
        if ep.addr.is_multicast() {
            eprintln!(
                "pvxvct-rs: listening on {}:{} (multicast, iface {})",
                ep.addr, ep.port, ep.iface
            );
        } else {
            eprintln!("pvxvct-rs: listening on {}:{}", ep.addr, ep.port);
        }
        collectors.push(collector);
        let f = filters.clone();
        handles.push(tokio::spawn(run_listener(rx, f)));
    }

    for h in handles {
        let _ = h.await;
    }
    // Keep the collectors alive until every listener task has ended.
    drop(collectors);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `-H` with no mask is an exact-host match (pvxs default
    /// `INADDR_BROADCAST`).
    #[test]
    fn parse_peer_exact_host() {
        let f = parse_peer("192.168.1.5").unwrap();
        assert_eq!(f.mask, 0xffff_ffff);
        assert_eq!(f.addr, u32::from(Ipv4Addr::new(192, 168, 1, 5)));
    }

    /// `/N` masks the high N bits and the address is masked at parse
    /// time: `1.2.3.4/24` == `1.2.3.0/24`.
    #[test]
    fn parse_peer_cidr_masks_address() {
        let f = parse_peer("1.2.3.4/24").unwrap();
        assert_eq!(f.mask, 0xffff_ff00);
        assert_eq!(f.addr, u32::from(Ipv4Addr::new(1, 2, 3, 0)));
    }

    /// A dotted netmask is equivalent to the matching prefix length.
    #[test]
    fn parse_peer_dotted_netmask() {
        let cidr = parse_peer("10.0.0.0/16").unwrap();
        let dotted = parse_peer("10.0.0.0/255.255.0.0").unwrap();
        assert_eq!(cidr, dotted);
    }

    /// `/0` matches everything and must not panic on the `<< 32` edge.
    #[test]
    fn parse_peer_zero_prefix_matches_all() {
        let f = parse_peer("0.0.0.0/0").unwrap();
        assert_eq!(f.mask, 0);
        assert_eq!(f.addr, 0);
    }

    #[test]
    fn parse_peer_rejects_bad_prefix() {
        assert!(parse_peer("10.0.0.0/33").is_err());
        assert!(parse_peer("10.0.0.0/abc").is_err());
    }

    /// `allow_peer` mirrors pvxs masked matching, and drops non-IPv4
    /// peers only when a filter is present.
    #[cfg(tokio_backend)]
    #[test]
    fn allow_peer_subnet_match() {
        let filters = Filters {
            client_only: false,
            server_only: false,
            peers: vec![parse_peer("10.0.0.0/24").unwrap()],
            pvnames: vec![],
        };
        assert!(filters.allow_peer("10.0.0.42".parse().unwrap()));
        assert!(!filters.allow_peer("10.0.1.42".parse().unwrap()));
        // IPv6 peer is rejected when an IPv4 filter is set.
        assert!(!filters.allow_peer("::1".parse().unwrap()));

        // No filter → allow everything, including IPv6.
        let open = Filters {
            client_only: false,
            server_only: false,
            peers: vec![],
            pvnames: vec![],
        };
        assert!(open.allow_peer("10.0.1.42".parse().unwrap()));
        assert!(open.allow_peer("::1".parse().unwrap()));
    }

    /// The `-P` gate hides zero-name discovery SEARCH frames when a
    /// filter is active — the core finding-19 correctness fix.
    #[cfg(tokio_backend)]
    #[test]
    fn pv_filter_hides_zero_name_discovery_when_set() {
        // No filter → show everything, including the discovery frame.
        assert!(pv_filter_allows(&[], &[]));
        assert!(pv_filter_allows(&[], &["any".to_string()]));

        let filter = vec!["wanted".to_string()];
        // Matching name shows.
        assert!(pv_filter_allows(&filter, &["wanted".to_string()]));
        assert!(pv_filter_allows(
            &filter,
            &["other".to_string(), "wanted".to_string()]
        ));
        // Non-matching name hidden.
        assert!(!pv_filter_allows(&filter, &["other".to_string()]));
        // Zero-name discovery SEARCH hidden (was wrongly shown before).
        assert!(!pv_filter_allows(&filter, &[]));
    }

    #[test]
    fn parse_bind_unicast_default_port() {
        let ep = parse_bind("192.168.1.5", 5076).unwrap();
        assert_eq!(ep.addr, Ipv4Addr::new(192, 168, 1, 5));
        assert_eq!(ep.port, 5076);
        assert_eq!(ep.iface, Ipv4Addr::UNSPECIFIED);
    }

    #[test]
    fn parse_bind_explicit_port() {
        let ep = parse_bind("0.0.0.0:5077", 5076).unwrap();
        assert_eq!(ep.addr, Ipv4Addr::UNSPECIFIED);
        assert_eq!(ep.port, 5077);
    }

    /// Multicast form `mcast,ttl@iface:port` parses all segments; the
    /// ttl is validated and discarded (a listener ignores it).
    #[test]
    fn parse_bind_multicast_full_form() {
        let ep = parse_bind("224.0.1.1,5@10.0.0.2:5078", 5076).unwrap();
        assert_eq!(ep.addr, Ipv4Addr::new(224, 0, 1, 1));
        assert!(ep.addr.is_multicast());
        assert_eq!(ep.iface, Ipv4Addr::new(10, 0, 0, 2));
        assert_eq!(ep.port, 5078);
    }

    #[test]
    fn parse_bind_rejects_bad_ttl() {
        assert!(parse_bind("224.0.1.1,abc", 5076).is_err());
    }

    /// The multicast `@iface` suffix accepts an OS interface *name*, not
    /// just an interface IPv4 address — the dual form pvxs's
    /// `SockEndpoint` ctor accepts (`config.cpp:76-80`). The loopback
    /// interface is `lo` on Linux and `lo0` on macOS/BSD; binding
    /// `224.0.1.1@<loopback>` must resolve the name to that interface's
    /// loopback IPv4 address.
    #[cfg(unix)]
    #[test]
    fn parse_bind_iface_accepts_interface_name() {
        let ep = parse_bind("224.0.1.1@lo0", 5076).or_else(|_| parse_bind("224.0.1.1@lo", 5076));
        if let Ok(ep) = ep {
            assert_eq!(ep.addr, Ipv4Addr::new(224, 0, 1, 1));
            assert!(
                ep.iface.is_loopback(),
                "interface name should resolve to the loopback IPv4, got {}",
                ep.iface
            );
        }
    }

    /// The bind default is the literal 5076 (pvxs `pvxvct` hard-codes it,
    /// `tools/pvxvct.cpp:155,172`), and `EPICS_PVA_BROADCAST_PORT` must
    /// NOT change it — pvxvct has no env-driven default. `-p` (the
    /// `Some(..)` arm) is the only override. Serialised on `epics_env`
    /// because the variable is process-global.
    #[test]
    #[serial_test::serial(epics_env)]
    fn default_bind_port_ignores_broadcast_port_env() {
        // SAFETY: std::env mutation is unsafe in edition 2024; the
        // `epics_env` serial guard makes it race-free.
        unsafe { std::env::remove_var("EPICS_PVA_BROADCAST_PORT") };
        // Unset env, no `-p` → 5076.
        assert_eq!(default_bind_port(None), 5076);

        // EPICS_PVA_BROADCAST_PORT=0 must still yield 5076 (pvxs listens
        // on 5076 regardless; the old code bound port 0).
        unsafe { std::env::set_var("EPICS_PVA_BROADCAST_PORT", "0") };
        assert_eq!(default_bind_port(None), 5076);

        // EPICS_PVA_BROADCAST_PORT=5099 must NOT move the default off
        // 5076 — this is the regression the fix closes.
        unsafe { std::env::set_var("EPICS_PVA_BROADCAST_PORT", "5099") };
        assert_eq!(default_bind_port(None), 5076);

        // `-p 5099` is the only thing that overrides, and it does so even
        // with the env set.
        assert_eq!(default_bind_port(Some(5099)), 5099);

        unsafe { std::env::remove_var("EPICS_PVA_BROADCAST_PORT") };
    }

    /// With the env-free default, `-B 0.0.0.0` (no `:port`) binds 5076
    /// while `-B 0.0.0.0:5099` still honours its explicit per-endpoint
    /// port — the two pvxs-parity boundaries the finding calls out.
    #[test]
    fn parse_bind_uses_env_free_default_but_honours_explicit_port() {
        let dflt = default_bind_port(None);
        assert_eq!(parse_bind("0.0.0.0", dflt).unwrap().port, 5076);
        assert_eq!(parse_bind("0.0.0.0:5099", dflt).unwrap().port, 5099);
    }

    /// Each `-B` is handed to the shared wildcard collector as a
    /// *destination*, never bound directly: `to_endpoint` preserves the
    /// address + port and passes a multicast `@iface` through (as a literal
    /// IPv4 the collector round-trips), while a unicast or broadcast
    /// endpoint carries no interface — the wildcard bind already receives it.
    /// This is the structural change behind the pvxs `UDPManager` parity
    /// (`src/udp_collector.cpp:140-151`): a broadcast `-B` no longer fails to
    /// bind its broadcast address, and a unicast `-B` no longer misses
    /// broadcast traffic.
    #[test]
    fn bind_endpoint_maps_to_collector_destination() {
        // Broadcast destination: the collector dest is the broadcast
        // address + port, with no join interface.
        let bcast = parse_bind("255.255.255.255:5076", 5076).unwrap();
        let dest = bcast.to_endpoint();
        assert_eq!(dest.addr, "255.255.255.255:5076".parse().unwrap());
        assert_eq!(dest.iface, None);

        // Unicast interface destination: address preserved, still no join.
        let uni = parse_bind("192.168.1.5", 5076).unwrap();
        let dest = uni.to_endpoint();
        assert_eq!(dest.addr, "192.168.1.5:5076".parse().unwrap());
        assert_eq!(dest.iface, None);

        // Multicast with `@iface`: the iface is passed to the collector for
        // the group join, as a literal IPv4 that `resolve_iface_v4` accepts.
        let mcast = parse_bind("224.0.1.1@10.0.0.2:5078", 5076).unwrap();
        let dest = mcast.to_endpoint();
        assert_eq!(dest.addr, "224.0.1.1:5078".parse().unwrap());
        assert_eq!(dest.iface.as_deref(), Some("10.0.0.2"));
    }
}
