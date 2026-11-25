//! C `dbRecordField` (`dbLexRoutines.c:1406-1415`) answers a `dbPutString`
//! refusal with `yyerror(NULL)`: the field is lost, the record is kept, and the
//! load fails. A dbCommon field the store refused on a NEW record used to
//! print a line and load anyway.

use std::collections::HashMap;

use epics_base_rs::server::database::{PvDatabase, RecordLoad};
use epics_base_rs::server::db_loader::{apply_fields, create_record, parse_db};
use epics_base_rs::types::EpicsValue;

/// `histogram` declares no INP (its input link is SVL), so the common-field
/// store refuses it after the menu and number checks have let it through.
const DB: &str = r#"
record(histogram, "R") {
    field(INP, "x")
    field(DESC, "kept")
}
"#;

#[epics_macros_rs::epics_test]
async fn a_new_record_keeps_itself_and_fails_the_load() {
    let db = PvDatabase::new();
    let def = parse_db(DB, &HashMap::new()).unwrap().remove(0);
    let mut rec = create_record(&def.record_type).unwrap();
    let mut common = Vec::new();
    apply_fields(&mut rec, &def.fields, &mut common).unwrap();
    let loaded = db
        .add_loaded_record(&def.name, rec, RecordLoad::from_common_fields(common))
        .await;
    assert!(loaded.is_err(), "a refused INP must fail the load");
    // The field is lost, the record and its other fields are not.
    assert_eq!(
        db.get_pv("R.DESC").unwrap(),
        EpicsValue::String("kept".into())
    );
}
