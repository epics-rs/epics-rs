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
//! expose this `ServerSource` PV — and so does the Rust server, via
//! `server_native::server_info::ServerInfoSource` — so the query works
//! against all of them.

use std::collections::{HashMap, HashSet};
use std::net::{SocketAddr, ToSocketAddrs};

use clap::Parser;
use futures_util::future::join_all;

use epics_pva_rs::client::PvaClient;
use epics_pva_rs::client_native::search_engine::{Discovered, SearchEngine};
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarValue};

#[derive(Parser)]
#[command(
    name = "pvlist-rs",
    about = "Discover PVA servers / list hosted channels",
    disable_version_flag = true
)]
struct Args {
    /// Print version information (pvxs `version_information`, tools
    /// `case 'V'`) and exit. Routed through `cli::version_information`
    /// so every PVA CLI reports the same library + protocol stack
    /// instead of clap's crate-only `<binary> <version>`. Distinct
    /// from `-v`/`--verbose` (discovery detail).
    #[arg(short = 'V', long = "version")]
    version: bool,

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

    /// Raise the `epics_pva_rs` library log level to DEBUG. Mirrors pvxs
    /// `pvxlist -d` mapping to `logger_level_set("pvxs.*",
    /// Level::Debug)` (`tools/list.cpp:66-99`).
    #[arg(short = 'd')]
    debug: bool,

    /// Server address(es) to query, `ip[:port]`. When given, switches
    /// from discovery mode to per-server channel enumeration.
    servers: Vec<String>,
}

impl Args {
    /// Resolve the active/passive discovery flag the way pvxs does:
    /// active by default, `-A` keeps it active, `-p` clears it. pvxs
    /// `tools/list.cpp:71` initializes `active = true` and passes it to
    /// `.pingAll(active)` (`:153`); only `-p` flips it off (`:86-88`).
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
/// resolved through the system resolver. When the resolver returns a
/// mix of IPv4 and IPv6, pvxs **prefers IPv4** "for maximum
/// compatibility" — it keeps the first IPv4 result and only falls back
/// to an IPv6 result when no IPv4 was returned (`util.cpp:529-538`).
/// Taking the bare first `getaddrinfo` result (which the OS may order
/// IPv6-first) would make `pvlist-rs localhost` pick `::1` where
/// `pvxlist localhost` picks `127.0.0.1`, so an IPv4-only server is
/// found by pvxs but missed by Rust.
fn resolve_host_port(host: &str, port: u16) -> Result<SocketAddr, String> {
    let mut v6_fallback: Option<SocketAddr> = None;
    for sa in (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {host:?}: {e}"))?
    {
        if sa.is_ipv4() {
            // First IPv4 wins immediately (pvxs `break` on AF_INET).
            return Ok(sa);
        }
        // Remember the first IPv6 result as the fallback for a host that
        // resolves to IPv6 only.
        v6_fallback.get_or_insert(sa);
    }
    v6_fallback.ok_or_else(|| format!("no address found for {host:?}"))
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
    if let Some((host, port_str)) = s.rsplit_once(':') {
        // pvxs `setAddress` parses the port with `parseTo<uint64_t>` and
        // then stores it into the 16-bit socket port, truncating the low
        // 16 bits (`util.cpp:540-546`). Match that: a numeric but
        // out-of-`u16`-range port truncates rather than failing the
        // `u16` parse and being misread as part of a hostname (which
        // would make `host:70000` resolve the literal `"host:70000"`).
        let port = match port_str.parse::<u64>() {
            Ok(p) => p as u16,
            Err(_) => return Err(format!("invalid port {port_str:?} in {s:?}")),
        };
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
///
/// Delegates to the shared [`epics_pva_rs::nt::NTURI::request`] builder
/// so the request carries **all four** normative members — `scheme`,
/// `authority`, `path`, `query` — that pvxs `NTURI::NTURI()` defines
/// (`src/nt.cpp:253-263`). The previous hand-rolled descriptor omitted
/// `authority`.
fn build_server_query(op: &str) -> (FieldDesc, PvField) {
    epics_pva_rs::nt::NTURI::request(
        "pva",
        "server",
        &[("op".to_string(), ScalarValue::String(op.into()))],
    )
}

/// Extract a string scalar field by name from a structure.
fn str_field(s: &PvStructure, name: &str) -> Option<String> {
    match s.get_field(name)? {
        PvField::Scalar(ScalarValue::String(v)) => Some(v.as_str_lossy().into_owned()),
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
                ScalarValue::String(s) => Some(s.as_str_lossy().into_owned()),
                _ => None,
            })
            .collect(),
        PvField::ScalarArrayTyped(t) => t
            .to_scalar_values()
            .into_iter()
            .filter_map(|v| match v {
                ScalarValue::String(s) => Some(s.as_str_lossy().into_owned()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Query one server's `server` PV and print either its info or its
/// hosted-channel list. A failing server reports the error to stderr
/// but does NOT fail the command: pvxs `pvxlist` query mode catches the
/// per-server exception, prints `From <server> : <error>`, and never
/// updates the process return code (`tools/list.cpp:162-194`), returning
/// 0 unconditionally after the wait (`:200-205`). Only invalid
/// invocation (a parse error, handled in `main`) is a command failure.
async fn query_server(client: &PvaClient, raw: &str, addr: SocketAddr, info: bool, verbose: bool) {
    let op = if info { "info" } else { "channels" };
    let (desc, value) = build_server_query(op);
    match client.pvrpc_from("server", addr, &desc, &value).await {
        Ok(reply) => {
            // A server may answer with the pvxs no-value reply shape
            // (`ExecOp::reply()`, a bare NULL type code); treat it as an
            // empty response rather than a failure.
            let resp_value = reply.into_value().map_or(PvField::Null, |(_, v)| v);
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
        }
        Err(e) => {
            eprintln!("pvlist-rs: from {raw}: {e}");
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
/// (`:138-139`). Verbose mode keeps the detailed `ONLINE`/`OFFLINE` status
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
    fn on_timeout(&mut self, server: SocketAddr, guid: [u8; 12], proto: &str) -> Option<String> {
        if self.verbose {
            Some(format!(
                "OFFLINE  {server:24}  guid={}  proto={proto}",
                fmt_guid(&guid)
            ))
        } else if proto == "tcp" {
            // pvxs erases the `(guid, proto)` entry on Timeout so a later
            // re-Online reprints the address — but the erase is gated on
            // `proto == "tcp"` (list.cpp:133-137), so a `tls` timeout must
            // NOT retire the `tcp` identity. Carrying `proto` on the event
            // is what makes that gate possible; without it, every timeout
            // wrongly cleared the lone `tcp` key.
            self.tcp_seen.remove(&(guid, proto.to_string()));
            None
        } else {
            // Non-verbose suppresses non-TCP endpoints (list.cpp:133).
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
    let reactor = epics_base_rs::runtime::task::Reactor::current()
        .expect("discover_mode is awaited on the tool's runtime");
    let engine = match SearchEngine::spawn(&reactor, Vec::new(), Vec::new()).await {
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
    // resolved flag, which is active by default (tools/list.cpp:71,151).
    if args.active_discovery() {
        engine.ping_all().await;
    }

    let mut printer = DiscoveryPrinter::new(args.verbose);
    // `-w 0` (and any non-positive / non-finite value) means "wait
    // forever". Derived from the same `TimeoutPolicy::wait_or_forever`
    // the query path uses, so the two modes cannot diverge on what `-w`
    // means; `Forever` → no deadline → unbounded receive wait.
    let deadline = epics_pva_rs::cli::TimeoutPolicy::wait_or_forever(args.timeout)
        .finite_duration()
        // The seam's clock, not `tokio::time::Instant`: the deadline is read
        // back by `task::timeout_at`, and naming tokio's type here pins the
        // deadline to the tokio clock on a backend that has none.
        .map(|d| epics_base_rs::runtime::task::Instant::now() + d);

    loop {
        let recv_fut = rx.recv();
        let evt = match deadline {
            Some(d) => match epics_base_rs::runtime::task::timeout_at(d, recv_fut).await {
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
            Discovered::Timeout {
                server,
                guid,
                proto,
                ..
            } => {
                if let Some(line) = printer.on_timeout(server, guid, &proto) {
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
    let args: Args = epics_pva_rs::cli::parse_or_exit();

    // pvxs `-V` prints version_information and exits before discovery
    // (tools/list.cpp:75-82).
    if args.version {
        print!("{}", epics_pva_rs::cli::version_information());
        return;
    }

    // Install the shared tracing subscriber (honours EPICS_PVA_LOG /
    // RUST_LOG) and apply `-d` as a DEBUG bump of the library namespace,
    // mirroring pvxs `logger_config_env()` + `-d` (tools/list.cpp:66-99).
    epics_pva_rs::log::install_cli_logging(args.debug);

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

    // Query mode shares discovery's `-w` policy: `pvxlist -w 0 SERVER`
    // waits indefinitely for the server RPCs (`tools/list.cpp:154-203`),
    // it does not fall back to a finite per-RPC timeout. Route `-w`
    // through the same `TimeoutPolicy::wait_or_forever` so `-w 0` is a
    // no-deadline operation timeout here too, not a 5 s clamp.
    let client = PvaClient::builder()
        .timeout(epics_pva_rs::cli::TimeoutPolicy::wait_or_forever(args.timeout).op_timeout())
        .build();

    // pvxs `tools/list.cpp:156-197` `exec()`s every server RPC before
    // waiting, then waits once on a shared event bounded by a single
    // command-level timeout (`:200-203`). Launch all queries
    // concurrently and await them as one batch so a slow or
    // unreachable earlier server cannot delay or block later servers;
    // the shared `client` timeout bounds the whole batch to one wait
    // window instead of `N * -w`.
    //
    // Per-server RPC failures are reported by `query_server` but do not
    // fail the command — pvxs query mode returns 0 unconditionally
    // (`:200-205`), so `pvlist-rs good bad` still prints `good`'s
    // channels and exits 0. Only invalid invocation (the parse loop
    // above, exit 2) is a command failure.
    join_all(
        addrs
            .iter()
            .map(|(raw, addr)| query_server(&client, raw, *addr, args.info, args.verbose)),
    )
    .await;
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

    /// `-d` parses into a real flag wired to `install_cli_logging`
    /// (pvxs `pvxlist -d`, list.cpp:66-99). Default off.
    #[test]
    fn debug_flag_parses() {
        assert!(Args::parse_from(["pvlist-rs", "-d"]).debug);
        assert!(!Args::parse_from(["pvlist-rs"]).debug);
    }

    /// `pvlist-rs -w 0` is "no timeout" in BOTH discovery and query
    /// modes (pvxs `tools/list.cpp:55-58,154-203`). Both modes derive the
    /// policy from the same parsed `-w`, so `-w 0` → `Forever` (no
    /// finite deadline) for either argument shape; the prior query-mode
    /// 5 s clamp is gone.
    #[test]
    fn w_zero_is_no_timeout_in_both_modes() {
        use epics_pva_rs::cli::TimeoutPolicy;
        // Discovery argument shape (no server) and query argument shape
        // (server present) parse the same `-w 0`.
        for argv in [
            vec!["pvlist-rs", "-w", "0"],
            vec!["pvlist-rs", "-w", "0", "1.2.3.4"],
        ] {
            let args = Args::parse_from(argv.clone());
            let policy = TimeoutPolicy::wait_or_forever(args.timeout);
            assert_eq!(policy, TimeoutPolicy::Forever, "argv={argv:?}");
            // Discovery: Forever → no deadline.
            assert_eq!(policy.finite_duration(), None, "argv={argv:?}");
        }
    }

    /// A positive `-w` is a bounded deadline in both modes.
    #[test]
    fn w_positive_is_finite_in_both_modes() {
        use epics_pva_rs::cli::TimeoutPolicy;
        let args = Args::parse_from(["pvlist-rs", "-w", "3", "1.2.3.4"]);
        let policy = TimeoutPolicy::wait_or_forever(args.timeout);
        assert_eq!(
            policy,
            TimeoutPolicy::Finite(std::time::Duration::from_secs(3))
        );
        assert_eq!(policy.op_timeout(), std::time::Duration::from_secs(3));
    }

    /// The `server` RPC request advertises all four normative NTURI
    /// members, including `authority` (pvxs `NTURI::NTURI()`,
    /// `src/nt.cpp:253-263`). The pre-fix hand-rolled descriptor omitted
    /// `authority`.
    #[test]
    fn server_query_nturi_includes_authority() {
        let (desc, value) = build_server_query("channels");
        let FieldDesc::Structure { struct_id, fields } = &desc else {
            panic!("expected NTURI structure descriptor");
        };
        assert_eq!(struct_id, "epics:nt/NTURI:1.0");
        let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["scheme", "authority", "path", "query"]);
        let PvField::Structure(root) = &value else {
            panic!("expected NTURI structure value");
        };
        assert!(
            root.get_field("authority").is_some(),
            "value must carry the authority member"
        );
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
        assert!(p.on_timeout(addr("10.0.0.5:5075"), guid, "tcp").is_none());
        // After timeout cleared the entry, a re-Online reprints.
        assert!(
            p.on_online(addr("10.0.0.5:5075"), guid, addr("10.0.0.5:1"), "tcp")
                .is_some()
        );
    }

    /// The non-verbose dedup erase is gated on `proto == "tcp"`
    /// (list.cpp:133-137): a `tls` Timeout must NOT clear the printed
    /// `tcp` identity. Before the fix `on_timeout` carried no proto and
    /// cleared `(guid, "tcp")` for *every* timeout, so a `tls` server going
    /// offline wrongly caused the `tcp` address to reprint on its next
    /// beacon.
    #[test]
    fn nonverbose_tls_timeout_does_not_clear_tcp() {
        let mut p = DiscoveryPrinter::new(false);
        let guid = [7u8; 12];
        // tcp endpoint printed once.
        assert!(
            p.on_online(addr("10.0.0.5:5075"), guid, addr("10.0.0.5:1"), "tcp")
                .is_some()
        );
        // A tls timeout for the SAME guid must not retire the tcp entry.
        assert!(p.on_timeout(addr("10.0.0.5:5076"), guid, "tls").is_none());
        // tcp beacon again: still deduped (entry survived the tls timeout).
        assert!(
            p.on_online(addr("10.0.0.5:5075"), guid, addr("10.0.0.5:1"), "tcp")
                .is_none(),
            "a tls timeout must not cause the tcp address to reprint"
        );
        // Only a tcp timeout clears it → the next tcp beacon reprints.
        assert!(p.on_timeout(addr("10.0.0.5:5075"), guid, "tcp").is_none());
        assert!(
            p.on_online(addr("10.0.0.5:5075"), guid, addr("10.0.0.5:1"), "tcp")
                .is_some(),
            "a tcp timeout clears the entry so the next tcp beacon reprints"
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
    ///
    /// `localhost` typically resolves to *both* `127.0.0.1` and `::1`;
    /// pvxs `setAddress` prefers the IPv4 result (`util.cpp:529-538`), so
    /// the resolved address must be IPv4, not whichever the OS returns
    /// first (which is often `::1`).
    #[test]
    fn parse_addr_resolves_hostname() {
        let bare = parse_server_addr("localhost", 5075).expect("localhost resolves");
        assert!(bare.ip().is_loopback(), "got {bare}");
        assert!(bare.is_ipv4(), "expected IPv4 preference, got {bare}");
        assert_eq!(bare.port(), 5075);

        let with_port = parse_server_addr("localhost:5099", 5075).expect("localhost:port resolves");
        assert!(with_port.ip().is_loopback(), "got {with_port}");
        assert!(
            with_port.is_ipv4(),
            "expected IPv4 preference, got {with_port}"
        );
        assert_eq!(with_port.port(), 5099);
    }

    /// An explicit port wider than 16 bits is parsed as `uint64_t` then
    /// truncated to the 16-bit socket port, matching pvxs
    /// `temp.setPort(parseTo<uint64_t>(port))` where `setPort` takes an
    /// `unsigned short` (`util.cpp:544-545`). `70000 & 0xFFFF == 4464`.
    /// A non-numeric port is a diagnostic, not silently swallowed as part
    /// of a hostname.
    #[test]
    fn parse_addr_port_truncates_like_pvxs() {
        assert_eq!(
            parse_server_addr("1.2.3.4:70000", 5075).unwrap(),
            addr("1.2.3.4:4464")
        );
        assert!(parse_server_addr("1.2.3.4:notaport", 5075).is_err());
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
        let off = p
            .on_timeout(addr("10.0.0.5:5075"), [4u8; 12], "tcp")
            .unwrap();
        assert!(off.starts_with("OFFLINE"));
        assert!(off.contains("proto=tcp"), "verbose OFFLINE carries proto");
    }
}
