//! R18-2: a CONSTANT input link delivers NOTHING at process time — on EVERY
//! reader, including the `ProcessAction::ReadDbLink` executor.
//!
//! C `dbGetLink` on a constant lands in `dbConstGetValue`
//! (`dbConstLink.c:219-225`):
//!
//! ```c
//! static long dbConstGetValue(struct link *plink, short dbrType, void *pbuffer,
//!         long *pnRequest)
//! {
//!     if (pnRequest) *pnRequest = 0;
//!     return 0;
//! }
//! ```
//!
//! — SUCCESS with nothing written, so the reader's field keeps what it holds.
//! The constant reaches the record exactly once, at `init_record`, through
//! `recGblInitConstantLink` (sseq's `SELL`: `sseqRecord.c:186-191`).
//!
//! The port had two readers and one classifier: `read_link_with_alarm`
//! classified a constant as `LinkFetch::NoData`, but the `ReadDbLink` executor
//! read through `read_link_value_as`, which handed the parsed constant back as
//! a live value on every cycle. Boundaries below: the constant's value at init
//! vs after a client put (sseq `SELL`), a constant with no init seed at all
//! (compress `INP`), and the real-link owner path that must keep delivering.

// RTEMS-EXEC-MODEL-ALLOW(4): checked - these run and pass in the feature-ON suite.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(sseq, "SEQ:CONST") {
    field(SELM, "Specified")
    field(SELL, "3")
}
record(sseq, "SEQ:LINK") {
    field(SELM, "Specified")
    field(SELL, "SRC:SEL")
}
record(ao, "SRC:SEL") {
    field(VAL, "7")
}
record(compress, "CMP:CONST") {
    field(ALG, "Circular Buffer")
    field(NSAM, "3")
    field(INP, "5")
}
"#;

async fn build() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &PvDatabase, name: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(name, &mut visited, 0)
        .await
        .unwrap();
}

/// C `sseqRecord.c:186-191`: a constant `SELL` is loaded into `SELN` once, at
/// init, by `recGblInitConstantLink(&pR->sell, DBF_USHORT, &pR->seln)`.
#[tokio::test]
async fn constant_sell_seeds_seln_at_init() {
    let db = build().await;

    assert_eq!(
        db.get_pv("SEQ:CONST.SELN").unwrap().to_f64(),
        Some(3.0),
        "C recGblInitConstantLink(&sell, DBF_USHORT, &seln) at init_record"
    );
}

/// The boundary the executor broke: a client's put to `SELN` must SURVIVE the
/// next process. Compiled softIoc on the `seq` twin (`field(SELL,"3")`):
/// `caput SELN 5` + process leaves `SELN = 5`, because `dbGetLink` on the
/// constant writes nothing. The port re-applied the constant every cycle and
/// reset it to 3 — firing step 3 where C fires step 5.
#[tokio::test]
async fn client_put_to_seln_survives_a_constant_sell() {
    let db = build().await;

    db.put_pv("SEQ:CONST.SELN", EpicsValue::UShort(5))
        .await
        .unwrap();
    process(&db, "SEQ:CONST").await;

    assert_eq!(
        db.get_pv("SEQ:CONST.SELN").unwrap().to_f64(),
        Some(5.0),
        "a constant SELL delivers nothing at process; SELN keeps the client's put"
    );
}

/// The owner path stays intact: a REAL `SELL` link is re-read every cycle and
/// overwrites `SELN` (C `dbGetLink` on a DB link — `sseqRecord.c:314-317`).
#[tokio::test]
async fn a_real_sell_link_still_delivers_every_cycle() {
    let db = build().await;

    db.put_pv("SEQ:LINK.SELN", EpicsValue::UShort(1))
        .await
        .unwrap();
    process(&db, "SEQ:LINK").await;

    assert_eq!(
        db.get_pv("SEQ:LINK.SELN").unwrap().to_f64(),
        Some(7.0),
        "a DB SELL link overwrites SELN on every process"
    );
}

/// compress has NO init seed for `INP` (C `compressRecord.c::init_record` calls
/// no `recGblInitConstantLink`), so a constant `INP` is dead on both paths: C
/// leaves the circular buffer empty. The port's executor was pushing the
/// constant into the buffer on every cycle — `VAL = [5, 5, 5]` against C's
/// empty buffer.
#[tokio::test]
async fn a_constant_inp_never_fills_a_compress_buffer() {
    let db = build().await;

    for _ in 0..3 {
        process(&db, "CMP:CONST").await;
    }

    let val = db.get_pv("CMP:CONST").unwrap();
    let filled = match &val {
        EpicsValue::DoubleArray(v) => v.contains(&5.0),
        _ => panic!("compress VAL is a DOUBLE array, got {val:?}"),
    };
    assert!(
        !filled,
        "C dbConstGetValue writes nothing: a constant INP never reaches the \
         circular buffer, got {val:?}"
    );
    assert_eq!(
        db.get_pv("CMP:CONST.NUSE").unwrap().to_f64(),
        Some(0.0),
        "no element was ever delivered, so the buffer holds none"
    );
}
