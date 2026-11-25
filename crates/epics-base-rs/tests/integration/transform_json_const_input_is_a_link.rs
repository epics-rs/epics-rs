//! `transformRecord.c:571` `no_inlink = plink->type == CONSTANT`: a
//! `{const:…}` input is a `JSON_LINK` (`dbStaticLib.c:2280-2287`), so the
//! channel counts as linked and the default Conditional mode leaves its CLCx
//! alone while the value does not change.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::transform::TransformRecord;
use epics_base_rs::types::EpicsValue;

fn a(db: &PvDatabase) -> f64 {
    db.get_record("T")
        .unwrap()
        .read()
        .record
        .get_field("A")
        .unwrap()
        .to_f64()
        .unwrap()
}

async fn process(db: &PvDatabase) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links("T", &mut visited)
        .await
        .unwrap();
}

#[epics_macros_rs::epics_test]
async fn a_json_const_input_is_linked_so_its_channel_does_not_calc() {
    let db = PvDatabase::new();
    let mut t = TransformRecord::default();
    t.put_field("INPA", EpicsValue::String("{const:2}".into()))
        .unwrap();
    t.put_field("CLCA", EpicsValue::String("A+1".into()))
        .unwrap();
    db.add_record("T", Box::new(t)).await.unwrap();

    let before = a(&db);
    process(&db).await;
    process(&db).await;
    assert_eq!(a(&db), before);
}
