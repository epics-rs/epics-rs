//! A soft dset's `init_record` loads a constant INP with
//! `recGblInitConstantLink(&prec->inp, dbrType, …)`: the link text parsed at
//! the dset's type, trailing text allowed, out-of-range refused. Expected
//! values are measured on softIoc R7.0.10.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

#[epics_macros_rs::epics_test]
async fn constant_inp_loads_at_the_dsets_type() {
    use EpicsValue::{Double, Enum, Int64, Long, String, UChar, ULong};
    // (record type, DTYP, INP, field, value, UDF)
    let cases: &[(&str, &str, &str, &str, EpicsValue, u8)] = &[
        // `epicsParseInt32` stops at `e`; the Long row does not go via double.
        ("longin", "Soft Channel", "1e3", "VAL", Long(1), 0),
        ("int64in", "Soft Channel", "1e3", "VAL", Int64(1), 0),
        // Out of range is a refused load: VAL untouched, UDF kept.
        ("longin", "Soft Channel", "4294967301", "VAL", Long(0), 1),
        ("bi", "Soft Channel", "65537", "VAL", Enum(0), 1),
        // A choice loads as DBF_USHORT.
        ("mbbi", "Soft Channel", "1.5", "VAL", Enum(1), 0),
        // `cvt_st_UInt32` admits a negative and wraps it.
        (
            "mbbiDirect",
            "Soft Channel",
            "4294967295",
            "VAL",
            Long(-1),
            0,
        ),
        // The Raw dsets load RVAL and leave UDF alone.
        ("ai", "Raw Soft Channel", "1e3", "RVAL", Long(1), 1),
        ("bi", "Raw Soft Channel", "-1", "RVAL", ULong(u32::MAX), 1),
        // devMbbiSoftRaw reads DBR_LONG but loads DBF_ULONG.
        (
            "mbbi",
            "Raw Soft Channel",
            "4294967295",
            "RVAL",
            ULong(u32::MAX),
            1,
        ),
        // `cvt_st_st` copies the text.
        (
            "stringin",
            "Soft Channel",
            "0x10",
            "VAL",
            String("0x10".into()),
            0,
        ),
        ("ai", "Soft Channel", "0x10", "VAL", Double(16.0), 0),
    ];
    let db_text: std::string::String = cases
        .iter()
        .enumerate()
        .map(|(i, (rtype, dtyp, inp, ..))| {
            format!("record({rtype}, \"R{i}\") {{\n field(DTYP, \"{dtyp}\")\n field(INP, \"{inp}\")\n}}\n")
        })
        .collect();
    let (db, _) = IocBuilder::new()
        .db_string(&db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    for (i, (rtype, dtyp, inp, field, value, udf)) in cases.iter().enumerate() {
        let got = (
            db.get_pv(format!("R{i}.{field}")).unwrap(),
            db.get_pv(format!("R{i}.UDF")).unwrap(),
        );
        assert_eq!(
            got,
            (value.clone(), UChar(*udf)),
            "{rtype} {dtyp} INP \"{inp}\""
        );
    }
}
