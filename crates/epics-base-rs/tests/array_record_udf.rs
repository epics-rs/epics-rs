//! R15-81: waveform/aai/aao clear UDF unconditionally and never raise
//! UDF_ALARM.
//!
//! Their C `process()` clears UDF itself, on the line after `readValue` returns
//! and whatever status it returned:
//!
//!     status = readValue(prec);
//!     if (!pact && prec->pact) return 0;
//!     prec->pact = TRUE;
//!     prec->udf = FALSE;              /* waveformRecord.c:144, aaiRecord.c:174 */
//!
//! (aao: `if (!pact) { prec->udf = FALSE; ... }`, aaoRecord.c:164-165.) So a
//! FAILED simulation read still leaves the record DEFINED — the framework's
//! simulation tail was gating the clear on the SIOL fetch status, which is the
//! rule for the scalar records (`longinRecord.c:418` `if (status == 0)
//! prec->udf = FALSE;`) but not for these three.
//!
//! And none of the three has a `checkAlarms` or names UDF_ALARM at all — the
//! only severities they raise are SIMM_ALARM and SOFT_ALARM — so UDF must never
//! become an alarm on them, whatever it says.
//!
//! subArray is the counter-case and keeps the defaults: `prec->udf = !!status;
//! if (status) recGblSetSevr(prec, UDF_ALARM, prec->udfs);`
//! (subArrayRecord.c:148-150).
//!
//! Boundaries: SIOL read failed vs succeeded; UDF set vs UDF alarm raised; the
//! three unconditional kinds vs subArray.

use std::collections::HashSet;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{AlarmSeverity, Record};
use epics_base_rs::server::records::waveform::{ArrayKind, WaveformRecord};
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// SIMM=YES with a SIOL pointing at a record that does not exist: the
/// simulation read fails.
const DB: &str = r#"
record(aai, "SIM:AAI") {
    field(FTVL, "DOUBLE")
    field(NELM, "8")
    field(SIMM, "YES")
    field(SIOL, "NO:SUCH:RECORD")
}
record(aao, "SIM:AAO") {
    field(FTVL, "DOUBLE")
    field(NELM, "8")
    field(SIMM, "YES")
    field(SIOL, "NO:SUCH:RECORD")
}
record(waveform, "SIM:WF") {
    field(FTVL, "DOUBLE")
    field(NELM, "8")
    field(SIMM, "YES")
    field(SIOL, "NO:SUCH:RECORD")
}
"#;

/// A failed SIOL read must still clear UDF, and must not raise any alarm from
/// UDF (SIMM_ALARM at SIMS=NO_ALARM is the only thing C raises here).
#[epics_macros_rs::epics_test]
async fn failed_sim_read_still_clears_udf_on_the_array_kinds() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    for rec in ["SIM:AAI", "SIM:AAO", "SIM:WF"] {
        let handle = db.get_record(rec).unwrap();
        assert!(
            handle.read().common.udf != 0,
            "{rec}: undefined before the first process"
        );

        let mut visited = HashSet::new();
        db.process_record_with_links(rec, &mut visited, 0)
            .await
            .unwrap();

        let common = &handle.read().common;
        assert!(
            common.udf == 0,
            "{rec}: C `process` clears UDF after readValue whatever its status"
        );
        assert_ne!(
            common.stat,
            epics_base_rs::server::recgbl::alarm_status::UDF_ALARM,
            "{rec}: these records never raise UDF_ALARM (no checkAlarms in C)"
        );
    }
}

/// The declared rules, per kind. subArray is the exception on both.
#[test]
fn subarray_keeps_the_status_gated_udf_rules() {
    for (kind, unconditional, raises) in [
        (ArrayKind::Waveform, true, false),
        (ArrayKind::Aai, true, false),
        (ArrayKind::Aao, true, false),
        (ArrayKind::SubArray, false, true),
    ] {
        let mut rec = WaveformRecord::new(4, DbFieldType::Double);
        rec.kind = kind;
        assert_eq!(
            rec.clears_udf_unconditionally(),
            unconditional,
            "{kind:?}: C `prec->udf = FALSE` in process (waveform/aai/aao) vs \
             `prec->udf = !!status` (subArray)"
        );
        assert_eq!(
            rec.raises_udf_alarm(),
            raises,
            "{kind:?}: only subArrayRecord.c:150 calls recGblSetSevr(UDF_ALARM)"
        );
    }
}

/// A never-processed waveform carries the INITIAL UDF severity — not a
/// process-time UDF alarm.
///
/// The two are different mechanisms and only the second is waveform-specific:
/// `iocInit.c:521-523` gives EVERY record `SEVR = UDFS` while `udf && stat ==
/// UDF_ALARM` (and STAT is born `UDF`), whereas `recGblCheckUdf` — the
/// process-time raise that `waveformRecord.c` never calls — is what this record
/// type genuinely lacks. softIoc, `record(waveform,"UDFWF")` with no INP, never
/// processed: `STAT=UDF SEVR=INVALID UDF=1`.
#[epics_macros_rs::epics_test]
async fn an_undefined_waveform_carries_the_initial_udf_severity() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"record(waveform, "UDF:WF") { field(FTVL, "DOUBLE") field(NELM, "4") }"#,
            &std::collections::HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    let handle = db.get_record("UDF:WF").unwrap();
    assert!(handle.read().common.udf != 0, "never processed, so UDF");

    assert_eq!(
        db.get_pv("UDF:WF.SEVR").unwrap().to_f64(),
        Some(AlarmSeverity::Invalid as i32 as f64),
        "iocInit gives every never-processed record SEVR = UDFS (INVALID)"
    );
    assert_eq!(
        db.get_pv("UDF:WF.STAT").unwrap().to_f64(),
        Some(f64::from(
            epics_base_rs::server::recgbl::alarm_status::UDF_ALARM
        )),
        "STAT is born UDF (dbCommon.dbd initial(\"UDF\"))"
    );
    assert_eq!(
        db.get_pv("UDF:WF").unwrap(),
        EpicsValue::DoubleArray(vec![])
    );
}
