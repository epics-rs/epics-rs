//! hrCtl reads autoMode as C's monitored variable, written as the monitor
//! arrives (hrCtl.st:64-66, no efflag).

use epics_base_rs::runtime::task::{self, BlockingBridge};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::types::EpicsValue;
use optics_rs::snl::hr_ctl::{self, HrConfig};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

const UR2D: f64 = 180.0 / std::f64::consts::PI / 1e6;
const HR_DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/db/hrSeq.db");
const MOTORS: &str = r#"
record(motor, "H:p1") { field(HLM, "1500000") field(LLM, "10000") }
record(motor, "H:p2") { field(HLM, "1500000") field(LLM, "10000") }
"#;

async fn f64_of(ioc: &PvDatabase, pv: &str) -> f64 {
    ioc.get_pv(pv).unwrap().to_f64().unwrap()
}

async fn string_of(ioc: &PvDatabase, pv: &str) -> String {
    match ioc.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.to_string(),
        v => format!("{v:?}"),
    }
}

async fn boot() -> Arc<PvDatabase> {
    let macros: HashMap<String, String> =
        [("P", "H:"), ("N", "1"), ("M_PHI1", "p1"), ("M_PHI2", "p2")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
    let ioc = IocBuilder::new()
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .register_record_type("motor", || Box::new(motor_rs::MotorRecord::new()))
        .db_string(MOTORS, &macros)
        .unwrap()
        .db_file(HR_DB, &macros)
        .unwrap()
        .build_served()
        .await
        .unwrap()
        .0;
    let db = PvDatabase::clone(&ioc);
    for name in ["H:p1", "H:p2"] {
        db.get_record(name)
            .unwrap()
            .write()
            .record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<motor_rs::MotorRecord>())
            .unwrap()
            .pos
            .rbv = 200_000.0;
    }
    BlockingBridge::capture().spawn(hr_ctl::run(HrConfig::new("H:", "1", "p1", "p2"), db));
    // The boot phiLimits pass: theta1Hi = HLM in degrees.
    wait_near(&ioc, "H:HR1_Theta1AO.DRVH", 1.5e6 * UR2D).await;
    assert_eq!(
        string_of(&ioc, "H:HR1_SeqMsg1SI").await,
        "Motor Soft Limit Changed"
    );
    settle().await;
    ioc
}

/// Past waitForCmndEnter's 0.1 s, which drops a command put inside it
/// (hrCtl.st:603-621).
async fn settle() {
    task::sleep(Duration::from_millis(500)).await;
}

/// An E put queued with Mode=Auto already sees auto mode and drives phi1,
/// though E is taken before Mode.
#[epics_base_rs::epics_test]
async fn an_e_put_queued_after_auto_mode_moves() {
    let ioc = boot().await;
    let db = PvDatabase::clone(&ioc);
    let e = f64_of(&ioc, "H:HR1_EAO").await;
    let before = f64_of(&ioc, "H:p1.VAL").await;
    // No await between the posts: on this one-thread runtime the sequencer
    // finds both queued.
    for (pv, v) in [
        ("H:HR1_ModeBO", EpicsValue::Enum(1)),
        ("H:HR1_EAO", EpicsValue::Double(e * 1.01)),
    ] {
        let rec = db.get_record(pv).unwrap();
        let mut inst = rec.write();
        inst.record.put_field("VAL", v).unwrap();
        inst.notify_field("VAL", EventMask::VALUE | EventMask::LOG);
    }
    for _ in 0..100 {
        let set = f64_of(&ioc, "H:HR1_phi1SetAO").await;
        if set != before && f64_of(&ioc, "H:p1.VAL").await == set {
            return;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "phi1 not driven: phi1SetAO {} p1.VAL {} (was {before})",
        f64_of(&ioc, "H:HR1_phi1SetAO").await,
        f64_of(&ioc, "H:p1.VAL").await
    );
}

async fn wait_near(ioc: &PvDatabase, pv: &str, want: f64) {
    for _ in 0..100 {
        if (f64_of(ioc, pv).await - want).abs() < 1e-4 {
            return;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    panic!("{pv} = {}, want {want}", f64_of(ioc, pv).await);
}

/// A motor soft-limit change, UseSet=Set and UseSet=Use each run C's
/// phiLimits (hrCtl.st:697-703, :765-783): Set zeroes the drive limits,
/// Use derives them again from the motor HLM/LLM.
#[epics_base_rs::epics_test]
async fn use_set_zeroes_the_drive_limits_and_use_restores_them() {
    let ioc = boot().await;
    let db = PvDatabase::clone(&ioc);
    let put = |pv: &'static str, v: EpicsValue| {
        let db = db.clone();
        async move {
            let (rec, field) = pv.split_once('.').unwrap_or((pv, "VAL"));
            db.put_record_field_from_ca(rec, field, v).await.unwrap();
        }
    };
    // phi1Hi = phi1MotHiLim*uR2D + phi1Off + worldOff*uR2D (hrCtl.st:845).
    let want = 1e6 * UR2D
        + f64_of(&ioc, "H:HR1_phi1OffAO").await
        + f64_of(&ioc, "H:HR1_worldOffAO").await * UR2D;

    put("H:p1.HLM", EpicsValue::Double(1e6)).await;
    wait_near(&ioc, "H:HR1_Theta1AO.DRVH", want).await;
    assert_eq!(
        string_of(&ioc, "H:HR1_SeqMsg1SI").await,
        "Motor Soft Limit Changed"
    );
    assert_ne!(f64_of(&ioc, "H:HR1_EAO.DRVH").await, 0.0);
    settle().await;

    put("H:HR1_UseSetBO", EpicsValue::Enum(1)).await;
    wait_near(&ioc, "H:HR1_Theta1AO.DRVH", 0.0).await;
    wait_near(&ioc, "H:HR1_EAO.DRVH", 0.0).await;
    wait_near(&ioc, "H:HR1_phi1AO.DRVH", 0.0).await;
    settle().await;

    put("H:HR1_UseSetBO", EpicsValue::Enum(0)).await;
    wait_near(&ioc, "H:HR1_Theta1AO.DRVH", want).await;
}

/// C's E takes the sequencer's own posts: UseSet=Set zeroes EHi before
/// ELo, aoRecord clamps E to 0, and eChanged reads that 0 (C kohzuIoc
/// oracle: "Wavelength > 2d spacing of crystal 1.", E back from theta1).
#[epics_base_rs::epics_test]
async fn set_mode_takes_e_as_its_own_limit_put_clamped_it() {
    let ioc = boot().await;
    let db = PvDatabase::clone(&ioc);
    // The oracle's limits, which leave ELo negative.
    for (rec, field, v) in [
        ("H:p1", "HLM", 90.0),
        ("H:p1", "LLM", -10.0),
        ("H:p2", "HLM", 90.0),
        ("H:p2", "LLM", -10.0),
    ] {
        db.put_record_field_from_ca(rec, field, EpicsValue::Double(v))
            .await
            .unwrap();
    }
    wait_near(&ioc, "H:HR1_EAO.DRVL", -229.18127).await;
    settle().await;
    let e = f64_of(&ioc, "H:HR1_EAO").await;
    db.put_record_field_from_ca("H:HR1_UseSetBO", "VAL", EpicsValue::Enum(1))
        .await
        .unwrap();
    for _ in 0..100 {
        if string_of(&ioc, "H:HR1_SeqMsg1SI").await == "Wavelength > 2d spacing of crystal 1." {
            break;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        string_of(&ioc, "H:HR1_SeqMsg1SI").await,
        "Wavelength > 2d spacing of crystal 1."
    );
    wait_near(&ioc, "H:HR1_EAO", e).await;
}
