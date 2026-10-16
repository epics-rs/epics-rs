//! An MS OUT link inherits the source's PENDING alarm as it stands at that
//! put (`dbDbLink.c:382-383` reads `psrce->nsta/nsev` after its `dbPut`),
//! so an earlier link of the same cycle whose put failed (`setLinkAlarm`,
//! `dbLink.c:445`) passes its LINK/INVALID on. oracleIoc (R7.0.10-146) on
//! this database, `dbpf DF.VAL 7`, `dbpf SQ.PROC 1`: DF, TB, SQ and SB all
//! end LINK/INVALID.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ai, "TA") {}
record(ai, "TB") {}
record(dfanout, "DF") {
    field(OUTA, "TA.INP NPP")
    field(OUTB, "TB.VAL PP MS")
}
record(ai, "SA") {}
record(ai, "SB") {}
record(seq, "SQ") {
    field(DOL1, "7") field(LNK1, "SA.INP NPP")
    field(DOL2, "1") field(LNK2, "SB.VAL PP MS")
}
"#;

#[epics_macros_rs::epics_test]
async fn a_later_ms_link_inherits_an_earlier_links_failure() {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    db.scan_control().run();
    let alarm = |rec: &str| {
        let num = |f: &str| db.get_pv(format!("{rec}.{f}")).unwrap().to_f64();
        [num("STAT"), num("SEVR")]
    };
    const LINK_INVALID: [Option<f64>; 2] = [Some(14.0), Some(3.0)];

    db.put_record_field_from_ca("DF", "VAL", EpicsValue::Double(7.0))
        .await
        .unwrap();
    assert_eq!(alarm("DF"), LINK_INVALID, "DF");
    assert_eq!(alarm("TB"), LINK_INVALID, "TB inherits OUTA's failure");

    db.put_record_field_from_ca("SQ", "PROC", EpicsValue::Double(1.0))
        .await
        .unwrap();
    // seq writes its links from a callback, after the put returns.
    for _ in 0..100 {
        if db.get_pv("SQ.PACT").unwrap().to_f64() == Some(0.0) && alarm("SQ") == LINK_INVALID {
            break;
        }
        epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(alarm("SQ"), LINK_INVALID, "SQ");
    assert_eq!(alarm("SB"), LINK_INVALID, "SB inherits LNK1's failure");
}
