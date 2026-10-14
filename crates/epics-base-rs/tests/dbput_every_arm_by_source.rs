//! Spec §5 test 2: every `PutFn` arm × source class, read back as `dbgf`
//! reads it. The policy is C's x86-64 cast (user decision 2026-10-10), so
//! every expectation is oracleStdIoc's readback (C ground truth §5.2, DBs
//! 02a/02b/02c verbatim). A refused string put leaves the target as it
//! was and raises LINK/INVALID on the source; the source's CA put result is
//! not asserted (rs answers success, C ECA_PUTFAIL: carried, spec §2). MBD
//! is disabled (DISV=DISA=1), as the oracle read its stored B0 byte: on a
//! live record `bitsFromVAL` resets B0 in the cycle the put starts.

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

#[derive(Debug, PartialEq)]
enum Read {
    I(i128),
    F(f64),
}

/// The field as `dbgf` prints it: an integer of the field's own signedness
/// (`DBF_CHAR` is signed), or a float.
fn read(db: &PvDatabase, pv: &str) -> Read {
    use EpicsValue as V;
    match db.get_pv(pv).unwrap() {
        V::Short(v) => Read::I(v.into()),
        V::Enum(v) | V::UShort(v) => Read::I(v.into()),
        V::Char(v) => Read::I((v as i8).into()),
        V::UChar(v) => Read::I(v.into()),
        V::Long(v) => Read::I(v.into()),
        V::ULong(v) => Read::I(v.into()),
        V::Int64(v) => Read::I(v.into()),
        V::UInt64(v) => Read::I(v.into()),
        V::Float(v) => Read::F(v.into()),
        V::Double(v) => Read::F(v),
        V::ShortArray(a) => Read::I(a[0].into()),
        V::EnumArray(a) | V::UShortArray(a) => Read::I(a[0].into()),
        V::CharArray(a) => Read::I((a[0] as i8).into()),
        V::UCharArray(a) => Read::I(a[0].into()),
        V::LongArray(a) => Read::I(a[0].into()),
        V::ULongArray(a) => Read::I(a[0].into()),
        V::Int64Array(a) => Read::I(a[0].into()),
        V::UInt64Array(a) => Read::I(a[0].into()),
        V::FloatArray(a) => Read::F(a[0].into()),
        V::DoubleArray(a) => Read::F(a[0]),
        other => panic!("{pv} = {other:?}"),
    }
}

async fn ioc(db: &str) -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

const DB_02A: &str = r#"
record(waveform,"WFF"){field(FTVL,"FLOAT") field(NELM,"1")}
record(waveform,"WFC"){field(FTVL,"CHAR") field(NELM,"1")}
record(waveform,"WFU"){field(FTVL,"UCHAR") field(NELM,"1")}
record(waveform,"WFS"){field(FTVL,"SHORT") field(NELM,"1")}
record(waveform,"WFUS"){field(FTVL,"USHORT") field(NELM,"1")}
record(waveform,"WFL"){field(FTVL,"LONG") field(NELM,"1")}
record(waveform,"WFUL"){field(FTVL,"ULONG") field(NELM,"1")}
record(waveform,"WFI6"){field(FTVL,"INT64") field(NELM,"1")}
record(waveform,"WFUI6"){field(FTVL,"UINT64") field(NELM,"1")}
record(ao,"DV"){}
record(mbboDirect,"MBD"){field(DISV,"1") field(DISA,"1")}
record(sel,"SEL"){}
record(longin,"LI"){}
record(int64in,"I6"){}
record(bo,"BO"){field(ZNAM,"Off") field(ONAM,"On")}
record(bo,"BO2"){}
record(mbbo,"MB"){}
record(mbbo,"MBS"){field(ZRST,"zero") field(ONST,"one") field(TWST,"two")}
record(mbboDirect,"MBD2"){}
record(mbbi,"MBI"){}
record(ai,"AI"){}
record(dfanout,"D1"){field(OUTA,"WFF.VAL NPP") field(OUTB,"WFC.VAL NPP") field(OUTC,"WFU.VAL NPP") field(OUTD,"WFS.VAL NPP") field(OUTE,"DV.DISV NPP") field(OUTF,"MBD.B0 NPP") field(OUTG,"SEL.SELN NPP") field(OUTH,"WFUS.VAL NPP")}
record(dfanout,"D2"){field(OUTA,"LI.VAL NPP") field(OUTB,"I6.VAL NPP") field(OUTC,"BO.VAL NPP") field(OUTD,"BO2.RVAL NPP") field(OUTE,"MB.VAL NPP") field(OUTF,"MBS.VAL NPP") field(OUTG,"MBD2.VAL NPP") field(OUTH,"MBI.RVAL NPP")}
record(dfanout,"D3"){field(OUTA,"WFL.VAL NPP") field(OUTB,"WFUL.VAL NPP") field(OUTC,"WFI6.VAL NPP") field(OUTD,"WFUI6.VAL NPP") field(OUTE,"AI.RVAL NPP")}
"#;

const FLOATS: [f64; 17] = [
    300.0,
    -2.7,
    1.9,
    0.0,
    1.0,
    2.0,
    3e9,
    5e9,
    3e10,
    -3e10,
    1e19,
    -1e19,
    f64::NAN,
    f64::INFINITY,
    f64::NEG_INFINITY,
    1e39,
    -1.0,
];

const MIN32: i128 = i32::MIN as i128;
const MIN64: i128 = i64::MIN as i128;
const TOP64: i128 = 1 << 63;

/// int32-routed narrow targets (CHAR, UCHAR, SHORT, USHORT, ENUM) all read
/// 0 from 3e9 on.
const fn narrow(n300: i128, m27: i128, m1: i128) -> [i128; 17] {
    [n300, m27, 1, 0, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, m1]
}
const CHAR: [i128; 17] = narrow(44, -2, -1);
const UCHAR: [i128; 17] = narrow(44, 254, 255);
const SHORT: [i128; 17] = narrow(300, -2, -1);
const USHORT: [i128; 17] = narrow(300, 65534, 65535);
const LONG: [i128; 17] = [
    300, -2, 1, 0, 1, 2, MIN32, MIN32, MIN32, MIN32, MIN32, MIN32, MIN32, MIN32, MIN32, MIN32, -1,
];
const ULONG: [i128; 17] = [
    300, 4294967294, 1, 0, 1, 2, 3000000000, 705032704, 4230196224, 64771072, 0, 0, 0, 0, 0, 0,
    4294967295,
];
const INT64: [i128; 17] = [
    300,
    -2,
    1,
    0,
    1,
    2,
    3000000000,
    5000000000,
    30000000000,
    -30000000000,
    MIN64,
    MIN64,
    MIN64,
    MIN64,
    MIN64,
    MIN64,
    -1,
];
const UINT64: [i128; 17] = [
    300,
    18446744073709551614,
    1,
    0,
    1,
    2,
    3000000000,
    5000000000,
    30000000000,
    18446744043709551616,
    10000000000000000000,
    TOP64,
    TOP64,
    0,
    TOP64,
    0,
    18446744073709551615,
];

#[epics_macros_rs::epics_test]
async fn a_float_source_stores_cs_x86_cast_in_every_arm() {
    let db = ioc(DB_02A).await;
    let table: [(&str, &str, &[i128; 17]); 20] = [
        ("D1", "WFC", &CHAR),
        ("D1", "WFU", &UCHAR),
        ("D1", "WFS", &SHORT),
        ("D1", "DV.DISV", &SHORT),
        ("D1", "MBD.B0", &UCHAR),
        ("D1", "SEL.SELN", &USHORT),
        ("D1", "WFUS", &USHORT),
        ("D2", "LI", &LONG),
        ("D2", "I6", &INT64),
        // bo/mbbo VAL is the `(epicsEnum16)` cast; dbgf's "Illegal_Value"
        // is VAL > 1 stored.
        ("D2", "BO", &USHORT),
        ("D2", "BO2.RVAL", &ULONG),
        ("D2", "MB", &USHORT),
        ("D2", "MBS", &USHORT),
        ("D2", "MBD2", &LONG),
        ("D2", "MBI.RVAL", &ULONG),
        ("D3", "WFL", &LONG),
        ("D3", "WFUL", &ULONG),
        ("D3", "WFI6", &INT64),
        ("D3", "WFUI6", &UINT64),
        ("D3", "AI.RVAL", &LONG),
    ];
    for (i, &v) in FLOATS.iter().enumerate() {
        for src in ["D1", "D2", "D3"] {
            db.put_record_field_from_ca(src, "VAL", EpicsValue::Double(v))
                .await
                .unwrap();
        }
        for (_, pv, want) in table {
            assert_eq!(read(&db, pv), Read::I(want[i]), "{pv} <- {v}");
        }
        // FLOAT: `f64_to_f32`, 1e39 clamped to FLT_MAX, inf kept.
        let float = if v == 1e39 { f32::MAX } else { v as f32 };
        match read(&db, "WFF") {
            Read::F(got) if float.is_nan() => assert!(got.is_nan(), "WFF <- {v}"),
            got => assert_eq!(got, Read::F(float.into()), "WFF <- {v}"),
        }
    }
}

const DB_02B: &str = r#"
record(bo,"BOL"){field(ZNAM,"Off") field(ONAM,"On")}
record(mbbo,"MBL"){}
record(mbbo,"MBSL"){field(ZRST,"zero") field(ONST,"one") field(TWST,"two")}
record(mbboDirect,"MBDL"){}
record(bo,"BOR"){}
record(waveform,"WCL"){field(FTVL,"CHAR") field(NELM,"1")}
record(waveform,"WSL"){field(FTVL,"SHORT") field(NELM,"1")}
record(ao,"AOL"){}
record(bo,"BOI"){}
record(longout,"LOT"){}
record(waveform,"WFI"){field(FTVL,"FLOAT") field(NELM,"1")}
record(ao,"AOI"){}
record(longout,"L1"){field(OUT,"BOL.VAL NPP")}
record(longout,"L2"){field(OUT,"MBL.VAL NPP")}
record(longout,"L3"){field(OUT,"MBSL.VAL NPP")}
record(longout,"L4"){field(OUT,"MBDL.VAL NPP")}
record(longout,"L5"){field(OUT,"BOR.RVAL NPP")}
record(longout,"L6"){field(OUT,"WCL.VAL NPP")}
record(longout,"L7"){field(OUT,"WSL.VAL NPP")}
record(longout,"L8"){field(OUT,"AOL.VAL NPP")}
record(int64out,"J1"){field(OUT,"BOI.VAL NPP")}
record(int64out,"J2"){field(OUT,"LOT.VAL NPP")}
record(int64out,"J3"){field(OUT,"WFI.VAL NPP")}
record(int64out,"J4"){field(OUT,"AOI.VAL NPP")}
"#;

/// An integer source keeps the low bits of the target's width.
#[epics_macros_rs::epics_test]
async fn an_integer_source_keeps_the_low_bits() {
    let db = ioc(DB_02B).await;
    const LONGS: [i32; 9] = [0, 1, 2, 3, -1, 300, 70000, i32::MAX, i32::MIN];
    let table: [(&str, &str, [i128; 9]); 8] = [
        ("L1", "BOL", [0, 1, 2, 3, 65535, 300, 4464, 65535, 0]),
        ("L2", "MBL", [0, 1, 2, 3, 65535, 300, 4464, 65535, 0]),
        ("L3", "MBSL", [0, 1, 2, 3, 65535, 300, 4464, 65535, 0]),
        (
            "L4",
            "MBDL",
            [0, 1, 2, 3, -1, 300, 70000, 2147483647, MIN32],
        ),
        (
            "L5",
            "BOR.RVAL",
            [0, 1, 2, 3, 4294967295, 300, 70000, 2147483647, 2147483648],
        ),
        ("L6", "WCL", [0, 1, 2, 3, -1, 44, 112, -1, 0]),
        ("L7", "WSL", [0, 1, 2, 3, -1, 300, 4464, -1, 0]),
        ("L8", "AOL", [0, 1, 2, 3, -1, 300, 70000, 2147483647, MIN32]),
    ];
    for (i, v) in LONGS.into_iter().enumerate() {
        for (src, pv, want) in table {
            db.put_record_field_from_ca(src, "VAL", EpicsValue::Long(v))
                .await
                .unwrap();
            let got = match read(&db, pv) {
                Read::F(f) => Read::I(f as i128),
                got => got,
            };
            assert_eq!(got, Read::I(want[i]), "{pv} <- {v}");
        }
    }
    const INT64S: [i64; 5] = [2, (1 << 40) + 5, -(1 << 40), i64::MAX, 16777217];
    for v in INT64S {
        for src in ["J1", "J2", "J3", "J4"] {
            db.put_record_field_from_ca(src, "VAL", EpicsValue::Int64(v))
                .await
                .unwrap();
        }
        assert_eq!(read(&db, "BOI"), Read::I((v as u16).into()), "BOI <- {v}");
        assert_eq!(read(&db, "LOT"), Read::I((v as i32).into()), "LOT <- {v}");
        assert_eq!(read(&db, "WFI"), Read::F((v as f32).into()), "WFI <- {v}");
        assert_eq!(read(&db, "AOI"), Read::F(v as f64), "AOI <- {v}");
    }
}

const DB_02C: &str = r#"
record(bo,"BOS"){field(ZNAM,"Off") field(ONAM,"On")}
record(mbbo,"MBSS"){field(ZRST,"zero") field(ONST,"one") field(TWST,"two")}
record(mbbo,"MBNS"){}
record(longout,"LOS"){}
record(ao,"AOS"){}
record(int64out,"I6S"){}
record(waveform,"WFCS"){field(FTVL,"CHAR") field(NELM,"1")}
record(stringout,"S1"){field(OUT,"BOS.VAL NPP")}
record(stringout,"S2"){field(OUT,"MBSS.VAL NPP")}
record(stringout,"S3"){field(OUT,"MBNS.VAL NPP")}
record(stringout,"S4"){field(OUT,"LOS.VAL NPP")}
record(stringout,"S5"){field(OUT,"AOS.VAL NPP")}
record(stringout,"S6"){field(OUT,"I6S.VAL NPP")}
record(stringout,"S7"){field(OUT,"WFCS.VAL NPP")}
"#;

const LINK_ALARM: f64 = 14.0;
const INVALID: f64 = 3.0;
/// The value each target holds before every put, so a refusal shows.
const RESET: f64 = 9.0;
/// `None` is REFUSED.
const R: Option<f64> = None;

/// The C table's columns, S1→BOS … S7→WFCS: `cvt_st_e` (state strings, then
/// an index below `no_str`) and the `cvt_st_*` numeric rows.
#[epics_macros_rs::epics_test]
async fn a_string_source_parses_or_refuses_as_cvt_st() {
    let db = ioc(DB_02C).await;
    const TARGETS: [(&str, &str); 7] = [
        ("S1", "BOS"),
        ("S2", "MBSS"),
        ("S3", "MBNS"),
        ("S4", "LOS"),
        ("S5", "AOS"),
        ("S6", "I6S"),
        ("S7", "WFCS"),
    ];
    let s = Some;
    let table: [(&str, [Option<f64>; 7]); 16] = [
        ("On", [s(1.0), R, R, R, R, R, R]),
        ("Off", [s(0.0), R, R, R, R, R, R]),
        ("7", [R, R, s(7.0), s(7.0), s(7.0), s(7.0), s(7.0)]),
        (
            "1",
            [s(1.0), s(1.0), s(1.0), s(1.0), s(1.0), s(1.0), s(1.0)],
        ),
        ("two", [R, s(2.0), R, R, R, R, R]),
        ("5", [R, R, s(5.0), s(5.0), s(5.0), s(5.0), s(5.0)]),
        ("x", [R; 7]),
        ("abc", [R; 7]),
        ("", [R, s(3.0), R, s(0.0), s(0.0), s(0.0), R]),
        ("12abc", [R, R, s(12.0), s(12.0), s(12.0), s(12.0), s(12.0)]),
        (" 7", [R, R, s(7.0), s(7.0), s(7.0), s(7.0), s(7.0)]),
        ("3.25", [R, R, s(3.0), s(3.0), s(3.25), s(3.0), s(3.0)]),
        ("0x10", [R, R, s(16.0), s(16.0), s(16.0), s(16.0), s(16.0)]),
        (
            "1e3",
            [R, s(1.0), s(1.0), s(1.0), s(1000.0), s(1.0), s(1.0)],
        ),
        ("-3", [R, R, s(65533.0), s(-3.0), s(-3.0), s(-3.0), s(-3.0)]),
        (
            "1 ",
            [s(1.0), s(1.0), s(1.0), s(1.0), s(1.0), s(1.0), s(1.0)],
        ),
    ];
    for (text, wants) in table {
        for ((src, target), want) in TARGETS.into_iter().zip(wants) {
            db.put_pv(target, EpicsValue::Double(RESET)).await.unwrap();
            let _ = db
                .put_record_field_from_ca(src, "VAL", EpicsValue::String(text.into()))
                .await;
            let got = match read(&db, target) {
                Read::I(v) => v as f64,
                Read::F(v) => v,
            };
            let sevr = db.get_pv(format!("{src}.SEVR")).unwrap().to_f64();
            let stat = db.get_pv(format!("{src}.STAT")).unwrap().to_f64();
            match want {
                Some(want) => {
                    assert_eq!(got, want, "{target} <- {text:?}");
                    assert_eq!(sevr, Some(0.0), "{src} <- {text:?}");
                }
                None => {
                    assert_eq!(got, RESET, "{target} <- {text:?} is refused");
                    assert_eq!(
                        (stat, sevr),
                        (Some(LINK_ALARM), Some(INVALID)),
                        "{src} <- {text:?}"
                    );
                }
            }
        }
    }
}
