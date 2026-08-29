//! Every integer `cvt_st_*` row against C, measured rather than reasoned.
//!
//! The table below is OUTPUT, not expectation: each cell was produced by
//! compiling the bodies of `dbFastLinkConv.c:91-188` verbatim at the `R7.0.10`
//! pin against this machine's built `libCom` and running the input through
//! them. `epicsStdlib.c` — where `epicsParseInt8`..`epicsParseUInt64` and
//! `epicsParseFloat64` live — is byte-identical between `R7.0.10` and the
//! checkout, so the built library's behaviour IS the pin's; `dbFastLinkConv.c`
//! differs by one whitespace line, in `cvt_f_st`.
//!
//! What it settles. `cvt_st_ul` (`:161-188`) is the ONLY row of the eight with
//! the via-double fallback; the other seven are a bare
//! `epicsParse<width>(from, to, dbConvertBase, &end)` with `&end` non-NULL, so
//! trailing text is legal and the integer prefix is stored. That is why
//! `"3.7e2"` is 3 into a LONG field and 370 into a ULONG one — the same string,
//! two answers, and neither is a bug.
//!
//! Four cells, all on the `ul` row, are neither a store nor a refusal: `-.5`,
//! `nan`, `inf` and `-inf`. C's fallback reassigns `status` from
//! `epicsParseFloat64`, which SUCCEEDS for all four, then skips the store
//! because `dval >= 0 && dval <= UINT_MAX` fails (`:182-185`). So C returns 0
//! having written nothing and the field keeps its old value — `NOSTORE` here,
//! and `Converted::Unchanged` in the port. They used to be the table's only
//! divergence, refused because `put_string` returned `CaResult<EpicsValue>`,
//! which had no "succeeded, store nothing" value; giving the row its own return
//! type closed them. All 344 cells now match C.

use epics_base_rs::types::EpicsValue;
use epics_base_rs::types::c_parse::{Converted, NumericField, put_string};

/// `input`, then the eight rows in `dbFastLinkConv.c` order:
/// `cvt_st_c, _uc, _s, _us, _l, _ul, _q, _uq`.
const C_MEASURED: &[[&str; 9]] = &[
    ["3.7", "3", "3", "3", "3", "3", "3", "3", "3"],
    ["3.7e2", "3", "3", "3", "3", "3", "370", "3", "3"],
    ["0.5", "0", "0", "0", "0", "0", "0", "0", "0"],
    [
        ".5", "REFUSE", "REFUSE", "REFUSE", "REFUSE", "REFUSE", "0", "REFUSE", "REFUSE",
    ],
    [
        "-.5", "REFUSE", "REFUSE", "REFUSE", "REFUSE", "REFUSE", "NOSTORE", "REFUSE", "REFUSE",
    ],
    [
        "-3.7",
        "-3",
        "253",
        "-3",
        "65533",
        "-3",
        "4294967293",
        "-3",
        "18446744073709551613",
    ],
    ["1.0e3", "1", "1", "1", "1", "1", "1000", "1", "1"],
    ["1.5e20", "1", "1", "1", "1", "1", "1", "1", "1"],
    ["1e999", "1", "1", "1", "1", "1", "REFUSE", "1", "1"],
    [
        "nan", "REFUSE", "REFUSE", "REFUSE", "REFUSE", "REFUSE", "NOSTORE", "REFUSE", "REFUSE",
    ],
    [
        "inf", "REFUSE", "REFUSE", "REFUSE", "REFUSE", "REFUSE", "NOSTORE", "REFUSE", "REFUSE",
    ],
    [
        "-inf", "REFUSE", "REFUSE", "REFUSE", "REFUSE", "REFUSE", "NOSTORE", "REFUSE", "REFUSE",
    ],
    ["  4.9  ", "4", "4", "4", "4", "4", "4", "4", "4"],
    ["0x10.5", "16", "16", "16", "16", "16", "16", "16", "16"],
    ["3.", "3", "3", "3", "3", "3", "3", "3", "3"],
    ["3e", "3", "3", "3", "3", "3", "3", "3", "3"],
    ["3E", "3", "3", "3", "3", "3", "3", "3", "3"],
    ["5.5e-1", "5", "5", "5", "5", "5", "0", "5", "5"],
    [
        "127.5", "127", "127", "127", "127", "127", "127", "127", "127",
    ],
    [
        "128.9", "REFUSE", "128", "128", "128", "128", "128", "128", "128",
    ],
    [
        "-128.9",
        "-128",
        "128",
        "-128",
        "65408",
        "-128",
        "4294967168",
        "-128",
        "18446744073709551488",
    ],
    [
        "255.9", "REFUSE", "255", "255", "255", "255", "255", "255", "255",
    ],
    [
        "256.1", "REFUSE", "REFUSE", "256", "256", "256", "256", "256", "256",
    ],
    [
        "32767.5", "REFUSE", "REFUSE", "32767", "32767", "32767", "32767", "32767", "32767",
    ],
    [
        "32768.5", "REFUSE", "REFUSE", "REFUSE", "32768", "32768", "32768", "32768", "32768",
    ],
    [
        "65535.9", "REFUSE", "REFUSE", "REFUSE", "65535", "65535", "65535", "65535", "65535",
    ],
    [
        "65536.1", "REFUSE", "REFUSE", "REFUSE", "REFUSE", "65536", "65536", "65536", "65536",
    ],
    [
        "2147483647.9",
        "REFUSE",
        "REFUSE",
        "REFUSE",
        "REFUSE",
        "2147483647",
        "2147483647",
        "2147483647",
        "2147483647",
    ],
    [
        "2147483648.0",
        "REFUSE",
        "REFUSE",
        "REFUSE",
        "REFUSE",
        "REFUSE",
        "2147483648",
        "2147483648",
        "2147483648",
    ],
    [
        "4294967295.5",
        "REFUSE",
        "REFUSE",
        "REFUSE",
        "REFUSE",
        "REFUSE",
        "4294967295",
        "4294967295",
        "4294967295",
    ],
    [
        "4294967296.0",
        "REFUSE",
        "REFUSE",
        "REFUSE",
        "REFUSE",
        "REFUSE",
        "REFUSE",
        "4294967296",
        "4294967296",
    ],
    [
        "-1",
        "-1",
        "255",
        "-1",
        "65535",
        "-1",
        "4294967295",
        "-1",
        "18446744073709551615",
    ],
    [
        "-1.5",
        "-1",
        "255",
        "-1",
        "65535",
        "-1",
        "4294967295",
        "-1",
        "18446744073709551615",
    ],
    ["007.5", "7", "7", "7", "7", "7", "7", "7", "7"],
    ["1_000.5", "1", "1", "1", "1", "1", "1", "1", "1"],
    [
        "abc", "REFUSE", "REFUSE", "REFUSE", "REFUSE", "REFUSE", "REFUSE", "REFUSE", "REFUSE",
    ],
    ["", "0", "0", "0", "0", "0", "0", "0", "0"],
    [
        " ", "REFUSE", "REFUSE", "REFUSE", "REFUSE", "REFUSE", "REFUSE", "REFUSE", "REFUSE",
    ],
    ["+2.5", "2", "2", "2", "2", "2", "2", "2", "2"],
    ["1.9999999999999999", "1", "1", "1", "1", "1", "2", "1", "1"],
    ["0b11.5", "3", "3", "3", "3", "3", "0", "3", "3"],
    [
        "9223372036854775807.5",
        "REFUSE",
        "REFUSE",
        "REFUSE",
        "REFUSE",
        "REFUSE",
        "REFUSE",
        "9223372036854775807",
        "9223372036854775807",
    ],
    [
        "18446744073709551615.5",
        "REFUSE",
        "255",
        "REFUSE",
        "65535",
        "REFUSE",
        "4294967295",
        "REFUSE",
        "18446744073709551615",
    ],
];

fn port(t: NumericField, s: &str) -> String {
    match put_string("F", t, s) {
        Ok(Converted::Stored(EpicsValue::Char(v))) => format!("{}", v as i8),
        Ok(Converted::Stored(EpicsValue::UChar(v))) => format!("{v}"),
        Ok(Converted::Stored(EpicsValue::Short(v))) => format!("{v}"),
        Ok(Converted::Stored(EpicsValue::UShort(v))) => format!("{v}"),
        Ok(Converted::Stored(EpicsValue::Long(v))) => format!("{v}"),
        Ok(Converted::Stored(EpicsValue::ULong(v))) => format!("{v}"),
        Ok(Converted::Stored(EpicsValue::Int64(v))) => format!("{v}"),
        Ok(Converted::Stored(EpicsValue::UInt64(v))) => format!("{v}"),
        Ok(Converted::Stored(other)) => format!("?{other:?}"),
        Ok(Converted::Unchanged) => "NOSTORE".to_string(),
        Err(_) => "REFUSE".to_string(),
    }
}

#[test]
fn every_integer_row_matches_c() {
    use NumericField::*;
    let rows = [
        ("c", Char),
        ("uc", UChar),
        ("s", Short),
        ("us", UShort),
        ("l", Long),
        ("ul", ULong),
        ("q", Int64),
        ("uq", UInt64),
    ];
    let mut nostore = 0;
    let mut cells = 0;
    for line in C_MEASURED {
        let input = line[0];
        for (i, (name, target)) in rows.iter().enumerate() {
            let c_says = line[i + 1];
            let got = port(*target, input);
            cells += 1;
            if c_says == "NOSTORE" {
                nostore += 1;
            }
            assert_eq!(
                got, c_says,
                "{input} / cvt_st_{name}: C (measured at R7.0.10) says {c_says}"
            );
        }
    }
    assert_eq!(cells, 344, "the table must stay whole");
    assert_eq!(
        nostore, 4,
        "the four succeeded-store-nothing cells stay named"
    );
}
