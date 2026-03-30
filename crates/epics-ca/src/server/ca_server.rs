//! Channel Access server — CaServer and CaServerBuilder.
//!
//! This module will eventually move to the `epics-ca` crate.

use std::sync::Arc;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::runtime::net::CA_SERVER_PORT;
use epics_base_rs::server::record::{self, Record, SubroutineFn};
use epics_base_rs::types::EpicsValue;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::scan::ScanScheduler;
use epics_base_rs::server::{access_security, autosave, db_loader, device_support, iocsh};
use epics_base_rs::server::DeviceSupportFactory;
use super::{tcp, udp, beacon};

/// Builder for CaServer configuration.
pub struct CaServerBuilder {
    port: u16,
    pvs: Vec<(String, EpicsValue)>,
    records: Vec<(String, Box<dyn Record>)>,
    db_defs: Vec<db_loader::DbRecordDef>,
    device_factories: std::collections::HashMap<String, DeviceSupportFactory>,
    record_factories: std::collections::HashMap<String, epics_base_rs::server::RecordFactory>,
    subroutine_registry: std::collections::HashMap<String, Arc<SubroutineFn>>,
    acf: Option<access_security::AccessSecurityConfig>,
    autosave_config: Option<autosave::AutosaveConfig>,
}

impl CaServerBuilder {
    pub fn new() -> Self {
        Self {
            port: CA_SERVER_PORT,
            pvs: Vec::new(),
            records: Vec::new(),
            db_defs: Vec::new(),
            device_factories: std::collections::HashMap::new(),
            record_factories: std::collections::HashMap::new(),
            subroutine_registry: std::collections::HashMap::new(),
            acf: None,
            autosave_config: None,
        }
    }

    /// Set the port for both UDP and TCP (default: 5064).
    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Add a simple PV to be created on server start.
    pub fn pv(mut self, name: &str, initial: EpicsValue) -> Self {
        self.pvs.push((name.to_string(), initial));
        self
    }

    /// Add a record to be created on server start.
    pub fn record(mut self, name: &str, record: impl Record) -> Self {
        self.records.push((name.to_string(), Box::new(record)));
        self
    }

    /// Add a pre-boxed record to be created on server start.
    pub fn record_boxed(mut self, name: &str, record: Box<dyn Record>) -> Self {
        self.records.push((name.to_string(), record));
        self
    }

    /// Load records from a .db file.
    pub fn db_file(
        mut self,
        path: &str,
        macros: &std::collections::HashMap<String, String>,
    ) -> CaResult<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| CaError::Io(e))?;
        let defs = db_loader::parse_db(&content, macros)?;
        self.db_defs.extend(defs);
        Ok(self)
    }

    /// Register a device support factory by DTYP name.
    pub fn register_device_support<F>(mut self, dtyp: &str, factory: F) -> Self
    where
        F: Fn() -> Box<dyn device_support::DeviceSupport> + Send + Sync + 'static,
    {
        self.device_factories.insert(dtyp.to_string(), Box::new(factory));
        self
    }

    /// Load an access security configuration file.
    pub fn acf_file(mut self, path: &str) -> CaResult<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(CaError::Io)?;
        self.acf = Some(access_security::parse_acf(&content)?);
        Ok(self)
    }

    /// Configure autosave.
    pub fn autosave(mut self, config: autosave::AutosaveConfig) -> Self {
        self.autosave_config = Some(config);
        self
    }

    /// Set access security configuration directly.
    pub fn acf(mut self, config: access_security::AccessSecurityConfig) -> Self {
        self.acf = Some(config);
        self
    }

    /// Register a subroutine function by name (for sub records).
    pub fn register_subroutine<F>(mut self, name: &str, func: F) -> Self
    where
        F: Fn(&mut dyn Record) -> CaResult<()> + Send + Sync + 'static,
    {
        self.subroutine_registry.insert(name.to_string(), Arc::new(Box::new(func)));
        self
    }

    /// Register an external record type factory.
    pub fn register_record_type<F>(mut self, type_name: &str, factory: F) -> Self
    where
        F: Fn() -> Box<dyn Record> + Send + Sync + 'static,
    {
        self.record_factories.insert(type_name.to_string(), Box::new(factory));
        self
    }

    /// Load records from a .db string.
    pub fn db_string(
        mut self,
        content: &str,
        macros: &std::collections::HashMap<String, String>,
    ) -> CaResult<Self> {
        let defs = db_loader::parse_db(content, macros)?;
        self.db_defs.extend(defs);
        Ok(self)
    }

    /// Build the server.
    pub async fn build(self) -> CaResult<CaServer> {
        let db = Arc::new(PvDatabase::new());
        for (name, value) in self.pvs {
            db.add_pv(&name, value).await;
        }
        for (name, record) in self.records {
            db.add_record(&name, record).await;
        }
        // Process DB file definitions
        for def in self.db_defs {
            let mut record = db_loader::create_record(&def.record_type)
                .or_else(|_| {
                    self.record_factories.get(&def.record_type)
                        .map(|f| f())
                        .ok_or_else(|| CaError::DbParseError {
                            line: 0,
                            column: 0,
                            message: format!("unknown record type: '{}'", def.record_type),
                        })
                })?;
            let mut common_fields = Vec::new();
            db_loader::apply_fields(&mut record, &def.fields, &mut common_fields)?;

            db.add_record(&def.name, record).await;

            // Apply common fields and device support to the RecordInstance
            if let Some(rec_arc) = db.get_record(&def.name).await {
                let mut instance = rec_arc.write().await;
                for (name, value) in common_fields {
                    match instance.put_common_field(&name, value) {
                        Ok(record::CommonFieldPutResult::ScanChanged { old_scan, new_scan, phas }) => {
                            drop(instance);
                            db.update_scan_index(&def.name, old_scan, new_scan, phas, phas).await;
                            instance = rec_arc.write().await;
                        }
                        Ok(record::CommonFieldPutResult::PhasChanged { scan, old_phas, new_phas }) => {
                            drop(instance);
                            db.update_scan_index(&def.name, scan, scan, old_phas, new_phas).await;
                            instance = rec_arc.write().await;
                        }
                        Ok(record::CommonFieldPutResult::NoChange) => {}
                        Err(e) => {
                            eprintln!("put_common_field({name}) failed for {}: {e}", def.name);
                        }
                    }
                }
                // TODO: refactor to global two-pass if inter-record init dependencies arise
                if let Err(e) = instance.record.init_record(0) {
                    eprintln!("init_record(0) failed for {}: {e}", def.name);
                }
                if let Err(e) = instance.record.init_record(1) {
                    eprintln!("init_record(1) failed for {}: {e}", def.name);
                }

                // Set up device support based on DTYP
                let dtyp = instance.common.dtyp.clone();
                if !dtyp.is_empty() && dtyp != "Soft Channel" {
                    if let Some(factory) = self.device_factories.get(&dtyp) {
                        let mut dev = factory();
                        let _ = dev.init(&mut *instance.record);
                        dev.set_record_info(&def.name, instance.common.scan);
                        instance.device = Some(dev);
                    }
                }
                // Resolve subroutine for sub records
                if instance.record.record_type() == "sub" {
                    if let Some(EpicsValue::String(snam)) = instance.record.get_field("SNAM") {
                        if let Some(sub_fn) = self.subroutine_registry.get(&snam) {
                            instance.subroutine = Some(sub_fn.clone());
                        }
                    }
                }
            }
        }
        let acf = Arc::new(self.acf);

        // Restore from autosave file if configured
        if let Some(ref autosave_cfg) = self.autosave_config {
            let count = autosave::restore_from_file(&db, &autosave_cfg.save_path).await?;
            if count > 0 {
                eprintln!("autosave: restored {count} PVs");
            }
        }

        // I/O Intr: collect all record names first, then access individually (deadlock prevention)
        let all_names = db.all_record_names().await;
        let io_intr_recs: Vec<(String, Arc<epics_base_rs::runtime::sync::RwLock<record::RecordInstance>>)> = {
            let mut recs = Vec::new();
            for name in &all_names {
                if let Some(arc) = db.get_record(name).await {
                    recs.push((name.clone(), arc));
                }
            }
            recs
        };

        for (name, rec_arc) in io_intr_recs {
            let mut inst = rec_arc.write().await;
            if inst.common.scan == record::ScanType::IoIntr {
                if let Some(mut dev) = inst.device.take() {
                    if let Some(mut intr_rx) = dev.io_intr_receiver() {
                        let db_clone = db.clone();
                        let rec_name = name.clone();
                        let rec_arc_clone = rec_arc.clone();
                        epics_base_rs::runtime::task::spawn(async move {
                            while intr_rx.recv().await.is_some() {
                                let is_io_intr = {
                                    let inst = rec_arc_clone.read().await;
                                    inst.common.scan == record::ScanType::IoIntr
                                };
                                if !is_io_intr {
                                    continue;
                                }
                                let mut visited = std::collections::HashSet::new();
                                let _ = db_clone.process_record_with_links(
                                    &rec_name, &mut visited, 0).await;
                            }
                        });
                    }
                    inst.device = Some(dev);
                }
            }
        }

        Ok(CaServer { db, port: self.port, acf, autosave_config: self.autosave_config, autosave_manager: None })
    }
}

/// A Channel Access server (IOC) that hosts process variables.
pub struct CaServer {
    db: Arc<PvDatabase>,
    port: u16,
    acf: Arc<Option<access_security::AccessSecurityConfig>>,
    autosave_config: Option<autosave::AutosaveConfig>,
    autosave_manager: Option<Arc<autosave::AutosaveManager>>,
}

impl CaServer {
    /// Create a builder for configuring the server.
    pub fn builder() -> CaServerBuilder {
        CaServerBuilder::new()
    }

    /// Construct a CaServer from pre-populated parts.
    /// Used by [`ioc_app::IocApplication`] after st.cmd execution and device support wiring.
    pub(crate) fn from_parts(
        db: Arc<PvDatabase>,
        port: u16,
        acf: Option<access_security::AccessSecurityConfig>,
        autosave_config: Option<autosave::AutosaveConfig>,
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

        let autosave_cmds = self.autosave_manager.as_ref()
            .map(|mgr| autosave::iocsh::autosave_commands(mgr.clone()));

        let server = Arc::new(self);

        let server_clone = server.clone();
        let server_handle = epics_base_rs::runtime::task::spawn(async move {
            server_clone.run().await
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

    /// Add a simple PV at runtime.
    pub async fn add_pv(&self, name: &str, initial: EpicsValue) {
        self.db.add_pv(name, initial).await;
    }

    /// Add a record at runtime.
    pub async fn add_record(&self, name: &str, record: impl Record) {
        self.db
            .add_record(name, Box::new(record))
            .await;
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

        // Spawn autosave: prefer manager (from startup config), fall back to legacy
        let autosave_handle = if let Some(ref mgr) = self.autosave_manager {
            let mgr = mgr.clone();
            let db_save = self.db.clone();
            Some(mgr.start(db_save))
        } else if let Some(ref cfg) = self.autosave_config {
            let db_save = self.db.clone();
            let cfg = cfg.clone();
            Some(epics_base_rs::runtime::task::spawn(async move {
                autosave::run_autosave(db_save, cfg).await;
            }))
        } else {
            None
        };

        let (tcp_tx, tcp_rx) = tokio::sync::oneshot::channel();
        let beacon_reset = std::sync::Arc::new(tokio::sync::Notify::new());
        let beacon_reset_tcp = beacon_reset.clone();

        let tcp_handle = epics_base_rs::runtime::task::spawn(async move {
            tcp::run_tcp_listener(db_tcp, port, acf, tcp_tx, beacon_reset_tcp).await
        });

        let tcp_port = tcp_rx.await.map_err(|_| CaError::Io(
            std::io::Error::new(std::io::ErrorKind::Other, "TCP listener failed to start")
        ))?;

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
