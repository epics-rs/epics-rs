//! An IOC's boot stderr is C's stream, so it carries no line softIoc does not
//! print. softIoc R7.0.10 on `record(ai, "X:AI") { field(VAL, "1") }`,
//! `softIoc -d x.db < /dev/null` with `EPICS_CAS_INTF_ADDR_LIST=127.0.0.1`
//! and `EPICS_CA_AUTO_ADDR_LIST=NO`, stderr:
//!
//! ```text
//! Starting iocInit
//! iocRun: All initialization complete
//! ```
//!
//! With CA forward links, [`FWD_DB`] with `EPICS_CA_ADDR_LIST=127.0.0.1`
//! added, softIoc warns from `doResolveLinks`, in `iterateRecords` order —
//! record types by name, each type's records in load order, each record's
//! links in field order:
//!
//! ```text
//! Starting iocInit
//! WARNING: Forward-link uses Channel Access without pointing to PROC field
//!     FL.FLNK => Y
//! WARNING: Forward-link uses Channel Access without pointing to PROC field
//!     FO.LNK2 => Z
//! WARNING: Forward-link uses Channel Access without pointing to PROC field
//!     FO.LNK4 => W2
//! WARNING: Forward-link uses Channel Access without pointing to PROC field
//!     FP.LNK1 => V
//! iocRun: All initialization complete
//! ```
//!
//! Each binary runs on the backend that boots it: `softioc-rs` on tokio,
//! `realtime-ca-ioc` (whose argv is its st.cmd) on the exec backend.

#![cfg(feature = "client-core")]

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SOFTIOC_BOOT_STDERR: &str = "Starting iocInit\niocRun: All initialization complete\n";
const DB: &str = "record(ai, \"X:AI\") { field(VAL, \"1\") }\n";
const FWD_DB: &str = "record(fanout, \"FO\") { field(LNK2, \"Z CA\") field(LNK4, \"W2\") }\n\
    record(calc, \"FL\") { field(CALC, \"VAL+1\") field(FLNK, \"Y CA\") }\n\
    record(fanout, \"FP\") { field(LNK1, \"V\") }\n";
const SOFTIOC_FWD_BOOT_STDERR: &str = "Starting iocInit\n\
    WARNING: Forward-link uses Channel Access without pointing to PROC field\n    FL.FLNK => Y\n\
    WARNING: Forward-link uses Channel Access without pointing to PROC field\n    FO.LNK2 => Z\n\
    WARNING: Forward-link uses Channel Access without pointing to PROC field\n    FO.LNK4 => W2\n\
    WARNING: Forward-link uses Channel Access without pointing to PROC field\n    FP.LNK1 => V\n\
    iocRun: All initialization complete\n";

use named_port::verdict as softioc_verdict;

/// `bin` with the db and C's environment on `port`, its stderr in a file
/// under `dir`.
fn spawn(bin: &str, args: &[&std::ffi::OsStr], dir: &std::path::Path, port: u16) -> Child {
    let err = std::fs::File::create(dir.join("stderr")).unwrap();
    let mut cmd = Command::new(bin);
    for (k, _) in std::env::vars().filter(|(k, _)| k.starts_with("EPICS_")) {
        cmd.env_remove(k);
    }
    cmd.args(args)
        .env("EPICS_CA_SERVER_PORT", port.to_string())
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        // Without it C's dbCa reports an empty search list once a CA link opens.
        .env("EPICS_CA_ADDR_LIST", "127.0.0.1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(err)
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {bin}: {e}"))
}

/// The stderr once iocRun has been printed and `settle` more has passed, the
/// child killed if it is still serving.
fn boot_stderr(mut child: Child, dir: &std::path::Path, settle: Duration) -> String {
    let read = || {
        let mut s = String::new();
        std::fs::File::open(dir.join("stderr"))
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        s
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while !read().contains("iocRun: All initialization complete\n") {
        if Instant::now() > deadline || child.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(settle);
    let _ = child.kill();
    let _ = child.wait();
    read()
}

/// [`boot_stderr`] of `bin` on `port`, or `None` when the IOC reports that
/// port taken.
fn boot_stderr_on(
    bin: &str,
    args: &[&std::ffi::OsStr],
    dir: &std::path::Path,
    port: u16,
) -> Option<String> {
    let stderr = boot_stderr(spawn(bin, args, dir, port), dir, Duration::from_millis(500));
    (!softioc_verdict::rs_ioc_port_was_taken(&stderr, port)).then_some(stderr)
}

/// [`boot_stderr`] of `bin` on a port it is named, retried on a fresh one
/// when the IOC reports that one taken.
fn named_boot_stderr(bin: &str, args: &[&std::ffi::OsStr], dir: &std::path::Path) -> String {
    named_port::on_a_named_port(|port| boot_stderr_on(bin, args, dir, port))
}

/// [`named_boot_stderr`] with the first candidate held by this test through
/// `hold`, and the number of attempts it took.
fn boot_past_a_held_port<H>(
    bin: &str,
    args: &[&std::ffi::OsStr],
    dir: &std::path::Path,
    hold: impl Fn(u16) -> std::io::Result<H>,
) -> (usize, String) {
    let mut attempts = 0;
    let stderr = named_port::on_a_named_port(|port| {
        attempts += 1;
        let _held = (attempts == 1).then(|| hold(port).expect("hold the first candidate"));
        boot_stderr_on(bin, args, dir, port)
    });
    (attempts, stderr)
}

fn hold_udp(port: u16) -> std::io::Result<std::net::UdpSocket> {
    std::net::UdpSocket::bind(("127.0.0.1", port))
}

fn hold_tcp(port: u16) -> std::io::Result<std::net::TcpListener> {
    std::net::TcpListener::bind(("127.0.0.1", port))
}

/// The retry path end to end on `bin`: whichever socket `hold` takes, the IOC
/// reports the first candidate taken and a later one boots softIoc's stderr.
fn retries_past_a_held_port<H>(
    bin: &str,
    args: impl Fn(&std::path::Path) -> Vec<std::ffi::OsString>,
    hold: impl Fn(u16) -> std::io::Result<H>,
) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("x.db");
    std::fs::write(&db, DB).unwrap();
    let args = args(&db);
    let args: Vec<&std::ffi::OsStr> = args.iter().map(|a| a.as_os_str()).collect();
    let (attempts, stderr) = boot_past_a_held_port(bin, &args, dir.path(), hold);
    assert!(attempts >= 2, "the held candidate was not reported taken");
    assert_eq!(stderr, SOFTIOC_BOOT_STDERR);
}

/// `bin`'s boot stderr on `db`; `db_args` is how `bin` is handed the db.
fn boot(bin: &str, db: &str, db_args: fn(&std::path::Path) -> Vec<std::ffi::OsString>) -> String {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.db");
    std::fs::write(&path, db).unwrap();
    let args = db_args(&path);
    let args: Vec<&std::ffi::OsStr> = args.iter().map(|a| a.as_os_str()).collect();
    named_boot_stderr(bin, &args, dir.path())
}

#[cfg(tokio_backend)]
fn softioc_rs(db: &str) -> String {
    boot(env!("CARGO_BIN_EXE_softioc-rs"), db, |p| {
        vec!["-d".into(), p.into()]
    })
}

#[cfg(exec_backend)]
fn realtime_ca_ioc(db: &str) -> String {
    boot(env!("CARGO_BIN_EXE_realtime-ca-ioc"), db, |p| {
        vec![p.into()]
    })
}

#[cfg(tokio_backend)]
#[test]
fn softioc_rs_boot_stderr_is_softiocs() {
    assert_eq!(softioc_rs(DB), SOFTIOC_BOOT_STDERR);
}

#[cfg(tokio_backend)]
#[test]
fn softioc_rs_forward_link_warnings_are_softiocs() {
    assert_eq!(softioc_rs(FWD_DB), SOFTIOC_FWD_BOOT_STDERR);
}

#[cfg(exec_backend)]
#[test]
fn realtime_ca_ioc_boot_stderr_is_softiocs() {
    assert_eq!(realtime_ca_ioc(DB), SOFTIOC_BOOT_STDERR);
}

#[cfg(exec_backend)]
#[test]
fn realtime_ca_ioc_forward_link_warnings_are_softiocs() {
    assert_eq!(realtime_ca_ioc(FWD_DB), SOFTIOC_FWD_BOOT_STDERR);
}

#[cfg(tokio_backend)]
fn softioc_rs_args(db: &std::path::Path) -> Vec<std::ffi::OsString> {
    vec!["-d".into(), db.into()]
}

#[cfg(tokio_backend)]
#[test]
fn softioc_rs_retries_a_held_udp_port() {
    retries_past_a_held_port(env!("CARGO_BIN_EXE_softioc-rs"), softioc_rs_args, hold_udp);
}

#[cfg(tokio_backend)]
#[test]
fn softioc_rs_retries_a_held_tcp_port() {
    retries_past_a_held_port(env!("CARGO_BIN_EXE_softioc-rs"), softioc_rs_args, hold_tcp);
}

#[cfg(exec_backend)]
fn realtime_ca_ioc_args(db: &std::path::Path) -> Vec<std::ffi::OsString> {
    vec![db.into()]
}

#[cfg(exec_backend)]
#[test]
fn realtime_ca_ioc_retries_a_held_udp_port() {
    retries_past_a_held_port(
        env!("CARGO_BIN_EXE_realtime-ca-ioc"),
        realtime_ca_ioc_args,
        hold_udp,
    );
}

#[cfg(exec_backend)]
#[test]
fn realtime_ca_ioc_retries_a_held_tcp_port() {
    retries_past_a_held_port(
        env!("CARGO_BIN_EXE_realtime-ca-ioc"),
        realtime_ca_ioc_args,
        hold_tcp,
    );
}
