//! `pvlist-rs` — server discovery + PV-name enumeration
//! mirroring pvxs `tools/list.cpp`.
//!
//! ```text
//! pvlist-rs                  # discover servers (active: broadcast ping + listen)
//! pvlist-rs -w 5             # discover for 5 seconds, then exit
//! pvlist-rs -p               # passive discovery (beacon listen only)
//! pvlist-rs -A               # active discovery (explicit; the default)
//! pvlist-rs --verbose        # include guid + proto + peer
//! pvlist-rs <ip[:port]>      # query one server for its hosted channels
//! pvlist-rs -i <ip[:port]>   # query one server for its serverInfo
//! ```
//!
//! The `pvlist-rs <ip>` form mirrors pvxs `pvxlist <ip>` (`tools/list.cpp`
//! query mode): it sends an RPC to the server's special `server` PV
//! with `op=channels` (or `op=info` for `-i`). pvxs/Java PVA servers
//! expose this `ServerSource` PV; the query therefore works against
//! them even though the Rust server does not yet host the `server` PV.

use std::collections::{HashMap, HashSet};
use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;

use clap::Parser;
use futures_util::future::join_all;

use epics_pva_rs::client::PvaClient;
use epics_pva_rs::client_native::search_engine::{Discovered, SearchEngine};
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};

#[derive(Parser)]
#[command(
    name = "pvlist-rs",
    version,
    about = "Discover PVA servers / list hosted channels"
)]
struct Args {
    /// Wait time in seconds before exiting (0 = forever)
    #[arg(short = 'w', default_value = "5.0")]
    timeout: f64,

    /// Active discovery mode (default): send a broadcast ping, then
    /// keep listening for beacons. pvxs `tools/list.cpp:50-52,83-85`
    /// documents `-A` as active and initializes `active = true`.
    #[arg(short = 'A', long = "active")]
    active: bool,

    /// Passive discovery mode: only listen for server beacons, no
    /// broadcast ping. pvxs `tools/list.cpp:53,86-88` makes `-p` set
    /// `active = false`.
    #[arg(short = 'p', long = "passive", conflicts_with = "active")]
    passive: bool,

    /// Verbose output — include GUID, proto, and beacon peer address.
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,

    /// Query server info (`op=info`) instead of the hosted-channel
    /// list. Requires a server address argument. Mirrors `pvxlist -i`.
    #[arg(short = 'i', long = "info")]
    info: bool,

    /// Server address(es) to query, `ip[:port]`. When given, switches
    /// from discovery mode to per-server channel enumeration.
    servers: Vec<String>,
}

impl Args {
    /// Resolve the active/passive discovery flag the way pvxs does:
    /// active by default, `-A` keeps it active, `-p` clears it. pvxs
    /// `tools/list.cpp:71` initializes `active = true` and passes it to
    /// `.pingAll(active)` (`:151`); only `-p` flips it off (`:86-88`).
    fn active_discovery(&self) -> bool {
        !self.passive
    }
}

fn fmt_guid(guid: &[u8; 12]) -> String {
    let mut s = String::with_capacity(24);
    for b in guid {
        use std::fmt::Write;
        write!(&mut s, "{b:02X}").unwrap();
    }
    s
}

/// Resolve `host` to a single [`SocketAddr`] at `port`, mirroring the
/// synchronous DNS fallback in pvxs `SockAddr::setAddress`
/// (`src/util.cpp:523-549`): when the token is not a literal IP it is
/// resolved through the system resolver. The first returned address is
/// used (pvxs likewise takes the first `getaddrinfo` result).
fn resolve_host_port(host: &str, port: u16) -> Result<SocketAddr, String> {
    (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {host:?}: {e}"))?
        .next()
        .ok_or_else(|| format!("no address found for {host:?}"))
}

/// Parse `host[:port]`, defaulting to the PVA server TCP port (5075, or
/// `$EPICS_PVA_SERVER_PORT`). A literal IPv4/IPv6 is used directly;
/// anything else is resolved through DNS, matching pvxs `pvxlist`, which
/// passes the raw argument into `forceServer.setAddress(...)`
/// (`src/client.cpp:347-359`) where `SockAddr::setAddress`
/// (`src/util.cpp:523-549`) resolves hostnames. IPv6 literals must be
/// bracketed (`[::1]:5075`) to disambiguate the colon.
fn parse_server_addr(s: &str, default_port: u16) -> Result<SocketAddr, String> {
    // Already a full socket address (handles bracketed IPv6 + port).
    if let Ok(addr) = s.parse::<SocketAddr>() {
        return Ok(addr);
    }
    // Bracketed IPv6 without a port.
    if let Some(inner) = s.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        let ip: std::net::IpAddr = inner
            .parse()
            .map_err(|e| format!("ipv6 {inner:?} invalid: {e}"))?;
        return Ok(SocketAddr::new(ip, default_port));
    }
    // Bare IP (no port) — works for IPv4 and unbracketed IPv6.
    if let Ok(ip) = s.parse::<std::net::IpAddr>() {
        return Ok(SocketAddr::new(ip, default_port));
    }
    // `host:port` — explicit port on an IPv4 literal or a hostname.
    if let Some((host, port_str)) = s.rsplit_once(':')
        && let Ok(port) = port_str.parse::<u16>()
    {
        return match host.parse::<std::net::IpAddr>() {
            Ok(ip) => Ok(SocketAddr::new(ip, port)),
            // Not a literal IP → resolve the hostname (pvxs setAddress).
            Err(_) => resolve_host_port(host, port),
        };
    }
    // Bare hostname (no port) — resolve with the default port.
    resolve_host_port(s, default_port)
}

/// Build the NTURI RPC request that the pvxs/Java `server` PV expects:
/// `scheme="pva"`, `path="server"`, `query.op=<op>`. Mirrors pvxs
/// `ctxt.rpc("server").arg("op", ...)` (`tools/list.cpp`).
fn build_server_query(op: &str) -> (FieldDesc, PvField) {
    let desc = FieldDesc::Structure {
        struct_id: "epics:nt/NTURI:1.0".into(),
        fields: vec![
            ("scheme".into(), FieldDesc::Scalar(ScalarType::String)),
            ("path".into(), FieldDesc::Scalar(ScalarType::String)),
            (
                "query".into(),
                FieldDesc::Structure {
                    struct_id: String::new(),
                    fields: vec![("op".into(), FieldDesc::Scalar(ScalarType::String))],
                },
            ),
        ],
    };
    let mut top = PvStructure::new("epics:nt/NTURI:1.0");
    top.fields.push((
        "scheme".into(),
        PvField::Scalar(ScalarValue::String("pva".into())),
    ));
    top.fields.push((
        "path".into(),
        PvField::Scalar(ScalarValue::String("server".into())),
    ));
    let mut query = PvStructure::new("");
    query
        .fields
        .push(("op".into(), PvField::Scalar(ScalarValue::String(op.into()))));
    top.fields.push(("query".into(), PvField::Structure(query)));
    (desc, PvField::Structure(top))
}

/// Extract a string scalar field by name from a structure.
fn str_field(s: &PvStructure, name: &str) -> Option<String> {
    match s.get_field(name)? {
        PvField::Scalar(ScalarValue::String(v)) => Some(v.clone()),
        _ => None,
    }
}

/// Collect the channel names from a `server` PV `channels` reply. pvxs
/// returns them under `value` as a string array.
fn channel_names(value: &PvField) -> Vec<String> {
    let arr = match value {
        PvField::Structure(s) => match s.get_field("value") {
            Some(v) => v,
            None => return Vec::new(),
        },
        other => other,
    };
    match arr {
        PvField::ScalarArray(items) => items
            .iter()
            .filter_map(|v| match v {
                ScalarValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        PvField::ScalarArrayTyped(t) => t
            .to_scalar_values()
            .into_iter()
            .filter_map(|v| match v {
                ScalarValue::String(s) => Some(s),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Query one server's `server` PV and print either its info or its
/// hosted-channel list. Returns `Ok(())` so a failing server doesn't
/// abort enumeration of the remaining ones.
async fn query_server(
    client: &PvaClient,
    raw: &str,
    addr: SocketAddr,
    info: bool,
    verbose: bool,
) -> Result<(), ()> {
    let op = if info { "info" } else { "channels" };
    let (desc, value) = build_server_query(op);
    match client.pvrpc_from("server", addr, &desc, &value).await {
        Ok((_resp_desc, resp_value)) => {
            if info {
                let mut line = raw.to_string();
                if let PvField::Structure(s) = &resp_value {
                    if let Some(v) = str_field(s, "version") {
                        line.push_str(&format!(" version={v:?}"));
                    }
                    if let Some(l) = str_field(s, "implLang") {
                        line.push_str(&format!(" lang={l:?}"));
                    }
                }
                println!("{line}");
            } else {
                if verbose {
                    println!("# From {raw}");
                }
                let names = channel_names(&resp_value);
                if names.is_empty() && verbose {
                    eprintln!("# {raw}: server returned no channels");
                }
                for name in names {
                    println!("{name}");
                }
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("pvlist-rs: from {raw}: {e}");
            Err(())
        }
    }
}

/// Decides what `pvlist-rs` prints for each discovery event, matching
/// pvxs `tools/list.cpp:129-148`.
///
/// Non-verbose mode is pipe-compatible: it emits only the raw TCP
/// server address (so `pvlist-rs $(pvlist-rs -w 5)` works — the address
/// feeds straight back into query mode), prints one address per
/// `(guid, proto)` even when a server is reachable through several
/// interfaces (`:139-147`), and suppresses Timeout/offline events
/// (`:136`). Verbose mode keeps the detailed `ONLINE`/`OFFLINE` status
/// lines.
struct DiscoveryPrinter {
    verbose: bool,
    /// Non-verbose dedup: one printed address per `(guid, proto)`.
    tcp_seen: HashSet<([u8; 12], String)>,
    /// Verbose dedup + "servers seen" summary count, keyed `(server, guid)`.
    online_seen: HashMap<(SocketAddr, [u8; 12]), ()>,
}

impl DiscoveryPrinter {
    fn new(verbose: bool) -> Self {
        Self {
            verbose,
            tcp_seen: HashSet::new(),
            online_seen: HashMap::new(),
        }
    }

    /// Line to print for an `Online` event, or `None` to suppress.
    fn on_online(
        &mut self,
        server: SocketAddr,
        guid: [u8; 12],
        peer: SocketAddr,
        proto: &str,
    ) -> Option<String> {
        if self.verbose {
            // Detailed status line; dedup by (server, guid).
            if self.online_seen.insert((server, guid), ()).is_some() {
                return None;
            }
            Some(format!(
                "ONLINE   {server:24}  guid={}  proto={proto}  peer={peer}",
                fmt_guid(&guid)
            ))
        } else if proto == "tcp" {
            // pvxs prints just one interface per (guid, proto) because
            // the list is piped back to fetch PVs (list.cpp:139-147).
            if self.tcp_seen.insert((guid, proto.to_string())) {
                Some(server.to_string())
            } else {
                None
            }
        } else {
            // Non-verbose suppresses non-TCP endpoints (list.cpp:133).
            None
        }
    }

    /// Line to print for a `Timeout` (offline) event, or `None`.
    fn on_timeout(&mut self, server: SocketAddr, guid: [u8; 12]) -> Option<String> {
        if self.verbose {
            Some(format!("OFFLINE  {server:24}  guid={}", fmt_guid(&guid)))
        } else {
            // pvxs erases the (guid, "tcp") entry on Timeout so a later
            // re-Online reprints the address (list.cpp:136). Our Timeout
            // event carries no proto and non-verbose only tracks tcp.
            self.tcp_seen.remove(&(guid, "tcp".to_string()));
            None
        }
    }

    /// Distinct servers seen, for the verbose summary line.
    fn seen_count(&self) -> usize {
        self.online_seen.len()
    }
}

/// Discovery mode — active by default (broadcast ping + beacon
/// listen); `-p` makes it passive (beacon listen only).
async fn discover_mode(args: &Args) {
    let engine = match SearchEngine::spawn(Vec::new(), Vec::new()).await {
        Ok(e) => e,
        Err(e) => {
            eprintln!("pvlist-rs: failed to spawn search engine: {e}");
            std::process::exit(1);
        }
    };
    let mut rx = match engine.discover().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("pvlist-rs: failed to subscribe to discovery: {e}");
            std::process::exit(1);
        }
    };
    // pvxs always calls `.pingAll(active)`; with active=false it skips
    // the broadcast probe. We gate the equivalent `ping_all()` on the
    // resolved flag, which is active by default (tools/list.cpp:151).
    if args.active_discovery() {
        engine.ping_all().await;
    }

    let mut printer = DiscoveryPrinter::new(args.verbose);
    // `args.timeout <= 0` means "wait forever" by design. Non-finite
    // (NaN / ±Inf) also collapses to "no deadline".
    let deadline = if args.timeout.is_finite() && args.timeout > 0.0 {
        Some(tokio::time::Instant::now() + Duration::from_secs_f64(args.timeout))
    } else {
        None
    };

    loop {
        let recv_fut = rx.recv();
        let evt = match deadline {
            Some(d) => match tokio::time::timeout_at(d, recv_fut).await {
                Ok(opt) => opt,
                Err(_) => break,
            },
            None => recv_fut.await,
        };
        let Some(evt) = evt else {
            break;
        };
        match evt {
            Discovered::Online {
                server,
                guid,
                peer,
                proto,
            } => {
                if let Some(line) = printer.on_online(server, guid, peer, &proto) {
                    println!("{line}");
                }
            }
            Discovered::Timeout { server, guid } => {
                if let Some(line) = printer.on_timeout(server, guid) {
                    println!("{line}");
                }
            }
        }
    }

    if args.verbose && printer.seen_count() > 0 {
        println!("\n{} server(s) seen.", printer.seen_count());
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    if args.servers.is_empty() {
        if args.info {
            eprintln!("pvlist-rs: -i requires at least one server address");
            std::process::exit(2);
        }
        discover_mode(&args).await;
        return;
    }

    // Query mode — enumerate hosted channels of each named server.
    let default_port = epics_pva_rs::config::server_port();
    let mut addrs: Vec<(String, SocketAddr)> = Vec::with_capacity(args.servers.len());
    for raw in &args.servers {
        match parse_server_addr(raw, default_port) {
            Ok(addr) => addrs.push((raw.clone(), addr)),
            Err(e) => {
                eprintln!("pvlist-rs: {e}");
                std::process::exit(2);
            }
        }
    }

    let client = PvaClient::builder()
        .timeout(epics_pva_rs::cli::timeout_duration(args.timeout))
        .build();

    // pvxs `tools/list.cpp:156-197` `exec()`s every server RPC before
    // waiting, then waits once on a shared event bounded by a single
    // command-level timeout (`:200-203`). Launch all queries
    // concurrently and await them as one batch so a slow or
    // unreachable earlier server cannot delay or block later servers;
    // the shared `client` timeout bounds the whole batch to one wait
    // window instead of `N * -w`.
    let results = join_all(
        addrs
            .iter()
            .map(|(raw, addr)| query_server(&client, raw, *addr, args.info, args.verbose)),
    )
    .await;
    if results.iter().any(|r| r.is_err()) {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// pvxs `tools/list.cpp:71` defaults `active = true`: no-argument
    /// discovery actively pings.
    #[test]
    fn discovery_is_active_by_default() {
        let args = Args::parse_from(["pvlist-rs"]);
        assert!(args.active_discovery());
    }

    /// pvxs `-p` (`tools/list.cpp:86-88`) sets `active = false`.
    #[test]
    fn passive_flag_disables_active_discovery() {
        let args = Args::parse_from(["pvlist-rs", "-p"]);
        assert!(!args.active_discovery());
        let long = Args::parse_from(["pvlist-rs", "--passive"]);
        assert!(!long.active_discovery());
    }

    /// pvxs `-A` (`tools/list.cpp:83-85`) keeps active mode.
    #[test]
    fn active_flag_keeps_active_discovery() {
        let args = Args::parse_from(["pvlist-rs", "-A"]);
        assert!(args.active_discovery());
    }

    /// `-A` and `-p` are mutually exclusive, mirroring the single
    /// `active` bool in pvxs that the two options write.
    #[test]
    fn active_and_passive_conflict() {
        assert!(Args::try_parse_from(["pvlist-rs", "-A", "-p"]).is_err());
    }

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    /// Non-verbose discovery prints only the raw TCP address (no
    /// ONLINE/OFFLINE label), and that line round-trips through
    /// query-mode address parsing — the `pvlist-rs $(pvlist-rs)` form.
    #[test]
    fn nonverbose_prints_pipeable_tcp_address() {
        let mut p = DiscoveryPrinter::new(false);
        let line = p
            .on_online(
                addr("10.0.0.5:5075"),
                [1u8; 12],
                addr("10.0.0.5:34000"),
                "tcp",
            )
            .expect("new tcp server prints its address");
        assert_eq!(line, "10.0.0.5:5075");
        // The printed token must parse back as a query-mode address.
        assert_eq!(
            parse_server_addr(&line, 5075).unwrap(),
            addr("10.0.0.5:5075")
        );
    }

    /// Non-verbose mode prints one address per (guid, proto), even
    /// across interfaces, and suppresses Timeout/offline events.
    #[test]
    fn nonverbose_dedups_and_suppresses_offline() {
        let mut p = DiscoveryPrinter::new(false);
        let guid = [2u8; 12];
        assert!(
            p.on_online(addr("10.0.0.5:5075"), guid, addr("10.0.0.5:1"), "tcp")
                .is_some()
        );
        // Same (guid, proto) via a different interface address: suppressed.
        assert!(
            p.on_online(addr("192.168.1.5:5075"), guid, addr("192.168.1.5:1"), "tcp")
                .is_none()
        );
        // Offline event prints nothing in non-verbose mode.
        assert!(p.on_timeout(addr("10.0.0.5:5075"), guid).is_none());
        // After timeout cleared the entry, a re-Online reprints.
        assert!(
            p.on_online(addr("10.0.0.5:5075"), guid, addr("10.0.0.5:1"), "tcp")
                .is_some()
        );
    }

    /// Non-verbose mode suppresses non-TCP endpoints (e.g. tls).
    #[test]
    fn nonverbose_suppresses_non_tcp() {
        let mut p = DiscoveryPrinter::new(false);
        assert!(
            p.on_online(addr("10.0.0.5:5076"), [3u8; 12], addr("10.0.0.5:1"), "tls")
                .is_none()
        );
    }

    /// A numeric IPv4 with no port takes the default port (no DNS).
    #[test]
    fn parse_addr_numeric_ipv4_default_port() {
        assert_eq!(
            parse_server_addr("1.2.3.4", 5075).unwrap(),
            addr("1.2.3.4:5075")
        );
        assert_eq!(
            parse_server_addr("1.2.3.4:5099", 5075).unwrap(),
            addr("1.2.3.4:5099")
        );
    }

    /// A bracketed IPv6 with explicit port parses without DNS.
    #[test]
    fn parse_addr_bracketed_ipv6_with_port() {
        assert_eq!(
            parse_server_addr("[::1]:5075", 5075).unwrap(),
            addr("[::1]:5075")
        );
        // Bracketed IPv6 without a port takes the default.
        assert_eq!(
            parse_server_addr("[::1]", 5075).unwrap(),
            addr("[::1]:5075")
        );
    }

    /// finding 66: a hostname must resolve (pvxs setAddress DNS
    /// fallback), not be rejected. `localhost` resolves offline via
    /// the hosts file, with and without an explicit port.
    #[test]
    fn parse_addr_resolves_hostname() {
        let bare = parse_server_addr("localhost", 5075).expect("localhost resolves");
        assert!(bare.ip().is_loopback(), "got {bare}");
        assert_eq!(bare.port(), 5075);

        let with_port = parse_server_addr("localhost:5099", 5075).expect("localhost:port resolves");
        assert!(with_port.ip().is_loopback(), "got {with_port}");
        assert_eq!(with_port.port(), 5099);
    }

    /// An invalid bracketed address yields a diagnostic, not a panic
    /// (the synchronous failure path; hostname resolution failures use
    /// the same `Result`-based reporting).
    #[test]
    fn parse_addr_invalid_is_error() {
        assert!(parse_server_addr("[not-an-ip]", 5075).is_err());
    }

    /// Verbose mode keeps the detailed ONLINE/OFFLINE status lines.
    #[test]
    fn verbose_keeps_status_labels() {
        let mut p = DiscoveryPrinter::new(true);
        let on = p
            .on_online(addr("10.0.0.5:5075"), [4u8; 12], addr("10.0.0.5:1"), "tcp")
            .unwrap();
        assert!(on.starts_with("ONLINE"));
        let off = p.on_timeout(addr("10.0.0.5:5075"), [4u8; 12]).unwrap();
        assert!(off.starts_with("OFFLINE"));
    }
}
