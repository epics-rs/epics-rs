//! An aSub reading its own fields as STRING renders at its own PREC and menu
//! labels without re-taking itself (it is held for writing across the
//! fetch); pinned to softIoc R7.0.10.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(aSub, "AS") {
    field(FTVA, "DOUBLE") field(NOVA, "3") field(PREC, "2")
    field(FTE, "STRING") field(NOE, "3") field(INPE, "AS.VALA")
    field(FTF, "STRING") field(INPF, "AS.VALA")
    field(FTG, "STRING") field(INPG, "AS.SCAN")
}
"#;

fn strings(v: EpicsValue) -> Vec<String> {
    match v {
        EpicsValue::StringArray(a) => a.iter().map(|s| s.as_str_lossy().into_owned()).collect(),
        EpicsValue::String(s) => vec![s.as_str_lossy().into_owned()],
        other => panic!("{other:?}"),
    }
}

#[epics_macros_rs::epics_test]
async fn an_asub_self_link_renders_strings_like_softioc() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    db.put_record_field_from_ca(
        "AS",
        "VALA",
        EpicsValue::DoubleArray(vec![1.5, 2.25, 3.0].into()),
    )
    .await
    .unwrap();
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links("AS", &mut visited)
        .await
        .unwrap();
    assert_eq!(
        strings(db.get_pv("AS.E").unwrap()),
        ["1.50", "2.25", "3.00"]
    );
    assert_eq!(strings(db.get_pv("AS.F").unwrap()), ["1.50"]);
    assert_eq!(strings(db.get_pv("AS.G").unwrap()), ["Passive"]);
}
