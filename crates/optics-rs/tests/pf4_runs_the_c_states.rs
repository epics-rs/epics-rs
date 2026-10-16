//! pf4's runner against `pf4common.db`/`pf4bank.db` and four `bo` hardware
//! bits: the `{BP}{Bn}` monitors, `displayBit` as a NoMon output, and the
//! `bankctl` 3→2→1 cascade through its own posts (`pf4.st:263-324`).

use epics_base_rs::runtime::task::{self, BlockingBridge};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::server::records::transform::TransformRecord;
use epics_base_rs::server::scan::ScanOwner;
use epics_base_rs::types::EpicsValue;
use optics_rs::snl::pf4::{self, MAT_AL, MAT_TI, Pf4BankConfig, Pf4Config};
use std::collections::HashMap;
use std::sync::Arc;
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
async fn boot() -> (Arc<PvDatabase>, ScanOwner) {
    boot_with("").await
}

/// [`boot`], with `extra` loaded after the bank.
async fn boot_with(extra: &str) -> (Arc<PvDatabase>, ScanOwner) {
    boot_on(HW, extra).await
}

/// [`boot_with`] on the hardware records `hw`.
async fn boot_on(hw: &str, extra: &str) -> (Arc<PvDatabase>, ScanOwner) {
    let p = macros(&[("P", "PF:"), ("H", "pf:"), ("A", "A"), ("B", "B")]);
    let ioc = IocBuilder::new()
        .register_record_type("transform", || Box::new(TransformRecord::default()))
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .db_string(hw, &macros(&[]))
        .unwrap()
        .db_file(&format!("{DB}pf4common.db"), &p)
        .unwrap()
        .db_file(
            &format!("{DB}pf4bank.db"),
            &macros(&[("P", "PF:"), ("H", "pf:"), ("B", "A")]),
        )
        .unwrap()
        .db_string(extra, &macros(&[]))
        .unwrap()
        .build_served()
        .await
        .unwrap()
        .0;
    let scan = ScanOwner::start(ioc.clone());
    let mut c = Pf4Config::new("PF:", "pf:", "A");
    c.mono = "PF:mono".into();
    c.bit_prefix = "PF:".into();
    c.bits = [1, 2, 3, 4].map(|i| format!("A{i}"));
    BlockingBridge::capture().spawn(pf4::run(c, PvDatabase::clone(&ioc)));
    settle().await;
    put(&ioc, "PF:pf:f1A", EpicsValue::Double(0.2)).await;
    put(&ioc, "PF:pf:f2A", EpicsValue::Double(0.02)).await;
    put(&ioc, "PF:pf:f3A", EpicsValue::Double(0.0)).await;
    put(&ioc, "PF:pf:f4A", EpicsValue::Double(0.0)).await;
    put(&ioc, "PF:pf:Z2A", EpicsValue::Enum(MAT_TI.into())).await;
    put(&ioc, "PF:pf:bankA", EpicsValue::Enum(1)).await;
    settle().await;
    (ioc, scan)
}

async fn settle() {
    task::sleep(Duration::from_millis(1000)).await;
}

async fn put(ioc: &PvDatabase, pv: &str, v: EpicsValue) {
    let (rec, field) = pv.split_once('.').unwrap_or((pv, "VAL"));
    ioc.put_record_field_from_ca(rec, field, v).await.unwrap();
}

async fn f64_of(ioc: &PvDatabase, pv: &str) -> f64 {
    ioc.get_pv(pv).unwrap().to_f64().unwrap()
}

async fn string_of(ioc: &PvDatabase, pv: &str) -> String {
    match ioc.get_pv(pv).unwrap() {
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
#[epics_base_rs::epics_test]
async fn switching_the_bank_on_runs_3_2_1() {
    let (ioc, _scan) = boot().await;
    assert_eq!(f64_of(&ioc, "PF:pf:bankA").await, 1.0);
    assert_eq!(string_of(&ioc, "PF:pf:fPosA.ZRST").await, "1.000e+00");
    // Al 0.2 mm and Ti 0.02 mm both in, C's `%.3e`.
    assert_eq!(string_of(&ioc, "PF:pf:fPosA.FFST").await, "9.263e-02");
    assert_eq!(f64_of(&ioc, "PF:pf:transA").await, 1.0);
}

/// `{BP}{Bn}` is monitored: a hardware bit set from outside shows on
/// `displayBit` and moves `fPos` to the position holding its pattern.
///
/// With the bank on, C runs `filterPos` without end (every label write posts
/// `fPos`), and a bit put landing in its 0.2 s sleep is written back and its
/// flag cleared. `recalcBank`'s `bankctl` 3 runs past that clear, so the bit
/// is set from `bankA`'s forward link there.
#[epics_base_rs::epics_test]
async fn a_hardware_bit_sets_display_bit_and_fpos() {
    let (ioc, _scan) = boot_with(KICK_A2).await;
    let (xmit, bits) = bank();
    let pos = pf4::find_position(&bits, 2);
    for _ in 0..100 {
        if f64_of(&ioc, "PF:pf:fPosA").await == pos as f64 {
            break;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(f64_of(&ioc, "PF:pf:fPosA").await, pos as f64);
    assert_eq!(f64_of(&ioc, "PF:pf:displayBit2A").await, 1.0);
    assert_eq!(f64_of(&ioc, "PF:A2").await, 1.0);
    assert_eq!(f64_of(&ioc, "PF:pf:transA").await, xmit[pos]);
}

/// `PF:A2` 1 whenever `bankA` is written 3.
const KICK_A2: &str = r#"
record(mbbo, "PF:pf:bankA") { field(FLNK, "PF:kick") }
record(calcout, "PF:kick") {
    field(INPA, "PF:pf:bankA NPP")
    field(CALC, "A=3")
    field(OOPT, "When Non-zero")
    field(OUT, "PF:A2 PP")
}
"#;

/// `displayBit` is an unmonitored output (`pf4.st:46`): a put to it is not
/// read back, and the hardware bit stays where it was.
#[epics_base_rs::epics_test]
async fn a_display_bit_put_leaves_the_hardware_bit() {
    let (ioc, _scan) = boot().await;
    put(&ioc, "PF:pf:displayBit1A", EpicsValue::Enum(1)).await;
    settle().await;
    assert_eq!(f64_of(&ioc, "PF:A1").await, 0.0);
    assert_eq!(f64_of(&ioc, "PF:pf:fPosA").await, 0.0);
}

/// `PVPUT` is `pvPut(var, SYNC)` (`seqPVmacros.h:167-171`): `filterPos` puts
/// `b1` and waits for it before `displayBit1` (`pf4.st:226`). With `b1` a
/// `busy` left set, `displayBit1` stays 0 while the put is pending.
#[epics_base_rs::epics_test]
async fn filter_pos_waits_for_a_bit_put_before_its_display_bit() {
    let hw = HW.replace(r#"record(bo, "PF:A1") {}"#, r#"record(busy, "PF:A1") {}"#);
    let (ioc, _scan) = boot_on(&hw, "").await;
    let (_, bits) = bank();
    let pos = pf4::find_position(&bits, 1);
    put(&ioc, "PF:pf:fPosA", EpicsValue::Enum(pos as u16)).await;
    settle().await;
    assert_eq!(f64_of(&ioc, "PF:A1").await, 1.0);
    assert_eq!(f64_of(&ioc, "PF:pf:displayBit1A").await, 0.0);
}
