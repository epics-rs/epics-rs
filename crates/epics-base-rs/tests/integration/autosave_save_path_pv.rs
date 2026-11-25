//! A list made with `SAVEPATHPV`/`SAVENAMEPV` macros is written where those
//! PVs say (save_restore.c:2018-2039): the path PV's directory, under the
//! save-file path when relative, and the name PV's file. A PV holding ""
//! writes nothing. C reads them once connected on the list's first pass
//! (:936-949), which warns when they cannot be.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::runtime::task::sleep;
use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::{AutosaveBuilder, AutosaveManager};
use epics_base_rs::server::autosave::save_file::read_save_file;
use epics_base_rs::server::autosave::save_set::{SaveSetConfig, SaveStrategy, SrStatus};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::stringout::StringoutRecord;

/// The oracle's `a.db`: PP "sub", NN "named.sav", EE "".
async fn db(path: &str) -> Arc<PvDatabase> {
    let db = Arc::new(PvDatabase::new());
    db.add_record("A", Box::new(AoRecord::new(3.0)))
        .await
        .unwrap();
    for (name, val) in [("PP", path), ("NN", "named.sav"), ("EE", "")] {
        db.add_record(name, Box::new(StringoutRecord::new(val)))
            .await
            .unwrap();
    }
    db
}

/// `create_manual_set("a.req", macros)` saving under `sav`; a list with a
/// path or name PV keeps no backups (save_restore.c:3697).
async fn manager(sav: &Path, macros: &[(&str, &str)]) -> Arc<AutosaveManager> {
    let req = sav.parent().unwrap();
    std::fs::write(req.join("a.req"), "A\n").unwrap();
    let config = SaveSetConfig {
        name: "a".into(),
        save_path: sav.join("a.sav"),
        request_file: Some("a.req".into()),
        request_pvs: vec!["A".into()],
        backup: BackupConfig { enable_savb: false },
        macros: macros
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<HashMap<_, _>>(),
        macro_string: None,
        search_paths: vec![req.to_path_buf()],
    };
    Arc::new(
        AutosaveBuilder::new()
            .add_set(config, SaveStrategy::Manual)
            .build()
            .await,
    )
}

/// Every file under `dir`, `/`-separated on every OS.
fn files(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for e in walk(dir) {
        let rel = e.strip_prefix(dir).unwrap();
        let parts: Vec<_> = rel.iter().map(|p| p.to_string_lossy()).collect();
        found.push(parts.join("/"));
    }
    found.sort();
    found
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

async fn saved_a(path: &Path) -> f64 {
    let contents = read_save_file(path).await.unwrap().unwrap();
    assert_eq!(contents.entries[0].pv_name, "A");
    contents.entries[0].value.parse().unwrap()
}

/// The oracle: PP "sub" and NN "named.sav" wrote `sav/sub/named.sav`,
/// no `.savB`, `manual_save` status 0.
#[epics_macros_rs::epics_test]
async fn the_path_and_name_pvs_place_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let sav = dir.path().join("sav");
    std::fs::create_dir_all(sav.join("sub")).unwrap();
    let db = db("sub").await;
    let mgr = manager(&sav, &[("SAVEPATHPV", "PP"), ("SAVENAMEPV", "NN")]).await;
    let status = mgr.save_file_op(&db, "a.req", "", &sav.join("")).await;
    assert_eq!(status, 0);
    assert_eq!(files(&sav), ["sub/named.sav"]);
    assert_eq!(saved_a(&sav.join("sub/named.sav")).await, 3.0);
}

/// An absolute path PV is the directory itself (:2023). C reads the PV as
/// one DBR_STRING (:2021), so the directory's path must fit in 39
/// characters, which macOS's and Windows' own temp directories do not.
#[epics_macros_rs::epics_test]
async fn an_absolute_path_pv_is_the_directory() {
    let dir = tempfile::tempdir().unwrap();
    let sav = dir.path().join("sav");
    std::fs::create_dir_all(&sav).unwrap();
    #[cfg(unix)]
    let root = std::path::PathBuf::from("/tmp");
    #[cfg(windows)]
    let root = std::env::temp_dir()
        .ancestors()
        .last()
        .unwrap()
        .to_path_buf();
    let short = tempfile::tempdir_in(root).unwrap();
    let abs = short.path();
    let db = db(abs.to_str().unwrap()).await;
    let mgr = manager(&sav, &[("SAVEPATHPV", "PP")]).await;
    let status = mgr.save_file_op(&db, "a.req", "", &sav.join("")).await;
    assert_eq!(status, 0);
    assert!(files(&sav).is_empty(), "{:?}", files(&sav));
    assert_eq!(files(abs), ["a.sav"]);
}

/// The oracle's `b.req`: EE "" wrote nothing, and its asVerify of ""
/// made `manual_save` -1 (:2035, :1225).
#[epics_macros_rs::epics_test]
async fn an_empty_name_pv_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let sav = dir.path().join("sav");
    std::fs::create_dir_all(&sav).unwrap();
    let db = db("sub").await;
    let mgr = manager(&sav, &[("SAVENAMEPV", "EE")]).await;
    let status = mgr.save_file_op(&db, "a.req", "", &sav.join("")).await;
    assert_eq!(status, -1);
    assert!(files(&sav).is_empty(), "{:?}", files(&sav));
    // write_save_file returned OK before writing (:2012-2013, :2035).
    let list = mgr.sets()[0].0.list_status();
    assert_eq!((list.code, list.text.as_str()), (SrStatus::Ok, "Ok"));
}

/// :936-949 on the list's first pass, and the show's lines (:2547-2556).
#[epics_macros_rs::epics_test]
async fn an_unresolved_path_pv_warns_on_the_first_pass() {
    let dir = tempfile::tempdir().unwrap();
    let sav = dir.path().join("sav");
    std::fs::create_dir_all(&sav).unwrap();
    let db = db("sub").await;
    let mgr = manager(&sav, &[("SAVEPATHPV", "NOPV"), ("SAVENAMEPV", "NN")]).await;
    let task = mgr.clone().start(db.clone());
    let set = mgr.sets()[0].0.clone();
    // The same pass's connect_list (:955) and then enable_list (:973)
    // rewrite the text, not the code.
    let want = (SrStatus::Warn, "Enabling list...");
    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while std::time::Instant::now() < deadline {
        let status = set.list_status();
        if (status.code, status.text.as_str()) == want {
            break;
        }
        sleep(Duration::from_millis(50)).await;
    }
    let status = set.list_status();
    assert_eq!((status.code, status.text.as_str()), want);
    let lines = Mutex::new(Vec::new());
    mgr.show_lists(&db, &|a| lines.lock().unwrap().push(a.to_string()), 0);
    mgr.shutdown();
    drop(task);
    let lines = lines.into_inner().unwrap();
    let at = lines.iter().position(|l| l == "    path PV: NOPV").unwrap();
    assert_eq!(
        lines[at..at + 4],
        [
            "    path PV: NOPV",
            "        path: ''",
            "    name PV: NN",
            "        name: 'named.sav'",
        ]
    );
}

/// A list with no PVs shows `None` for both.
#[epics_macros_rs::epics_test]
async fn a_list_without_pvs_shows_none() {
    let dir = tempfile::tempdir().unwrap();
    let db = db("sub").await;
    let mgr = manager(&dir.path().join("sav"), &[]).await;
    let lines = Mutex::new(Vec::new());
    mgr.show_lists(&db, &|a| lines.lock().unwrap().push(a.to_string()), 0);
    let lines = lines.into_inner().unwrap();
    let at = lines.iter().position(|l| l == "    path PV: None").unwrap();
    assert_eq!(lines[at + 1], "    name PV: None");
}

/// A path PV holding "" writes nothing either (:2023-2025), where the
/// standard directory would otherwise take the file.
#[epics_macros_rs::epics_test]
async fn an_empty_path_pv_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let sav = dir.path().join("sav");
    std::fs::create_dir_all(&sav).unwrap();
    let db = db("").await;
    let mgr = manager(&sav, &[("SAVEPATHPV", "PP")]).await;
    let status = mgr.save_file_op(&db, "a.req", "", &sav.join("")).await;
    assert_eq!(status, -1);
    assert!(files(&sav).is_empty(), "{:?}", files(&sav));
}

/// A named `manual_save` goes in the path PV's directory too (:2018-2030).
#[epics_macros_rs::epics_test]
async fn a_named_save_goes_in_the_path_pvs_directory() {
    let dir = tempfile::tempdir().unwrap();
    let sav = dir.path().join("sav");
    std::fs::create_dir_all(sav.join("sub")).unwrap();
    let db = db("sub").await;
    let mgr = manager(&sav, &[("SAVEPATHPV", "PP")]).await;
    let status = mgr
        .save_file_op(&db, "a.req", "x.sav", &sav.join("x.sav"))
        .await;
    assert_eq!(status, 0);
    assert_eq!(files(&sav), ["sub/x.sav"]);
}
