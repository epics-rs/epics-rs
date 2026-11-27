//! A DB OUT link puts through its cached target address — C `dbDbPutValue`
//! → `dbPut` on the link's `dbAddr` — and the tails that need the target's
//! name get the canonical one even when the link names an alias. A link whose
//! target is not a local record keeps the name route.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::ScanType;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;
use std::sync::Arc;

const DB: &str = r#"
record(longout, "SRC") { field(VAL, "7") field(OUT, "NICK NPP") }
record(longin, "DST") { alias("NICK") }
record(longout, "SS") { field(VAL, "6") field(OUT, "TICK:NICK.SCAN NPP") }
record(calc, "TICK") { field(CALC, "VAL+1") alias("TICK:NICK") }
record(longout, "TOSPV") { field(VAL, "5") field(OUT, "SPV NPP") }
"#;

type Db = Arc<epics_base_rs::server::database::PvDatabase>;

async fn build() -> Db {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    db.add_pv("SPV", EpicsValue::Long(0)).await.unwrap();
    db
}

fn f64_of(db: &Db, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

/// softIoc: `dbpr DST` after processing SRC shows VAL 7 — the alias is the
/// record — and NPP leaves DST unprocessed.
#[epics_macros_rs::epics_test]
async fn an_out_link_through_an_alias_writes_the_record() {
    let db = build().await;
    db.process_record("SRC").await.unwrap();
    assert_eq!(f64_of(&db, "DST"), 7.0);
    assert_eq!(f64_of(&db, "NICK"), 7.0);
}

/// softIoc: SS writes 6 ("1 second") into TICK's SCAN through an alias and
/// `scanppl` lists TICK — the scan-list move took the canonical record, not
/// the alias name the link carries.
#[epics_macros_rs::epics_test]
async fn a_scan_put_through_an_alias_moves_the_canonical_record() {
    let db = build().await;
    db.process_record("SS").await.unwrap();
    assert_eq!(db.get_pv("TICK.SCAN").unwrap(), EpicsValue::Enum(6));
    assert_eq!(db.records_for_scan(ScanType::Menu(6)).await, ["TICK"]);
}

/// A target that is not a record has no cached address; the put goes by name.
#[epics_macros_rs::epics_test]
async fn an_out_link_to_a_simple_pv_still_writes_it() {
    let db = build().await;
    db.process_record("TOSPV").await.unwrap();
    assert_eq!(f64_of(&db, "SPV"), 5.0);
}
