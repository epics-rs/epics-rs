//! The `table` record type in base's dbStatic reports once its factory ran,
//! as `dbLoadDatabase` of the module's `.dbd` adds it to `pdbbase` in C.
//! rs-only: the C oracle IOC does not build this module.

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

fn run(shell: &IocShell, line: &str) -> String {
    let out = std::env::temp_dir().join(format!("table-dbstatic-{}", std::process::id()));
    shell
        .execute_line(&format!("{line} > {}", out.display()))
        .unwrap();
    let text = std::fs::read_to_string(&out).unwrap();
    std::fs::remove_file(&out).unwrap();
    text
}

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — ran and passed on the
// exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p
// optics-rs --all-features --test dbstatic_dumps_list_table`). The runtime only
// feeds `BlockingBridge::capture` for the shell; no CA server is built.
#[test]
fn the_factory_adds_the_record_type_to_the_dumps_dbstaticlib_c_3304() {
    let _ = optics_rs::table_record_factory();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let bridge = {
        let _guard = rt.enter();
        epics_base_rs::runtime::task::BlockingBridge::capture()
    };
    let shell = IocShell::new(Arc::new(PvDatabase::new()), bridge);
    let fields = optics_rs::records::dbd_generated::record_declaration_order("table")
        .unwrap()
        .len();
    let dump = run(&shell, "dbDumpRecordType pdbbase table");
    assert!(
        dump.starts_with(&format!("name(table) no_fields({fields}) ")),
        "{dump}"
    );
    assert!(run(&shell, "dbDumpDevice pdbbase table").starts_with("recordtype(table)\n"));
    assert!(
        run(&shell, "dbDumpField pdbbase table VAL").starts_with("recordtype(table) \n    VAL\n")
    );
    drop(shell);
    std::mem::forget(rt);
}
