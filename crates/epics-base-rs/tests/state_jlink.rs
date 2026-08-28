//! The `state` jlink — C `lnkStateIf` (`lnkState.c:226-232`).
//!
//! A `{state:"NAME"}` link addresses one named `dbState` bit; a leading `!`
//! inverts BOTH directions of that one link. C's five jlink types are
//! `const`, `calc`, `state`, `debug` and `trace`; this port now reads and
//! writes the first three. `debug` and `trace` are lset *wrappers* — they
//! forward every lset method to a child link (`lnkDebug.c`, `lnkTrace.c`) —
//! and this port dispatches links by `ParsedLink` variant rather than through
//! a per-link vtable, so there is nothing for them to wrap. They stay absent.
//!
//! Every expectation below is a measurement of
//! `/home/stevek/work/epics-base/bin/linux-x86_64/softIoc` (R7.0.10-146)
//! driving these exact records, not a reading of the C.
//!
//! The one case a test written against states it creates itself cannot see:
//! **a state that does not exist yet**. C `lnkState_open` (`lnkState.c:110-116`)
//! calls `dbStateCreate(slink->name)`, and `dbStateCreate` (`dbState.c:50-66`)
//! is find-or-create — an unknown name is CREATED when the link opens at
//! `iocInit`, zeroed by `calloc`, so it reads FALSE and is never an error.
//! Measured: `dbStateShowAll 1` prints nothing after `dbLoadRecords` alone,
//! and after `iocInit`, with none of the records ever processed, lists every
//! name any link mentions, all FALSE. See `an_untouched_state_reads_false`
//! and `every_named_state_exists_after_ioc_init`.

use std::collections::HashSet;

use epics_base_rs::server::database::filters::sync::db_state_registry;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(bo,  "S:SET")   { field(OUT, "{state:\"GREEN\"}") }
record(ai,  "S:GET")   { field(INP, "{state:\"GREEN\"}") }
record(ai,  "S:NGET")  { field(INP, "{state:\"!GREEN\"}") }
record(bo,  "S:NSET")  { field(OUT, "{state:\"!BLUE\"}") }
record(ai,  "S:BGET")  { field(INP, "{state:\"BLUE\"}") }
record(ai,  "S:UNSEEN") { field(INP, "{state:\"NEVERTOUCHED\"}") }
record(stringout, "S:STR") { field(OUT, "{state:\"WORD\"}") }
record(ai,  "S:WGET")  { field(INP, "{state:\"WORD\"}") }
record(ai,  "S:EMPTY") { field(INP, "{state:\"\"}") }
record(ai,  "S:BANG")  { field(INP, "{state:\"!\"}") }
"#;

async fn build() -> std::sync::Arc<epics_base_rs::server::database::PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &epics_base_rs::server::database::PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

/// Put to an output record's VAL and process it, which is what a
/// `dbpf`/`caput` to a `pp(TRUE)` VAL does — [`PvDatabase::put_pv`] is the
/// bare `dbPut` and drives no OUT link on its own.
async fn write(db: &epics_base_rs::server::database::PvDatabase, rec: &str, value: EpicsValue) {
    db.put_pv(rec, value).await.unwrap();
    process(db, rec).await;
}

/// Process the reader and answer its VAL.
async fn read(db: &epics_base_rs::server::database::PvDatabase, rec: &str) -> f64 {
    process(db, rec).await;
    db.get_pv(rec)
        .unwrap_or_else(|e| panic!("{rec} has no VAL: {e}"))
        .to_f64()
        .unwrap_or_else(|| panic!("{rec}.VAL is not numeric"))
}

/// The plain direction, both boundary values of the bit.
///
/// softIoc: `S:GET` reads 0, then 1 after `dbpf S:SET 1`.
#[epics_macros_rs::epics_test]
async fn a_state_link_reads_and_writes_the_named_bit() {
    let db = build().await;

    assert_eq!(read(&db, "S:GET").await, 0.0, "an unset state is FALSE");

    write(&db, "S:SET", EpicsValue::Enum(1)).await;
    assert_eq!(read(&db, "S:GET").await, 1.0);
    assert!(db_state_registry().get("GREEN"));

    write(&db, "S:SET", EpicsValue::Enum(0)).await;
    assert_eq!(read(&db, "S:GET").await, 0.0);
    assert!(!db_state_registry().get("GREEN"));
}

/// `!` is one flag on the link, read AND write, not one per direction.
///
/// C `lnkState_getValue` (`lnkState.c:139-151`) returns
/// `slink->invert ^ dbStateGet(...)` and `lnkState_putValue` (`:153-203`)
/// ends `(val ^ invert) ? dbStateSet : dbStateClear` — the same `invert`.
/// softIoc: `dbpf S:NSET 0` leaves BLUE **TRUE**, `dbpf S:NSET 1` leaves it
/// FALSE, and `S:NGET` answers the complement of `S:GET` throughout.
#[epics_macros_rs::epics_test]
async fn a_leading_bang_inverts_both_directions_of_one_link() {
    let db = build().await;

    // Read side: the complement, at both boundary values.
    assert_eq!(read(&db, "S:NGET").await, 1.0);
    write(&db, "S:SET", EpicsValue::Enum(1)).await;
    assert_eq!(read(&db, "S:NGET").await, 0.0);

    // Write side: a put of 0 through an inverted link SETS the bit.
    write(&db, "S:NSET", EpicsValue::Enum(0)).await;
    assert_eq!(read(&db, "S:BGET").await, 1.0, "0 ^ 1 = 1 → dbStateSet");

    write(&db, "S:NSET", EpicsValue::Enum(1)).await;
    assert_eq!(read(&db, "S:BGET").await, 0.0, "1 ^ 1 = 0 → dbStateClear");
}

/// A string put is textual, not numeric: C's own comment at `lnkState.c:180`
/// is `Only "" and "0" are FALSE`, so `"00"` and `"false"` are both TRUE.
///
/// softIoc, through a `stringout` whose OUT is the state link:
/// `"false"` → 1, `"0"` → 0, `"00"` → 1, `""` → 0.
#[epics_macros_rs::epics_test]
async fn a_string_put_is_false_only_when_empty_or_exactly_zero() {
    let db = build().await;

    for (put, expect) in [("false", 1.0), ("0", 0.0), ("00", 1.0), ("", 0.0)] {
        write(&db, "S:STR", EpicsValue::String(put.into())).await;
        assert_eq!(
            read(&db, "S:WGET").await,
            expect,
            "stringout put {put:?} through a state link"
        );
    }
}

/// The case a self-created state cannot witness: a name nothing has ever
/// written. C creates it when the LINK OPENS at `iocInit` — `lnkState_open`
/// (`lnkState.c:110-116`) is `dbStateCreate`, and `dbStateCreate`
/// (`dbState.c:50-66`) is find-or-create — not when the link is first used.
/// Measured on softIoc R7.0.10 with this database: `dbStateShowAll 1` after
/// `dbLoadRecords` alone prints nothing, and after `iocInit`, with no record
/// having processed, it lists every name any link mentions, all FALSE.
#[epics_macros_rs::epics_test]
async fn an_untouched_state_reads_false() {
    let db = build().await;

    assert!(
        db_state_registry().find("NEVERTOUCHED").is_some(),
        "the link's open at iocInit created the state, as C's dbStateCreate does"
    );
    assert!(!db_state_registry().get("NEVERTOUCHED"));
    assert_eq!(read(&db, "S:UNSEEN").await, 0.0);
}

/// Opening at `iocInit` is what makes the registry agree with C for records
/// that have not processed: every name any link in the database mentions is
/// present and FALSE, including the two degenerate ones C also creates —
/// `{state:""}` names a state whose name is empty, and `{state:"!"}` a state
/// literally named `!`, because the inversion rule needs `len > 1`
/// (`lnkState_string`, `lnkState.c:79-89`).
#[epics_macros_rs::epics_test]
async fn every_named_state_exists_after_ioc_init() {
    let _db = build().await;

    for name in ["GREEN", "BLUE", "NEVERTOUCHED", "WORD", "", "!"] {
        assert!(
            db_state_registry().find(name).is_some(),
            "no record processed, but C lists {name:?} from iocInit onward"
        );
        assert!(!db_state_registry().get(name), "{name:?} starts FALSE");
    }
}

/// The link is a JSON link and reports itself as one, so `dbDumpLink`'s
/// per-type census counts it — that census is what says which of C's five
/// jlink types this port implements.
#[epics_macros_rs::epics_test]
async fn a_state_link_reports_itself_as_a_json_link() {
    let db = build().await;

    let rec = db.get_record("S:GET").unwrap();
    let parsed = rec.read().parsed_inp.clone();
    assert!(
        matches!(parsed, epics_base_rs::server::record::ParsedLink::State(_)),
        "{parsed:?}"
    );
    assert_eq!(
        parsed.db_link_type(),
        epics_base_rs::server::record::DbLinkType::JsonLink
    );
}
