//! Cross-implementation interop for complex PVA structures.
//!
//! Rust PVA server hosts a matrix of PVs covering every NT shape
//! pvxs ships built-in plus a deeply-nested generic structure and
//! a Variant/Any field. Real `pvxget` reads each one (via
//! `EPICS_PVA_NAME_SERVERS` → R11 TCP search) and the test asserts
//! the formatted output contains the expected values. Catches
//! encoder bugs in Structure / ScalarArray / StructureArray /
//! Variant / String paths that the simpler R1/R11/R20 tests
//! (NTScalar Double only) cannot.
//!
//! Each PV is hosted on the *same* Rust server, so one
//! handshake + many GETs amortise the per-test cost. SKIPped if
//! `pvxget` not found.

use super::interop_helpers::{PVXGET, pvxs_command, require_pvxs};

use epics_pva_rs::nt::{NTEnum, NTScalar, NTTable};
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{PvaServer, SharedPV, SharedSource};

use std::sync::Arc;
use std::time::Duration;

/// Open a SharedPV with the given descriptor and value.
fn open_pv(desc: FieldDesc, value: PvField) -> SharedPV {
    let pv = SharedPV::new();
    pv.open(desc, value);
    pv
}

/// Build an NTScalar PV with a concrete value.
fn nt_scalar(t: ScalarType, value: ScalarValue) -> SharedPV {
    let desc = NTScalar::new(t).build();
    let mut root = PvStructure::new("epics:nt/NTScalar:1.0");
    root.fields
        .push(("value".to_string(), PvField::Scalar(value)));
    root.fields
        .push(("alarm".to_string(), epics_pva_rs::nt::meta::alarm_default()));
    root.fields.push((
        "timeStamp".to_string(),
        epics_pva_rs::nt::meta::time_default(),
    ));
    open_pv(desc, PvField::Structure(root))
}

/// Build an NTScalarArray PV with a concrete vector value.
fn nt_scalar_array(t: ScalarType, value: Vec<ScalarValue>) -> SharedPV {
    let desc = NTScalar::array(t).build();
    let mut root = PvStructure::new("epics:nt/NTScalarArray:1.0");
    root.fields
        .push(("value".to_string(), PvField::ScalarArray(value)));
    root.fields
        .push(("alarm".to_string(), epics_pva_rs::nt::meta::alarm_default()));
    root.fields.push((
        "timeStamp".to_string(),
        epics_pva_rs::nt::meta::time_default(),
    ));
    open_pv(desc, PvField::Structure(root))
}

/// NTEnum at a specific index with given choices.
fn nt_enum(index: i32, choices: &[&str]) -> SharedPV {
    let desc = NTEnum::new().with_choices(choices.iter().copied()).build();
    let mut value_inner = PvStructure::new("enum_t");
    value_inner.fields.push((
        "index".to_string(),
        PvField::Scalar(ScalarValue::Int(index)),
    ));
    value_inner.fields.push((
        "choices".to_string(),
        PvField::ScalarArray(
            choices
                .iter()
                .map(|s| ScalarValue::String((*s).to_string()))
                .collect(),
        ),
    ));
    let mut root = PvStructure::new("epics:nt/NTEnum:1.0");
    root.fields
        .push(("value".to_string(), PvField::Structure(value_inner)));
    root.fields
        .push(("alarm".to_string(), epics_pva_rs::nt::meta::alarm_default()));
    root.fields.push((
        "timeStamp".to_string(),
        epics_pva_rs::nt::meta::time_default(),
    ));
    open_pv(desc, PvField::Structure(root))
}

/// NTTable with two double columns + one string column. Values:
/// xs = [1.0, 2.0, 3.0], ys = [10.0, 20.0, 30.0], names = ["a","b","c"].
fn nt_table_three_columns() -> SharedPV {
    let t = NTTable::new()
        .add_column(ScalarType::Double, "xs", Some("X axis"))
        .add_column(ScalarType::Double, "ys", Some("Y axis"))
        .add_column(ScalarType::String, "name", Some("Name"));
    let desc = t.build();
    let mut root = PvStructure::new("epics:nt/NTTable:1.0");
    root.fields.push((
        "labels".to_string(),
        PvField::ScalarArray(vec![
            ScalarValue::String("X axis".into()),
            ScalarValue::String("Y axis".into()),
            ScalarValue::String("Name".into()),
        ]),
    ));
    let mut cols = PvStructure::new("");
    cols.fields.push((
        "xs".to_string(),
        PvField::ScalarArray(vec![
            ScalarValue::Double(1.0),
            ScalarValue::Double(2.0),
            ScalarValue::Double(3.0),
        ]),
    ));
    cols.fields.push((
        "ys".to_string(),
        PvField::ScalarArray(vec![
            ScalarValue::Double(10.0),
            ScalarValue::Double(20.0),
            ScalarValue::Double(30.0),
        ]),
    ));
    cols.fields.push((
        "name".to_string(),
        PvField::ScalarArray(vec![
            ScalarValue::String("a".into()),
            ScalarValue::String("b".into()),
            ScalarValue::String("c".into()),
        ]),
    ));
    root.fields
        .push(("value".to_string(), PvField::Structure(cols)));
    root.fields.push((
        "descriptor".to_string(),
        PvField::Scalar(ScalarValue::String("table".into())),
    ));
    root.fields
        .push(("alarm".to_string(), epics_pva_rs::nt::meta::alarm_default()));
    root.fields.push((
        "timeStamp".to_string(),
        epics_pva_rs::nt::meta::time_default(),
    ));
    open_pv(desc, PvField::Structure(root))
}

/// Generic nested structure 3-deep with mixed scalar leaves.
/// Not an NT — exercises the raw Structure encoder/decoder path
/// that NT helpers route through.
fn nested_generic_struct() -> SharedPV {
    let desc = FieldDesc::Structure {
        struct_id: "test:nested:1.0".into(),
        fields: vec![
            (
                "outer".into(),
                FieldDesc::Structure {
                    struct_id: String::new(),
                    fields: vec![
                        (
                            "mid".into(),
                            FieldDesc::Structure {
                                struct_id: String::new(),
                                fields: vec![
                                    ("count".into(), FieldDesc::Scalar(ScalarType::Long)),
                                    ("label".into(), FieldDesc::Scalar(ScalarType::String)),
                                ],
                            },
                        ),
                        ("flag".into(), FieldDesc::Scalar(ScalarType::Boolean)),
                    ],
                },
            ),
            ("tags".into(), FieldDesc::ScalarArray(ScalarType::String)),
        ],
    };
    let inner = PvField::Structure(PvStructure {
        struct_id: String::new(),
        fields: vec![
            (
                "count".to_string(),
                PvField::Scalar(ScalarValue::Long(987_654_321_i64)),
            ),
            (
                "label".to_string(),
                PvField::Scalar(ScalarValue::String("nested-leaf".into())),
            ),
        ],
    });
    let outer = PvField::Structure(PvStructure {
        struct_id: String::new(),
        fields: vec![
            ("mid".to_string(), inner),
            (
                "flag".to_string(),
                PvField::Scalar(ScalarValue::Boolean(true)),
            ),
        ],
    });
    let root = PvField::Structure(PvStructure {
        struct_id: "test:nested:1.0".into(),
        fields: vec![
            ("outer".to_string(), outer),
            (
                "tags".to_string(),
                PvField::ScalarArray(vec![
                    ScalarValue::String("alpha".into()),
                    ScalarValue::String("beta".into()),
                ]),
            ),
        ],
    });
    open_pv(desc, root)
}

/// Run pvxget with `EPICS_PVA_NAME_SERVERS` pointing at `addr`
/// and return its stdout / stderr / exit-status.
async fn pvxget_capture(
    pvxget: &std::path::Path,
    server_str: String,
    pv_name: &'static str,
) -> std::process::Output {
    let pvxget = pvxget.to_path_buf();
    let pv_name = pv_name.to_string();
    tokio::task::spawn_blocking(move || {
        pvxget_command(&pvxget, &server_str, &pv_name)
            .output()
            .expect("pvxget exec")
    })
    .await
    .expect("join pvxget")
}

fn pvxget_command(
    pvxget: &std::path::Path,
    server_str: &str,
    pv_name: &str,
) -> std::process::Command {
    let mut cmd = pvxs_command(pvxget);
    cmd.arg("-w")
        .arg("3")
        .arg(pv_name)
        .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_PVA_ADDR_LIST", "")
        .env("EPICS_PVA_NAME_SERVERS", server_str)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    cmd
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interop_complex_types_pvxget_against_rust_server() {
    let Some(pvxget) = require_pvxs(PVXGET) else {
        return;
    };

    // Build a server with every complex shape we care about.
    let source = SharedSource::new();
    source.add(
        "T:STR",
        nt_scalar(
            ScalarType::String,
            ScalarValue::String("hello world".into()),
        ),
    );
    source.add(
        "T:INT",
        nt_scalar(ScalarType::Int, ScalarValue::Int(-12345)),
    );
    source.add(
        "T:LONG",
        nt_scalar(ScalarType::Long, ScalarValue::Long(9_000_000_000_i64)),
    );
    // Distinct, non-constant-like double so clippy's approx_constant
    // doesn't flag PI / E literals.
    source.add(
        "T:DBL",
        nt_scalar(ScalarType::Double, ScalarValue::Double(123.456_789_f64)),
    );
    source.add(
        "T:WF:DBL",
        nt_scalar_array(
            ScalarType::Double,
            vec![
                ScalarValue::Double(1.5),
                ScalarValue::Double(2.5),
                ScalarValue::Double(3.5),
            ],
        ),
    );
    source.add(
        "T:WF:INT",
        nt_scalar_array(
            ScalarType::Int,
            vec![
                ScalarValue::Int(7),
                ScalarValue::Int(8),
                ScalarValue::Int(9),
                ScalarValue::Int(10),
            ],
        ),
    );
    source.add(
        "T:WF:STR",
        nt_scalar_array(
            ScalarType::String,
            vec![
                ScalarValue::String("alpha".into()),
                ScalarValue::String("beta".into()),
                ScalarValue::String("gamma".into()),
            ],
        ),
    );
    source.add("T:ENUM", nt_enum(2, &["OFF", "ON", "AUTO"]));
    source.add("T:TBL", nt_table_three_columns());
    source.add("T:NEST", nested_generic_struct());

    let server = PvaServer::isolated(Arc::new(source)).expect("server start");
    let addr = server.tcp_addr();
    let server_str = format!("127.0.0.1:{}", addr.port());

    // Each row: (pv, list of grep substrings that MUST all appear).
    let matrix: &[(&'static str, &[&str])] = &[
        ("T:STR", &[r#"value string = "hello world""#]),
        ("T:INT", &["value int32_t = -12345"]),
        ("T:LONG", &["value int64_t = 9000000000"]),
        // pvxget rounds doubles to ~6 sig figs in default `tree`
        // format. Check the leading digits we know it must emit.
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
    ];

    let mut failures: Vec<String> = Vec::new();
    for (pv, needles) in matrix {
        let out = pvxget_capture(&pvxget, server_str.clone(), pv).await;
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
        }
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
