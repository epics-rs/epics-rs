//! A CP/CPP modifier on a `DBF_OUTLINK` is discarded with C's warning, printed
//! by each of `dbParseLink`'s callers (`dbStaticLib.c:2385`). softIoc R7.0.10,
//! `-d` on the DB below, then `dbpf D.OUTB "G:H CP"`, `dbpf S.DOL1 "I:J CP"`,
//! `dbpf B.OUT "W:V CPP"`:
//!
//! ```text
//! WARNING: Discarding CP/CPP modifier in CA output link from A.OUT to X:Y.
//! WARNING: Discarding CP/CPP modifier in CA output link from C.OUT to E:F.
//! WARNING: Discarding CP/CPP modifier in CA output link from D.OUTA to P:Q.
//! WARNING: Discarding CP/CPP modifier in CA output link from S.LNK1 to R:S.
//! WARNING: Discarding CP/CPP modifier in CA output link from A.OUT to X:Y.
//! WARNING: Discarding CP/CPP modifier in CA output link from C.OUT to E:F.
//! WARNING: Discarding CP/CPP modifier in CA output link from D.OUTA to P:Q.
//! WARNING: Discarding CP/CPP modifier in CA output link from S.LNK1 to R:S.
//! WARNING: Discarding CP/CPP modifier in CA output link from D.OUTB to G:H.
//! WARNING: Discarding CP/CPP modifier in CA output link from B.OUT to W:V.
//! ```
//!
//! the `.db` load, then `iocInit`'s `dbInitRecordLinks`, then the puts.
//! `S.LNK2` resolves `PP` ahead of `CP` and the `DOL1` inlink keeps its `CP`,
//! so neither prints.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::runtime::log::{errlog_add_listener, errlog_flush, errlog_strip_ansi};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

#[epics_macros_rs::epics_test]
async fn a_discarded_cp_prints_cs_warning_at_each_parse() {
    let heard = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&heard);
    errlog_add_listener(move |m| {
        let m = errlog_strip_ansi(m);
        if m.contains("Discarding") {
            sink.lock().unwrap().push(m);
        }
    });
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"
record(ao, "A") { field(OUT, "  X:Y CP MS ") }
record(calcout, "C") { field(OUT, "E:F CP") }
record(dfanout, "D") { field(OUTA, "P:Q CPP") }
record(seq, "S") { field(LNK1, "R:S QQCPPXMSITT") field(LNK2, "T:U PP CP") }
record(ao, "B") { field(OUT, "Z") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    for (rec, field, text) in [
        ("D", "OUTB", "G:H CP"),
        ("S", "DOL1", "I:J CP"),
        ("B", "OUT", "W:V CPP"),
    ] {
        db.put_record_field_from_ca(rec, field, EpicsValue::String(text.into()))
            .await
            .unwrap();
    }
    errlog_flush();
    let line = |from: &str, to: &str| {
        format!("WARNING: Discarding CP/CPP modifier in CA output link from {from} to {to}.\n")
    };
    let load = [
        line("A.OUT", "X:Y"),
        line("C.OUT", "E:F"),
        line("D.OUTA", "P:Q"),
        line("S.LNK1", "R:S"),
    ];
    let mut want = [load.clone(), load].concat();
    want.extend([line("D.OUTB", "G:H"), line("B.OUT", "W:V")]);
    assert_eq!(*heard.lock().unwrap(), want);
}

/// Only a PV link reaches C's modifier scan (`dbStaticLib.c:2266-2357`): an
/// array constant or an INST_IO link prints nothing, while text that merely
/// starts like JSON, a quote, a number or an array is a PV name. softIoc
/// R7.0.10, `-d` on the DB below, prints each of A2, A3, A4, A5 and A7 at the
/// load and again at `iocInit`, and A1 and A6 never.
#[epics_macros_rs::epics_test]
async fn only_a_pv_link_reaches_the_modifier_scan() {
    let heard = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&heard);
    errlog_add_listener(move |m| {
        let m = errlog_strip_ansi(m);
        if m.contains("Discarding") && m.contains(" from A") {
            sink.lock().unwrap().push(m);
        }
    });
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"
record(ao, "A1") { field(OUT, "[\"a\", \"CPU\"]") }
record(ao, "A2") { field(OUT, "{x CP") }
record(ao, "A3") { field(OUT, "\"x\" CP") }
record(ao, "A4") { field(OUT, "5 CP") }
record(ao, "A5") { field(OUT, "[x CP") }
record(ao, "A6") { field(OUT, "@x CP") }
record(ao, "A7") { field(OUT, "pva://x CPP") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    errlog_flush();
    let pass: Vec<String> = [
        ("A2", "{x"),
        ("A3", "\"x\""),
        ("A4", "5"),
        ("A5", "[x"),
        ("A7", "pva://x"),
    ]
    .iter()
    .map(|(rec, to)| {
        format!("WARNING: Discarding CP/CPP modifier in CA output link from {rec}.OUT to {to}.\n")
    })
    .collect();
    assert_eq!(*heard.lock().unwrap(), [pass.clone(), pass].concat());
}
