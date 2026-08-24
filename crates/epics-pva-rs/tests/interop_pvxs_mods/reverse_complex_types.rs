//! Reverse-direction cross-impl interop: pvxs server → Rust client.
//!
//! Symmetric to `complex_types.rs`. A C++ helper
//! (`cpp_helpers/reverse_server.cpp`) hosts the same matrix of
//! NT shapes via `pvxs::server::Server` + `nt::NTScalar` /
//! `NTEnum` / `NTTable` builders. The Rust `PvaClient` connects
//! via `EPICS_PVA_NAME_SERVERS` (TCP search → handler in the
//! pvxs server) and GETs each PV. Test asserts the decoded
//! `PvField` value matches the value we know pvxs set on its
//! side.
//!
//! Catches decoder regressions that the forward direction can
//! never catch: pvxs's encoder may emit subtly different bytes
//! than Rust's (different field-order canonicalisation, different
//! type-cache assignment, etc.), and any of those differences
//! must round-trip through the Rust decoder.
//!
//! SKIPped if either `c++` or the pvxs headers are absent.

// RTEMS-EXEC-MODEL-ALLOW(1): not run by the default nextest profile - this file is a module of the `interop_pvxs` binary, which `.config/nextest.toml`'s default-filter excludes.

use super::interop_helpers::pvxs_lib_dir;

use epics_pva_rs::pvdata::{PvField, ScalarValue};

/// Normalise a PvField scalar-array to `Vec<ScalarValue>` regardless
/// of whether the decoder produced the boxed `ScalarArray` or the
/// typed fast-path `ScalarArrayTyped` variant.
fn array_to_vec(v: &PvField) -> Option<Vec<ScalarValue>> {
    match v {
        PvField::ScalarArray(xs) => Some(xs.clone()),
        PvField::ScalarArrayTyped(t) => Some(t.to_scalar_values()),
        _ => None,
    }
}

use std::process::{Command, Stdio};
use std::time::Duration;

/// Walk into a struct value and pull the `value` leaf (or return
/// the struct itself when it's a 2-level value-and-choices enum).
fn extract_value(decoded: &PvField, path: &str) -> Option<PvField> {
    let mut cur = decoded;
    for segment in path.split('.') {
        let PvField::Structure(s) = cur else {
            return None;
        };
        let next = s
            .fields
            .iter()
            .find(|(n, _)| n == segment)
            .map(|(_, v)| v)?;
        cur = next;
    }
    Some(cur.clone())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interop_reverse_complex_types_rust_client_decodes_pvxs() {
    let Some(helper) = super::interop_helpers::cpp_helper("reverse_server") else {
        return;
    };

    // Bind a port for the pvxs server.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let ready = std::env::temp_dir().join(format!("reverse_ready.{port}"));
    let _ = std::fs::remove_file(&ready);

    let mut cmd = Command::new(&helper);
    cmd.arg("--port")
        .arg(port.to_string())
        .arg("--ready")
        .arg(&ready)
        .env(
            if cfg!(target_os = "macos") {
                "DYLD_LIBRARY_PATH"
            } else {
                "LD_LIBRARY_PATH"
            },
            pvxs_lib_dir(),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: failed to spawn reverse_server: {e}");
            return;
        }
    };

    // Wait for the readiness file (≤ 5s).
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
        eprintln!("SKIP: reverse_server did not become ready");
        return;
    }
    let _ = std::fs::remove_file(&ready);

    // Drive a Rust client at the pvxs server via TCP-search-only
    // path so we don't depend on EPICS_PVA_BROADCAST_PORT (which
    // pvxs's UDP responder defaults to 5076).
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let client = epics_pva_rs::client_native::PvaClient::builder()
        .timeout(Duration::from_secs(5))
        .name_servers(vec![addr])
        .build();

    // Expected per PV: (pv name, path-to-value-field, expected ScalarValue
    // or expected ScalarArray).
    enum Expected {
        Scalar(ScalarValue),
        Array(Vec<ScalarValue>),
        EnumIdx(i32),
        TableXs,
    }
    let cases: &[(&str, &str, Expected)] = &[
        (
            "T:STR",
            "value",
            Expected::Scalar(ScalarValue::String("hello world".into())),
        ),
        ("T:INT", "value", Expected::Scalar(ScalarValue::Int(-12345))),
        (
            "T:LONG",
            "value",
            Expected::Scalar(ScalarValue::Long(9_000_000_000_i64)),
        ),
        (
            "T:DBL",
            "value",
            Expected::Scalar(ScalarValue::Double(123.456_789_f64)),
        ),
        (
            "T:WF:DBL",
            "value",
            Expected::Array(vec![
                ScalarValue::Double(1.5),
                ScalarValue::Double(2.5),
                ScalarValue::Double(3.5),
            ]),
        ),
        (
            "T:WF:INT",
            "value",
            Expected::Array(vec![
                ScalarValue::Int(7),
                ScalarValue::Int(8),
                ScalarValue::Int(9),
                ScalarValue::Int(10),
            ]),
        ),
        (
            "T:WF:STR",
            "value",
            Expected::Array(vec![
                ScalarValue::String("alpha".into()),
                ScalarValue::String("beta".into()),
                ScalarValue::String("gamma".into()),
            ]),
        ),
        ("T:ENUM", "value", Expected::EnumIdx(2)),
        ("T:TBL", "value", Expected::TableXs),
    ];

    let mut failures: Vec<String> = Vec::new();
    for (pv, path, expected) in cases {
        let decoded = match client.pvget(pv).await {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!("[{pv}] pvget failed: {e:?}"));
                continue;
            }
        };
        let Some(value_field) = extract_value(&decoded, path) else {
            failures.push(format!(
                "[{pv}] could not locate `{path}` in decoded struct: {decoded:?}"
            ));
            continue;
        };
        match expected {
            Expected::Scalar(want) => match &value_field {
                PvField::Scalar(got) if got == want => {}
                _ => failures.push(format!("[{pv}] scalar mismatch: got {value_field:?}")),
            },
            Expected::Array(want) => match array_to_vec(&value_field) {
                Some(got) if &got == want => {}
                Some(got) => {
                    failures.push(format!("[{pv}] array mismatch: got {got:?}, want {want:?}"))
                }
                None => failures.push(format!("[{pv}] expected scalar array, got {value_field:?}")),
            },
            Expected::EnumIdx(want) => {
                // NTEnum: value field is a struct{index, choices}.
                let PvField::Structure(s) = &value_field else {
                    failures.push(format!("[{pv}] enum value not a struct: {value_field:?}"));
                    continue;
                };
                let idx = s.fields.iter().find_map(|(n, v)| match v {
                    PvField::Scalar(ScalarValue::Int(i)) if n == "index" => Some(*i),
                    _ => None,
                });
                if idx != Some(*want) {
                    failures.push(format!("[{pv}] enum index mismatch: got {idx:?}"));
                }
            }
            Expected::TableXs => {
                let PvField::Structure(s) = &value_field else {
                    failures.push(format!("[{pv}] table value not a struct: {value_field:?}"));
                    continue;
                };
                let xs = s
                    .fields
                    .iter()
                    .find(|(n, _)| n == "xs")
                    .and_then(|(_, v)| array_to_vec(v));
                let want = vec![
                    ScalarValue::Double(1.0),
                    ScalarValue::Double(2.0),
                    ScalarValue::Double(3.0),
                ];
                if xs.as_deref() != Some(want.as_slice()) {
                    failures.push(format!("[{pv}] table xs mismatch: got {xs:?}"));
                }
            }
        }
    }

    // Tear down pvxs server.
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        failures.is_empty(),
        "{} reverse-direction interop failure(s):\n{}",
        failures.len(),
        failures.join("\n----\n"),
    );
}
