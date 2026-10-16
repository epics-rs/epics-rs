//! A double into an 8/16-bit DBR type is C's 32-bit convert then its low
//! bits (`c_cast`), pinned to softIoc R7.0.10.1 through a DB link: waveform
//! INP at FTVL CHAR/UCHAR/SHORT/USHORT and printf `%hhd %hhu %hd %hu`. Past
//! 32 bits softIoc on x86-64 stores INT_MIN's low bits, 0.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

/// (source, CHAR, UCHAR, SHORT, USHORT); printf prints the same numbers.
#[rustfmt::skip]
const ROWS: &[(&str, i64, i64, i64, i64)] = &[
    ("300", 44, 44, 300, 300),
    ("-300", -44, 212, -300, 65236),
    ("70000", 112, 112, 4464, 4464),
    ("-2.7", -2, 254, -2, 65534),
    ("70000.9", 112, 112, 4464, 4464),
    ("-1.5", -1, 255, -1, 65535),
    ("200.7", -56, 200, 200, 200),
    ("0/0", 0, 0, 0, 0),
    ("1e10", 0, 0, 0, 0),
    ("5e9", 0, 0, 0, 0),
    ("2147483648", 0, 0, 0, 0),
    ("1/0", 0, 0, 0, 0),
];

const FTVL: [&str; 4] = ["CHAR", "UCHAR", "SHORT", "USHORT"];
const FMT: [&str; 4] = ["%hhd", "%hhu", "%hd", "%hu"];

#[epics_macros_rs::epics_test]
async fn narrow_double_casts_match_softioc() {
    let mut text = String::new();
    for (i, (src, ..)) in ROWS.iter().enumerate() {
        text.push_str(&format!(
            "record(calc, \"S{i}\") {{ field(CALC, \"{src}\") }}\n"
        ));
        for j in 0..4 {
            text.push_str(&format!(
                "record(waveform, \"W{i}_{j}\") {{ field(FTVL, \"{}\") field(NELM, \"1\") field(INP, \"S{i}\") }}\n\
                 record(printf, \"P{i}_{j}\") {{ field(FMT, \"{}\") field(INP0, \"S{i}\") }}\n",
                FTVL[j], FMT[j]
            ));
        }
    }
    let db = IocBuilder::new()
        .db_string(&text, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let mut failed = Vec::new();
    for (i, &(src, c, uc, s, us)) in ROWS.iter().enumerate() {
        let mut visited = epics_base_rs::server::database::ProcStack::new();
        db.process_record_with_links(&format!("S{i}"), &mut visited)
            .await
            .unwrap();
        for (j, want) in [c, uc, s, us].into_iter().enumerate() {
            for rec in [format!("W{i}_{j}"), format!("P{i}_{j}")] {
                let mut visited = epics_base_rs::server::database::ProcStack::new();
                db.process_record_with_links(&rec, &mut visited)
                    .await
                    .unwrap();
                let got = match db.get_pv(format!("{rec}.VAL")).unwrap() {
                    EpicsValue::CharArray(b) if rec.starts_with('P') => {
                        String::from_utf8_lossy(b.split(|&c| c == 0).next().unwrap_or_default())
                            .parse::<i64>()
                            .unwrap()
                    }
                    EpicsValue::CharArray(b) => b[0] as i8 as i64,
                    EpicsValue::UCharArray(b) => b[0] as i64,
                    EpicsValue::ShortArray(v) => v[0] as i64,
                    EpicsValue::UShortArray(v) => v[0] as i64,
                    other => panic!("{rec}: {other:?}"),
                };
                if got != want {
                    failed.push(format!("{rec} ({}) of {src}: {got}, want {want}", FTVL[j]));
                }
            }
        }
    }
    assert!(failed.is_empty(), "{failed:#?}");
}
