//! Cross-implementation interop for complex PVA structures.
//!
//! Rust PVA server hosts a matrix of PVs covering every NT shape
//! pvxs ships built-in plus a deeply-nested generic structure.
//! Real `pvxget` reads each one (via `EPICS_PVA_NAME_SERVERS` →
//! TCP search) and the test asserts the formatted output
//! contains the expected values. Catches encoder bugs in Structure
//! / ScalarArray / StructureArray / String paths that the simpler
//! NTScalar-Double-only tests cannot.
//!
//! When `UPDATE_GOLDENS=1` is set in the environment, this test
//! additionally re-encodes each PV via the Rust encoder and writes
//! the bytes to `tests/fixtures/pvxs/<pv>.bin` after a successful
//! pvxget round-trip. The default-suite golden replay
//! (`tests/wire_golden_complex_types.rs`) then compares the same
//! encoder output against those fixtures on every push without
//! needing pvxs locally. The trust chain: interop verifies pvxs
//! accepts the bytes → capture freezes them → default suite holds
//! the encoder stable against the frozen artifacts.
//!
//! SKIPped if `pvxget` not found.

// RTEMS-EXEC-MODEL-ALLOW(1): not run by the default nextest profile - this file is a module of the `interop_pvxs` binary, which `.config/nextest.toml`'s default-filter excludes.

use super::interop_helpers::pv_builders::{PvBuild, complex_pv_matrix, encode_pv_fixture};
use super::interop_helpers::{PVXGET, pvxs_command, require_pvxs};

use epics_pva_rs::server_native::{PvaServer, SharedSource};

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

async fn pvxget_capture(
    pvxget: &std::path::Path,
    server_str: String,
    pv_name: &'static str,
    extra_args: &[&'static str],
) -> std::process::Output {
    let pvxget = pvxget.to_path_buf();
    let pv_name = pv_name.to_string();
    let extra: Vec<String> = extra_args.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let mut cmd = pvxs_command(&pvxget);
        cmd.arg("-w").arg("3");
        for a in &extra {
            cmd.arg(a);
        }
        cmd.arg(&pv_name)
            .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
            .env("EPICS_PVA_ADDR_LIST", "")
            .env("EPICS_PVA_NAME_SERVERS", &server_str)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        cmd.output().expect("pvxget exec")
    })
    .await
    .expect("join pvxget")
}

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pvxs")
}

fn capture_golden(build: &PvBuild) {
    let dir = fixture_dir();
    std::fs::create_dir_all(&dir).expect("mkdir fixtures");
    // Replace ':' with '_' so the filename is portable.
    let stem = build.name.replace([':', '/'], "_");
    let path = dir.join(format!("{stem}.bin"));
    let bytes = encode_pv_fixture(build);
    std::fs::write(&path, &bytes).expect("write fixture");
    eprintln!(
        "UPDATE_GOLDENS: wrote {} bytes for {} → {:?}",
        bytes.len(),
        build.name,
        path,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interop_complex_types_pvxget_against_rust_server() {
    let Some(pvxget) = require_pvxs(PVXGET) else {
        return;
    };

    let pvs = complex_pv_matrix();
    let source = SharedSource::new();
    for b in &pvs {
        source.add(b.name, b.open());
    }
    let server = PvaServer::isolated(Arc::new(source)).expect("server start");
    let addr = server.tcp_addr();
    let server_str = format!("127.0.0.1:{}", addr.port());

    let matrix: &[(&'static str, &[&str])] = &[
        ("T:STR", &[r#"value string = "hello world""#]),
        ("T:INT", &["value int32_t = -12345"]),
        ("T:LONG", &["value int64_t = 9000000000"]),
        ("T:DBL", &["value double = 123.457"]),
        ("T:WF:DBL", &["value double[]", "1.5", "2.5", "3.5"]),
        ("T:WF:INT", &["value int32_t[]", "7", "8", "9", "10"]),
        ("T:WF:STR", &["value string[]", "alpha", "beta", "gamma"]),
        (
            "T:ENUM",
            &[
                "value.index int32_t = 2",
                r#""OFF""#,
                r#""ON""#,
                r#""AUTO""#,
            ],
        ),
        (
            "T:TBL",
            &[
                "labels string[]",
                r#""X axis""#,
                r#""Y axis""#,
                r#""Name""#,
                "value.xs double[]",
                "1",
                "2",
                "3",
                "value.ys double[]",
                "10",
                "20",
                "30",
                "value.name string[]",
                r#""a""#,
                r#""b""#,
                r#""c""#,
            ],
        ),
        (
            "T:NEST",
            &[
                "outer.mid.count int64_t = 987654321",
                r#"outer.mid.label string = "nested-leaf""#,
                "outer.flag bool = true",
                "tags string[]",
                r#""alpha""#,
                r#""beta""#,
            ],
        ),
        (
            "T:SA",
            &[
                "points",
                r#"x int32_t = 1"#,
                r#"y string = "alpha""#,
                r#"x int32_t = 2"#,
                r#"y string = "beta""#,
                r#"x int32_t = 3"#,
                r#"y string = "gamma""#,
            ],
        ),
        (
            "T:ANY",
            &[
                // Variant of int prints as `any.<...> int32_t = 424242`.
                "424242",
            ],
        ),
        (
            "T:NDARR",
            &[
                // The top-level `struct "epics:nt/NTNDArray:1.0"` line is
                // deliberately NOT asserted here: pvxget's default Delta
                // format prints the very-top struct line only when the
                // root changed-bit is set (pvxs `FmtDelta::field`,
                // datafmt.cpp:19 `if(verytop && !val.isMarked(false))
                // return;`), and neither pvxs's own server nor this one
                // sets the root bit on a GET reply — `to_wire_valid`
                // emits only the marked leaves with no parent/root bit
                // (dataencode.cpp:416-439) and `Value::mark` cannot set a
                // structure's own bit (data.cpp:256-270). Verified end to
                // end: a real pvxs SharedPV server serving an NTNDArray
                // yields identical Delta output with no top id line. The
                // top-level type identity IS wire-observable via Tree
                // format and is asserted separately below.
                //
                // These needles cover the Delta-observable shapes: Union
                // select (ubyteValue branch), a scalar leaf, a nested
                // Structure (alarm), and a StructureArray-with-Variant
                // element (attribute name).
                "ubyteValue",
                "uniqueId int32_t = 7",
                r#"alarm.message string = "NO_ALARM""#,
                r#"name string = "ColorMode""#,
            ],
        ),
    ];

    let update_goldens = std::env::var_os("UPDATE_GOLDENS").is_some();

    let mut failures: Vec<String> = Vec::new();
    for (pv, needles) in matrix {
        let out = pvxget_capture(&pvxget, server_str.clone(), pv, &[]).await;
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        if !out.status.success() {
            failures.push(format!(
                "[{pv}] pvxget exited non-zero ({:?}).\n  stdout: {stdout}\n  stderr: {stderr}",
                out.status,
            ));
            continue;
        }
        let mut missing: Vec<&str> = Vec::new();
        for needle in *needles {
            if !stdout.contains(needle) {
                missing.push(needle);
            }
        }
        if !missing.is_empty() {
            failures.push(format!(
                "[{pv}] missing substrings: {missing:?}\n  stdout: {stdout}\n  stderr: {stderr}",
            ));
            continue;
        }
        // pvxget accepted these bytes — capture them as golden if
        // the operator asked.
        if update_goldens && let Some(build) = pvs.iter().find(|b| b.name == *pv) {
            capture_golden(build);
        }
    }

    // The NTNDArray top-level type identity (`epics:nt/NTNDArray:1.0`)
    // is not observable in pvxget's default Delta output (see the
    // T:NDARR matrix comment above), but it IS in Tree format, where
    // FmtTree prints every struct's id unconditionally
    // (`datafmt.cpp:196-200`). Assert it there so a regression that drops
    // or corrupts the server's top-level id is still caught by a
    // C-observable pvxget invocation.
    let tree = pvxget_capture(&pvxget, server_str.clone(), "T:NDARR", &["-F", "tree"]).await;
    let tree_stdout = String::from_utf8_lossy(&tree.stdout).to_string();
    let tree_stderr = String::from_utf8_lossy(&tree.stderr).to_string();
    if !tree.status.success() {
        failures.push(format!(
            "[T:NDARR -F tree] pvxget exited non-zero ({:?}).\n  stdout: {tree_stdout}\n  stderr: {tree_stderr}",
            tree.status,
        ));
    } else if !tree_stdout.contains(r#"struct "epics:nt/NTNDArray:1.0""#) {
        failures.push(format!(
            "[T:NDARR -F tree] missing top-level id `struct \"epics:nt/NTNDArray:1.0\"`.\n  stdout: {tree_stdout}\n  stderr: {tree_stderr}",
        ));
    }

    server.stop();
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(
        failures.is_empty(),
        "{} complex-type interop case(s) failed:\n{}",
        failures.len(),
        failures.join("\n----\n"),
    );
}
