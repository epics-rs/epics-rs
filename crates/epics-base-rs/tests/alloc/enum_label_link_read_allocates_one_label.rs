//! A `DBR_STRING` link read of an enum VAL renders one state label: a
//! stringin reading an mbbi with 16 states, or a bo, allocates no more per
//! process than one reading another stringin.
//! Counted with the alloc binary's allocator over 1000 processes; only the
//! calling thread's allocations inside `count` are seen.

use std::collections::HashMap;

use alloc_count::thread;
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;

const DB: &str = r#"
record(mbbi, "M") {
    field(VAL, "3")
    field(ZRST, "s0") field(ONST, "s1") field(TWST, "s2") field(THST, "s3")
    field(FRST, "s4") field(FVST, "s5") field(SXST, "s6") field(SVST, "s7")
    field(EIST, "s8") field(NIST, "s9") field(TEST, "s10") field(ELST, "s11")
    field(TVST, "s12") field(TTST, "s13") field(FTST, "s14") field(FFST, "s15")
}
record(bo, "B") { field(VAL, "1") field(ZNAM, "off") field(ONAM, "on") }
record(stringin, "S") { field(VAL, "s3") }
record(stringin, "SI_M") { field(INP, "M NPP") }
record(stringin, "SI_B") { field(INP, "B NPP") }
record(stringin, "SI_S") { field(INP, "S NPP") }
"#;

async fn allocs_over_1000(db: &PvDatabase, rec: &str) -> u64 {
    let mut stack = ProcStack::new();
    db.process_record_with_links(rec, &mut stack).await.unwrap();
    let window = thread::open();
    for _ in 0..1000 {
        let mut stack = ProcStack::new();
        db.process_record_with_links(rec, &mut stack).await.unwrap();
    }
    let allocs = window.close().allocs;
    assert!(
        epics_base_rs::runtime::task::in_test_ioc_thread(),
        "counted off an IOC thread"
    );
    allocs
}

#[test]
fn a_string_read_of_an_enum_val_allocates_as_a_string_source() {
    epics_base_rs::runtime::task::test_ioc_thread(|| {
        epics_base_rs::runtime::task::test_block_on(
            a_string_read_of_an_enum_val_allocates_as_a_string_source_body(),
        )
    });
}

async fn a_string_read_of_an_enum_val_allocates_as_a_string_source_body() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    // 20 and 6 while the read built the whole `EnumStringForm`.
    for rec in ["SI_S", "SI_M", "SI_B"] {
        assert_eq!(allocs_over_1000(&db, rec).await, 2 * 1000, "{rec}");
    }
}
