//! `scanparmRecord.c` driven through a database, its output links aimed at
//! local stand-ins for the sscan fields `scanParms.db` names.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::{
    ao::AoRecord, bi::BiRecord, longin::LonginRecord, longout::LongoutRecord,
    stringout::StringoutRecord,
};
use epics_base_rs::types::{DbFieldType, EpicsValue};

const TARGETS: &str = r#"
record(bi, "S:busy") {}
record(longin, "S:mpts") { field(VAL, "100") }
record(longout, "S:pre") {}
record(stringout, "S:ppv") {}
record(stringout, "S:rpv") {}
record(longout, "S:sm") {}
record(ao, "S:sp") {}
record(ao, "S:ep") {}
record(longout, "S:np") {}
record(longout, "S:ar") {}
record(ao, "S:aqt") {}
record(longout, "S:sc") {}
record(longout, "S:go") {}
record(longout, "S:load") {}
record(calc, "S:flnk") { field(INPA, "S:flnk") field(CALC, "A+1") }
"#;

/// `scanParms.db`'s wiring, with `TPV`/`DPV`/`AFT` left unlinked so the
/// empty-link skip is on the path.
fn scanparm(imp: &str, iact: &str) -> String {
    format!(
        r#"
record(scanparm, "P") {{
  field(PRE, "4")
  field(OPRE, "S:pre PP")
  field(PPV, "m1")
  field(OPPV, "S:ppv PP")
  field(RPV, "m1.RBV")
  field(ORPV, "S:rpv PP")
  field(TPV, "det")
  field(SM, "TABLE")
  field(OSM, "S:sm PP")
  field(AR, "RELATIVE")
  field(OAR, "S:ar PP")
  field(SP, "-1")
  field(OSP, "S:sp PP")
  field(EP, "1")
  field(OEP, "S:ep PP")
  field(NP, "21")
  field(ONP, "S:np PP")
  field(AQT, "0.5")
  field(OAQT, "S:aqt PP")
  field(SC, "1")
  field(OSC, "S:sc PP")
  field(OGO, "S:go PP")
  field(OLOAD, "S:load PP")
  field(IMP, "{imp}")
  field(IACT, "{iact}")
}}
"#
    )
}

async fn ioc(db_text: &str) -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .register_record_type("scanparm", || {
            Box::new(sscan_rs::records::scanparm::ScanparmRecord::default())
        })
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .register_record_type("bi", || Box::new(BiRecord::default()))
        .register_record_type("longin", || Box::new(LonginRecord::default()))
        .register_record_type("longout", || Box::new(LongoutRecord::default()))
        .register_record_type("stringout", || Box::new(StringoutRecord::default()))
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    db
}

async fn wired() -> Arc<PvDatabase> {
    sscan_rs::register_sscan_dbd();
    ioc(&format!(
        "{TARGETS}{}",
        scanparm("S:mpts NPP", "S:busy NPP")
    ))
    .await
}

fn f(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

fn s(db: &PvDatabase, pv: &str) -> String {
    match db.get_pv(pv).unwrap() {
        EpicsValue::String(v) => v.to_string(),
        other => panic!("{pv}: {other:?}"),
    }
}

async fn put(db: &PvDatabase, field: &str, value: f64) {
    db.put_record_field_from_ca("P", field, EpicsValue::Double(value))
        .await
        .unwrap();
}

/// The parameters LOAD and GO share (`scanparmRecord.c:157-200`).
fn assert_loaded(db: &PvDatabase) {
    assert_eq!(f(db, "S:pre"), 4.0);
    assert_eq!(s(db, "S:ppv"), "m1");
    assert_eq!(s(db, "S:rpv"), "m1.RBV");
    assert_eq!(f(db, "S:sm"), 1.0, "SM goes out as its menu index");
    assert_eq!(f(db, "S:ar"), 1.0);
    assert_eq!(f(db, "S:sp"), -1.0);
    assert_eq!(f(db, "S:ep"), 1.0);
    assert_eq!(f(db, "S:np"), 21.0);
}

#[epics_base_rs::epics_test]
async fn init_record_sets_the_version_and_the_last_step() {
    let db = wired().await;
    assert_eq!(f(&db, "P.VERS"), 3.0);
    assert_eq!(f(&db, "P.LSTP"), 0.1);
}

#[epics_base_rs::epics_test]
async fn init_record_raises_np_to_two() {
    sscan_rs::register_sscan_dbd();
    let db = ioc(r#"record(scanparm, "P") { field(SP, "0") field(EP, "4") field(NP, "1") }"#).await;
    assert_eq!(f(&db, "P.NP"), 2.0);
    assert_eq!(f(&db, "P.LSTP"), 4.0);
}

#[epics_base_rs::epics_test]
async fn load_writes_the_parameters_but_not_the_start() {
    let db = wired().await;
    put(&db, "LOAD", 1.0).await;
    assert_loaded(&db);
    assert_eq!(f(&db, "S:load"), 1.0);
    assert_eq!(f(&db, "P.LOAD"), 0.0);
    // AQT and SC go out on GO alone (:202-214), and GO's own link only
    // when GO is set (:248-254).
    assert_eq!(f(&db, "S:aqt"), 0.0);
    assert_eq!(f(&db, "S:sc"), 0.0);
    assert_eq!(f(&db, "S:go"), 0.0);
}

#[epics_base_rs::epics_test]
async fn go_writes_the_parameters_then_starts_the_scan() {
    let db = wired().await;
    put(&db, "GO", 1.0).await;
    assert_loaded(&db);
    assert_eq!(f(&db, "S:aqt"), 0.5);
    assert_eq!(f(&db, "S:sc"), 1.0);
    assert_eq!(f(&db, "S:go"), 1.0);
    assert_eq!(f(&db, "P.GO"), 0.0);
    assert_eq!(f(&db, "S:load"), 0.0);
}

/// A busy scan takes no parameters, but GO and LOAD still go out through
/// their own links and clear (:154, :248-262).
#[epics_base_rs::epics_test]
async fn a_busy_scan_is_not_loaded() {
    let db = wired().await;
    db.put_pv("S:busy", EpicsValue::Double(1.0)).await.unwrap();
    put(&db, "GO", 1.0).await;
    assert_eq!(f(&db, "P.ACT"), 1.0);
    assert_eq!(f(&db, "S:pre"), 0.0);
    assert_eq!(f(&db, "S:sc"), 0.0);
    assert_eq!(f(&db, "S:go"), 1.0);
    assert_eq!(f(&db, "P.GO"), 0.0);
}

/// NP is held to `[2, MP]` with MP read from IMP each process (:141-152).
#[epics_base_rs::epics_test]
async fn np_is_clamped_to_the_scan_maximum() {
    let db = wired().await;
    db.put_pv("S:mpts", EpicsValue::Double(10.0)).await.unwrap();
    put(&db, "NP", 50.0).await;
    assert_eq!(f(&db, "P.MP"), 10.0);
    assert_eq!(f(&db, "P.NP"), 10.0);
    assert_eq!(f(&db, "P.STEP"), 2.0 / 9.0);

    db.put_pv("S:mpts", EpicsValue::Double(1.0)).await.unwrap();
    put(&db, "NP", 50.0).await;
    assert_eq!(f(&db, "P.NP"), 2.0);
    assert_eq!(f(&db, "P.STEP"), 2.0);
}

/// A constant IMP is MP's only source; it delivers at init, never at
/// process (:124-126).
#[epics_base_rs::epics_test]
async fn a_constant_imp_seeds_mp_once() {
    sscan_rs::register_sscan_dbd();
    let db = ioc(&format!("{TARGETS}{}", scanparm("7", "S:busy NPP"))).await;
    assert_eq!(f(&db, "P.MP"), 7.0);
    put(&db, "NP", 21.0).await;
    assert_eq!(f(&db, "P.NP"), 7.0);
}

/// `scanparmRecord.c:122-124` seeds MP only for `imp.type == CONSTANT`, and a
/// `{const:…}` IMP is a `JSON_LINK` (`dbStaticLib.c:2280-2287`).
#[epics_base_rs::epics_test]
async fn a_json_imp_does_not_seed_mp() {
    sscan_rs::register_sscan_dbd();
    let db = ioc(&format!("{TARGETS}{}", scanparm("{const:7}", "S:busy NPP"))).await;
    assert_eq!(f(&db, "P.MP"), 0.0);
}

/// No IMP at all leaves MP 0, so every process pulls NP down to 2.
#[epics_base_rs::epics_test]
async fn no_imp_holds_np_at_two() {
    sscan_rs::register_sscan_dbd();
    let db = ioc(&format!("{TARGETS}{}", scanparm("", "S:busy NPP"))).await;
    put(&db, "NP", 21.0).await;
    assert_eq!(f(&db, "P.NP"), 2.0);
}

/// A failed IACT read returns its status ahead of everything (:136-137):
/// GO stays set and nothing goes out.
#[epics_base_rs::epics_test]
async fn a_failed_iact_read_does_nothing() {
    sscan_rs::register_sscan_dbd();
    let db = ioc(&format!(
        "{TARGETS}{}",
        scanparm("S:mpts NPP", "NO:SUCH.BUSY CA")
    ))
    .await;
    put(&db, "GO", 1.0).await;
    assert_eq!(f(&db, "P.GO"), 1.0);
    assert_eq!(f(&db, "S:go"), 0.0);
    assert_eq!(f(&db, "S:pre"), 0.0);
}

/// C reads IMP only after IACT read cleanly (`:136-141`), so a failed IACT
/// leaves MP where it was.
#[epics_base_rs::epics_test]
async fn a_failed_iact_read_skips_the_imp_read() {
    sscan_rs::register_sscan_dbd();
    let db = ioc(&format!(
        "{TARGETS}{}",
        scanparm("S:mpts NPP", "NO:SUCH.BUSY CA")
    ))
    .await;
    db.put_pv("S:mpts", EpicsValue::Double(7.0)).await.unwrap();
    put(&db, "NP", 21.0).await;
    assert_eq!(f(&db, "P.MP"), 0.0);
    assert_eq!(f(&db, "P.NP"), 21.0, "nor is NP clamped");
}

/// Appends `<FIELD> <value>` for each event it passes.
struct Line {
    field: &'static str,
    stream: Arc<Mutex<Vec<String>>>,
}

impl SubscriptionFilter for Line {
    fn name(&self) -> &'static str {
        "line"
    }

    fn apply(&self, event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
        let v = event.event.value().as_value().and_then(|v| v.to_f64());
        self.stream
            .lock()
            .unwrap()
            .push(format!("{} {}", self.field, v.unwrap()));
        Some(event)
    }
}

/// C posts NP when it clamps, GO and LOAD as they clear, and STEP only
/// when it moved; VAL never (:145-152, :248-262, :274-283).
#[epics_base_rs::epics_test]
async fn a_cycle_posts_np_go_load_and_a_moved_step() {
    let db = wired().await;
    let stream: Arc<Mutex<Vec<String>>> = Arc::default();
    let mut readers = Vec::new();
    {
        let cell = db.get_record("P").unwrap();
        let mut inst = cell.write();
        for (sid, (field, ty)) in [
            ("VAL", DbFieldType::Double),
            ("NP", DbFieldType::Long),
            ("GO", DbFieldType::Short),
            ("LOAD", DbFieldType::Short),
            ("STEP", DbFieldType::Double),
        ]
        .into_iter()
        .enumerate()
        {
            readers.push(
                inst.add_subscriber(field, sid as u32, ty, EventMask::VALUE.bits())
                    .unwrap(),
            );
            assert!(inst.attach_filter_to_last_subscriber(
                field,
                Arc::new(Line {
                    field,
                    stream: stream.clone(),
                }),
            ));
        }
    }
    let take = || std::mem::take(&mut *stream.lock().unwrap());

    // init_record left LSTP at the step this process derives.
    db.process_record("P").await.unwrap();
    assert_eq!(take(), Vec::<String>::new());

    db.put_pv("S:mpts", EpicsValue::Double(11.0)).await.unwrap();
    db.process_record("P").await.unwrap();
    assert_eq!(take(), ["NP 11", "STEP 0.2"]);

    put(&db, "LOAD", 1.0).await;
    assert_eq!(take(), ["LOAD 1", "LOAD 0"]);

    put(&db, "GO", 1.0).await;
    assert_eq!(take(), ["GO 1", "GO 0"]);
}

/// `wired()` with `link` a CA link to no PV, whose `dbPutLink` fails, and
/// FLNK set. A later `field()` overrides an earlier one, as `dbLoadRecords`
/// does. The oracle lines are oracleSscanIoc on the same database.
async fn failing(link: &str) -> Arc<PvDatabase> {
    sscan_rs::register_sscan_dbd();
    let base = scanparm("S:mpts NPP", "S:busy NPP");
    let text = base.trim_end().strip_suffix('}').unwrap();
    ioc(&format!(
        "{TARGETS}{text}  field({link}, \"NO:SUCH CA\")\n  field(FLNK, \"S:flnk\")\n}}\n"
    ))
    .await
}

async fn try_put(db: &PvDatabase, field: &str) -> bool {
    db.put_record_field_from_ca_no_notify("P", field, EpicsValue::Double(1.0))
        .await
        .is_ok()
}

/// The LOAD group's last put fails: `process()` returns its status (:206)
/// ahead of `pact = TRUE`, so neither GO's link nor its clear runs, no
/// forward link fires, and the next put processes again.
#[epics_base_rs::epics_test]
async fn a_failed_last_load_put_returns() {
    // oracle: S:np 21, S:sc 0, S:go 0, P.GO 1, S:flnk 0, P.PACT 0, UDF/INVALID.
    let db = failing("OAR").await;
    assert!(!try_put(&db, "GO").await, "the put's status");
    assert_eq!(f(&db, "S:np"), 21.0, "the puts ahead of it went out");
    assert_eq!(f(&db, "S:sc"), 0.0, "the GO group did not");
    assert_eq!(f(&db, "S:go"), 0.0);
    assert_eq!(f(&db, "P.GO"), 1.0, "GO stays set");
    assert_eq!(f(&db, "S:flnk"), 0.0, "no forward link");
    assert_eq!(f(&db, "P.PACT"), 0.0);
    assert_eq!(alarm(&db), (17, 3), "no monitor(), no reset");

    // oracle: S:go 0, S:flnk 0.
    assert!(
        !try_put(&db, "GO").await,
        "processed again, and failed again"
    );
    assert_eq!(f(&db, "S:go"), 0.0);
    assert_eq!(f(&db, "S:flnk"), 0.0);
}

/// A failed put that a later put of the group overwrites in `status` does
/// not return (:157-206).
#[epics_base_rs::epics_test]
async fn a_failed_put_ahead_of_the_last_does_not_return() {
    // oracle: S:sc 1, S:go 1, P.GO 0, S:flnk 1.
    let db = failing("OSP").await;
    assert!(try_put(&db, "GO").await);
    assert_eq!(f(&db, "S:sc"), 1.0);
    assert_eq!(f(&db, "S:go"), 1.0);
    assert_eq!(f(&db, "P.GO"), 0.0);
    assert_eq!(f(&db, "S:flnk"), 1.0);
}

/// The GO group's last put fails (:219): OGO is not put and GO stays set.
#[epics_base_rs::epics_test]
async fn a_failed_start_put_returns() {
    // oracle: S:aqt 0.5, S:go 0, P.GO 1, S:flnk 0.
    let db = failing("OSC").await;
    assert!(!try_put(&db, "GO").await);
    assert_eq!(f(&db, "S:aqt"), 0.5);
    assert_eq!(f(&db, "S:go"), 0.0);
    assert_eq!(f(&db, "P.GO"), 1.0);
    assert_eq!(f(&db, "S:flnk"), 0.0);
}

/// STAT and SEVR of P.
fn alarm(db: &PvDatabase) -> (u16, u16) {
    let rec = db.get_record("P").unwrap();
    let g = rec.read();
    (g.common.stat, g.common.sevr as u16)
}

/// `failing(link)` with `S:up` and `S:upl` putting GO and LOAD through a PP
/// link: their LINK/INVALID is `dbPutLink` seeing scanparm's status.
async fn failing_upstream(link: &str) -> Arc<PvDatabase> {
    sscan_rs::register_sscan_dbd();
    let base = scanparm("S:mpts NPP", "S:busy NPP");
    let text = base.trim_end().strip_suffix('}').unwrap();
    ioc(&format!(
        "{TARGETS}{text}  field({link}, \"NO:SUCH CA\")\n  field(FLNK, \"S:flnk\")\n}}\n\
         record(longout, \"S:up\") {{ field(OUT, \"P.GO PP\") field(VAL, \"1\") }}\n\
         record(longout, \"S:upl\") {{ field(OUT, \"P.LOAD PP\") field(VAL, \"1\") }}\n"
    ))
    .await
}

/// STAT and SEVR of `rec`.
fn alarm_of(db: &PvDatabase, rec: &str) -> (u16, u16) {
    let rec = db.get_record(rec).unwrap();
    let g = rec.read();
    (g.common.stat, g.common.sevr as u16)
}

/// A failed OGO put on a cycle that completes is still `process()`'s
/// status (:250, :271), and a LOAD whose OLOAD put succeeds is not.
#[epics_base_rs::epics_test]
async fn a_failed_go_put_is_the_status_of_a_completed_cycle() {
    // oracle: P.GO 0, S:flnk 1, S:up LINK/INVALID; S:upl NO_ALARM.
    let db = failing_upstream("OGO").await;
    assert!(!try_put(&db, "GO").await, "the OGO put's status");
    assert_eq!(f(&db, "P.GO"), 0.0, "the cycle ran on");
    assert_eq!(f(&db, "S:flnk"), 1.0);
    db.process_record("S:up").await.unwrap();
    assert_eq!(alarm_of(&db, "S:up"), (14, 3));
    db.process_record("S:upl").await.unwrap();
    assert_eq!(alarm_of(&db, "S:upl"), (0, 0));
    assert_eq!(f(&db, "S:flnk"), 3.0);
}

/// OLOAD is put after OGO, so its status is the one returned (:258).
#[epics_base_rs::epics_test]
async fn a_failed_load_put_is_the_status_and_go_alone_is_not() {
    // oracle: S:up NO_ALARM; S:upl LINK/INVALID; S:flnk 3.
    let db = failing_upstream("OLOAD").await;
    assert!(try_put(&db, "GO").await);
    assert!(!try_put(&db, "LOAD").await);
    db.process_record("S:up").await.unwrap();
    assert_eq!(alarm_of(&db, "S:up"), (0, 0));
    db.process_record("S:upl").await.unwrap();
    assert_eq!(alarm_of(&db, "S:upl"), (14, 3));
    assert_eq!(f(&db, "S:flnk"), 4.0);
}

/// TIME of P.
fn time(db: &PvDatabase) -> epics_base_rs::types::WallTime {
    db.get_record("P").unwrap().read().common.time
}

/// `recGblGetTimeStamp` comes after the puts (:263): a cycle that returns
/// at a failed put leaves TIME, and the next completing cycle stamps it.
#[epics_base_rs::epics_test]
async fn a_returned_cycle_leaves_time() {
    // oracle: dbpr P 4 shows TIME <undefined> after the failed GO and LOAD.
    let db = failing("OAR").await;
    let born = time(&db);
    assert!(!try_put(&db, "GO").await);
    assert!(!try_put(&db, "LOAD").await);
    assert_eq!(time(&db), born);
    db.put_pv("S:busy", EpicsValue::Double(1.0)).await.unwrap();
    db.process_record("P").await.unwrap();
    assert_ne!(time(&db), born, "the completing cycle stamps");
}

/// `monitor()` runs `recGblResetAlarms` only when STEP moved (:280): a GO
/// at the init step keeps the born UDF/INVALID, and the LINK/INVALID a
/// failed put raised waits in NSEV until a step change commits it.
#[epics_base_rs::epics_test]
async fn the_alarm_is_reset_only_when_the_step_moves() {
    // oracle: UDF/INVALID, after GO UDF/INVALID, NP 11 LINK/INVALID, NP 21
    // NO_ALARM twice.
    let db = failing("OSP").await;
    assert_eq!(alarm(&db), (17, 3), "born UDF/INVALID");
    assert!(try_put(&db, "GO").await);
    assert_eq!(alarm(&db), (17, 3), "step unchanged: no reset");

    put(&db, "NP", 11.0).await;
    assert_eq!(alarm(&db), (14, 3), "the failed OSP put's LINK/INVALID");
    put(&db, "NP", 21.0).await;
    assert_eq!(alarm(&db), (0, 0));
    put(&db, "NP", 21.0).await;
    assert_eq!(alarm(&db), (0, 0));
}

/// A cycle that returns early never reaches `monitor()`, so LSTP keeps the
/// step it had and the next completing cycle posts the move (:206, :282).
#[epics_base_rs::epics_test]
async fn an_early_return_leaves_lstp() {
    // oracle: STEP 0.2, LSTP 0.1; after the busy cycle GO 0, S:go 1,
    // LSTP 0.2, LINK/INVALID.
    let db = failing("OAR").await;
    db.put_pv("S:mpts", EpicsValue::Double(11.0)).await.unwrap();
    assert!(!try_put(&db, "GO").await);
    assert_eq!(f(&db, "P.STEP"), 0.2);
    assert_eq!(f(&db, "P.LSTP"), 0.1);
    // A busy scan skips the groups, so this cycle completes.
    db.put_pv("S:busy", EpicsValue::Double(1.0)).await.unwrap();
    db.process_record("P").await.unwrap();
    assert_eq!(f(&db, "P.GO"), 0.0);
    assert_eq!(f(&db, "P.LSTP"), 0.2);
    assert_eq!(alarm(&db), (14, 3), "the failed OAR put's LINK/INVALID");
}

/// GO and LOAD set together: OGO goes out, GO clears and posts, and only
/// then OLOAD goes out, so its target reads GO 0 (:248-262).
#[epics_base_rs::epics_test]
async fn go_clears_and_posts_before_the_load_put() {
    // oracle: S:go 1, S:seen 0, P.GO 0, P.LOAD 0.
    sscan_rs::register_sscan_dbd();
    let base = scanparm("S:mpts NPP", "S:busy NPP");
    let text = base.trim_end().strip_suffix('}').unwrap();
    let db = ioc(&format!(
        "{TARGETS}record(calc, \"S:seen\") {{ field(INPA, \"P.GO NPP\") field(CALC, \"A\") field(MDEL, \"-1\") }}\n\
         {text}  field(OLOAD, \"S:seen.PROC\")\n}}\n"
    ))
    .await;
    let stream: Arc<Mutex<Vec<String>>> = Arc::default();
    let mut readers = Vec::new();
    for (rec, field, ty) in [
        ("P", "GO", DbFieldType::Short),
        ("P", "LOAD", DbFieldType::Short),
        ("S:seen", "VAL", DbFieldType::Double),
    ] {
        let cell = db.get_record(rec).unwrap();
        let mut inst = cell.write();
        readers.push(
            inst.add_subscriber(field, 0, ty, EventMask::VALUE.bits())
                .unwrap(),
        );
        assert!(inst.attach_filter_to_last_subscriber(
            field,
            Arc::new(Line {
                field,
                stream: stream.clone(),
            }),
        ));
    }
    db.get_record("P")
        .unwrap()
        .write()
        .record
        .put_field("GO", EpicsValue::Short(1))
        .unwrap();
    put(&db, "LOAD", 1.0).await;
    assert_eq!(f(&db, "S:go"), 1.0);
    assert_eq!(f(&db, "S:seen"), 0.0, "OLOAD's target read GO cleared");
    assert_eq!(
        std::mem::take(&mut *stream.lock().unwrap()),
        ["LOAD 1", "GO 0", "VAL 0", "LOAD 0"]
    );
}

/// `scanparmRecord.c` names `udf` nowhere: process neither clears it nor
/// raises UDF_ALARM on it, so a step move resets the born alarm and leaves
/// UDF at 1.
#[epics_base_rs::epics_test]
async fn process_neither_clears_udf_nor_alarms_on_it() {
    // oracle (oracleSscanIoc, bare record): UDF 1 UDF/INVALID; EP 1: STEP 1,
    // UDF 1 NO_ALARM; UDF 1: UDF 1 NO_ALARM; EP 2: UDF 1 NO_ALARM.
    sscan_rs::register_sscan_dbd();
    let db = ioc("record(scanparm, \"P\") {}\n").await;
    let udf = |db: &PvDatabase| db.get_record("P").unwrap().read().common.udf;
    assert_eq!((udf(&db), alarm(&db)), (1, (17, 3)), "born UDF/INVALID");
    put(&db, "EP", 1.0).await;
    assert_eq!(f(&db, "P.STEP"), 1.0);
    assert_eq!((udf(&db), alarm(&db)), (1, (0, 0)));
    db.put_record_field_from_ca("P", "UDF", EpicsValue::UChar(1))
        .await
        .unwrap();
    assert_eq!((udf(&db), alarm(&db)), (1, (0, 0)));
    put(&db, "EP", 2.0).await;
    assert_eq!((udf(&db), alarm(&db)), (1, (0, 0)));
}
