//! A `$` address on a link field or attribute keeps that field's dbfType,
//! so `dbGet` reads its text through `getLinkValue` / `getAttrValue`
//! whatever the request, while its DBR type is DBR_CHAR. Pinned to
//! oracleStdIoc (R7.0.10.1 + calc) for the same records.

use std::time::Duration;

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::{scalcout::ScalcoutRecord, sseq::SseqRecord};
use epics_base_rs::types::EpicsValue;

const L: &str = "123456789012345678901234567890123456789012345678901234567890";

fn db_text() -> String {
    format!(
        "record(ai, \"X\") {{ field(INP, \"{L}\") }}\n\
         record(lsi, \"LI\") {{ field(SIZV, \"100\") field(INP, \"X.INP$\") }}\n\
         record(lso, \"LO\") {{ field(SIZV, \"100\") field(DOL, \"X.INP$\") field(OMSL, \"closed_loop\") }}\n\
         record(sseq, \"SQ\") {{ field(DOL1, \"X.INP$ NPP\") }}\n\
         record(stringin, \"SI\") {{ field(INP, \"X.INP$\") }}\n\
         record(longin, \"LG\") {{ field(INP, \"X.INP$\") }}\n\
         record(ai, \"AI\") {{ field(INP, \"X.INP$\") }}\n\
         record(calc, \"CA\") {{ field(INPA, \"X.INP$\") field(CALC, \"A\") }}\n\
         record(stringin, \"SA\") {{ field(INP, \"X.RTYP$\") }}\n\
         record(ai, \"AA\") {{ field(INP, \"X.RTYP$\") }}\n\
         record(lsi, \"LA\") {{ field(SIZV, \"100\") field(INP, \"X.RTYP$\") }}\n\
         record(scalcout, \"SC\") {{ field(INAA, \"X.INP$\") field(CALC, \"AA\") }}\n"
    )
}

fn text(db: &PvDatabase, pv: &str) -> String {
    match db.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        EpicsValue::CharArray(b) => {
            let b: Vec<u8> = b.iter().copied().take_while(|&c| c != 0).collect();
            String::from_utf8(b).unwrap()
        }
        other => panic!("{pv} = {other:?}"),
    }
}

/// Until `rec` leaves PACT: sseq runs even DLY 0 links from a callback
/// (`sseqRecord.c:443-448`), after its process returns.
async fn idle(db: &PvDatabase, rec: &str) {
    let cell = db.get_record(rec).unwrap();
    epics_base_rs::runtime::task::timeout(fact_budget::FACT_BUDGET, async {
        while cell.read().is_processing() {
            epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{rec} never left PACT"));
}

#[epics_macros_rs::epics_test]
async fn a_dollar_link_field_reads_its_text_at_every_request_like_softioc() {
    let db = IocBuilder::new()
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .db_string(&db_text(), &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    for rec in [
        "LI", "LO", "SQ", "SI", "LG", "AI", "CA", "SA", "AA", "LA", "SC",
    ] {
        db.process_record_with_links(rec, &mut ProcStack::new())
            .await
            .or_else(epics_base_rs::error::CaError::unless_process_status)
            .unwrap();
    }
    idle(&db, "SQ").await;
    // A number, or the text; SEVR 3 is INVALID and STAT 14 LINK_ALARM.
    let get = |pv: &str| match db.get_pv(pv).unwrap() {
        EpicsValue::String(_) | EpicsValue::CharArray(_) => text(&db, pv),
        v => format!("{:?}", v.to_f64().unwrap()),
    };
    let l39 = &L[..39];
    let want = [
        // DBR_CHAR, the DBR type `dbGetLinkDBFtype` gives lsi/lso.
        ("LI.LEN", "61.0"),
        ("LI.VAL", L),
        ("LO.LEN", "61.0"),
        ("LO.VAL", L),
        ("LA.LEN", "3.0"),
        ("LA.VAL", "ai"),
        // DBR_STRING: the text in MAX_STRING_SIZE.
        ("SQ.STR1", l39),
        ("SQ.DO1", &format!("{:?}", l39.parse::<f64>().unwrap())),
        ("SI.VAL", l39),
        ("SI.SEVR", "0.0"),
        ("SA.VAL", "ai"),
        ("SA.SEVR", "0.0"),
        ("SC.AA", l39),
        ("SC.SVAL", l39),
        // DBR_DOUBLE: a link's NaN, an attribute's S_db_badDbrtype.
        ("AI.VAL", "NaN"),
        ("CA.A", "NaN"),
        ("CA.VAL", "NaN"),
        ("AA.SEVR", "3.0"),
        ("AA.STAT", "14.0"),
        // DBR_LONG: S_db_badDbrtype.
        ("LG.SEVR", "3.0"),
        ("LG.STAT", "14.0"),
    ];
    let got: Vec<_> = want.iter().map(|(pv, _)| (*pv, get(pv))).collect();
    let want: Vec<_> = want.iter().map(|(pv, v)| (*pv, v.to_string())).collect();
    assert_eq!(got, want);
}
