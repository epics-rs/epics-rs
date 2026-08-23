//! `recGblInitSimm` does its work only for a CONSTANT SIML.
//!
//! ```c
//! /* recGbl.c:439-446 */
//! void recGblInitSimm(struct dbCommon *pcommon, epicsEnum16 *psscn,
//!     epicsEnum16 *poldsimm, epicsEnum16 *psimm, struct link *psiml) {
//!     if (dbLinkIsConstant(psiml)) {
//!         recGblSaveSimm(*psscn, poldsimm, *psimm);
//!         dbLoadLink(psiml, DBF_USHORT, psimm);
//!         recGblCheckSimm(pcommon, psscn, *poldsimm, *psimm);
//!     }
//! }
//! ```
//!
//! One guard, all three steps. Its twin `recGblGetSimm` (`:448-457`) has no
//! guard at all — that asymmetry is the point: at init a PV-valued SIML has
//! nothing to deliver yet, so C leaves OLDSIMM at its dbd initial and leaves
//! SCAN where the `.db` put it, and the first `recGblGetSimm` of the first
//! process cycle is what starts tracking the mode.
//!
//! The port ran the `recGblSaveSimm` latch and the `recGblCheckSimm` tail
//! outside the guard. The boundary axis is the SIML's link CLASS.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::ScanType;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;
use std::sync::Arc;

type Db = Arc<epics_base_rs::server::database::PvDatabase>;

/// `menuScan.dbd`: 0 Passive … 6 "1 second".
const SCAN_1_SECOND: u16 = 6;
const SCAN_PASSIVE: u16 = 0;

async fn build(db_text: &str) -> Db {
    IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn common(db: &Db, rec: &str, field: &str) -> Option<EpicsValue> {
    db.get_record(rec).unwrap().read().get_common_field(field)
}

fn scan(db: &Db, rec: &str) -> ScanType {
    db.get_record(rec).unwrap().read().common.scan
}

fn simm(db: &Db, rec: &str) -> Option<EpicsValue> {
    db.get_record(rec).unwrap().read().record.get_field("SIMM")
}

/// CONSTANT SIML: all three steps run. The latch takes the OUTGOING mode (NO),
/// the load moves SIMM to YES, and the resulting transition swaps SCAN with
/// SSCN before the IOC ever reaches runtime.
#[epics_macros_rs::epics_test]
async fn a_constant_siml_latches_loads_and_swaps() {
    let db = build(
        r#"record(longin, "C") { field(SIML, "1") field(SCAN, "1 second") field(SSCN, "Passive") }"#,
    )
    .await;

    assert_eq!(simm(&db, "C"), Some(EpicsValue::Short(1)), "dbLoadLink");
    assert_eq!(
        common(&db, "C", "OLDSIMM"),
        Some(EpicsValue::Short(0)),
        "recGblSaveSimm latched the mode the record was leaving"
    );
    assert_eq!(scan(&db, "C"), ScanType::Passive, "recGblCheckSimm swapped");
    assert_eq!(
        common(&db, "C", "SSCN"),
        Some(EpicsValue::Enum(SCAN_1_SECOND)),
        "and SSCN holds the scan the record left"
    );
}

/// PV-valued SIML: `dbLinkIsConstant` is false, so `recGblInitSimm` returns
/// having touched nothing — OLDSIMM keeps its dbd initial even though SIMM was
/// loaded YES from the `.db`.
#[epics_macros_rs::epics_test]
async fn a_pv_valued_siml_gets_none_of_the_three_steps() {
    let db = build(
        r#"
record(longin, "SRC") { field(VAL, "1") }
record(longin, "L") { field(SIML, "SRC.VAL") field(SIMM, "YES")
                      field(SCAN, "1 second") field(SSCN, "Passive") }
"#,
    )
    .await;

    assert_eq!(simm(&db, "L"), Some(EpicsValue::Short(1)), "the .db value");
    assert_eq!(
        common(&db, "L", "OLDSIMM"),
        Some(EpicsValue::Short(0)),
        "no recGblSaveSimm at init: OLDSIMM keeps its dbd initial"
    );
    assert_eq!(
        scan(&db, "L"),
        ScanType::Sec1,
        "no recGblCheckSimm at init: SCAN is where the .db put it"
    );
    assert_eq!(
        common(&db, "L", "SSCN"),
        Some(EpicsValue::Enum(SCAN_PASSIVE)),
        "and SSCN too"
    );
}

/// An UNSET SIML is a constant link (`dbConstLink.c`'s lset with a NULL
/// string), so the guard opens: the latch runs, `dbLoadLink` returns
/// `S_db_badField` and stores nothing, and SIMM == OLDSIMM leaves no
/// transition to swap.
#[epics_macros_rs::epics_test]
async fn an_unset_siml_is_constant_and_latches_without_loading() {
    let db = build(
        r#"record(longin, "U") { field(SIMM, "YES") field(SCAN, "1 second") field(SSCN, "Passive") }"#,
    )
    .await;

    assert_eq!(simm(&db, "U"), Some(EpicsValue::Short(1)), "unchanged");
    assert_eq!(
        common(&db, "U", "OLDSIMM"),
        Some(EpicsValue::Short(1)),
        "the latch ran and took the current mode"
    );
    assert_eq!(scan(&db, "U"), ScanType::Sec1, "no transition, no swap");
}
