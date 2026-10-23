//! StreamEpics.cc `formatValue` with a field address: the value printed is
//! the redirected field's, read with dbGet in the format's type, and a
//! `%s` of a CHAR array prints the array as one string.

mod redirect;

use epics_base_rs::types::EpicsValue;
use redirect::{ioc, process};

#[test]
fn field_formats_print_the_named_field() {
    epics_base_rs::runtime::task::test_block_on(async {
        let db = ioc(r#"
record(ao, "src") { field(VAL, "7") }
record(waveform, "w") { field(FTVL, "CHAR") field(NELM, "16") }
record(ao, "a") { field(DTYP, "stream") field(OUT, "@redirect.proto dotted R") }
record(ao, "b") { field(DTYP, "stream") field(OUT, "@redirect.proto own R") field(PREC, "3") }
record(ao, "c") { field(DTYP, "stream") field(OUT, "@redirect.proto bare R") }
record(ao, "d") { field(DTYP, "stream") field(OUT, "@redirect.proto own_egu R") field(EGU, "mm") }
record(ao, "e") { field(DTYP, "stream") field(OUT, "@redirect.proto chars R") }
"#)
        .await;
        db.put_pv("w", EpicsValue::CharArray(b"hello".to_vec().into()))
            .await
            .unwrap();
        let mut got = Vec::new();
        for r in ["a", "b", "c", "d", "e"] {
            got.push(process(&db, r, 1).await.join("|"));
        }
        assert_eq!(got, ["7", "3", "7.0", "mm", "hello"]);
    });
}
