//! A pva link put the server refuses prints pvxs `linkPutDone`'s line,
//! `<pv> PVA link put ERROR: <reason>` (`ioc/pvalink_channel.cpp:192`), for
//! an OUT write and a forward link alike, the reason being the server's
//! message. softIocPVX 1.5.3 writing to a p4p PV with no put handler prints
//! `NOV:PV PVA link put ERROR: Put not supported`; a put whose server is
//! stopped and then killed under it prints nothing, retry or not.
#![cfg(all(feature = "pvalink", feature = "qsrv", tokio_backend, unix))]

mod common;

use std::io::{BufRead, BufReader};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::{CHILD, Shell, free_port, signal, spawn_ioc};
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{PvaServer, SharedPV, SharedSource};

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
    let remote_port = free_port();
    let mut remote = spawn_ioc(dir.path(), "remote", REMOTE, remote_port, "");
    let mut ioc = spawn_ioc(
        dir.path(),
        "local",
        DB,
        free_port(),
        &format!("{} 127.0.0.1:{remote_port}", server.tcp_addr()),
    );
    let (tx, lines) = std::sync::mpsc::channel();
    let stderr = BufReader::new(ioc.stderr.take().unwrap());
    let reader = std::thread::spawn(move || {
        for line in stderr.lines().map_while(Result::ok) {
            if line.contains("link put") {
                let _ = tx.send(line);
            }
        }
    });
    let mut sh = Shell {
        stdin: ioc.stdin.take().unwrap(),
        dir: dir.path(),
        tag: "p",
        n: 0,
    };
    let mut seen = Vec::new();
    let mut wait_for = |n: usize| {
        let deadline = Instant::now() + Duration::from_secs(20);
        while seen.len() < n && Instant::now() < deadline {
            if let Ok(line) = lines.recv_timeout(Duration::from_millis(100)) {
                seen.push(line);
            }
        }
    };

    sh.capture_until("dbjlr O 0", "conn=T");
    sh.line("dbpf O 5");
    wait_for(1);
    sh.line("dbpf F.PROC 1");
    wait_for(2);

    sh.capture_until("dbjlr WR 0", "conn=T");
    signal(&remote, "-STOP");
    sh.line("dbpf WR 5");
    sh.line("dbpf WRT 6");
    sh.capture_until("dbjlr WR 0", "conn=T Put");
    remote.kill().unwrap();
    let _ = remote.wait();
    sh.capture_until("dbjlr WR 1", "conn=F #disconn=1");
    std::thread::sleep(Duration::from_millis(500));
    sh.line("exit");
    drop(sh);
    let _ = ioc.wait();
    reader.join().unwrap();
    seen.extend(lines.try_iter());

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
