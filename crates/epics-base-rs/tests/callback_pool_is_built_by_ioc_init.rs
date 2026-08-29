//! **The process-global callback pool MUST NOT be constructed before the
//! `iocInit` line performs the build.**
//!
//! C builds it in exactly one place — `iocState = iocBuilding; taskwdInit();
//! callbackInit();` inside `iocBuild_1` (`iocInit.c:148-152`) — and that
//! position is the whole contract of `callbackSetQueueSize` and
//! `callbackParallelThreads`: the pool reads their knobs once, when it is
//! constructed, so both refuse afterwards (`callback.c:106-109`, `:162-165`)
//! and their usage strings say "Must be called before iocInit()".
//!
//! GROUND TRUTH — the built C `softIoc` (7.0.10.1-DEV) and `softioc-rs` on the
//! same `st.cmd`, `callbackSetQueueSize 5000` / `dbLoadRecords` / `iocInit` /
//! `callbackQueueShow 0` / `callbackSetQueueSize 6000`:
//!
//! ```text
//! C:    (line 1 silent)              Q SIZE 5000   then "Callback system already initialized"
//! port: "Callback system already     Q SIZE 2000   then "Callback system already initialized"
//!        initialized"
//! ```
//!
//! Under `on error break` the port's refusal on line 1 broke the boot where C
//! ran on. Two things built the pool before the script: the explicit
//! `background_init()` at the top of `IocApplication::run_to_completion`, and
//! — the reason moving that alone changed nothing — the ASG `INP*` watcher and
//! the HAG DNS refresher, both of which `spawn_background`, and both of which
//! were started with the ACF cell before the script.
//!
//! This file pins the invariant from the shell's side, which is where it is
//! observable: the state of the pool on the line before `iocInit` and on the
//! line after it.

use std::sync::{Arc, Mutex};

use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::iocsh::registry::{ArgValue, CommandDef, CommandOutcome};

/// What the pool looks like from a startup-script line: whether it exists at
/// all (the predicate `callbackSetQueueSize` refuses on, C's
/// `epicsAtomicGetIntT(&cbState) != cbInit`) and, if it does, the queue depth
/// it was constructed with.
type PoolProbe = (bool, Option<usize>);

fn probe(into: Arc<Mutex<Vec<PoolProbe>>>) -> CommandDef {
    CommandDef::new(
        "probeCallbackPool",
        vec![],
        "probeCallbackPool - record the callback pool's state for this test",
        move |_args: &[ArgValue], _ctx: &_| {
            let started = epics_base_rs::runtime::task::background_started();
            let size = epics_base_rs::runtime::task::background_callback_stats(false)
                .map(|bands| bands[0].size);
            into.lock().unwrap().push((started, size));
            Ok(CommandOutcome::Continue)
        },
    )
}

/// A real `st.cmd` on disk: `on error break` is a script directive, and a
/// refused line has to reach the shell as a script line for it to act on.
fn st_cmd(name: &str, lines: &[&str]) -> String {
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, format!("{}\n", lines.join("\n"))).expect("write st.cmd");
    path.to_string_lossy().into_owned()
}

/// Both halves of the contract in one boot, because they are one rule seen
/// from either side of the `iocInit` line.
///
/// The last two lines are the refusal's proof: under `on error break` C stops
/// the script on a `callbackSetQueueSize` after `iocInit` — measured,
/// `Callback system already initialized` / `iocsh Error: Break` / `ERROR:
/// Error in brk2.cmd`, with the following line never run — so line 6 must
/// break the script and the `probeCallbackPool` on line 7 must not record a
/// third entry.
#[epics_macros_rs::epics_test]
async fn the_script_owns_the_queue_size_until_ioc_init_builds_the_pool() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let script = st_cmd(
        "callback_pool.cmd",
        &[
            "on error break",
            "probeCallbackPool",
            "callbackSetQueueSize 5000",
            "iocInit",
            "probeCallbackPool",
            "callbackSetQueueSize 6000",
            "probeCallbackPool",
        ],
    );
    let failure = IocApplication::new()
        .port(0)
        .register_startup_command(probe(seen.clone()))
        .startup_script(&script)
        .run(|_config| async move { Ok(()) })
        .await
        .expect_err("`on error break` on a refused line ends the script");
    assert!(
        failure.to_string().ends_with(":6"),
        "line 6 is the `callbackSetQueueSize` after `iocInit`, and it is the \
         line that must break the script: {failure}"
    );

    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.len(),
        2,
        "the line-7 probe runs only if the post-`iocInit` \
         `callbackSetQueueSize` was accepted, which C refuses: {seen:?}"
    );
    assert_eq!(
        seen[0],
        (false, None),
        "the line before `iocInit` must find no pool — anything that built one \
         has already eaten this script's `callbackSetQueueSize`"
    );
    assert_eq!(
        seen[1],
        (true, Some(5000)),
        "the build must construct the pool at the size the script asked for"
    );
}
