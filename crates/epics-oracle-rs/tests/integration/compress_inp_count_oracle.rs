//! compress reads INP at `DBF_DOUBLE` for `nelements`, INP's
//! `dbGetNelements` (`compressRecord.c:328-342`), under every algorithm. A
//! bare link field is one element, so C asks for one and `dbDbGetValue`'s
//! scalar branch refuses it whatever its text (`dbDbLink.c:186-198`):
//! LINK/INVALID, nothing buffered. Over CA the same field answers
//! `getLinkValue`'s NaN (`dbAccess.c:838-841`). A waveform is the control.
//! The put's outcome is compared too: `array_average` short of `N` arrays
//! returns 1, so C fires no forward link and the put never completes.

use epics_oracle_rs::catool::{CaTools, PutOutcome};
use epics_oracle_rs::ioc::{CTools, Ioc, Pair, Side};
use epics_oracle_rs::runner::workdir;

const ALGS: [&str; 6] = [
    "N to 1 Low Value",
    "N to 1 High Value",
    "N to 1 Average",
    "Average",
    "Circular Buffer",
    "N to 1 Median",
];

/// INP of each compress, and what C answers for the put, NUSE, INPN, SEVR and
/// STAT of one process: the refusal for the link field; the waveform, whose
/// answer differs per algorithm, is compared with C only.
const SOURCES: [(&str, Option<[&str; 5]>); 7] = [
    ("CIC:C.INPA", Some(["completed", "0", "1", "3", "14"])),
    ("CIC:C.INPB", Some(["completed", "0", "1", "3", "14"])),
    ("CIC:C.FLNK", Some(["completed", "0", "1", "3", "14"])),
    // A PP link is read with the lock set released, apart from NPP's read.
    ("CIC:C.INPA PP", Some(["completed", "0", "1", "3", "14"])),
    ("CIC:C.INPB PP", Some(["completed", "0", "1", "3", "14"])),
    ("CIC:C.INPA CA", None),
    ("CIC:W", None),
];

fn db() -> String {
    let mut db = String::from(
        "record(calc, \"CIC:C\") { field(INPA, \"CIC:W\") field(CALC, \"A\") }\n\
         record(waveform, \"CIC:W\") { field(NELM, \"3\") field(FTVL, \"DOUBLE\") }\n",
    );
    for (s, (inp, _)) in SOURCES.iter().enumerate() {
        for (a, alg) in ALGS.iter().enumerate() {
            db.push_str(&format!(
                "record(compress, \"CIC:Z{s}{a}\") {{ field(INP, \"{inp}\") \
                 field(ALG, \"{alg}\") field(NSAM, \"4\") field(N, \"3\") }}\n"
            ));
        }
    }
    db
}

fn run(ca: &CaTools, rec: &str) -> Vec<String> {
    let put = ca
        .caput(&format!("{rec}.PROC"), "1")
        .unwrap_or_else(|e| panic!("{:?} caput {rec}: {e:?}", ca.side()));
    let pvs: Vec<String> = ["NUSE", "INPN", "SEVR", "STAT"]
        .map(|f| format!("{rec}.{f}"))
        .to_vec();
    let got = ca
        .caget_batch(&pvs, true)
        .unwrap_or_else(|e| panic!("{:?} caget {rec}: {e:?}", ca.side()));
    std::iter::once(put.as_surface().to_string())
        .chain(got)
        .collect()
}

#[test]
fn compress_asks_inp_for_its_element_count() {
    super::runner_pair::built_oracle_ioc();
    let tools = CTools::discover().expect(
        "the C EPICS tree must be built for the oracle to have ground truth; \
         set EPICS_BASE_BIN and EPICS_ORACLE_IOC_BIN",
    );
    let dir = workdir(None).expect("workdir");
    let path = dir.join("compress_inp_count.db");
    std::fs::write(&path, db()).expect("write db");
    let pair = Pair::boot(&tools, &path, "CIC:W").expect("both IOCs must boot");
    let c = CaTools::new(&tools, pair.c.port(), Side::C);
    let rust = CaTools::new(&tools, pair.rust.port(), Side::Rust);
    for ca in [&c, &rust] {
        let put = ca
            .caput_array("CIC:W", &["4".into(), "5".into(), "6".into()])
            .unwrap_or_else(|e| panic!("{:?} caput CIC:W: {e:?}", ca.side()));
        assert_eq!(put, PutOutcome::Completed);
    }

    let mut wrong = Vec::new();
    for (s, (inp, want)) in SOURCES.iter().enumerate() {
        for (a, alg) in ALGS.iter().enumerate() {
            let rec = format!("CIC:Z{s}{a}");
            let (c_got, rust_got) = (run(&c, &rec), run(&rust, &rec));
            if let Some(want) = want {
                assert_eq!(c_got, want, "C {inp} {alg}: put, NUSE, INPN, SEVR, STAT");
            }
            if rust_got != c_got {
                wrong.push(format!("{inp} {alg}: C {c_got:?}, rust {rust_got:?}"));
            }
        }
    }
    assert!(wrong.is_empty(), "put, NUSE, INPN, SEVR, STAT: {wrong:#?}");
}
