use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::{
    ai::AiRecord, ao::AoRecord, bi::BiRecord, bo::BoRecord, busy::BusyRecord, event::EventRecord,
    histogram::HistogramRecord, int64in::Int64inRecord, int64out::Int64outRecord,
    longin::LonginRecord, longout::LongoutRecord, lsi::LsiRecord, lso::LsoRecord, mbbi::MbbiRecord,
    mbbi_direct::MbbiDirectRecord, mbbo::MbboRecord, mbbo_direct::MbboDirectRecord,
    stringin::StringinRecord, stringout::StringoutRecord, swait::SwaitRecord,
    waveform::WaveformRecord,
};
use epics_base_rs::types::EpicsValue;

fn sim_records() -> Vec<Box<dyn Record>> {
    vec![
        Box::new(AiRecord::default()),
        Box::new(AoRecord::default()),
        Box::new(BiRecord::default()),
        Box::new(BoRecord::default()),
        Box::new(BusyRecord::default()),
        Box::new(EventRecord::default()),
        Box::new(HistogramRecord::default()),
        Box::new(Int64inRecord::default()),
        Box::new(Int64outRecord::default()),
        Box::new(LonginRecord::default()),
        Box::new(LongoutRecord::default()),
        Box::new(LsiRecord::default()),
        Box::new(LsoRecord::default()),
        Box::new(MbbiRecord::default()),
        Box::new(MbbiDirectRecord::default()),
        Box::new(MbboRecord::default()),
        Box::new(MbboDirectRecord::default()),
        Box::new(StringinRecord::default()),
        Box::new(StringoutRecord::default()),
        Box::new(SwaitRecord::default()),
        Box::new(WaveformRecord::default()),
    ]
}

#[test]
fn sim_links_are_lent_as_the_text_get_field_serves() {
    let mut failures = Vec::new();
    for mut rec in sim_records() {
        let rt = rec.record_type();
        if rec.link_text_ref("SIML") != Some("") || rec.link_text_ref("SIOL") != Some("") {
            failures.push(format!(
                "{rt}: unset SIML/SIOL lent as {:?}/{:?}",
                rec.link_text_ref("SIML"),
                rec.link_text_ref("SIOL")
            ));
            continue;
        }
        if let Err(e) = rec.put_field("SIOL", EpicsValue::String("OTHER.VAL".into())) {
            failures.push(format!("{rt}: put SIOL refused: {e}"));
            continue;
        }
        let owned = match rec.get_field("SIOL") {
            Some(EpicsValue::String(s)) => s.as_str_lossy().into_owned(),
            other => {
                failures.push(format!("{rt}: get_field SIOL is {other:?}"));
                continue;
            }
        };
        let lent = rec.link_text_ref("SIOL");
        if lent != Some("OTHER.VAL") || lent != Some(owned.as_str()) {
            failures.push(format!("{rt}: SIOL lent {lent:?}, get_field {owned:?}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
