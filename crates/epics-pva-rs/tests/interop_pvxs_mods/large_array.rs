//! Large-array cross-impl. Catches:
//!
//! - Server-side: encoder must handle payloads >> typical segment
//!   boundary (~16K). pvxs splits into multiple SegFirst /
//!   SegMiddle / SegLast frames; Rust currently emits a single
//!   large frame (PVA wire allows u32 payload size). Both forms
//!   must be acceptable to the peer.
//! - Client-side: segmented-message reassembly
//!   path must hold for real pvxs server output.
//!
//! Tests use 100K elements of f64 (~800KB) — large enough to
//! cross both the 16K libevent default and any TCP buffer
//! boundary, small enough to stay quick.

// RTEMS-EXEC-MODEL-ALLOW(2): not run by the default nextest profile - this file is a module of the `interop_pvxs` binary, which `.config/nextest.toml`'s default-filter excludes.

use super::interop_helpers::{PVXGET, pvxs_command, pvxs_lib_dir, require_pvxs};

use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{PvaServer, SharedPV, SharedSource};

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

fn env_key() -> &'static str {
    if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else {
        "LD_LIBRARY_PATH"
    }
}

const N: usize = 100_000;

fn build_large_array_pv() -> SharedPV {
    let desc = FieldDesc::Structure {
        struct_id: "epics:nt/NTScalarArray:1.0".into(),
        fields: vec![("value".into(), FieldDesc::ScalarArray(ScalarType::Double))],
    };
    let values: Vec<ScalarValue> = (0..N)
        .map(|i| ScalarValue::Double(i as f64 * 0.5))
        .collect();
    let value = PvField::Structure(PvStructure {
        struct_id: "epics:nt/NTScalarArray:1.0".into(),
        fields: vec![("value".into(), PvField::ScalarArray(values))],
    });
    let pv = SharedPV::new();
    pv.open(desc, value).unwrap();
    pv
}

/// Direction A: Rust server hosts 100K doubles. pvxget reads
/// the entire array. pvxget by default truncates printing to 20
/// elements; we pass `-# 0` to print all, then count distinct
/// numeric tokens in the output as a rough sanity bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interop_large_array_a_pvxget_reads_huge_rust_array() {
    let Some(pvxget) = require_pvxs(PVXGET) else {
        return;
    };

    let src = SharedSource::new();
    src.add("L:DBL", build_large_array_pv());
    let server = PvaServer::isolated(Arc::new(src)).expect("server start");
    let addr = server.tcp_addr();
    let server_str = format!("127.0.0.1:{}", addr.port());

    let pvxget_p = pvxget.clone();
    let server_str_c = server_str.clone();
    let lib = pvxs_lib_dir().into_os_string();
    let out = tokio::task::spawn_blocking(move || {
        pvxs_command(&pvxget_p)
            .arg("-w")
            .arg("10")
            .arg("-#")
            .arg("0") // unlimited element print — forces pvxget
            // to consume the full reassembled buffer.
            .arg("-r")
            .arg("field(value)")
            .arg("L:DBL")
            .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
            .env("EPICS_PVA_NAME_SERVERS", &server_str_c)
            .env(env_key(), lib)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("pvxget exec")
    })
    .await
    .expect("join pvxget");

    server.stop();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "pvxget exit={:?} for large array\nstderr: {stderr}\nstdout (first 200B): {}",
        out.status,
        &stdout.chars().take(200).collect::<String>(),
    );
    // pvxget prints `value double[] = {N}[...]` — the count
    // marker tells us how many elements were decoded. Must match
    // the array we hosted.
    let marker = format!("value double[] = {{{N}}}");
    assert!(
        stdout.contains(&marker),
        "expected `{marker}` in pvxget output (segment reassembly truncated?). \
         first 400B of stdout:\n{}",
        &stdout.chars().take(400).collect::<String>(),
    );
    // Spot-check a couple of values that fall in the middle and
    // end of the array so a partial-buffer failure can't hide.
    let mid = 50_000_f64 * 0.5;
    let last = (N as f64 - 1.0) * 0.5;
    assert!(
        stdout.contains(&format!("{mid}")),
        "mid-array value {mid} missing"
    );
    assert!(
        stdout.contains(&format!("{last}")),
        "last-array value {last} missing"
    );
}

/// Direction B: pvxs server hosts 100K-double array; Rust client
/// decodes the full array (exercising segmented-message reassembly
/// in `client_native/server_conn.rs`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interop_large_array_b_rust_client_reads_huge_pvxs_array() {
    let Some(helper) = super::interop_helpers::cpp_helper("reverse_server") else {
        return;
    };

    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let ready = std::env::temp_dir().join(format!("large_arr_ready.{port}"));
    let _ = std::fs::remove_file(&ready);

    let mut child = match std::process::Command::new(&helper)
        .arg("--port")
        .arg(port.to_string())
        .arg("--ready")
        .arg(&ready)
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
        .timeout(Duration::from_secs(10))
        .name_servers(vec![addr])
        .build();

    let decoded = match client.pvget("L:DBL").await {
        Ok(v) => v,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("pvget on L:DBL failed: {e:?}");
        }
    };

    let _ = child.kill();
    let _ = child.wait();

    let PvField::Structure(s) = &decoded else {
        panic!("expected struct, got {decoded:?}");
    };
    let (_, v) = s
        .fields
        .iter()
        .find(|(n, _)| n == "value")
        .expect("value field");
    use epics_pva_rs::pvdata::TypedScalarArray;
    let xs: Vec<f64> = match v {
        PvField::ScalarArray(arr) => arr
            .iter()
            .map(|sv| match sv {
                ScalarValue::Double(d) => *d,
                _ => f64::NAN,
            })
            .collect(),
        PvField::ScalarArrayTyped(TypedScalarArray::Double(arr)) => arr.to_vec(),
        _ => panic!("expected Double array, got {v:?}"),
    };
    assert_eq!(
        xs.len(),
        N,
        "Rust client decoded short array (segment reassembly bug?)"
    );
    assert!((xs[0] - 0.0).abs() < 1e-9, "xs[0] = {}", xs[0]);
    assert!(
        (xs[50_000] - 25_000.0).abs() < 1e-9,
        "xs[50k] = {}",
        xs[50_000]
    );
    assert!(
        (xs[N - 1] - (N as f64 - 1.0) * 0.5).abs() < 1e-9,
        "xs[last] = {}",
        xs[N - 1]
    );
}
