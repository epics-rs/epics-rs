//! A DB input link carrying a channel filter still names a LOCAL target, and
//! the link-connection-status diagnostics must say so.
//!
//! `PvDatabase::link_target_field_type` is what `classify_link` asks
//! (`records/link_status.rs:165`) to fill `calcout`'s `INAV`..`INUV`,
//! `acalcout`/`scalcout`'s `IAAV`.., `transform`'s `IAV`.. and `swait`'s PV
//! status: `Some(type)` is `Local PV`, `None` is `Ext PV NC`. It resolved the
//! target from the link's raw record/field halves, which for a filtered link
//! are the WHOLE name and `VAL` — `get_record("SRC.VAL[0]")` misses, and every
//! filtered local link was reported as an unconnected external PV.
//!
//! Measured on `R7.0.10-146-g8f5015b66` softIoc, a `calcout` whose inputs are
//! `SRC.VAL[0]`, `SRC.VAL`, `SRC.EGU[0]`, `SRC.VAL{"dbnd":{"d":1}}` and
//! `SRC.{"dbnd":{"d":1}}`: `dbgf CO.INAV`..`CO.INEV` answer `"Local PV"` for
//! all five. C reaches that through `dbNameToAddr` on the name its own
//! channel parse has already split, which is the same question
//! `DbLink::target()` answers here.

use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::types::EpicsValue;

// menu(calcoutINAV) indices (calcoutRecord.dbd.pod:45-50).
const EXT_NC: i16 = 0;
const LOC: i16 = 2;

async fn poll_status(db: &PvDatabase, pv: &str, want: i16, label: &str) {
    for _ in 0..400 {
        if let Ok(v) = db.get_pv(pv)
            && v.to_f64().map(|f| f as i16) == Some(want)
        {
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!(
        "{label}: {pv} did not reach {want} before timeout (last {:?}, \
         {EXT_NC} is Ext PV NC)",
        db.get_pv(pv)
    );
}

fn read_status(db: &PvDatabase, pv: &str) -> i16 {
    db.get_pv(pv)
        .ok()
        .and_then(|v| v.to_f64())
        .map(|f| f as i16)
        .unwrap_or_else(|| panic!("{pv} not readable as a number"))
}

/// One case per filter shape the channel parser can hand back, because each
/// leaves a different pair of raw halves behind: `[range]` on the default
/// field, `[range]` on a named field, JSON after a named field, and JSON
/// after the bare separator.
#[epics_macros_rs::epics_test]
async fn every_filter_shape_still_reports_a_local_target() {
    let db = PvDatabase::new();
    db.add_record("FLT_LS_TGT", Box::new(AiRecord::new(3.0)))
        .await
        .unwrap();

    let mut co = CalcoutRecord::default();
    for (field, link) in [
        ("INPA", "FLT_LS_TGT.VAL[0]"),
        ("INPB", "FLT_LS_TGT.VAL"),
        ("INPC", "FLT_LS_TGT.EGU[0]"),
        ("INPD", r#"FLT_LS_TGT.VAL{"dbnd":{"d":1}}"#),
        ("INPE", r#"FLT_LS_TGT.{"dbnd":{"d":1}}"#),
    ] {
        co.put_field(field, EpicsValue::String(link.into()))
            .unwrap();
    }
    db.add_record("FLT_LS", Box::new(co)).await.unwrap();

    poll_status(&db, "FLT_LS.INAV", LOC, "`[0]` on the default field").await;
    for (pv, label) in [
        ("FLT_LS.INBV", "unfiltered, the control"),
        ("FLT_LS.INCV", "`[0]` on a named field"),
        ("FLT_LS.INDV", "JSON after a named field"),
        ("FLT_LS.INEV", "JSON after the bare separator"),
    ] {
        assert_eq!(read_status(&db, pv), LOC, "{label}: {pv}");
    }
}

/// The rule it must not swallow: a filter does not make a link local. The
/// same shapes on a record this IOC does not hold stay external.
#[epics_macros_rs::epics_test]
async fn a_filtered_link_to_a_missing_record_is_still_external() {
    let db = PvDatabase::new();
    let mut co = CalcoutRecord::default();
    for (field, link) in [
        ("INPA", "FLT_LS_ABSENT.VAL[0]"),
        ("INPB", "FLT_LS_ABSENT.VAL"),
    ] {
        co.put_field(field, EpicsValue::String(link.into()))
            .unwrap();
    }
    db.add_record("FLT_LS_EXT", Box::new(co)).await.unwrap();

    poll_status(&db, "FLT_LS_EXT.INAV", EXT_NC, "filtered, absent target").await;
    assert_eq!(
        read_status(&db, "FLT_LS_EXT.INBV"),
        EXT_NC,
        "unfiltered, absent target"
    );
}
