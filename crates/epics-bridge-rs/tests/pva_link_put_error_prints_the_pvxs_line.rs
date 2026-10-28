//! A pva link put the server refuses prints pvxs `linkPutDone`'s line,
//! `<pv> PVA link put ERROR: <reason>` (`ioc/pvalink_channel.cpp:192`), for
//! an OUT write and a forward link alike, the reason being the server's
//! message. softIocPVX 1.5.3 writing to a p4p PV with no put handler prints
//! `NOV:PV PVA link put ERROR: Put not supported`; a put whose server is
//! stopped and then killed under it prints nothing, retry or not.
#![cfg(all(feature = "pvalink", feature = "qsrv", tokio_backend, unix))]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::{CHILD, Shell, spawn_ioc, stop};
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{PvaServer, SharedPV, SharedSource};
use named_port::HeldPort;

/// The child half; a no-op unless the parent set [`CHILD`].
#[test]
fn ioc_child() {
    common::ioc_child();
}

const DB: &str = r#"
record(ao, "O") { field(OUT, {pva:"NOV:PV"}) }
record(calc, "F") { field(FLNK, {pva:"NOV:PV"}) }
record(ao, "WR") { field(OUT, {pva:"REM"}) }
record(ao, "WRT") { field(OUT, {pva:{pv:"REM", retry:true}}) }
"#;

const REMOTE: &str = r#"record(ai, "REM") { field(VAL, "7") }"#;

#[test]
fn a_refused_pva_link_put_prints_the_pvxs_line() {
    if std::env::var_os(CHILD).is_some() {
        return;
    }
    let rt = tokio::runtime::Runtime::new().unwrap();
    let refused = Arc::new(AtomicUsize::new(0));
    let server = rt.block_on(async {
        let pv = SharedPV::new();
        let mut value = PvStructure::new("epics:nt/NTScalar:1.0");
        value
            .fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(1.0))));
        pv.open(
            FieldDesc::Structure {
                struct_id: "epics:nt/NTScalar:1.0".into(),
                fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))].into(),
            },
            PvField::Structure(value),
        )
        .unwrap();
        pv.on_put({
            let refused = refused.clone();
            move |_, _| {
                refused.fetch_add(1, Ordering::SeqCst);
                Err("Put not supported".into())
            }
        });
        let source = SharedSource::new();
        source.add("NOV:PV", pv);
        PvaServer::isolated(Arc::new(source)).expect("test PVA server must start")
    });

    let dir = tempfile::tempdir().unwrap();
    let (remote_port, local_port) = (HeldPort::tcp(), HeldPort::tcp());
    let mut remote = spawn_ioc(dir.path(), "remote", REMOTE, &remote_port, "");
    let mut ioc = spawn_ioc(
        dir.path(),
        "local",
        DB,
        &local_port,
        &format!("{} 127.0.0.1:{}", server.tcp_addr(), remote_port.port),
    );
    let stderr = dir.path().join("local.err");
    // The put lines among the whole lines written so far.
    let put_lines = || -> Vec<String> {
        let text = std::fs::read_to_string(&stderr).unwrap_or_default();
        text[..text.rfind('\n').map_or(0, |i| i + 1)]
            .lines()
            .filter(|line| line.contains("link put"))
            .map(str::to_string)
            .collect()
    };
    let mut sh = Shell::new(&mut ioc, dir.path(), "local");

    sh.capture_until("dbjlr O 0", "conn=T");
    sh.line("dbpf O 5");
    sh.wait_until(|| !put_lines().is_empty());
    sh.line("dbpf F.PROC 1");
    sh.wait_until(|| put_lines().len() >= 2);

    sh.capture_until("dbjlr WR 0", "conn=T");
    stop(&remote);
    sh.line("dbpf WR 5");
    sh.line("dbpf WRT 6");
    sh.capture_until("dbjlr WR 0", "conn=T Put");
    remote.kill().unwrap();
    let _ = remote.wait();
    // The disconnect ends the put, pvxs's `op_put.reset()`
    // (`ioc/pvalink_channel.cpp:369`): no report shows one without the other.
    let text = sh.capture_when("dbjlr WR 0", |text| text.contains("conn=F"));
    assert!(
        text.ends_with("conn=F\n"),
        "the put outlived the disconnect:\n{text}"
    );
    sh.capture_until("dbjlr WR 1", "conn=F #disconn=1");
    std::thread::sleep(Duration::from_millis(500));
    sh.line("exit");
    drop(sh);
    let _ = ioc.wait();
    let seen = put_lines();

    assert_eq!(
        refused.load(Ordering::SeqCst),
        2,
        "both puts reached the server"
    );
    assert_eq!(
        seen, ["NOV:PV PVA link put ERROR: Put not supported"; 2],
        "each refused put prints pvxs's line, once"
    );
}
