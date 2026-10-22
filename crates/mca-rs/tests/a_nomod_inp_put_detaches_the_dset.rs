//! A runtime INP put on a "Soft Channel" mca passes the `dsxt` gate, removes
//! the link, and is refused by `dbPutSpecial`'s SPC_NOMOD (`dbAccess.c:1167`,
//! `:1174`, `:1183-1188`): the text stays, the link is unadded, and the record
//! is left with no dset and PACT set. A scratch C IOC (base 8f5015b66 plus
//! mca's `mcaRecord.c` and `device(mca,CONSTANT,devMCA_soft,"Soft Channel")`)
//! on this database:
//!
//! ```text
//! epics> dbpf MA.INP WF2
//! recGblDbaddrError: dbPut Attempt to modify noMod field PV: MA.INP
//! DBF_STRING:         "WF NPP NMS"
//! epics> dbpr MA 4   ... DPVT: PTR (nil)  DSET: PTR (nil)
//!                        INP : PV_LINK WF NPP NMS  PACT: 1
//! epics> dbpf MA.PROC 1
//! DBF_UCHAR:          1 = 0x1
//! epics> dbgf MA.RPRO
//! DBF_UCHAR:          1 = 0x1
//! epics> dbpf MA.INP WF
//! recGblDbaddrError: dbPut Attempt to modify noMod field PV: MA.INP
//! DBF_STRING:         "WF NPP NMS"
//! ```
//!
//! The removed link takes its CP subscription with it (`dbRemoveLink`), so
//! with `INP "WF CP"` three `dbpf WF.PROC 1` after the refusal leave
//! `MA.LCNT` at 0, as does a `dbpf WF2.PROC 1`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, register_device_row};

/// `mcaSupport.dbd`'s `device(mca,CONSTANT,devMCA_soft,"Soft Channel")`, a
/// row the support's own registration dedups by choice.
static SOFT_MCA: DevSup = DevSup {
    name: "devMCA_soft",
    choice: "Soft Channel",
    link_type: DbLinkType::Constant,
    dset: Dset::new(6, &[2, 4, 5], false),
};

const DB: &str = r#"
record(waveform, "WF") { field(NELM, "4") field(FTVL, "LONG") }
record(waveform, "WF2") { field(NELM, "4") field(FTVL, "LONG") }
record(mca, "MA") { field(DTYP, "Soft Channel") field(INP, "WF") field(NMAX, "4") }
"#;

fn run(shell: &IocShell, line: &str) -> String {
    let out = std::env::temp_dir().join(format!("mca-nomod-{}", std::process::id()));
    shell
        .execute_line(format!("{line} > \"{}\"", out.display()))
        .unwrap();
    let text = std::fs::read_to_string(&out).unwrap();
    std::fs::remove_file(&out).unwrap();
    text
}

/// `dbpr`'s cells for `names`, as `NAME: value` with the padding squeezed. A
/// label is an upper-case token followed by `:` (`INP :`, `DSET:`).
fn cells(dbpr: &str, names: &[&str]) -> Vec<String> {
    let tokens: Vec<&str> = dbpr.split_whitespace().collect();
    let label_at = |i: usize| -> Option<(&str, usize)> {
        let t = tokens[i];
        let upper = |s: &str| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        };
        if let Some(name) = t.strip_suffix(':') {
            upper(name).then_some((name, 1))
        } else {
            (upper(t) && tokens.get(i + 1) == Some(&":")).then_some((t, 2))
        }
    };
    let mut found = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let Some((name, width)) = label_at(i) else {
            i += 1;
            continue;
        };
        let start = i + width;
        let mut end = start;
        while end < tokens.len() && label_at(end).is_none() {
            end += 1;
        }
        found.push((name, tokens[start..end].join(" ")));
        i = end;
    }
    names
        .iter()
        .map(|name| {
            let (_, value) = found
                .iter()
                .find(|(n, _)| n == name)
                .unwrap_or_else(|| panic!("{name} in {dbpr}"));
            format!("{name}: {value}")
        })
        .collect()
}

fn heard_lines(heard: &Mutex<Vec<String>>) -> Vec<String> {
    epics_base_rs::runtime::log::errlog_flush();
    heard
        .lock()
        .unwrap()
        .drain(..)
        .flat_map(|m| m.lines().map(str::to_string).collect::<Vec<_>>())
        .filter(|l| l.contains("MA."))
        .collect()
}

// RTEMS-EXEC-MODEL-ALLOW(1): the runtime only feeds the build and
// `BlockingBridge::capture` for the shell; no CA server is built. Both
// tests pass under EPICS_RS_BUILD_EXEC_BACKEND=thread.
fn build(
    db_text: &str,
) -> (
    tokio::runtime::Runtime,
    Arc<epics_base_rs::server::database::PvDatabase>,
    IocShell,
) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (name, factory) = mca_rs::mca_record_factory();
    register_device_row("mca", &SOFT_MCA);
    let (db, bridge) = rt.block_on(async {
        let (db, _) = IocBuilder::new()
            .register_record_type(name, factory)
            .db_string(db_text, &HashMap::new())
            .unwrap()
            .build()
            .await
            .unwrap();
        db.initialize_link_locality().await;
        db.setup_cp_links().await;
        db.process_cp_links_on_connect().await;
        (db, epics_base_rs::runtime::task::BlockingBridge::capture())
    });
    let shell = IocShell::new(Arc::clone(&db), bridge);
    (rt, db, shell)
}

#[test]
fn a_nomod_inp_put_detaches_the_dset_as_in_c() {
    let heard = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&heard);
    epics_base_rs::runtime::log::errlog_add_listener(move |m| {
        sink.lock().unwrap().push(m.to_string());
    });

    let (rt, _db, shell) = build(DB);
    let rows = ["INP", "DSET", "DPVT", "PACT"];
    let before = cells(&run(&shell, "dbpr MA 4"), &rows);
    assert_eq!(before[0], "INP: DB_LINK WF NPP NMS", "{before:?}");
    assert_eq!(before[3], "PACT: 0", "{before:?}");
    heard_lines(&heard);

    let refused = ["recGblDbaddrError: dbPut Attempt to modify noMod field PV: MA.INP".to_string()];
    let detached = [
        "INP: PV_LINK WF NPP NMS",
        "DSET: PTR (nil)",
        "DPVT: PTR (nil)",
        "PACT: 1",
    ];
    assert_eq!(
        run(&shell, "dbpf MA.INP WF2"),
        "DBF_STRING:         \"WF NPP NMS\"        \n"
    );
    assert_eq!(heard_lines(&heard), refused);
    assert_eq!(cells(&run(&shell, "dbpr MA 4"), &rows), detached);

    assert_eq!(
        run(&shell, "dbpf MA.PROC 1"),
        "DBF_UCHAR:          1 = 0x1   \n"
    );
    assert_eq!(
        run(&shell, "dbgf MA.RPRO"),
        "DBF_UCHAR:          1 = 0x1   \n"
    );

    assert_eq!(
        run(&shell, "dbpf MA.INP WF"),
        "DBF_STRING:         \"WF NPP NMS\"        \n"
    );
    assert_eq!(heard_lines(&heard), refused);
    assert_eq!(cells(&run(&shell, "dbpr MA 4"), &rows), detached);
    drop(shell);
    std::mem::forget(rt);
}

#[test]
fn a_nomod_inp_put_drops_its_cp_subscription() {
    let (rt, db, shell) = build(&DB.replace(r#"field(INP, "WF")"#, r#"field(INP, "WF CP")"#));
    assert!(!db.get_external_cp_targets(b"WF").is_empty(), "CP is armed");
    let gf = |pv: &str| run(&shell, &format!("dbgf {pv}")).trim_end().to_string();
    assert_eq!(gf("MA.INP"), r#"DBF_STRING:         "WF CP NMS""#);
    run(&shell, "dbpf MA.INP WF2");
    for _ in 0..3 {
        run(&shell, "dbpf WF.PROC 1");
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert_eq!(gf("MA.LCNT"), "DBF_UCHAR:          0 = 0x0");
    run(&shell, "dbpf WF2.PROC 1");
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(gf("MA.LCNT"), "DBF_UCHAR:          0 = 0x0");
    drop(shell);
    std::mem::forget(rt);
}
