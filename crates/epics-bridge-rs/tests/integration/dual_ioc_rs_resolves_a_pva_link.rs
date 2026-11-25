//! dual-ioc-rs declares `pva` before its `--db` loads, as an IOC linking
//! pvxs7x.dbd has `link("pva", "lsetPVX")`, so a `{pva:…}` link it loads
//! resolves, as softIocPVA R7.0.10-146-g8f5015b66 resolves it:
//!
//! ```text
//! record(ao, "SRC") { field(VAL, "5") field(PINI, "YES") }
//! record(ai, "DST") { field(INP, {pva:"SRC"}) field(SCAN, "1 second") }
//!
//! $ pvget DST DST.SEVR
//! DST       5
//! DST.SEVR  (0) NO_ALARM
//! ```
#![cfg(all(feature = "dual-ioc-bin", unix))]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use epics_pva_rs::client::PvaClient;
use epics_pva_rs::pvdata::{PvField, ScalarValue};

/// `(value, alarm.severity)` of `pv`'s NTScalar.
async fn value_and_severity(client: &PvaClient, pv: &str) -> Option<(f64, i32)> {
    let PvField::Structure(s) = client.pvget(pv).await.ok()? else {
        return None;
    };
    let ScalarValue::Double(value) = *s.get_value()? else {
        return None;
    };
    let PvField::Scalar(ScalarValue::Int(sevr)) = s.get_alarm()?.get_field("severity")? else {
        return None;
    };
    Some((value, *sevr))
}

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — the tokio runtime is only
// the client harness, and the IOC under test is the dual-ioc-rs process.
#[tokio::test]
async fn dual_ioc_rs_resolves_a_pva_link_as_soft_ioc_pva() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pl.db");
    std::fs::write(
        &db,
        "record(ao, \"SRC\") { field(VAL, \"5\") field(PINI, \"YES\") }\n\
         record(ai, \"DST\") { field(INP, {pva:\"SRC\"}) field(SCAN, \"1 second\") }\n",
    )
    .unwrap();
    let mut candidates = named_port::Candidates::new(named_port::free_for_tcp_and_udp);
    for tcp in candidates.by_ref() {
        // The link's client reaches this IOC's own server by name server
        // only, so the search port can be the kernel's.
        let mut child = Command::new(env!("CARGO_BIN_EXE_dual-ioc-rs"))
            .arg("--db")
            .arg(&db)
            .args(["--no-ca", "--pva-port", &tcp.to_string()])
            .env_remove("PVXS_QSRV_ENABLE")
            .env_remove("EPICS_IOC_IGNORE_SERVERS")
            .envs([
                ("EPICS_PVAS_INTF_ADDR_LIST", "127.0.0.1".to_string()),
                ("EPICS_PVAS_BROADCAST_PORT", "0".to_string()),
                ("EPICS_PVA_NAME_SERVERS", format!("127.0.0.1:{tcp}")),
                ("EPICS_PVA_ADDR_LIST", String::new()),
                ("EPICS_PVA_AUTO_ADDR_LIST", "NO".to_string()),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn dual-ioc-rs");
        // Its console, the port fallback warning included, is on stdout.
        let console = Arc::new(Mutex::new(String::new()));
        let readers = [
            Box::new(child.stdout.take().unwrap()) as Box<dyn std::io::Read + Send>,
            Box::new(child.stderr.take().unwrap()),
        ]
        .map(|stream| {
            let console = console.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    let mut c = console.lock().unwrap();
                    c.push_str(&line);
                    c.push('\n');
                }
            })
        });
        // The link's first name-server connect can land before the
        // listener is up, and its retry comes 10 s later (pvxs
        // `tcpNSCheckInterval`).
        let client = PvaClient::builder()
            .server_addr(std::net::SocketAddr::from(([127, 0, 0, 1], tcp)))
            .timeout(Duration::from_secs(2))
            .build();
        let mut got = None;
        let deadline = Instant::now() + Duration::from_secs(30);
        while got.is_none() && Instant::now() < deadline && child.try_wait().unwrap().is_none() {
            got = value_and_severity(&client, "DST")
                .await
                .filter(|&g| g == (5.0, 0));
            if got.is_none() {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        for reader in readers {
            reader.join().unwrap();
        }
        let console = console.lock().unwrap().clone();
        if let Some(got) = got {
            assert_eq!(got, (5.0, 0));
            // softIocPVX's `enable2()` line, printed once at its registrar.
            assert_eq!(
                console
                    .matches("INFO: PVXS QSRV2 is loaded, permitted, and ENABLED.\n")
                    .count(),
                1,
                "{console}"
            );
            return;
        }
        // A taken TCP port is not fatal: the server falls back to another
        // (runtime.rs), which neither this client nor the link finds.
        if !console.contains(&format!("Server unable to bind port {tcp},")) {
            panic!(
                "dual-ioc-rs on tcp {tcp} never resolved DST, and no port was taken:\n{console}"
            );
        }
    }
    candidates.exhausted()
}
