//! `configMenu.db`'s aSub routines (`configMenuSub.c`): the menu lists the
//! `<CONFIG>_*.cfg` files, and a load or save started by one process is
//! reported by a second one that the completion `scanOnce`s.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::server::autosave::config_menu::{self, ConfigFile};
use epics_base_rs::server::autosave::{AutosaveBuilder, AutosaveStartupConfig, SaveSetConfig};
use epics_base_rs::server::autosave::{BackupConfig, SaveStrategy};
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "TGT") { }
record(stringout, "XMenu:currDesc") { }
record(aSub, "LIST") {
    field(INAM, "configMenuList_init") field(SNAM, "configMenuList_do")
    field(FTA, "STRING") field(FTB, "SHORT") field(FTC, "SHORT")
    field(FTVA, "STRING") field(FTVB, "STRING") field(FTVC, "STRING") field(FTVD, "STRING")
    field(FTVE, "STRING") field(FTVF, "STRING") field(FTVG, "STRING") field(FTVH, "STRING")
    field(FTVI, "STRING") field(FTVJ, "STRING") field(FTVK, "STRING") field(FTVL, "STRING")
    field(FTVM, "STRING") field(FTVN, "STRING") field(FTVO, "STRING") field(FTVP, "STRING")
    field(FTVQ, "STRING") field(FTVR, "STRING") field(FTVS, "STRING") field(FTVT, "STRING")
}
record(aSub, "DO") {
    field(INAM, "configMenu_init") field(SNAM, "configMenu_do")
    field(FTA, "STRING") field(FTB, "LONG") field(FTC, "STRING") field(FTD, "LONG")
    field(FTE, "SHORT") field(FTF, "STRING") field(FTG, "STRING")
    field(FTVA, "LONG") field(FTVB, "LONG") field(FTVC, "LONG")
}
"#;

async fn ioc(holder: Arc<Mutex<AutosaveStartupConfig>>) -> Arc<PvDatabase> {
    let mut b = IocBuilder::new();
    for (name, func) in config_menu::subroutines(holder) {
        b = b.register_context_subroutine(name, func);
    }
    b.db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn holder(dir: &std::path::Path) -> Arc<Mutex<AutosaveStartupConfig>> {
    let mut cfg = AutosaveStartupConfig::new();
    cfg.save_file_path = Some(dir.to_path_buf());
    Arc::new(Mutex::new(cfg))
}

fn put(db: &PvDatabase, rec: &str, field: &str, v: EpicsValue) {
    db.get_record(rec)
        .unwrap()
        .write()
        .record
        .put_field(field, v)
        .unwrap();
}

fn get(db: &PvDatabase, rec: &str, field: &str) -> Option<EpicsValue> {
    db.get_record(rec).unwrap().read().record.get_field(field)
}

fn s(v: &str) -> EpicsValue {
    EpicsValue::String(v.into())
}

async fn process(db: &PvDatabase, rec: &str) {
    db.process_record_with_links(rec, &mut ProcStack::new())
        .await
        .unwrap();
}

/// Start an operation on `DO` and wait for the completion's second process
/// to drop VALA back to 0.
async fn run_do(db: &PvDatabase, name: &str, save: bool, req: &str) {
    put(db, "DO", "A", s(name));
    put(db, "DO", "C", s("Done"));
    put(db, "DO", "E", EpicsValue::Short(i16::from(save)));
    put(db, "DO", "F", s(req));
    put(db, "DO", "G", s("CFG"));
    process(db, "DO").await;
    assert_eq!(get(db, "DO", "VALA"), Some(EpicsValue::Long(1)), "busy set");
    assert_eq!(get(db, "DO", "B"), Some(EpicsValue::Long(0)), "queued");
    // The second process runs with the busy record no longer "Done".
    put(db, "DO", "C", s("Loading"));
    for _ in 0..200 {
        if get(db, "DO", "VALA") == Some(EpicsValue::Long(0)) {
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
    }
    panic!("the completion never re-processed DO");
}

#[test]
fn find_config_files_lists_matching_cfg_files_with_descriptions() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("CFG_alpha.cfg"),
        "TGT 5\nXMenu:currDesc first one\r\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("CFG_beta.cfg"), "TGT 6\n").unwrap();
    std::fs::write(dir.path().join("CFG_gamma.sav"), "").unwrap();
    std::fs::write(dir.path().join("OTHER_delta.cfg"), "").unwrap();

    let mut found =
        config_menu::find_config_files(Some(dir.path()), "CFG", &Default::default()).unwrap();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(
        found,
        vec![
            ConfigFile {
                name: "alpha".into(),
                description: Some("first one".into()),
            },
            ConfigFile {
                name: "beta".into(),
                description: None,
            },
        ]
    );
    assert_eq!(
        config_menu::find_config_files(None, "CFG", &Default::default()),
        Err(-1)
    );
    assert_eq!(
        config_menu::find_config_files(
            Some(&dir.path().join("missing")),
            "CFG",
            &Default::default()
        ),
        Err(-1)
    );
}

#[test]
fn make_legal_replaces_every_non_alphanumeric() {
    assert_eq!(config_menu::make_legal("a b/c.1"), "a_b_c_1");
}

#[epics_macros_rs::epics_test]
async fn the_list_fills_names_and_descriptions_from_the_save_directory() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("CFG_alpha.cfg"),
        "XMenu:currDesc first one\n",
    )
    .unwrap();
    let db = ioc(holder(dir.path())).await;
    put(&db, "LIST", "A", s("CFG"));
    put(&db, "LIST", "C", EpicsValue::Short(1));
    process(&db, "LIST").await;
    assert_eq!(get(&db, "LIST", "VALA"), Some(s("alpha")));
    assert_eq!(get(&db, "LIST", "VALK"), Some(s("first one")));
    assert_eq!(get(&db, "LIST", "VALB"), Some(s("")));
    assert_eq!(get(&db, "LIST", "C"), Some(EpicsValue::Short(0)));
}

#[epics_macros_rs::epics_test]
async fn a_load_completes_with_the_restored_values_and_success() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("CFG_alpha.cfg"),
        "# no header needed\nTGT 5\nXMenu:currDesc first one\n",
    )
    .unwrap();
    let db = ioc(holder(dir.path())).await;
    run_do(&db, "alpha", false, "CFGMenu.req").await;
    assert_eq!(get(&db, "TGT", "VAL"), Some(EpicsValue::Double(5.0)));
    assert_eq!(get(&db, "XMenu:currDesc", "VAL"), Some(s("first one")));
    assert_eq!(get(&db, "DO", "D"), Some(EpicsValue::Long(0)));
    assert_eq!(get(&db, "DO", "VALC"), Some(EpicsValue::Long(0)));
}

#[epics_macros_rs::epics_test]
async fn a_load_of_a_missing_file_completes_with_failure() {
    let dir = tempfile::tempdir().unwrap();
    let db = ioc(holder(dir.path())).await;
    run_do(&db, "nope", false, "CFGMenu.req").await;
    assert_eq!(get(&db, "DO", "D"), Some(EpicsValue::Long(-1)));
    assert_eq!(get(&db, "DO", "VALC"), Some(EpicsValue::Long(1)));
}

async fn holder_with_set(dir: &std::path::Path) -> Arc<Mutex<AutosaveStartupConfig>> {
    std::fs::write(dir.join("CFGMenu.req"), "TGT\nXMenu:currDesc\n").unwrap();
    let mgr = AutosaveBuilder::new()
        .add_set(
            SaveSetConfig {
                name: "CFGMenu".into(),
                save_path: dir.join("CFGMenu.sav"),
                request_file: Some("CFGMenu.req".into()),
                request_pvs: Vec::new(),
                backup: BackupConfig::default(),
                macros: HashMap::new(),
                macro_string: String::new(),
                search_paths: vec![dir.to_path_buf()],
            },
            SaveStrategy::Manual,
        )
        .build()
        .await;
    let h = holder(dir);
    h.lock().unwrap().manager = Some(Arc::new(mgr));
    h
}

#[epics_macros_rs::epics_test]
async fn a_save_writes_the_named_cfg_and_completes_with_success() {
    let dir = tempfile::tempdir().unwrap();
    let db = ioc(holder_with_set(dir.path()).await).await;
    put(&db, "TGT", "VAL", EpicsValue::Double(7.0));
    put(&db, "XMenu:currDesc", "VAL", s("seven"));
    run_do(&db, "my cfg", true, "CFGMenu.req").await;
    assert_eq!(get(&db, "DO", "D"), Some(EpicsValue::Long(0)));
    assert_eq!(get(&db, "DO", "VALC"), Some(EpicsValue::Long(0)));
    let cfg = std::fs::read_to_string(dir.path().join("CFG_my_cfg.cfg")).unwrap();
    assert!(cfg.contains("TGT 7"), "{cfg}");
    assert!(cfg.contains("XMenu:currDesc seven"), "{cfg}");
    assert!(
        !dir.path().join("CFGMenu.sav").exists(),
        "a config save leaves the set's own file alone"
    );
}

#[epics_macros_rs::epics_test]
async fn a_save_for_a_request_file_no_set_uses_completes_with_failure() {
    let dir = tempfile::tempdir().unwrap();
    let db = ioc(holder_with_set(dir.path()).await).await;
    run_do(&db, "x", true, "Other.req").await;
    assert_eq!(get(&db, "DO", "D"), Some(EpicsValue::Long(-1)));
    assert_eq!(get(&db, "DO", "VALC"), Some(EpicsValue::Long(1)));
}

#[epics_macros_rs::epics_test]
async fn an_empty_name_fails_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = ioc(holder(dir.path())).await;
    put(&db, "DO", "C", s("Done"));
    process(&db, "DO").await;
    assert_eq!(get(&db, "DO", "D"), Some(EpicsValue::Long(1)));
    assert_eq!(get(&db, "DO", "VALC"), Some(EpicsValue::Long(1)));
    assert_eq!(get(&db, "DO", "VALA"), Some(EpicsValue::Long(0)));
}
