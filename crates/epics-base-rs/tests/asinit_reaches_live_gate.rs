//! A script-driven `asInit` must reach the IOC's LIVE access-security
//! gate, not a shell-local copy.
//!
//! Upstream context: epics-base issue #667 territory (`asDbLib` global
//! state vs the running servers). The Rust port's `asInit` previously
//! stored the parsed ACF only into the `as_state()` shell global —
//! `astac`/`asdbdump` showed a loaded config while every server kept
//! gating on its own untouched `AcfCell` (= permissive). Access
//! security was silently OFF for any IOC configured from its st.cmd
//! rather than through `IocApplication::acf()`.
//!
//! The fix threads ONE `AcfCell` per IOC: created by
//! `IocApplication::run` before the startup script, administered by the
//! script/interactive shells, and handed to every protocol server via
//! `IocRunConfig.acf`. These tests pin both halves of that invariant.

use std::io::Write;

use epics_base_rs::server::access_security::{
    AccessLevel, asg_change_generation, new_acf_cell, parse_acf,
};
use epics_base_rs::server::ioc_app::IocApplication;

/// UAG `ops` may write; everyone else reads only.
const ACF: &str =
    "UAG(ops) { alice }\nASG(DEFAULT) { RULE(1, READ) RULE(1, WRITE) { UAG(ops) } }\n";

/// Owner path: st.cmd `asSetFilename` + `asInit` → the parsed policy is
/// live in `IocRunConfig.acf` — the very cell the protocol runner hands
/// its servers — by the time the runner is entered.
#[epics_macros_rs::epics_test]
async fn script_asinit_lands_in_the_run_config_cell() {
    let acf_file = tempfile::Builder::new().suffix(".acf").tempfile().unwrap();
    write!(acf_file.as_file(), "{ACF}").unwrap();

    let mut script = tempfile::Builder::new().suffix(".cmd").tempfile().unwrap();
    writeln!(script, "asSetFilename(\"{}\")", acf_file.path().display()).unwrap();
    writeln!(script, "asInit").unwrap();

    IocApplication::new()
        .startup_script(script.path().to_str().unwrap())
        .run(|config| async move {
            let policy = config.acf.load_full().expect(
                "asInit from the startup script must populate the IOC's \
                 live policy cell, not a shell-local copy",
            );
            assert!(
                policy.uag.contains_key("ops"),
                "the cell must hold the script's parsed ACF"
            );
            // The policy is enforceable exactly as the servers will
            // enforce it: bob reads, alice writes.
            assert_eq!(
                policy.check_access("DEFAULT", "somehost", "bob"),
                AccessLevel::Read
            );
            assert_eq!(
                policy.check_access("DEFAULT", "somehost", "alice"),
                AccessLevel::ReadWrite
            );
            Ok(())
        })
        .await
        .unwrap();
}

/// Change-signal half: a post-boot `AcfCell::store` (what `asInit` and
/// the `reload_acf*` paths do) must move the process ASG-change
/// generation, so the CA `reeval_access_rights` task, the
/// `AccessGate` check cache and the QSRV grant cache all observe the
/// swap instead of serving grants computed under the old policy.
#[epics_macros_rs::epics_test]
async fn acf_cell_store_moves_the_change_generation() {
    let cell = new_acf_cell(None);
    let before = asg_change_generation();
    cell.store(Some(std::sync::Arc::new(parse_acf(ACF).unwrap())));
    assert!(
        asg_change_generation() > before,
        "AcfCell::store must fire the ASG-change notification"
    );
}
