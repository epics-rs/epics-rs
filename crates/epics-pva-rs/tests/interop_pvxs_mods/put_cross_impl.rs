//! PUT cross-impl interop — direction A (pvxs → Rust server) and
//! direction B (Rust client → pvxs server). Both round-trip via
//! GET to assert the PUT actually applied.
//!
//! Pre-existing tests covered:
//!   - Rust↔Rust PUT (unit + parity tests)
//!   - GET cross-impl (forward + reverse goldens)
//!   - PUT bounds + PUTFAIL semantics
//!
//! Missing until this batch:
//!   - The PUT wire path itself across implementations. A bug in
//!     either side's PUT request encoder / response decoder would
//!     silently desync without this check.

// RTEMS-EXEC-MODEL-ALLOW(2): not run by the default nextest profile - this file is a module of the `interop_pvxs` binary, which `.config/nextest.toml`'s default-filter excludes.

use super::interop_helpers::{PVXGET, PVXPUT, pvxs_command, require_pvxs};

use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{PvaServer, SharedPV, SharedSource};

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

fn pvxs_lib_dir_str() -> std::ffi::OsString {
    super::interop_helpers::pvxs_lib_dir().into_os_string()
}

/// Direction A: pvxput → Rust SharedPV → readback via Rust SharedPV::current().
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interop_put_a_pvxput_writes_into_rust_server() {
    use std::sync::{Mutex, OnceLock};

    let Some(pvxput) = require_pvxs(PVXPUT) else {
        return;
    };

    // One-shot tracing subscriber: captures the Rust server's
    // debug output for the failure message.
    static BUF: OnceLock<std::sync::Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    let buf = BUF
        .get_or_init(|| std::sync::Arc::new(Mutex::new(Vec::new())))
        .clone();
    static INSTALL: OnceLock<()> = OnceLock::new();
    INSTALL.get_or_init(|| {
        use tracing_subscriber::fmt::MakeWriter;
        #[derive(Clone)]
        struct W(std::sync::Arc<Mutex<Vec<u8>>>);
        impl<'a> MakeWriter<'a> for W {
            type Writer = W;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }
        impl std::io::Write for W {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_new("epics_pva_rs=debug")
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug")),
            )
            .with_writer(W(buf.clone()))
            .with_ansi(false)
            .try_init();
    });
    let start_len = buf.lock().unwrap().len();

    // Rust server hosting two writable PVs. pvxs has no implicit-writable
    // SharedPV, and neither does the Rust port: a plain SharedPV::new()
    // rejects client PUTs with "PUT not supported by this PV". Use
    // build_mailbox() so an inbound PUT stores the value and lands in
    // current() — mirrors pvxs SharedPV::buildMailbox.
    let str_pv = SharedPV::build_mailbox();
    str_pv
        .open(
            FieldDesc::Structure {
                struct_id: "epics:nt/NTScalar:1.0".into(),
                fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::String))],
            },
            PvField::Structure(PvStructure {
                struct_id: "epics:nt/NTScalar:1.0".into(),
                fields: vec![(
                    "value".into(),
                    PvField::Scalar(ScalarValue::String("initial".into())),
                )],
            }),
        )
        .unwrap();
    let int_pv = SharedPV::build_mailbox();
    int_pv
        .open(
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
    src.add("W:A:STR", str_pv.clone());
    src.add("W:A:INT", int_pv.clone());
    let server = PvaServer::isolated(Arc::new(src)).expect("server start");
    let addr = server.tcp_addr();
    let server_str = format!("127.0.0.1:{}", addr.port());

    // Fire pvxput for each PV in spawn_blocking so the synchronous
    // wait_with_output doesn't park the runtime worker.
    for (pv, val) in [("W:A:STR", "written-by-pvxput"), ("W:A:INT", "9991")] {
        let pvxput = pvxput.clone();
        let server_str = server_str.clone();
        let pv_s = pv.to_string();
        let val_s = val.to_string();
        let lib = pvxs_lib_dir_str();
        let env_key = if cfg!(target_os = "macos") {
            "DYLD_LIBRARY_PATH"
        } else {
            "LD_LIBRARY_PATH"
        };
        let out = tokio::task::spawn_blocking(move || {
            pvxs_command(&pvxput)
                .arg("-w")
                .arg("3")
                .arg(&pv_s)
                .arg(&val_s)
                .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
                .env("EPICS_PVA_NAME_SERVERS", &server_str)
                .env(env_key, lib)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .expect("pvxput exec")
        })
        .await
        .expect("join");
        if !out.status.success() {
            let server_log = {
                let g = buf.lock().unwrap();
                String::from_utf8_lossy(&g[start_len..]).to_string()
            };
            panic!(
                "pvxput {pv} {val} exit={:?}\npvxput stderr: {}\n--- Rust server log ---\n{server_log}",
                out.status,
                String::from_utf8_lossy(&out.stderr),
            );
        }
    }

    // Settle: SharedPV.put runs on the connection's read task, the
    // pvxput exit returns when the server ACK'd the PUT_EXEC, which
    // happens *after* the apply. Small breathing room for the
    // post() that follows.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let str_val = str_pv.current().expect("W:A:STR current");
    let int_val = int_pv.current().expect("W:A:INT current");

    fn value_field(pv: &PvField) -> Option<&PvField> {
        let PvField::Structure(s) = pv else {
            return None;
        };
        s.fields.iter().find(|(n, _)| n == "value").map(|(_, v)| v)
    }
    assert_eq!(
        value_field(&str_val),
        Some(&PvField::Scalar(ScalarValue::String(
            "written-by-pvxput".into()
        ))),
        "W:A:STR did not apply pvxput value: got {str_val:?}",
    );
    assert_eq!(
        value_field(&int_val),
        Some(&PvField::Scalar(ScalarValue::Int(9991))),
        "W:A:INT did not apply pvxput value: got {int_val:?}",
    );

    server.stop();
}

/// Direction B: Rust client → pvxs writable server (reverse_server
/// --writable) → readback via pvxget.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interop_put_b_rust_client_writes_into_pvxs_server() {
    let Some(pvxget) = require_pvxs(PVXGET) else {
        return;
    };
    // Build reverse_server (re-uses batch-1 helper compile pipeline).
    let Some(helper) = build_reverse_server() else {
        return;
    };

    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let ready = std::env::temp_dir().join(format!("rev_ready_w.{port}"));
    let _ = std::fs::remove_file(&ready);

    let env_key = if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else {
        "LD_LIBRARY_PATH"
    };

    let mut child = match std::process::Command::new(&helper)
        .arg("--port")
        .arg(port.to_string())
        .arg("--ready")
        .arg(&ready)
        .arg("--writable")
        .env(env_key, pvxs_lib_dir_str())
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
        eprintln!("SKIP: reverse_server did not become ready");
        return;
    }
    let _ = std::fs::remove_file(&ready);

    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let client = epics_pva_rs::client_native::PvaClient::builder()
        .timeout(Duration::from_secs(5))
        .name_servers(vec![addr])
        .build();

    client
        .pvput("W:STR", "written-by-rust-client")
        .await
        .expect("Rust client PUT to W:STR");
    client
        .pvput("W:INT", "7777")
        .await
        .expect("Rust client PUT to W:INT");

    // Read back via pvxget.
    let server_str = format!("127.0.0.1:{port}");
    let server_str_c = server_str.clone();
    let pvxget_c = pvxget.clone();
    let env_key_c = env_key;
    let out = tokio::task::spawn_blocking(move || {
        pvxs_command(&pvxget_c)
            .arg("-w")
            .arg("3")
            .arg("W:STR")
            .arg("W:INT")
            .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
            .env("EPICS_PVA_NAME_SERVERS", &server_str_c)
            .env(env_key_c, pvxs_lib_dir_str())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("pvxget exec")
    })
    .await
    .expect("join");

    let _ = child.kill();
    let _ = child.wait();

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "pvxget exit={:?}\nstdout: {stdout}\nstderr: {stderr}",
        out.status,
    );
    assert!(
        stdout.contains(r#"value string = "written-by-rust-client""#),
        "W:STR readback missing expected value.\nstdout: {stdout}",
    );
    assert!(
        stdout.contains("value int32_t = 7777"),
        "W:INT readback missing expected value.\nstdout: {stdout}",
    );
}

// ------------------------------------------------------------------
// Reuse the batch-1 build pipeline for reverse_server. Kept private
// here so we don't pull the whole reverse module just to share one
// function.
fn build_reverse_server() -> Option<PathBuf> {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/interop_pvxs_mods/cpp_helpers/reverse_server.cpp");
    if !src.is_file() {
        eprintln!("SKIP: reverse_server source missing");
        return None;
    }
    let out_dir = std::env::var("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    std::fs::create_dir_all(&out_dir).ok();
    let out = out_dir.join("reverse_server");
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
    let status = std::process::Command::new("c++")
        .args(["-std=c++17", "-O0", "-g"])
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
            eprintln!("SKIP: c++ build of reverse_server failed (exit {s})");
            None
        }
        Err(e) => {
            eprintln!("SKIP: c++ compiler unavailable: {e}");
            None
        }
    }
}
