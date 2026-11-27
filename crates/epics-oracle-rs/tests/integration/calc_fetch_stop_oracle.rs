//! C `fetch_values` in scalcout and acalcout returns at the first failing
//! numeric `dbGetLink` (`sCalcoutRecord.c:885-887`, `aCalcoutRecord.c:1068-1071`),
//! so neither the numeric inputs after it nor the string / array inputs that
//! follow in a second loop are read: `B` and `AA` keep what they held. Each
//! record's `AA` is put first, so "not read" is told apart from "read empty".

use epics_oracle_rs::catool::{CaTools, PutOutcome};
use epics_oracle_rs::ioc::{CTools, Ioc, Pair, Side};
use epics_oracle_rs::runner::workdir;

const DB: &str = "record(stringin, \"CFS:S\") { field(VAL, \"hello\") }\n\
     record(ai, \"CFS:B\") { field(VAL, \"5\") }\n\
     record(scalcout, \"CFS:Q1\") { field(INPA, \"CFS:NOPE\") field(INPB, \"CFS:B\") \
       field(INAA, \"CFS:S\") field(CALC, \"AA\") }\n\
     record(scalcout, \"CFS:Q2\") { field(INPA, \"CFS:NOPE\") field(INAA, \"CFS:NOPE2\") \
       field(CALC, \"AA\") }\n\
     record(scalcout, \"CFS:Q3\") { field(INPA, \"CFS:B\") field(INAA, \"CFS:S\") \
       field(CALC, \"AA\") }\n\
     record(acalcout, \"CFS:R1\") { field(NELM, \"3\") field(INPA, \"CFS:NOPE\") \
       field(INPB, \"CFS:B\") field(INAA, \"CFS:B\") field(CALC, \"AA\") }\n\
     record(acalcout, \"CFS:R2\") { field(NELM, \"3\") field(INPA, \"CFS:B\") \
       field(INAA, \"CFS:B\") field(CALC, \"AA\") }\n";

/// The record, the fields read after one process, and what C reads.
const ROWS: [(&str, &[(&str, &str)]); 5] = [
    ("CFS:Q1", &[("B", "0"), ("AA", "old"), ("SVAL", "")]),
    ("CFS:Q2", &[("AA", "old")]),
    ("CFS:Q3", &[("A", "5"), ("AA", "hello"), ("SVAL", "hello")]),
    ("CFS:R1", &[("B", "0"), ("AA", "3 7 7 7")]),
    ("CFS:R2", &[("A", "5"), ("AA", "3 5 0 0")]),
];

fn put(ca: &CaTools, pv: &str, value: &[&str]) {
    let put = match value {
        [one] => ca.caput(pv, one),
        many => ca.caput_array(pv, &many.iter().map(|v| v.to_string()).collect::<Vec<_>>()),
    }
    .unwrap_or_else(|e| panic!("{:?} caput {pv}: {e:?}", ca.side()));
    assert_eq!(put, PutOutcome::Completed, "{:?} caput {pv}", ca.side());
}

fn run(ca: &CaTools, rec: &str, fields: &[(&str, &str)]) -> Vec<String> {
    let old: &[&str] = if rec.starts_with("CFS:R") {
        &["7", "7", "7"]
    } else {
        &["old"]
    };
    put(ca, &format!("{rec}.AA"), old);
    put(ca, &format!("{rec}.PROC"), &["1"]);
    let pvs: Vec<String> = fields
        .iter()
        .map(|(f, _)| format!("{rec}.{f}"))
        .chain(["SEVR", "STAT"].map(|f| format!("{rec}.{f}")))
        .collect();
    ca.caget_batch(&pvs, true)
        .unwrap_or_else(|e| panic!("{:?} caget {rec}: {e:?}", ca.side()))
}

#[test]
fn a_failed_numeric_input_stops_the_fetch() {
    super::runner_pair::built_oracle_ioc();
    let tools = CTools::discover().expect(
        "the C EPICS tree must be built for the oracle to have ground truth; \
         set EPICS_BASE_BIN and EPICS_ORACLE_IOC_BIN",
    );
    let dir = workdir(None).expect("workdir");
    let db = dir.join("calc_fetch_stop.db");
    std::fs::write(&db, DB).expect("write db");
    let pair = Pair::boot(&tools, &db, "CFS:Q1").expect("both IOCs must boot");
    let c = CaTools::new(&tools, pair.c.port(), Side::C);
    let rust = CaTools::new(&tools, pair.rust.port(), Side::Rust);

    let mut wrong = Vec::new();
    for (rec, fields) in ROWS {
        let (c_got, rust_got) = (run(&c, rec, fields), run(&rust, rec, fields));
        let want: Vec<&str> = fields.iter().map(|(_, v)| *v).collect();
        assert_eq!(c_got[..fields.len()], want, "C {rec}");
        if rust_got != c_got {
            wrong.push(format!(
                "{rec} {fields:?} +SEVR,STAT: C {c_got:?}, rust {rust_got:?}"
            ));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}
