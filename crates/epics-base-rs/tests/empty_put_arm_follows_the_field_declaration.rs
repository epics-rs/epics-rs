//! A zero-length put picks its `dbPut` arm from the FIELD's declaration, not
//! from the shape of the value the field happens to be holding.
//!
//! C line numbers resolve at epics-base tag `R7.0.10`, not at this machine's
//! working tree (`R7.0.10-146-g8f5015b66`), where PR #944 puts `dbPut`'s header
//! 3 lines lower and its body, from the first `if (special)` on, 5 lower.
//!
//! `dbAccess.c:1350`:
//!
//! ```c
//!     if (nRequest>1 || paddr->pfldDes->special == SPC_DBADDR) {
//! ```
//!
//! Two independent tests, and `no_elements` is in neither — it appears only in
//! the ARRAY arm's clamp at `:1360`. The `SPC_DBADDR` disjunct is what sends a
//! zero-length request into the array arm, where `dbPutConvertRoutine` (`:1362`)
//! copies nothing, `put_array_info(paddr, 0)` (`:1367`) drops the valid length,
//! and `status` stays 0 — so the scalar arm's
//! `recGblSetSevr(precord, LINK_ALARM, INVALID_ALARM)` at `:1371` is unreachable
//! for such a field. `pfldDes` is the STATIC descriptor: `cvt_dbaddr` may
//! overwrite `paddr->special` (`lsiRecord.c:127-134` raises SPC_MOD/SPC_NOMOD)
//! and the branch does not consult that copy.
//!
//! One case per side of the disagreement between the declaration and the port's
//! storage, because that disagreement is what the value-shape probe got wrong:
//! `special(SPC_DBADDR)` stored as a scalar (`mbbo.VAL`,
//! `mbboRecord.dbd.pod:194`), and a field declared `special(SPC_DBADDR)` whose
//! `cvt_dbaddr` overwrites `paddr->special` with something else (`lsi.VAL`,
//! `lsiRecord.c:127-130`). The two agreeing cases are here as well so a fix
//! cannot close one side by breaking the other.
//!
//! Every put goes through `put_pv` — C's bare `dbPut`, which runs no `monitor()`
//! — so NSTA/NSEV are read exactly as `dbPut` left them, with no process cycle
//! in between to recompute them.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::db_loader::{DbFieldDef, apply_fields, create_record};
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::{EpicsValue, PvString};

async fn load(db: &PvDatabase, name: &str, rtype: &str, fields: &[(&str, &str)]) {
    let mut rec = create_record(rtype).unwrap();
    let parsed: Vec<DbFieldDef> = fields
        .iter()
        .map(|(k, v)| DbFieldDef::new(*k, PvString::from(*v)))
        .collect();
    let mut common = vec![];
    apply_fields(&mut rec, &parsed, &mut common).unwrap();
    db.add_record(name, rec).await.unwrap();
    for (k, v) in common {
        db.put_pv(&format!("{name}.{k}"), v).await.unwrap();
    }
    db.ioc_init().await;
}

/// `(NSTA, NSEV)` — the pending alarm `recGblSetSevr` writes, before any
/// `recGblResetAlarms` promotes it to STAT/SEVR.
fn pending_alarm(db: &PvDatabase, name: &str) -> (u16, u16) {
    let inst = db.get_record(name).unwrap();
    let g = inst.read();
    match (
        g.get_common_field("NSTA").unwrap(),
        g.get_common_field("NSEV").unwrap(),
    ) {
        (EpicsValue::Short(s), EpicsValue::Short(v)) => (s as u16, v as u16),
        other => panic!("NSTA/NSEV are DBF_MENU shorts, got {other:?}"),
    }
}

fn field(db: &PvDatabase, name: &str, f: &str) -> Option<EpicsValue> {
    db.get_record(name).unwrap().read().record.get_field(f)
}

/// `special(SPC_DBADDR)` stored as a SCALAR. C takes the array arm on the
/// declaration alone, converts zero elements — so `mbbo.VAL` keeps the value
/// it had — and never reaches `:1371`.
#[epics_macros_rs::epics_test]
async fn a_dbaddr_field_the_port_stores_as_a_scalar_takes_the_array_arm() {
    let db = PvDatabase::new();
    load(&db, "M", "mbbo", &[("VAL", "3")]).await;

    db.put_pv("M.VAL", EpicsValue::DoubleArray(vec![]))
        .await
        .expect("C `dbPut` returns 0 for a zero-length request");

    assert_eq!(
        field(&db, "M", "VAL"),
        Some(EpicsValue::UShort(3)),
        "C converted zero elements into VAL, so the stored value stands"
    );
    assert_eq!(
        pending_alarm(&db, "M"),
        (alarm_status::NO_ALARM, AlarmSeverity::NoAlarm as u16),
        "`mbbo.VAL` is special(SPC_DBADDR), so `dbPut` never reaches the \
         scalar arm's recGblSetSevr at :1371"
    );
}

/// `special(SPC_DBADDR)` stored as a Vec. Same arm; here the port models
/// `put_array_info(paddr, 0)` by storing the empty array, so NORD follows.
#[epics_macros_rs::epics_test]
async fn a_dbaddr_field_the_port_stores_as_a_vec_drops_its_valid_length() {
    let db = PvDatabase::new();
    load(&db, "W", "waveform", &[("FTVL", "DOUBLE"), ("NELM", "10")]).await;
    db.put_pv("W.VAL", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]))
        .await
        .unwrap();

    db.put_pv("W.VAL", EpicsValue::DoubleArray(vec![]))
        .await
        .expect("C `dbPut` returns 0 for a zero-length request");

    assert_eq!(
        field(&db, "W", "NORD"),
        Some(EpicsValue::ULong(0)),
        "`put_array_info(paddr, 0)` (:1367) drops the valid length to zero"
    );
    assert_eq!(
        pending_alarm(&db, "W"),
        (alarm_status::NO_ALARM, AlarmSeverity::NoAlarm as u16),
        "the array arm raises no alarm"
    );
}

/// A plain scalar field: the arm C's commit `12cfd418d` added, which SETS the
/// target to LINK/INVALID and still returns success.
#[epics_macros_rs::epics_test]
async fn a_plain_scalar_field_takes_the_alarm_arm() {
    let db = PvDatabase::new();
    load(&db, "A", "ai", &[("VAL", "7")]).await;

    db.put_pv("A.VAL", EpicsValue::DoubleArray(vec![]))
        .await
        .expect("`dbPut` returns 0 — the alarm is the effect, not a refusal");

    assert_eq!(
        field(&db, "A", "VAL"),
        Some(EpicsValue::Double(7.0)),
        "nothing is written on this arm either"
    );
    assert_eq!(
        pending_alarm(&db, "A"),
        (alarm_status::LINK_ALARM, AlarmSeverity::Invalid as u16),
        "`recGblSetSevr(precord, LINK_ALARM, INVALID_ALARM)` (:1371)"
    );
}

/// The long-string case, which is where the declaration and
/// [`FieldDesc::special`] disagree in the OTHER direction: `lsi.VAL` declares
/// `special(SPC_DBADDR)` (`lsiRecord.dbd.pod:56-60`), but `cvt_dbaddr` raises
/// `paddr->special` to `SPC_MOD` (`lsiRecord.c:127-130`) and the port's `.dbd`
/// table records that runtime special rather than the declaration
/// (`dbd/cvt_dbaddr.types:67-70`). `dbPut` reads `pfldDes`, so C still takes
/// the array arm — a predicate that consulted `FieldDesc::special` alone would
/// hand these five fields a LINK/INVALID the C IOC does not raise.
#[epics_macros_rs::epics_test]
async fn a_long_string_field_takes_the_array_arm_despite_its_runtime_special() {
    let db = PvDatabase::new();
    load(&db, "S", "lsi", &[("SIZV", "40")]).await;

    db.put_pv("S.VAL", EpicsValue::CharArray(vec![]))
        .await
        .expect("`dbPut` returns 0");

    assert_eq!(
        pending_alarm(&db, "S"),
        (alarm_status::NO_ALARM, AlarmSeverity::NoAlarm as u16),
        "`lsi.VAL` is declared special(SPC_DBADDR); the SPC_MOD its cvt_dbaddr          installs is not what `dbPut`'s branch reads"
    );
}
