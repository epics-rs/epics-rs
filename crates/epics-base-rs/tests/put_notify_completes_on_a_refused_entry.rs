//! A chain entry the port refuses must still leave the put-notify wait-set.
//!
//! `join_put_notify` (C `dbNotifyAdd`) runs in the link dispatcher on the
//! will-process branch, *before* the recursion decides whether the entry may
//! run:
//!
//! ```text
//! links.rs   let pact = tg.is_processing();
//! links.rs   if !pact { tg.common.putf = src_putf;
//! links.rs              join_put_notify(&mut tg, src_notify); }   // ws.enter()
//! links.rs   self.process_record_with_links_recursive(target, visited, depth + 1)
//! ```
//!
//! so a target that is then refused by `MAX_LINK_DEPTH`, or
//! stopped by the `visited` cycle guard, was counted into the wait-set and
//! never counted out. The set could not drain, the completion oneshot never
//! fired, and a CA `WRITE_NOTIFY` driving that chain got no reply at all.
//!
//! C decides this with one flag and one finalizer. `callNotifyCompletion`
//! starts FALSE (`dbAccess.c:495`) and is raised on the exits where the record
//! will not run its cycle — disabled (`:577`) and no RSET (`:599`) — while the
//! already-active branch (`:552-556`) deliberately leaves it FALSE, because a
//! record whose own cycle is running owns its completion:
//!
//! ```c
//! all_done:
//!     if (set_trace)
//!         *ptrace = 0;
//!     if (callNotifyCompletion && precord->ppn)
//!         dbNotifyCompletion(precord);
//! ```
//!
//! The port's bounds are the "will not run its cycle" shape, so they belong on
//! C's `callNotifyCompletion = TRUE` side.
//!
//! Measured on x86_64-wrs-vxworks before the fix (`RTEMS:E8:H` is an `ao` at
//! the head of an 18-record FLNK chain, `MAX_LINK_DEPTH` = 16):
//!
//! ```text
//! [    0.1s] discrim SOLO   RTEMS:AO      =601.0 budget= 45s elapsed=  0.012s OK    (ao, no FLNK, VAL preset)
//! [    0.1s] discrim SOLO   RTEMS:E8:L15  =602.0 budget= 45s elapsed=  0.004s OK    (calc, FLNK->L16, entry depth 0 so no bail)
//! [   45.2s] discrim SOLO   RTEMS:E8:H    =603.0 budget= 45s elapsed= 45.016s LOST  (ao + 16-deep chain that BAILS)
//! [   45.2s] discrim SOLO   RTEMS:E8:H    =604.0 budget= 20s elapsed=  0.025s OK    (same record, new connection)
//! ```
//!
//! The fourth line is the tell: the leak is one-shot per record, because
//! `join_put_notify` skips a target whose `notify` is already `Some` — so the
//! record that stranded the first wait-set silently declines to join every
//! later one.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::ProcessCompletion;
use epics_base_rs::types::EpicsValue;

/// Well past `MAX_LINK_DEPTH` (16), so the bound is crossed wherever it sits
/// below 32 — the same reason `link_chain_bound_is_audible` uses 24.
const CHAIN: usize = 24;

/// `ao` head + `calc` FLNK chain — the shape of `RTEMS:E8:H` -> `RTEMS:E8:L*`
/// on the VxWorks rig. A put to an `ao`'s VAL drives processing, which is what
/// makes this reachable from `CA_PROTO_WRITE_NOTIFY`.
async fn chain_db(len: usize) -> Arc<PvDatabase> {
    let mut db_text = String::from("record(ao, \"H\") { field(FLNK,\"L0\") }\n");
    for i in 0..len {
        let flnk = if i + 1 < len {
            format!("field(FLNK,\"L{}\")", i + 1)
        } else {
            String::new()
        };
        let inp = if i == 0 {
            "H".to_string()
        } else {
            format!("L{}", i - 1)
        };
        db_text.push_str(&format!(
            "record(calc, \"L{i}\") {{ field(CALC,\"A+1\") field(INPA,\"{inp}\") {flnk} }}\n"
        ));
    }
    IocBuilder::new()
        .db_string(&db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

/// The boundary this closes: a chain LONGER than `MAX_LINK_DEPTH`. Every
/// record in it is synchronous, so the whole cascade settles inside the call
/// and the put-notify must report `Sync`. Before the fix this returned
/// `Async(rx)` on a wait-set that no longer had anyone left to drain it.
#[epics_macros_rs::epics_test]
async fn a_put_notify_into_an_over_long_chain_completes() {
    let db = chain_db(CHAIN).await;
    let completion = db
        .put_record_field_from_ca("H", "VAL", EpicsValue::Double(1.0))
        .await
        .expect("the put itself succeeds — the bound refuses a link target, not the head");
    assert!(
        matches!(completion, ProcessCompletion::Sync),
        "every record in this chain is synchronous, so the refused entry at the \
         depth bound is the only thing that can hold the wait-set open"
    );
}

/// The other side of the same boundary: a chain that FITS. Without it the test
/// above would still pass if the wait-set were never armed at all.
#[epics_macros_rs::epics_test]
async fn a_put_notify_into_a_chain_inside_the_bound_completes() {
    let db = chain_db(4).await;
    let completion = db
        .put_record_field_from_ca("H", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    assert!(matches!(completion, ProcessCompletion::Sync));
}

/// The second port-only non-run: the `visited` cycle guard. Unlike the two
/// above this one passes with or without the release, because a record
/// re-reached inside one depth-first cascade is still `pact` — so the link
/// dispatcher takes the RPRO branch and never joins it in the first place. It
/// is here as the boundary that must NOT change: routing the silent guard
/// through the same exit as the loud bounds must not start completing a
/// put-notify that the record's own live cycle still owns.
#[epics_macros_rs::epics_test]
async fn a_put_notify_into_a_cycle_completes() {
    let db_text = "record(ao, \"H\") { field(FLNK,\"C0\") }\n\
                   record(calc, \"C0\") { field(CALC,\"A+1\") field(FLNK,\"C1\") }\n\
                   record(calc, \"C1\") { field(CALC,\"A+1\") field(FLNK,\"C0\") }\n";
    let db = IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let completion = db
        .put_record_field_from_ca("H", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    assert!(matches!(completion, ProcessCompletion::Sync));
}

/// The one-shot half of the leak, which is why the rig saw exactly ONE stall
/// per run however many puts it made: a record left holding a stranded
/// wait-set never joins a later one, so put #2 onward completed while put #1
/// hung forever. Both puts must now report `Sync`.
#[epics_macros_rs::epics_test]
async fn a_second_put_notify_into_the_same_over_long_chain_also_completes() {
    let db = chain_db(CHAIN).await;
    for round in 1..=2 {
        let completion = db
            .put_record_field_from_ca("H", "VAL", EpicsValue::Double(round as f64))
            .await
            .unwrap();
        assert!(
            matches!(completion, ProcessCompletion::Sync),
            "put #{round} must complete; before the fix only #1 hung and the rest \
             passed because the refused record no longer joined"
        );
    }
}
