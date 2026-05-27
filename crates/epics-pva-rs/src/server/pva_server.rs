//! PVA server wrapper — mirrors the `CaServer` pattern for pvAccess.
//!
//! Built on top of the native runtime in [`crate::server_native`]. No
//! `spvirit_server` dependency.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::scan::ScanScheduler;
use epics_base_rs::server::{access_security, autosave, iocsh};
use epics_base_rs::types::EpicsValue;

use crate::server_native::{ChannelSource, PvaServerConfig, run_pva_server};

use super::native_source::PvDatabaseSource;

// ── Builder ──────────────────────────────────────────────────────────────

/// Builder for constructing a [`PvaServer`] with simple PVs and/or records.
pub struct PvaServerBuilder {
    ioc: ioc_builder::IocBuilder,
    port: u16,
    acf: Option<access_security::AccessSecurityConfig>,
}

impl PvaServerBuilder {
    pub fn new() -> Self {
        Self {
            ioc: ioc_builder::IocBuilder::new(),
            port: epics_base_rs::runtime::net::PVA_SERVER_PORT,
            acf: None,
        }
    }

    /// Set the TCP port (UDP = port + 1).
    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
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
        let acf = Arc::new(tokio::sync::RwLock::new(self.acf));
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
    port: u16,
    /// Access Security configuration. Forwarded to the default
    /// `PvDatabaseSource` in `run()` so PVA PUTs are gated through
    /// `check_access_method`. Callers that supply their own
    /// ChannelSource via `run_with_source` must install ACF
    /// themselves.
    ///
    /// `RwLock`-wrapped so [`Self::reload_acf_from`] can
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
            port,
            acf: Arc::new(tokio::sync::RwLock::new(acf)),
            acl_version: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            autosave_config,
            autosave_manager,
        }
    }

    /// Reload the Access Security policy from a `.acf` file. Mirrors
    /// `CaServer::reload_acf_from`. Parses the file off the async
    /// runtime (blocking IO; small file) and then swaps the AcfCell
    /// under a write guard so in-flight ACF checks finish under the
    /// old policy and subsequent checks see the new one.
    pub async fn reload_acf_from(&self, path: &std::path::Path) -> CaResult<()> {
        let content = std::fs::read_to_string(path).map_err(epics_base_rs::error::CaError::Io)?;
        let cfg = access_security::parse_acf(&content)?;
        *self.acf.write().await = Some(cfg);
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
        *self.acf.write().await = None;
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
        self.db.get_pv(name).await
    }

    /// Run with the default [`PvDatabaseSource`].
    ///
    /// The default source is constructed with the builder-supplied
    /// ACF (if any) so PUTs are gated through Access Security in
    /// the same way as the CA server. Callers that supply their own
    /// source via [`Self::run_with_source`] are responsible for
    /// installing ACF themselves.
    pub async fn run(&self) -> CaResult<()> {
        let source = Arc::new(PvDatabaseSource::new_with_acf_and_version(
            self.db.clone(),
            self.acf.clone(),
            self.acl_version.clone(),
        ));
        self.run_with_source(source).await
    }

    /// Run with a caller-supplied [`ChannelSource`] (e.g. qsrv group source).
    pub async fn run_with_source<S: ChannelSource + 'static>(
        &self,
        source: Arc<S>,
    ) -> CaResult<()> {
        let config = PvaServerConfig {
            tcp_port: self.port,
            udp_port: self.port + 1,
            ..Default::default()
        };

        let scanner = ScanScheduler::new(self.db.clone());

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

        let result = tokio::select! {
            res = run_pva_server(source, config) => res.map_err(|e| CaError::InvalidValue(e.to_string())),
            _ = scanner.run() => {
                eprintln!("Scan scheduler exited");
                Ok(())
            }
        };

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

        let server_clone = server.clone();
        let server_handle =
            epics_base_rs::runtime::task::spawn(async move { server_clone.run().await });

        let (tx, rx) = epics_base_rs::runtime::sync::oneshot::channel();
        std::thread::spawn(move || {
            let shell = iocsh::IocShell::new(db, handle);
            register_fn(&shell);
            if let Some(cmds) = autosave_cmds {
                for cmd in cmds {
                    shell.register(cmd);
                }
            }
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

        let server_clone = server.clone();
        let server_handle = epics_base_rs::runtime::task::spawn(async move {
            server_clone.run_with_source(source).await
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
