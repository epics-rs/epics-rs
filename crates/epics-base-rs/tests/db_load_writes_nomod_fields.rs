//! A `.db` may set a `special(SPC_NOMOD)` field; a runtime put may not.
//!
//! C splits the two writers. `dbLoadRecords` goes through dbStatic's
//! `dbPutString` (dbStaticLib.c:2570), which consults `special` for `SPC_CALC`
//! alone — SPC_NOMOD appears in that layer only at `dbLexRoutines.c:1285`, as a
//! filter excluding such fields from the misspelled-field GUESSER, never as a
//! refusal of a field the `.db` names outright. The refusal lives in
//! `dbPutSpecial` pass 0 (`S_db_noMod`, dbAccess.c:123-127), reached only from
//! the runtime `dbPutField`/`dbPut`.
//!
//! The port applied the runtime refusal to both paths, so every
//! `field(<SPC_NOMOD>, …)` directive was dropped with a stderr line and the
//! record kept the `.dbd` initial. The victims are every declared SPC_NOMOD
//! field a Rust record does not model itself: `sub`'s LA..LU, `sel`'s
//! LA..NLST, `scalcout`'s PA..MLST, `asyn`'s AINP/NORD/ERRS, `swait`'s VERS —
//! and `mca`'s SIOL/SIML, which is what made a simulated `mca` untestable.

use std::collections::HashMap;

use epics_base_rs::server::database::{PvDatabase, RecordLoad};
use epics_base_rs::server::db_loader::{apply_fields, create_record, parse_db};
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(sub, "SUBREC") {
    field(SNAM, "mySubroutine")
    field(LA, "17.5")
}
"#;

async fn load(db: &PvDatabase, text: &str) {
    for def in parse_db(text, &HashMap::new()).unwrap() {
        let mut rec = create_record(&def.record_type).unwrap();
        let mut common = Vec::new();
        apply_fields(&mut rec, &def.fields, &mut common).unwrap();
        db.add_loaded_record(&def.name, rec, RecordLoad::from_common_fields(common))
            .await
            .unwrap();
    }
}

/// `subRecord.dbd:338-342` declares `LA` as `DBF_DOUBLE` with
/// `special(SPC_NOMOD)`, and `SubRecord` does not model it, so the `.db` value
/// has to land in the declared-override store to be readable at all.
#[epics_macros_rs::epics_test]
async fn a_db_file_may_set_a_spc_nomod_field() {
    let db = PvDatabase::new();
    load(&db, DB).await;
    assert_eq!(
        db.get_pv("SUBREC.LA").unwrap(),
        EpicsValue::Double(17.5),
        "field(LA, \"17.5\") was dropped at load"
    );
}

/// The other half: `dbPutSpecial` still refuses the same field at runtime, so
/// relaxing the loader must not open a `caput` route to it.
#[epics_macros_rs::epics_test]
async fn a_runtime_put_to_a_spc_nomod_field_is_still_refused() {
    let db = PvDatabase::new();
    load(&db, DB).await;
    let err = db
        .put_pv("SUBREC.LA", EpicsValue::Double(99.0))
        .await
        .expect_err("SPC_NOMOD must refuse a runtime put");
    assert!(
        format!("{err:?}").contains("ReadOnlyField"),
        "expected S_db_noMod, got {err:?}"
    );
    assert_eq!(db.get_pv("SUBREC.LA").unwrap(), EpicsValue::Double(17.5));
}
