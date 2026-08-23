//! R3-5: a B0..B1F put on an mbboDirect is refused while OMSL=closed_loop.
//!
//! C `mbboDirectRecord.c::special` (263-269), the pre-store pass:
//!
//! ```c
//!     if(after==0 && fieldIndex >= mbboDirectRecordB0
//!                 && fieldIndex <= mbboDirectRecordB1F) {
//!         if(prec->omsl == menuOmslclosed_loop) {
//!             /* To avoid confusion, reject changes to bit fields while in
//!              * closed loop. */
//!             return S_db_noMod;
//!         }
//!     }
//! ```
//!
//! `dbPut` returns that status before storing anything (`dbAccess.c:1350-1352`,
//! `if (special) { status = dbPutSpecial(paddr, 0); if (status) return status; }`),
//! so neither the bit nor the VAL it would recompute moves. The port stored the
//! bit and let `bits_to_val()` overwrite the closed-loop setpoint until the next
//! process cycle put it back.
//!
//! The invariant: **while OMSL=closed_loop, no put route may write a B field.**
//! The boundaries are the two OMSL values, the B/non-B field split, a live OMSL
//! flip (C reads `prec->omsl` at put time, not at channel-open time), and each
//! put route — the CA route and the no-process autosave-restore route, which is
//! C's `dbPutField` under `reboot_restore` and runs the same pass-0 special.

use std::collections::HashSet;

use epics_base_rs::error::CaError;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "SRC") {
    field(VAL, "5")
}
record(mbboDirect, "CL") {
    field(NOBT, "8")
    field(OMSL, "closed_loop")
    field(DOL, "SRC")
}
record(mbboDirect, "SUP") {
    field(NOBT, "8")
    field(OMSL, "supervisory")
}
"#;

async fn build() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn field(db: &PvDatabase, rec: &str, f: &str) -> EpicsValue {
    let inst = db.get_record(rec).unwrap();
    let g = inst.read();
    g.record.get_field(f).unwrap()
}

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

/// The refusal itself: the put FAILS, and because C returns before the store,
/// both the bit and VAL are exactly where the closed-loop cycle left them.
#[epics_macros_rs::epics_test]
async fn closed_loop_refuses_a_bit_put_and_stores_nothing() {
    let db = build().await;
    process(&db, "CL").await;
    // DOL=5 -> VAL=5 -> B0=1 B1=0 B2=1.
    assert_eq!(field(&db, "CL", "VAL").await, EpicsValue::Long(5));
    assert_eq!(field(&db, "CL", "B1").await, EpicsValue::UChar(0));

    let err = db
        .put_record_field_from_ca("CL", "B1", EpicsValue::UChar(1))
        .await
        .expect_err("C returns S_db_noMod from the pass-0 special");
    assert!(
        matches!(err, CaError::ReadOnlyField(ref f) if f == "B1"),
        "S_db_noMod maps to ECA_NOWTACCESS; got {err:?}"
    );

    assert_eq!(
        field(&db, "CL", "B1").await,
        EpicsValue::UChar(0),
        "the store never ran, so the bit is untouched"
    );
    assert_eq!(
        field(&db, "CL", "VAL").await,
        EpicsValue::Long(5),
        "and bitsToVAL never ran, so the closed-loop setpoint stands"
    );
}

/// The other side of the OMSL boundary: supervisory accepts the identical put,
/// stores the bit and recomputes VAL (C's `after==1` arm, `:271-291`).
#[epics_macros_rs::epics_test]
async fn supervisory_accepts_the_same_bit_put() {
    let db = build().await;
    db.put_record_field_from_ca("SUP", "B1", EpicsValue::UChar(1))
        .await
        .expect("C's pass-0 refusal is gated on OMSL=closed_loop only");
    assert_eq!(field(&db, "SUP", "B1").await, EpicsValue::UChar(1));
    assert_eq!(field(&db, "SUP", "VAL").await, EpicsValue::Long(2));
}

/// The field boundary: C's arm is `fieldIndex >= B0 && fieldIndex <= B1F`, so a
/// closed-loop VAL put is NOT refused — only the bit fields are. Checked on the
/// no-process route so the store is observable; VAL is `pp(TRUE)`, so the CA
/// route accepts the put and the closed-loop cycle it triggers then re-drives
/// VAL from DOL, which is C's behaviour and not what this boundary is about.
#[epics_macros_rs::epics_test]
async fn closed_loop_still_accepts_a_val_put() {
    let db = build().await;
    db.put_pv_no_process("CL.VAL", EpicsValue::Long(9))
        .await
        .expect("VAL is outside C's B0..B1F index range");
    assert_eq!(field(&db, "CL", "VAL").await, EpicsValue::Long(9));

    db.put_record_field_from_ca("CL", "VAL", EpicsValue::Long(9))
        .await
        .expect("and the CA route accepts it too");
}

/// C reads `prec->omsl` inside `special()`, at put time — so the refusal
/// follows a live OMSL change with no reconnect.
#[epics_macros_rs::epics_test]
async fn the_refusal_follows_a_live_omsl_change() {
    let db = build().await;
    db.put_record_field_from_ca("SUP", "B2", EpicsValue::UChar(1))
        .await
        .unwrap();
    assert_eq!(field(&db, "SUP", "VAL").await, EpicsValue::Long(4));

    db.put_record_field_from_ca("SUP", "OMSL", EpicsValue::Short(1))
        .await
        .unwrap();
    db.put_record_field_from_ca("SUP", "B3", EpicsValue::UChar(1))
        .await
        .expect_err("the same channel is refused once OMSL flips");
    assert_eq!(
        field(&db, "SUP", "VAL").await,
        EpicsValue::Long(4),
        "and VAL is unchanged by the refused put"
    );
}

/// The no-process route (`put_pv_no_process`) is C's `dbPutField` under
/// autosave `reboot_restore`, which runs the same `dbPutSpecial(paddr, 0)`.
/// It was the one `dbPut` body in the port that skipped the pre-store pass.
#[epics_macros_rs::epics_test]
async fn the_autosave_restore_route_is_refused_too() {
    let db = build().await;
    process(&db, "CL").await;

    db.put_pv_no_process("CL.B1", EpicsValue::UChar(1))
        .await
        .expect_err("dbPutField runs dbPutSpecial(paddr, 0) like every dbPut");
    assert_eq!(field(&db, "CL", "B1").await, EpicsValue::UChar(0));
    assert_eq!(field(&db, "CL", "VAL").await, EpicsValue::Long(5));

    // Control: the same route on a supervisory record still works.
    db.put_pv_no_process("SUP.B1", EpicsValue::UChar(1))
        .await
        .unwrap();
    assert_eq!(field(&db, "SUP", "VAL").await, EpicsValue::Long(2));
}
