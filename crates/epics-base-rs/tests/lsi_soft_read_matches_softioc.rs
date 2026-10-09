//! lsi soft INP read pinned to softIoc R7.0.10.1 (`devLsiSoft.c` is
//! `dbGetLinkLS`): a CHAR waveform source copies up to SIZV bytes, a string
//! source reads DBR_STRING (39 bytes), and LEN is `strlen + 1`. A VAL put on
//! the Passive record processes it (VAL is pp) and the INP read replaces it.
//! UDF is pinned by `soft_input_udf_follows_the_device`.
//!
//! ```text
//! lso W(61 chars) -> WF CHAR   L  SIZV 80 LEN 62   L2 SIZV 20 LEN 20
//!   (C's lso puts LEN = 62 bytes, the NUL included: dbPutLinkLS)
//! L3 INP -> stringin "abc"     init LEN 0 "" INVALID; proc LEN 4 "abc"
//!                              NO_ALARM; put "xy" -> "abc" LEN 4
//! ```

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const X: &str = "0123456789012345678901234567890123456789012345678901234567890";

const DB: &str = r#"
record(waveform, "WF") { field(FTVL, "CHAR") field(NELM, "100") }
record(lsi, "L") { field(SIZV, "80") field(INP, "WF") }
record(lsi, "L2") { field(SIZV, "20") field(INP, "WF") }
record(stringin, "SI") { field(VAL, "abc") }
record(lsi, "L3") { field(SIZV, "80") field(INP, "SI") }
"#;

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
}

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

fn text(db: &PvDatabase, rec: &str) -> String {
    match db.get_pv(&format!("{rec}.VAL")).unwrap() {
        EpicsValue::CharArray(b) => String::from_utf8_lossy(&b).into_owned(),
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        other => panic!("{rec}: {other:?}"),
    }
}

#[epics_macros_rs::epics_test]
async fn lsi_soft_read_matches_softioc() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let wf = [X.as_bytes(), b"\0"].concat();
    db.put_record_field_from_ca("WF", "VAL", EpicsValue::CharArray(wf.as_slice().into()))
        .await
        .unwrap();
    for (rec, len) in [("L", 62.0), ("L2", 20.0)] {
        process(&db, rec).await;
        assert_eq!(num(&db, &format!("{rec}.LEN")), len, "{rec}.LEN");
        assert_eq!(text(&db, rec), X[..len as usize - 1], "{rec}.VAL");
    }

    let state = |db: &PvDatabase| (num(db, "L3.LEN"), text(db, "L3"), num(db, "L3.SEVR"));
    assert_eq!(state(&db), (0.0, String::new(), 3.0), "init");
    process(&db, "L3").await;
    assert_eq!(state(&db), (4.0, "abc".into(), 0.0), "proc");
    db.put_record_field_from_ca("L3", "VAL", EpicsValue::String("xy".into()))
        .await
        .unwrap();
    assert_eq!(state(&db), (4.0, "abc".into(), 0.0), "put");
    process(&db, "L3").await;
    assert_eq!(state(&db), (4.0, "abc".into(), 0.0), "proc2");
}

/// `dbGetLinkLS`'s `pbuffer[--len] = 0` (`dbLink.c:521`) overwrites the last
/// char element read, on every reader of it: lsi INP and SIOL, lso DOL.
/// softIoc R7.0.10.1, SIZV 20, sources written with `caput -a`:
///
/// ```text
/// NORD 0 (VAL xyz)  -> xyz  LEN 4     abc..s (19) -> 18 chars LEN 19
/// h i j             -> hi   LEN 3     abc..v (22) -> 19 chars LEN 20
/// a b \0 d          -> ab   LEN 3     a b c d \0  -> abcd     LEN 5
/// a                 -> ""   LEN 1     INP W.UDF (scalar UCHAR) -> "" LEN 1
/// lso SIZV 100, DOL 70 chars + NUL -> LEN 71
/// ```
#[epics_macros_rs::epics_test]
async fn a_char_source_loses_its_last_read_byte_like_softioc() {
    const DB: &str = r#"
record(waveform, "W") { field(FTVL, "CHAR") field(NELM, "100") }
record(waveform, "WU") { field(FTVL, "UCHAR") field(NELM, "100") }
record(lsi, "L") { field(INP, "W") field(SIZV, "20") }
record(lsi, "LU") { field(INP, "WU") field(SIZV, "20") }
record(lsi, "LS") { field(INP, "W.UDF") field(SIZV, "20") }
record(lsi, "LM") { field(SIMM, "YES") field(SIOL, "W") field(SIZV, "20") }
record(lso, "O") { field(DOL, "W") field(OMSL, "closed_loop") field(SIZV, "20") }
record(lso, "O100") { field(DOL, "W") field(OMSL, "closed_loop") field(SIZV, "100") }
"#;
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let check = |db: &PvDatabase, want: &str, label: &str| {
        for rec in ["L", "LU", "LM", "O"] {
            let got = (text(db, rec), num(db, &format!("{rec}.LEN")));
            assert_eq!(
                got,
                (want.to_string(), want.len() as f64 + 1.0),
                "{label}: {rec}"
            );
        }
    };
    for rec in ["L", "LU", "LM", "O"] {
        db.put_record_field_from_ca(rec, "VAL", EpicsValue::String("xyz".into()))
            .await
            .unwrap();
        process(&db, rec).await;
    }
    check(&db, "xyz", "NORD 0");
    process(&db, "LS").await;
    assert_eq!(
        (text(&db, "LS"), num(&db, "LS.LEN")),
        (String::new(), 1.0),
        "LS"
    );

    let abc = |n: usize| (b'a'..).take(n).collect::<Vec<u8>>();
    let rows: [(Vec<u8>, &str); 6] = [
        (b"hij".to_vec(), "hi"),
        (abc(19), "abcdefghijklmnopqr"),
        (abc(22), "abcdefghijklmnopqrs"),
        (b"ab\0d".to_vec(), "ab"),
        (b"abcd\0".to_vec(), "abcd"),
        (b"a".to_vec(), ""),
    ];
    for (bytes, want) in rows {
        db.put_record_field_from_ca("W", "VAL", EpicsValue::CharArray(bytes.as_slice().into()))
            .await
            .unwrap();
        db.put_record_field_from_ca("WU", "VAL", EpicsValue::UCharArray(bytes.as_slice().into()))
            .await
            .unwrap();
        for rec in ["L", "LU", "LM", "O"] {
            process(&db, rec).await;
        }
        check(&db, want, &String::from_utf8_lossy(&bytes));
    }

    let seventy = [abc(70).as_slice(), b"\0"].concat();
    db.put_record_field_from_ca("W", "VAL", EpicsValue::CharArray(seventy.as_slice().into()))
        .await
        .unwrap();
    process(&db, "O100").await;
    assert_eq!(num(&db, "O100.LEN"), 71.0, "O100");
}
