//! A `.db` CALC is stored as text: C's `dbPutString` only checks that it
//! compiles (`dbStaticLib.c:2592`), and `init_record` writes CLCV in pass 1
//! (`swaitRecord.c:302`, after the pass-0 return at :280). Compiling into
//! CLCV at load overwrote a `.db` CLCV.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::EpicsValue;

const DB: &str = "record(swait, \"W\") {\n  field(CLCV, \"7\")\n  field(CALC, \"A+1\")\n}\n";

fn builder() -> IocBuilder {
    IocBuilder::new()
        .register_record_type("swait", || -> Box<dyn Record> {
            Box::new(SwaitRecord::default())
        })
        .db_string(DB, &HashMap::new())
        .unwrap()
}

#[epics_macros_rs::epics_test]
async fn a_loaded_calc_leaves_clcv_until_pass_1() {
    let loaded = builder().load().await.unwrap();
    let got = loaded.get_pv("W.CLCV").unwrap();
    assert_eq!(got.to_f64(), Some(7.0), "after load read {got:?}");
    assert_eq!(
        loaded.get_pv("W.CALC").unwrap(),
        EpicsValue::String("A+1".into())
    );

    let (built, _) = builder().build().await.unwrap();
    let got = built.get_pv("W.CLCV").unwrap();
    assert_eq!(got.to_f64(), Some(0.0), "after iocInit read {got:?}");
}

#[test]
fn pass_0_does_not_compile() {
    let mut w = SwaitRecord::default();
    w.load_field("CALC", EpicsValue::String("1+".into()))
        .unwrap();
    w.init_record(0).unwrap();
    assert_eq!(w.clcv, 0, "between the passes");
    w.init_record(1).unwrap();
    assert_eq!(w.clcv, -1, "after pass 1");
}
