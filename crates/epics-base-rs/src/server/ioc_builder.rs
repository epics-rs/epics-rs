//! IocBuilder — protocol-agnostic IOC bootstrap logic.
//!
//! Collects PVs, records, .db definitions, device support factories,
//! record type factories, subroutine registrations, and autosave config,
//! then materialises a populated [`PvDatabase`] in a single async `build()`.

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::{CaError, CaResult};
use crate::server::record::{Record, SubroutineFn};
use crate::types::EpicsValue;

use super::cvt_bpt::BrkTable;
use super::database::{PvDatabase, RecordLoad};
use super::device_support;
use super::ioc_app::{DeviceSupportContext, DynamicDeviceSupportFactory};
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
    /// `breaktable(...)` definitions parsed from the loaded `.db`/`.dbd` text,
    /// used to populate the breakpoint-table registry for `ai`/`ao` records
    /// with `LINR >= 3`.
    breaktables: Vec<BrkTable>,
    device_factories: HashMap<String, DeviceSupportFactory>,
    /// Fallback factory consulted when the static `device_factories`
    /// map has no entry for a record's DTYP. Mirrors
    /// `IocApplication::register_dynamic_device_support` so universal
    /// drivers (asyn `universal_asyn_factory`, areaDetector plugin
    /// dispatch) work in both build paths.
    dynamic_device_factory: Option<DynamicDeviceSupportFactory>,
    record_factories: HashMap<String, RecordFactory>,
    subroutine_registry: HashMap<String, Arc<SubroutineFn>>,
    autosave_config: Option<autosave::SaveSetConfig>,
}

impl IocBuilder {
    /// Create an empty builder. Built-in device support that historically
    /// ships with epics-base (currently: `getenv` for stringin/lsi)
    /// is pre-registered so `.db` files can use the canonical DTYP
    /// names with zero extra setup.
    pub fn new() -> Self {
        // No context-free built-in device support: every base builtin
        // (`Soft Timestamp`, `stdio`, `Db State`, `getenv`) needs the record's
        // INST_IO `INP`/`OUT`, which only the dynamic factory's
        // `DeviceSupportContext` carries — so all base builtins are dispatched
        // below, and this static map starts empty (users register their own
        // context-free device support into it via `register_device_support`).
        let device_factories: HashMap<String, DeviceSupportFactory> = HashMap::new();
        Self {
            pvs: Vec::new(),
            records: Vec::new(),
            db_defs: Vec::new(),
            breaktables: Vec::new(),
            device_factories,
            // The base built-in device support — all needing the runtime
            // context (INP/OUT). Pre-registered as the base of the
            // dynamic-factory chain so a user's
            // `register_dynamic_device_support` factory takes priority and
            // falls through to here.
            dynamic_device_factory: Some(Box::new(super::builtin_devices::builtin_dynamic_factory)),
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
        let (defs, breaktables) = db_loader::parse_db_with_breaktables(&content, macros)?;
        self.db_defs.extend(defs);
        self.breaktables.extend(breaktables);
        Ok(self)
    }

    /// Load records from a .db string.
    pub fn db_string(mut self, content: &str, macros: &HashMap<String, String>) -> CaResult<Self> {
        let (defs, breaktables) = db_loader::parse_db_with_breaktables(content, macros)?;
        self.db_defs.extend(defs);
        self.breaktables.extend(breaktables);
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

    /// Register a dynamic device support factory.
    ///
    /// Called as a fallback when a record's DTYP doesn't match any
    /// statically registered factory. Multiple calls are chained:
    /// the new factory is tried first, then the previously registered
    /// one. Mirrors
    /// [`crate::server::ioc_app::IocApplication::register_dynamic_device_support`]
    /// so universal drivers (asyn `universal_asyn_factory`,
    /// areaDetector plugin dispatch) attach correctly in both build
    /// paths.
    pub fn register_dynamic_device_support<F>(mut self, factory: F) -> Self
    where
        F: Fn(&DeviceSupportContext) -> Option<Box<dyn device_support::DeviceSupport>>
            + Send
            + Sync
            + 'static,
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

    /// Register a subroutine function by name (for sub/aSub records).
    /// The closure returns the C `long` status (`Ok(0)` normal, `Ok(n<0)`
    /// raises `SOFT_ALARM`/`BRSV`; `aSub` publishes it as `VAL`).
    pub fn register_subroutine<F>(mut self, name: &str, func: F) -> Self
    where
        F: Fn(&mut dyn Record) -> CaResult<i64> + Send + Sync + 'static,
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

        // A build is a load plus C's `iocInit`: records are created here, and
        // the link-status classifications they issue are queued until
        // `db.ioc_init()` at the end runs them against the finished database.
        db.begin_load()
            .expect("a database created a line ago has not run iocInit");

        // Breakpoint-table registry (C `bptList`): merge every loaded
        // `breaktable(...)` into the database's shared registry (the single
        // owner — also grown later by runtime `dbLoadRecords`) and take a
        // snapshot to install on this build's `ai`/`ao` records. Name-sorted by
        // `BreakTableRegistry`, so `LINR = 3` selects the first table. When no
        // tables were loaded the snapshot is empty and never installed (zero
        // overhead for IOCs that use none).
        let breaktable_registry = db.add_breaktables(self.breaktables).await;

        // 1. Simple PVs
        for (name, value) in self.pvs {
            db.add_pv(&name, value).await?;
        }

        // 2. Inline records
        for (name, record) in self.records {
            // The sink runs C's `iocInit` passes (`run_init_passes`) — an
            // inline record has no `.db` field set to load first, so it is
            // complete the moment it is added. Common-link classification
            // (init_links) and the mbboDirect UDF finalisation stay on the
            // `.db` path only: an inline record has no parsed common fields
            // to classify or fold.
            db.add_record(&name, record).await?;
            if let Some(rec_arc) = db.get_record(&name) {
                let mut instance = rec_arc.write();
                // Seed MLST/ALST/LALM from val so the first process posts a
                // monitor only on a real change (C init_record invariant).
                instance.record.seed_deadband_tracking();
            }
        }

        // 3. .db definitions — create records, apply fields, init, wire device support & subs
        for mut def in self.db_defs {
            let mut record =
                db_loader::create_record_with_factories(&def.record_type, &self.record_factories)?;

            // Resolve a `LINR` field that names a loaded breakpoint table to the
            // numeric `menuConvert` index that selects it (before apply_fields,
            // which only knows the fixed menuConvert labels). The registry
            // itself is installed by `add_record` (the single creation sink).
            db_loader::resolve_linr_breaktable_names(
                &def.record_type,
                &mut def.fields,
                &breaktable_registry,
            );

            let mut common_fields = Vec::new();
            db_loader::apply_fields(&mut record, &def.fields, &mut common_fields)?;

            // The record and its whole loaded field set enter the database
            // together: the sink applies the common fields and the info tags,
            // and only then runs C's `iocInit` passes — so the initial UDF
            // severity is evaluated against the `.db`'s final UDF/STAT/UDFS
            // (C `dbLoadRecords` → `iocInit`, not the reverse).
            db.add_loaded_record(
                &def.name,
                record,
                RecordLoad {
                    common_fields,
                    info_tags: std::mem::take(&mut def.info_tags),
                },
            )
            .await?;

            // alias(...) directives.
            for alias in &def.aliases {
                if let Err(e) = db.add_alias(alias, &def.name).await {
                    eprintln!(
                        "alias({alias}) for {target} rejected: {e}",
                        target = def.name
                    );
                }
            }

            // Device support and the post-init owners for the RecordInstance
            if let Some(rec_arc) = db.get_record(&def.name) {
                // Hand the record its resolved common link fields so a
                // link-classifying record (calcout INAV..INUV/OUTV) can run
                // its C `init_record` checkLinks step now — the common OUT
                // link is set above, after `set_async_context` already ran at
                // `add_record`. Defaulted no-op for records that do not
                // classify common links. The data guard is released at each
                // block close before the init awaits below (parking_lot guards
                // are `!Send`).
                {
                    let mut instance = rec_arc.write();
                    let inst = &mut *instance;
                    inst.record.init_links(&inst.common);
                }
                // C: a soft-channel INPUT dev support's `init_record` loads a
                // CONSTANT INP into the record's value
                // (`recGblInitConstantLink` / `dbLoadLinkArray`) — and it runs
                // BEFORE the record seeds MLST/ALST/LALM from VAL (e.g.
                // `aiRecord.c:114-127`: `pdset->common.init_record` first, then
                // `prec->mlst = prec->val`). So this owner runs here, ahead of
                // `seed_deadband_tracking`, or the first process would post a
                // spurious monitor for a value that was there since init.
                db.rec_gbl_init_constant_links(&rec_arc);

                // Seed MLST/ALST/LALM from val (after any UDF/bit fold that
                // may have changed val) so the first process posts a monitor
                // only on a real change (C init_record invariant).
                {
                    let mut instance = rec_arc.write();
                    instance.record.seed_deadband_tracking();
                }

                let mut instance = rec_arc.write();

                // Device support based on DTYP
                let dtyp = instance.common.dtyp.clone();
                if !crate::server::device_support::is_soft_dtyp(&dtyp) {
                    let dev_opt = if let Some(factory) = self.device_factories.get(&dtyp) {
                        Some(factory())
                    } else if let Some(ref dyn_factory) = self.dynamic_device_factory {
                        // Same fallback shape as
                        // `ioc_app::wire_device_support` — universal
                        // drivers (asyn etc.) need this to attach.
                        let ctx = DeviceSupportContext {
                            dtyp: &dtyp,
                            inp: &instance.common.inp,
                            out: &instance.common.out,
                        };
                        dyn_factory(&ctx)
                    } else {
                        None
                    };
                    if let Some(dev) = dev_opt {
                        // Canonical device-support init order (M1/M2):
                        // set_record_info → apply_record_info → init.
                        // Previously this path ran `init` FIRST and
                        // discarded its `Result` with `let _ =`; the
                        // IocApplication path ran info-setup first and
                        // partially handled the error. Both paths now
                        // share `wire_device_to_record`, so a driver
                        // author can write one correct `init()` and an
                        // init failure is logged + flags the record.
                        device_support::wire_device_to_record(&mut instance, dev);
                    }
                }
                // Subroutine resolution for sub / aSub records (C
                // `init_record` -> `registryFunctionFind` for both types).
                let rt = instance.record.record_type();
                if rt == "sub" || rt == "aSub" {
                    // INAM: invoke the init routine once, before SNAM
                    // resolution (C `init_record`: `registryFunctionFind(inam)`
                    // then `(*psubroutine)(prec)`, return discarded; a missing
                    // function is an init error -> stderr).
                    if let Some(EpicsValue::String(inam)) = instance.record.get_field("INAM") {
                        let inam = inam.as_str_lossy();
                        if !inam.is_empty() {
                            match self.subroutine_registry.get(inam.as_ref()) {
                                Some(init_fn) => {
                                    let init_fn = init_fn.clone();
                                    if let Err(e) = init_fn(&mut *instance.record) {
                                        eprintln!(
                                            "iocInit: {}.INAM '{inam}' init routine failed: {e}",
                                            def.name
                                        );
                                    }
                                }
                                None => eprintln!(
                                    "iocInit: {}.INAM function '{inam}' not found",
                                    def.name
                                ),
                            }
                        }
                    }
                    // Unconditional, as in `IocApp` — see the invariant on
                    // `field_io::SnamPut`.
                    if let Some(EpicsValue::String(snam)) = instance.record.get_field("SNAM") {
                        instance.subroutine = self
                            .subroutine_registry
                            .get(snam.as_str_lossy().as_ref())
                            .cloned();
                    }
                }
            }
        }

        // Retain the registry in the database for runtime re-resolution
        // (aSub LFLG=READ / SUBL); the static SNAM wiring above already
        // performed init-time resolution (C `init_record`).
        db.install_subroutine_registry(self.subroutine_registry.clone())
            .await;

        // 4. Autosave restore
        if let Some(ref autosave_cfg) = self.autosave_config {
            let count = autosave::restore_from_file(&db, &autosave_cfg.save_path).await?;
            if count > 0 {
                eprintln!("autosave: restored {count} PVs");
            }
        }

        // 5. I/O Intr setup — through the single owner shared with the
        // `IocApplication` iocInit path, so C `scanAdd`'s failure exits
        // (`dbScan.c:266-297`: no DSET / no interrupt source → log and demote
        // SCAN to Passive) apply on both startup routes.
        let _ = crate::server::ioc_app::setup_io_intr(db.clone()).await;

        // 6. Out-of-band PROPERTY-post setup (asyn enum-string runtime
        // re-propagation). Independent of SCAN, so it is a separate pass
        // from the I/O Intr wiring above. Shared with the IocApplication
        // iocInit path so both builders arm the enum callback identically.
        crate::server::ioc_app::setup_property_posts(db.clone()).await;

        // 7. The `iocInit` barrier: every record exists, so run the link-status
        // classifications queued during the load. Link status is FINAL when
        // `build` returns, as it is when C's `iocInit` returns.
        db.ioc_init().await;

        Ok((db, self.autosave_config))
    }
}

impl Default for IocBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ai_factory() -> Box<dyn Record> {
        Box::new(crate::server::records::ai::AiRecord::new(0.0))
    }

    /// Regression: an `alias("ALT")` directive in a .db file
    /// loaded through `IocBuilder::db_string` must be registered as
    /// an alias on the resulting `PvDatabase` so that lookup by the
    /// alias resolves to the same record as lookup by the canonical
    /// name. Pre-fix, `ioc_builder` discarded `def.aliases`.
    #[epics_macros_rs::epics_test]
    async fn db_string_registers_aliases() {
        let db_content = r#"
record(ai, "REAL:NAME") {
    field(VAL, "1.0")
    alias("PRETTY:NAME")
}
"#;
        let (db, _) = IocBuilder::new()
            .register_record_type("ai", ai_factory)
            .db_string(db_content, &HashMap::new())
            .unwrap()
            .build()
            .await
            .unwrap();

        assert!(db.has_name("REAL:NAME").await);
        assert!(
            db.has_name("PRETTY:NAME").await,
            "alias must be registered on the database",
        );
        assert!(db.find_entry("PRETTY:NAME").await.is_some());
    }

    /// Regression: `info("key", "value")` directives must be
    /// stored on the resulting RecordInstance. Pre-fix, no consumer
    /// existed for `def.info_tags` — every record's `info` map was
    /// silently empty after build.
    #[epics_macros_rs::epics_test]
    async fn db_string_populates_info_tags() {
        let db_content = r#"
record(ai, "AI:WITH:INFO") {
    field(VAL, "0.0")
    info("asyn:READBACK", "1")
    info("Q:group", "myGroup")
}
"#;
        let (db, _) = IocBuilder::new()
            .register_record_type("ai", ai_factory)
            .db_string(db_content, &HashMap::new())
            .unwrap()
            .build()
            .await
            .unwrap();

        let rec = db.get_record("AI:WITH:INFO").unwrap();
        let inst = rec.read();
        assert_eq!(inst.get_info("asyn:READBACK"), Some("1"));
        assert_eq!(inst.get_info("Q:group"), Some("myGroup"));
        assert_eq!(inst.get_info("missing"), None);
    }

    /// End-to-end breakpoint table: a `breaktable(...)` plus an `ai` whose
    /// `LINR` names it must, after `build()`, resolve the name to its
    /// `menuConvert` index AND convert raw -> eng through the installed
    /// registry. Proves the full loader -> registry -> record wiring.
    #[epics_macros_rs::epics_test]
    async fn db_string_breaktable_linr_resolves_and_converts() {
        let db_content = r#"
breaktable(ramp) {
    0    0
    100  10
    300  30
}
record(ai, "AI:BPT") {
    field(LINR, "ramp")
}
"#;
        let (db, _) = IocBuilder::new()
            .register_record_type("ai", ai_factory)
            .db_string(db_content, &HashMap::new())
            .unwrap()
            .build()
            .await
            .unwrap();

        let rec = db.get_record("AI:BPT").unwrap();
        let mut inst = rec.write();
        // "ramp" is a non-standard name -> first user-table index (15); the
        // standard menuConvert names reserve 3..=14.
        assert_eq!(inst.record.get_field("LINR"), Some(EpicsValue::Short(15)));
        // The installed registry makes the conversion work end-to-end:
        // raw 50 in [0,100] -> eng 5.0.
        inst.record.put_field("RVAL", EpicsValue::Long(50)).unwrap();
        inst.record.process().unwrap();
        assert_eq!(inst.record.get_field("VAL"), Some(EpicsValue::Double(5.0)));
    }

    /// Regression: IocBuilder must consult the dynamic
    /// device-support factory when no static factory matches the
    /// record's DTYP — otherwise universal drivers (asyn
    /// `universal_asyn_factory`, areaDetector plugin dispatch) only
    /// attach when records are loaded through IocApplication's
    /// startup-script path, never the pure-Rust IocBuilder path.
    #[epics_macros_rs::epics_test]
    async fn dynamic_device_factory_attaches_when_static_missing() {
        use crate::server::device_support::{DeviceReadOutcome, DeviceSupport};
        use crate::server::record::ScanType;

        struct UniversalDev;
        impl DeviceSupport for UniversalDev {
            fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
                Ok(())
            }
            fn dtyp(&self) -> &str {
                "asynInt32"
            }
            fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
                Ok(DeviceReadOutcome::ok())
            }
            fn set_record_info(&mut self, _name: &str, _scan: ScanType) {}
        }

        let db_content = r#"
record(ai, "AI:DYN") {
    field(DTYP, "asynInt32")
    field(INP, "@asyn(PORT,0)VAL")
}
"#;
        let (db, _) = IocBuilder::new()
            .register_record_type("ai", ai_factory)
            .register_dynamic_device_support(|ctx: &DeviceSupportContext| {
                if ctx.dtyp.starts_with("asyn") {
                    Some(Box::new(UniversalDev))
                } else {
                    None
                }
            })
            .db_string(db_content, &HashMap::new())
            .unwrap()
            .build()
            .await
            .unwrap();

        let rec = db.get_record("AI:DYN").unwrap();
        let inst = rec.read();
        assert!(
            inst.device.is_some(),
            "dynamic factory must attach when static factories miss"
        );
    }

    /// Chaining preserves earlier dynamic factories. Newest
    /// factory wins for matching DTYPs; non-matching DTYPs fall
    /// through to previously registered factories. Mirrors
    /// `IocApplication::register_dynamic_device_support` chaining.
    #[epics_macros_rs::epics_test]
    async fn dynamic_factories_chain_lifo_with_fallthrough() {
        use crate::server::device_support::{DeviceReadOutcome, DeviceSupport};
        use crate::server::record::ScanType;

        struct DevA;
        struct DevB;
        impl DeviceSupport for DevA {
            fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
                Ok(())
            }
            fn dtyp(&self) -> &str {
                "DTA"
            }
            fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
                Ok(DeviceReadOutcome::ok())
            }
            fn set_record_info(&mut self, _name: &str, _scan: ScanType) {}
        }
        impl DeviceSupport for DevB {
            fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
                Ok(())
            }
            fn dtyp(&self) -> &str {
                "DTB"
            }
            fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
                Ok(DeviceReadOutcome::ok())
            }
            fn set_record_info(&mut self, _name: &str, _scan: ScanType) {}
        }

        let db_a = r#"
record(ai, "REC:A") { field(DTYP, "DTA") }
record(ai, "REC:B") { field(DTYP, "DTB") }
"#;
        let (db, _) = IocBuilder::new()
            .register_record_type("ai", ai_factory)
            .register_dynamic_device_support(|ctx| {
                if ctx.dtyp == "DTA" {
                    Some(Box::new(DevA))
                } else {
                    None
                }
            })
            .register_dynamic_device_support(|ctx| {
                if ctx.dtyp == "DTB" {
                    Some(Box::new(DevB))
                } else {
                    None
                }
            })
            .db_string(db_a, &HashMap::new())
            .unwrap()
            .build()
            .await
            .unwrap();

        // Both records should have a device attached — proves the
        // newer factory passes through DTA to the older factory.
        for name in ["REC:A", "REC:B"] {
            let rec = db.get_record(name).unwrap();
            assert!(
                rec.read().device.is_some(),
                "factory chaining failed for {name}"
            );
        }
    }
}
