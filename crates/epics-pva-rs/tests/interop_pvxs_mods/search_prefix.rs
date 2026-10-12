//! The search answers what `dbChannelTest` answers: the record name, then
//! the field name's identifier prefix (`dbStaticLib.c:1800-1802`); the rest
//! of the name is judged only at create. Each name is classed by `pvxget`
//! against softIocPVX and against the Rust server on the same records.

// `server::PvaServer` exists only on the tokio backend.
#![cfg(tokio_backend)]

use super::interop_helpers::PvxsOutput;
use super::interop_helpers::{
    PVXGET, SOFT_IOC_PVX, pick_localhost_port, pvxs_command, require_pvxs, spawn_soft_ioc_pvx,
};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::waveform::WaveformRecord;
use epics_base_rs::server::records::{ai::AiRecord, stringin::StringinRecord};
use epics_base_rs::types::DbFieldType;
use epics_pva_rs::server::PvaServer;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::watch;

const DB: &str = r#"
record(ai, "R") { field(VAL, "1.5") field(EGU, "mm") }
record(waveform, "WF") { field(NELM, "4") field(FTVL, "DOUBLE") }
record(stringin, "SI") { field(VAL, "s") }
"#;

const NAMES: [&str; 20] = [
    "R",
    "R.VAL",
    "R.EGU",
    "R.RTYP",
    "R.",
    "R.NOSUCH",
    "R.val",
    "NOSUCH",
    "R.VAL$",
    "R.EGU$",
    "R.NOSUCH$",
    "R.VAL$garbage",
    "R.VAL.{bad",
    "R.{bad",
    "R.1abc",
    r#"R.VAL.{"ts":{}}"#,
    r#"R.VAL{"ts":{}}"#,
    "WF.VAL$",
    "SI.VAL$",
    "R.VAL_X",
];

#[derive(Debug, PartialEq)]
enum Answer {
    Served,
    Refused,
    Unanswered,
}

fn class(pvxget: &Path, tcp: u16, name: &str) -> Answer {
    let out = pvxs_command(pvxget)
        .args(["-w", "2", name])
        .env("EPICS_PVA_NAME_SERVERS", format!("127.0.0.1:{tcp}"))
        .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_PVA_ADDR_LIST", "")
        .pvxs_output()
        .expect("spawn pvxget");
    let text = String::from_utf8_lossy(&out.stdout) + String::from_utf8_lossy(&out.stderr);
    if out.status.success() {
        Answer::Served
    } else if text.contains("refuses channel") {
        Answer::Refused
    } else {
        assert!(text.contains("Timeout"), "{name}: {text}");
        Answer::Unanswered
    }
}

/// pvxget does not repeat a search sent before the name-server connection
/// is validated, so a name is unanswered only when three tries time out.
fn classes(pvxget: &Path, tcp: u16) -> Vec<Answer> {
    std::thread::scope(|s| {
        let runs: Vec<_> = NAMES
            .iter()
            .map(|n| {
                s.spawn(move || {
                    (0..3)
                        .map(|_| class(pvxget, tcp, n))
                        .find(|a| *a != Answer::Unanswered)
                        .unwrap_or(Answer::Unanswered)
                })
            })
            .collect();
        runs.into_iter().map(|r| r.join().unwrap()).collect()
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_search_answers_the_record_and_field_prefix_as_softiocpvx() {
    let (Some(ioc_bin), Some(pvxget)) = (require_pvxs(SOFT_IOC_PVX), require_pvxs(PVXGET)) else {
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("t.db");
    std::fs::File::create(&db_path)
        .and_then(|mut f| f.write_all(DB.as_bytes()))
        .expect("write db");
    let (_ioc, c_tcp) = spawn_soft_ioc_pvx(&ioc_bin, &db_path, dir.path());

    let rs_tcp = pick_localhost_port();
    // SAFETY: nextest runs this test alone in its process.
    unsafe {
        std::env::set_var("EPICS_PVAS_SERVER_PORT", rs_tcp.to_string());
        std::env::set_var("EPICS_PVAS_BROADCAST_PORT", "0");
    }
    let db = Arc::new(PvDatabase::new());
    db.add_record("R", Box::new(AiRecord::new(1.5)))
        .await
        .unwrap();
    db.add_record("WF", Box::new(WaveformRecord::new(4, DbFieldType::Double)))
        .await
        .unwrap();
    db.add_record("SI", Box::new(StringinRecord::new("s")))
        .await
        .unwrap();
    let server = PvaServer::from_parts(
        db,
        None,
        epics_base_rs::server::access_security::new_acf_cell(None),
        None,
        None,
    );
    let (tx, mut rx) = watch::channel(None);
    let run = tokio::spawn(async move { server.run_reporting(Some(tx)).await });
    while rx.borrow_and_update().is_none() {
        rx.changed().await.expect("server reports its ports");
    }
    let ioc_text = || {
        ["ioc.out", "ioc.err"]
            .map(|f| std::fs::read_to_string(dir.path().join(f)).unwrap_or_default())
            .join("\n")
    };

    let (c, rs) =
        tokio::task::spawn_blocking(move || (classes(&pvxget, c_tcp), classes(&pvxget, rs_tcp)))
            .await
            .unwrap();
    run.abort();
    let differ: Vec<_> = NAMES
        .iter()
        .zip(c.iter().zip(&rs))
        .filter(|(_, (c, rs))| c != rs)
        .map(|(n, (c, rs))| format!("{n}: softIocPVX {c:?}, rs {rs:?}"))
        .collect();
    assert!(
        differ.is_empty(),
        "{differ:#?}\nsoftIocPVX:\n{}",
        ioc_text()
    );
}
