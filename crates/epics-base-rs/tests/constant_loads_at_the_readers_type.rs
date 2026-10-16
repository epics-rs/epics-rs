//! A CONSTANT link loads at the type its reader names, not as a parsed double
//! cast afterwards: printf's `recGblInitConstantLink(plink, DBRTYPE, &val)`
//! per conversion and `dbLoadLinkLS` for `%ls` (`printfRecord.c:47-57`,
//! `:240-243`, `:289`), histogram's SVL at `DBF_DOUBLE`
//! (`devHistogramSoft.c:44`), and a soft dset's INP at its own type. A
//! refused printf load prints IVLS and raises no alarm. Values are softIoc
//! R7.0.10.1 (8f5015b66).

use epics_base_rs::server::database::ProcStack;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(printf, "P1") { field(FMT, "%d") field(INP0, "5e9") }
record(printf, "P2") { field(FMT, "%ld") field(INP0, "1e19") }
record(printf, "P3") { field(FMT, "%hd") field(INP0, "70000") }
record(printf, "P4") { field(FMT, "%u") field(INP0, "-1") }
record(printf, "P5") { field(FMT, "%f") field(INP0, "1.5") }
record(printf, "P6") { field(FMT, "%s") field(INP0, "2.5") }
record(printf, "P7") { field(FMT, "%ls") field(INP0, "5") }
record(printf, "P8") { field(FMT, "%ls") field(INP0, {const:"hi"}) }
record(printf, "P9") { field(FMT, "%*d") field(INP0, "4") field(INP1, "7") }
record(printf, "P10") { field(FMT, "%c") field(INP0, "300") }
record(printf, "P11") { field(FMT, "%lld") field(INP0, "1e19") }
record(printf, "P12") { field(FMT, "%d") field(INP0, "0x10") }
record(printf, "P13") { field(FMT, "%s") field(INP0, {const:3}) }
record(printf, "P14") { field(FMT, "%d") field(INP0, "[7,8]") }
record(printf, "P15") { field(FMT, "%d") field(INP0, "2.9") }
record(printf, "P16") { field(FMT, "%d") field(INP0, "-2.9") }
record(histogram, "H1") { field(SVL, "5e9") }
record(histogram, "H2") { field(SVL, "0x10") }
record(histogram, "H3") { field(SVL, "[7,8]") }
record(histogram, "H4") { field(SVL, " 12 ") }
record(longin, "L1") { field(INP, "5e9") }
record(longin, "L2") { field(INP, "[7,8]") }
record(int64in, "Q1") { field(INP, "1e19") }
record(int64in, "Q2") { field(INP, "-2.9") }
"#;

const PRINTF: [(&str, &str); 16] = [
    ("P1", "5"),
    ("P2", "1"),
    ("P3", "LNK"),
    ("P4", "4294967295"),
    ("P5", "1.500000"),
    ("P6", "2.5"),
    ("P7", ""),
    ("P8", "hi"),
    ("P9", "   7"),
    ("P10", "LNK"),
    ("P11", "1"),
    ("P12", "16"),
    ("P13", "3"),
    ("P14", "7"),
    ("P15", "2"),
    ("P16", "-2"),
];

const HISTOGRAM: [(&str, f64); 4] = [("H1", 5e9), ("H2", 16.0), ("H3", 7.0), ("H4", 12.0)];

/// The soft dsets' `init_record` loads, at `DBF_LONG` and `DBF_INT64`.
const SOFT_INPUT: [(&str, EpicsValue); 4] = [
    ("L1", EpicsValue::Long(5)),
    ("L2", EpicsValue::Long(7)),
    ("Q1", EpicsValue::Int64(1)),
    ("Q2", EpicsValue::Int64(-2)),
];

#[epics_macros_rs::epics_test]
async fn constant_loads_at_the_readers_type() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let mut diverging = Vec::new();
    for (rec, want) in PRINTF {
        db.process_record_with_links(rec, &mut ProcStack::new())
            .await
            .unwrap();
        let got = match db.get_pv(format!("{rec}.VAL")).unwrap() {
            EpicsValue::CharArray(b) => b.to_vec(),
            other => panic!("{rec}.VAL {other:?}"),
        };
        let len = db.get_pv(format!("{rec}.LEN")).unwrap();
        let sevr = db.get_pv(format!("{rec}.SEVR")).unwrap();
        if got != [want.as_bytes(), b"\0"].concat()
            || len != EpicsValue::ULong(want.len() as u32 + 1)
            || sevr != EpicsValue::Short(0)
        {
            diverging.push(format!(
                "{rec}: {:?} {len:?} {sevr:?}",
                String::from_utf8_lossy(&got)
            ));
        }
    }
    for (rec, want) in HISTOGRAM {
        let sgnl = db.get_pv(format!("{rec}.SGNL")).unwrap();
        if sgnl != EpicsValue::Double(want) {
            diverging.push(format!("{rec}.SGNL {sgnl:?}"));
        }
    }
    for (rec, want) in SOFT_INPUT {
        let val = db.get_pv(format!("{rec}.VAL")).unwrap();
        if val != want {
            diverging.push(format!("{rec}.VAL {val:?}"));
        }
    }
    assert!(diverging.is_empty(), "{diverging:#?}");
}
