//! An lsi INP / lso DOL naming `<rec>.VAL$` reads the long string as
//! DBR_CHAR up to SIZV (`dbLink.c` dbGetLinkLS char arm), not 40-byte
//! DBR_STRING; pinned to oracleIoc R7.0.10.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const X: &str = "0123456789012345678901234567890123456789012345678901234567890123456789";

fn db_text() -> String {
    let mut t = String::new();
    for n in [70, 40, 39] {
        t.push_str(&format!(
            "record(lsi, \"S{n}\") {{ field(SIZV, \"100\") }}\n"
        ));
        for z in [100, 20] {
            t.push_str(&format!(
                "record(lsi, \"LI{n}-{z}\") {{ field(SIZV, \"{z}\") field(INP, \"S{n}.VAL$\") }}\n\
                 record(lso, \"LO{n}-{z}\") {{ field(SIZV, \"{z}\") field(DOL, \"S{n}.VAL$\") field(OMSL, \"closed_loop\") }}\n"
            ));
        }
    }
    t
}

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
}

fn text(db: &PvDatabase, rec: &str) -> String {
    match db.get_pv(format!("{rec}.VAL")).unwrap() {
        EpicsValue::CharArray(b) => String::from_utf8_lossy(&b)
            .trim_end_matches('\0')
            .to_owned(),
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        other => panic!("{rec}: {other:?}"),
    }
}

#[epics_macros_rs::epics_test]
async fn a_dollar_link_reads_the_long_string_up_to_sizv_like_softioc() {
    let db = IocBuilder::new()
        .db_string(&db_text(), &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    for n in [70usize, 40, 39] {
        db.put_record_field_from_ca(
            &format!("S{n}"),
            "VAL",
            EpicsValue::CharArray(X.as_bytes()[..n].into()),
        )
        .await
        .unwrap();
        for (z, len) in [(100usize, n + 1), (20, 20)] {
            for kind in ["LI", "LO"] {
                let rec = format!("{kind}{n}-{z}");
                process(&db, &rec).await;
                let got = db.get_pv(format!("{rec}.LEN")).unwrap().to_f64().unwrap();
                assert_eq!(got, len as f64, "{rec}.LEN");
                assert_eq!(text(&db, &rec), X[..len - 1], "{rec}.VAL");
            }
        }
    }
}

/// The other readers of a `$` source, and the fields `$` does and does not
/// apply to; pinned to softIoc R7.0.10.1 with S70 written by `dbpf S70.VAL$`.
/// DBR_STRING of the DBF_CHAR view is element 0 ("48"); `SD.DESC$` is the
/// 41-byte DESC as chars. `AI.VAL$` is refused by `dbChannelTest`, which
/// leaves C a CA link (its iocInit then never returns), so the read fails.
#[epics_macros_rs::epics_test]
async fn every_reader_of_a_dollar_link_sees_a_char_array_like_softioc() {
    let db_text = format!(
        "record(lsi, \"S70\") {{ field(SIZV, \"100\") }}\n\
         record(stringin, \"SI\") {{ field(INP, \"S70.VAL$\") }}\n\
         record(printf, \"PF\") {{ field(SIZV, \"100\") field(FMT, \"%ls\") field(INP0, \"S70.VAL$\") }}\n\
         record(printf, \"PF2\") {{ field(SIZV, \"100\") field(FMT, \"%s\") field(INP0, \"S70.VAL$\") }}\n\
         record(ai, \"SD\") {{ field(DESC, \"{}\") }}\n\
         record(lsi, \"LD\") {{ field(SIZV, \"100\") field(INP, \"SD.DESC$\") }}\n\
         record(lsi, \"LN\") {{ field(SIZV, \"100\") field(INP, \"S70\") }}\n\
         record(ai, \"AI\") {{ field(VAL, \"3\") }}\n\
         record(lsi, \"LA\") {{ field(SIZV, \"100\") field(INP, \"AI.VAL$\") }}\n",
        &X[..40]
    );
    let db = IocBuilder::new()
        .db_string(&db_text, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    db.put_record_field_from_ca("S70", "VAL", EpicsValue::CharArray(X.as_bytes().into()))
        .await
        .unwrap();
    let num = |pv: &str| db.get_pv(pv).unwrap().to_f64().unwrap();
    for rec in ["SI", "PF", "PF2", "LD", "LN", "LA"] {
        process(&db, rec).await;
    }
    assert_eq!(text(&db, "SI"), "48");
    assert_eq!((num("PF.LEN"), text(&db, "PF")), (71.0, X.to_owned()));
    assert_eq!((num("PF2.LEN"), text(&db, "PF2")), (3.0, "48".to_owned()));
    assert_eq!((num("LD.LEN"), text(&db, "LD")), (41.0, X[..40].to_owned()));
    assert_eq!((num("LN.LEN"), text(&db, "LN")), (40.0, X[..39].to_owned()));
    assert_eq!((num("LA.LEN"), num("LA.SEVR")), (0.0, 3.0));
    assert_eq!(num("SI.SEVR") + num("PF.SEVR") + num("LD.SEVR"), 0.0);
}
