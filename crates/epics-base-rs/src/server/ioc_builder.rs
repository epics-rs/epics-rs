//! IocBuilder — protocol-agnostic IOC bootstrap logic.
//!
//! Collects PVs, records, .db definitions, device support factories,
//! record type factories, subroutine registrations, and autosave config,
//! then materialises a populated [`PvDatabase`] in a single async `build()`.

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::{CaError, CaResult};
use crate::runtime::log::ERL_ERROR;
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
    /// One entry per `.db` text taken in, in the order it was loaded,
    /// holding the source C would quote in `ERROR: Failed to load '%s'`.
    /// The file name is known only here, never to the record itself.
    sources: Vec<String>,
    /// Each `.db` definition, tagged with its [`IocBuilder::sources`]
    /// index.
    db_defs: Vec<(usize, db_loader::DbRecordDef)>,
    /// Which loads produced at least one recoverable diagnostic, as
    /// [`IocBuilder::sources`] indices. C recovers from every
    /// `yyerror(NULL)` and settles a load's status only when its file is
    /// finished (`dbLoadRecords`), so this is the whole status of every
    /// load: `build` fails naming the EARLIEST-LOADED entry, which is the
    /// file C's `softMain` would have stopped on. Indices, not names,
    /// because the two halves report at different times — a later file's
    /// parse fault is recorded before an earlier file's records are even
    /// built — and only the load position orders them the way C does.
    failed_loads: Vec<usize>,
    /// File-scope aliases whose target no `.db` added so far declares,
    /// each tagged with the [`IocBuilder::sources`] index that carried it.
    /// Resolved at `build` against the accumulated definitions, which is
    /// this builder's stand-in for C's `savedPdbbase`.
    db_aliases: Vec<(usize, String, String)>,
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
            sources: Vec::new(),
            db_defs: Vec::new(),
            failed_loads: Vec::new(),
            db_aliases: Vec::new(),
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
    ///
    /// A recoverable diagnostic in the text fails the eventual
    /// [`IocBuilder::build`] with [`CaError::DbLoadFailed`], which is C
    /// `dbLoadRecords` returning non-zero (`dbAccess.c:795-813`);
    /// `softIoc -d` on such a file exits 2 without reaching `iocInit`.
    /// Only a syntax error the parser cannot recover from — C's
    /// `yyerrorAbort` — fails here.
    pub fn db_file(mut self, path: &str, macros: &HashMap<String, String>) -> CaResult<Self> {
        let content = std::fs::read_to_string(path).map_err(CaError::Io)?;
        let parsed = db_loader::parse_db_with_breaktables(&content, macros)?;
        self.absorb_parsed(path, parsed);
        Ok(self)
    }

    /// Load records from a .db string. Settles its status exactly as
    /// [`IocBuilder::db_file`] does.
    pub fn db_string(mut self, content: &str, macros: &HashMap<String, String>) -> CaResult<Self> {
        let parsed = db_loader::parse_db_with_breaktables(content, macros)?;
        self.absorb_parsed(db_loader::DB_STRING_SOURCE, parsed);
        Ok(self)
    }

    /// Take one text's parse result into the builder.
    ///
    /// C keeps every record it managed to read past a `yyerror(NULL)` and
    /// carries the failure as the load's status instead of throwing the
    /// text away, so the fault is remembered rather than returned:
    /// [`IocBuilder::build`] is the only place that can report the
    /// per-record refusals as well and then fail once, with the name C
    /// quotes. Returning here instead would end the load at the first
    /// diagnostic and let the caller's own error stand in for C's tail.
    fn absorb_parsed(&mut self, source: &str, parsed: db_loader::ParsedDb) {
        let load = self.sources.len();
        self.sources.push(source.to_string());
        if !parsed.faults.is_empty() {
            self.failed_loads.push(load);
        }
        self.db_defs
            .extend(parsed.records.into_iter().map(|def| (load, def)));
        self.breaktables.extend(parsed.breaktables);
        self.db_aliases.extend(
            parsed
                .unresolved_aliases
                .into_iter()
                .map(|(target, alias)| (load, target, alias)),
        );
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
        let factory: super::RecordFactory = Box::new(factory);
        super::db_loader::snapshot_declared_fields(type_name, &factory);
        self.record_factories.insert(type_name.to_string(), factory);
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
            // to classify or fold. The sink also ends in the init-seed owner
            // (`seed_constant_links`), which runs BOTH halves of C's
            // `init_record` tail — this loop must not call either half itself,
            // or it decides for the record which half of its tail runs.
            db.add_record(&name, record).await?;
        }

        // 3. .db definitions — create records, apply fields, init, wire device support & subs
        //
        // C reports every record in the file and settles the load's status
        // only when the file is finished, so a refused FIELD must not end the
        // loop. C is not uniform here and neither is this loop: a field that
        // `dbPutString` refuses ends in `yyerror(NULL)`
        // (`dbLexRoutines.c:1409-1417`), which sets `yyFailed` and resumes at
        // the next record, while a record whose TYPE cannot be resolved or
        // whose creation fails ends in `yyerrorAbort(NULL)` (`:1163-1167` and
        // `:1189-1193`), which stops the parse of that file outright. So the
        // create step below abandons the rest of its own load and the two
        // field steps carry on.
        //
        // The diagnostic is printed by whichever site refuses, and C prints
        // at the refusal.
        let mut failed_loads = self.failed_loads;
        let mut aborted_loads: std::collections::HashSet<usize> = std::collections::HashSet::new();
        for (load, mut def) in self.db_defs {
            if aborted_loads.contains(&load) {
                continue;
            }
            let mut record = match db_loader::create_record_with_factories(
                &def.record_type,
                &self.record_factories,
            ) {
                Ok(record) => record,
                Err(e) => {
                    // C names the record TYPE as well as the name, and picks
                    // between two texts by cause — `Record type '%s' for
                    // record '%s' not found` when `dbFindRecordType` misses,
                    // `Can't create %s record '%s'` when `dbCreateRecord`
                    // itself fails. The port has one construction step for
                    // both, so it prints C's generic line and lets the
                    // reason say which happened.
                    eprintln!(
                        "{ERL_ERROR}: Can't create {} record '{}': {e}",
                        def.record_type, def.name
                    );
                    failed_loads.push(load);
                    aborted_loads.insert(load);
                    continue;
                }
            };

            // Resolve a `LINR` field that names a loaded breakpoint table to the
            // numeric `menuConvert` index that selects it (before apply_fields,
            // which only knows the fixed menuConvert labels). The registry
            // itself is installed by `add_record` (the single creation sink).
            db_loader::resolve_linr_breaktable_names(
                &def.record_type,
                &mut def.fields,
                &breaktable_registry,
            );

            // Screen the menu values before the record is built, which is the
            // same screen the iocsh `dbLoadRecords` path runs and for the same
            // reason. C creates the record and only THEN puts each field
            // (`dbCreateRecord` at `dbLexRoutines.c:1172`, `dbPutString` at
            // `:1405`), so a value `dbPutString` refuses costs that FIELD its
            // value and nothing else: the record stays with the dbd default,
            // its other fields load, and only the load's status goes non-zero.
            //
            // Deciding it at whichever site happened to apply the field gave
            // one C rule two spellings here too — `SCAN` reached
            // `add_loaded_record` and reported C's wording, `SELM` reached
            // `apply_fields` and reported the port's — and both discarded a
            // record C keeps. After the screen neither apply site can see a
            // value its menu would refuse, so neither can decide anything, and
            // the two load paths cannot drift apart again.
            let (screen_type, screen_name) = (def.record_type.clone(), def.name.clone());
            let mut screen_refused = false;
            def.fields.retain(|f| {
                let Some(refusal) = db_loader::menu_value_refusal(
                    &screen_type,
                    &screen_name,
                    &f.name.to_uppercase(),
                    &f.value.as_str_lossy(),
                ) else {
                    return true;
                };
                if let Some(notice) = refusal.notice {
                    eprintln!("{notice}");
                }
                eprintln!("{}", refusal.line);
                // C's `dbPutStringSuggest` (`dbLexRoutines.c:1414`) follows
                // the refusal it explains and leaves the status alone.
                if let Some(suggestion) = refusal.suggestion {
                    eprintln!("{suggestion}");
                }
                screen_refused = true;
                false
            });
            if screen_refused {
                failed_loads.push(load);
            }

            let mut common_fields = Vec::new();
            if let Err(e) = db_loader::apply_fields(&mut record, &def.fields, &mut common_fields) {
                eprintln!("{ERL_ERROR}: Can't load record '{}': {e}", def.name);
                failed_loads.push(load);
                continue;
            }

            // The record and its whole loaded field set enter the database
            // together: the sink applies the common fields and the info tags,
            // and only then runs C's `iocInit` passes — so the initial UDF
            // severity is evaluated against the `.db`'s final UDF/STAT/UDFS
            // (C `dbLoadRecords` → `iocInit`, not the reverse).
            if let Err(e) = db
                .add_loaded_record(
                    &def.name,
                    record,
                    RecordLoad {
                        common_fields,
                        info_tags: std::mem::take(&mut def.info_tags),
                    },
                )
                .await
            {
                eprintln!("{ERL_ERROR}: Can't load record '{}': {e}", def.name);
                failed_loads.push(load);
                continue;
            }

            // alias(...) directives. C `dbRecordAlias` reports the rejection
            // and calls `yyerror(NULL)` (`dbLexRoutines.c:1496`), so the
            // record keeps its place and the load's status goes non-zero.
            for alias in &def.aliases {
                if let Err(e) = db.add_alias(alias, &def.name).await {
                    eprintln!(
                        "alias({alias}) for {target} rejected: {e}",
                        target = def.name
                    );
                    failed_loads.push(load);
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
                // `aiRecord.c:115-129`: `pdset->common.init_record` first, then
                // `prec->mlst = prec->val`). The owner ENDS in C's init tail —
                // `init_record_tail` then `seed_deadband_tracking`, in that
                // order — so running it here is what puts the tail after the
                // constant load, and nothing outside the owner may run either
                // half.
                db.rec_gbl_init_constant_links(&rec_arc);

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
                    // `field_io::snam_special_after_put`.
                    if let Some(EpicsValue::String(snam)) = instance.record.get_field("SNAM") {
                        instance.subroutine = self
                            .subroutine_registry
                            .get(snam.as_str_lossy().as_ref())
                            .cloned();
                    }
                }
            }
        }

        // File-scope `alias("record","new")` that no single `.db` could
        // resolve on its own. C `dbAlias` looks the target up in
        // `savedPdbbase` (`dbLexRoutines.c:1508`), the whole accumulated
        // database — here that is every definition added to the builder,
        // now installed. An unknown target keeps C's diagnostic and
        // leaves the records that did load in place.
        // Both arms are `yyerror(NULL)` in C `dbAlias`
        // (`dbLexRoutines.c:1509-1517`), so both fail the load.
        for (load, target, alias) in self.db_aliases {
            if db.get_record(&target).is_none() {
                eprintln!("{}", db_loader::unknown_alias_message(&alias, &target));
                failed_loads.push(load);
            } else if let Err(e) = db.add_alias(&alias, &target).await {
                eprintln!("alias({alias}) for {target} rejected: {e}");
                failed_loads.push(load);
            }
        }

        // The status of every load, settled once for the whole build. C's
        // `softMain` exits on the first `dbLoadRecords` that returns
        // non-zero (`softMain.cpp:198`), so the file named is the
        // earliest-loaded one that failed, and `iocInit` below never runs.
        if let Some(&load) = failed_loads.iter().min() {
            return Err(CaError::DbLoadFailed(self.sources[load].clone()));
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

    /// C `dbLoadRecords` prints the diagnostic and returns non-zero
    /// (`dbAccess.c:795-813`), and `softMain` turns that into exit 2
    /// (`softMain.cpp:198,274-278`) — measured against softIoc at the
    /// R7.0.10 pin. The port used to print the same diagnostic and hand
    /// back `Ok`, so the IOC came up serving a database the operator had
    /// been told was bad.
    #[epics_macros_rs::epics_test]
    async fn a_recovered_diagnostic_fails_the_load() {
        let bad = r#"
record(ai, "A:ONE") {
    field(ASL, "1")
}
"#;
        let Err(err) = IocBuilder::new()
            .register_record_type("ai", ai_factory)
            .db_string(bad, &HashMap::new())
            .unwrap()
            .build()
            .await
        else {
            panic!("a dropped field must fail the load's status");
        };
        assert!(
            matches!(&err, CaError::DbLoadFailed(source) if source == db_loader::DB_STRING_SOURCE),
            "unexpected error: {err:?}",
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.db");
        std::fs::write(&path, bad).unwrap();
        let Err(named) = IocBuilder::new()
            .register_record_type("ai", ai_factory)
            .db_file(path.to_str().unwrap(), &HashMap::new())
            .unwrap()
            .build()
            .await
        else {
            panic!("db_file must fail the same way");
        };
        // C names the file it could not load; so must we.
        assert_eq!(
            named.to_string(),
            format!("Failed to load '{}'", path.display())
        );
    }

    /// C reports every record in the file and only then fails
    /// (`dbLexRoutines.c` recovers from each `yyerror(NULL)` and
    /// `dbLoadRecords` returns the accumulated status), so a file with
    /// three separately-bad records prints three diagnostics and one
    /// tail. The port stopped at the first, and then the caller's own
    /// error stood in for C's tail. Boundary: the first bad record is a
    /// PARSE refusal and the later two are LOAD refusals, which is the
    /// pair that used to short-circuit in two different places.
    #[epics_macros_rs::epics_test]
    async fn every_bad_record_is_reported_before_the_load_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("three.db");
        std::fs::write(
            &path,
            concat!(
                "record(ai, \"R1\") { field(NOSUCH, \"1\") }\n",
                "record(ai, \"R2\") { field(SCAN, \"Bogus\") }\n",
                "record(ai, \"R3\") { field(INP, \"#zz1 q2\") }\n",
                "record(ai, \"R4\") { field(DESC, \"fine\") }\n",
            ),
        )
        .unwrap();
        let Err(err) = IocBuilder::new()
            .register_record_type("ai", ai_factory)
            .db_file(path.to_str().unwrap(), &HashMap::new())
            .unwrap()
            .build()
            .await
        else {
            panic!("three bad records must fail the load");
        };
        assert_eq!(
            err.to_string(),
            format!("Failed to load '{}'", path.display()),
            "the tail names the file, not the last record's own error",
        );
    }

    /// C creates the record and only THEN puts its fields, so a value
    /// `dbPutString` refuses costs that FIELD and nothing else: the record
    /// stays, keeps the dbd default, and only the load's status goes
    /// non-zero. Measured on softIoc R7.0.10 with `menu4.db` —
    /// `field(SELM,"Bogus")`, `field(LINR,"Nope")`, `field(SCAN,"Bogus")`
    /// and one clean record — `dbl` lists all four, `dbgf S1.SELM` after
    /// `iocInit` is `"Specified"` (the dbd default), and the load ends
    /// `ERROR: Failed to load 'menu4.db'`.
    ///
    /// The screen is what makes that hold: it removes the value before
    /// either apply site can see it, so neither `apply_fields` nor
    /// `add_loaded_record` can fail on a menu value and neither can drop the
    /// record. This asserts the screen's effect on the real parsed
    /// definition — the refusal's WORDING has its own owner and its own test
    /// (`db_loader::tests::the_refusal_is_byte_exact_against_the_reference_ioc`)
    /// and is not restated here.
    #[epics_macros_rs::epics_test]
    async fn a_refused_menu_value_costs_the_field_not_the_record() {
        let db = "record(sel, \"S1\") {\n    field(SELM, \"Bogus\")\n    field(NVL, \"SRC\")\n}\n";

        let Err(err) = IocBuilder::new()
            .db_string(db, &HashMap::new())
            .unwrap()
            .build()
            .await
        else {
            panic!("a refused menu value fails the load's status");
        };
        assert_eq!(
            err.to_string(),
            format!("Failed to load '{}'", db_loader::DB_STRING_SOURCE),
            "the status carries the source, not the field's own error",
        );

        // What the screen leaves for the apply sites: the refused value gone,
        // every other field intact, and the record buildable — which is why
        // the `continue` that used to drop it is now unreachable for a menu
        // value.
        let mut defs = db_loader::parse_db(db, &HashMap::new()).unwrap();
        let def = &mut defs[0];
        assert!(
            db_loader::menu_value_refusal("sel", "S1", "SELM", "Bogus").is_some(),
            "SELM is the record-own menu field this screen exists for"
        );
        def.fields.retain(|f| {
            db_loader::menu_value_refusal(
                "sel",
                "S1",
                &f.name.to_uppercase(),
                &f.value.as_str_lossy(),
            )
            .is_none()
        });
        assert!(
            def.fields
                .iter()
                .any(|f| f.name.eq_ignore_ascii_case("NVL")),
            "only the refused field is dropped",
        );

        let mut record = db_loader::create_record_with_factories("sel", &HashMap::new()).unwrap();
        let mut common = Vec::new();
        db_loader::apply_fields(&mut record, &def.fields, &mut common)
            .expect("after the screen no apply site can see a refused menu value");
        assert_eq!(
            record.get_field("SELM").and_then(|v| v.to_f64()),
            Some(0.0),
            "the field keeps its dbd default, C's \"Specified\"",
        );
    }

    /// The other half of C's rule, and the boundary between the two: a
    /// record whose type cannot be resolved ends in `yyerrorAbort(NULL)`
    /// (`dbLexRoutines.c:1163-1167`), which stops the parse of that FILE, so
    /// the definitions after it never load. Measured on softIoc R7.0.10 with
    /// `record(ai,"N1")`, `record(nosuchtype,"N2")`, `record(ai,"N3")`:
    /// `dbl` lists `N1` alone, and the load ends
    /// `ERROR: Failed to load 'nosuch.db'`.
    ///
    /// The field-level refusals above resume at the next record; this one
    /// does not. Both fail the load — the difference is what still gets
    /// built.
    #[epics_macros_rs::epics_test]
    async fn an_unbuildable_record_type_abandons_the_rest_of_its_file() {
        // `tripwire` stands where C's parser would already have stopped: if
        // the loop resumed at the next record its factory would run, so the
        // panic is the assertion. `register_record_type` itself builds one
        // to snapshot the type's declared fields, so only calls after that
        // first one are the build loop's.
        fn tripwire() -> Box<dyn Record> {
            static BUILT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            assert_eq!(
                BUILT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                0,
                "the file was abandoned at N2; N3 must never be built"
            );
            Box::new(crate::server::records::ai::AiRecord::new(0.0))
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nosuch.db");
        std::fs::write(
            &path,
            concat!(
                "record(ai, \"N1\") { field(DESC, \"first\") }\n",
                "record(nosuchtype, \"N2\") { field(DESC, \"bad\") }\n",
                "record(tripwire, \"N3\") { field(DESC, \"third\") }\n",
            ),
        )
        .unwrap();
        let Err(err) = IocBuilder::new()
            .register_record_type("ai", ai_factory)
            .register_record_type("tripwire", tripwire)
            .db_file(path.to_str().unwrap(), &HashMap::new())
            .unwrap()
            .build()
            .await
        else {
            panic!("an unresolvable record type must fail the load");
        };
        assert_eq!(
            err.to_string(),
            format!("Failed to load '{}'", path.display())
        );
    }

    /// C `softMain` exits on the FIRST `dbLoadRecords` that returns
    /// non-zero (`softMain.cpp:198`), so when several files are bad the
    /// name in the tail is the first of them — the builder reads them all
    /// before settling the status, and must not let the last one rename
    /// the failure.
    #[epics_macros_rs::epics_test]
    async fn the_tail_names_the_first_file_that_failed() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.db");
        let second = dir.path().join("second.db");
        std::fs::write(&first, "record(ai, \"F1\") { field(INP, \"#zz1 q2\") }\n").unwrap();
        std::fs::write(&second, "record(ai, \"S1\") { field(NOSUCH, \"1\") }\n").unwrap();
        let Err(err) = IocBuilder::new()
            .register_record_type("ai", ai_factory)
            .db_file(first.to_str().unwrap(), &HashMap::new())
            .unwrap()
            .db_file(second.to_str().unwrap(), &HashMap::new())
            .unwrap()
            .build()
            .await
        else {
            panic!("two bad files must fail the load");
        };
        assert_eq!(
            err.to_string(),
            format!("Failed to load '{}'", first.display()),
        );
    }

    /// A clean .db still loads, so the gate above cannot be passing by
    /// refusing everything.
    #[epics_macros_rs::epics_test]
    async fn a_clean_db_still_loads() {
        let (db, _) = IocBuilder::new()
            .register_record_type("ai", ai_factory)
            .db_string(
                "record(ai, \"A:GOOD\") { field(VAL, \"1.0\") }",
                &HashMap::new(),
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        assert!(db.has_name("A:GOOD").await);
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
