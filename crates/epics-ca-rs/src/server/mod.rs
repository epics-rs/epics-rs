//! CA server components — TCP handler, UDP search, beacon, monitor.

pub mod access_token;
pub mod addr_list;
// The async server front-end — the `tokio::net` accept/beacon/introspection
// stack and the `CaServer` orchestrator (which also drives `tokio::signal` and
// the `discovery` stack) — is host-only; its deps do not build for RTEMS or
// VxWorks. The embedded build serves CA through the `std::net` `blocking`
// driver plus the runtime-agnostic shared logic in
// `tcp`/`udp`/`monitor`/`stats`. The gate is `tokio_backend` — the accept loop
// hands each client to `runtime::task::Reactor`, so it needs a build where
// that seam is the tokio runtime, which excludes the two embedded targets and
// a host `exec_backend` build alike.
#[cfg(tokio_backend)]
pub mod beacon;
pub mod blocking;
#[cfg(tokio_backend)]
pub mod ca_server;
#[cfg(test)]
mod delivery_alloc_tests;
pub(crate) mod frame;
#[cfg(tokio_backend)]
pub mod introspection;
pub mod ioc_app;
pub mod iocsh;
pub mod monitor;
pub mod outbox;
pub mod rate_limit;
pub(crate) mod recv;
pub(crate) mod send;
#[cfg(all(feature = "cap-tokens", tokio_backend))]
pub mod signed_beacon;
pub mod stats;
pub mod tcp;
pub mod udp;

#[cfg(tokio_backend)]
pub use ca_server::{AccessRightsNotifier, CaServer, CaServerBuilder};
/// Live-connection / byte / channel / subscription counters. Runtime-agnostic
/// (pure atomics) and shared by the async server and the blocking driver's
/// monitor path, so it lives outside the host-only `ca_server` module.
pub use stats::ServerStats;
pub use tcp::ServerConnectionEvent;

// `run_ca_ioc` (below) builds a `CaServer` — the async front-end — so both it
// and these imports carry its `tokio_backend` gate. Every reactor-free build
// enters through the blocking server driver (`server::blocking`) instead: the
// two embedded targets, and a host `exec_backend` build with them.
#[cfg(tokio_backend)]
use epics_base_rs::error::CaResult;
#[cfg(tokio_backend)]
use epics_base_rs::server::ioc_app::IocRunConfig;

/// A `$` channel's delivery: the value as `dbGet` reads it through the
/// `DBR_CHAR` address, see
/// [`posted_char_view`](epics_base_rs::server::database::char_view::posted_char_view).
/// `requested_count` is `dbGet`'s `nRequest` (0 for autosize); the wire
/// pads to a fixed request count, as C `read_reply` does.
pub(super) fn apply_long_string(
    v: epics_base_rs::types::EpicsValue,
    no_elements: u32,
    nul_at_count: bool,
    requested_count: u32,
) -> epics_base_rs::types::EpicsValue {
    let n = match requested_count {
        0 => no_elements,
        r => r.min(no_elements),
    };
    epics_base_rs::server::database::char_view::posted_char_view(v, n as usize, nul_at_count)
}

/// Convert a long-string *record* field's snapshot value from
/// `EpicsValue::CharArray` to a scalar `EpicsValue::String`. C
/// `cvt_dbaddr` presents lsi/lso VAL & OVAL and printf VAL as a scalar
/// `DBF_STRING` with `no_elements = 1` (lsiRecord.c:141-143,
/// lsoRecord.c:183-185, printfRecord.c:415-417); the record stores the
/// value as a NUL-terminable CHAR array (the long-string carrier). This
/// is the inverse of [`apply_long_string`] — the conversion the CA
/// boundary applies for *plain* (non-`$`) access so the channel ships a
/// single `DBR_STRING` element. The buffer is decoded verbatim (no
/// UTF-8 validation, matching pvxs raw-byte storage) up to the first
/// NUL; the DBR_STRING encoder then truncates to `MAX_STRING_SIZE`
/// (= 40), so an over-long value clips on the wire exactly as C does.
/// Non-`CharArray` values pass through unchanged.
///
/// The carrier holds `LEN` elements, and `dbGet` reads `min(LEN, 1)`
/// (`get_array_info` against the capacity of 1, `dbAccess.c:936-1001`), so
/// an empty carrier is no element, not `""`.
pub(super) fn apply_native_long_string(
    v: epics_base_rs::types::EpicsValue,
) -> epics_base_rs::types::EpicsValue {
    use epics_base_rs::types::EpicsValue;
    match v {
        EpicsValue::CharArray(b) | EpicsValue::UCharArray(b) if b.is_empty() => {
            EpicsValue::StringArray(Vec::new().into())
        }
        v => epics_base_rs::server::database::char_view::long_string_text(v),
    }
}

/// How a channel presents a long-string field on the CA wire. `$`-access
/// and plain access to a long-string *record* field are mutually
/// exclusive boundary conversions, so they share one mode rather than two
/// booleans — the illegal "both at once" state cannot be constructed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum LongStringMode {
    /// Ordinary field: deliver the value verbatim.
    Plain,
    /// Client appended `$`: the field is delivered as C's `DBR_CHAR`
    /// address of it, `no_elements` long (`dbAccess.c:682-701`). See
    /// [`apply_long_string`].
    DollarChar {
        no_elements: u32,
        nul_at_count: bool,
        /// The `DBR_CHAR` image the channel's post chain runs on.
        view: epics_base_rs::server::database::char_view::DollarView,
    },
    /// Plain access to a long-string *record* field (lsi/lso VAL & OVAL,
    /// printf VAL): C `cvt_dbaddr` presents it as a scalar `DBF_STRING`,
    /// so the CHAR-array carrier is decoded to a scalar string before
    /// encoding. See [`apply_native_long_string`].
    NativeString,
}

/// Apply the boundary conversion selected by `mode` to a delivery
/// snapshot before DBR encoding.
pub(super) fn apply_long_string_mode(
    v: epics_base_rs::types::EpicsValue,
    mode: LongStringMode,
    requested_count: u32,
) -> epics_base_rs::types::EpicsValue {
    match mode {
        LongStringMode::DollarChar {
            no_elements,
            nul_at_count,
            ..
        } => apply_long_string(v, no_elements, nul_at_count, requested_count),
        LongStringMode::NativeString => apply_native_long_string(v),
        LongStringMode::Plain => v,
    }
}

/// What one CA subscription does to each event between its queue and the
/// wire: the channel's post chain, on a `$` channel's char image, then the
/// long-string boundary conversion. Every delivery path reads its events
/// through this one owner, as C's event task runs the post chain for every
/// `event_read` (`dbEvent.c:984-985`).
#[derive(Clone)]
pub(crate) struct DeliveryStage {
    post: Option<std::sync::Arc<epics_base_rs::server::database::db_access::PostStage>>,
    long_string_mode: LongStringMode,
    /// What an autosize request reads, C `paddr->no_elements`.
    autosize: u32,
}

impl DeliveryStage {
    /// The stage for a channel whose post chain is `post`, on a field of
    /// `autosize` elements.
    pub(crate) fn new(
        post: epics_base_rs::server::database::filters::FilterChain,
        long_string_mode: LongStringMode,
        autosize: u32,
    ) -> Self {
        let dollar = match long_string_mode {
            LongStringMode::DollarChar { view, .. } if !post.is_empty() => Some(view),
            _ => None,
        };
        Self {
            post: epics_base_rs::server::database::db_access::PostStage::new(post, dollar)
                .map(std::sync::Arc::new),
            long_string_mode,
            autosize,
        }
    }

    /// A stage that sends every event as posted.
    #[cfg(test)]
    pub(crate) fn plain() -> Self {
        Self {
            post: None,
            long_string_mode: LongStringMode::Plain,
            autosize: 0,
        }
    }

    /// `read_reply`'s `item_count` for a `requested_count` request
    /// (`camessage.c:510-514`).
    pub(crate) fn item_count(&self, requested_count: u32) -> u32 {
        match requested_count {
            0 => self.autosize,
            n => n,
        }
    }

    /// Whether every event goes out exactly as posted.
    pub(crate) fn is_identity(&self) -> bool {
        self.post.is_none() && self.long_string_mode == LongStringMode::Plain
    }

    /// `event` as sent for a `requested_count` request, `None` where the post
    /// chain drops it.
    pub(crate) fn prepare(
        &self,
        event: epics_base_rs::server::pv::MonitorEvent,
        requested_count: u32,
    ) -> Option<epics_base_rs::server::pv::MonitorEvent> {
        // An autosize request reads the field's own count, which a log the
        // filters grew (`ts`) is cut to.
        let n_request = self.item_count(requested_count) as usize;
        let mut event = match &self.post {
            // A `$` view's read is already `dbGet`'s chars for this count.
            Some(post) if post.reads_chars() => return post.read_request(event, n_request),
            Some(post) => {
                let mut event = post.read(event)?;
                if event.value().as_value().map_or(0, |v| v.count() as usize) > n_request {
                    let mut value = event.replace_value(epics_base_rs::types::EpicsValue::Long(0));
                    value.truncate(n_request);
                    event.replace_value(value);
                }
                event
            }
            None => event,
        };
        if self.long_string_mode != LongStringMode::Plain {
            let value = event.replace_value(epics_base_rs::types::EpicsValue::Long(0));
            event.replace_value(apply_long_string_mode(
                value,
                self.long_string_mode,
                requested_count,
            ));
        }
        Some(event)
    }

    /// [`Self::prepare`] for a snapshot no queue carried: a read, or a single
    /// event posted under subscriber mask `mask`.
    pub(crate) fn prepare_snapshot(
        &self,
        snapshot: epics_base_rs::server::snapshot::Snapshot,
        requested_count: u32,
        mask: u16,
    ) -> Option<epics_base_rs::server::snapshot::Snapshot> {
        if self.is_identity() {
            return Some(snapshot);
        }
        let mut snapshot = snapshot;
        let log = epics_base_rs::server::pv::MonitorEvent::read_log(
            &mut snapshot,
            epics_base_rs::server::recgbl::EventMask::from_bits(mask),
        );
        self.prepare(log, requested_count)?
            .write_log_over(&mut snapshot);
        Some(snapshot)
    }
}

/// Stand `app` up on Channel Access — [`run_ca_ioc`] with C's `rsrvRegistrar`
/// already run, which is the only order that works.
///
/// C runs `rsrvRegistrar` out of `dbLoadDatabase`'s `.dbd` expansion
/// (`rsrvIocRegister.c:34-38`), so `casr` and the `dbServer` join are in
/// place before the first `st.cmd` line. Rust has no link-time registrar, so
/// the port has to make the call — and a call every application head must
/// remember is a rule that holds until one forgets. Pairing it with the
/// runner here is what removes the choice: the head that wants a CA server
/// says so once, and gets the registrar with it.
///
/// `IocApplication::run(run_ca_ioc)` still works and still registers `casr`
/// on the INTERACTIVE shell — it is the startup shell, the one `st.cmd`
/// executes on, that only this entry point can reach.
///
/// The `ca` link set comes with it for the same reason: C's dbCa is in every
/// IOC, so it is the runner's to install, not each head's.
#[cfg(tokio_backend)]
pub async fn run_ca_ioc_app(app: epics_base_rs::server::ioc_app::IocApplication) -> CaResult<()> {
    iocsh::register_rsrv_commands(app)
        .register_link_type("ca", crate::calink::calink_link_set_install)
        .run(run_ca_ioc)
        .await
}

/// Run an IOC with the Channel Access protocol.
///
/// This is the standard protocol runner for [`epics_base_rs::server::ioc_app::IocApplication::run`].
/// It creates a [`CaServer`] from the provided configuration and
/// starts the CA server with an interactive iocsh shell.
///
/// # Example
///
/// ```rust,ignore
/// epics_ca_rs::server::run_ca_ioc_app(
///     IocApplication::new().startup_script("st.cmd"),
/// )
/// .await
/// ```
///
/// [`run_ca_ioc_app`] rather than `.run(run_ca_ioc)`: a head that hands this
/// runner to `IocApplication::run` directly gets `casr` on the prompt but not
/// on the script, because this function is dispatched after the script has
/// already run.
#[cfg(tokio_backend)]
pub async fn run_ca_ioc(config: IocRunConfig) -> CaResult<()> {
    // For a caller that reaches the CA server only through this runner: C's
    // `registrar(rsrvRegistrar)` is there from the moment the binary links
    // RSRV, so it must not wait on `run_with_shell`. It is still too late for
    // the startup script, which has already run by the time this runner is
    // dispatched; `run_ca_ioc_app` is the entry point that is early enough.
    iocsh::declare_rsrv_registrar();
    let server = CaServer::from_parts(
        config.db,
        config.port,
        config.tcp_port,
        config.acf,
        config.autosave_config,
        config.autosave_manager,
    )
    .await?;
    // `config.after_init_hooks` is always handed over EMPTY —
    // `IocApplication::run` drains the hooks itself after PINI (H3) and
    // owns scanning via the core `ScanOwner`, so the CA server neither
    // runs hooks nor scans.
    // No `casr` here: `run_with_shell` registers it for every caller, so
    // pushing a second copy would depend on which one the registry keeps.
    server
        .run_with_shell(move |shell| {
            for cmd in config.shell_commands {
                shell.register(cmd);
            }
        })
        .await
}

/// The address lists RSRV's `casr` prints from level 1 up
/// (`caservertask.c:938-1017`), assembled for
/// [`iocsh::casr_command`].
///
/// C builds its `servers` list at bind time out of `casIntfAddrList`,
/// and its multicast / beacon / ignore lists out of the same env parse;
/// [`addr_list::from_env`] is that parse and is memoized, so reading it
/// here re-derives the binder's own lists rather than re-resolving them.
/// The one place this can disagree with the sockets actually bound is an
/// interface whose broadcast responder failed to bind — `bind_udp_responders`
/// logs that failure and continues, and C's report, reading the socket,
/// would drop back to the "name server" wording.
#[cfg(tokio_backend)]
pub(crate) fn casr_addrs(server: &CaServer) -> CaResult<iocsh::CasrAddrs> {
    use std::net::SocketAddr;

    let cfg = addr_list::from_env()?;
    let (udp_port, tcp_port) = (server.udp_port(), server.tcp_port());
    Ok(iocsh::CasrAddrs {
        interfaces: cfg
            .intf_addrs
            .iter()
            .map(|ip| iocsh::CasrInterface {
                tcp: SocketAddr::from((*ip, tcp_port)),
                udp: SocketAddr::from((*ip, udp_port)),
                udp_bcast: bcast_responder_addr(*ip, udp_port),
            })
            .collect(),
        mcast: cfg
            .mcast_addrs
            .iter()
            .map(|ip| SocketAddr::from((*ip, udp_port)))
            .collect(),
        beacon: cfg.beacon_addrs.clone(),
        // C prints these with `sin_port = 0`.
        ignore: cfg
            .ignore_addrs
            .iter()
            .map(|ip| SocketAddr::from((*ip, 0)))
            .collect(),
    })
}

/// Where C binds a second UDP name-server socket for an interface, and this
/// port binds one too.
///
/// Loopback is the one place the answer is `None` while C still binds:
/// `osiSockDiscoverBroadcastAddresses` short-circuits an `INADDR_LOOPBACK`
/// match to the loopback address itself (`osdNetIfAddrs.c:42-54`), so
/// `rsrv_init` binds `udpbcast` to the same `127.0.0.1:<port>` under
/// `epicsSocketEnableAddressUseForDatagramFanout` and runs a second
/// `cast_server` thread, `CAS-UDP2` (`caservertask.c:677-706`, `:728-738`).
/// SO_REUSEPORT hands each datagram to exactly one of that pair, so C
/// answers a loopback search once — which is what one socket does.
/// Measured with `ss -lunp`, `softIoc R7.0.10` against this port with
/// `EPICS_CAS_INTF_ADDR_LIST` pinned: on `172.17.0.1` (`IFF_BROADCAST`)
/// both bind `172.17.0.1:5188` and `172.17.255.255:5188`; on `127.0.0.1`
/// C binds `127.0.0.1:5188` twice and this binds it once.
///
/// `casr` needs no adjustment for that: C picks its wording from whether
/// the second socket exists, not from what the interface is
/// (`caservertask.c:953-966`), so one socket prints C's own single
/// `CAS-UDP name server` line.
///
/// The broadcast responder address for one interface, under the same
/// gate `bind_udp_responders` binds the socket with — C
/// `caservertask.c:671,728` skips it on Windows, and there is no
/// broadcast address for a wildcard or loopback interface.
#[cfg(tokio_backend)]
fn bcast_responder_addr(ip: std::net::Ipv4Addr, port: u16) -> Option<std::net::SocketAddr> {
    #[cfg(any(windows, target_os = "windows"))]
    {
        let _ = (ip, port);
        None
    }
    #[cfg(not(any(windows, target_os = "windows")))]
    {
        addr_list::broadcast_for_ip(ip).map(|b| std::net::SocketAddr::from((b, port)))
    }
}

#[cfg(test)]
mod spawn_capability_guard {
    //! The CA server's production code may not read an executor out of the
    //! calling thread.
    //!
    //! Two entry points state which executor the server is on, and they are
    //! the only places allowed to: `run_with_shell` starts `run` on the
    //! ambient tokio `Handle` (`ca_server.rs:902`) because `run` drives
    //! `tokio::net` listeners, and `run` mints from its own runtime the two
    //! capabilities the sites below it need (`:997` tokio, `:999` seam).
    //! `bridge.reactor()` used to place `run`, and on the exec backend that
    //! is a callback band with no reactor, so the server bound its sockets
    //! and then panicked inside `tokio::net` on the first accepted client.
    //!
    //! Below those two, a task site must take the capability as an argument.
    //! A bare `tokio::spawn` reads the thread-local of whichever worker
    //! happens to poll the caller, which is not necessarily the runtime that
    //! owns the sockets — and being right on the host proves nothing about
    //! the placement, which is the whole failure mode.
    //!
    //! `spawn_blocking` is deliberately not on this list — it names a
    //! different pool with different band semantics on the exec backend, and
    //! folding it in here would hide that difference behind one needle.
    //!
    //! `introspection.rs` is not on the file list either, and for the opposite
    //! reason: its accept loop hands each request to a handler that does
    //! `tokio::net` I/O, so that task belongs on the tokio runtime and nowhere
    //! else — routing it through the seam `Reactor` puts it on a callback
    //! worker under `exec_backend`, where it panics on the first poll
    //! (`end_to_end_healthz` measures exactly that). The ambient read is sound
    //! there because a successful `TcpListener::bind` earlier on the same task
    //! already proves the runtime it reads.
    //!
    //! Needles are assembled with `concat!` so this module's own text cannot
    //! satisfy the check it performs.

    use source_guard::{Comments, production};

    #[test]
    fn server_production_spawns_go_through_a_held_reactor() {
        let files: [(&str, &str); 4] = [
            ("ca_server.rs", include_str!("ca_server.rs")),
            ("tcp.rs", include_str!("tcp.rs")),
            ("blocking.rs", include_str!("blocking.rs")),
            ("monitor.rs", include_str!("monitor.rs")),
        ];

        // Fail closed: if the slicer ever stops covering the code this guard
        // is about, say so instead of reporting a vacuous pass.
        let anchors = [
            ("ca_server.rs", "pub async fn run("),
            ("tcp.rs", "fn write_notify_queue("),
            ("blocking.rs", "fn command_drives_without_spawn("),
            ("monitor.rs", "fn deliver_monitor_event"),
        ];

        let bare = concat!("tokio", "::spawn(");
        let aliased = concat!("tokio::task", "::spawn(");

        for (name, src) in files {
            let prod = production(src, Comments::Strip);
            let anchor = anchors.iter().find(|(n, _)| *n == name).unwrap().1;
            assert!(
                prod.contains(anchor),
                "{name}: production slice no longer contains `{anchor}` — the \
                 slicer stopped covering the guarded code"
            );
            assert_eq!(
                prod.matches(bare).count(),
                0,
                "{name}: production must start tasks on a capability handed \
                 down from `run`; found bare `{bare}`, which reads the \
                 executor out of whichever worker polls the caller"
            );
            assert_eq!(
                prod.matches(aliased).count(),
                0,
                "{name}: found `{aliased}` — the same ambient read under \
                 another name"
            );
        }
    }
}
