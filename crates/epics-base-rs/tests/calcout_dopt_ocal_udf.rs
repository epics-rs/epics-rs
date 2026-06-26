//! calcout DOPT=Use_OVAL: a NaN OVAL must raise UDF_ALARM / INVALID.
//!
//! C `calcoutRecord.c::execOutput:620-628`: on a DOPT=Use_OVAL output cycle a
//! successful OCAL `calcPerform` sets `prec->udf = isnan(prec->oval)`, then
//! `if (prec->udf) recGblSetSevr(UDF_ALARM, udfs)`. So a *finite* VAL with a
//! NaN OVAL still goes INVALID, and IVOA gates the OUT write. Before the fix
//! the Rust udf was VAL-based only (`value_is_undefined` default), so OCAL→NaN
//! drove NaN to the OUT link with NO_ALARM — a silent-wrong-value divergence.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::{AlarmSeverity, Record};
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::types::EpicsValue;

#[tokio::test]
async fn calcout_dopt_use_ocal_nan_oval_raises_udf_alarm() {
    let db = PvDatabase::new();

    let mut co = CalcoutRecord::default();
    // CALC="1" → VAL = 1 (finite, defined). OCAL="0/0" → OVAL = NaN.
    co.put_field("CALC", EpicsValue::String("1".into()))
        .unwrap();
    co.put_field("OCAL", EpicsValue::String("0/0".into()))
        .unwrap();
    co.dopt = 1; // Use OCAL (DOPT=Use_OVAL)
    co.oopt = 0; // Every Time → always an output cycle
    db.add_record("CO_OCAL_NAN", Box::new(co)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("CO_OCAL_NAN", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("CO_OCAL_NAN").await.unwrap();
    let inst = rec.read().await;

    // VAL is finite, OVAL is NaN → C raises UDF_ALARM at INVALID severity.
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Invalid,
        "a NaN OVAL on a Use_OVAL output cycle must raise INVALID (was NO_ALARM \
         before the fix — NaN silently driven to OUT)"
    );
    assert_eq!(
        inst.common.stat,
        alarm_status::UDF_ALARM,
        "STAT must be UDF (the OVAL is undefined), not NO_ALARM"
    );
    // The value that would have been silently written is indeed NaN.
    assert!(
        matches!(inst.record.get_field("OVAL"), Some(EpicsValue::Double(v)) if v.is_nan()),
        "OVAL must be NaN (OCAL = 0/0)"
    );

    // VAL itself stays finite and defined — the udf came from OVAL, not VAL.
    assert_eq!(inst.record.get_field("VAL"), Some(EpicsValue::Double(1.0)));
}
