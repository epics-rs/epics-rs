//! The posts a `special()` makes, against camonitor on softIoc R7.0.10 (base
//! records) and on the calc R3-7-5 oracle IOC (scalcout, acalcout, sseq, swait): which
//! fields post, under which of `-m v`, `-m l`, `-m a`, on which put. A line is
//! `mask FIELD value status severity`; the alarm tells a post `special()` made
//! before the put's process from the one its `monitor()` made after.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::{
    acalcout::AcalcoutRecord, scalcout::ScalcoutRecord, sseq::SseqRecord, swait::SwaitRecord,
};
use epics_base_rs::types::{DbFieldType, EpicsValue};

struct Cam {
    db: Arc<PvDatabase>,
    subs: Vec<(char, String, EventReader)>,
}

impl Cam {
    /// One subscriber per `REC.FIELD` and mask, in camonitor's three modes.
    async fn new(records: &str, pvs: &[&str]) -> Self {
        Self::boot(records, pvs, false).await
    }

    /// [`Cam::new`] loading as the oracle IOC's st.cmd `dbLoadRecords` does:
    /// a refused field fails the load and iocInit still boots the record.
    async fn from_script(records: &str, pvs: &[&str]) -> Self {
        Self::boot(records, pvs, true).await
    }

    async fn boot(records: &str, pvs: &[&str], script: bool) -> Self {
        // The calc module's types, as the oracle IOC's dbd registers them.
        let builder = IocBuilder::new()
            .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
            .register_record_type("acalcout", || Box::new(AcalcoutRecord::default()))
            .register_record_type("sseq", || Box::new(SseqRecord::default()))
            .register_record_type("swait", || Box::new(SwaitRecord::default()))
            .db_string(records, &HashMap::new())
            .unwrap();
        let builder = if script {
            builder.boot_past_failed_loads()
        } else {
            builder
        };
        let (db, _) = builder.build().await.unwrap();
        epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
        let mut subs = Vec::new();
        for (m, mask) in [
            ('v', EventMask::VALUE),
            ('l', EventMask::LOG),
            ('a', EventMask::ALARM),
        ] {
            for &pv in pvs {
                let (rec, field) = pv.split_once('.').unwrap();
                let rx = db
                    .get_record(rec)
                    .unwrap()
                    .write()
                    .add_subscriber(field, subs.len() as u32, DbFieldType::String, mask.bits())
                    .unwrap();
                subs.push((m, pv.to_string(), rx));
            }
        }
        Self { db, subs }
    }

    /// `caput REC.FIELD text`, then every post it made.
    async fn put(&mut self, pv: &str, text: &str) -> Vec<String> {
        let (rec, field) = pv.split_once('.').unwrap_or((pv, "VAL"));
        let _ = self
            .db
            .put_record_field_from_ca(rec, field, EpicsValue::String(text.into()))
            .await;
        epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
        let mut out = Vec::new();
        for (m, pv, rx) in &mut self.subs {
            let (rec, field) = pv.split_once('.').unwrap();
            while let Ok(e) = rx.try_recv() {
                let value = e
                    .delivered_value(&self.db.get_record(rec).unwrap().read(), field)
                    .unwrap();
                out.push(format!("{m} {pv} {value} {} {}", e.stat(), e.sevr()));
            }
        }
        out
    }
}

/// compress RES and N are `SPC_RESET`: `reset()` then `monitor()`, whose
/// `recGblResetAlarms` commits the born UDF and ORs DBE_ALARM into NUSE and
/// VAL; NUSE posts only when that moved the alarm or NUSE moved against OUSE
/// (compressRecord.c:428-438). softIoc, `camonitor CMP.VAL CMP.NUSE CMP.RES
/// CMP.STAT` on a fresh record:
///
/// ```text
/// caput CMP.RES 1   -v STAT NO_ALARM, NUSE 0, VAL, RES 0   -l NUSE 0, VAL, RES 0
///                   -a STAT NO_ALARM, NUSE 0, VAL
/// caput CMP.RES 1   -v VAL, RES 0   -l VAL, RES 0   -a nothing
/// caput CMP.N 2     -v VAL   -l VAL   -a nothing
/// ```
#[epics_macros_rs::epics_test]
async fn compress_reset_posts_match_softioc() {
    let mut cam = Cam::new(
        r#"record(compress, "CMP") { field(NSAM, "4") field(ALG, "Circular Buffer") }"#,
        &["CMP.VAL", "CMP.NUSE", "CMP.RES", "CMP.STAT"],
    )
    .await;
    assert_eq!(
        cam.put("CMP.RES", "1").await,
        [
            "v CMP.VAL [] 0 0",
            "v CMP.NUSE 0 0 0",
            "v CMP.RES 0 0 0",
            "v CMP.STAT 0 0 0",
            "l CMP.VAL [] 0 0",
            "l CMP.NUSE 0 0 0",
            "l CMP.RES 0 0 0",
            "a CMP.VAL [] 0 0",
            "a CMP.NUSE 0 0 0",
            "a CMP.STAT 0 0 0",
        ]
    );
    assert_eq!(
        cam.put("CMP.RES", "1").await,
        [
            "v CMP.VAL [] 0 0",
            "v CMP.RES 0 0 0",
            "l CMP.VAL [] 0 0",
            "l CMP.RES 0 0 0",
        ]
    );
    assert_eq!(
        cam.put("CMP.N", "2").await,
        ["v CMP.VAL [] 0 0", "l CMP.VAL [] 0 0"]
    );
}

/// mbbi/mbbo `??ST` are `SPC_MOD` and `pp(TRUE)`: `special()` posts VAL with
/// DBE_VALUE|DBE_LOG only when the string written is the current state's
/// (mbbiRecord.c:218-224, mbboRecord.c:233-239), before the process, so it
/// still carries the born UDF; the process then posts VAL on DBE_ALARM alone.
/// softIoc, `camonitor MI.VAL MI.ONST MO.VAL MO.ONST`, VAL=1:
///
/// ```text
/// caput MI.ONST uno   -v/-l MI.VAL uno UDF NO_ALARM, MI.ONST uno UDF NO_ALARM
///                     -a MI.VAL uno (NO_ALARM)
/// caput MI.TWST dos   nothing
/// ```
///
/// and the same for MO.
#[epics_macros_rs::epics_test]
async fn mbbi_mbbo_state_string_posts_match_softioc() {
    for rec in ["MI", "MO"] {
        let rtype = if rec == "MI" { "mbbi" } else { "mbbo" };
        let (val, onst) = (format!("{rec}.VAL"), format!("{rec}.ONST"));
        let mut cam = Cam::new(
            &format!(
                r#"record({rtype}, "{rec}") {{ field(VAL, "1") field(ZRST, "zero") field(ONST, "one") }}"#
            ),
            &[&val, &onst],
        )
        .await;
        assert_eq!(
            cam.put(&onst, "uno").await,
            [
                format!("v {val} 1 17 0"),
                format!("v {onst} uno 17 0"),
                format!("l {val} 1 17 0"),
                format!("l {onst} uno 17 0"),
                format!("a {val} 1 0 0"),
            ],
            "{rec}"
        );
        assert_eq!(
            cam.put(&format!("{rec}.TWST"), "dos").await,
            Vec::<String>::new(),
            "{rec}"
        );
    }
}

/// calcout CALC/OCAL are `SPC_CALC` and `pp(TRUE)`: `special()` posts CLCV or
/// OCLV with a literal DBE_VALUE (calcoutRecord.c:332-343) before the process.
/// softIoc, `camonitor CO.CLCV CO.OCLV CO.CALC CO.OCAL`, CALC=A:
///
/// ```text
/// caput CO.CALC A+1     -v CLCV 0 UDF INVALID, CALC   -l CALC   -a nothing
/// caput CO.CALC "A+("   -v CLCV -1, CALC              -l CALC   -a nothing
/// caput CO.OCAL B       -v OCLV 0 CALC INVALID, OCAL  -l OCAL   -a nothing
/// caput CO.OCAL "B+("   -v OCLV -1 CALC INVALID, OCAL -l OCAL   -a nothing
/// ```
#[epics_macros_rs::epics_test]
async fn calcout_calc_posts_match_softioc() {
    let mut cam = Cam::new(
        r#"record(calcout, "CO") { field(CALC, "A") }"#,
        &["CO.CLCV", "CO.OCLV", "CO.CALC", "CO.OCAL"],
    )
    .await;
    assert_eq!(
        cam.put("CO.CALC", "A+1").await,
        [
            "v CO.CLCV 0 17 3",
            "v CO.CALC A+1 17 3",
            "l CO.CALC A+1 17 3"
        ]
    );
    assert_eq!(
        cam.put("CO.CALC", "A+(").await,
        ["v CO.CLCV -1 0 0", "v CO.CALC A+( 0 0", "l CO.CALC A+( 0 0"]
    );
    assert_eq!(
        cam.put("CO.OCAL", "B").await,
        ["v CO.OCLV 0 12 3", "v CO.OCAL B 12 3", "l CO.OCAL B 12 3"]
    );
    assert_eq!(
        cam.put("CO.OCAL", "B+(").await,
        [
            "v CO.OCLV -1 12 3",
            "v CO.OCAL B+( 12 3",
            "l CO.OCAL B+( 12 3"
        ]
    );
}

/// scalcout's `special()` posts CLCV/OCLV with a literal DBE_VALUE
/// (sCalcoutRecord.c:470, :480). calc oracle IOC, `camonitor SC.CLCV
/// SC.OCLV`, CALC=A:
///
/// ```text
/// caput SC.CALC A+1     -v CLCV 0 UDF INVALID     -l/-a nothing
/// caput SC.CALC "A+("   -v CLCV -1                -l/-a nothing
/// caput SC.OCAL B       -v OCLV 0 CALC INVALID    -l/-a nothing
/// ```
#[epics_macros_rs::epics_test]
async fn scalcout_calc_posts_match_oracle() {
    let mut cam = Cam::new(
        r#"record(scalcout, "SC") { field(CALC, "A") }"#,
        &["SC.CLCV", "SC.OCLV"],
    )
    .await;
    assert_eq!(cam.put("SC.CALC", "A+1").await, ["v SC.CLCV 0 17 3"]);
    assert_eq!(cam.put("SC.CALC", "A+(").await, ["v SC.CLCV -1 0 0"]);
    assert_eq!(cam.put("SC.OCAL", "B").await, ["v SC.OCLV 0 12 3"]);
}

/// acalcout's `special()` posts CLCV/OCLV with a literal DBE_VALUE
/// (aCalcoutRecord.c:478, :490). calc oracle IOC, `camonitor AC.CLCV
/// AC.OCLV`, CALC=A, NELM=4:
///
/// ```text
/// caput AC.CALC A+1     -v CLCV 0 UDF INVALID   -l/-a nothing
/// caput AC.OCAL "B+("   -v OCLV -1              -l/-a nothing
/// ```
#[epics_macros_rs::epics_test]
async fn acalcout_calc_posts_match_oracle() {
    let mut cam = Cam::new(
        r#"record(acalcout, "AC") { field(CALC, "A") field(NELM, "4") }"#,
        &["AC.CLCV", "AC.OCLV"],
    )
    .await;
    assert_eq!(cam.put("AC.CALC", "A+1").await, ["v AC.CLCV 0 17 3"]);
    assert_eq!(cam.put("AC.OCAL", "B+(").await, ["v AC.OCLV -1 0 0"]);
}

/// sseq's DLYn `special()` re-quantizes and posts DLY1 whichever DLY was put,
/// DBE_VALUE alone, before dbPut posts the field itself (sseqRecord.c:1023-1031).
/// calc oracle IOC, `camonitor SQ.DLY1 SQ.DLY3`:
///
/// ```text
/// caput SQ.DLY3 1.5   -v DLY1 0, DLY3 1.5   -l DLY3 1.5   -a nothing
/// caput SQ.DLY1 2     -v DLY1 2, DLY1 2     -l DLY1 2     -a nothing
/// ```
#[epics_macros_rs::epics_test]
async fn sseq_dly_posts_match_oracle() {
    let mut cam = Cam::new(r#"record(sseq, "SQ") {}"#, &["SQ.DLY1", "SQ.DLY3"]).await;
    assert_eq!(
        cam.put("SQ.DLY3", "1.5").await,
        [
            "v SQ.DLY1 0 17 3",
            "v SQ.DLY3 1.5 17 3",
            "l SQ.DLY3 1.5 17 3"
        ]
    );
    assert_eq!(
        cam.put("SQ.DLY1", "2").await,
        ["v SQ.DLY1 2 17 3", "v SQ.DLY1 2 17 3", "l SQ.DLY1 2 17 3"]
    );
}

/// lso/lsi VAL `special()` sets LEN and posts it DBE_VALUE|DBE_LOG on every
/// put (lsoRecord.c:239, lsiRecord.c:197), changed or not, and `monitor()`
/// posts it again when `len != olen`. A link put through `dbPutLink` runs
/// the same `special()`, so an NPP write posts LEN alone. softIoc, `camonitor
/// LSO.LEN LSO.VAL LSI.LEN LSI.VAL`, SIZV=64, W/WI stringouts with OUT at
/// LSO/LSI NPP:
///
/// ```text
/// caput LSO abc    -v/-l LEN 4 UDF INVALID, LEN 4, VAL abc   -a VAL abc
/// caput LSO abc    -v/-l LEN 4                                -a nothing
/// caput LSO abcd   -v/-l LEN 5, LEN 5, VAL abcd               -a nothing
/// caput W xy       -v/-l LSO.LEN 3                            -a nothing
/// caput W xy       -v/-l LSO.LEN 3                            -a nothing
/// ```
///
/// and the same for LSI through WI.
#[epics_macros_rs::epics_test]
async fn lso_lsi_len_posts_match_softioc() {
    for (rec, rtype, writer) in [("LSO", "lso", "W"), ("LSI", "lsi", "WI")] {
        let (len, val) = (format!("{rec}.LEN"), format!("{rec}.VAL"));
        let mut cam = Cam::new(
            &format!(
                r#"record({rtype}, "{rec}") {{ field(SIZV, "64") }}
record(stringout, "{writer}") {{ field(OUT, "{rec} NPP") }}"#
            ),
            &[&len, &val],
        )
        .await;
        let both = |lines: &[&str]| -> Vec<String> {
            ["v", "l"]
                .iter()
                .flat_map(|m| lines.iter().map(move |l| format!("{m} {l}")))
                .collect()
        };
        let mut want = both(&[
            &format!("{len} 4 17 3"),
            &format!("{len} 4 0 0"),
            &format!("{val} abc 0 0"),
        ]);
        want.push(format!("a {val} abc 0 0"));
        assert_eq!(cam.put(rec, "abc").await, want, "{rec}");
        assert_eq!(
            cam.put(rec, "abc").await,
            both(&[&format!("{len} 4 0 0")]),
            "{rec}"
        );
        assert_eq!(
            cam.put(rec, "abcd").await,
            both(&[
                &format!("{len} 5 0 0"),
                &format!("{len} 5 0 0"),
                &format!("{val} abcd 0 0"),
            ]),
            "{rec}"
        );
        for _ in 0..2 {
            assert_eq!(
                cam.put(writer, "xy").await,
                both(&[&format!("{len} 3 0 0")]),
                "{writer} -> {rec}"
            );
        }
    }
}

/// swait CALC is `SPC_CALC`, not `pp`: `special()` compiles it, writes
/// `recGblRecordError` on a refused `postfix()`, and posts CLCV, CALC, CLCV
/// with a literal DBE_VALUE (swaitRecord.c:560-569). The special post of CALC
/// and the put's own DBE_VALUE|DBE_LOG post reach a `-m v` monitor as one event:
/// a string field log is a reference, which `db_queue_event_log` does not queue
/// twice (dbEvent.c:794-801). calc oracle IOC (calc f207871), swait.db,
/// `camonitor SW.CLCV SW.CALC`:
///
/// ```text
/// dbpf SW.CALC "A+1"   -v CLCV 0 UDF INVALID, CALC A+1, CLCV 0   -l CALC A+1
/// dbpf SW.CALC "A+"    -v CLCV -1, CALC A+, CLCV -1             -l CALC A+
/// recGblRecordError: swaitRecord:special: Illegal CALC field Illegal field value PV: SW
/// ```
#[epics_macros_rs::epics_test]
async fn swait_calc_posts_match_oracle() {
    let heard = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&heard);
    epics_base_rs::runtime::log::errlog_add_listener(move |m| {
        sink.lock().unwrap().push(m.to_string());
    });
    let mut cam = Cam::new(
        r#"
record(ao, "SWIN") { field(VAL, "3") }
record(swait, "SW") { field(INAN, "SWIN") field(CALC, "A") }
"#,
        &["SW.CLCV", "SW.CALC"],
    )
    .await;
    let errlog = || {
        epics_base_rs::runtime::log::errlog_flush();
        std::mem::take(&mut *heard.lock().unwrap())
    };
    assert_eq!(errlog(), Vec::<String>::new());
    assert_eq!(
        cam.put("SW.CALC", "A+1").await,
        [
            "v SW.CLCV 0 17 3",
            "v SW.CLCV 0 17 3",
            "v SW.CALC A+1 17 3",
            "l SW.CALC A+1 17 3"
        ]
    );
    assert_eq!(errlog(), Vec::<String>::new());
    assert_eq!(
        cam.put("SW.CALC", "A+").await,
        [
            "v SW.CLCV -1 17 3",
            "v SW.CLCV -1 17 3",
            "v SW.CALC A+ 17 3",
            "l SW.CALC A+ 17 3"
        ]
    );
    assert_eq!(
        errlog(),
        ["recGblRecordError: swaitRecord:special: Illegal CALC field Illegal field value PV: SW\n"]
    );
    assert_eq!(cam.db.get_pv("SW.CALC").unwrap().to_string(), "A+");
    assert_eq!(cam.db.get_pv("SW.CLCV").unwrap().to_f64(), Some(-1.0));
}

/// swait `init_record` reports a refused CALC through `recGblRecordError` and
/// nothing else (swaitRecord.c:304-308). calc oracle IOC, swait.db with CALC
/// "A+": `recGblRecordError: swait:init_record: Illegal CALC field Illegal
/// field value PV: SW`, then CLCV -1.
#[epics_macros_rs::epics_test]
async fn swait_init_bad_calc_matches_oracle() {
    let heard = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&heard);
    epics_base_rs::runtime::log::errlog_add_listener(move |m| {
        sink.lock().unwrap().push(m.to_string());
    });
    let cam = Cam::from_script(r#"record(swait, "SW") { field(CALC, "A+") }"#, &["SW.CLCV"]).await;
    epics_base_rs::runtime::log::errlog_flush();
    // The loader's own `in CALC expression` line, where it refuses the text,
    // is the load's to pin; this is init_record's.
    let heard: Vec<String> = std::mem::take(&mut *heard.lock().unwrap())
        .into_iter()
        .filter(|m| m.starts_with("recGblRecordError"))
        .collect();
    assert_eq!(
        heard,
        ["recGblRecordError: swait:init_record: Illegal CALC field Illegal field value PV: SW\n"]
    );
    assert_eq!(cam.db.get_pv("SW.CALC").unwrap().to_string(), "A+");
    assert_eq!(cam.db.get_pv("SW.CLCV").unwrap().to_f64(), Some(-1.0));
}
