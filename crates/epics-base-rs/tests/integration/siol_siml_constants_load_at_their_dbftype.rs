//! `init_record`'s constant SIOL/SIML loads are `dbLoadLink` at the type the
//! call names: `DBF_USHORT` for SIML (`recGbl.c:443`), and for SIOL the
//! record's own `recGblInitConstantLink` type, which for bi/mbbi is
//! `DBF_USHORT` into a ULONG SVAL. softIoc 8f5015b66, `dbgf` after `iocInit`:
//!
//! ```text
//! longin   SIOL "5e9"    SVAL 5
//! int64in  SIOL "1e19"   SVAL 1
//! bi       SIOL "70000"  SVAL 0   (epicsParseUInt16 refuses)
//! mbbi     SIOL "70000"  SVAL 0
//! mbbiDirect SIOL "-1"   SVAL -1
//! ai       SIML "1e5"    SIMM YES
//! ai       SIML "65537"  SIMM NO
//! ```

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;

const DB: &str = r#"
record(longin, "SL") { field(SIOL, "5e9") }
record(int64in, "SQ") { field(SIOL, "1e19") }
record(bi, "SB") { field(SIOL, "70000") }
record(mbbi, "SN") { field(SIOL, "70000") }
record(mbbiDirect, "SM") { field(SIOL, "-1") }
record(ai, "SA") { field(SIML, "1e5") }
record(ai, "SC") { field(SIML, "65537") }
"#;

#[epics_macros_rs::epics_test]
async fn a_constant_siol_or_siml_loads_at_its_calls_dbftype() {
    let (db, _h) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let got: Vec<_> = [
        "SL.SVAL", "SQ.SVAL", "SB.SVAL", "SN.SVAL", "SM.SVAL", "SA.SIMM", "SC.SIMM",
    ]
    .iter()
    .map(|pv| db.get_pv(pv).ok())
    .collect();
    assert_eq!(
        got,
        [
            Some(EpicsValue::Long(5)),
            Some(EpicsValue::Int64(1)),
            Some(EpicsValue::ULong(0)),
            Some(EpicsValue::ULong(0)),
            Some(EpicsValue::Long(-1)),
            Some(EpicsValue::Short(1)),
            Some(EpicsValue::Short(0)),
        ]
    );
}
