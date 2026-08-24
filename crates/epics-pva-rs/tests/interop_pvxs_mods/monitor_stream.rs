//! MONITOR stream semantics cross-impl.
//!
//! Verifies the multi-event monitor flow across implementations
//! (where the earlier pipeline tests only confirmed pipeline
//! negotiation happens, and the GET-based matrix only proves a
//! single value round-trip):
//!
//! - **Direction A** — Rust server publishes a stream of events
//!   on a SharedPV; pvxmonitor (LE pvxs client) subscribes and
//!   prints. Test parses pvxmonitor stdout, asserts ≥N distinct
//!   values landed in correct order.
//!
//! - **Direction B** — pvxs writable server (`reverse_server
//!   --writable`) hosts a mailbox PV; Rust client subscribes,
//!   another task fires pvxput updates, the Rust monitor callback
//!   should observe ≥N distinct values.
//!
//! Pre-batch-7 the multi-event MONITOR path between
//! implementations was unverified at the test level — Rust↔Rust
//! covered it but not pvxs↔Rust.

// RTEMS-EXEC-MODEL-ALLOW(2): not run by the default nextest profile - this file is a module of the `interop_pvxs` binary, which `.config/nextest.toml`'s default-filter excludes.

use super::interop_helpers::{PVXMONITOR, PVXPUT, pvxs_command, pvxs_lib_dir, require_pvxs};

use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{PvaServer, SharedPV, SharedSource};

use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

fn env_key() -> &'static str {
    if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else {
        "LD_LIBRARY_PATH"
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interop_monitor_a_pvxmonitor_streams_from_rust_server() {
    let Some(pvxmonitor) = require_pvxs(PVXMONITOR) else {
        return;
    };

    let pv = SharedPV::new();
    pv.open(
        FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Int))],
        },
        PvField::Structure(PvStructure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), PvField::Scalar(ScalarValue::Int(0)))],
        }),
    )
    .unwrap();
    let src = SharedSource::new();
    src.add("M:STREAM", pv.clone());
    let server = PvaServer::isolated(Arc::new(src)).expect("server start");
    let addr = server.tcp_addr();
    let server_str = format!("127.0.0.1:{}", addr.port());

    // Background ticker: every 80 ms post a new int value.
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let pv_t = pv.clone();
    let ticker = tokio::spawn(async move {
        let mut i = 1i32;
        while !stop_t.load(Ordering::Relaxed) {
            let val = PvField::Structure(PvStructure {
                struct_id: "epics:nt/NTScalar:1.0".into(),
                fields: vec![("value".into(), PvField::Scalar(ScalarValue::Int(i)))],
            });
            let _ = pv_t.try_post(val);
            i += 1;
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
    });

    // Spawn pvxmonitor for ~1.5 s; SIGTERM it (so it flushes
    // stdout before exit — SIGKILL drops the buffered output);
    // collect stdout.
    let pvxmonitor_p = pvxmonitor.clone();
    let server_str_c = server_str.clone();
    let lib = pvxs_lib_dir().into_os_string();
    let out = tokio::task::spawn_blocking(move || {
        let mut child = pvxs_command(&pvxmonitor_p)
            .arg("M:STREAM")
            .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
            .env("EPICS_PVA_NAME_SERVERS", &server_str_c)
            .env(env_key(), lib)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("pvxmonitor spawn");
        std::thread::sleep(Duration::from_millis(1500));
        // SIGTERM so the child has a chance to flush stdout.
        unsafe {
            libc::kill(child.id() as i32, libc::SIGTERM);
        }
        std::thread::sleep(Duration::from_millis(150));
        // Fallback hard-kill if still alive.
        let _ = child.kill();
        child.wait_with_output().expect("wait")
    })
    .await
    .expect("join");

    stop.store(true, Ordering::Relaxed);
    let _ = ticker.await;
    server.stop();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();

    // Parse `M:STREAM` event blocks: count the `value int32_t = N`
    // lines and pull out the integer values to confirm ordering.
    let mut seen: Vec<i32> = Vec::new();
    for line in stdout.lines() {
        let s = line.trim();
        if let Some(rest) = s.strip_prefix("value int32_t = ")
            && let Ok(n) = rest.parse::<i32>()
        {
            seen.push(n);
        }
    }
    assert!(
        seen.len() >= 4,
        "expected ≥4 monitor events from Rust server, got {}: {seen:?}\nfull stdout:\n{stdout}",
        seen.len(),
    );
    // Each successive value should be ≥ the previous (the ticker
    // monotonically increments; mpsc / squash semantics may skip
    // intermediates but never reorder).
    for w in seen.windows(2) {
        assert!(
            w[1] >= w[0],
            "monitor sequence not monotonic: {seen:?}\nstdout:\n{stdout}",
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interop_monitor_b_rust_client_streams_from_pvxs_server() {
    let Some(pvxput) = require_pvxs(PVXPUT) else {
        return;
    };
    let Some(helper) = super::interop_helpers::cpp_helper("reverse_server") else {
        return;
    };

    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let ready = std::env::temp_dir().join(format!("mon_b_ready.{port}"));
    let _ = std::fs::remove_file(&ready);

    let mut child = match std::process::Command::new(&helper)
        .arg("--port")
        .arg(port.to_string())
        .arg("--ready")
        .arg(&ready)
        .arg("--writable")
        .env(env_key(), pvxs_lib_dir())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: failed to spawn reverse_server: {e}");
            return;
        }
    };
    let mut up = false;
    for _ in 0..50 {
        if ready.exists() {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !up {
        let _ = child.kill();
        let _ = child.wait();
        eprintln!("SKIP: reverse_server not ready");
        return;
    }
    let _ = std::fs::remove_file(&ready);

    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let client = epics_pva_rs::client_native::PvaClient::builder()
        .timeout(Duration::from_secs(5))
        .name_servers(vec![addr])
        .build();

    let received = Arc::new(std::sync::Mutex::new(Vec::<i32>::new()));
    let received_cb = received.clone();
    let handle = client
        .pvmonitor_handle(
            "W:INT",
            move |_desc, v| {
                if let PvField::Structure(s) = v
                    && let Some((_, PvField::Scalar(ScalarValue::Int(i)))) =
                        s.fields.iter().find(|(n, _)| n == "value")
                {
                    received_cb.lock().unwrap().push(*i);
                }
            },
            |_| {},
        )
        .await
        .expect("subscribe");

    // Drive pvxput to update the value 5 times. Sleep between
    // each so the pvxs server can dispatch one MONITOR DATA frame
    // per put.
    let pvxput_p = pvxput.clone();
    let server_str = format!("127.0.0.1:{port}");
    let lib = pvxs_lib_dir().into_os_string();
    for val in 1..=5 {
        let p = pvxput_p.clone();
        let s = server_str.clone();
        let l = lib.clone();
        let _ = tokio::task::spawn_blocking(move || {
            pvxs_command(&p)
                .arg("-w")
                .arg("3")
                .arg("W:INT")
                .arg(val.to_string())
                .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
                .env("EPICS_PVA_NAME_SERVERS", &s)
                .env(env_key(), l)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
        })
        .await;
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    handle.stop();

    let _ = child.kill();
    let _ = child.wait();

    let seen = received.lock().unwrap().clone();
    // 5 puts fire, but the subscribe-establishment race can swallow
    // the first one or two on slow CI. ≥3 distinct events out of 5
    // is enough to prove the MONITOR stream itself works.
    assert!(
        seen.len() >= 3,
        "expected ≥3 monitor events on Rust client, got {}: {seen:?}",
        seen.len(),
    );
    for w in seen.windows(2) {
        assert!(
            w[1] >= w[0],
            "Rust client monitor sequence not monotonic: {seen:?}",
        );
    }
}
