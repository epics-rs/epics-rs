//! IOC Application — st.cmd-style startup for Rust IOCs.
//!
//! Provides a 2-phase IOC lifecycle matching the C++ EPICS pattern:
//!
//! **Phase 1 (pre-init):** Execute startup script (`st.cmd`)
//!   - `epicsEnvSet`, `dbLoadRecords`, custom driver config commands
//!
//! **Phase 2 (iocInit):** Wire device support, start CA server
//!
//! **Phase 3 (post-init):** Interactive iocsh REPL
//!   - `dbl`, `dbgf`, `dbpf`, `dbpr`, custom commands
//!
//! # Example
//!
//! ```rust,ignore
//! IocApplication::new()
//!     .port(5064)
//!     .register_device_support("myDevice", || Box::new(MyDeviceSupport::new()))
//!     .register_startup_command(my_config_command())
//!     .startup_script("st.cmd")
//!     .run()
//!     .await
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::{CaError, CaResult};
use crate::runtime::net::CA_SERVER_PORT;
use crate::server::record::SubroutineFn;

use super::database::PvDatabase;
use super::device_support::DeviceSupport;
use super::iocsh::{self, registry::CommandDef};
use super::record::{self, Record};
use super::{autosave, access_security, CaServer, DeviceSupportFactory};

/// Dynamic device support factory: given a DTYP name, returns device support if recognized.
pub type DynamicDeviceSupportFactory =
    Box<dyn Fn(&str) -> Option<Box<dyn DeviceSupport>> + Send + Sync>;

/// IOC Application with st.cmd-style startup support.
pub struct IocApplication {
    port: u16,
    device_factories: HashMap<String, DeviceSupportFactory>,
    dynamic_device_factory: Option<DynamicDeviceSupportFactory>,
    subroutine_registry: HashMap<String, Arc<SubroutineFn>>,
    acf: Option<access_security::AccessSecurityConfig>,
    autosave_config: Option<autosave::AutosaveConfig>,
    startup_commands: Vec<CommandDef>,
    shell_commands: Vec<CommandDef>,
    startup_script: Option<String>,
    /// Records added via the declarative builder (Phase 7).
    inline_records: Vec<(String, Box<dyn Record>)>,
}

impl IocApplication {
    pub fn new() -> Self {
        Self {
            port: CA_SERVER_PORT,
            device_factories: HashMap::new(),
            dynamic_device_factory: None,
            subroutine_registry: HashMap::new(),
            acf: None,
            autosave_config: None,
            startup_commands: Vec::new(),
            shell_commands: Vec::new(),
            startup_script: None,
            inline_records: Vec::new(),
        }
    }

    /// Set the CA server port (default: 5064).
    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Register a device support factory by DTYP name.
    /// Called during iocInit to wire device support to records.
    pub fn register_device_support<F>(mut self, dtyp: &str, factory: F) -> Self
    where
        F: Fn() -> Box<dyn DeviceSupport> + Send + Sync + 'static,
    {
        self.device_factories.insert(dtyp.to_string(), Box::new(factory));
        self
    }

    /// Register a dynamic device support factory.
    ///
    /// Called as a fallback when a record's DTYP doesn't match any
    /// statically registered factory. The closure receives the DTYP name
    /// and returns `Some(device_support)` if it can handle that DTYP.
    ///
    /// Multiple calls are chained: new factory is tried first, then existing.
    pub fn register_dynamic_device_support<F>(mut self, factory: F) -> Self
    where
        F: Fn(&str) -> Option<Box<dyn DeviceSupport>> + Send + Sync + 'static,
    {
        if let Some(existing) = self.dynamic_device_factory.take() {
            self.dynamic_device_factory = Some(Box::new(move |dtyp: &str| {
                factory(dtyp).or_else(|| existing(dtyp))
            }));
        } else {
            self.dynamic_device_factory = Some(Box::new(factory));
        }
        self
    }

    /// Register a command available during startup script execution (Phase 1).
    /// Use this for driver configuration commands like `simDetectorConfig`.
    pub fn register_startup_command(mut self, cmd: CommandDef) -> Self {
        self.startup_commands.push(cmd);
        self
    }

    /// Register a command available in the interactive shell (Phase 3).
    /// Use this for runtime commands like `simDetectorReport`.
    pub fn register_shell_command(mut self, cmd: CommandDef) -> Self {
        self.shell_commands.push(cmd);
        self
    }

    /// Set the startup script path (executed before iocInit).
    pub fn startup_script(mut self, path: &str) -> Self {
        self.startup_script = Some(path.to_string());
        self
    }

    /// Register a subroutine function by name (for sub records).
    pub fn register_subroutine<F>(mut self, name: &str, func: F) -> Self
    where
        F: Fn(&mut dyn Record) -> CaResult<()> + Send + Sync + 'static,
    {
        self.subroutine_registry
            .insert(name.to_string(), Arc::new(Box::new(func)));
        self
    }

    /// Configure autosave.
    pub fn autosave(mut self, config: autosave::AutosaveConfig) -> Self {
        self.autosave_config = Some(config);
        self
    }

    /// Configure access security.
    pub fn acf(mut self, config: access_security::AccessSecurityConfig) -> Self {
        self.acf = Some(config);
        self
    }

    // --- Declarative IOC Builder (Phase 7) ---

    /// Add a typed record to the IOC (no .db file needed).
    ///
    /// ```rust,ignore
    /// IocApplication::new()
    ///     .record("sensor:temp", AiRecord::new(0.0))
    ///     .record("heater:sp", AoRecord::new(0.0))
    ///     .run().await
    /// ```
    pub fn record(mut self, name: &str, record: impl Record) -> Self {
        self.inline_records
            .push((name.to_string(), Box::new(record)));
        self
    }

    /// Add a pre-boxed record.
    pub fn record_boxed(mut self, name: &str, record: Box<dyn Record>) -> Self {
        self.inline_records.push((name.to_string(), record));
        self
    }

    /// Run the full IOC lifecycle: startup script → iocInit → interactive shell.
    pub async fn run(self) -> CaResult<()> {
        let db = Arc::new(PvDatabase::new());
        let handle = tokio::runtime::Handle::current();

        let Self {
            port,
            device_factories,
            dynamic_device_factory,
            subroutine_registry,
            acf,
            autosave_config,
            startup_commands,
            shell_commands,
            startup_script,
            inline_records,
        } = self;

        // Add inline records (Phase 7 declarative builder)
        for (name, record) in inline_records {
            db.add_record(&name, record).await;
        }

        // Phase 1: Execute startup script in a separate std::thread.
        // std::thread (not spawn_blocking) is required because iocsh commands
        // use Handle::block_on() which panics inside the tokio runtime context.
        if let Some(script) = startup_script {
            let db1 = db.clone();
            let h1 = handle.clone();

            let (tx, rx) = crate::runtime::sync::oneshot::channel();
            std::thread::Builder::new()
                .name("iocsh-startup".into())
                .spawn(move || {
                    let shell = iocsh::IocShell::new(db1, h1);
                    for cmd in startup_commands {
                        shell.register(cmd);
                    }
                    let result = shell.execute_script(&script);
                    let _ = tx.send(result);
                })
                .expect("failed to spawn startup thread");

            let result = rx
                .await
                .map_err(|_| CaError::InvalidValue("startup thread dropped".into()))?;
            result.map_err(|e| CaError::InvalidValue(e))?;
        }

        // Phase 2: iocInit — wire device support to all records with DTYP
        let record_count = wire_device_support(&db, &device_factories, &dynamic_device_factory).await?;
        wire_subroutines(&db, &subroutine_registry).await;
        let io_intr_count = setup_io_intr(db.clone()).await;
        db.setup_cp_links().await;

        // Restore autosave
        if let Some(ref cfg) = autosave_config {
            let count = autosave::restore_from_file(&db, &cfg.save_path).await?;
            if count > 0 {
                eprintln!("autosave: restored {count} PVs");
            }
        }

        let total_records = db.all_record_names().await.len();
        eprintln!("iocInit: {total_records} records, {record_count} with device support, {io_intr_count} I/O Intr");

        // Phase 3: Build CaServer and run with interactive shell
        let server = CaServer::from_parts(db, port, acf, autosave_config);

        server
            .run_with_shell(move |shell| {
                for cmd in shell_commands {
                    shell.register(cmd);
                }
            })
            .await
    }
}

/// Wire device support to all records that have DTYP set.
async fn wire_device_support(
    db: &PvDatabase,
    factories: &HashMap<String, DeviceSupportFactory>,
    dynamic_factory: &Option<DynamicDeviceSupportFactory>,
) -> CaResult<usize> {
    let names = db.all_record_names().await;
    let mut count = 0;
    for name in names {
        if let Some(rec_arc) = db.get_record(&name).await {
            let mut instance = rec_arc.write().await;
            let dtyp = instance.common.dtyp.clone();
            if !dtyp.is_empty() && dtyp != "Soft Channel" {
                let dev_opt = if let Some(factory) = factories.get(&dtyp) {
                    Some(factory())
                } else if let Some(ref dyn_factory) = dynamic_factory {
                    dyn_factory(&dtyp)
                } else {
                    None
                };
                if let Some(mut dev) = dev_opt {
                    dev.set_record_info(&name, instance.common.scan);
                    let init_ok = dev.init(&mut *instance.record).is_ok();
                    // Clear UDF if init successfully set a value (e.g. initial readback)
                    if init_ok && instance.record.val().is_some() {
                        instance.common.udf = false;
                    }
                    instance.device = Some(dev);
                    count += 1;
                } else {
                    eprintln!("warning: no device support registered for DTYP '{dtyp}' (record: {name})");
                }
            }
        }
    }
    Ok(count)
}

/// Wire subroutine functions to sub records.
async fn wire_subroutines(
    db: &PvDatabase,
    registry: &HashMap<String, Arc<SubroutineFn>>,
) {
    if registry.is_empty() {
        return;
    }
    let names = db.all_record_names().await;
    for name in names {
        if let Some(rec_arc) = db.get_record(&name).await {
            let mut instance = rec_arc.write().await;
            if instance.record.record_type() == "sub" {
                if let Some(crate::types::EpicsValue::String(snam)) =
                    instance.record.get_field("SNAM")
                {
                    if let Some(sub_fn) = registry.get(&snam) {
                        instance.subroutine = Some(sub_fn.clone());
                    }
                }
            }
        }
    }
}

/// Set up I/O Intr scanning for records with SCAN="I/O Intr".
async fn setup_io_intr(db: Arc<PvDatabase>) -> usize {
    let all_names = db.all_record_names().await;
    let io_intr_recs: Vec<(String, Arc<crate::runtime::sync::RwLock<record::RecordInstance>>)> = {
        let mut recs = Vec::new();
        for name in &all_names {
            if let Some(arc) = db.get_record(name).await {
                recs.push((name.clone(), arc));
            }
        }
        recs
    };

    let mut count = 0;
    for (name, rec_arc) in io_intr_recs {
        let mut inst = rec_arc.write().await;
        if inst.common.scan == record::ScanType::IoIntr {
            if let Some(mut dev) = inst.device.take() {
                if let Some(mut intr_rx) = dev.io_intr_receiver() {
                    let db_clone = db.clone();
                    let rec_name = name.clone();
                    let rec_arc_clone = rec_arc.clone();
                    crate::runtime::task::spawn(async move {
                        while intr_rx.recv().await.is_some() {
                            // Only process if record is still on I/O Intr scan
                            let is_io_intr = {
                                let inst = rec_arc_clone.read().await;
                                inst.common.scan == record::ScanType::IoIntr
                            };
                            if !is_io_intr {
                                continue;
                            }
                            let mut visited = std::collections::HashSet::new();
                            let _ = db_clone
                                .process_record_with_links(&rec_name, &mut visited, 0)
                                .await;
                        }
                    });
                    count += 1;
                }
                inst.device = Some(dev);
            }
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_ioc_application_empty() {
        // An empty IocApplication with no script or records should start and stop cleanly
        // We can't easily test run() because it blocks on REPL, so test the wiring functions
        let db = Arc::new(PvDatabase::new());
        let factories = HashMap::new();
        let count = wire_device_support(&db, &factories, &None).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_wire_device_support_no_dtyp() {
        use crate::server::records::ai::AiRecord;

        let db = Arc::new(PvDatabase::new());
        db.add_record("TEST", Box::new(AiRecord::new(0.0))).await;

        let factories = HashMap::new();
        let count = wire_device_support(&db, &factories, &None).await.unwrap();
        assert_eq!(count, 0); // No DTYP set, so no wiring
    }
}
