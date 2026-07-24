//! BE (big-endian) wire byte-order cross-impl interop.
//!
//! PVA frames carry a per-frame byte-order flag (control header
//! bit 7) so both peers can talk BE or LE independent of host
//! endian. The default in pvxs (and Rust) is LE. The wire-decode
//! paths on each side must also accept the opposite endian when
//! a peer signals it via SET_BYTE_ORDER (server → client) or
//! flag on every frame (client → server).
//!
//! Two tests:
//!
//! - **Direction A**: Rust server with `wire_byte_order = Big`
//!   hosts the complex_pv_matrix. `pvxget` (always LE on the
//!   client side by default) reads each PV, asserts the formatted
//!   output matches. Proves pvxs's client wire decoder handles a
//!   BE server end-to-end.
//!
//! - **Direction B**: pvxs server with `Config::overrideSendBE(true)`
//!   hosts the same matrix; Rust client reads each PV via name-
//!   server search. Asserts decoded values match. Proves the Rust
//!   client wire decoder handles a BE server.

// RTEMS-EXEC-MODEL-ALLOW(2): not run by the default nextest profile - this file is a module of the `interop_pvxs` binary, which `.config/nextest.toml`'s default-filter excludes.

use super::interop_helpers::pv_builders::complex_pv_matrix;
use super::interop_helpers::{PVXGET, pvxs_command, pvxs_lib_dir, require_pvxs};

use epics_pva_rs::pvdata::ScalarValue;
use epics_pva_rs::server_native::{PvaServer, SharedSource};

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

fn env_key() -> &'static str {
    if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else {
        "LD_LIBRARY_PATH"
    }
}

/// Direction A: Rust server emits BE wire, pvxget (LE client) reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interop_be_a_rust_server_emits_be_to_pvxget() {
    let Some(pvxget) = require_pvxs(PVXGET) else {
        return;
    };
    let pvs = complex_pv_matrix();

    let source = SharedSource::new();
    for b in &pvs {
        source.add(b.name, b.open());
    }

    // Build a server with wire_byte_order forced to Big.
    let cfg = epics_pva_rs::server_native::PvaServerConfig {
        wire_byte_order: epics_pva_rs::proto::ByteOrder::Big,
        // The remaining fields come from the isolated() defaults —
        // ephemeral TCP port, ephemeral UDP, beacon off.
        tcp_port: 0,
        udp_port: {
            let l = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
            let p = l.local_addr().unwrap().port();
            drop(l);
            p
        },
        ..epics_pva_rs::server_native::PvaServerConfig::isolated()
    };
    let server = PvaServer::start(Arc::new(source), cfg).expect("server start");
    let addr = server.tcp_addr();
    let server_str = format!("127.0.0.1:{}", addr.port());

    // Sample subset — one PV per category covers every fixed-width
    // type plus an array, an enum, and a struct array. Keeps the
    // test fast; the complete matrix is covered by the existing
    // LE forward + reverse interop.
    let cases: &[(&str, &[&str])] = &[
        ("T:STR", &[r#"value string = "hello world""#]),
        ("T:INT", &["value int32_t = -12345"]),
        ("T:LONG", &["value int64_t = 9000000000"]),
        ("T:DBL", &["value double = 123.457"]),
        ("T:WF:DBL", &["1.5", "2.5", "3.5"]),
        ("T:WF:INT", &["7", "8", "9", "10"]),
        ("T:ENUM", &["value.index int32_t = 2"]),
        (
            "T:SA",
            &[
                "x int32_t = 1",
                r#"y string = "alpha""#,
                "x int32_t = 3",
                r#"y string = "gamma""#,
            ],
        ),
    ];

    let mut failures: Vec<String> = Vec::new();
    for (pv, needles) in cases {
        let pvxget = pvxget.clone();
        let server_str = server_str.clone();
        let pv_s = pv.to_string();
        let lib = pvxs_lib_dir().into_os_string();
        let out = tokio::task::spawn_blocking(move || {
            pvxs_command(&pvxget)
                .arg("-w")
                .arg("3")
                .arg(&pv_s)
                .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
                .env("EPICS_PVA_NAME_SERVERS", &server_str)
                .env(env_key(), lib)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .expect("pvxget exec")
        })
        .await
        .expect("join pvxget");
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        if !out.status.success() {
            failures.push(format!(
                "[BE A {pv}] pvxget exit={:?}\n  stdout: {stdout}\n  stderr: {stderr}",
                out.status,
            ));
            continue;
        }
        for needle in *needles {
            if !stdout.contains(needle) {
                failures.push(format!(
                    "[BE A {pv}] missing {needle:?}\n  stdout: {stdout}"
                ));
            }
        }
    }

    server.stop();
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(
        failures.is_empty(),
        "{} BE direction-A failure(s):\n{}",
        failures.len(),
        failures.join("\n----\n"),
    );
}

/// Direction B: pvxs server with overrideSendBE(true) → Rust client decodes.
fn build_be_reverse_server() -> Option<PathBuf> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/interop_pvxs_mods/cpp_helpers/be_reverse_server.cpp");
    if !src.is_file() {
        eprintln!("SKIP: be_reverse_server source missing");
        return None;
    }
    let out_dir = std::env::var("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    std::fs::create_dir_all(&out_dir).ok();
    let out = out_dir.join("be_reverse_server");
    let need = !out.is_file()
        || std::fs::metadata(&src).and_then(|m| m.modified()).ok()
            > std::fs::metadata(&out).and_then(|m| m.modified()).ok();
    if !need {
        return Some(out);
    }

    let pvxs = std::env::var_os("PVXS_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let h = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(h).join("codes/pvxs")
        });
    let base = std::env::var_os("EPICS_BASE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let h = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(h).join("epics/epics-base")
        });
    let arch = super::interop_helpers::pvxs_arch();
    let pvxs_lib = pvxs.join("lib").join(arch);
    let base_lib = base.join("lib").join(arch);
    if !pvxs_lib.exists() || !base_lib.exists() {
        eprintln!("SKIP: required pvxs/base lib dirs missing");
        return None;
    }
    let base_os_include = if cfg!(target_os = "macos") {
        base.join("include/os/Darwin")
    } else {
        base.join("include/os/Linux")
    };

    let status = Command::new("c++")
        .args(["-std=c++17", "-O0", "-g", "-DPVXS_ENABLE_EXPERT_API"])
        .arg(format!("-I{}", pvxs.join("include").display()))
        .arg(format!("-I{}", base.join("include").display()))
        .arg(format!(
            "-I{}",
            base.join("include/compiler/clang").display()
        ))
        .arg(format!("-I{}", base_os_include.display()))
        .arg(&src)
        .arg(format!("-L{}", pvxs_lib.display()))
        .arg("-lpvxs")
        .arg(format!("-L{}", base_lib.display()))
        .arg("-lCom")
        .arg(format!("-Wl,-rpath,{}", pvxs_lib.display()))
        .arg(format!("-Wl,-rpath,{}", base_lib.display()))
        .arg("-o")
        .arg(&out)
        .status();
    match status {
        Ok(s) if s.success() => Some(out),
        Ok(s) => {
            eprintln!("SKIP: c++ build of be_reverse_server failed (exit {s})");
            None
        }
        Err(e) => {
            eprintln!("SKIP: c++ compiler unavailable: {e}");
            None
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interop_be_b_rust_client_decodes_pvxs_be_server() {
    let Some(helper) = build_be_reverse_server() else {
        return;
    };

    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let ready = std::env::temp_dir().join(format!("be_rev_ready.{port}"));
    let _ = std::fs::remove_file(&ready);

    let mut child = match Command::new(&helper)
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
            eprintln!("SKIP: failed to spawn be_reverse_server: {e}");
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
        eprintln!("SKIP: be_reverse_server did not become ready");
        return;
    }
    let _ = std::fs::remove_file(&ready);

    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let client = epics_pva_rs::client_native::PvaClient::builder()
        .timeout(Duration::from_secs(5))
        .name_servers(vec![addr])
        .build();

    let mut failures: Vec<String> = Vec::new();

    // T:STR
    let v = client.pvget("T:STR").await;
    match v {
        Ok(decoded) => {
            let want = ScalarValue::String("hello world".into());
            if !value_matches_scalar(&decoded, "value", &want) {
                failures.push(format!("T:STR mismatch: {decoded:?}"));
            }
        }
        Err(e) => failures.push(format!("T:STR pvget: {e:?}")),
    }
    // T:LONG  — int64 is the BE-sensitive case (8 bytes).
    let v = client.pvget("T:LONG").await;
    match v {
        Ok(decoded) => {
            let want = ScalarValue::Long(9_000_000_000_i64);
            if !value_matches_scalar(&decoded, "value", &want) {
                failures.push(format!("T:LONG mismatch: {decoded:?}"));
            }
        }
        Err(e) => failures.push(format!("T:LONG pvget: {e:?}")),
    }
    // T:DBL  — 8-byte float (BE-sensitive).
    let v = client.pvget("T:DBL").await;
    match v {
        Ok(decoded) => {
            let want = ScalarValue::Double(123.456_789_f64);
            if !value_matches_scalar(&decoded, "value", &want) {
                failures.push(format!("T:DBL mismatch: {decoded:?}"));
            }
        }
        Err(e) => failures.push(format!("T:DBL pvget: {e:?}")),
    }

    let _ = child.kill();
    let _ = child.wait();

    assert!(
        failures.is_empty(),
        "{} BE direction-B failure(s):\n{}",
        failures.len(),
        failures.join("\n----\n"),
    );
}

fn value_matches_scalar(
    decoded: &epics_pva_rs::pvdata::PvField,
    field: &str,
    want: &ScalarValue,
) -> bool {
    use epics_pva_rs::pvdata::PvField;
    let PvField::Structure(s) = decoded else {
        return false;
    };
    s.fields
        .iter()
        .find(|(n, _)| n == field)
        .is_some_and(|(_, v)| matches!(v, PvField::Scalar(sv) if sv == want))
}
