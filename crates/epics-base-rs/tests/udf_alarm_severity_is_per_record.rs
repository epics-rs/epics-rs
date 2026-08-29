//! B4-4 — `lso` and `mbbiDirect` raise UDF at a hard-coded `INVALID_ALARM`;
//! every other base record that raises UDF takes the severity from `UDFS`.
//!
//! Census of `std/rec/*.c`, re-run for this fix: 21 `UDF_ALARM` raise sites,
//! 19 pass `prec->udfs`, exactly two pass the literal:
//!
//! ```c
//! /* lsoRecord.c:117-118, mbbiDirectRecord.c:168-169 */
//! if (prec->udf)
//!     recGblSetSevr(prec, UDF_ALARM, INVALID_ALARM);
//!
//! /* mbboDirectRecord.c:191 */
//! recGblSetSevrMsg(prec, UDF_ALARM, prec->udfs, "UDFS");
//! ```
//!
//! Nothing derives the split — the Direct pair disagrees with itself, and
//! `stringout` (an output record like `lso`) passes `prec->udfs` — so it is a
//! per-record fact carried on `Record::udf_alarm_severity` and applied by the
//! single owner, `rec_gbl_check_udf`.
//!
//! The port took `UDFS` for all of them. Since `rec_gbl_set_sevr_msg` is
//! strict-greater, `UDFS=NO_ALARM` on an `lso` raised NOTHING: an undefined
//! record reported no alarm at all where C reports INVALID/UDF.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::rec_gbl_check_udf;
use epics_base_rs::server::record::{AlarmSeverity, CommonFields, Record};
use epics_base_rs::server::records::lso::LsoRecord;
use epics_base_rs::server::records::mbbi_direct::MbbiDirectRecord;
use epics_base_rs::server::records::mbbo_direct::MbboDirectRecord;
use epics_base_rs::server::records::stringout::StringoutRecord;
use epics_base_rs::types::EpicsValue;

const UDF_ALARM: u16 = 17;

/// Load `record` as `REC`, set `UDFS`, process it once, and report
/// `(SEVR, STAT)` — the trigger the brief names, `caput REC.PROC 1` on a
/// record left undefined.
async fn udf_alarm_after_process(record: Box<dyn Record>, udfs: &str) -> (AlarmSeverity, u16) {
    let db = PvDatabase::new();
    db.add_record("REC", record).await.unwrap();
    db.put_record_field_from_ca("REC", "UDFS", EpicsValue::String(udfs.into()))
        .await
        .unwrap();
    db.put_record_field_from_ca("REC", "PROC", EpicsValue::Long(1))
        .await
        .unwrap();
    let inst = db.get_record("REC").unwrap();
    let g = inst.read();
    assert_ne!(g.common.udf, 0, "the record must still be undefined");
    (g.common.sevr, g.common.stat)
}

/// `UDFS=NO_ALARM` is the case that produced no alarm at all: the port fed it
/// to a strict-greater raise, so nothing moved.
#[epics_macros_rs::epics_test]
async fn lso_udf_is_invalid_even_when_udfs_says_no_alarm() {
    assert_eq!(
        udf_alarm_after_process(Box::new(LsoRecord::default()), "NO_ALARM").await,
        (AlarmSeverity::Invalid, UDF_ALARM),
        "lsoRecord.c:117-118 passes INVALID_ALARM, so UDFS cannot silence it"
    );
}

#[epics_macros_rs::epics_test]
async fn lso_udf_is_invalid_even_when_udfs_says_minor() {
    assert_eq!(
        udf_alarm_after_process(Box::new(LsoRecord::default()), "MINOR").await,
        (AlarmSeverity::Invalid, UDF_ALARM),
        "UDFS cannot weaken lso's UDF alarm either"
    );
}

/// `mbbiDirect` is driven through the owner rather than through a process
/// cycle, because the four cases above are about the `evaluate_alarms` wiring
/// and a process cycle would also exercise the read-status gate that decides
/// whether the record is still undefined by the time `checkAlarms` runs. That
/// gate — C's `mbbiDirectRecord.c:155-166`, where the `udf = FALSE` sits
/// inside the `status == 0` arm — is pinned across the whole status-gated
/// family by `udf_survives_a_failed_soft_inp_read.rs`, `MBD` included.
#[epics_macros_rs::epics_test]
async fn mbbi_direct_udf_is_invalid_even_when_udfs_says_minor() {
    let rec = MbbiDirectRecord::default();
    let mut common = CommonFields {
        udfs: AlarmSeverity::Minor as i16,
        ..Default::default()
    };
    assert_ne!(common.udf, 0, "a fresh record is undefined");

    rec_gbl_check_udf(
        &mut common,
        rec.udf_alarm_on_exact_one(),
        rec.udf_alarm_severity(),
        rec.udf_alarm_message(),
    );

    assert_eq!(
        (common.nsev, common.nsta),
        (AlarmSeverity::Invalid, UDF_ALARM),
        "mbbiDirectRecord.c:168-169 passes INVALID_ALARM"
    );
}

/// The hook itself, for the two records C hard-codes and one it does not.
#[test]
fn only_lso_and_mbbi_direct_pin_the_udf_severity() {
    assert_eq!(
        LsoRecord::default().udf_alarm_severity(),
        Some(AlarmSeverity::Invalid)
    );
    assert_eq!(
        MbbiDirectRecord::default().udf_alarm_severity(),
        Some(AlarmSeverity::Invalid)
    );
    assert_eq!(
        MbboDirectRecord::default().udf_alarm_severity(),
        None,
        "mbboDirectRecord.c:191 passes prec->udfs"
    );
    assert_eq!(
        StringoutRecord::default().udf_alarm_severity(),
        None,
        "stringoutRecord.c:147 passes prec->udfs"
    );
}

/// The other half of the Direct pair keeps UDFS — the in/out split is real and
/// must survive this fix.
#[epics_macros_rs::epics_test]
async fn mbbo_direct_udf_follows_udfs() {
    assert_eq!(
        udf_alarm_after_process(Box::new(MbboDirectRecord::default()), "MINOR").await,
        (AlarmSeverity::Minor, UDF_ALARM),
        "mbboDirectRecord.c:191 passes prec->udfs"
    );
}

/// A representative of the 19-record majority, and an output record like
/// `lso`, so the exception cannot be mistaken for an output-record rule.
#[epics_macros_rs::epics_test]
async fn stringout_udf_follows_udfs() {
    assert_eq!(
        udf_alarm_after_process(Box::new(StringoutRecord::default()), "MINOR").await,
        (AlarmSeverity::Minor, UDF_ALARM),
        "stringoutRecord.c:147 passes prec->udfs"
    );
}
