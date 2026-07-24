//! Interop: pvxs C++ client builds the *typed* pipeline
//! pvRequest (`Context::request().record("pipeline", true)`) and
//! connects to a Rust PVA server hosting one PV. Previously the
//! Rust server's `monitor_pipeline_options` parser only matched
//! the parsed-string form (`record[pipeline=true]`) and silently
//! disabled flow control whenever a real pvxs program drove the
//! subscription via the typed builder.
//!
//! The cpp helper (`cpp_helpers/r20_typed_monitor.cpp`) is built
//! on the fly via `c++` against `~/codes/pvxs/{include,lib}` if
//! the binary is missing. Test is SKIPped (not failed) when
//! either the compiler or the pvxs/EPICS-base headers are absent
//! — that way a CI host without pvxs installed isn't a hard
//! failure but a host that *has* pvxs runs the real assertion.

// RTEMS-EXEC-MODEL-ALLOW(1): not run by the default nextest profile - this file is a module of the `interop_pvxs` binary, which `.config/nextest.toml`'s default-filter excludes.

use super::interop_helpers::{pick_localhost_port, pvxs_arch, pvxs_lib_dir};

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

fn epics_base_home() -> PathBuf {
    if let Ok(h) = std::env::var("EPICS_BASE") {
        return PathBuf::from(h);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join("epics/epics-base")
}

fn pvxs_home() -> PathBuf {
    if let Ok(h) = std::env::var("PVXS_HOME") {
        return PathBuf::from(h);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join("codes/pvxs")
}

/// Ensure the helper binary exists. If missing, try to compile it
/// from `cpp_helpers/r20_typed_monitor.cpp`. Returns `Some(path)`
/// on success, `None` on skip with a SKIP-prefixed stderr line.
fn build_helper() -> Option<PathBuf> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/interop_pvxs_mods/cpp_helpers/r20_typed_monitor.cpp");
    if !src.is_file() {
        eprintln!("SKIP: helper source missing: {src:?}");
        return None;
    }
    let out_dir = std::env::var("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    std::fs::create_dir_all(&out_dir).ok();
    let out = out_dir.join("r20_typed_monitor");
    let need_rebuild = !out.is_file()
        || std::fs::metadata(&src).and_then(|m| m.modified()).ok()
            > std::fs::metadata(&out).and_then(|m| m.modified()).ok();
    if !need_rebuild {
        return Some(out);
    }

    let pvxs = pvxs_home();
    let base = epics_base_home();
    let arch = pvxs_arch();
    let pvxs_include = pvxs.join("include");
    let base_include = base.join("include");
    let base_compiler_include = base.join("include/compiler/clang");
    let base_os_include = if cfg!(target_os = "macos") {
        base.join("include/os/Darwin")
    } else {
        base.join("include/os/Linux")
    };
    let pvxs_lib = pvxs.join("lib").join(arch);
    let base_lib = base.join("lib").join(arch);

    for p in [&pvxs_include, &base_include, &pvxs_lib, &base_lib] {
        if !p.exists() {
            eprintln!("SKIP: required path missing for cpp helper build: {p:?}");
            return None;
        }
    }

    let status = Command::new("c++")
        .args(["-std=c++17", "-O0", "-g"])
        .arg(format!("-I{}", pvxs_include.display()))
        .arg(format!("-I{}", base_include.display()))
        .arg(format!("-I{}", base_compiler_include.display()))
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
            eprintln!("SKIP: c++ build of r20 helper failed (exit {s})");
            None
        }
        Err(e) => {
            eprintln!("SKIP: c++ compiler unavailable: {e}");
            None
        }
    }
}

/// Shared capture buffer for tracing output emitted by the Rust
/// server during this test. We install a process-global subscriber
/// once that writes formatted events into this buffer; the test
/// snapshots its length before and after the helper run so other
/// concurrent tests in the same binary don't pollute the slice we
/// inspect.
fn capture_buffer() -> Arc<Mutex<Vec<u8>>> {
    static BUF: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    BUF.get_or_init(|| Arc::new(Mutex::new(Vec::new()))).clone()
}

#[derive(Clone)]
struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
    type Writer = CaptureWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn install_tracing_once() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        use tracing_subscriber::{EnvFilter, fmt};
        let writer = CaptureWriter(capture_buffer());
        let _ = fmt()
            .with_env_filter(
                EnvFilter::try_new("epics_pva_rs=debug")
                    .unwrap_or_else(|_| EnvFilter::new("debug")),
            )
            .with_writer(writer)
            .with_ansi(false)
            .with_target(true)
            .try_init();
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interop_r20_typed_pipeline_from_pvxs_against_rust_server() {
    use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
    use epics_pva_rs::server_native::{PvaServer, SharedPV, SharedSource};
    use std::sync::atomic::{AtomicBool, Ordering};

    let Some(helper) = build_helper() else {
        return;
    };

    install_tracing_once();
    // Snapshot the buffer length so we only inspect output emitted
    // during this test.
    let buf = capture_buffer();
    let start_len = buf.lock().unwrap().len();

    // Spin up a Rust server hosting a counter PV and ticking the
    // value every 100 ms so the subscriber sees a stream, not a
    // single snapshot.
    let pv = SharedPV::new();
    pv.open(
        FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".to_string(),
            fields: vec![("value".to_string(), FieldDesc::Scalar(ScalarType::Double))],
        },
        PvField::Structure(PvStructure {
            struct_id: "epics:nt/NTScalar:1.0".to_string(),
            fields: vec![(
                "value".to_string(),
                PvField::Scalar(ScalarValue::Double(0.0)),
            )],
        }),
    )
    .unwrap();
    let source = SharedSource::new();
    source.add("R20:PV", pv.clone());
    let source_arc = Arc::new(source);

    // Bind the server to a known TCP port — the helper's nameServer
    // entry needs an exact host:port.
    let _bind_port = pick_localhost_port();
    let server = PvaServer::isolated(source_arc).expect("server start");
    let addr = server.tcp_addr();

    // Ticker: post a new value every 100 ms until the helper exits.
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let pv_t = pv.clone();
    let ticker = tokio::spawn(async move {
        let mut i = 1i32;
        while !stop_t.load(Ordering::Relaxed) {
            let val = PvField::Structure(PvStructure {
                struct_id: "epics:nt/NTScalar:1.0".to_string(),
                fields: vec![(
                    "value".to_string(),
                    PvField::Scalar(ScalarValue::Double(i as f64)),
                )],
            });
            let _ = pv_t.try_post(val);
            i += 1;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });

    // Spawn the helper as a subprocess. Wait inside spawn_blocking
    // so the synchronous .wait_with_output() doesn't park the
    // runtime worker.
    let helper_str = helper.display().to_string();
    let server_str = format!("{}:{}", addr.ip(), addr.port());
    let env_key = if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else {
        "LD_LIBRARY_PATH"
    };
    let lib_dir = pvxs_lib_dir();

    let output = tokio::task::spawn_blocking(move || {
        let child = Command::new(&helper_str)
            .arg("--server")
            .arg(&server_str)
            .arg("--pv")
            .arg("R20:PV")
            .arg("--events")
            .arg("3")
            .arg("--timeout")
            .arg("6")
            .env(env_key, lib_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child.wait_with_output()
    })
    .await
    .expect("join helper");
    let output = match output {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP: failed to run r20 helper: {e}");
            stop.store(true, Ordering::Relaxed);
            let _ = ticker.await;
            server.stop();
            return;
        }
    };

    stop.store(true, Ordering::Relaxed);
    let _ = ticker.await;
    server.stop();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "pvxs typed-pipeline client did not receive the expected \
         events from the Rust server. Helper exit={:?}\n\
         stdout: {stdout}\nstderr: {stderr}",
        output.status,
    );

    // Discriminating assertion: the Rust server emits a debug event
    // `MONITOR INIT pipeline negotiated` only when the parser
    // recognises the typed-Bool pipeline option. Previously the parser
    // returned None for typed-Bool, the event never fired, the server
    // still echoed events back to the client (no flow control), so
    // the helper would still exit 0 — only this log assertion
    // distinguishes a fixed parser from a regressed one.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let captured = {
        let g = buf.lock().unwrap();
        String::from_utf8_lossy(&g[start_len..]).to_string()
    };
    assert!(
        captured.contains("MONITOR INIT pipeline negotiated"),
        "Regression: Rust server did not log \
         `MONITOR INIT pipeline negotiated` for the typed-Bool pipeline \
         pvRequest. monitor_pipeline_options either failed to match the \
         typed shape or short-circuited before installing the window. \
         Captured server output during the test window:\n{captured}",
    );
}
