#![cfg(tokio_backend)]
//! pf4's runner against `pf4common.db`/`pf4bank.db` and four `bo` hardware
//! bits: the `{BP}{Bn}` monitors, `displayBit` as a NoMon output, and the
//! `bankctl` 3→2→1 cascade through its own posts (`pf4.st:263-324`).

use epics_base_rs::server::records::transform::TransformRecord;
use epics_base_rs::server::scan::ScanOwner;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::{CaServer, CaServerBuilder};
use optics_rs::snl::pf4::{self, MAT_AL, MAT_TI, Pf4BankConfig, Pf4Config};
use std::collections::HashMap;
use std::time::Duration;

const DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/db/");
const HW: &str = r#"
record(bo, "PF:A1") {}
record(bo, "PF:A2") {}
record(bo, "PF:A3") {}
record(bo, "PF:A4") {}
record(ao, "PF:mono") { field(VAL, "10") }
"#;

fn macros(kv: &[(&str, &str)]) -> HashMap<String, String> {
    kv.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Bank A with Al 0.2 mm and Ti 0.02 mm, the other two empty, switched on.
async fn boot() -> (CaServer, ScanOwner) {
    let p = macros(&[("P", "PF:"), ("H", "pf:"), ("A", "A"), ("B", "B")]);
    let server = CaServerBuilder::new()
        .port(0)
        .register_record_type("transform", || Box::new(TransformRecord::default()))
        .db_string(HW, &macros(&[]))
        .unwrap()
        .db_file(&format!("{DB}pf4common.db"), &p)
        .unwrap()
        .db_file(
            &format!("{DB}pf4bank.db"),
            &macros(&[("P", "PF:"), ("H", "pf:"), ("B", "A")]),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let scan = ScanOwner::start(server.database().clone());
    let mut c = Pf4Config::new("PF:", "pf:", "A");
    c.mono = "PF:mono".into();
    c.bit_prefix = "PF:".into();
    c.bits = [1, 2, 3, 4].map(|i| format!("A{i}"));
    tokio::spawn(pf4::run(c, (**server.database()).clone()));
    settle().await;
    put(&server, "PF:pf:f1A", EpicsValue::Double(0.2)).await;
    put(&server, "PF:pf:f2A", EpicsValue::Double(0.02)).await;
    put(&server, "PF:pf:f3A", EpicsValue::Double(0.0)).await;
    put(&server, "PF:pf:f4A", EpicsValue::Double(0.0)).await;
    put(&server, "PF:pf:Z2A", EpicsValue::Enum(MAT_TI.into())).await;
    put(&server, "PF:pf:bankA", EpicsValue::Enum(1)).await;
    settle().await;
    (server, scan)
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(1000)).await;
}

async fn put(server: &CaServer, pv: &str, v: EpicsValue) {
    let (rec, field) = pv.split_once('.').unwrap_or((pv, "VAL"));
    server
        .database()
        .put_record_field_from_ca(rec, field, v)
        .await
        .unwrap();
}

async fn f64_of(server: &CaServer, pv: &str) -> f64 {
    server.get(pv).await.unwrap().to_f64().unwrap()
}

async fn string_of(server: &CaServer, pv: &str) -> String {
    match server.get(pv).await.unwrap() {
        EpicsValue::String(s) => s.to_string(),
        v => format!("{v:?}"),
    }
}

fn bank() -> ([f64; 16], [u8; 16]) {
    let cfg = Pf4BankConfig {
        thicknesses: [0.2, 0.02, 0.0, 0.0],
        material_indices: [MAT_AL, MAT_TI, MAT_AL, MAT_AL],
        ..Default::default()
    };
    pf4::recalc_filters(10.0, &cfg, true, 0)
}

/// Switching the bank on goes `bankOff` → `recalcBank` → 3 (labels) → 2
/// (`trans`) → 1, each step taken from the `bankctl` post of the one before.
#[tokio::test(flavor = "multi_thread")]
async fn switching_the_bank_on_runs_3_2_1() {
    let (server, _scan) = boot().await;
    assert_eq!(f64_of(&server, "PF:pf:bankA").await, 1.0);
    assert_eq!(string_of(&server, "PF:pf:fPosA.ZRST").await, "1.000e+00");
    // Al 0.2 mm and Ti 0.02 mm both in, C's `%.3e`.
    assert_eq!(string_of(&server, "PF:pf:fPosA.FFST").await, "9.263e-02");
    assert_eq!(f64_of(&server, "PF:pf:transA").await, 1.0);
}

/// `{BP}{Bn}` is monitored: a hardware bit set from outside shows on
/// `displayBit` and moves `fPos` to the position holding its pattern.
///
/// With the bank on, C runs `filterPos` without end (every label write posts
/// `fPos`), and a bit put landing in its 0.2 s sleep is written back and its
/// flag cleared, so the put is repeated until one lands outside it.
#[tokio::test(flavor = "multi_thread")]
async fn a_hardware_bit_sets_display_bit_and_fpos() {
    let (server, _scan) = boot().await;
    let (xmit, bits) = bank();
    let pos = pf4::find_position(&bits, 2);
    for _ in 0..60 {
        put(&server, "PF:A2", EpicsValue::Enum(1)).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        if f64_of(&server, "PF:pf:fPosA").await == pos as f64 {
            break;
        }
    }
    assert_eq!(f64_of(&server, "PF:pf:fPosA").await, pos as f64);
    assert_eq!(f64_of(&server, "PF:pf:displayBit2A").await, 1.0);
    assert_eq!(f64_of(&server, "PF:A2").await, 1.0);
    assert_eq!(f64_of(&server, "PF:pf:transA").await, xmit[pos]);
}

/// `displayBit` is an unmonitored output (`pf4.st:46`): a put to it is not
/// read back, and the hardware bit stays where it was.
#[tokio::test(flavor = "multi_thread")]
async fn a_display_bit_put_leaves_the_hardware_bit() {
    let (server, _scan) = boot().await;
    put(&server, "PF:pf:displayBit1A", EpicsValue::Enum(1)).await;
    settle().await;
    assert_eq!(f64_of(&server, "PF:A1").await, 0.0);
    assert_eq!(f64_of(&server, "PF:pf:fPosA").await, 0.0);
}
