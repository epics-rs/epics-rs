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
use super::database::PvDatabase;
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
    /// One entry per `.db` text taken in, in the order it was loaded.
    loads: Vec<DbLoad>,
    /// What a failed load does to the build: `false` is `softIoc -d`, which
    /// exits before `iocInit`; `true` is a `dbLoadRecords` line in a startup
    /// script, which reports and lets the script go on to `iocInit`.
    boot_past_failed_loads: bool,
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
    link_set_installers: Vec<crate::server::ioc_app::LinkSetInstaller>,
    /// The link types those installers serve ([`Self::register_link_type`]).
    json_link_types: Vec<String>,
    /// The database the build fills, when its caller made it first so a
    /// port can be registered to its `interruptAccept` before the records
    /// load.
    database: Option<Arc<PvDatabase>>,
}

/// Whether the build runs the link passes of C `initDatabase`.
enum LinkInit {
    /// An in-process fixture: links are left as loaded, for the caller.
    Caller,
    /// An IOC process: the build runs them, as `IocApplication`'s does.
    Build,
}

/// One `.db` text, parsed and waiting for `build` to install it as C's
/// `dbLoadRecords` would.
struct DbLoad {
    /// What C quotes in `ERROR: Failed to load '%s'`.
    source: String,
    /// `None` for a file `dbReadCOM` could not open: a failed load with
    /// nothing to install, reported in its turn.
    text: Option<DbText>,
}

/// What one opened `.db` text parsed to.
struct DbText {
    records: Vec<db_loader::DbRecordDef>,
    file_aliases: Vec<db_loader::DbFileAlias>,
    aborted: Option<db_loader::ParseAbort>,
    /// Every positioned diagnostic of the parse, released as `build`
    /// installs this load, so the console reads in C's file order.
    faults: db_loader::DbFaults,
}

/// The builder's side of [`db_loader::install_records`].
struct BuilderInstall<'a> {
    db: &'a PvDatabase,
    record_factories: &'a HashMap<String, RecordFactory>,
}

impl db_loader::InstallHost for BuilderInstall<'_> {
    fn db(&self) -> &PvDatabase {
        self.db
    }

    fn records_once_only(&self) -> bool {
        crate::server::iocsh::db_records_once_only()
    }

    fn create_record(&self, record_type: &str) -> Option<Box<dyn Record>> {
        db_loader::create_record_with_factories(record_type, self.record_factories).ok()
    }

    fn warn(&self, line: &str) {
        eprintln!("{line}");
    }
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
            loads: Vec::new(),
            boot_past_failed_loads: false,
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
            link_set_installers: Vec::new(),
            json_link_types: Vec::new(),
            database: None,
        }
    }

    /// Build into `db` instead of a fresh database. The caller owns the
    /// database's lifecycle until `build`; it must be a database no build
    /// has loaded into.
    pub fn with_database(mut self, db: Arc<PvDatabase>) -> Self {
        self.database = Some(db);
        self
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
    /// A recoverable diagnostic in the text, a syntax error (once the records
    /// above it are installed) or a file that cannot be opened is a failed
    /// load, C `dbLoadRecords` returning non-zero (`dbAccess.c:795-813`);
    /// [`IocBuilder::boot_past_failed_loads`] decides what
    /// [`IocBuilder::build`] does next.
    ///
    /// The read is `dbLoadRecords`' own: the file is found on the include
    /// path list and its `include`s resolve there, as `softIoc -d` does.
    pub fn db_file(mut self, path: &str, macros: &HashMap<String, String>) -> CaResult<Self> {
        let (config, opened) = db_loader::resolve_db_file(path, "");
        let Some(opened) = opened else {
            self.loads.push(DbLoad {
                source: path.to_string(),
                text: None,
            });
            return Ok(self);
        };
        let parsed = db_loader::parse_db_opened_held(&opened, macros, &config)?;
        self.absorb_parsed(path, parsed);
        Ok(self)
    }

    /// Load records from a .db string. Settles its status exactly as
    /// [`IocBuilder::db_file`] does.
    pub fn db_string(mut self, content: &str, macros: &HashMap<String, String>) -> CaResult<Self> {
        let parsed = db_loader::parse_db_held(content, macros);
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
        self.breaktables.extend(parsed.breaktables);
        self.loads.push(DbLoad {
            source: source.to_string(),
            text: Some(DbText {
                records: parsed.records,
                file_aliases: parsed.file_aliases,
                aborted: parsed.aborted,
                faults: parsed.faults,
            }),
        });
    }

    /// Register a device support: its factory under `devsup.choice`, and
    /// `devsup` as a row of `record_type`'s device menu.
    pub fn register_device_support<F>(
        mut self,
        record_type: &str,
        devsup: &'static crate::server::record::DevSup,
        factory: F,
    ) -> Self
    where
        F: Fn() -> Box<dyn device_support::DeviceSupport> + Send + Sync + 'static,
    {
        crate::server::record::register_device_object_row(record_type, devsup);
        self.device_factories
            .insert(devsup.choice.to_string(), Box::new(factory));
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

    /// Load as a startup script's `dbLoadRecords` does rather than as
    /// `softIoc -d`: the loads after a failed one still run and the build
    /// still boots every record that parsed, a refused field keeping whatever
    /// its `dbPutString` left (`dbLexRoutines.c:1405-1415` is `yyerror(NULL)`,
    /// not an abort).
    pub fn boot_past_failed_loads(mut self) -> Self {
        self.boot_past_failed_loads = true;
        self
    }

    /// Configure autosave with a save set configuration.
    pub fn autosave(mut self, config: autosave::SaveSetConfig) -> Self {
        self.autosave_config = Some(config);
        self
    }

    /// Add the link type `key`, declared on the database ahead of its
    /// records, and the installer of its link set, run at C `dbCaLinkInit`
    /// before any record initialises — the builder twin of
    /// [`IocApplication::register_link_type`](crate::server::ioc_app::IocApplication::register_link_type).
    ///
    /// # Panics
    ///
    /// When `key` already has an installer.
    pub fn register_link_type<F, Fut>(mut self, key: &str, installer: F) -> Self
    where
        F: FnOnce(Arc<PvDatabase>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Vec<crate::server::iocsh::registry::CommandDef>>
            + Send
            + 'static,
    {
        assert!(
            !self.json_link_types.iter().any(|k| k == key),
            "link type '{key}' already has its link set installer"
        );
        self.json_link_types.push(key.to_string());
        self.link_set_installers
            .push(Box::new(move |db| Box::pin(installer(db))));
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
    ///
    /// The database has no IOC lifecycle: `iocState` is not touched and
    /// nothing of `iocBuild` is said. An IOC process builds with
    /// [`Self::build_ioc`] instead.
    pub async fn build(self) -> CaResult<(Arc<PvDatabase>, Option<autosave::SaveSetConfig>)> {
        let (db, autosave_config, ()) = self.build_through(|| Some(()), LinkInit::Caller).await?;
        Ok((db, autosave_config))
    }

    /// Build the database as an IOC process's `iocBuild`: the same steps as
    /// [`Self::build`], with C `iocBuild_1` — `Starting iocInit`, the
    /// `coreRelease` banner, `iocBuilding` — between the record load and the
    /// first `init_record`, where `dbLoadRecords` ends and `iocInit` begins,
    /// then `initialProcess` (PINI=YES) and `iocBuilt` at the end. Run it
    /// with [`BareIoc::run`].
    ///
    /// Once per process: a second call is refused as C's `iocBuild` is.
    pub async fn build_ioc(self) -> CaResult<BareIoc> {
        let (db, autosave_config, build) = self
            .build_through(crate::server::ioc_app::BareBuild::begin, LinkInit::Build)
            .await?;
        crate::server::ioc_app::initial_process(&db);
        build.finish();
        Ok(BareIoc {
            db,
            autosave_config,
        })
    }

    /// `build`'s body. `iocinit_begins` runs once the load has succeeded and
    /// before any deferred `init_record`; `None` from it fails the build.
    async fn build_through<T>(
        self,
        iocinit_begins: impl FnOnce() -> Option<T>,
        link_init: LinkInit,
    ) -> CaResult<(Arc<PvDatabase>, Option<autosave::SaveSetConfig>, T)> {
        let db = self
            .database
            .clone()
            .unwrap_or_else(|| Arc::new(PvDatabase::new()));
        for key in &self.json_link_types {
            db.declare_link_type(key);
        }

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

        // C's device support table and function registry, installed on the
        // database BEFORE the first record is created — `registerRecord\
        // DeviceDriver` before `dbLoadRecords`. The creation sink binds each
        // record's dset from the first ahead of `init_record(0)` and resolves
        // INAM/SNAM against the second from inside pass 1, which is C's order;
        // this build used to do both AFTER the record's passes had already
        // run, so `init_record` could not see its own dset and ran the tail C
        // returns before.
        db.install_device_support_resolver(crate::server::ioc_app::device_support_resolver(
            self.device_factories,
            self.dynamic_device_factory,
        ));
        db.install_subroutine_registry(self.subroutine_registry)
            .await;

        // 1. Simple PVs
        for (name, value) in self.pvs {
            db.add_pv(&name, value).await?;
        }

        // 2. Inline records
        for (name, record) in self.records {
            // The sink owes the record its `iocInit` passes, as it owes a
            // `.db` record's: `begin_load` above opened the LOAD phase, so
            // `drain_deferred_record_inits` runs them after `iocinit_begins`.
            // Common-link classification (init_links) and the mbboDirect UDF
            // finalisation stay on the `.db` path only: an inline record has
            // no parsed common fields
            // to classify or fold. The sink also ends in the init-seed owner
            // (`seed_constant_links`), which runs BOTH halves of C's
            // `init_record` tail — this loop must not call either half itself,
            // or it decides for the record which half of its tail runs.
            db.add_record(&name, record).await?;
        }

        // 3. .db definitions — each load installed as C's `dbLoadRecords`
        // reads it, through the same per-record install as iocsh, and a failed
        // one prints its tail as `dbLoadRecords` does (`dbAccess.c`).
        let host = BuilderInstall {
            db: &db,
            record_factories: &self.record_factories,
        };
        let mut loads = self.loads.into_iter();
        while let Some(load) = loads.next() {
            let installed = match load.text {
                Some(mut text) => db_loader::install_records(
                    &host,
                    text.records,
                    text.file_aliases,
                    text.aborted,
                    &breaktable_registry,
                    &mut text.faults,
                )
                .await
                .is_ok(),
                None => {
                    eprintln!("{ERL_ERROR}: Can't open file '{}'", load.source);
                    false
                }
            };
            if !installed {
                eprintln!("{ERL_ERROR}: Failed to load '{}'", load.source);
                if !self.boot_past_failed_loads {
                    for unread in loads {
                        if let Some(mut text) = unread.text {
                            text.faults.stop_reading();
                        }
                    }
                    return Err(CaError::DbLoadFailed(load.source));
                }
            }
        }
        let Some(begun) = iocinit_begins() else {
            return Err(CaError::InvalidValue(
                "IOC can only be initialized from uninitialized or stopped state".into(),
            ));
        };
        // C `iocBuild` runs `dbCaLinkInit` right after `iocBuild_1`
        // (`iocInit.c:217`), so a link set exists before `init_record` and the
        // link passes below.
        crate::server::ioc_app::ca_link_init(&db, self.link_set_installers).await;

        // 4. Autosave restore
        if let Some(ref autosave_cfg) = self.autosave_config {
            let count = autosave::restore_from_file(&db, &autosave_cfg.save_path).await?;
            if count > 0 {
                eprintln!("autosave: restored {count} PVs");
            }
        }

        // 4.5. C's `initDatabase` per-record pass: bind each record's dset and
        // run its `init_record` now that every record has loaded — deferred from
        // load time so a record's device-support port could be configured first.
        // Before `setup_io_intr` (C's `scanInit`), so a dset is bound before I/O
        // Intr wiring reads it.
        db.drain_deferred_record_inits();

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
        if let LinkInit::Build = link_init {
            crate::server::ioc_app::init_links(&db).await;
        }

        // 7. The `iocInit` barrier: every record exists, so run the link-status
        // classifications queued during the load. Link status is FINAL when
        // `build` returns, as it is when C's `iocInit` returns.
        db.ioc_init().await;

        Ok((db, self.autosave_config, begun))
    }
}

/// An IOC [`IocBuilder::build_ioc`] built: C's `iocBuilt`, quiescent.
pub struct BareIoc {
    db: Arc<PvDatabase>,
    autosave_config: Option<autosave::SaveSetConfig>,
}

impl BareIoc {
    pub fn database(&self) -> &Arc<PvDatabase> {
        &self.db
    }

    pub fn autosave_config(&self) -> Option<&autosave::SaveSetConfig> {
        self.autosave_config.as_ref()
    }

    /// C `iocRun` (`iocInit.c:246-276`); the scan owner it returns is held
    /// for as long as the IOC scans. `None` when `iocRun` refused, which it
    /// has already reported.
    pub async fn run(self) -> Option<crate::server::scan::ScanOwner> {
        crate::server::ioc_app::run_built(&self.db).await
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

    /// A head adding the link set its runner already installs is refused at
    /// the call, not left to install two.
    #[test]
    #[should_panic(expected = "link type 'ca' already has its link set installer")]
    fn a_second_installer_for_one_link_type_is_refused() {
        let _ = IocBuilder::new()
            .register_link_type("ca", |_db| async { Vec::new() })
            .register_link_type("ca", |_db| async { Vec::new() });
    }

    /// An inline record's `init_record` runs after `iocBuild_1`, as a `.db`
    /// record's does: the build is in its LOAD phase when step 2 adds it.
    #[epics_macros_rs::epics_test]
    async fn an_inline_record_initialises_after_ioc_init_begins() {
        use crate::server::record::{FieldDesc, ProcessOutcome};
        use std::sync::Mutex;

        struct InitProbe(Arc<Mutex<Vec<String>>>);
        impl Record for InitProbe {
            fn record_type(&self) -> &'static str {
                "init_probe"
            }
            fn init_record(&mut self, pass: u8) -> CaResult<()> {
                self.0.lock().unwrap().push(format!("init_record({pass})"));
                Ok(())
            }
            fn process(&mut self) -> CaResult<ProcessOutcome> {
                Ok(ProcessOutcome::complete())
            }
            fn get_field(&self, _name: &str) -> Option<EpicsValue> {
                None
            }
            fn put_field(&mut self, _name: &str, _value: EpicsValue) -> CaResult<()> {
                Ok(())
            }
            fn declared_fields(&self) -> &'static [FieldDesc] {
                &[]
            }
        }

        let log = Arc::new(Mutex::new(Vec::new()));
        let begun = Arc::clone(&log);
        IocBuilder::new()
            .record("INLINE", InitProbe(Arc::clone(&log)))
            .build_through(
                move || {
                    begun.lock().unwrap().push("iocBuild_1".to_string());
                    Some(())
                },
                LinkInit::Caller,
            )
            .await
            .unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            ["iocBuild_1", "init_record(0)", "init_record(1)"]
        );
    }

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

    /// The same refusal under a startup script's `dbLoadRecords` boots: C
    /// oracleSwaitIoc on `record(ai,"AI"){field(SCAN,"bogus") field(DESC,
    /// "kept")}` prints `Failed to load` and iocInit runs, `dbgf AI.SCAN`
    /// `"Passive"`, `AI.DESC` `"kept"`.
    #[epics_macros_rs::epics_test]
    async fn a_script_load_boots_past_its_refused_field() {
        let (db, _) = IocBuilder::new()
            .db_string(
                "record(ai, \"AI\") { field(SCAN, \"bogus\") field(DESC, \"kept\") }\n",
                &HashMap::new(),
            )
            .unwrap()
            .boot_past_failed_loads()
            .build()
            .await
            .expect("a script's failed load still boots");
        assert_eq!(db.get_pv("AI.SCAN").unwrap().to_f64(), Some(0.0), "Passive");
        assert_eq!(db.get_pv("AI.DESC").unwrap().to_string(), "kept");
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

    /// A `SPC_CALC` string `postfix()` refuses fails the load's status, as
    /// C's `softIoc -d` does (exit 2, `Failed to load`), whichever record type
    /// owns the field; the same text compiling loads.
    #[epics_macros_rs::epics_test]
    async fn a_calc_expression_that_does_not_compile_fails_the_load() {
        for db in [
            "record(calc, \"R\") { field(CALC, \"A+\") }\n",
            "record(calcout, \"R\") { field(OCAL, \"(\") }\n",
            "record(swait, \"R\") { field(CALC, \"1)\") }\n",
        ] {
            let result = IocBuilder::new()
                .db_string(db, &HashMap::new())
                .unwrap()
                .build()
                .await;
            let Err(err) = result else {
                panic!("{db:?} must fail the load");
            };
            assert_eq!(
                err.to_string(),
                format!("Failed to load '{}'", db_loader::DB_STRING_SOURCE),
                "{db:?}"
            );
        }
        IocBuilder::new()
            .db_string(
                "record(calc, \"R\") { field(CALC, \"A+1\") }\n",
                &HashMap::new(),
            )
            .unwrap()
            .build()
            .await
            .expect("a CALC that compiles loads");
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

    /// A st.cmd's policy: the failed file keeps its other records, the next
    /// file loads, and the build reaches `iocInit`.
    #[epics_macros_rs::epics_test]
    async fn continue_loads_past_a_failed_file() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.db");
        let second = dir.path().join("second.db");
        let missing = dir.path().join("missing.db");
        std::fs::write(
            &first,
            "record(ai, \"F1\") { }\nrecord(ai, \"F2\") { field(SCAN, \"Bogus\") }\n",
        )
        .unwrap();
        std::fs::write(&second, "record(ai, \"S1\") { }\n").unwrap();
        let (db, _) = IocBuilder::new()
            .register_record_type("ai", ai_factory)
            .boot_past_failed_loads()
            .db_file(first.to_str().unwrap(), &HashMap::new())
            .unwrap()
            .db_file(missing.to_str().unwrap(), &HashMap::new())
            .unwrap()
            .db_file(second.to_str().unwrap(), &HashMap::new())
            .unwrap()
            .build()
            .await
            .expect("a st.cmd goes on");
        for rec in ["F1", "F2", "S1"] {
            assert!(db.get_record(rec).is_some(), "{rec}");
        }
    }

    /// `softIoc -d nope.db` is `Can't open file` then `Failed to load` and
    /// exit 2: an unopened file is a failed load like any other.
    #[epics_macros_rs::epics_test]
    async fn stop_fails_the_build_at_a_missing_file() {
        let Err(err) = IocBuilder::new()
            .db_file("/nonexistent/missing.db", &HashMap::new())
            .unwrap()
            .build()
            .await
        else {
            panic!("a missing file fails the load");
        };
        assert_eq!(err.to_string(), "Failed to load '/nonexistent/missing.db'");
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
