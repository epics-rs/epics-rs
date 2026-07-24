//! PVA server wrapper — mirrors the `CaServer` pattern for pvAccess.
//!
//! Built on top of the native runtime in [`crate::server_native`].

// RTEMS-EXEC-MODEL-ALLOW(1): checked - these run and pass in the feature-ON suite.

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

impl PvaServerBuilder {
    pub fn new() -> Self {
        Self {
            ioc: ioc_builder::IocBuilder::new(),
            port: None,
            acf: None,
        }
    }

    /// Set the TCP port (UDP = port + 1), overriding the environment.
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
        let (db, autosave_config) = self.ioc.build().await?;
        let acf = epics_base_rs::server::access_security::new_acf_cell(self.acf);
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

    pub fn from_parts(
        db: Arc<PvDatabase>,
        port: u16,
        acf: Option<access_security::AccessSecurityConfig>,
        autosave_config: Option<autosave::SaveSetConfig>,
        autosave_manager: Option<Arc<autosave::AutosaveManager>>,
    ) -> Self {
        Self {
            db,
            port: Some(port),
            acf: epics_base_rs::server::access_security::new_acf_cell(acf),
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
        let content = std::fs::read_to_string(path).map_err(epics_base_rs::error::CaError::Io)?;
        let cfg = access_security::parse_acf(&content)?;
        self.acf.store(Some(std::sync::Arc::new(cfg)));
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
    /// `report_tx` the instant the native server binds. Backs the iocsh
    /// `pvxsr` command on the default-source shell path
    /// ([`Self::run_with_shell`]).
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
    /// server binds its listeners (so the actually-bound ports are
    /// known). [`Self::run_with_source_and_shell`] uses this to back the
    /// iocsh `pvxsr` command; the plain [`Self::run_with_source`] passes
    /// `None`.
    async fn run_with_source_inner<S: ChannelSource + 'static>(
        &self,
        source: Arc<S>,
        report_tx: Option<watch::Sender<Option<ServerReportHandle>>>,
    ) -> CaResult<()> {
        // The environment is the base (pvxs `Config::applyEnv`); an
        // explicit `.port(p)` overrides it, keeping the port/port+1
        // derivation this builder has always used.
        let mut config = PvaServerConfig::default().with_env();
        if let Some(port) = self.port {
            config.tcp_port = port;
            // `0` is the ephemeral sentinel (`PvaServerBuilder::port` documents
            // `Some(0)` as "an ephemeral bind"), not a base to offset from:
            // `0 + 1` asked for the *privileged* UDP port 1, so the one
            // configuration that means "kernel, pick both" was the one that
            // could not work. Ephemeral TCP implies ephemeral UDP.
            config.udp_port = if port == 0 { 0 } else { port + 1 };
        }

        // NOTE: no scan scheduler here. Scanning (and the PINI=YES pass)
        // is owned by the IOC core — `epics_base_rs::server::scan::
        // ScanOwner`, started by `IocApplication::run` at the C `scanRun`
        // point or by the IOC entry binary — never by a protocol server.

        let autosave_handle = if let Some(ref mgr) = self.autosave_manager {
            Some(mgr.clone().start(self.db.clone()))
        } else if let Some(ref cfg) = self.autosave_config {
            let builder = autosave::AutosaveBuilder::new().add_set(cfg.clone());
            match builder.build().await {
                Ok(mgr) => Some(Arc::new(mgr).start(self.db.clone())),
                Err(e) => {
                    eprintln!("autosave: failed to start: {e}");
                    None
                }
            }
        } else {
            None
        };

        let result = crate::server_native::runtime::run_pva_server_reporting(
            source,
            config,
            move |handle| {
                if let Some(tx) = report_tx {
                    // Best-effort: a dropped receiver (no shell) just
                    // means nobody is watching the report.
                    let _ = tx.send(Some(handle));
                }
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
        let handle = tokio::runtime::Handle::current();

        let autosave_cmds = self
            .autosave_manager
            .as_ref()
            .map(|mgr| autosave::iocsh::autosave_commands(mgr.clone()));

        let server = Arc::new(self);

        let (report_tx, report_rx) = watch::channel(None);
        let server_clone = server.clone();
        let server_handle = epics_base_rs::runtime::task::spawn(async move {
            server_clone.run_reporting(Some(report_tx)).await
        });

        let (tx, rx) = epics_base_rs::runtime::sync::oneshot::channel();
        std::thread::spawn(move || {
            let shell = iocsh::IocShell::new(db, handle);
            register_fn(&shell);
            if let Some(cmds) = autosave_cmds {
                for cmd in cmds {
                    shell.register(cmd);
                }
            }
            shell.register(super::iocsh::pvxsr_command(report_rx));
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
        let handle = tokio::runtime::Handle::current();

        let autosave_cmds = self
            .autosave_manager
            .as_ref()
            .map(|mgr| autosave::iocsh::autosave_commands(mgr.clone()));

        let server = Arc::new(self);

        let (report_tx, report_rx) = watch::channel(None);
        let server_clone = server.clone();
        let server_handle = epics_base_rs::runtime::task::spawn(async move {
            server_clone
                .run_with_source_inner(source, Some(report_tx))
                .await
        });

        let (tx, rx) = epics_base_rs::runtime::sync::oneshot::channel();
        std::thread::spawn(move || {
            let shell = iocsh::IocShell::new(db, handle);
            register_fn(&shell);
            if let Some(cmds) = autosave_cmds {
                for cmd in cmds {
                    shell.register(cmd);
                }
            }
            shell.register(super::iocsh::pvxsr_command(report_rx));
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

    /// `from_parts(db, 0, ..)` must bind BOTH ports ephemerally and report
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
        let server = PvaServer::from_parts(db, 0, None, None, None);

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
        run.abort();
    }
}
