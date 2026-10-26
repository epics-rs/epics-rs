//! StreamEpics.cc `getFieldAddress` of a `record.FIELD$` name: the
//! `dbNameToAddr` address of the field's characters (`dbAccess.c:681-701`),
//! which `formatValue` prints and `matchValue` puts as a CHAR array.

mod redirect;

use epics_base_rs::types::EpicsValue;
use redirect::{REPLIES, get, ioc, process};

#[test]
fn a_dollar_field_is_read_and_written_as_characters() {
    REPLIES.lock().push(("Q8", "bye"));
    epics_base_rs::runtime::task::test_block_on(async {
        let db = ioc(r#"
record(ai, "sd") { field(DESC, "hello") }
record(ai, "sd2") { field(DESC, "x") }
record(ao, "a") { field(DTYP, "stream") field(OUT, "@redirect.proto dollar_out R") }
record(ao, "b") { field(DTYP, "stream") field(OUT, "@redirect.proto dollar_in R") }
"#)
        .await;
        assert_eq!(process(&db, "a", 1).await, ["hello"]);
        process(&db, "b", 1).await;
        assert_eq!(get(&db, "sd2.DESC"), EpicsValue::String("bye".into()));
    });
}
