//! StreamEpics.cc `getFieldAddress`: a `%(record.FIELD)`, `%(FIELD)` of the
//! record itself, or `%(record)` (its VAL) format compiles, so the record
//! initializes and its protocol runs.

mod redirect;

use redirect::{ioc, process};

#[test]
fn field_formats_compile_and_their_protocols_run() {
    epics_base_rs::runtime::task::test_block_on(async {
        let db = ioc(r#"
record(ao, "src") { field(VAL, "7") }
record(ao, "a") { field(DTYP, "stream") field(OUT, "@redirect.proto dotted R") }
record(ao, "b") { field(DTYP, "stream") field(OUT, "@redirect.proto own R") field(PREC, "3") }
record(ao, "c") { field(DTYP, "stream") field(OUT, "@redirect.proto bare R") }
"#)
        .await;
        for r in ["a", "b", "c"] {
            assert_eq!(process(&db, r, 1).await.len(), 1, "{r} wrote nothing");
        }
    });
}
