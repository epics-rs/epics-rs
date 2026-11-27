//! A runtime put to SIML/SIOL is honoured by the very next process cycle: each
//! case fails if a parse of the OLD link text were reused after the put.

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::{AlarmSeverity, FieldDesc, ProcessOutcome, Record};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::{DbFieldType, DbfCode, EpicsValue};

const MINOR: i16 = 1;

async fn process(db: &PvDatabase, name: &str) {
    let mut v = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(name, &mut v).await.unwrap();
}

async fn alarm_of(db: &PvDatabase, name: &str) -> (u16, AlarmSeverity) {
    let rec = db.get_record(name).expect("record exists");
    let inst = rec.read();
    (inst.common.stat, inst.common.sevr)
}

/// `caput REC.FIELD text` — a link field changes only through `dbPutField`.
async fn put_link(db: &PvDatabase, pv: &str, text: &str) {
    let (rec, field) = pv.split_once('.').unwrap();
    db.put_record_field_from_ca(rec, field, EpicsValue::String(text.into()))
        .await
        .unwrap();
}

/// A simulated ai: SIML names a record whose value is nonzero, so SIMM=YES and
/// the cycle reads SIOL into VAL, raising SIMM_ALARM at SIMS=MINOR.
async fn add_simulated_ai(db: &PvDatabase, name: &str, siml: &str, siol: &str) {
    let mut ai = AiRecord::new(0.0);
    ai.siml = siml.into();
    ai.siol = siol.into();
    ai.sims = MINOR;
    db.add_record(name, Box::new(ai)).await.unwrap();
}

/// SRC_A (11) and SRC_B (22), plus the SIML switch SIM_ON (1).
async fn add_sources(db: &PvDatabase) {
    db.add_record("SIM_ON", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("SRC_A", Box::new(AoRecord::new(11.0)))
        .await
        .unwrap();
    db.add_record("SRC_B", Box::new(AoRecord::new(22.0)))
        .await
        .unwrap();
}

/// Boundary (a) — the SIOL text the record was loaded with.
#[epics_macros_rs::epics_test]
async fn siol_load_time_text_is_read() {
    let db = PvDatabase::new();
    add_sources(&db).await;
    add_simulated_ai(&db, "AI_SIM", "SIM_ON", "SRC_A").await;

    process(&db, "AI_SIM").await;

    assert_eq!(db.get_pv("AI_SIM").unwrap(), EpicsValue::Double(11.0));
    assert_eq!(
        alarm_of(&db, "AI_SIM").await,
        (alarm_status::SIMM_ALARM, AlarmSeverity::Minor)
    );
}

/// Boundary (b) — a put that CHANGES the SIOL text: the next cycle reads the
/// new target.
#[epics_macros_rs::epics_test]
async fn siol_put_of_new_text_is_read_next_cycle() {
    let db = PvDatabase::new();
    add_sources(&db).await;
    add_simulated_ai(&db, "AI_SIM", "SIM_ON", "SRC_A").await;
    process(&db, "AI_SIM").await;
    assert_eq!(db.get_pv("AI_SIM").unwrap(), EpicsValue::Double(11.0));

    put_link(&db, "AI_SIM.SIOL", "SRC_B").await;
    process(&db, "AI_SIM").await;

    assert_eq!(
        db.get_pv("AI_SIM").unwrap(),
        EpicsValue::Double(22.0),
        "the cycle after `caput AI_SIM.SIOL SRC_B` reads SRC_B, not SRC_A"
    );
}

/// Boundary (c) — a put of the SAME SIOL text the record already holds changes
/// nothing: same value, same SIMM_ALARM.
#[epics_macros_rs::epics_test]
async fn siol_put_of_identical_text_changes_nothing() {
    let db = PvDatabase::new();
    add_sources(&db).await;
    add_simulated_ai(&db, "AI_SIM", "SIM_ON", "SRC_A").await;
    process(&db, "AI_SIM").await;
    put_link(&db, "AI_SIM.SIOL", "SRC_B").await;
    process(&db, "AI_SIM").await;
    assert_eq!(db.get_pv("AI_SIM").unwrap(), EpicsValue::Double(22.0));
    let alarm_before = alarm_of(&db, "AI_SIM").await;
    assert_eq!(
        alarm_before,
        (alarm_status::SIMM_ALARM, AlarmSeverity::Minor)
    );

    put_link(&db, "AI_SIM.SIOL", "SRC_B").await;
    process(&db, "AI_SIM").await;

    assert_eq!(db.get_pv("AI_SIM").unwrap(), EpicsValue::Double(22.0));
    assert_eq!(alarm_of(&db, "AI_SIM").await, alarm_before);
}

/// A simulated input type that models no SIML/SIOL storage of its own — the
/// shape of `mca` (`mca-rs/src/record/mod.rs:896-902`) — so both link texts
/// live in the instance's declared-override store. Its simulation substitutes
/// the input stage: SIOL is read into SVAL and landed in VAL.
struct OverrideSimRecord {
    val: f64,
    sval: f64,
    simm: i16,
}

const OVERRIDE_SIM_TYPE: &str = "sim_link_override_test";

impl Record for OverrideSimRecord {
    fn record_type(&self) -> &'static str {
        OVERRIDE_SIM_TYPE
    }
    fn simulation_substitutes_input_stage(&self) -> bool {
        true
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            "SVAL" => Some(EpicsValue::Double(self.sval)),
            "SIMM" => Some(EpicsValue::Short(self.simm)),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => self.val = value.to_f64().unwrap_or(self.val),
            "SVAL" => self.sval = value.to_f64().unwrap_or(self.sval),
            "SIMM" => self.simm = value.to_f64().map_or(self.simm, |v| v as i16),
            _ => return Err(epics_base_rs::error::CaError::FieldNotFound(name.into())),
        }
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        OVERRIDE_SIM_FIELDS
    }
}

/// SIML/SIOL must be declared as `DBF_INLINK`; `FieldDesc::new` derives the
/// DBF from the served type, so the class is written out.
const fn inlink(name: &'static str) -> FieldDesc {
    FieldDesc {
        declared_dbf: DbfCode::Inlink,
        ..FieldDesc::new(name, DbFieldType::String, false)
    }
}

static OVERRIDE_SIM_FIELDS: &[FieldDesc] = &[
    FieldDesc::new("VAL", DbFieldType::Double, false),
    FieldDesc::new("SVAL", DbFieldType::Double, false),
    FieldDesc::new("SIMM", DbFieldType::Short, false),
    inlink("SIML"),
    inlink("SIOL"),
];

/// Boundary (d) — a type whose SIML/SIOL text lives in the declared-override
/// store, not the record: the loaded SIOL is read, then a runtime SIOL put is
/// read on the next cycle.
#[epics_macros_rs::epics_test]
async fn siol_override_store_text_follows_runtime_put() {
    epics_base_rs::server::record::register_declared_fields(OVERRIDE_SIM_TYPE, OVERRIDE_SIM_FIELDS);
    let db = PvDatabase::new();
    add_sources(&db).await;
    db.add_record(
        "OV_SIM",
        Box::new(OverrideSimRecord {
            val: 0.0,
            sval: 0.0,
            simm: 0,
        }),
    )
    .await
    .unwrap();
    db.put_pv_no_process("OV_SIM.SIML", EpicsValue::String("SIM_ON".into()))
        .await
        .unwrap();
    db.put_pv_no_process("OV_SIM.SIOL", EpicsValue::String("SRC_A".into()))
        .await
        .unwrap();

    process(&db, "OV_SIM").await;
    assert_eq!(
        db.get_pv("OV_SIM").unwrap(),
        EpicsValue::Double(11.0),
        "the override-stored SIOL is read"
    );

    put_link(&db, "OV_SIM.SIOL", "SRC_B").await;
    process(&db, "OV_SIM").await;

    assert_eq!(
        db.get_pv("OV_SIM").unwrap(),
        EpicsValue::Double(22.0),
        "the cycle after `caput OV_SIM.SIOL SRC_B` reads SRC_B, not SRC_A"
    );
}

/// Boundary (e) — SIOL in the OUTPUT direction: a simulated ao writes to the
/// new SIOL target after a put, and the old target keeps what it was given.
#[epics_macros_rs::epics_test]
async fn siol_put_redirects_an_output_write() {
    let db = PvDatabase::new();
    db.add_record("SIM_ON", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("TGT_A", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("TGT_B", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    let mut ao = AoRecord::new(5.0);
    ao.siml = "SIM_ON".into();
    ao.siol = "TGT_A".into();
    db.add_record("AO_SIM", Box::new(ao)).await.unwrap();
    db.get_record("AO_SIM").unwrap().write().common.udf = 0;

    process(&db, "AO_SIM").await;
    assert_eq!(db.get_pv("TGT_A").unwrap(), EpicsValue::Double(5.0));
    assert_eq!(db.get_pv("TGT_B").unwrap(), EpicsValue::Double(0.0));

    put_link(&db, "AO_SIM.SIOL", "TGT_B").await;
    db.put_pv("AO_SIM", EpicsValue::Double(7.0)).await.unwrap();
    process(&db, "AO_SIM").await;

    assert_eq!(
        db.get_pv("TGT_B").unwrap(),
        EpicsValue::Double(7.0),
        "the cycle after `caput AO_SIM.SIOL TGT_B` writes TGT_B"
    );
    assert_eq!(
        db.get_pv("TGT_A").unwrap(),
        EpicsValue::Double(5.0),
        "the old SIOL target is no longer written"
    );
}

/// Boundary (e), INPUT direction: the same redirect on a simulated ai reads the
/// new target and leaves both sources untouched.
#[epics_macros_rs::epics_test]
async fn siol_put_redirects_an_input_read() {
    let db = PvDatabase::new();
    add_sources(&db).await;
    add_simulated_ai(&db, "AI_SIM", "SIM_ON", "SRC_A").await;
    process(&db, "AI_SIM").await;
    assert_eq!(db.get_pv("AI_SIM").unwrap(), EpicsValue::Double(11.0));

    put_link(&db, "AI_SIM.SIOL", "SRC_B").await;
    process(&db, "AI_SIM").await;

    assert_eq!(db.get_pv("AI_SIM").unwrap(), EpicsValue::Double(22.0));
    assert_eq!(db.get_pv("SRC_A").unwrap(), EpicsValue::Double(11.0));
    assert_eq!(db.get_pv("SRC_B").unwrap(), EpicsValue::Double(22.0));
}

/// Boundary (f) — a put that changes the SIML text: SIMM is re-read from the
/// new switch, which is 0, so the next cycle is not simulated.
#[epics_macros_rs::epics_test]
async fn siml_put_of_new_text_rereads_simm() {
    let db = PvDatabase::new();
    add_sources(&db).await;
    db.add_record("SW_A", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("SW_B", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    add_simulated_ai(&db, "AI_SIM", "SW_A", "SRC_A").await;
    process(&db, "AI_SIM").await;
    assert_eq!(db.get_pv("AI_SIM").unwrap(), EpicsValue::Double(11.0));
    assert_eq!(db.get_pv("AI_SIM.SIMM").unwrap(), EpicsValue::Short(1));

    put_link(&db, "AI_SIM.SIML", "SW_B").await;
    // A simulated cycle would now read 99 out of SIOL.
    db.put_pv("SRC_A", EpicsValue::Double(99.0)).await.unwrap();
    process(&db, "AI_SIM").await;

    assert_eq!(
        db.get_pv("AI_SIM.SIMM").unwrap(),
        EpicsValue::Short(0),
        "SIMM is re-read from SW_B"
    );
    assert_eq!(
        db.get_pv("AI_SIM").unwrap(),
        EpicsValue::Double(11.0),
        "an unsimulated soft ai with no INP leaves VAL alone"
    );
    assert_ne!(alarm_of(&db, "AI_SIM").await.0, alarm_status::SIMM_ALARM);
}
