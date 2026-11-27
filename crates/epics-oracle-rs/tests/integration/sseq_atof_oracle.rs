//! sseq derives `DOn` from `STRn` with C `atof` at all four of its sites: the
//! loaded `STRn` (`sseqRecord.c:244`), a `DOLn` read of a `DBF_STRING` (`:657`)
//! or `DBF_CHAR` (`:696`) source, and a `STRn` put (`:1133`). That is glibc's
//! `strtod` prefix, so trailing text is ignored and text with no number is 0.
//! Each row's string goes down all four paths on both IOCs, read back with C
//! `caget`.

use epics_oracle_rs::catool::{CaTools, PutOutcome};
use epics_oracle_rs::ioc::{CTools, Ioc, Pair, Side};
use epics_oracle_rs::runner::workdir;

/// The text, and `DOn` as C `caget` prints it (`%g`).
const ROWS: [(&str, &str); 15] = [
    ("0123456789abc", "1.23457e+08"),
    ("", "0"),
    (" 42", "42"),
    ("  -3.5e2xyz", "-350"),
    ("12abc", "12"),
    ("0x1A", "26"),
    ("0x1p3", "8"),
    ("0x", "0"),
    ("017", "17"),
    ("abc", "0"),
    ("-Infinity", "-inf"),
    ("nan", "nan"),
    ("1e400", "inf"),
    ("+.5e", "0.5"),
    ("1,5", "1"),
];

/// `DOn` of row `i`'s sseq, per path: loaded `STR4`, `DOL1` from `VAL`,
/// `DOL2` from `VAL$`, and the `STR3` put.
fn read(ca: &CaTools, i: usize) -> Vec<String> {
    let pvs: Vec<String> = ["DO4", "DO1", "DO2", "DO3"]
        .iter()
        .map(|f| format!("SQA:Q{i}.{f}"))
        .collect();
    ca.caget_batch(&pvs, true)
        .unwrap_or_else(|e| panic!("{:?} caget row {i}: {e:?}", ca.side()))
}

fn drive(ca: &CaTools, i: usize, text: &str) {
    for (pv, value) in [
        (format!("SQA:Q{i}.STR3"), text),
        (format!("SQA:Q{i}.PROC"), "1"),
    ] {
        let put = ca
            .caput(&pv, value)
            .unwrap_or_else(|e| panic!("{:?} caput {pv}: {e:?}", ca.side()));
        assert_eq!(put, PutOutcome::Completed, "{:?} caput {pv}", ca.side());
    }
}

#[test]
fn sseq_reads_a_string_as_atof_does() {
    super::runner_pair::built_oracle_ioc();
    let tools = CTools::discover().expect(
        "the C EPICS tree must be built for the oracle to have ground truth; \
         set EPICS_BASE_BIN and EPICS_ORACLE_IOC_BIN",
    );
    let dir = workdir(None).expect("workdir");
    let db = dir.join("sseq_atof.db");
    let mut text = String::new();
    for (i, (s, _)) in ROWS.iter().enumerate() {
        text.push_str(&format!(
            "record(stringout, \"SQA:S{i}\") {{ field(VAL, \"{s}\") }}\n\
             record(sseq, \"SQA:Q{i}\") {{ field(STR4, \"{s}\") \
             field(DOL1, \"SQA:S{i}.VAL\") field(DOL2, \"SQA:S{i}.VAL$\") }}\n"
        ));
    }
    std::fs::write(&db, text).expect("write db");
    let pair = Pair::boot(&tools, &db, "SQA:Q0").expect("both IOCs must boot");
    let c = CaTools::new(&tools, pair.c.port(), Side::C);
    let rust = CaTools::new(&tools, pair.rust.port(), Side::Rust);

    let mut wrong = Vec::new();
    for (i, (s, want)) in ROWS.iter().enumerate() {
        drive(&c, i, s);
        drive(&rust, i, s);
        let (c_do, rust_do) = (read(&c, i), read(&rust, i));
        assert_eq!(c_do, vec![*want; 4], "C row {s:?}: STR4, DOL1, DOL2, STR3");
        if rust_do != c_do {
            wrong.push(format!("{s:?}: C {c_do:?}, rust {rust_do:?}"));
        }
    }
    assert!(wrong.is_empty(), "STR4, DOL1, DOL2, STR3: {wrong:#?}");
}
