//! `AcfAccessControl` must enforce the record's ASG in every caller context.
//!
//! The ACF answer depends on the record's `ASG` field, which lives behind the
//! database's async locks. Reaching it from a *sync* access check needs a
//! blocking bridge, and that bridge only works on a multi-threaded runtime —
//! so on a current-thread runtime the old code quietly fell back to
//! `("DEFAULT", 0)`, i.e. it evaluated the wrong access group and answered the
//! wrong access question. Not a panic, not an error: a silently wrong
//! access-security decision, which for a write check means granting a write the
//! ACF denies.
//!
//! Access control must not be a function of which runtime flavor the server
//! happens to be built with.
//!
//! `qsrv-core` and not `qsrv`: this file reaches only `epics_bridge_rs::qsrv`,
//! which is what `qsrv-core` selects, and never the `PvaClient` that `qsrv`
//! additionally restores. Naming the wider feature would gate the file out of
//! the target's own selection for no reason.
#![cfg(feature = "qsrv-core")]

// RTEMS-EXEC-MODEL-ALLOW(2): checked - these run and pass in the feature-ON suite.

use std::sync::Arc;

use epics_base_rs::server::access_security::parse_acf;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::EpicsValue;
use epics_bridge_rs::qsrv::{AccessContext, AcfAccessControl, ClientCreds};

/// `DEFAULT` grants write to anyone; `RESTRICTED` grants read only. A record in
/// `RESTRICTED` must therefore be read-only for our test client — and the only
/// way to know it is in `RESTRICTED` is to read its `ASG` field from the
/// database.
const ACF: &str = r#"
ASG(DEFAULT) {
    RULE(1, WRITE)
}
ASG(RESTRICTED) {
    RULE(1, READ)
}
"#;

async fn restricted_context() -> (AccessContext, &'static str) {
    let db = Arc::new(PvDatabase::new());
    db.add_record("SECURE:ai", Box::new(AiRecord::new(1.0)))
        .await
        .unwrap();
    db.put_pv("SECURE:ai.ASG", EpicsValue::String("RESTRICTED".into()))
        .await
        .unwrap();

    let acf = AcfAccessControl::new(db, parse_acf(ACF).unwrap());
    let ctx = AccessContext::with_creds(
        Arc::new(acf),
        ClientCreds {
            user: "operator".into(),
            host: "workstation".into(),
            method: "ca".into(),
            authority: String::new(),
            roles: Vec::new(),
        },
    );
    (ctx, "SECURE:ai")
}

/// A multi-threaded runtime — the one context the blocking bridge supported.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restricted_asg_denies_write_on_multi_thread_runtime() {
    let (ctx, pv) = restricted_context().await;
    assert!(ctx.can_read(pv).await, "RESTRICTED grants READ");
    assert!(
        !ctx.can_write(pv).await,
        "RESTRICTED grants no WRITE — the record's ASG must decide, not DEFAULT"
    );
    assert!(!ctx.write_grant(pv).await.allowed);
}

/// The same check on a current-thread runtime must produce the same answer.
///
/// On unfixed main it does not: the blocking ASG lookup is skipped and the
/// record is evaluated against `DEFAULT`, which grants WRITE. The write check
/// returns `true` for a record the ACF puts in a read-only group.
#[tokio::test(flavor = "current_thread")]
async fn restricted_asg_denies_write_on_current_thread_runtime() {
    let (ctx, pv) = restricted_context().await;
    assert!(ctx.can_read(pv).await, "RESTRICTED grants READ");
    assert!(
        !ctx.can_write(pv).await,
        "RESTRICTED grants no WRITE — a current-thread runtime must not silently \
         fall back to the DEFAULT ASG and grant the write"
    );
    assert!(
        !ctx.write_grant(pv).await.allowed,
        "a denied write must stay denied on a current-thread runtime"
    );
}
