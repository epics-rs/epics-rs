//! calc compiles CALC into RPCL in `init_record` pass 1 (`calcRecord.c:105`,
//! after the pass-0 return at :98): between the passes the record has no
//! program, and a cycle run then evaluates nothing.

use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::types::EpicsValue;

#[test]
fn pass_0_leaves_no_program() {
    let run = |passes: &[u8]| {
        let mut rec = CalcRecord::default();
        rec.load_field("CALC", EpicsValue::String("A+1".into()))
            .unwrap();
        rec.load_field("A", EpicsValue::Double(1.0)).unwrap();
        for &pass in passes {
            rec.init_record(pass).unwrap();
        }
        let _ = rec.process();
        rec.get_field("VAL").and_then(|v| v.to_f64())
    };
    assert_eq!(run(&[0]), Some(0.0), "after pass 0");
    assert_eq!(run(&[0, 1]), Some(2.0), "after pass 1");
}
