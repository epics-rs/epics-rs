//! `dbPut`'s UDF clear is AFTER the value branch joins, so a request that
//! converted zero elements clears UDF exactly as a stored value does.
//!
//! C line numbers resolve at epics-base tag `R7.0.10`, not at this machine's
//! working tree (`R7.0.10-146-g8f5015b66`), where PR #944 puts `dbPut`'s header
//! 3 lines lower and its body, from the first `if (special)` on, 5 lower.
//!
//! ```c
//! 1391      }                       /* the value branch joins here */
//! ...
//! 1404      if (status) goto done;
//! ...
//! 1409      isValueField = dbIsValueField(pfldDes);
//! 1410      if (isValueField) precord->udf = FALSE;
//! ```
//!
//! Nothing between `:1391` and `:1410` asks which arm ran, and `status` is 0 on
//! both — the scalar arm's `recGblSetSevr` at `:1371` sets an alarm, not a
//! status. The port cleared UDF only where a value was actually stored, on a
//! comment that read the C backwards, so a zero-length `caput -a` left the
//! record born-UDF and its next process cycle republished UDF/INVALID where the
//! C IOC publishes the LINK/INVALID the put itself raised — or, on the array
//! arm, NO_ALARM.
//!
//! One case per arm, plus the negative that keeps the `if (status)` gate
//! honest: a put whose after-put `special()` fails takes `:1404`'s `goto done`
//! and must NOT clear UDF.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::db_loader::{DbFieldDef, apply_fields, create_record};
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

fn udf(db: &PvDatabase, name: &str) -> u8 {
    match db
        .get_record(name)
        .unwrap()
        .read()
        .get_common_field("UDF")
        .unwrap()
    {
        EpicsValue::UChar(v) => v,
        other => panic!("UDF is DBF_UCHAR, got {other:?}"),
    }
}

/// The SCALAR arm. C sets LINK/INVALID at `:1371` and still falls through to
/// `:1410`, because the alarm is not a status.
#[epics_macros_rs::epics_test]
async fn a_zero_length_put_into_a_scalar_still_clears_udf() {
    let db = PvDatabase::new();
    load(&db, "A", "ai", &[]).await;
    assert_eq!(udf(&db, "A"), 1, "a record with no value is born UDF");

    db.put_pv("A.VAL", EpicsValue::DoubleArray(vec![]))
        .await
        .expect("`dbPut` returns 0");

    assert_eq!(
        udf(&db, "A"),
        0,
        "`:1410` is past the branch join and `status` is 0 on the alarm arm too"
    );
}

/// The ARRAY arm, on a `special(SPC_DBADDR)` field the port stores as a scalar
/// — the arm the value-shape probe used to miss entirely.
#[epics_macros_rs::epics_test]
async fn a_zero_length_put_into_a_dbaddr_scalar_clears_udf() {
    let db = PvDatabase::new();
    load(&db, "M", "mbbo", &[]).await;
    assert_eq!(udf(&db, "M"), 1, "a record with no value is born UDF");

    db.put_pv("M.VAL", EpicsValue::DoubleArray(vec![]))
        .await
        .expect("`dbPut` returns 0");

    assert_eq!(udf(&db, "M"), 0, "the array arm reaches `:1410` as well");
}

/// The `if (status) goto done;` at `:1404`. `calcRecord::special` refuses an
/// uncompilable CALC, so the put returns non-zero and never reaches the clear.
#[epics_macros_rs::epics_test]
async fn a_put_the_after_special_rejects_leaves_udf_alone() {
    let db = PvDatabase::new();
    load(&db, "C", "calc", &[]).await;
    assert_eq!(udf(&db, "C"), 1, "a record with no value is born UDF");

    db.put_pv("C.CALC", EpicsValue::String("A+".into()))
        .await
        .expect_err("an uncompilable CALC is S_db_badField from dbPutSpecial(paddr, 1)");

    assert_eq!(
        udf(&db, "C"),
        1,
        "`:1404` jumps past the clear on a non-zero status"
    );
}
