//! A record monitor's first update carries the leaves its own `DBE` assigns:
//! pvxs seeds each half with `db_post_single_event` on that half's mask
//! (`subscriptionctx.h:56-57`), so `DBE=ARCHIVE` sends no `alarm`. Each
//! first update's leaf set is read by `pvxmonitor -F delta` against
//! softIocPVX and against the Rust server on the same record. The same
//! `enable()` runs on every START, so a resumed monitor gets the current
//! value again.

// `server::PvaServer` exists only on the tokio backend.
#![cfg(tokio_backend)]

use super::interop_helpers::{
    DropChild, PVXMONITOR, SOFT_IOC_PVX, pvxs_command, require_pvxs, rerun_pvxs, spawn_soft_ioc_pvx,
};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ai::AiRecord;
use epics_pva_rs::client_native::context::PvaClient;
use epics_pva_rs::pvdata::{PvField, ScalarValue};
use epics_pva_rs::server::PvaServer;
use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

const DB: &str = r#"record(ai, "R") { field(VAL, "1.5") }"#;

const CASES: [(&str, &str); 10] = [
    ("R", ""),
    ("R", "record[DBE=ARCHIVE]"),
    ("R", "record[DBE=VALUE]"),
    ("R", "record[DBE=ALARM]"),
    ("R", "record[DBE=PROPERTY]"),
    ("R.HOPR", ""),
    ("R.HOPR", "record[DBE=ARCHIVE]"),
    ("R.HOPR", "record[DBE=VALUE]"),
    ("R.HOPR", "record[DBE=ALARM]"),
    ("R.HOPR", "record[DBE=PROPERTY]"),
];

/// The leaf names of the first update `pvxmonitor` prints for `name`.
fn first_leaves(pvxmonitor: &Path, tcp: u16, dir: &Path, n: usize) -> BTreeSet<String> {
    let (name, request) = CASES[n];
    let out = dir.join(format!("mon{n}.txt"));
    let err = dir.join(format!("err{n}.txt"));
    let read = || std::fs::read_to_string(&out).unwrap_or_default();
    rerun_pvxs(
        || {
            // A file stdout is block-buffered, and the monitor is killed, not ended.
            let mut cmd = pvxs_command(Path::new("stdbuf"));
            cmd.arg("-oL").arg(pvxmonitor).args(["-F", "delta"]);
            if !request.is_empty() {
                cmd.args(["-r", request]);
            }
            let child = cmd
                .arg(name)
                .env("EPICS_PVA_NAME_SERVERS", format!("127.0.0.1:{tcp}"))
                .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
                .env("EPICS_PVA_ADDR_LIST", "")
                .stdout(std::fs::File::create(&out).expect("monitor out"))
                .stderr(std::fs::File::create(&err).expect("err"))
                .spawn()
                .expect("spawn pvxmonitor");
            let _mon = DropChild { child };
            let deadline = Instant::now() + Duration::from_secs(8);
            while !read().lines().any(|l| l.starts_with("    ")) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            // The update is printed whole by one write; give a second one no room
            // to be mistaken for part of it.
            std::thread::sleep(Duration::from_millis(300));
        },
        |()| std::fs::read_to_string(&err).unwrap_or_default(),
    );
    read()
        .lines()
        .skip_while(|l| *l != name)
        .skip(1)
        .take_while(|l| l.starts_with("    "))
        .filter_map(|l| l.split_whitespace().next().map(str::to_string))
        .collect()
}

fn all_first_leaves(pvxmonitor: &Path, tcp: u16, dir: &Path) -> Vec<BTreeSet<String>> {
    std::thread::scope(|s| {
        let runs: Vec<_> = (0..CASES.len())
            .map(|n| {
                // A search sent before the name-server connection is
                // validated is not repeated; a later try is.
                s.spawn(move || {
                    (0..3)
                        .map(|_| first_leaves(pvxmonitor, tcp, dir, n))
                        .find(|l| !l.is_empty())
                        .unwrap_or_default()
                })
            })
            .collect();
        runs.into_iter().map(|r| r.join().unwrap()).collect()
    })
}

/// softIocPVX and the Rust server, each serving `R` as [`DB`] does.
struct Servers {
    dir: tempfile::TempDir,
    c_tcp: u16,
    rs_tcp: u16,
    _ioc: DropChild,
    run: tokio::task::JoinHandle<()>,
}

impl Drop for Servers {
    fn drop(&mut self) {
        self.run.abort();
    }
}

async fn start(ioc_bin: &Path) -> Servers {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("t.db");
    std::fs::File::create(&db_path)
        .and_then(|mut f| f.write_all(DB.as_bytes()))
        .expect("write db");
    let (_ioc, c_tcp) = spawn_soft_ioc_pvx(ioc_bin, &db_path, dir.path());

    // SAFETY: nextest runs this test alone in its process.
    unsafe {
        std::env::set_var("EPICS_PVAS_SERVER_PORT", "0");
        std::env::set_var("EPICS_PVAS_BROADCAST_PORT", "0");
    }
    let db = Arc::new(PvDatabase::new());
    db.add_record("R", Box::new(AiRecord::new(1.5)))
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
    let run = tokio::spawn(async move {
        let _ = server.run_reporting(Some(tx)).await;
    });
    // The kernel picks the port and the server reports it.
    let rs_tcp = loop {
        if let Some(h) = rx.borrow_and_update().clone() {
            break h.report().tcp_port;
        }
        rx.changed().await.expect("server reports its ports");
    };

    Servers {
        dir,
        c_tcp,
        rs_tcp,
        _ioc,
        run,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_update_carries_the_leaves_its_dbe_assigns_as_softiocpvx() {
    let (Some(ioc_bin), Some(pvxmonitor)) = (require_pvxs(SOFT_IOC_PVX), require_pvxs(PVXMONITOR))
    else {
        return;
    };
    let servers = start(&ioc_bin).await;
    let (dir, c_tcp, rs_tcp) = (&servers.dir, servers.c_tcp, servers.rs_tcp);
    let path = dir.path().to_path_buf();
    std::fs::create_dir_all(path.join("c")).unwrap();
    std::fs::create_dir_all(path.join("rs")).unwrap();
    let (c, rs) = tokio::task::spawn_blocking(move || {
        (
            all_first_leaves(&pvxmonitor, c_tcp, &path.join("c")),
            all_first_leaves(&pvxmonitor, rs_tcp, &path.join("rs")),
        )
    })
    .await
    .unwrap();
    let differ: Vec<_> = CASES
        .iter()
        .zip(c.iter().zip(&rs))
        .filter(|(_, (c, rs))| c != rs)
        .map(|((name, req), (c, rs))| {
            format!(
                "{name} {req:?}: softIocPVX-only {:?}, rs-only {:?}",
                c.difference(rs).collect::<Vec<_>>(),
                rs.difference(c).collect::<Vec<_>>()
            )
        })
        .collect();
    assert!(
        c.iter().all(|l| !l.is_empty()),
        "softIocPVX sent no first update: {c:?}\n{}",
        ["c/mon0.txt", "c/err0.txt"]
            .map(|f| std::fs::read_to_string(dir.path().join(f)).unwrap_or_default())
            .join("\n")
    );
    assert!(differ.is_empty(), "{differ:#?}");
}

/// The values a monitor on `R` delivers after a RESUME, `R` having been
/// put to 7 while it was stopped.
async fn after_resume(tcp: u16) -> Vec<f64> {
    let client = PvaClient::builder()
        .timeout(Duration::from_secs(2))
        .server_addr(std::net::SocketAddr::from(([127, 0, 0, 1], tcp)))
        .build();
    let got = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let sink = got.clone();
    let handle = client
        .pvmonitor_handle(
            "R",
            move |_desc, value| {
                if let PvField::Structure(s) = value
                    && let Some(ScalarValue::Double(v)) = s.get_value()
                {
                    sink.lock().push(*v);
                }
            },
            |_| {},
        )
        .await
        .expect("subscribe R");
    let settle = Duration::from_millis(500);
    tokio::time::sleep(settle).await;
    handle.pause().await;
    tokio::time::sleep(settle).await;
    client.pvput("R", "7").await.expect("put R");
    tokio::time::sleep(settle).await;
    let before = got.lock().len();
    handle.resume().await;
    tokio::time::sleep(settle).await;
    handle.stop();
    got.lock()[before..].to_vec()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resumed_monitor_gets_the_current_value_again_as_softiocpvx() {
    let Some(ioc_bin) = require_pvxs(SOFT_IOC_PVX) else {
        return;
    };
    let servers = start(&ioc_bin).await;
    let c = after_resume(servers.c_tcp).await;
    let rs = after_resume(servers.rs_tcp).await;
    assert!(!c.is_empty(), "softIocPVX delivered nothing on RESUME");
    assert_eq!(rs, c, "after RESUME: rs vs softIocPVX");
}
