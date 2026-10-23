//! `"Async Soft Channel"`'s `add_record` refuses a PV link whose target
//! `dbChannelCreate` cannot find (`devAiSoftCallback.c:94-101` and the input
//! twins), at `iocInit` and on a runtime INP put alike. Every line below is
//! `softIoc` R7.0.10-146 (8f5015b66) on the same database and puts.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ai, "SRC") { field(VAL, "3") }
record(ai, "ACB") { field(DTYP, "Async Soft Channel") field(INP, "SRC") }
record(ai, "APL") { field(INP, "SRC") }
record(ai, "AINIT") { field(DTYP, "Async Soft Channel") field(INP, "NOPE") }
record(stringin, "SCB") { field(DTYP, "Async Soft Channel") field(INP, "SRC") }
record(longin, "LCB") { field(DTYP, "Async Soft Channel") field(INP, "SRC") }
record(waveform, "WS") { field(FTVL, "STRING") field(NELM, "2") }
record(waveform, "WD") { field(FTVL, "DOUBLE") field(NELM, "2") }
"#;

fn listen() -> Arc<Mutex<Vec<String>>> {
    let heard = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&heard);
    epics_base_rs::runtime::log::errlog_add_listener(move |m| {
        sink.lock().expect("sink").push(m.to_string());
    });
    heard
}

/// The `recGblRecordError` lines heard since the last call.
fn drain(sink: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    epics_base_rs::runtime::log::errlog_flush();
    sink.lock()
        .expect("sink")
        .drain(..)
        .flat_map(|m| m.lines().map(str::to_string).collect::<Vec<_>>())
        .filter(|l| l.starts_with("recGblRecordError"))
        .collect()
}

async fn ioc() -> Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn put_inp(db: &PvDatabase, rec: &str, text: &str) -> bool {
    db.put_record_field_from_ca_no_notify(rec, "INP", EpicsValue::String(text.into()))
        .await
        .is_ok()
}

#[epics_macros_rs::epics_test]
async fn a_missing_target_is_refused_at_init_and_on_a_put() {
    let sink = listen();
    let db = ioc().await;
    assert_eq!(
        drain(&sink),
        [
            "recGblRecordError: devAiSoftCallback (add_record) link target not found \
          Process Variable Not Found PV: AINIT"
        ]
    );

    assert!(!put_inp(&db, "ACB", "NOPE").await);
    assert_eq!(
        drain(&sink),
        [
            "recGblRecordError: devAiSoftCallback (add_record) link target not found \
          Process Variable Not Found PV: ACB"
        ]
    );
    // C moves the link before asking `add_record`: INP reads `NOPE NPP NMS`.
    let inp = db.get_pv("ACB.INP").unwrap();
    assert!(format!("{inp:?}").contains("NOPE"), "{inp:?}");

    assert!(!put_inp(&db, "SCB", "NOPE").await);
    assert!(!put_inp(&db, "LCB", "NOPE").await);
    assert_eq!(
        drain(&sink),
        [
            "recGblRecordError: devSiSoftCallback (add_record) linked record not found \
             Process Variable Not Found PV: SCB",
            "recGblRecordError: devLiSoftCallback (init_record) linked record not found \
             Process Variable Not Found PV: LCB",
        ]
    );

    // A constant has no `lset` yet when `add_record` runs, so it is not
    // `dbLinkIsDefined` and fails the PV_LINK test.
    assert!(!put_inp(&db, "LCB", "5").await);
    assert_eq!(
        drain(&sink),
        [
            "recGblRecordError: devLiSoftCallback (add_record) Illegal INP field \
          Illegal field value PV: LCB"
        ]
    );

    // A plain soft channel has no `add_record`: the name becomes a CA link.
    assert!(put_inp(&db, "APL", "NOPE").await);
    assert_eq!(drain(&sink), Vec::<String>::new());
}

/// `add_record`'s `dbChannelCreate` parses the field modifiers too
/// (`dbChannel.c:483-523`): a filter or range after the field is accepted, and
/// with no `.` the whole text is the record name (`dbFindRecordPart`). `$`
/// is judged on the type `cvt_dbaddr` leaves, so a string waveform's VAL
/// takes it.
#[epics_macros_rs::epics_test]
async fn a_filtered_target_is_created_as_a_channel() {
    let sink = listen();
    let db = ioc().await;
    drain(&sink);
    let refused = "recGblRecordError: devAiSoftCallback (add_record) link target not found \
                   Process Variable Not Found PV: ACB";
    for (inp, accepted, shown) in [
        (
            r#"SRC.VAL{"dbnd":{"d":1}}"#,
            true,
            r#"SRC.VAL{"dbnd":{"d":1}} NMS"#,
        ),
        (
            r#"SRC.VAL{"arr":{"s":1}}"#,
            true,
            r#"SRC.VAL{"arr":{"s":1}} NMS"#,
        ),
        ("SRC.VAL[1]", true, "SRC.VAL[1] NMS"),
        (r#"SRC.{"ts":{}}"#, true, r#"SRC.{"ts":{}} NMS"#),
        (r#"SRC{"ts":{}}"#, false, r#"SRC{"ts":{}} NPP NMS"#),
        (r#"NOPE{"ts":{}}"#, false, r#"NOPE{"ts":{}} NPP NMS"#),
        (r#"SRC{"nosuch":{}}"#, false, r#"SRC{"nosuch":{}} NPP NMS"#),
        ("SRC.NOFLD", false, "SRC.NOFLD NPP NMS"),
        ("SRC{bad json", false, "SRC{bad NPP NMS"),
        (
            r#"SRC.VAL{"nosuch":{}}"#,
            false,
            r#"SRC.VAL{"nosuch":{}} NPP NMS"#,
        ),
        ("SRC.VAL{bad", false, "SRC.VAL{bad NPP NMS"),
        ("SRC", true, "SRC NMS"),
        ("SRC.RTYP", true, "SRC.RTYP NMS"),
        ("SRC.DESC$", true, "SRC.DESC$ NMS"),
        ("SRC.INP$", true, "SRC.INP$ NMS"),
        ("SRC.", true, "SRC. NMS"),
        ("SRC.PACT", true, "SRC.PACT NMS"),
        ("SRC.VAL$", false, "SRC.VAL$ NPP NMS"),
        ("SRC$", false, "SRC$ NPP NMS"),
        ("WS.VAL$", true, "WS.VAL$ NMS"),
        ("WD.VAL$", false, "WD.VAL$ NPP NMS"),
    ] {
        assert_eq!(put_inp(&db, "ACB", inp).await, accepted, "{inp}");
        let heard = drain(&sink);
        if accepted {
            assert!(heard.is_empty(), "{inp}: {heard:?}");
        } else {
            assert_eq!(heard, [refused], "{inp}");
        }
        let pact = db.get_pv("ACB.PACT").unwrap().to_f64().unwrap();
        assert_eq!(pact, if accepted { 0.0 } else { 1.0 }, "{inp}");
        let EpicsValue::String(text) = db.get_pv("ACB.INP").unwrap() else {
            panic!("{inp}: INP is not a string");
        };
        assert_eq!(text.as_str_lossy(), shown, "{inp}");
    }
}

/// `dbPutFieldLink`'s `pact = FALSE` (`dbAccess.c:1194`) restarts nothing: a
/// put-notify that took the record while it was detached waits for
/// `dbNotifyCompletion` at the end of the next cycle (`dbNotify.c:224-230`).
/// `softIoc` R7.0.10-146 (8f5015b66): `dbtpn ACB 5` after `dbpf ACB.INP NOPE`
/// does not call back across `dbpf ACB.INP SRC`; `dbpf ACB.PROC 1` then
/// prints `dbtpnCallback: success record=ACB` and ACB reads 3.
#[epics_macros_rs::epics_test]
async fn a_put_notify_parked_on_a_detached_record_waits_for_the_next_cycle() {
    let db = ioc().await;
    assert!(!put_inp(&db, "ACB", "NOPE").await);
    let mut rx = db
        .put_record_field_from_ca("ACB", "VAL", EpicsValue::Double(5.0))
        .await
        .expect("the put-notify is accepted")
        .into_handle()
        .expect("a PACT record parks the put-notify");
    assert!(put_inp(&db, "ACB", "SRC").await);
    assert_eq!(db.get_pv("ACB.PACT").unwrap().to_f64(), Some(0.0));
    epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        rx.try_recv().is_err(),
        "the rebind restarted the put-notify"
    );
    assert_eq!(db.get_pv("ACB").unwrap().to_f64(), Some(0.0));

    db.put_record_field_from_ca_no_notify("ACB", "PROC", EpicsValue::Long(1))
        .await
        .unwrap();
    epics_base_rs::runtime::task::timeout(std::time::Duration::from_secs(5), rx)
        .await
        .expect("the next cycle restarts the put-notify")
        .expect("the completion sender must not be dropped");
    assert_eq!(db.get_pv("ACB").unwrap().to_f64(), Some(3.0));
}
