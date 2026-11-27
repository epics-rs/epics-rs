//! calcout, scalcout and acalcout compile CALC/OCAL into CLCV/OCLV in
//! `init_record` pass 1, after the pass-0 return (`calcoutRecord.c:139-142`,
//! `sCalcoutRecord.c:237`, `aCalcoutRecord.c:192`); calcout seeds PVAL from
//! VAL there too (`calcoutRecord.c:216`).

use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::types::EpicsValue;

fn read(rec: &dyn Record, field: &str) -> Option<f64> {
    rec.get_field(field).and_then(|v| v.to_f64())
}

#[test]
fn pass_0_compiles_nothing() {
    let recs: [(&str, Box<dyn Record>); 3] = [
        ("calcout", Box::new(CalcoutRecord::default())),
        ("scalcout", Box::new(ScalcoutRecord::default())),
        ("acalcout", Box::new(AcalcoutRecord::default())),
    ];
    for (rtype, mut rec) in recs {
        for field in ["CALC", "OCAL"] {
            rec.load_field(field, EpicsValue::String("A+".into()))
                .unwrap();
        }
        rec.init_record(0).unwrap();
        for field in ["CLCV", "OCLV"] {
            assert_eq!(
                read(&*rec, field),
                Some(0.0),
                "{rtype}.{field} after pass 0"
            );
        }
        rec.init_record(1).unwrap();
        for field in ["CLCV", "OCLV"] {
            assert_ne!(
                read(&*rec, field),
                Some(0.0),
                "{rtype}.{field} after pass 1"
            );
        }
    }
}

#[test]
fn calcout_seeds_pval_in_pass_1() {
    let mut rec = CalcoutRecord::default();
    rec.load_field("VAL", EpicsValue::Double(5.0)).unwrap();
    rec.init_record(0).unwrap();
    assert_eq!(read(&rec, "PVAL"), Some(0.0), "after pass 0");
    rec.init_record(1).unwrap();
    assert_eq!(read(&rec, "PVAL"), Some(5.0), "after pass 1");
}
