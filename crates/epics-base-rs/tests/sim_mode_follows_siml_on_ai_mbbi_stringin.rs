//! SIMM resolved from a SIML DB link, a SIML put and a SIMM put, on ai, mbbi
//! and stringin: the expected tuples are softIoc 7.0.10's (VAL, SIMM, STAT,
//! SEVR, PACT after each step), including RAW on the YesNo stringin (SOFT /
//! INVALID, no substitution), SIMM kept across an emptied SIML, and the SDLY
//! window.

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "SW") { field(VAL, "0") }
record(ao, "SW2") { field(VAL, "0") }
record(ao, "SRCD") { field(VAL, "42.5") }
record(longin, "SRCL") { field(VAL, "3") }
record(stringin, "SRCS") { field(VAL, "simstr") }
record(ai, "AI") { field(SIML, "SW") field(SIOL, "SRCD") field(SIMS, "MINOR") field(INP, "0") }
record(mbbi, "MB") { field(SIML, "SW") field(SIOL, "SRCL") field(SIMS, "MAJOR") field(ZRVL,"0") field(ONVL,"1") field(TWVL,"2") field(THVL,"3") field(ZRST,"z") field(ONST,"o") field(TWST,"t") field(THST,"th") }
record(stringin, "SI") { field(SIML, "SW") field(SIOL, "SRCS") field(SIMS, "MINOR") field(VAL, "init") }
"#;

use epics_base_rs::server::recgbl::alarm_status::{SIMM_ALARM as SIMM, SOFT_ALARM as SOFT};

const NO_ALARM: u16 = 0;

async fn put(db: &PvDatabase, pv: &str, v: EpicsValue) {
    let (r, f) = pv.split_once('.').unwrap_or((pv, "VAL"));
    db.put_record_field_from_ca(r, f, v).await.unwrap();
}

async fn proc(db: &PvDatabase, rec: &str) {
    let mut v = ProcStack::new();
    db.process_record_with_links(rec, &mut v).await.unwrap();
}

/// `(VAL, SIMM, STAT, SEVR, PACT)` as text.
async fn state(db: &PvDatabase, rec: &str) -> String {
    let val = match db.get_pv(&format!("{rec}.VAL")).unwrap() {
        EpicsValue::Double(d) => d.to_string(),
        EpicsValue::Enum(e) => e.to_string(),
        other => other.to_string(),
    };
    let inst = db.get_record(rec).unwrap();
    let inst = inst.read();
    let simm = inst.record.simm_value().unwrap();
    format!(
        "{val}|{simm}|{}|{}|{}",
        inst.common.stat,
        inst.common.sevr as u16,
        u8::from(inst.is_processing())
    )
}

async fn run(db: &PvDatabase, r: &str, want: &[(&str, &str)]) {
    let mut got = Vec::new();
    let mut check = |step: &str, s: String| got.push((step.to_string(), s));
    for v in [0.0, 1.0, 2.0, 0.0] {
        put(db, "SW", EpicsValue::Double(v)).await;
        proc(db, r).await;
        check(&format!("sw={v}"), state(db, r).await);
    }
    put(db, "SW2", EpicsValue::Double(1.0)).await;
    put(db, &format!("{r}.SIML"), EpicsValue::String("SW2".into())).await;
    put(db, "SW", EpicsValue::Double(0.0)).await;
    proc(db, r).await;
    check("siml->SW2", state(db, r).await);
    put(db, &format!("{r}.SIML"), EpicsValue::String("".into())).await;
    proc(db, r).await;
    check("siml->empty", state(db, r).await);
    put(db, "SW", EpicsValue::Double(2.0)).await;
    put(db, &format!("{r}.SIML"), EpicsValue::String("SW".into())).await;
    proc(db, r).await;
    check("siml->SW(2)", state(db, r).await);
    put(db, &format!("{r}.SIML"), EpicsValue::String("SW2".into())).await;
    put(db, &format!("{r}.SIMM"), EpicsValue::Short(0)).await;
    put(db, &format!("{r}.SIML"), EpicsValue::String("".into())).await;
    proc(db, r).await;
    check("simm=0", state(db, r).await);
    put(db, &format!("{r}.SIMM"), EpicsValue::Short(1)).await;
    proc(db, r).await;
    check("simm=1", state(db, r).await);
    put(db, &format!("{r}.SIMM"), EpicsValue::Short(0)).await;
    put(db, &format!("{r}.SIML"), EpicsValue::String("SW".into())).await;
    put(db, "SW", EpicsValue::Double(1.0)).await;
    put(db, &format!("{r}.SDLY"), EpicsValue::Double(0.4)).await;
    proc(db, r).await;
    check("sdly fresh", state(db, r).await);
    tokio::time::sleep(std::time::Duration::from_millis(900)).await;
    check("sdly done", state(db, r).await);
    let want: Vec<(String, String)> = want
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
    assert_eq!(got, want, "{r}");
}

async fn db() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

#[epics_macros_rs::epics_test]
async fn ai_follows_siml() {
    let no = format!("|0|{NO_ALARM}|0|0");
    run(
        &*db().await,
        "AI",
        &[
            ("sw=0", &format!("0{no}")),
            ("sw=1", &format!("42.5|1|{SIMM}|1|0")),
            ("sw=2", &format!("42|2|{SIMM}|1|0")),
            ("sw=0", &format!("42{no}")),
            ("siml->SW2", &format!("42.5|1|{SIMM}|1|0")),
            ("siml->empty", &format!("42.5|1|{SIMM}|1|0")),
            ("siml->SW(2)", &format!("42|2|{SIMM}|1|0")),
            ("simm=0", &format!("42{no}")),
            ("simm=1", &format!("42.5|1|{SIMM}|1|0")),
            ("sdly fresh", &format!("42.5|1|{SIMM}|1|1")),
            ("sdly done", &format!("42.5|1|{SIMM}|1|0")),
        ],
    )
    .await;
}

#[epics_macros_rs::epics_test]
async fn mbbi_follows_siml() {
    let no = format!("|0|{NO_ALARM}|0|0");
    run(
        &*db().await,
        "MB",
        &[
            ("sw=0", &format!("0{no}")),
            ("sw=1", &format!("3|1|{SIMM}|2|0")),
            ("sw=2", &format!("3|2|{SIMM}|2|0")),
            ("sw=0", &format!("3{no}")),
            ("siml->SW2", &format!("3|1|{SIMM}|2|0")),
            ("siml->empty", &format!("3|1|{SIMM}|2|0")),
            ("siml->SW(2)", &format!("3|2|{SIMM}|2|0")),
            ("simm=0", &format!("3{no}")),
            ("simm=1", &format!("3|1|{SIMM}|2|0")),
            ("sdly fresh", &format!("3|1|{SIMM}|2|1")),
            ("sdly done", &format!("3|1|{SIMM}|2|0")),
        ],
    )
    .await;
}

#[epics_macros_rs::epics_test]
async fn stringin_follows_siml() {
    let no = format!("|0|{NO_ALARM}|0|0");
    let raw = format!("simstr|2|{SOFT}|3|0");
    run(
        &*db().await,
        "SI",
        &[
            ("sw=0", &format!("init{no}")),
            ("sw=1", &format!("simstr|1|{SIMM}|1|0")),
            ("sw=2", &raw),
            ("sw=0", &format!("simstr{no}")),
            ("siml->SW2", &format!("simstr|1|{SIMM}|1|0")),
            ("siml->empty", &format!("simstr|1|{SIMM}|1|0")),
            ("siml->SW(2)", &raw),
            ("simm=0", &format!("simstr{no}")),
            ("simm=1", &format!("simstr|1|{SIMM}|1|0")),
            ("sdly fresh", &format!("simstr|1|{SIMM}|1|1")),
            ("sdly done", &format!("simstr|1|{SIMM}|1|0")),
        ],
    )
    .await;
}
