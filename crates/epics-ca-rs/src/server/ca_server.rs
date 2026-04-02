//! Channel Access server — CaServer and CaServerBuilder.
//!
//! CaServerBuilder delegates all IOC-level bootstrap logic to
//! [`epics_base_rs::server::ioc_builder::IocBuilder`] and adds only
//! CA-specific configuration (port, access security).

use std::sync::Arc;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::runtime::net::CA_SERVER_PORT;
use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;

use super::{beacon, tcp, udp};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::scan::ScanScheduler;
use epics_base_rs::server::{access_security, autosave, device_support, ioc_builder, iocsh};

/// Builder for CaServer configuration.
///
/// IOC-level methods (`pv`, `record`, `db_file`, `register_device_support`,
/// etc.) delegate to the inner [`ioc_builder::IocBuilder`].  Only `port()`,
/// `acf()`, and `acf_file()` are CA-specific.
pub struct CaServerBuilder {
    ioc: ioc_builder::IocBuilder,
    port: u16,
    acf: Option<access_security::AccessSecurityConfig>,
}

impl CaServerBuilder {
    pub fn new() -> Self {
        Self {
            ioc: ioc_builder::IocBuilder::new(),
            port: CA_SERVER_PORT,
            acf: None,
        }
    }

    // ── CA-specific methods ──────────────────────────────────────────

    /// Set the port for both UDP and TCP (default: 5064).
    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Load an access security configuration file.
    pub fn acf_file(mut self, path: &str) -> CaResult<Self> {
        let content = std::fs::read_to_string(path).map_err(CaError::Io)?;
        self.acf = Some(access_security::parse_acf(&content)?);
        Ok(self)
    }

    /// Set access security configuration directly.
    pub fn acf(mut self, config: access_security::AccessSecurityConfig) -> Self {
        self.acf = Some(config);
        self
    }

    // ── IOC-delegated methods ────────────────────────────────────────

    /// Add a simple PV to be created on server start.
    pub fn pv(mut self, name: &str, initial: EpicsValue) -> Self {
        self.ioc = self.ioc.pv(name, initial);
        self
    }

    /// Add a record to be created on server start.
    pub fn record(mut self, name: &str, record: impl Record) -> Self {
        self.ioc = self.ioc.record(name, record);
        self
    }

    /// Add a pre-boxed record to be created on server start.
    pub fn record_boxed(mut self, name: &str, record: Box<dyn Record>) -> Self {
        self.ioc = self.ioc.record_boxed(name, record);
        self
    }

    /// Load records from a .db file.
    pub fn db_file(
        mut self,
        path: &str,
        macros: &std::collections::HashMap<String, String>,
    ) -> CaResult<Self> {
        self.ioc = self.ioc.db_file(path, macros)?;
        Ok(self)
    }

    /// Load records from a .db string.
    pub fn db_string(
        mut self,
        content: &str,
        macros: &std::collections::HashMap<String, String>,
    ) -> CaResult<Self> {
        self.ioc = self.ioc.db_string(content, macros)?;
        Ok(self)
    }

    /// Register a device support factory by DTYP name.
    pub fn register_device_support<F>(mut self, dtyp: &str, factory: F) -> Self
    where
        F: Fn() -> Box<dyn device_support::DeviceSupport> + Send + Sync + 'static,
    {
        self.ioc = self.ioc.register_device_support(dtyp, factory);
        self
    }

    /// Register an external record type factory.
    pub fn register_record_type<F>(mut self, type_name: &str, factory: F) -> Self
    where
        F: Fn() -> Box<dyn Record> + Send + Sync + 'static,
    {
        self.ioc = self.ioc.register_record_type(type_name, factory);
        self
    }

    /// Register a subroutine function by name (for sub records).
    pub fn register_subroutine<F>(mut self, name: &str, func: F) -> Self
    where
        F: Fn(&mut dyn Record) -> CaResult<()> + Send + Sync + 'static,
    {
        self.ioc = self.ioc.register_subroutine(name, func);
        self
    }

    /// Configure autosave with a save set configuration.
    pub fn autosave(mut self, config: autosave::SaveSetConfig) -> Self {
        self.ioc = self.ioc.autosave(config);
        self
    }

    /// Build the server.
    pub async fn build(self) -> CaResult<CaServer> {
        let (db, autosave_config) = self.ioc.build().await?;
        let acf = Arc::new(self.acf);
        Ok(CaServer {
            db,
            port: self.port,
            acf,
            autosave_config,
            autosave_manager: None,
        })
    }
}

/// A Channel Access server (IOC) that hosts process variables.
pub struct CaServer {
    db: Arc<PvDatabase>,
    port: u16,
    acf: Arc<Option<access_security::AccessSecurityConfig>>,
    autosave_config: Option<autosave::SaveSetConfig>,
    autosave_manager: Option<Arc<autosave::AutosaveManager>>,
}

impl CaServer {
    /// Create a builder for configuring the server.
    pub fn builder() -> CaServerBuilder {
        CaServerBuilder::new()
    }

    /// Construct a CaServer from pre-populated parts.
    /// Used by [`ioc_app::IocApplication`] after st.cmd execution and device support wiring.
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
            acf: Arc::new(acf),
            autosave_config,
            autosave_manager,
        }
    }

    /// Expose PV database for shell/external use.
    pub fn database(&self) -> &Arc<PvDatabase> {
        &self.db
    }

    /// Run server + interactive shell. Shell exit stops server.
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

    /// Add a simple PV at runtime.
    pub async fn add_pv(&self, name: &str, initial: EpicsValue) {
        self.db.add_pv(name, initial).await;
    }

    /// Add a record at runtime.
    pub async fn add_record(&self, name: &str, record: impl Record) {
        self.db.add_record(name, Box::new(record)).await;
    }

    /// Set a PV value (notifies subscribers).
    pub async fn put(&self, name: &str, value: EpicsValue) -> CaResult<()> {
        self.db.put_pv(name, value).await
    }

    /// Get a PV value.
    pub async fn get(&self, name: &str) -> CaResult<EpicsValue> {
        self.db.get_pv(name).await
    }

    /// Run the server (UDP + TCP + beacon + scan scheduler).
    /// This function runs indefinitely.
    pub async fn run(&self) -> CaResult<()> {
        let db_udp = self.db.clone();
        let db_tcp = self.db.clone();
        let db_scan = self.db.clone();
        let acf = self.acf.clone();
        let port = self.port;

        let scanner = ScanScheduler::new(db_scan);

        // Spawn autosave: prefer existing manager, otherwise build one from SaveSetConfig
        let autosave_handle = if let Some(ref mgr) = self.autosave_manager {
            let mgr = mgr.clone();
            let db_save = self.db.clone();
            Some(mgr.start(db_save))
        } else if let Some(ref cfg) = self.autosave_config {
            let builder = autosave::AutosaveBuilder::new().add_set(cfg.clone());
            match builder.build().await {
                Ok(mgr) => {
                    let mgr = Arc::new(mgr);
                    let db_save = self.db.clone();
                    Some(mgr.start(db_save))
                }
                Err(e) => {
                    eprintln!("autosave: failed to start: {e}");
                    None
                }
            }
        } else {
            None
        };

        let (tcp_tx, tcp_rx) = tokio::sync::oneshot::channel();
        let beacon_reset = std::sync::Arc::new(tokio::sync::Notify::new());
        let beacon_reset_tcp = beacon_reset.clone();

        let tcp_handle = epics_base_rs::runtime::task::spawn(async move {
            tcp::run_tcp_listener(db_tcp, port, acf, tcp_tx, beacon_reset_tcp).await
        });

        let tcp_port = tcp_rx.await.map_err(|_| {
            CaError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "TCP listener failed to start",
            ))
        })?;

        eprintln!("CA server: UDP search on port {port}, TCP on port {tcp_port}");

        let result = tokio::select! {
            r = udp::run_udp_search_responder(db_udp, port, tcp_port) => {
                eprintln!("UDP responder exited: {r:?}");
                r
            }
            r = tcp_handle => {
                eprintln!("TCP listener exited: {r:?}");
                match r {
                    Ok(inner) => inner,
                    Err(e) => Err(CaError::Io(
                        std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
                    )),
                }
            }
            r = beacon::run_beacon_emitter(tcp_port, beacon_reset) => {
                eprintln!("Beacon emitter exited: {r:?}");
                r
            }
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
}
