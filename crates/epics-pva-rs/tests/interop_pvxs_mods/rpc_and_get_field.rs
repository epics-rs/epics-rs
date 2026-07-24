//! RPC + GET_FIELD cross-impl interop.
//!
//! Both ops have their own wire commands distinct from GET/PUT/
//! MONITOR. They were previously covered only by Rust↔Rust tests.
//!
//! Tests:
//!
//! - **RPC A**: pvxcall against a Rust SharedPV with an RPC
//!   handler. Asserts the response value matches what the handler
//!   produced.
//! - **GET_FIELD A**: pvxinfo against a Rust server — sends
//!   CMD_GET_FIELD, expects an introspection-only response (no
//!   value). Asserts the descriptor structure is reported.

// RTEMS-EXEC-MODEL-ALLOW(2): not run by the default nextest profile - this file is a module of the `interop_pvxs` binary, which `.config/nextest.toml`'s default-filter excludes.

use super::interop_helpers::{pvxs_command, pvxs_lib_dir, require_pvxs};

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interop_rpc_pvxcall_against_rust_server() {
    let Some(pvxcall) = require_pvxs("pvxcall") else {
        return;
    };

    // RPC handler that echoes the input "x" arg back as
    // `Structure { value: Scalar<Int>(x * 2) }`.
    let pv = SharedPV::new();
    pv.open(
        FieldDesc::Structure {
            struct_id: "rpc_in".into(),
            fields: vec![("x".into(), FieldDesc::Scalar(ScalarType::Int))],
        },
        PvField::Structure(PvStructure {
            struct_id: "rpc_in".into(),
            fields: vec![("x".into(), PvField::Scalar(ScalarValue::Int(0)))],
        }),
    )
    .unwrap();
    pv.on_rpc(|_pv, _desc_in, val_in| {
        // pvxcall sends arguments as
        //   epics:nt/NTURI:1.0 {
        //     string scheme, string authority, string path,
        //     struct query { string x }   // each --key=val as a String
        //   }
        // (call.cpp:104). Drill into `query.x` and parse as int.
        let x = match &val_in {
            PvField::Structure(root) => root
                .fields
                .iter()
                .find_map(|(n, v)| {
                    if n != "query" {
                        return None;
                    }
                    let PvField::Structure(q) = v else {
                        return None;
                    };
                    q.fields.iter().find_map(|(qn, qv)| match qv {
                        PvField::Scalar(ScalarValue::String(s)) if qn == "x" => {
                            s.as_str_lossy().parse().ok()
                        }
                        PvField::Scalar(ScalarValue::Int(i)) if qn == "x" => Some(*i),
                        _ => None,
                    })
                })
                .unwrap_or(0),
            _ => 0,
        };
        let out_desc = FieldDesc::Structure {
            struct_id: "rpc_out".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Int))],
        };
        let out_val = PvField::Structure(PvStructure {
            struct_id: "rpc_out".into(),
            fields: vec![("value".into(), PvField::Scalar(ScalarValue::Int(x * 2)))],
        });
        Ok((out_desc, out_val))
    });

    let src = SharedSource::new();
    src.add("R:RPC", pv);
    let server = PvaServer::isolated(Arc::new(src)).expect("server start");
    let addr = server.tcp_addr();
    let server_str = format!("127.0.0.1:{}", addr.port());

    let pvxcall_p = pvxcall.clone();
    let server_str_c = server_str.clone();
    let lib = pvxs_lib_dir().into_os_string();
    let out = tokio::task::spawn_blocking(move || {
        pvxs_command(&pvxcall_p)
            .arg("-w")
            .arg("3")
            .arg("R:RPC")
            .arg("x=21")
            .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
            .env("EPICS_PVA_NAME_SERVERS", &server_str_c)
            .env(env_key(), lib)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("pvxcall exec")
    })
    .await
    .expect("join pvxcall");

    server.stop();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "pvxcall exit={:?}\nstdout: {stdout}\nstderr: {stderr}",
        out.status,
    );
    // pvxcall prints RPC results in pvxs `tree` form which is
    // `<type> <name> = <value>`, not `<name> <type> = <value>`.
    assert!(
        stdout.contains("int32_t value = 42"),
        "pvxcall did not show expected RPC echo result (21 * 2 = 42).\n{stdout}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interop_get_field_pvxinfo_against_rust_server() {
    let Some(pvxinfo) = require_pvxs("pvxinfo") else {
        return;
    };

    let pv = SharedPV::new();
    pv.open(
        FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
        },
        PvField::Structure(PvStructure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), PvField::Scalar(ScalarValue::Double(7.5)))],
        }),
    )
    .unwrap();
    let src = SharedSource::new();
    src.add("R:INFO:PV", pv);
    let server = PvaServer::isolated(Arc::new(src)).expect("server start");
    let addr = server.tcp_addr();
    let server_str = format!("127.0.0.1:{}", addr.port());

    let pvxinfo_p = pvxinfo.clone();
    let server_str_c = server_str.clone();
    let lib = pvxs_lib_dir().into_os_string();
    let out = tokio::task::spawn_blocking(move || {
        pvxs_command(&pvxinfo_p)
            .arg("-w")
            .arg("3")
            .arg("R:INFO:PV")
            .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
            .env("EPICS_PVA_NAME_SERVERS", &server_str_c)
            .env(env_key(), lib)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("pvxinfo exec")
    })
    .await
    .expect("join pvxinfo");

    server.stop();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "pvxinfo exit={:?}\nstdout: {stdout}\nstderr: {stderr}",
        out.status,
    );
    // pvxinfo prints the introspection in pvxs's textual form;
    // the struct_id + value-field type must appear.
    assert!(
        stdout.contains("epics:nt/NTScalar:1.0"),
        "pvxinfo did not print the struct id.\n{stdout}",
    );
    assert!(
        stdout.contains("double value"),
        "pvxinfo did not print the value field type.\n{stdout}",
    );
}
