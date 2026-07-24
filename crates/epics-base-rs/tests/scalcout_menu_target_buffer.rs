//! R15-64 — a scalcout OUT link to a `DBF_MENU` / `DBF_DEVICE` target is put as
//! `DBR_STRING` from OSV, like STRING/ENUM/link targets.
//!
//! C `devsCalcoutSoft.c:128-130` switches the OUT put on the TARGET field's DBF
//! class, and sends the string result for seven of them:
//!
//! ```c
//! case DBF_STRING: case DBF_ENUM: case DBF_MENU: case DBF_DEVICE:
//! case DBF_INLINK: case DBF_OUTLINK: case DBF_FWDLINK:
//!     status = dbPutLink(&pscalcout->out, DBR_STRING, &pscalcout->osv, 1);
//!     break;
//! default:  /* … DBR_DOUBLE from OVAL */
//! ```
//!
//! The port's `DbFieldType` is a DBR wire type with no `Menu`/`Device` variant,
//! so a switch on it alone caught only STRING and ENUM: a menu target (`PRIO`,
//! `STAT`, `SEVR`, `DISS`, `ACKT`) — whose index the port stores as a short —
//! and `DTYP` fell through to the numeric arm and received OVAL. The class is
//! now settled at target resolution (`OutTarget::puts_as_string`).

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::types::EpicsValue;

/// A scalcout driving `out`. `DOPT` picks which result the OUT put stages:
/// 1 = "Use OCAL" (the string result OSV = the menu label "HIGH", numeric OVAL
/// = 0 — a string expression has no numeric value), 0 = "Use CALC" (OVAL = VAL
/// = 7). The C device support then chooses which buffer actually goes on the
/// wire from the TARGET's DBF class, which is what this file is about.
async fn add_scalcout(db: &PvDatabase, out: &str, dopt: i16) {
    let mut rec = ScalcoutRecord::new();
    rec.put_field("CALC", EpicsValue::String("7".into()))
        .unwrap();
    rec.put_field("OCAL", EpicsValue::String("\"HIGH\"".into()))
        .unwrap();
    rec.special("CALC", true).unwrap();
    rec.special("OCAL", true).unwrap();
    rec.put_field("DOPT", EpicsValue::Short(dopt)).unwrap();
    rec.put_field("OUT", EpicsValue::String(out.into()))
        .unwrap();
    db.add_record("SC", Box::new(rec)).await.unwrap();
}

async fn process(db: &PvDatabase, name: &str) {
    let mut v = HashSet::new();
    db.process_record_with_links(name, &mut v, 0).await.unwrap();
}

/// Boundary 1 — `DBF_MENU` target: `TGT.PRIO` is `menu(menuPriority)`
/// (LOW/MEDIUM/HIGH). C puts the OSV string, so the label resolves to index 2.
/// The pre-fix port took the numeric arm and sent OVAL — 0.0 here, i.e. `LOW`.
#[epics_macros_rs::epics_test]
async fn r15_64_menu_target_receives_the_osv_label() {
    let db = PvDatabase::new();
    db.add_record("TGT", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    add_scalcout(&db, "TGT.PRIO", 1).await; // Use OCAL: OSV="HIGH", OVAL=0.0

    process(&db, "SC").await;

    let prio = db.get_record("TGT").unwrap().read().common.prio;
    assert_eq!(
        prio, 2,
        "a DBF_MENU target takes DBR_STRING from OSV — the label \"HIGH\" \
         resolves to menuPriority index 2 (devsCalcoutSoft.c:128-130)"
    );
}

/// Boundary 2 — the numeric target is unchanged: C's `default:` arm puts
/// `DBR_DOUBLE` from OVAL (`devsCalcoutSoft.c:140`).
#[epics_macros_rs::epics_test]
async fn r15_64_numeric_target_still_receives_oval() {
    let db = PvDatabase::new();
    db.add_record("TGT", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    add_scalcout(&db, "TGT", 0).await; // Use CALC: OVAL = VAL = 7.0

    process(&db, "SC").await;

    assert_eq!(
        db.get_pv("TGT").unwrap(),
        EpicsValue::Double(7.0),
        "a DBF_DOUBLE target still takes OVAL"
    );
}
