//! `selRecord.c:102-105` sets A..L to NaN in `init_record` pass 1 and only
//! then loads each constant INPx, so a `.db` A..L value survives only
//! behind a constant link. softIoc R7.0.10.1, no process:
//!
//! ```text
//! A 7 INPA SRC (DB) -> nan     B (unset) INPB 3 -> 3
//! C 5 (no INPC)     -> nan     D 4 INPD 9       -> 9     E -> nan
//! ```

use epics_base_rs::server::ioc_builder::IocBuilder;

const DB: &str = r#"
record(ai, "SRC") { field(VAL, "1") }
record(sel, "S") { field(A, "7") field(INPA, "SRC") field(INPB, "3") field(C, "5") field(D, "4") field(INPD, "9") }
"#;

#[epics_macros_rs::epics_test]
async fn init_writes_nan_before_the_constant_links() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let a = |f: &str| db.get_pv(format!("S.{f}")).unwrap().to_f64().unwrap();
    assert!(a("A").is_nan(), "A {}", a("A"));
    assert_eq!(a("B"), 3.0);
    assert!(a("C").is_nan(), "C {}", a("C"));
    assert_eq!(a("D"), 9.0);
    assert!(a("E").is_nan(), "E {}", a("E"));
}
