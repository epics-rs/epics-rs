//! C `fetch_values` reads the link that names what the cycle reads or runs
//! before any input link, and returns when that read fails: sel's `NVL` in
//! `Specified` mode (`selRecord.c:423-425`), aSub's `SUBL` with `LFLG=READ`, or
//! the unregistered name it delivered (`aSubRecord.c:256-265`). `INPA` is then
//! never read, so `A` keeps what was put to it. An aSub name that is empty, or
//! is the one the record booted with (`ONAM` is seeded from `SNAM` at init,
//! `:162`), skips the lookup and the inputs are read.

use epics_oracle_rs::catool::{CaTools, PutOutcome};
use epics_oracle_rs::ioc::{CTools, Ioc, Pair, Side};
use epics_oracle_rs::runner::workdir;

const DB: &str = "record(ai, \"FPO:B\") { field(VAL, \"5\") }\n\
     record(longin, \"FPO:N\") { field(VAL, \"0\") }\n\
     record(stringin, \"FPO:BAD\") { field(VAL, \"nosuch\") }\n\
     record(stringin, \"FPO:EMPTY\") {}\n\
     record(aSub, \"FPO:Q1\") { field(LFLG, \"READ\") field(SUBL, \"FPO:NOPE\") \
       field(INPA, \"FPO:B\") }\n\
     record(aSub, \"FPO:Q2\") { field(LFLG, \"READ\") field(SUBL, \"FPO:BAD\") \
       field(INPA, \"FPO:B\") }\n\
     record(aSub, \"FPO:Q3\") { field(LFLG, \"READ\") field(SNAM, \"nosuch\") \
       field(INPA, \"FPO:B\") }\n\
     record(aSub, \"FPO:Q4\") { field(LFLG, \"READ\") field(SUBL, \"FPO:EMPTY\") \
       field(INPA, \"FPO:B\") }\n\
     record(sel, \"FPO:L1\") { field(SELM, \"Specified\") field(NVL, \"FPO:NOPE\") \
       field(INPA, \"FPO:B\") }\n\
     record(sel, \"FPO:L2\") { field(SELM, \"Specified\") field(NVL, \"FPO:N\") \
       field(INPA, \"FPO:B\") }\n";

/// The record, the fields read after one process, and what C reads.
const ROWS: [(&str, &[(&str, &str)]); 6] = [
    ("FPO:Q1", &[("A", "7"), ("SNAM", "")]),
    ("FPO:Q2", &[("A", "7"), ("SNAM", "nosuch")]),
    ("FPO:Q3", &[("A", "5"), ("SNAM", "nosuch")]),
    ("FPO:Q4", &[("A", "5"), ("SNAM", "")]),
    ("FPO:L1", &[("A", "7")]),
    ("FPO:L2", &[("A", "5")]),
];

fn run(ca: &CaTools, rec: &str, fields: &[(&str, &str)]) -> Vec<String> {
    for (pv, value) in [(format!("{rec}.A"), "7"), (format!("{rec}.PROC"), "1")] {
        let put = ca
            .caput(&pv, value)
            .unwrap_or_else(|e| panic!("{:?} caput {pv}: {e:?}", ca.side()));
        assert_eq!(put, PutOutcome::Completed, "{:?} caput {pv}", ca.side());
    }
    let pvs: Vec<String> = fields
        .iter()
        .map(|(f, _)| *f)
        .chain(["VAL", "SEVR", "STAT"])
        .map(|f| format!("{rec}.{f}"))
        .collect();
    ca.caget_batch(&pvs, true)
        .unwrap_or_else(|e| panic!("{:?} caget {rec}: {e:?}", ca.side()))
}

#[test]
fn a_failed_prologue_read_stops_the_input_fetch() {
    super::runner_pair::built_oracle_ioc();
    let tools = CTools::discover().expect(
        "the C EPICS tree must be built for the oracle to have ground truth; \
         set EPICS_BASE_BIN and EPICS_ORACLE_IOC_BIN",
    );
    let dir = workdir(None).expect("workdir");
    let db = dir.join("fetch_prologue.db");
    std::fs::write(&db, DB).expect("write db");
    let pair = Pair::boot(&tools, &db, "FPO:Q1").expect("both IOCs must boot");
    let c = CaTools::new(&tools, pair.c.port(), Side::C);
    let rust = CaTools::new(&tools, pair.rust.port(), Side::Rust);

    let mut wrong = Vec::new();
    for (rec, fields) in ROWS {
        let (c_got, rust_got) = (run(&c, rec, fields), run(&rust, rec, fields));
        let want: Vec<&str> = fields.iter().map(|(_, v)| *v).collect();
        assert_eq!(c_got[..fields.len()], want, "C {rec}");
        if rust_got != c_got {
            wrong.push(format!(
                "{rec} {fields:?} +VAL,SEVR,STAT: C {c_got:?}, rust {rust_got:?}"
            ));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}
