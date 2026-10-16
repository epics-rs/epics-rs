//! `Record::seq_group_cells` is what the seq dispatch reads DLYn/DOn
//! through; it must hand back exactly what `get_field` serves for them.

use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::seq::SeqRecord;
use epics_base_rs::types::EpicsValue;

#[test]
fn every_group_answers_its_own_dly_and_do() {
    let mut rec = SeqRecord::new();
    for (i, g) in "0123456789ABCDEF".chars().enumerate() {
        rec.put_field(&format!("DLY{g}"), EpicsValue::Double(i as f64 + 0.25))
            .unwrap();
        rec.put_field(&format!("DO{g}"), EpicsValue::Double(-(i as f64) - 0.5))
            .unwrap();
    }
    for (i, g) in "0123456789ABCDEF".chars().enumerate() {
        let by_name = |f: String| rec.get_field(&f).and_then(|v| v.to_f64()).unwrap();
        assert_eq!(
            rec.seq_group_cells(i),
            Some((by_name(format!("DLY{g}")), by_name(format!("DO{g}")))),
            "group {g}"
        );
    }
    assert_eq!(rec.seq_group_cells(16), None);
}
