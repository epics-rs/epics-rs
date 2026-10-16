//! Spec §5 test 21: an ao OUT into `X.VAL$`. `$` re-types the address as
//! `DBF_CHAR` (`dbAccess.c:682-701`), so a double takes `cvt_d_c`: a plain
//! `DBF_STRING` stringout keeps its tail and has only byte 0 replaced (the
//! scalar row), an `SPC_DBADDR` lso takes the array row, one element, and
//! its `special` sets LEN. Pinned to softIoc 8f5015b66 with this DB:
//! stringout "xyz" → `"\x02yz"`, lso "abcdef" → `"\x02"`, LEN 2.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(stringout,"SO"){field(VAL,"x")}
record(stringout,"SOP"){field(VAL,"x")}
record(lso,"LS"){field(SIZV,"40")}
record(lso,"LSP"){field(SIZV,"40")}
record(ao,"AN"){field(PREC,"3") field(OUT,"SO.VAL$ NPP")}
record(ao,"AP"){field(PREC,"3") field(OUT,"SOP.VAL$ PP")}
record(ao,"LN"){field(PREC,"3") field(OUT,"LS.VAL$ NPP")}
record(ao,"LP"){field(PREC,"3") field(OUT,"LSP.VAL$ PP")}
"#;

/// Posts on VAL (count) and LEN (values), with the alarm each carried.
struct Watch {
    val: EventReader,
    len: Option<EventReader>,
}

fn watch(db: &PvDatabase, rec: &str, lso: bool) -> Watch {
    let r = db.get_record(rec).unwrap();
    let mut g = r.write();
    let mask = (EventMask::VALUE | EventMask::ALARM).bits();
    let val = g
        .add_subscriber("VAL", 1, DbFieldType::String, mask)
        .unwrap();
    let len = lso.then(|| {
        g.add_subscriber("LEN", 2, DbFieldType::Double, mask)
            .unwrap()
    });
    Watch { val, len }
}

impl Watch {
    /// `(VAL posts, LEN posts)` since the last drain.
    fn drain(&mut self) -> (usize, Vec<f64>) {
        let mut val = 0;
        while self.val.try_recv().is_ok() {
            val += 1;
        }
        let mut len = Vec::new();
        if let Some(rx) = &mut self.len {
            while let Ok(ev) = rx.try_recv() {
                len.push(ev.value().as_value().and_then(EpicsValue::to_f64).unwrap());
            }
        }
        (val, len)
    }
}

fn bytes(db: &PvDatabase, pv: &str) -> Vec<u8> {
    match db.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.as_bytes().to_vec(),
        // lso VAL reads as its `DBF_CHAR` image, NUL-terminated.
        EpicsValue::CharArray(a) => a.iter().copied().take_while(|&c| c != 0).collect(),
        other => panic!("{pv} = {other:?}"),
    }
}

fn sevr(db: &PvDatabase, rec: &str) -> f64 {
    db.get_pv(format!("{rec}.SEVR")).unwrap().to_f64().unwrap()
}

fn chars(s: &str) -> EpicsValue {
    let mut b = s.as_bytes().to_vec();
    b.push(0);
    EpicsValue::CharArray(b.into())
}

async fn ioc() -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    for so in ["SO", "SOP"] {
        db.put_record_field_from_ca(so, "VAL", EpicsValue::String("xyz".into()))
            .await
            .unwrap();
    }
    for ls in ["LS", "LSP"] {
        db.put_record_field_from_ca(ls, "VAL$", chars("abcdef"))
            .await
            .unwrap();
    }
    db
}

async fn put(db: &PvDatabase, src: &str, v: f64) {
    db.put_record_field_from_ca(src, "VAL", EpicsValue::Double(v))
        .await
        .unwrap();
}

#[epics_macros_rs::epics_test]
async fn a_dollar_out_into_stringout_replaces_byte_zero_only() {
    let db = ioc().await;
    let mut so = watch(&db, "SO", false);
    let mut sop = watch(&db, "SOP", false);
    so.drain();
    sop.drain();

    put(&db, "AN", 2.5).await;
    assert_eq!(bytes(&db, "SO"), b"\x02yz");
    assert_eq!(
        so.drain(),
        (0, vec![]),
        "NPP posts nothing: VAL is pp(TRUE)"
    );
    assert_eq!(sevr(&db, "AN"), 0.0);

    put(&db, "AN", 300.0).await;
    assert_eq!(bytes(&db, "SO"), b",yz", "(epicsInt8)300 == 44");

    put(&db, "AP", 2.5).await;
    assert_eq!(bytes(&db, "SOP"), b"\x02yz");
    assert_eq!(sop.drain(), (1, vec![]), "PP: the target's own monitor()");
    assert_eq!(sevr(&db, "AP"), 0.0);
}

#[epics_macros_rs::epics_test]
async fn a_dollar_out_into_lso_stores_one_element_and_sets_len() {
    let db = ioc().await;
    let mut ls = watch(&db, "LS", true);
    let mut lsp = watch(&db, "LSP", true);
    ls.drain();
    lsp.drain();

    put(&db, "LN", 2.5).await;
    assert_eq!(bytes(&db, "LS"), b"\x02");
    assert_eq!(f64_of(&db, "LS.LEN"), 2.0);
    assert_eq!(ls.drain(), (0, vec![2.0]), "special(after) posts LEN");
    assert_eq!(sevr(&db, "LN"), 0.0);

    put(&db, "LP", 2.5).await;
    assert_eq!(bytes(&db, "LSP"), b"\x02");
    assert_eq!(
        lsp.drain(),
        (1, vec![2.0, 2.0]),
        "LEN from special(after), then LEN and VAL from monitor()"
    );
    assert_eq!(sevr(&db, "LP"), 0.0);
}

fn f64_of(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}
