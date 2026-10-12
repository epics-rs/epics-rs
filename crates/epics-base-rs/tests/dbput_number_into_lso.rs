//! Spec §5 test 6: a numeric OUT put into lso VAL stores the `cvt_d_st` text
//! (precision 6: lso has no `get_precision`) and posts LEN from the put, not
//! VAL (`pp(TRUE)`); a PP link's process then posts LEN and VAL again. The
//! source takes no LINK alarm. Pinned to oracleStdIoc (C ground truth §5.6,
//! its DB verbatim).

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(lso,"LS"){field(SIZV,"40")}
record(ao,"AO"){field(PREC,"3") field(OUT,"LS.VAL NPP")}
record(lso,"LSP"){field(SIZV,"40")}
record(ao,"AOP"){field(PREC,"3") field(OUT,"LSP.VAL PP")}
record(stringout,"SOT"){}
record(stringin,"SIT"){}
record(ao,"A2"){field(PREC,"3") field(OUT,"SOT.VAL NPP")}
record(ao,"A3"){field(PREC,"3") field(OUT,"SIT.VAL NPP")}
"#;

const UDF_ALARM: u16 = 17;
const INVALID: u16 = 3;

struct Subs {
    len: EventReader,
    val: EventReader,
}

fn subscribe(db: &PvDatabase, rec: &str) -> Subs {
    let r = db.get_record(rec).unwrap();
    let mut g = r.write();
    let mask = (EventMask::VALUE | EventMask::ALARM).bits();
    let mut len = g.add_subscriber("LEN", 1, DbFieldType::Long, mask).unwrap();
    let mut val = g.add_subscriber("VAL", 2, DbFieldType::Char, mask).unwrap();
    drop(g);
    while len.try_recv().is_ok() {}
    while val.try_recv().is_ok() {}
    Subs { len, val }
}

impl Subs {
    /// LEN's posts as `(len, stat, sevr)`, and how many VAL posts.
    fn drain(&mut self) -> (Vec<(f64, u16, u16)>, usize) {
        let mut len = Vec::new();
        while let Ok(ev) = self.len.try_recv() {
            let v = ev.value().as_value().and_then(EpicsValue::to_f64).unwrap();
            len.push((v, ev.stat(), ev.sevr()));
        }
        let mut val = 0;
        while self.val.try_recv().is_ok() {
            val += 1;
        }
        (len, val)
    }
}

fn text(db: &PvDatabase, pv: &str) -> String {
    match db.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        EpicsValue::CharArray(b) => {
            let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
            String::from_utf8_lossy(&b[..end]).into_owned()
        }
        other => panic!("{pv} = {other:?}"),
    }
}

fn f64_of(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

#[epics_macros_rs::epics_test]
async fn a_number_into_lso_stores_the_text_and_posts_len() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut ls = subscribe(&db, "LS");
    let mut lsp = subscribe(&db, "LSP");

    db.put_record_field_from_ca("AO", "VAL", EpicsValue::Double(2.5))
        .await
        .unwrap();
    assert_eq!(ls.drain(), (vec![(9.0, UDF_ALARM, INVALID)], 0), "step 1");
    assert_eq!(text(&db, "LS"), "2.500000");
    assert_eq!(f64_of(&db, "LS.LEN"), 9.0);
    assert_eq!(f64_of(&db, "LS.UDF"), 0.0);
    assert_eq!(f64_of(&db, "LS.SEVR"), f64::from(INVALID));
    assert_eq!((f64_of(&db, "AO.STAT"), f64_of(&db, "AO.SEVR")), (0.0, 0.0));
    assert_eq!(text(&db, "AO.AMSG"), "");

    // PP: LEN from the put (the target's alarm still UDF/INVALID), then LEN
    // and VAL from its process.
    db.put_record_field_from_ca("AOP", "VAL", EpicsValue::Double(2.5))
        .await
        .unwrap();
    assert_eq!(
        lsp.drain(),
        (vec![(9.0, UDF_ALARM, INVALID), (9.0, 0, 0)], 1),
        "step 4"
    );
    for (v, len, want) in [
        (-1e-7, 10.0, "-0.000000"),
        (123456789012.0, 17.0, "123456789012.000"),
    ] {
        db.put_record_field_from_ca("AOP", "VAL", EpicsValue::Double(v))
            .await
            .unwrap();
        assert_eq!(lsp.drain(), (vec![(len, 0, 0), (len, 0, 0)], 1), "{v:e}");
        assert_eq!(text(&db, "LSP"), want);
        assert_eq!(f64_of(&db, "LSP.LEN"), len);
    }

    for (src, target) in [("A2", "SOT"), ("A3", "SIT")] {
        db.put_record_field_from_ca(src, "VAL", EpicsValue::Double(2.5))
            .await
            .unwrap();
        assert_eq!(text(&db, target), "2.500000");
        assert_eq!(f64_of(&db, &format!("{src}.SEVR")), 0.0);
    }
}

/// The integer row (`cvt_l_st`) into the same `DBF_STRING` of `SIZV`: C
/// ground truth §5.5 has longout 42 → stringout `"42"`, and lso VAL takes
/// that row through its `cvt_dbaddr`.
#[epics_macros_rs::epics_test]
async fn an_integer_into_lso_stores_its_decimal_text() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"
record(lso,"LS"){field(SIZV,"40")}
record(longout,"LO"){field(OUT,"LS.VAL NPP")}
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.put_record_field_from_ca("LO", "VAL", EpicsValue::Long(42))
        .await
        .unwrap();
    assert_eq!(text(&db, "LS"), "42");
    assert_eq!(f64_of(&db, "LS.LEN"), 3.0);
    assert_eq!(f64_of(&db, "LO.SEVR"), 0.0);
}
