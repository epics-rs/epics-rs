//! A soft input dset names the `dbrType` of its INP `dbGetLink`, so one record
//! type reads the same source differently under each soft DTYP. Each record
//! is read twice, through `fetch_input_stage`'s held-target read (`NPP`) and
//! its general read (`PP`), which must ask the same type.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const SOURCES: &str = r#"
record(int64in, "BIG") {
    field(VAL, "4294967301")
}
record(stringin, "HEX") {
    field(VAL, "0x10")
}
record(longin, "NEG") {
    field(VAL, "-1")
}
record(bi, "STATE") {
    field(ZNAM, "Low")
    field(ONAM, "High")
    field(VAL, "1")
}
record(stringin, "EMPTY") {
}
record(stringin, "WORD") {
    field(VAL, "abc")
}
"#;

async fn ioc(records: &str) -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .db_string(&format!("{SOURCES}{records}"), &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

/// Process `name` and return `fields` and its STAT.
async fn read_after_process(db: &PvDatabase, name: &str, fields: &[&str]) -> Vec<EpicsValue> {
    db.process_record(name).await.unwrap();
    let rec = db.get_record(name).unwrap();
    let inst = rec.read();
    let mut out: Vec<_> = fields
        .iter()
        .map(|f| inst.record.get_field(f).unwrap())
        .collect();
    out.push(EpicsValue::Short(inst.common.stat as i16));
    out
}

/// Both paths answer the same, and that answer is `expect` (STAT last).
async fn assert_both_paths(
    rtype: &str,
    dtyp: &str,
    source: &str,
    fields: &[&str],
    expect: &[EpicsValue],
) {
    let db = ioc(&format!(
        r#"record({rtype}, "HELD") {{
    field(DTYP, "{dtyp}")
    field(INP, "{source} NPP")
}}
record({rtype}, "GENERAL") {{
    field(DTYP, "{dtyp}")
    field(INP, "{source} PP")
}}"#
    ))
    .await;
    let held = read_after_process(&db, "HELD", fields).await;
    let general = read_after_process(&db, "GENERAL", fields).await;
    assert_eq!(
        held, general,
        "{rtype} {dtyp} from {source}: held vs general"
    );
    assert_eq!(held, expect, "{rtype} {dtyp} from {source}");
}

const NO_ALARM: EpicsValue = EpicsValue::Short(0);
/// `LINK_ALARM`: `dbGetLink` failed, so `setLinkAlarm` ran.
const LINK: EpicsValue = EpicsValue::Short(14);

/// devAiSoft reads `DBR_DOUBLE` into VAL, devAiSoftRaw `DBR_LONG` into RVAL:
/// an int64 outside i32 keeps its value in one and wraps in the other, and
/// both parse `"0x10"` as 16.
#[epics_macros_rs::epics_test]
async fn ai_reads_inp_at_its_dsets_type() {
    use EpicsValue::{Double, Long};
    assert_both_paths(
        "ai",
        "Soft Channel",
        "BIG",
        &["VAL"],
        &[Double(4294967301.0), NO_ALARM],
    )
    .await;
    assert_both_paths(
        "ai",
        "Async Soft Channel",
        "BIG",
        &["VAL"],
        &[Double(4294967301.0), NO_ALARM],
    )
    .await;
    assert_both_paths(
        "ai",
        "Raw Soft Channel",
        "BIG",
        &["RVAL", "VAL"],
        &[Long(5), Double(5.0), NO_ALARM],
    )
    .await;
    assert_both_paths(
        "ai",
        "Soft Channel",
        "HEX",
        &["VAL"],
        &[Double(16.0), NO_ALARM],
    )
    .await;
    assert_both_paths(
        "ai",
        "Raw Soft Channel",
        "HEX",
        &["RVAL", "VAL"],
        &[Long(16), Double(16.0), NO_ALARM],
    )
    .await;
    // `cvt_st_d` / `cvt_st_l`: the empty string is a successful 0, a word is
    // a failed read.
    for dtyp in ["Soft Channel", "Raw Soft Channel"] {
        assert_both_paths("ai", dtyp, "EMPTY", &["VAL"], &[Double(0.0), NO_ALARM]).await;
        assert_both_paths("ai", dtyp, "WORD", &["VAL"], &[Double(0.0), LINK]).await;
    }
}

/// devBiSoft reads `DBR_USHORT` into VAL, devBiSoftRaw `DBR_ULONG` into RVAL
/// and devBiSoftCallback `DBR_ENUM`, whose string row asks the source record
/// for a choice a stringin does not have.
#[epics_macros_rs::epics_test]
async fn bi_reads_inp_at_its_dsets_type() {
    use EpicsValue::{Enum, ULong};
    for (dtyp, source, expect) in [
        ("Soft Channel", "NEG", vec![Enum(65535), NO_ALARM]),
        ("Async Soft Channel", "NEG", vec![Enum(65535), NO_ALARM]),
        ("Soft Channel", "HEX", vec![Enum(16), NO_ALARM]),
        ("Async Soft Channel", "HEX", vec![Enum(0), LINK]),
        ("Soft Channel", "WORD", vec![Enum(0), LINK]),
    ] {
        assert_both_paths("bi", dtyp, source, &["VAL"], &expect).await;
    }
    for (source, expect) in [
        ("NEG", vec![ULong(u32::MAX), Enum(1), NO_ALARM]),
        ("HEX", vec![ULong(16), Enum(1), NO_ALARM]),
        ("WORD", vec![ULong(0), Enum(0), LINK]),
    ] {
        assert_both_paths("bi", "Raw Soft Channel", source, &["RVAL", "VAL"], &expect).await;
    }
}

/// devMbbiSoft reads `DBR_USHORT`, devMbbiSoftRaw `DBR_LONG` into RVAL and
/// devMbbiSoftCallback `DBR_ENUM`.
#[epics_macros_rs::epics_test]
async fn mbbi_reads_inp_at_its_dsets_type() {
    use EpicsValue::{Enum, ULong};
    for (dtyp, source, expect) in [
        ("Soft Channel", "NEG", vec![Enum(65535), NO_ALARM]),
        ("Async Soft Channel", "NEG", vec![Enum(65535), NO_ALARM]),
        ("Soft Channel", "HEX", vec![Enum(16), NO_ALARM]),
        ("Async Soft Channel", "HEX", vec![Enum(0), LINK]),
        ("Soft Channel", "WORD", vec![Enum(0), LINK]),
    ] {
        assert_both_paths("mbbi", dtyp, source, &["VAL"], &expect).await;
    }
    for (source, expect) in [
        ("NEG", vec![ULong(u32::MAX), NO_ALARM]),
        ("HEX", vec![ULong(16), NO_ALARM]),
        ("WORD", vec![ULong(0), LINK]),
    ] {
        assert_both_paths("mbbi", "Raw Soft Channel", source, &["RVAL"], &expect).await;
    }
}

/// devMbbiDirectSoft, its Raw and its Callback twin all read `DBR_ULONG`.
#[epics_macros_rs::epics_test]
async fn mbbi_direct_reads_inp_at_its_dsets_type() {
    use EpicsValue::{Long, ULong};
    for dtyp in ["Soft Channel", "Async Soft Channel"] {
        for (source, expect) in [
            ("NEG", vec![Long(-1), NO_ALARM]),
            ("HEX", vec![Long(16), NO_ALARM]),
            ("WORD", vec![Long(0), LINK]),
        ] {
            assert_both_paths("mbbiDirect", dtyp, source, &["VAL"], &expect).await;
        }
    }
    for (source, expect) in [
        ("NEG", vec![ULong(u32::MAX), NO_ALARM]),
        ("HEX", vec![ULong(16), NO_ALARM]),
        ("WORD", vec![ULong(0), LINK]),
    ] {
        assert_both_paths("mbbiDirect", "Raw Soft Channel", source, &["RVAL"], &expect).await;
    }
}

/// devSiSoft and devSiSoftCallback read `DBR_STRING`, so an enum source
/// delivers its state label.
#[epics_macros_rs::epics_test]
async fn stringin_reads_inp_at_its_dsets_type() {
    for dtyp in ["Soft Channel", "Async Soft Channel"] {
        assert_both_paths(
            "stringin",
            dtyp,
            "STATE",
            &["VAL"],
            &[EpicsValue::String("High".into()), NO_ALARM],
        )
        .await;
    }
}

/// devEventSoft reads `DBR_STRING`: an enum source names the event by its
/// state label.
#[epics_macros_rs::epics_test]
async fn event_reads_inp_at_its_dsets_type() {
    assert_both_paths(
        "event",
        "Soft Channel",
        "STATE",
        &["VAL"],
        &[EpicsValue::String("High".into()), NO_ALARM],
    )
    .await;
}

/// devLiSoft and devLiSoftCallback read `DBR_LONG`: an int64 outside i32
/// wraps, and a word is a failed read.
#[epics_macros_rs::epics_test]
async fn longin_reads_inp_at_its_dsets_type() {
    use EpicsValue::Long;
    for dtyp in ["Soft Channel", "Async Soft Channel"] {
        for (source, expect) in [
            ("BIG", vec![Long(5), NO_ALARM]),
            ("HEX", vec![Long(16), NO_ALARM]),
            ("EMPTY", vec![Long(0), NO_ALARM]),
            ("WORD", vec![Long(0), LINK]),
        ] {
            assert_both_paths("longin", dtyp, source, &["VAL"], &expect).await;
        }
    }
}

/// devI64inSoft and devI64inSoftCallback read `DBR_INT64`: an int64 outside
/// i32 keeps its value, and a word is a failed read.
#[epics_macros_rs::epics_test]
async fn int64in_reads_inp_at_its_dsets_type() {
    use EpicsValue::Int64;
    for dtyp in ["Soft Channel", "Async Soft Channel"] {
        for (source, expect) in [
            ("BIG", vec![Int64(0x1_0000_0005), NO_ALARM]),
            ("NEG", vec![Int64(-1), NO_ALARM]),
            ("HEX", vec![Int64(16), NO_ALARM]),
            ("EMPTY", vec![Int64(0), NO_ALARM]),
            ("WORD", vec![Int64(0), LINK]),
        ] {
            assert_both_paths("int64in", dtyp, source, &["VAL"], &expect).await;
        }
    }
}
