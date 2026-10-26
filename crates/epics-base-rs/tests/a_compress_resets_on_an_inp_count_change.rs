//! compress latches INPN from the INP link's element count and resets its
//! ring when that count changes, before the read — `compressRecord.c:331-343`:
//!
//! ```c
//! if (!prec->wptr || nelements != prec->inpn) {
//!     if (prec->wptr) {
//!         free(prec->wptr);
//!         reset(prec);
//!     }
//!     prec->wptr = dbCalloc(nelements, sizeof(double));
//!     prec->inpn = nelements;
//! }
//! status = dbGetLink(&prec->inp, DBF_DOUBLE, prec->wptr, 0, &nelements);
//! ```
//!
//! `nelements` is `dbGetNelements`, the source field's `no_elements` (a
//! waveform's NELM, a compress's NSAM, a subArray's MALM), not the count the
//! read delivers; and the reset runs before `dbGetLink`, so a read that then
//! fails still finds the ring emptied.
//!
//! oracleIoc (R7.0.10.1), SRC NELM=4, SS3 STRING NELM=3, SD3 DOUBLE NELM=3,
//! CMP NSAM=8 ALG="Circular Buffer":
//!
//! ```text
//! SRC=[1,2,3]          : INPN=4 NUSE=3 VAL=1 2 3
//! SRC=[4,5]            : INPN=4 NUSE=5 VAL=1 2 3 4 5
//! INP=SS3 ("7","abc")  : INPN=3 NUSE=0 VAL=(empty)  LINK/INVALID
//! INP=SD3 [11,12,13]   : INPN=3 NUSE=3 VAL=11 12 13 NO_ALARM
//! INP=SS3              : INPN=3 NUSE=3 VAL=11 12 13 INVALID
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::runtime::task::{MandatoryThread, StackSizeClass, ThreadPriority};
use epics_base_rs::server::database::{
    LinkDbfType, LinkRef, LinkSet, LinkShape, ProcStack, PvDatabase,
};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(waveform, "SRC") { field(FTVL, "DOUBLE") field(NELM, "4") }
record(waveform, "SS3") { field(FTVL, "STRING") field(NELM, "3") }
record(waveform, "SD3") { field(FTVL, "DOUBLE") field(NELM, "3") }
record(compress, "CMP") { field(INP, "SRC") field(ALG, "Circular Buffer") field(NSAM, "8") }
"#;

async fn ioc(db: &str) -> Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process_record(db: &PvDatabase, rec: &str) {
    db.process_record_with_links(rec, &mut ProcStack::new())
        .await
        .unwrap();
}

async fn process(db: &PvDatabase) {
    process_record(db, "CMP").await;
}

/// `rec` processed `n` times on a scan thread, where a cycle that locks its
/// own record again wedges and fails the wait.
fn process_on_scan_thread(db: &Arc<PvDatabase>, rec: &'static str, n: usize) {
    let (tx, rx) = std::sync::mpsc::channel();
    let db = db.clone();
    MandatoryThread::new("scan-1", ThreadPriority::ScanLow, StackSizeClass::Big).spawn(move || {
        for _ in 0..n {
            db.process_record_with_links_already_locked(rec, &mut ProcStack::new())
                .or_else(epics_base_rs::error::CaError::unless_process_status)
                .unwrap();
        }
        tx.send(()).unwrap();
    });
    rx.recv_timeout(Duration::from_secs(20))
        .expect("the cycle wedged");
}

async fn retarget(db: &PvDatabase, src: &str) {
    db.put_record_field_from_ca_no_notify("CMP", "INP", EpicsValue::String(src.into()))
        .await
        .unwrap();
}

fn state_of(db: &PvDatabase, rec: &str) -> (i64, i64, EpicsValue, AlarmSeverity, u16) {
    let inpn = db.get_pv(format!("{rec}.INPN")).unwrap().to_f64().unwrap() as i64;
    let nuse = db.get_pv(format!("{rec}.NUSE")).unwrap().to_f64().unwrap() as i64;
    let val = db.get_pv(rec).unwrap();
    let inst = db.get_record(rec).unwrap();
    let c = &inst.read().common;
    (inpn, nuse, val, c.sevr, c.stat)
}

fn state(db: &PvDatabase) -> (i64, i64, EpicsValue, AlarmSeverity, u16) {
    state_of(db, "CMP")
}

fn read_ok(inpn: i64, val: &[f64]) -> (i64, i64, EpicsValue, AlarmSeverity, u16) {
    (
        inpn,
        val.len() as i64,
        EpicsValue::DoubleArray(val.to_vec().into()),
        AlarmSeverity::NoAlarm,
        alarm_status::NO_ALARM,
    )
}

#[epics_macros_rs::epics_test]
async fn inpn_is_the_source_capacity_and_a_change_resets_before_the_read() {
    let (db, _): (Arc<PvDatabase>, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.put_pv("SRC", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0].into()))
        .await
        .unwrap();
    db.put_pv(
        "SS3",
        EpicsValue::StringArray(vec!["7".into(), "abc".into(), "9".into()].into()),
    )
    .await
    .unwrap();
    db.put_pv(
        "SD3",
        EpicsValue::DoubleArray(vec![11.0, 12.0, 13.0].into()),
    )
    .await
    .unwrap();

    process(&db).await;
    assert_eq!(
        state(&db),
        (
            4,
            3,
            EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0].into()),
            AlarmSeverity::NoAlarm,
            alarm_status::NO_ALARM
        )
    );

    // NORD changes, the capacity does not: no reset.
    db.put_pv("SRC", EpicsValue::DoubleArray(vec![4.0, 5.0].into()))
        .await
        .unwrap();
    process(&db).await;
    assert_eq!(
        state(&db),
        (
            4,
            5,
            EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0, 5.0].into()),
            AlarmSeverity::NoAlarm,
            alarm_status::NO_ALARM
        )
    );

    // The capacity changes and the read fails: the reset already ran.
    retarget(&db, "SS3").await;
    process(&db).await;
    assert_eq!(
        state(&db),
        (
            3,
            0,
            EpicsValue::DoubleArray(vec![].into()),
            AlarmSeverity::Invalid,
            alarm_status::LINK_ALARM
        )
    );

    retarget(&db, "SD3").await;
    process(&db).await;
    assert_eq!(
        state(&db),
        (
            3,
            3,
            EpicsValue::DoubleArray(vec![11.0, 12.0, 13.0].into()),
            AlarmSeverity::NoAlarm,
            alarm_status::NO_ALARM
        )
    );

    // The read fails at an unchanged capacity: the ring is kept.
    retarget(&db, "SS3").await;
    process(&db).await;
    assert_eq!(
        state(&db),
        (
            3,
            3,
            EpicsValue::DoubleArray(vec![11.0, 12.0, 13.0].into()),
            AlarmSeverity::Invalid,
            alarm_status::LINK_ALARM
        )
    );
}

/// `lnkCalc_getElements` is 1 (`lnkCalc.c:526-530`): oracleIoc after two
/// passes reads `INPN=1 NUSE=2 VAL=6 6 NO_ALARM`.
#[epics_macros_rs::epics_test]
async fn a_calc_inp_latches_one_element() {
    let (db, _): (Arc<PvDatabase>, _) = IocBuilder::new()
        .db_string(
            r#"record(compress, "CMP") { field(INP, {calc:{expr:"A+1",args:[5]}}) field(ALG, "Circular Buffer") field(NSAM, "8") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    process(&db).await;
    process(&db).await;
    assert_eq!(
        state(&db),
        (
            1,
            2,
            EpicsValue::DoubleArray(vec![6.0, 6.0].into()),
            AlarmSeverity::NoAlarm,
            alarm_status::NO_ALARM
        )
    );
}

const NO_NELM: &str = r#"
record(ai, "A") { field(VAL, "3") }
record(compress, "CW") { field(INP, "A") field(ALG, "Circular Buffer") field(NSAM, "8") }
record(compress, "CN") { field(INP, "CW") field(ALG, "Circular Buffer") field(NSAM, "16") }
record(waveform, "W") { field(FTVL, "DOUBLE") field(NELM, "10") }
record(subArray, "SA") { field(INP, "W") field(FTVL, "DOUBLE") field(MALM, "10") field(NELM, "3") }
record(compress, "CSA") { field(INP, "SA") field(ALG, "Circular Buffer") field(NSAM, "16") }
"#;

/// A compress source's `no_elements` is its NSAM, whatever its NUSE: C
/// softIoc (R7.0.10.1), CW then CN each cycle, reads `INPN=8` with `NUSE`
/// 1, 3, 6 — the ring is never reset.
#[epics_macros_rs::epics_test]
async fn a_compress_source_latches_its_nsam() {
    let db = ioc(NO_NELM).await;
    for val in [&[3.0][..], &[3.0; 3], &[3.0; 6]] {
        process_record(&db, "CW").await;
        process_record(&db, "CN").await;
        assert_eq!(state_of(&db, "CN"), read_ok(8, val));
    }
}

/// A subArray source's `no_elements` is its MALM, not NELM: C softIoc,
/// W=[1..10], SA then CSA, reads `INPN=10 NUSE=3`, and after `SA.NELM=4`
/// `INPN=10 NUSE=7 VAL=1 2 3 1 2 3 4` — no reset.
#[epics_macros_rs::epics_test]
async fn a_subarray_source_latches_its_malm() {
    let db = ioc(NO_NELM).await;
    db.put_record_field_from_ca_no_notify(
        "W",
        "VAL",
        EpicsValue::DoubleArray((1..=10).map(f64::from).collect()),
    )
    .await
    .unwrap();
    process_record(&db, "SA").await;
    process_record(&db, "CSA").await;
    assert_eq!(state_of(&db, "CSA"), read_ok(10, &[1.0, 2.0, 3.0]));
    db.put_record_field_from_ca_no_notify("SA", "NELM", EpicsValue::Long(4))
        .await
        .unwrap();
    process_record(&db, "SA").await;
    process_record(&db, "CSA").await;
    assert_eq!(
        state_of(&db, "CSA"),
        read_ok(10, &[1.0, 2.0, 3.0, 1.0, 2.0, 3.0, 4.0])
    );
}

/// An INP naming the compress's own field: C softIoc reads `INPN=1 NUSE=2
/// VAL=8 8 NO_ALARM` after two passes of `CS.NSAM`, and `INPN=0 NUSE=0
/// LINK/INVALID` after one of `CX.NOPE`, a field the record does not have.
/// Neither count is asked under the record's own hold.
#[epics_macros_rs::epics_test]
async fn a_self_referencing_inp_is_counted_without_the_hold() {
    let db = ioc(r#"
record(compress, "CS") { field(INP, "CS.NSAM") field(ALG, "Circular Buffer") field(NSAM, "8") }
record(compress, "CX") { field(INP, "CX.NOPE") field(ALG, "Circular Buffer") field(NSAM, "8") }
"#)
    .await;
    process_on_scan_thread(&db, "CS", 2);
    assert_eq!(state_of(&db, "CS"), read_ok(1, &[8.0, 8.0]));
    process_on_scan_thread(&db, "CX", 1);
    assert_eq!(
        state_of(&db, "CX"),
        (
            0,
            0,
            EpicsValue::DoubleArray(vec![].into()),
            AlarmSeverity::Invalid,
            alarm_status::LINK_ALARM
        )
    );
}

/// `dbGetNelements` is the channel's final count, through the link's
/// filters: C softIoc, W=[1..10] NELM=10, reads `INPN=3 NUSE=3 VAL=1 2 3`
/// through `W.{"arr":{"s":0,"e":2}}`.
#[epics_macros_rs::epics_test]
async fn a_filtered_inp_latches_the_filtered_count() {
    let db = ioc(r#"
record(waveform, "W") { field(FTVL, "DOUBLE") field(NELM, "10") }
record(compress, "CF") { field(INP, "W.{\"arr\":{\"s\":0,\"e\":2}}") field(ALG, "Circular Buffer") field(NSAM, "16") }
"#)
    .await;
    db.put_record_field_from_ca_no_notify(
        "W",
        "VAL",
        EpicsValue::DoubleArray((1..=10).map(f64::from).collect()),
    )
    .await
    .unwrap();
    process_record(&db, "CF").await;
    assert_eq!(state_of(&db, "CF"), read_ok(3, &[1.0, 2.0, 3.0]));
}

/// A CA INP's count is the channel's, `dbCaGetNelements`'s `pca->nelements`
/// (`dbCa.c:662-670`). C softIoc, W=[1,2,3] NELM=16, AE an ai, CS8 an empty
/// compress NSAM=8, reads `CCW INPN=16 NUSE=3` then `NUSE=6 VAL=1 2 3 1 2 3`,
/// `CCA INPN=1 NUSE=2 VAL=3 3` after two passes, and `CCS INPN=8 NUSE=0
/// LINK/INVALID`.
#[epics_macros_rs::epics_test]
async fn a_ca_inp_latches_the_channels_count() {
    let db = ioc(r#"
record(waveform, "W") { field(FTVL, "DOUBLE") field(NELM, "16") }
record(compress, "CCW") { field(INP, "W CA") field(ALG, "Circular Buffer") field(NSAM, "40") }
record(ai, "AE") { field(VAL, "3") field(EGU, "mm") field(DESC, "d") }
record(compress, "CCA") { field(INP, "AE CA") field(ALG, "Circular Buffer") field(NSAM, "8") }
record(compress, "CS8") { field(ALG, "Circular Buffer") field(NSAM, "8") }
record(compress, "CCS") { field(INP, "CS8 CA") field(ALG, "Circular Buffer") field(NSAM, "16") }
"#)
    .await;
    db.put_pv("W", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0].into()))
        .await
        .unwrap();
    db.ioc_init().await;
    db.setup_cp_links().await;
    db.setup_external_link_opens().await;
    db.process_cp_links_on_connect().await;
    let lset = db.link_set("ca").await.expect("the ca link set");
    for rec in ["CCW", "CCA", "CCS"] {
        let (_, _, inp) = db
            .record_link_fields(rec)
            .into_iter()
            .find(|(field, _, _)| field == "INP")
            .unwrap();
        let inp = inp.external().expect("a CA link");
        let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
        while db.external_link_metadata(inp).is_none()
            || lset
                .get_cached_value(LinkRef::new(inp.name, inp.open))
                .is_none()
        {
            assert!(std::time::Instant::now() < deadline, "{rec}.INP delivers");
            epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
        }
    }

    process_record(&db, "CCW").await;
    assert_eq!(state_of(&db, "CCW"), read_ok(16, &[1.0, 2.0, 3.0]));
    process_record(&db, "CCW").await;
    assert_eq!(
        state_of(&db, "CCW"),
        read_ok(16, &[1.0, 2.0, 3.0, 1.0, 2.0, 3.0])
    );
    process_record(&db, "CCA").await;
    process_record(&db, "CCA").await;
    assert_eq!(state_of(&db, "CCA"), read_ok(1, &[3.0, 3.0]));
    process_record(&db, "CCS").await;
    assert_eq!(
        state_of(&db, "CCS"),
        (
            8,
            0,
            EpicsValue::DoubleArray(vec![].into()),
            AlarmSeverity::Invalid,
            alarm_status::LINK_ALARM
        )
    );
}

/// Connected `{pva:...}` links holding `[1, 2, 3]`: one ending in `Z` counts 0
/// elements, any other has no count yet.
struct Uncounted;

#[async_trait::async_trait]
impl LinkSet for Uncounted {
    fn is_connected(&self, _: LinkRef<'_>) -> bool {
        true
    }
    fn get_cached_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        Some(EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0].into()))
    }
    async fn get_value(&self, link: LinkRef<'_>) -> Option<EpicsValue> {
        self.get_cached_value(link)
    }
    fn link_shape(&self, link: LinkRef<'_>) -> Option<LinkShape> {
        link.name.ends_with(b"Z").then_some(LinkShape {
            dbf_type: Some(LinkDbfType::Double),
            element_count: Some(0),
        })
    }
}

/// A count `compressRecord.c:328-330` refuses is no read: LINK/INVALID by
/// `recGblSetSevr`, so no AMSG, and the epilogue still runs. C softIoc
/// (R7.0.10-146) with `INP "NOWHERE:X CA"`, never connecting, reads
/// `INVALID LINK AMSG="" UDF=0 INPN=0 NUSE=0`; a connected link with a value
/// and no count yet, or a count of 0, fails the same test.
#[epics_macros_rs::epics_test]
async fn a_refused_count_is_no_read() {
    let ioc = IocBuilder::new()
        .register_link_type("pva", |db| async move {
            db.register_link_set("pva", Arc::new(Uncounted)).await;
            Vec::new()
        })
        .db_string(
            r#"
record(compress, "CN") { field(INP, "NOWHERE:X CA") field(ALG, "Circular Buffer") field(NSAM, "8") }
record(compress, "CU") { field(INP, {pva:"U"}) field(ALG, "Circular Buffer") field(NSAM, "8") }
record(compress, "CZ") { field(INP, {pva:"Z"}) field(ALG, "Circular Buffer") field(NSAM, "8") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    let mut seen = Vec::new();
    for rec in ["CN", "CU", "CZ"] {
        process_record(&db, rec).await;
        let amsg = db.get_pv(format!("{rec}.AMSG")).unwrap();
        let udf = db.get_pv(format!("{rec}.UDF")).unwrap().to_f64();
        seen.push((rec, state_of(&db, rec), amsg, udf));
    }
    let refused = |rec| {
        (
            rec,
            (
                0,
                0,
                EpicsValue::DoubleArray(vec![].into()),
                AlarmSeverity::Invalid,
                alarm_status::LINK_ALARM,
            ),
            EpicsValue::String("".into()),
            Some(0.0),
        )
    };
    assert_eq!(seen, ["CN", "CU", "CZ"].map(refused));
}
