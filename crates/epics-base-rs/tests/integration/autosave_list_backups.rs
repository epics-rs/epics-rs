//! C `readReqFile` turns a list's backups off when a level's macros give
//! `SAVEPATHPV`, `SAVENAMEPV` or `CONFIGMENU` a value through `macGetValue`
//! (save_restore.c:3689-3699); `save_restoreShow` then says "backups: NO".

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::autosave::save_set::SaveSet;

/// `create_*_set("s.req", macros)`'s backups, `s.req` holding `req`.
async fn backups(macros: &str, req: &str) -> bool {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("s.req"), req).unwrap();
    std::fs::write(dir.path().join("inc.req"), "A\n").unwrap();
    let mut startup = AutosaveStartupConfig::new();
    startup.request_file_paths.push(dir.path().to_path_buf());
    SaveSet::new(startup.set_config("s.req", Some(macros)))
        .await
        .unwrap()
        .do_backups()
}

#[epics_macros_rs::epics_test]
async fn a_config_menu_or_pv_named_list_keeps_no_backups() {
    for macros in [
        "",
        "P=x",
        "CONFIGMENU=",
        "E=,CONFIGMENU=$(E)",
        "SAVEPATHPV=$(Q)",
    ] {
        assert!(backups(macros, "A\n").await, "{macros}");
    }
    for macros in [
        "P=x,CONFIGMENU=1",
        "SAVEPATHPV=a:path",
        "SAVENAMEPV=a:name",
        "P=N:,SAVENAMEPV=$(P)name",
    ] {
        assert!(!backups(macros, "A\n").await, "{macros}");
    }
    assert!(!backups("", "file inc.req CONFIGMENU=1\n").await);
    assert!(backups("", "file inc.req CONFIGMENU=$(Q)\n").await);
}
