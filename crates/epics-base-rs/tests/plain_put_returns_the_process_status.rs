//! A plain put returns the status the record's `process()` returned; a
//! put-callback never does.
//!
//! C `dbPutField` returns `dbProcess`'s status (`dbAccess.c:1279`), which is
//! `process()`'s, which for a soft record is its device support's link status.
//! Every case below processes once with a dead CA link where C's code puts the
//! status; measured on softIocPVX (`caput` plain, `caput -c`, `pvxput`) unless
//! the note cites the code. A failing record fails both plain servers and
//! answers a put-callback normally.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::error::CaError;
use epics_base_rs::server::database::{LinkPutOp, LinkSet, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::{
    acalcout::AcalcoutRecord, busy::BusyRecord, scalcout::ScalcoutRecord, sseq::SseqRecord,
    transform::TransformRecord,
};
use epics_base_rs::types::EpicsValue;

/// The CA link set of an IOC whose every remote channel is down: `dbCa`'s
/// `-1` for a get or put on an unconnected link (`dbCa.c:558-560`).
struct Down;

#[async_trait::async_trait]
impl LinkSet for Down {
    fn is_connected(&self, _: &[u8]) -> bool {
        false
    }
    fn get_cached_value(&self, _: &[u8]) -> Option<EpicsValue> {
        None
    }
    async fn get_value(&self, _: &[u8]) -> Option<EpicsValue> {
        None
    }
    async fn put_value(&self, _: &[u8], _: EpicsValue, _: LinkPutOp) -> Result<(), String> {
        Err("channel disconnected".into())
    }
}

/// `process()` returns the status: the soft device's `dbGetLink`/`dbPutLink`,
/// a DB link's `processTarget`, or the record's own.
const FAILS: &str = r#"
record(ao, "F:ao") { field(OUT, "NOPV:X CA") }
record(bo, "F:bo") { field(OUT, "NOPV:X CA") }
record(stringout, "F:so") { field(OUT, "NOPV:X CA") }
record(ai, "F:ai") { field(INP, "NOPV:X CA") }
record(bi, "F:bi") { field(INP, "NOPV:X CA") }
record(longin, "F:li") { field(INP, "NOPV:X CA") }
record(mbbi, "F:mbbi") { field(INP, "NOPV:X CA") }
record(mbbiDirect, "F:mbbiD") { field(INP, "NOPV:X CA") }
record(stringin, "F:si") { field(INP, "NOPV:X CA") }
record(int64in, "F:i64in") { field(INP, "NOPV:X CA") }
record(aai, "F:aai") { field(INP, "NOPV:X CA") field(NELM, "4") field(FTVL, "DOUBLE") }
record(waveform, "F:wf") { field(INP, "NOPV:X CA") field(NELM, "4") field(FTVL, "DOUBLE") }
record(event, "F:event") { field(INP, "NOPV:X CA") }
# devBusySoft.c:64-66 returns dbPutLink's status, busyRecord.c:274 returns it.
record(busy, "F:busy") { field(OUT, "NOPV:X CA") }
# subRecord.c fetch_values returns -1 on the first failed INPx and
# process() skips do_sub and returns it.
record(sub, "F:subInp") { field(SNAM, "zero") field(INPA, "NOPV:X CA") }
# do_sub returns the subroutine's negative status.
record(sub, "F:subNeg") { field(SNAM, "minusOne") }
# dbDbPutValue -> processTarget -> dbProcess(F:ao) (dbDbLink.c:387-392).
record(ao, "F:aoPP") { field(OUT, "F:ao PP") }
# dbDbGetValue -> dbScanPassive(F:ai) (dbDbLink.c:175-178).
record(ai, "F:aiPP") { field(INP, "F:ai PP") }
# devAoSoftCallback: dbPutLinkAsync on a dead link fails before PACT.
record(ao, "F:aoAsyncDead") { field(DTYP, "Async Soft Channel") field(OUT, "NOPV:X CA") }
# dbProcess returns the SDIS read's status when the record is disabled
# (dbAccess.c:569-572, DISA 0 == DISV 0).
record(ao, "F:sdis") { field(SDIS, "NOPV:X CA") field(DISV, "0") }
# DOL read fails, IVOA "Don't drive" skips writeValue, the DOL status stays.
record(ao, "F:dolIvoa") { field(DOL, "NOPV:X CA") field(OMSL, "closed_loop") field(IVOA, "Don't drive outputs") }
# dfanoutRecord.c:119 keeps the DOL status to the end.
record(dfanout, "F:dfDol") { field(DOL, "NOPV:X CA") field(OMSL, "closed_loop") }
"#;

/// `process()` returns 0 whatever the links did.
const SUCCEEDS: &str = r#"
record(ao, "S:sink") {}
# Device support that drops dbPutLink's status (devLoSoft.c:45-46 and kin),
# or whose LS put returns 0 on an unconnected link (dbLink.c:529-532).
record(longout, "S:lo") { field(OUT, "NOPV:X CA") }
record(mbbo, "S:mbbo") { field(OUT, "NOPV:X CA") }
record(mbboDirect, "S:mbboD") { field(OUT, "NOPV:X CA") }
record(int64out, "S:i64out") { field(OUT, "NOPV:X CA") }
record(aao, "S:aao") { field(OUT, "NOPV:X CA") field(NELM, "4") field(FTVL, "DOUBLE") }
record(lso, "S:lso") { field(OUT, "NOPV:X CA") field(SIZV, "40") }
record(printf, "S:printf") { field(OUT, "NOPV:X CA") field(FMT, "x") }
record(lsi, "S:lsi") { field(INP, "NOPV:X CA") field(SIZV, "40") }
record(histogram, "S:hist") { field(SVL, "NOPV:X CA") }
# Records whose process() returns 0 on every path.
record(calc, "S:calc") { field(INPA, "NOPV:X CA") field(CALC, "A") }
record(calcout, "S:calcout") { field(INPA, "NOPV:X CA") field(OUT, "NOPV:X CA") field(CALC, "A") }
record(dfanout, "S:dfanout") { field(OUTA, "NOPV:X CA") }
record(seq, "S:seq") { field(DOL1, "NOPV:X CA") field(LNK1, "NOPV:X CA") field(SELM, "All") }
record(subArray, "S:sa") { field(INP, "NOPV:X CA") field(MALM, "4") field(NELM, "2") field(FTVL, "DOUBLE") }
record(compress, "S:comp") { field(INP, "NOPV:X CA") field(NSAM, "4") }
record(sel, "S:sel") { field(INPA, "NOPV:X CA") }
record(aSub, "S:asub") { field(SNAM, "zero") field(INPA, "NOPV:X CA") }
record(scalcout, "S:scalcout") { field(INPA, "NOPV:X CA") field(OUT, "NOPV:X CA") field(CALC, "A") }
record(acalcout, "S:acalcout") { field(INPA, "NOPV:X CA") field(OUT, "NOPV:X CA") field(CALC, "A") }
record(transform, "S:transform") { field(INPA, "NOPV:X CA") field(OUTA, "NOPV:X CA") }
record(sseq, "S:sseq") { field(DOL1, "NOPV:X CA") field(LNK1, "NOPV:X CA") field(SELM, "All") }
# recGblFwdLink discards the target's status; a fanout's LNKn is a FLNK.
record(fanout, "S:fan") { field(LNK1, "F:ao") }
record(ao, "S:aoFlnk") { field(FLNK, "F:ao") }
# A DB OUT link through device support that drops the status.
record(longout, "S:loPP") { field(OUT, "F:ao PP") }
# The same device on a DB link: no putAsync, so devAoSoftCallback.c falls back
# to dbPutLink's 0. On a live CA link it goes PACT and aoRecord.c:223 returns
# 0; that boundary is pinned on an asyn ASYN_CANBLOCK port (asyn-rs tests).
record(ao, "S:aoAsyncDb") { field(DTYP, "Async Soft Channel") field(OUT, "S:sink") }
# Not disabled (DISA 0 != DISV 1): the record processes and overwrites the
# SDIS status.
record(ao, "S:sdisLive") { field(SDIS, "NOPV:X CA") field(DISV, "1") }
# DOL fails, but IVOA "Continue normally" runs writeValue, whose 0 (empty
# OUT) replaces it.
record(ao, "S:dol") { field(DOL, "NOPV:X CA") field(OMSL, "closed_loop") }
record(bo, "S:boDol") { field(DOL, "NOPV:X CA") field(OMSL, "closed_loop") }
record(stringout, "S:soDol") { field(DOL, "NOPV:X CA") field(OMSL, "closed_loop") }
"#;

async fn ioc() -> Arc<PvDatabase> {
    let db = Arc::new(PvDatabase::new());
    db.register_link_set("ca", Arc::new(Down)).await;
    let (db, _) = IocBuilder::new()
        .with_database(db)
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .register_record_type("acalcout", || Box::new(AcalcoutRecord::default()))
        .register_record_type("transform", || Box::new(TransformRecord::default()))
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .register_subroutine("zero", |_| Ok(0))
        .register_subroutine("minusOne", |_| Ok(-1))
        .db_string(FAILS, &HashMap::new())
        .unwrap()
        .db_string(SUCCEEDS, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    db
}

fn names(db: &str) -> Vec<&str> {
    db.lines()
        .filter_map(|l| l.strip_prefix("record(")?.split('"').nth(1))
        .filter(|n| *n != "S:sink")
        .collect()
}

#[epics_macros_rs::epics_test]
async fn a_plain_put_returns_a_failing_process_status() {
    let db = ioc().await;
    let mut wrong = Vec::new();
    for name in names(FAILS) {
        let put = db
            .put_record_field_from_ca_no_notify(name, "PROC", EpicsValue::Long(1))
            .await;
        if !matches!(put, Err(CaError::ProcessStatus(s)) if s.get() == -1) {
            wrong.push(format!("{name}: {put:?}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "C dbPutField returns process()'s -1: {wrong:#?}"
    );
}

#[epics_macros_rs::epics_test]
async fn a_plain_put_succeeds_when_process_returns_zero() {
    let db = ioc().await;
    let mut wrong = Vec::new();
    for name in names(SUCCEEDS) {
        let put = db
            .put_record_field_from_ca_no_notify(name, "PROC", EpicsValue::Long(1))
            .await;
        if put.is_err() {
            wrong.push(format!("{name}: {put:?}"));
        }
    }
    assert!(wrong.is_empty(), "process() returns 0: {wrong:#?}");
}

/// `dbNotify` never reads the process status (`db_put_process`,
/// `db_access.c:1025-1044`): `caput -c` and `pvxput -r record[block=true]`
/// succeed on every record above.
#[epics_macros_rs::epics_test]
async fn a_put_callback_ignores_the_process_status() {
    let db = ioc().await;
    let mut wrong = Vec::new();
    for name in names(FAILS).into_iter().chain(names(SUCCEEDS)) {
        let put = db
            .put_record_field_from_ca(name, "PROC", EpicsValue::Long(1))
            .await;
        if let Err(e) = put {
            wrong.push(format!("{name}: {e:?}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "a put-callback answers normally: {wrong:#?}"
    );
}
