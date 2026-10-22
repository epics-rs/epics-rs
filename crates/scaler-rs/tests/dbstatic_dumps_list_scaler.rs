//! The `scaler` record type in base's dbStatic reports once its factory ran,
//! as `dbLoadDatabase` of the module's `.dbd` adds it to `pdbbase` in C.
//! rs-only: the C oracle IOC does not build this module.

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

fn run(shell: &IocShell, line: &str) -> String {
    let out = std::env::temp_dir().join(format!("scaler-dbstatic-{}", std::process::id()));
    shell
        // quoted: outside a quote iocsh takes a Windows path's `\` as an escape
        .execute_line(format!("{line} > \"{}\"", out.display()))
        .unwrap();
    let text = std::fs::read_to_string(&out).unwrap();
    std::fs::remove_file(&out).unwrap();
    text
}

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — ran and passed on the
// exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p
// scaler-rs --all-features --test dbstatic_dumps_list_scaler`). The runtime only
// feeds `BlockingBridge::capture` for the shell; no CA server is built.
#[test]
fn the_factory_adds_the_record_type_to_the_dumps_dbstaticlib_c_3304() {
    let _ = scaler_rs::scaler_record_factory();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let bridge = {
        let _guard = rt.enter();
        epics_base_rs::runtime::task::BlockingBridge::capture()
    };
    let shell = IocShell::new(Arc::new(PvDatabase::new()), bridge);
    let fields = scaler_rs::records::dbd_generated::record_declaration_order("scaler")
        .unwrap()
        .len();
    let dump = run(&shell, "dbDumpRecordType pdbbase scaler");
    assert!(
        dump.starts_with(&format!("name(scaler) no_fields({fields}) ")),
        "{dump}"
    );
    assert!(run(&shell, "dbDumpDevice pdbbase scaler").starts_with("recordtype(scaler)\n"));
    assert!(
        run(&shell, "dbDumpField pdbbase scaler VAL").starts_with("recordtype(scaler) \n    VAL\n")
    );
    drop(shell);
    std::mem::forget(rt);
}
