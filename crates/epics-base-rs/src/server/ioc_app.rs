//! IOC Application — st.cmd-style startup for Rust IOCs.
//!
//! Provides a 2-phase IOC lifecycle matching the C++ EPICS pattern:
//!
//! **Phase 1 (pre-init):** Execute startup script (`st.cmd`)
//!   - `epicsEnvSet`, `dbLoadRecords`, custom driver config commands
//!
//! **Phase 2 (iocInit):** Wire device support, start protocol server
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
//!     .run(my_protocol_runner)
//!     .await
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::error::{CaError, CaResult};
use crate::runtime::net::CA_SERVER_PORT;
use crate::server::record::{self, Record, SubroutineFn};

use crate::server::database::PvDatabase;
use crate::server::device_support::DeviceSupport;
use crate::server::iocsh::{self, registry::CommandDef};
use crate::server::{DeviceSupportFactory, access_security, autosave};
use autosave::startup::AutosaveStartupConfig;

/// Context passed to dynamic device support factories during iocInit wiring.
pub struct DeviceSupportContext<'a> {
    pub dtyp: &'a str,
    pub inp: &'a str,
    pub out: &'a str,
}

/// Dynamic device support factory: given a context, returns device support if recognized.
pub type DynamicDeviceSupportFactory =
    Box<dyn Fn(&DeviceSupportContext) -> Option<Box<dyn DeviceSupport>> + Send + Sync>;

/// Configuration passed to the protocol runner after IOC initialization.
///
/// Contains all the pieces needed to start a protocol-specific server
/// (e.g., CA or PVA) with an interactive shell.
pub struct IocRunConfig {
    pub db: Arc<PvDatabase>,
    pub port: u16,
    pub acf: Option<access_security::AccessSecurityConfig>,
    pub autosave_config: Option<autosave::SaveSetConfig>,
    pub autosave_manager: Option<Arc<autosave::AutosaveManager>>,
    pub shell_commands: Vec<CommandDef>,
}

/// IOC Application with st.cmd-style startup support.
pub struct IocApplication {
    port: u16,
    device_factories: HashMap<String, DeviceSupportFactory>,
    dynamic_device_factory: Option<DynamicDeviceSupportFactory>,
    record_factories: HashMap<String, super::RecordFactory>,
    subroutine_registry: HashMap<String, Arc<SubroutineFn>>,
    acf: Option<access_security::AccessSecurityConfig>,
    autosave_config: Option<autosave::SaveSetConfig>,
    autosave_startup: Option<Arc<Mutex<AutosaveStartupConfig>>>,
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
            record_factories: HashMap::new(),
            subroutine_registry: HashMap::new(),
            acf: None,
            autosave_config: None,
            autosave_startup: None,
            startup_commands: Vec::new(),
            shell_commands: Vec::new(),
            startup_script: None,
            inline_records: Vec::new(),
        }
    }

    /// Set the server port (default: 5064).
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
        self.device_factories
            .insert(dtyp.to_string(), Box::new(factory));
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
        F: Fn(&DeviceSupportContext) -> Option<Box<dyn DeviceSupport>> + Send + Sync + 'static,
    {
        if let Some(existing) = self.dynamic_device_factory.take() {
            self.dynamic_device_factory = Some(Box::new(move |ctx: &DeviceSupportContext| {
                factory(ctx).or_else(|| existing(ctx))
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

    /// Register a record type factory (e.g., "motor", "asyn").
    /// Avoids the global registry — factories are passed to IocBuilder.
    pub fn register_record_type<F>(mut self, type_name: &str, factory: F) -> Self
    where
        F: Fn() -> Box<dyn Record> + Send + Sync + 'static,
    {
        self.record_factories
            .insert(type_name.to_string(), Box::new(factory));
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

    /// Configure autosave with a save set configuration.
    pub fn autosave(mut self, config: autosave::SaveSetConfig) -> Self {
        self.autosave_config = Some(config);
        self
    }

    /// Configure autosave startup (C-compatible iocsh commands).
    ///
    /// When set, autosave iocsh commands (`set_requestfile_path`, `create_monitor_set`,
    /// `set_pass0_restoreFile`, etc.) are registered as startup commands and populate
    /// the config during st.cmd execution. After iocInit, the config is consumed to
    /// build an `AutosaveManager`.
    pub fn autosave_startup(mut self, config: Arc<Mutex<AutosaveStartupConfig>>) -> Self {
        self.autosave_startup = Some(config);
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
    ///     .run(my_runner).await
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

    /// Run the full IOC lifecycle: startup script -> iocInit -> protocol runner.
    ///
    /// The `protocol_runner` closure receives an [`IocRunConfig`] containing the
    /// fully initialized database, port, and configuration. It is responsible for
    /// starting the protocol-specific server (e.g., CA, PVA) and the interactive shell.
    pub async fn run<F, Fut>(self, protocol_runner: F) -> CaResult<()>
    where
        F: FnOnce(IocRunConfig) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = CaResult<()>> + Send,
    {
        let db = Arc::new(PvDatabase::new());
        let handle = tokio::runtime::Handle::current();

        let Self {
            port,
            device_factories,
            dynamic_device_factory,
            record_factories,
            subroutine_registry,
            acf,
            autosave_config,
            autosave_startup,
            mut startup_commands,
            shell_commands,
            startup_script,
            inline_records,
        } = self;

        // Register record type factories with global registry so dbLoadRecords
        // (called from st.cmd) can find them. This bridges the injected factories
        // to the global registry that the iocsh dbLoadRecords command uses.
        for (name, factory) in record_factories {
            super::db_loader::register_record_type(&name, factory);
        }

        // Register autosave startup commands if configured
        if let Some(ref config) = autosave_startup {
            let cmds = AutosaveStartupConfig::register_startup_commands(config.clone());
            startup_commands.extend(cmds);
        }

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

        // Collect restore paths and builder from startup config (scoped mutex lock)
        let (pass0_files, pass1_files, builder_opt) = if let Some(ref config) = autosave_startup {
            let cfg = config.lock().unwrap();
            let pass0: Vec<std::path::PathBuf> = cfg
                .pass0_restores
                .iter()
                .map(|r| cfg.resolve_save_file(&r.filename))
                .collect();
            let pass1: Vec<std::path::PathBuf> = cfg
                .pass1_restores
                .iter()
                .map(|r| cfg.resolve_save_file(&r.filename))
                .collect();
            let builder = if !cfg.monitor_sets.is_empty() || !cfg.triggered_sets.is_empty() {
                Some(cfg.into_builder())
            } else {
                None
            };
            (pass0, pass1, builder)
        } else {
            (Vec::new(), Vec::new(), None)
        };

        // Phase 2a: Pass0 restore (before device support wiring)
        for sav_path in &pass0_files {
            match autosave::restore_from_file(&db, sav_path).await {
                Ok(count) if count > 0 => {
                    eprintln!("pass0 restore: {count} PVs from {}", sav_path.display());
                }
                Err(e) => {
                    eprintln!("pass0 restore warning: {} - {e}", sav_path.display());
                }
                _ => {}
            }
        }

        // Phase 2b: iocInit — wire device support to all records with DTYP
        let record_count =
            wire_device_support(&db, &device_factories, &dynamic_device_factory).await?;
        wire_subroutines(&db, &subroutine_registry).await;
        let io_intr_count = setup_io_intr(db.clone()).await;
        db.setup_cp_links().await;

        // Phase 2c: Pass1 restore (after device support wiring)
        for sav_path in &pass1_files {
            match autosave::restore_from_file(&db, sav_path).await {
                Ok(count) if count > 0 => {
                    eprintln!("pass1 restore: {count} PVs from {}", sav_path.display());
                }
                Err(e) => {
                    eprintln!("pass1 restore warning: {} - {e}", sav_path.display());
                }
                _ => {}
            }
        }

        // Autosave restore from SaveSetConfig
        if let Some(ref cfg) = autosave_config {
            let count = autosave::restore_from_file(&db, &cfg.save_path).await?;
            if count > 0 {
                eprintln!("autosave: restored {count} PVs");
            }
        }

        // Phase 2d: Build AutosaveManager from startup config
        let autosave_manager = if let Some(builder) = builder_opt {
            match builder.build().await {
                Ok(mgr) => {
                    eprintln!("autosave: {} save set(s) configured", mgr.set_names().len());
                    Some(Arc::new(mgr))
                }
                Err(e) => {
                    eprintln!("autosave: failed to build manager: {e}");
                    None
                }
            }
        } else {
            None
        };

        let total_records = db.all_record_names().await.len();
        eprintln!(
            "iocInit: {total_records} records, {record_count} with device support, {io_intr_count} I/O Intr"
        );

        // Phase 3: Hand off to protocol runner
        let config = IocRunConfig {
            db,
            port,
            acf,
            autosave_config,
            autosave_manager,
            shell_commands,
        };
        protocol_runner(config).await
    }
}

/// Wire device support to all records that have DTYP set.
pub(crate) async fn wire_device_support(
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
            if !crate::server::device_support::is_soft_dtyp(&dtyp) {
                let ctx = DeviceSupportContext {
                    dtyp: &dtyp,
                    inp: &instance.common.inp,
                    out: &instance.common.out,
                };
                let dev_opt = if let Some(factory) = factories.get(&dtyp) {
                    Some(factory())
                } else if let Some(dyn_factory) = dynamic_factory {
                    dyn_factory(&ctx)
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
                    eprintln!(
                        "warning: no device support registered for DTYP '{dtyp}' (record: {name})"
                    );
                }
            }
        }
    }
    Ok(count)
}

/// Wire subroutine functions to sub records.
async fn wire_subroutines(db: &PvDatabase, registry: &HashMap<String, Arc<SubroutineFn>>) {
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
    let io_intr_recs: Vec<(
        String,
        Arc<crate::runtime::sync::RwLock<record::RecordInstance>>,
    )> = {
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
