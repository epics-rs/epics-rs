//! IocBuilder — protocol-agnostic IOC bootstrap logic.
//!
//! Collects PVs, records, .db definitions, device support factories,
//! record type factories, subroutine registrations, and autosave config,
//! then materialises a populated [`PvDatabase`] in a single async `build()`.

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::{CaError, CaResult};
use crate::server::record::{self, Record, SubroutineFn};
use crate::types::EpicsValue;

use super::database::PvDatabase;
use super::device_support;
use super::{DeviceSupportFactory, RecordFactory};
use super::{autosave, db_loader};

/// Builder that performs all IOC-level database population, record
/// initialisation, device-support wiring, autosave restore, and I/O Intr
/// setup.  It is protocol-agnostic — the resulting [`PvDatabase`] can be
/// served over CA, PVA, or any other transport.
pub struct IocBuilder {
    pvs: Vec<(String, EpicsValue)>,
    records: Vec<(String, Box<dyn Record>)>,
    db_defs: Vec<db_loader::DbRecordDef>,
    device_factories: HashMap<String, DeviceSupportFactory>,
    record_factories: HashMap<String, RecordFactory>,
    subroutine_registry: HashMap<String, Arc<SubroutineFn>>,
    autosave_config: Option<autosave::SaveSetConfig>,
}

impl IocBuilder {
    /// Create an empty builder.
    pub fn new() -> Self {
        Self {
            pvs: Vec::new(),
            records: Vec::new(),
            db_defs: Vec::new(),
            device_factories: HashMap::new(),
            record_factories: HashMap::new(),
            subroutine_registry: HashMap::new(),
            autosave_config: None,
        }
    }

    /// Add a simple PV to be created on build.
    pub fn pv(mut self, name: &str, initial: EpicsValue) -> Self {
        self.pvs.push((name.to_string(), initial));
        self
    }

    /// Add a typed record to be created on build.
    pub fn record(mut self, name: &str, record: impl Record) -> Self {
        self.records.push((name.to_string(), Box::new(record)));
        self
    }

    /// Add a pre-boxed record to be created on build.
    pub fn record_boxed(mut self, name: &str, record: Box<dyn Record>) -> Self {
        self.records.push((name.to_string(), record));
        self
    }

    /// Load records from a .db file.
    pub fn db_file(mut self, path: &str, macros: &HashMap<String, String>) -> CaResult<Self> {
        let content = std::fs::read_to_string(path).map_err(CaError::Io)?;
        let defs = db_loader::parse_db(&content, macros)?;
        self.db_defs.extend(defs);
        Ok(self)
    }

    /// Load records from a .db string.
    pub fn db_string(mut self, content: &str, macros: &HashMap<String, String>) -> CaResult<Self> {
        let defs = db_loader::parse_db(content, macros)?;
        self.db_defs.extend(defs);
        Ok(self)
    }

    /// Register a device support factory by DTYP name.
    pub fn register_device_support<F>(mut self, dtyp: &str, factory: F) -> Self
    where
        F: Fn() -> Box<dyn device_support::DeviceSupport> + Send + Sync + 'static,
    {
        self.device_factories
            .insert(dtyp.to_string(), Box::new(factory));
        self
    }

    /// Register an external record type factory.
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

    /// Build the populated database.
    ///
    /// Performs, in order:
    /// 1. PV creation
    /// 2. Record creation (inline + .db definitions)
    /// 3. Field application, `init_record` passes
    /// 4. Device support wiring
    /// 5. Subroutine resolution
    /// 6. Autosave restore
    /// 7. I/O Intr setup
    ///
    /// Returns the populated database and the optional autosave config (so the
    /// caller can start the autosave loop).
    pub async fn build(self) -> CaResult<(Arc<PvDatabase>, Option<autosave::SaveSetConfig>)> {
        let db = Arc::new(PvDatabase::new());

        // 1. Simple PVs
        for (name, value) in self.pvs {
            db.add_pv(&name, value).await;
        }

        // 2. Inline records
        for (name, record) in self.records {
            db.add_record(&name, record).await;
        }

        // 3. .db definitions — create records, apply fields, init, wire device support & subs
        for def in self.db_defs {
            let mut record =
                db_loader::create_record_with_factories(&def.record_type, &self.record_factories)?;
            let mut common_fields = Vec::new();
            db_loader::apply_fields(&mut record, &def.fields, &mut common_fields)?;

            db.add_record(&def.name, record).await;

            // Apply common fields and device support to the RecordInstance
            if let Some(rec_arc) = db.get_record(&def.name).await {
                let mut instance = rec_arc.write().await;
                for (name, value) in common_fields {
                    match instance.put_common_field(&name, value) {
                        Ok(record::CommonFieldPutResult::ScanChanged {
                            old_scan,
                            new_scan,
                            phas,
                        }) => {
                            drop(instance);
                            db.update_scan_index(&def.name, old_scan, new_scan, phas, phas)
                                .await;
                            instance = rec_arc.write().await;
                        }
                        Ok(record::CommonFieldPutResult::PhasChanged {
                            scan,
                            old_phas,
                            new_phas,
                        }) => {
                            drop(instance);
                            db.update_scan_index(&def.name, scan, scan, old_phas, new_phas)
                                .await;
                            instance = rec_arc.write().await;
                        }
                        Ok(record::CommonFieldPutResult::NoChange) => {}
                        Err(e) => {
                            eprintln!("put_common_field({name}) failed for {}: {e}", def.name);
                        }
                    }
                }
                // init_record passes
                if let Err(e) = instance.record.init_record(0) {
                    eprintln!("init_record(0) failed for {}: {e}", def.name);
                }
                if let Err(e) = instance.record.init_record(1) {
                    eprintln!("init_record(1) failed for {}: {e}", def.name);
                }

                // Device support based on DTYP
                let dtyp = instance.common.dtyp.clone();
                if !crate::server::device_support::is_soft_dtyp(&dtyp) {
                    if let Some(factory) = self.device_factories.get(&dtyp) {
                        let mut dev = factory();
                        let _ = dev.init(&mut *instance.record);
                        dev.set_record_info(&def.name, instance.common.scan);
                        instance.device = Some(dev);
                    }
                }
                // Subroutine resolution for sub records
                if instance.record.record_type() == "sub" {
                    if let Some(EpicsValue::String(snam)) = instance.record.get_field("SNAM") {
                        if let Some(sub_fn) = self.subroutine_registry.get(&snam) {
                            instance.subroutine = Some(sub_fn.clone());
                        }
                    }
                }
            }
        }

        // 4. Autosave restore
        if let Some(ref autosave_cfg) = self.autosave_config {
            let count = autosave::restore_from_file(&db, &autosave_cfg.save_path).await?;
            if count > 0 {
                eprintln!("autosave: restored {count} PVs");
            }
        }

        // 5. I/O Intr setup
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
                    }
                    inst.device = Some(dev);
                }
            }
        }

        Ok((db, self.autosave_config))
    }
}

impl Default for IocBuilder {
    fn default() -> Self {
        Self::new()
    }
}
