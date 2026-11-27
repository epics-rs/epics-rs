//! A pva link put the server refuses prints pvxs `linkPutDone`'s line,
//! `<pv> PVA link put ERROR: <reason>` (`ioc/pvalink_channel.cpp:193`), for
//! an OUT write and a forward link alike, the reason being the server's
//! message. softIocPVX 1.5.3 writing to a p4p PV with no put handler prints
//! `NOV:PV PVA link put ERROR: Put not supported`; a put whose server is
//! stopped and then killed under it prints nothing, retry or not.
#![cfg(all(feature = "pvalink", feature = "qsrv", tokio_backend, unix))]

use crate::common;

use std::io::{self, BufRead, BufReader};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::{CHILD, Shell, spawn_ioc, stop};
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{PvaServer, SharedPV, SharedSource};
use fact_budget::FACT_BUDGET;
use named_port::HeldPort;

const IOC_CHILD: &str = "pva_link_put_error_prints_the_pvxs_line::ioc_child";

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

/// A PVA server holding `NOV:PV`, whose every put is refused with
/// `Put not supported`, and the count of the puts it refused.
fn refusing_server(rt: &tokio::runtime::Runtime) -> (PvaServer, Arc<AtomicUsize>) {
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
    (server, refused)
}

#[test]
fn a_refused_pva_link_put_prints_the_pvxs_line() {
    if std::env::var_os(CHILD).is_some() {
        return;
    }
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (server, refused) = refusing_server(&rt);

    let dir = tempfile::tempdir().unwrap();
    let (remote_port, local_port) = (HeldPort::tcp(), HeldPort::tcp());
    let mut remote = spawn_ioc(IOC_CHILD, dir.path(), "remote", REMOTE, &remote_port, "");
    let mut ioc = spawn_ioc(
        IOC_CHILD,
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
    // absence-window: no third put-error line follows the disconnect.
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

/// The IOC's log client, connected within [`FACT_BUDGET`].
fn accept_within_budget(server: &TcpListener) -> TcpStream {
    server.set_nonblocking(true).unwrap();
    let deadline = epics_base_rs::runtime::time::deadline_from_now(FACT_BUDGET);
    loop {
        match server.accept() {
            Ok((peer, _)) => {
                peer.set_nonblocking(false).unwrap();
                return peer;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "no log client within {FACT_BUDGET:?}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("accept: {e}"),
        }
    }
}

/// The lines `log` gets before `end`, which must arrive within [`FACT_BUDGET`].
fn lines_before(log: &mut BufReader<TcpStream>, end: &str) -> Vec<String> {
    let deadline = epics_base_rs::runtime::time::deadline_from_now(FACT_BUDGET);
    let mut lines = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(
            !left.is_zero(),
            "no {end:?} within {FACT_BUDGET:?}: {lines:?}"
        );
        log.get_ref().set_read_timeout(Some(left)).unwrap();
        let mut line = String::new();
        match log.read_line(&mut line) {
            Ok(0) => panic!("the log closed before {end:?}: {lines:?}"),
            Ok(_) => match line.trim_end_matches(['\r', '\n']) {
                line if line == end => return lines,
                line => lines.push(line.to_owned()),
            },
            Err(e) => panic!("no {end:?} within {FACT_BUDGET:?} ({e}): {lines:?}"),
        }
    }
}

/// The line is errlog's, as `linkPutDone`'s `errlogPrintf` is: an iocLog
/// server receives it, and an IOC told `eltc 0` keeps it off its console.
#[test]
fn a_refused_pva_link_put_reaches_the_log_server_and_not_a_silenced_console() {
    if std::env::var_os(CHILD).is_some() {
        return;
    }
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (server, refused) = refusing_server(&rt);
    let log_server = TcpListener::bind("127.0.0.1:0").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let port = HeldPort::tcp();
    let db = r#"record(ao, "O") { field(OUT, {pva:"NOV:PV"}) }"#;
    let mut ioc = spawn_ioc(
        IOC_CHILD,
        dir.path(),
        "local",
        db,
        &port,
        &server.tcp_addr().to_string(),
    );
    let mut sh = Shell::new(&mut ioc, dir.path(), "local");
    sh.line("epicsEnvSet EPICS_IOC_LOG_INET 127.0.0.1");
    sh.line(&format!(
        "epicsEnvSet EPICS_IOC_LOG_PORT {}",
        log_server.local_addr().unwrap().port()
    ));
    sh.line("iocLogInit");
    let mut log = BufReader::new(accept_within_budget(&log_server));
    sh.line("eltc 0");

    sh.capture_until("dbjlr O 0", "conn=T");
    sh.line("dbpf O 5");
    sh.wait_until(|| refused.load(Ordering::SeqCst) == 1);
    // The line is posted before the put ends, and the marker after it.
    sh.capture_until("dbjlr O 0", "conn=T");
    sh.line("errlog marker");
    let mut logged = lines_before(&mut log, "marker");
    logged.retain(|line| line.contains("link put"));
    sh.line("exit");
    drop(sh);
    let _ = ioc.wait();
    let console = std::fs::read_to_string(dir.path().join("local.err")).unwrap();

    assert_eq!(
        logged,
        ["NOV:PV PVA link put ERROR: Put not supported"],
        "the log server gets the line"
    );
    assert!(
        !console.contains("link put"),
        "eltc 0 keeps it off the console:\n{console}"
    );
}
