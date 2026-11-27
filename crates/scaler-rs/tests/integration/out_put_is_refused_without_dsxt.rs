//! A runtime OUT put on an "Asyn Scaler" scaler is refused for want of a
//! `dsxt` (`dbAccess.c:1149-1153`) before `dbPutSpecial` would refuse the
//! NOMOD field (`:1174`), so nothing is printed. `oracleScalerIoc
//! st-scaler.cmd` (scaler 913e2e4, base 8f5015b66):
//!
//! ```text
//! epics> dbpf SC.OUT "@asyn(scalerPort 0 1)"
//! DBF_STRING:         "@asyn(scalerPortC 0 0)"
//! epics> dbpr SC 4        ... OUT : INST_IO @asyn(scalerPortC 0 0)    PACT: 0
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::device_support::DeviceSupport;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset};
use scaler_rs::device_support::scaler_asyn::ScalerAsynDeviceSupport;
use scaler_rs::device_support::scaler_soft::SoftScalerDriver;

/// `scalerSupport.dbd`'s `device(scaler,INST_IO,devScalerAsyn,"Asyn Scaler")`,
/// a row the support's own registration dedups by choice.
static ASYN_SCALER: DevSup = DevSup {
    name: "devScalerAsyn",
    choice: "Asyn Scaler",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(7, &[2, 4, 5, 6], false),
};

const DB: &str = r#"
record(scaler, "SC") {
    field(DTYP, "Asyn Scaler")
    field(OUT,  "@asyn(scalerPortC 0 0)")
}
"#;

fn run(shell: &IocShell, line: &str) -> String {
    let out = std::env::temp_dir().join(format!("scaler-dsxt-{}", std::process::id()));
    shell
        // quoted: outside a quote iocsh takes a Windows path's `\` as an escape
        .execute_line(format!("{line} > \"{}\"", out.display()))
        .unwrap();
    let text = std::fs::read_to_string(&out).unwrap();
    std::fs::remove_file(&out).unwrap();
    text
}

// RTEMS-EXEC-MODEL-ALLOW(1): the runtime only feeds the build and
// `BlockingBridge::capture` for the shell; no CA server is built.
#[test]
fn an_out_put_is_refused_silently_as_in_oracle_scaler_ioc() {
    let heard = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&heard);
    epics_base_rs::runtime::log::errlog_add_listener(move |m| {
        sink.lock().unwrap().push(m.to_string());
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    let (name, factory) = scaler_rs::scaler_record_factory();
    let (db, bridge) = rt.block_on(async {
        let (db, _) = IocBuilder::new()
            .register_record_type(name, factory)
            .register_dynamic_device_support(
                &[("scaler", std::slice::from_ref(&ASYN_SCALER))],
                |ctx| {
                    (ctx.dtyp == "Asyn Scaler").then(|| {
                        Box::new(ScalerAsynDeviceSupport::new(Box::new(
                            SoftScalerDriver::new(8),
                        ))) as Box<dyn DeviceSupport>
                    })
                },
            )
            .db_string(DB, &HashMap::new())
            .unwrap()
            .build()
            .await
            .unwrap();
        (db, epics_base_rs::runtime::task::BlockingBridge::capture())
    });
    let shell = IocShell::new(Arc::clone(&db), bridge);
    epics_base_rs::runtime::log::errlog_flush();
    heard.lock().unwrap().clear();

    assert_eq!(
        run(&shell, r#"dbpf SC.OUT "@asyn(scalerPort 0 1)""#),
        "DBF_STRING:         \"@asyn(scalerPortC 0 0)\"      \n"
    );
    epics_base_rs::runtime::log::errlog_flush();
    let lines: Vec<String> = heard
        .lock()
        .unwrap()
        .iter()
        .flat_map(|m| m.lines().map(str::to_string).collect::<Vec<_>>())
        .filter(|l| l.contains("SC"))
        .collect();
    assert_eq!(lines, Vec::<String>::new(), "errlog");
    let inst = db.get_record("SC").unwrap();
    let inst = inst.read();
    assert!(!inst.is_processing(), "PACT");
    assert!(!inst.common.dtyp.is_detached(), "DSET");
    drop(inst);
    drop(shell);
    std::mem::forget(rt);
}
