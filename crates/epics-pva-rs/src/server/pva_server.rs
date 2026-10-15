//! PVA server wrapper — mirrors the `CaServer` pattern for pvAccess.
//!
//! Built on top of the native runtime in [`crate::server_native`].

// RTEMS-EXEC-MODEL-ALLOW(4): this file does not build on the exec backend -
// server/mod.rs declares it under #[cfg(tokio_backend)] mod pva_server;.
use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::{access_security, autosave, iocsh};
use epics_base_rs::types::EpicsValue;
use tokio::sync::watch;

use crate::server_native::{ChannelSource, PvaServerConfig, ServerReportHandle};

use super::native_source::PvDatabaseSource;

// ── Builder ──────────────────────────────────────────────────────────────

/// Builder for constructing a [`PvaServer`] with simple PVs and/or records.
pub struct PvaServerBuilder {
    ioc: ioc_builder::IocBuilder,
    /// `None` = nobody named a port, so the EPICS environment decides
    /// (`PvaServerConfig::with_env`, whose `PickOne` order is pvxs's:
    /// `EPICS_PVAS_SERVER_PORT` before `EPICS_PVA_SERVER_PORT`). `Some(p)`
    /// is an explicit request, and `Some(0)` an ephemeral bind — the two
    /// meanings a literal 5075 default could not tell apart.
    port: Option<u16>,
    acf: Option<access_security::AccessSecurityConfig>,
}

/// A server that cannot start leaves the IOC and its shell running, with
/// the reason on stderr: pvxs `pvxsBaseRegistrar` catches the
/// `Server(conf)` exception thrown by a failed `sock.bind()`
/// (`udp_collector.cpp:151`) and prints `Error in <func> : <what>`
/// (`ioc/iochooks.cpp:508-509`, pvxs 1.5.3).
fn announce_server_failure(result: CaResult<()>) -> CaResult<()> {
    if let Err(e) = &result {
        eprintln!("Error in pvxsBaseRegistrar : {e}");
    }
    result
}

impl PvaServerBuilder {
    pub fn new() -> Self {
        Self {
            ioc: ioc_builder::IocBuilder::new(),
            port: None,
            acf: None,
        }
    }

    /// Set the TCP port, overriding the environment. The search (UDP) port
    /// stays `EPICS_PVAS_BROADCAST_PORT`'s, as pvxs reads the two
    /// independently (`config.cpp:402-415`); `0` binds both ephemerally.
    pub fn port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    /// Add a simple PV.
    pub fn pv(mut self, name: &str, initial: EpicsValue) -> Self {
        self.ioc = self.ioc.pv(name, initial);
        self
    }

    /// Add a record.
    pub fn record(mut self, name: &str, record: impl Record) -> Self {
        self.ioc = self.ioc.record(name, record);
        self
    }

    pub fn db_string(mut self, content: &str, macros: &HashMap<String, String>) -> CaResult<Self> {
        self.ioc = self.ioc.db_string(content, macros)?;
        Ok(self)
    }

    pub fn db_file(mut self, path: &str, macros: &HashMap<String, String>) -> CaResult<Self> {
        self.ioc = self.ioc.db_file(path, macros)?;
        Ok(self)
    }

    pub async fn build(self) -> CaResult<PvaServer> {
        let (db, autosave_config) = self.ioc.build_served().await?;
        let acf = epics_base_rs::server::access_security::new_acf_cell_watching(self.acf, &db);
        Ok(PvaServer {
            db,
            port: self.port,
            acf,
            acl_version: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            autosave_config,
            autosave_manager: None,
        })
    }
}

// ── PvaServer ────────────────────────────────────────────────────────────

pub struct PvaServer {
    db: Arc<PvDatabase>,
    /// `None` = the EPICS environment decides the bind port; see
    /// [`PvaServerBuilder::port`].
    port: Option<u16>,
    /// Access Security configuration. Forwarded to the default
    /// `PvDatabaseSource` in `run()` so PVA PUTs are gated through
    /// `check_access_method`. Callers that supply their own
    /// ChannelSource via `run_with_source` must install ACF
    /// themselves.
    ///
    /// A lock-free snapshot cell, so [`Self::reload_acf_from`] can
    /// swap the policy at runtime (mirrors `CaServer::reload_acf`).
    /// All `PvDatabaseSource` ACF check sites pick the latest
    /// policy on their next read.
    acf: crate::server::native_source::AcfCell,
    /// Monotonic ACL generation. Bumped by
    /// `reload_acf_from` / `clear_acf`. The default
    /// `PvDatabaseSource` constructed in `run()` shares this `Arc`
    /// via `AccessGate::required_with_version`, so monitor tasks
    /// observe the bump on their next event and tear down
    /// subscriptions that the new policy denies.
    acl_version: Arc<std::sync::atomic::AtomicU64>,
    autosave_config: Option<autosave::SaveSetConfig>,
    autosave_manager: Option<Arc<autosave::AutosaveManager>>,
}

impl PvaServer {
    pub fn builder() -> PvaServerBuilder {
        PvaServerBuilder::new()
    }

    /// `acf` is the IOC's live policy cell, ADOPTED rather than
    /// re-created: a protocol runner passes `IocRunConfig.acf` so this
    /// server, the sibling CA/QSRV servers, and the iocsh `asInit`
    /// command all gate on one cell. A standalone caller creates its
    /// own with [`epics_base_rs::server::access_security::new_acf_cell`].
    ///
    /// `port` is [`PvaServerBuilder::port`]'s: `None` leaves both ports to
    /// the environment, so a caller never resolves `EPICS_PVAS_SERVER_PORT`
    /// itself and hands it back as an explicit port.
    pub fn from_parts(
        db: Arc<PvDatabase>,
        port: Option<u16>,
        acf: epics_base_rs::server::access_security::AcfCell,
        autosave_config: Option<autosave::SaveSetConfig>,
        autosave_manager: Option<Arc<autosave::AutosaveManager>>,
    ) -> Self {
        Self {
            db,
            port,
            acf,
            acl_version: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            autosave_config,
            autosave_manager,
        }
    }

    /// Reload the Access Security policy from a `.acf` file. Mirrors
    /// `CaServer::reload_acf_from`. Parses the file off the async
    /// runtime (blocking IO; small file) and then publishes the new
    /// policy into the AcfCell. An in-flight ACF check finishes under the
    /// policy it started with — it holds that `Arc` for its whole body — and
    /// subsequent checks see the new one. Unlike the `RwLock` this replaced,
    /// the reload does not wait for in-flight checks to finish.
    pub async fn reload_acf_from(&self, path: &std::path::Path) -> CaResult<()> {
        let cfg = access_security::read_acf(path).await?;
        self.acf.install_async(cfg).await;
        // Bump the shared ACL generation so monitor tasks
        // spawned on the default `PvDatabaseSource` (which captured
        // this counter at spawn time) detect the change on their
        // next event and re-check ACL — peers that the new policy
        // denies see their subscriptions torn down with a MONITOR
        // FINISH frame, matching CA `reeval_access_rights`
        // semantics.
        self.acl_version
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    /// Clear the Access Security policy at runtime (returns the
    /// server to unrestricted PUT/GET/MONITOR mode). Mirrors the
    /// negative form of `reload_acf_from`.
    pub async fn clear_acf(&self) {
        self.acf.store(None);
        self.acl_version
            .fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    pub fn database(&self) -> &Arc<PvDatabase> {
        &self.db
    }

    pub async fn add_pv(&self, name: &str, initial: EpicsValue) -> CaResult<()> {
        self.db.add_pv(name, initial).await
    }

    pub async fn put(&self, name: &str, value: EpicsValue) -> CaResult<()> {
        self.db.put_pv(name, value).await
    }

    pub async fn get(&self, name: &str) -> CaResult<EpicsValue> {
        self.db.get_pv(name)
    }

    /// Run with the default [`PvDatabaseSource`].
    ///
    /// The default source is constructed with the builder-supplied
    /// ACF (if any) so PUTs are gated through Access Security in
    /// the same way as the CA server. Callers that supply their own
    /// source via [`Self::run_with_source`] are responsible for
    /// installing ACF themselves.
    pub async fn run(&self) -> CaResult<()> {
        self.run_reporting(None).await
    }

    /// As [`Self::run`], but publishes a [`ServerReportHandle`] into
    /// `report_tx` the instant the native server binds.
    ///
    /// The iocsh `pvxsr` command does NOT ride on this channel: it is
    /// registered by a registrar before any server exists, so it reads
    /// `iocsh::publish_pvxs_report`, which every run path fills whether or
    /// not a `report_tx` was supplied. This parameter is for a caller that
    /// needs the handle in its own code.
    ///
    /// Paired with `from_parts(db, 0, ..)` this is **bind-and-read-back**: the
    /// handle's `report()` names the TCP/UDP ports the kernel actually
    /// assigned, so a caller that must not guess a port (or probe one and
    /// re-bind it later — a PVA search-port collision is silent, see
    /// `crate::server_native::udp::bind_udp`) can learn them the only way
    /// that is race-free. `epics-oracle-rs`'s differential harness boots its
    /// Rust PVA side through here.
    pub async fn run_reporting(
        &self,
        report_tx: Option<watch::Sender<Option<ServerReportHandle>>>,
    ) -> CaResult<()> {
        let source = Arc::new(PvDatabaseSource::new_with_acf_and_version(
            self.db.clone(),
            self.acf.clone(),
            self.acl_version.clone(),
        ));
        self.run_with_source_inner(source, report_tx).await
    }

    /// Run with a caller-supplied [`ChannelSource`] (e.g. qsrv group source).
    pub async fn run_with_source<S: ChannelSource + 'static>(
        &self,
        source: Arc<S>,
    ) -> CaResult<()> {
        self.run_with_source_inner(source, None).await
    }

    /// As [`Self::run_with_source`], but publishes a
    /// [`ServerReportHandle`] into `report_tx` the instant the native
    /// server binds its listeners (so the actually-bound ports are known).
    /// Only a caller that needs the handle itself passes `Some`; the iocsh
    /// `pvxsr` command reads `iocsh::publish_pvxs_report`, which this fills
    /// on every path.
    async fn run_with_source_inner<S: ChannelSource + 'static>(
        &self,
        source: Arc<S>,
        report_tx: Option<watch::Sender<Option<ServerReportHandle>>>,
    ) -> CaResult<()> {
        // The environment is the base (pvxs `Config::applyEnv`); an
        // explicit port overrides the TCP port alone. The search port is
        // `EPICS_PVAS_BROADCAST_PORT`, then `EPICS_PVA_BROADCAST_PORT`, then
        // 5076 (pvxs `config.cpp:410-415`), whatever the TCP port is.
        let mut config = PvaServerConfig::default().with_env();
        if let Some(port) = self.port {
            config.tcp_port = port;
            // An explicit `0` asks for a server on no fixed port, so the
            // search port goes ephemeral with it and two such servers on one
            // host never share one. `EPICS_PVAS_SERVER_PORT=0` is not this: it
            // reaches here as `None` and leaves the search port alone, as
            // softIocPVX 1.5.3 keeps 5076.
            if port == 0 {
                config.udp_port = 0;
            }
        }

        // NOTE: no scan scheduler here. Scanning (and the PINI=YES pass)
        // is owned by the IOC core — `epics_base_rs::server::scan::
        // ScanOwner`, started by `IocApplication::run` at the C `scanRun`
        // point or by the IOC entry binary — never by a protocol server.

        // Every `run*` entry point funnels through here, so this is the one
        // place the server needs the capability: the autosave tail is a
        // reactor-bound task and says so in `AutosaveManager::start`.
        let reactor = epics_base_rs::runtime::task::Reactor::current()
            .expect("the PVA server's run loop is awaited on an executor");
        let autosave_handle = if let Some(ref mgr) = self.autosave_manager {
            Some(mgr.clone().start(&reactor, self.db.clone()))
        } else if let Some(ref cfg) = self.autosave_config {
            let builder = autosave::AutosaveBuilder::new().add_set(cfg.clone());
            // `build` cannot fail: a set it could not construct is reported
            // on the error log and carried as that set's error status, so
            // there is nothing left here to abandon the autosave task for.
            Some(Arc::new(builder.build().await).start(&reactor, self.db.clone()))
        } else {
            None
        };

        let result = crate::server_native::runtime::run_pva_server_reporting(
            source,
            config,
            move |handle| {
                // pvxs `initialisePvxsServer` filling the `server()`
                // singleton its already-registered `pvxsr` reads. Every
                // caller publishes, shell or not, because the command may
                // have been registered by `iocsh::register_pvxs_commands`
                // at an application head this server knows nothing about.
                super::iocsh::publish_pvxs_report(handle.clone());
                // The same function publishes the effective TCP port into the
                // environment, as pvxs `initialisePvxsServer` does: recsync and
                // any later `$(PVAS_SERVER_PORT)` expansion in `st.cmd` read it
                // from there. It has to be the bound port and not the requested
                // one, because an ephemeral (`0`) or EADDRINUSE-fallen-back bind
                // is exactly when a reader cannot derive it. The second name is
                // pvAccessCPP's, which pvxs sets too so either stack answers.
                let portstr = handle.bound_tcp_port().to_string();
                for name in ["PVXS_SERVER_PORT", "PVAS_SERVER_PORT"] {
                    epics_base_rs::runtime::env::set(name, &portstr);
                }
                if let Some(tx) = report_tx {
                    // Best-effort: a dropped receiver (no shell) just
                    // means nobody is watching the report.
                    let _ = tx.send(Some(handle));
                }
                // Last in the callback, after everything the bound half owes:
                // bound, published, report handed out, and about to serve —
                // the PVA layer's half of what C's `dbRunServers()` returns
                // having done. Every `run*` entry funnels through here, so it
                // is the one place the layer can say it, and `BuiltIoc::run`
                // holds the startup script's `iocInit` line until it has.
                epics_base_rs::server::db_server::announce_serving();
            },
        )
        .await
        .map_err(|e| CaError::InvalidValue(e.to_string()));

        if let Some(h) = autosave_handle {
            h.abort();
        }
        result
    }

    pub async fn run_with_shell<F>(self, register_fn: F) -> CaResult<()>
    where
        F: FnOnce(&iocsh::IocShell) + Send + 'static,
    {
        let db = self.db.clone();
        let acf = self.acf.clone();
        let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();

        let autosave_cmds = self
            .autosave_manager
            .as_ref()
            .map(|mgr| autosave::iocsh::autosave_commands(mgr.clone()));

        let server = Arc::new(self);

        let server_clone = server.clone();
        let server_handle = bridge
            .reactor()
            .spawn(async move { announce_server_failure(server_clone.run_reporting(None).await) });

        let (tx, rx) = epics_base_rs::runtime::sync::oneshot::channel();
        std::thread::spawn(move || {
            // C runs `iocsh()` on the thread `epicsThreadInit` listed as
            // `_main_` (`osdThread.c:406-412`); this driver runs it here.
            epics_base_rs::runtime::task::register_main_thread();
            // Administer this server's live policy cell so an interactive
            // `asInit` is a real ACF (re)load, not a dead-end.
            let shell = iocsh::IocShell::new_with_acf(db, bridge, acf);
            register_fn(&shell);
            if let Some(cmds) = autosave_cmds {
                for cmd in cmds {
                    shell.register(cmd);
                }
            }
            shell.register(super::iocsh::pvxsr_command());
            let result = shell.run_repl();
            let _ = tx.send(result);
        });

        let shell_result = rx.await;

        server_handle.abort();
        let _ = server_handle.await;

        match shell_result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                eprintln!("shell error: {e}");
                Err(CaError::InvalidValue(e))
            }
            Err(_) => {
                eprintln!("shell thread dropped unexpectedly");
                Err(CaError::InvalidValue("shell thread dropped".to_string()))
            }
        }
    }

    pub async fn run_with_source_and_shell<S, F>(
        self,
        source: Arc<S>,
        register_fn: F,
    ) -> CaResult<()>
    where
        S: ChannelSource + 'static,
        F: FnOnce(&iocsh::IocShell) + Send + 'static,
    {
        let db = self.db.clone();
        let acf = self.acf.clone();
        let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();

        let autosave_cmds = self
            .autosave_manager
            .as_ref()
            .map(|mgr| autosave::iocsh::autosave_commands(mgr.clone()));

        let server = Arc::new(self);

        let server_clone = server.clone();
        let server_handle = bridge.reactor().spawn(async move {
            announce_server_failure(server_clone.run_with_source_inner(source, None).await)
        });

        let (tx, rx) = epics_base_rs::runtime::sync::oneshot::channel();
        std::thread::spawn(move || {
            // C runs `iocsh()` on the thread `epicsThreadInit` listed as
            // `_main_` (`osdThread.c:406-412`); this driver runs it here.
            epics_base_rs::runtime::task::register_main_thread();
            // Same as `run_with_shell`: the shell administers this
            // server's live policy cell.
            let shell = iocsh::IocShell::new_with_acf(db, bridge, acf);
            register_fn(&shell);
            if let Some(cmds) = autosave_cmds {
                for cmd in cmds {
                    shell.register(cmd);
                }
            }
            shell.register(super::iocsh::pvxsr_command());
            let result = shell.run_repl();
            let _ = tx.send(result);
        });

        let shell_result = rx.await;

        server_handle.abort();
        let _ = server_handle.await;

        match shell_result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                eprintln!("shell error: {e}");
                Err(CaError::InvalidValue(e))
            }
            Err(_) => {
                eprintln!("shell thread dropped unexpectedly");
                Err(CaError::InvalidValue("shell thread dropped".to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `from_parts(db, Some(0), ..)` must bind BOTH ports ephemerally and report
    /// what it got.
    ///
    /// `PvaServerBuilder::port` documents `Some(0)` as "an ephemeral bind",
    /// but the `udp_port = port + 1` derivation turned it into a request for
    /// privileged UDP port **1** — so the single configuration meaning
    /// "kernel, pick both" was the one that could not work.
    ///
    /// A caller needs this path precisely when it must not guess a port, and
    /// for PVA that is not a preference: the search socket sets SO_REUSEPORT,
    /// so two servers on one port bind silently and answer searches at random
    /// (no error, unlike CA's `cas WARNING`). Reading the port back off the
    /// bind is the only race-free way to learn it.
    #[tokio::test]
    async fn ephemeral_port_zero_reports_both_bound_ports() {
        let db = Arc::new(PvDatabase::new());
        let server = PvaServer::from_parts(
            db,
            Some(0),
            epics_base_rs::server::access_security::new_acf_cell(None),
            None,
            None,
        );

        let (tx, mut rx) = watch::channel(None);
        let run = tokio::spawn(async move { server.run_reporting(Some(tx)).await });

        let handle = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Some(h) = rx.borrow_and_update().clone() {
                    return h;
                }
                if rx.changed().await.is_err() {
                    panic!("server exited without reporting bound ports");
                }
            }
        })
        .await
        .expect("server must report its bound ports");

        let report = handle.report();
        assert_ne!(report.tcp_port, 0, "TCP port 0 must resolve to a real port");
        assert_ne!(report.udp_port, 0, "UDP port 0 must resolve to a real port");
        assert_ne!(
            report.udp_port, 1,
            "udp_port = tcp_port + 1 must not apply to the ephemeral sentinel: \
             port 1 is privileged and is not what `Some(0)` asks for",
        );

        // The port published into the environment is the BOUND one. The
        // ephemeral bind is the case that proves it: a reader cannot derive
        // this number from the request, which was `0`.
        for name in ["PVXS_SERVER_PORT", "PVAS_SERVER_PORT"] {
            assert_eq!(
                std::env::var(name).ok().as_deref(),
                Some(report.tcp_port.to_string().as_str()),
                "{name} must carry the bound TCP port (pvxs initialisePvxsServer)"
            );
        }
        assert_eq!(handle.bound_tcp_port(), report.tcp_port);
        run.abort();
    }

    /// The search port is `EPICS_PVAS_BROADCAST_PORT`'s whatever TCP port the
    /// caller passes (pvxs `config.cpp:402-415` reads the two apart);
    /// softIocPVX 1.5.3 with 5281/5287 binds TCP 5281 and UDP 5287. The
    /// pvxget half is `interop_pvxs::server_ports_env`.
    #[tokio::test]
    #[serial_test::serial(epics_env)]
    async fn search_port_is_the_broadcast_port_not_tcp_plus_one() {
        let tcp = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let udp = std::net::UdpSocket::bind("0.0.0.0:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        // SAFETY: `serial(epics_env)` holds every env-touching test off.
        unsafe { std::env::set_var("EPICS_PVAS_BROADCAST_PORT", udp.to_string()) };
        let server = PvaServer::from_parts(
            Arc::new(PvDatabase::new()),
            Some(tcp),
            epics_base_rs::server::access_security::new_acf_cell(None),
            None,
            None,
        );
        let (tx, mut rx) = watch::channel(None);
        let run = tokio::spawn(async move { server.run_reporting(Some(tx)).await });
        let report = loop {
            if let Some(h) = rx.borrow_and_update().clone() {
                break h.report();
            }
            rx.changed().await.expect("server reports its ports");
        };
        run.abort();
        unsafe { std::env::remove_var("EPICS_PVAS_BROADCAST_PORT") };
        assert_eq!((report.tcp_port, report.udp_port), (tcp, udp));
    }

    /// `EPICS_PVAS_SERVER_PORT=0` makes the TCP port ephemeral and nothing
    /// else: softIocPVX 1.5.3 under it listens on an ephemeral TCP port and
    /// still searches on 5076 (here, `EPICS_PVAS_BROADCAST_PORT`'s).
    #[tokio::test]
    #[serial_test::serial(epics_env)]
    async fn env_server_port_zero_leaves_the_search_port() {
        let udp = std::net::UdpSocket::bind("0.0.0.0:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        // SAFETY: `serial(epics_env)` holds every env-touching test off.
        unsafe {
            std::env::set_var("EPICS_PVAS_SERVER_PORT", "0");
            std::env::set_var("EPICS_PVAS_BROADCAST_PORT", udp.to_string());
        }
        let server = PvaServer::from_parts(
            Arc::new(PvDatabase::new()),
            None,
            epics_base_rs::server::access_security::new_acf_cell(None),
            None,
            None,
        );
        let (tx, mut rx) = watch::channel(None);
        let run = tokio::spawn(async move { server.run_reporting(Some(tx)).await });
        let report = loop {
            if let Some(h) = rx.borrow_and_update().clone() {
                break h.report();
            }
            rx.changed().await.expect("server reports its ports");
        };
        run.abort();
        unsafe {
            std::env::remove_var("EPICS_PVAS_SERVER_PORT");
            std::env::remove_var("EPICS_PVAS_BROADCAST_PORT");
        }
        assert_ne!(report.tcp_port, 0);
        assert_eq!(report.udp_port, udp);
    }

    /// An explicit port still wins with `EPICS_PVAS_INTF_ADDR_LIST` set: a
    /// bare interface entry keeps port 0 and takes the resolved `tcp_port`
    /// at bind. qsrv-rs `--port`, dual-ioc-rs `--pva-port` and oracle_ioc's
    /// `Some(0)` reach `from_parts`; the builder sets the same field.
    #[tokio::test]
    #[serial_test::serial(epics_env)]
    async fn explicit_port_survives_the_interface_list() {
        async fn bound_tcp(server: PvaServer) -> u16 {
            let (tx, mut rx) = watch::channel(None);
            let run = tokio::spawn(async move { server.run_reporting(Some(tx)).await });
            let port = loop {
                if let Some(h) = rx.borrow_and_update().clone() {
                    break h.report().tcp_port;
                }
                rx.changed().await.expect("server reports its ports");
            };
            run.abort();
            let _ = run.await;
            port
        }
        let from_parts = |port| {
            PvaServer::from_parts(
                Arc::new(PvDatabase::new()),
                Some(port),
                epics_base_rs::server::access_security::new_acf_cell(None),
                None,
                None,
            )
        };
        let free = || {
            std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port()
        };
        let udp = std::net::UdpSocket::bind("0.0.0.0:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        // SAFETY: `serial(epics_env)` holds every env-touching test off.
        unsafe {
            std::env::remove_var("EPICS_PVAS_SERVER_PORT");
            std::env::set_var("EPICS_PVAS_INTF_ADDR_LIST", "127.0.0.1");
            std::env::set_var("EPICS_PVAS_BROADCAST_PORT", udp.to_string());
        }
        let explicit = free();
        let got_explicit = bound_tcp(from_parts(explicit)).await;
        let got_zero = bound_tcp(from_parts(0)).await;
        let built = free();
        let got_built = bound_tcp(PvaServer::builder().port(built).build().await.unwrap()).await;
        unsafe {
            std::env::remove_var("EPICS_PVAS_INTF_ADDR_LIST");
            std::env::remove_var("EPICS_PVAS_BROADCAST_PORT");
        }
        assert_eq!(got_explicit, explicit);
        assert!(
            got_zero != 0 && got_zero != 5075,
            "Some(0) bound {got_zero}"
        );
        assert_eq!(got_built, built);
    }

    /// C `asInit` returns only once `asCaStart` has the inputs' first readings
    /// (`asCaTaskWait`), so a check right after a reload sees them, and the
    /// wait parks no thread, so it completes on the watcher's own band.
    #[epics_macros_rs::epics_test]
    async fn reload_acf_from_returns_with_the_first_readings_in_place() {
        let db = Arc::new(PvDatabase::new());
        db.add_record(
            "rv",
            Box::new(epics_base_rs::server::records::ai::AiRecord::new(5.0)),
        )
        .await
        .unwrap();
        db.put_record_field_from_ca("rv", "VAL", epics_base_rs::types::EpicsValue::Double(5.0))
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.acf");
        std::fs::write(
            &path,
            r#"ASG(DEFAULT) { INPA(rv) RULE(1,READ) { CALC("A>1") } }"#,
        )
        .unwrap();
        let acf = epics_base_rs::server::access_security::new_acf_cell_watching(None, &db);
        let server = PvaServer::from_parts(db.clone(), Some(0), acf.clone(), None, None);
        // Hold `rv`'s lock across the reload: the first reading is in place at
        // the assert only if the reload waited for the watcher to take it.
        let (locked, held) = std::sync::mpsc::channel();
        let rec = db.get_record("rv").unwrap();
        let holder = std::thread::spawn(move || {
            let _guard = rec.write();
            locked.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(200));
        });
        held.recv().unwrap();
        // From cbMedium, the INP watcher's band: an install that parks its
        // thread there waits on a watcher that cannot run.
        let reload = epics_base_rs::runtime::task::spawn_background(
            epics_base_rs::runtime::task::CallbackPriority::Medium,
            async move { server.reload_acf_from(&path).await },
        );
        epics_base_rs::runtime::task::timeout(std::time::Duration::from_secs(5), reload)
            .await
            .expect("the reload parked cbMedium")
            .unwrap()
            .unwrap();
        assert_eq!(
            acf.load_full().unwrap().asg[b"DEFAULT".as_slice()].inp_bad(),
            0
        );
        holder.join().unwrap();
    }
}
