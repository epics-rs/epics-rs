//! StreamEpics.cc `matchValue` with a field address: the scanned value is
//! put to the redirected field, with dbPutField into another record (which
//! processes it), dbPut into the record itself or during `@init` (which
//! clears the other record's alarm instead), and a failed scan puts the
//! other record in CALC/INVALID alarm.

mod redirect;

use epics_base_rs::types::EpicsValue;
use redirect::{REPLIES, get, ioc, process};

fn f(v: EpicsValue) -> f64 {
    v.to_f64().unwrap()
}

#[test]
fn scanned_values_land_in_the_named_field() {
    REPLIES.lock().extend([
        ("Q1", "42"),
        ("Q2", "degC"),
        ("Q3", "hello"),
        ("Q4", "x"),
        ("Q5", "17"),
        ("Q6", "1000000000.5"),
    ]);
    epics_base_rs::runtime::task::test_block_on(async {
        let db = ioc(r#"
record(calc, "cnt") { field(CALC, "VAL+1") }
record(calc, "cnt2") { field(CALC, "VAL+1") }
record(ai, "t") { field(FLNK, "cnt") }
record(ai, "t2") { field(FLNK, "cnt2") }
record(ai, "t3") { field(TSE, "-2") }
record(waveform, "w2") { field(FTVL, "CHAR") field(NELM, "16") }
record(ao, "a") { field(DTYP, "stream") field(OUT, "@redirect.proto put_other R") }
record(ao, "b") { field(DTYP, "stream") field(OUT, "@redirect.proto put_own R") }
record(ao, "c") { field(DTYP, "stream") field(OUT, "@redirect.proto put_chars R") }
record(ao, "d") { field(DTYP, "stream") field(OUT, "@redirect.proto put_bad R") }
record(ao, "e") { field(DTYP, "stream") field(OUT, "@redirect.proto put_init R") }
record(ao, "g") { field(DTYP, "stream") field(OUT, "@redirect.proto put_time R") }
record(ao, "h") { field(DTYP, "stream") field(OUT, "@redirect.proto get_time R") }
"#)
        .await;

        // @init: dbPut, the other record not processed but its alarm cleared
        assert_eq!(f(get(&db, "t2")), 17.0);
        assert_eq!(f(get(&db, "cnt2")), 0.0);
        assert_eq!(f(get(&db, "t2.UDF")), 0.0);
        assert_eq!(f(get(&db, "t2.SEVR")), 0.0);
        assert_eq!(f(get(&db, "t2.STAT")), 0.0);

        // run time: dbPutField processes the other record
        process(&db, "a", 1).await;
        assert_eq!(f(get(&db, "t")), 42.0);
        assert_eq!(f(get(&db, "cnt")), 1.0);

        // this record's own field
        process(&db, "b", 1).await;
        assert_eq!(get(&db, "b.EGU").text_g(), "degC");

        // a string into a CHAR array
        process(&db, "c", 1).await;
        match get(&db, "w2") {
            EpicsValue::CharArray(a) => assert_eq!(&a[..], b"hello\0"),
            other => panic!("w2 = {other:?}"),
        }

        // no value scanned: the other record in CALC/INVALID, processed
        process(&db, "d", 1).await;
        assert_eq!(f(get(&db, "t.SEVR")), 3.0);
        assert_eq!(f(get(&db, "t.STAT")), 12.0);
        assert_eq!(f(get(&db, "cnt")), 2.0);

        // TIME written as unix seconds, read back the same
        process(&db, "g", 1).await;
        assert_eq!(process(&db, "h", 1).await, ["1000000000.5"]);
    });
}
