//! A waveform FTVL STRING reading a `$` source takes `dbGetLink`'s GET row,
//! one decimal string per byte. Pinned to softIoc R7.0.10.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(lsi, "L100") { field(SIZV, "100") field(INP, {const:"012"}) }
record(waveform, "W") { field(FTVL, "STRING") field(NELM, "10") field(INP, "L100.VAL$") }
record(waveform, "W1") { field(FTVL, "STRING") field(NELM, "1") field(INP, "L100.VAL$") }
"#;

#[epics_macros_rs::epics_test]
async fn a_string_waveform_reads_a_char_source_one_element_per_byte_like_softioc() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    for (rec, want) in [("W", &["48", "49", "50", "0"][..]), ("W1", &["48"][..])] {
        let mut visited = epics_base_rs::server::database::ProcStack::new();
        db.process_record_with_links(rec, &mut visited)
            .await
            .unwrap();
        let Some(EpicsValue::StringArray(got)) = db.get_pv(rec).ok() else {
            panic!("{rec} is not a string array");
        };
        let got: Vec<String> = got.iter().map(|s| s.as_str_lossy().into_owned()).collect();
        assert_eq!(got, want, "{rec}");
    }
}
