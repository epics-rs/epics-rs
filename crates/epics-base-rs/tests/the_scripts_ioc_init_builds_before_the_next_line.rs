//! The startup script's `iocInit` line performs the build, so the line after it
//! runs against a built, running IOC.
//!
//! `iocInit` used to name two different operations: the iocsh command, which
//! closed the record-load phase and nothing else, and `IocApplication`'s own
//! build, which ran after the WHOLE script. Every line a real `st.cmd` puts
//! after `iocInit` — `dbpf`, `dbl`, `dbtr`, `asSetFilename`, `seq` — therefore
//! ran against an IOC with no device support, no scan threads and no PINI, and
//! `iocRun`/`iocPause` typed in a script answered from `iocVoid` instead of
//! from the state the IOC was actually in.
//!
//! Measured before the fix, with `softioc-rs -S st.cmd` whose `st.cmd` was
//! `dbLoadRecords` + `iocInit` + `dbl`: `dbl` listed both records and
//! `caget T:AI` timed out with `'T:AI' not found` — the IOC never built, so the
//! protocol runner was never reached and nothing was served. The same script
//! against C's `softIoc` answered `T:AI 1`.
//!
//! What closes it is one owner: `IocBuild` is armed before the script and
//! consumed exactly once, by the script's `iocInit` line or by the turn after
//! the script when the script never spelled one.

use std::sync::{Arc, Mutex};

use epics_base_rs::server::ioc_app::{IocApplication, IocState, get_ioc_state};
use epics_base_rs::server::iocsh::registry::{ArgValue, CommandDef, CommandOutcome};

/// A startup command that records the IOC state at the moment it runs.
fn probe(into: Arc<Mutex<Vec<IocState>>>) -> CommandDef {
    CommandDef::new(
        "probeIocState",
        vec![],
        "probeIocState - record the lifecycle state for this test",
        move |_args: &[ArgValue], _ctx: &_| {
            into.lock().unwrap().push(get_ioc_state());
            Ok(CommandOutcome::Continue)
        },
    )
}

/// A real `st.cmd` on disk, because a refused line has to reach the shell as a
/// script line rather than as an argv line. C's `-d`/`-x` lines are
/// `errIf(...)`-guarded direct calls and a failure there throws out of `main`
/// (`softMain.cpp:108-112,198`), which [`IocApplication::startup_line`] mirrors;
/// a line inside the script only reports and the script goes on.
fn st_cmd(name: &str, lines: &[&str]) -> String {
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, format!("{}\n", lines.join("\n"))).expect("write st.cmd");
    path.to_string_lossy().into_owned()
}

#[epics_macros_rs::epics_test]
async fn the_line_after_ioc_init_sees_a_running_ioc() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    IocApplication::new()
        .port(0)
        .register_startup_command(probe(seen.clone()))
        .startup_line("probeIocState")
        .startup_line("iocInit")
        .startup_line("probeIocState")
        .run(|_config| async move { Ok(()) })
        .await
        .unwrap();

    assert_eq!(
        *seen.lock().unwrap(),
        vec![IocState::Void, IocState::Running],
        "the line before `iocInit` must see an unbuilt IOC and the line after \
         it a running one"
    );
}

/// `iocPause` and `iocRun` typed into a startup script answer from the state
/// the IOC is really in. Both returned -1 with a `WARNING` before the fix,
/// because at script time the IOC was still `iocVoid`.
#[epics_macros_rs::epics_test]
async fn ioc_pause_and_ioc_run_in_a_script_report_the_real_state() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    IocApplication::new()
        .port(0)
        .register_startup_command(probe(seen.clone()))
        .startup_line("iocInit")
        .startup_line("iocPause")
        .startup_line("probeIocState")
        .startup_line("iocRun")
        .startup_line("probeIocState")
        .run(|_config| async move { Ok(()) })
        .await
        .unwrap();

    assert_eq!(
        *seen.lock().unwrap(),
        vec![IocState::Paused, IocState::Running],
        "a script's `iocPause`/`iocRun` must move the real lifecycle"
    );
}

/// The invariant's other half: **exactly once**. A script that spells `iocInit`
/// twice — and every script does spell it once, after which the post-script
/// turn would have built again under the old order — builds one time.
#[epics_macros_rs::epics_test]
async fn the_build_runs_once_however_many_times_the_script_asks() {
    let builds = Arc::new(Mutex::new(0usize));
    let counter = builds.clone();
    IocApplication::new()
        .port(0)
        // Drained at the end of the build, so it counts builds, not lines.
        .register_after_init(move || *counter.lock().unwrap() += 1)
        .startup_script(&st_cmd("twice.cmd", &["iocInit", "iocInit"]))
        .run(|_config| async move { Ok(()) })
        .await
        .unwrap();

    assert_eq!(
        *builds.lock().unwrap(),
        1,
        "the build owner is consumed once; a second `iocInit` has no build left \
         to perform"
    );
}

/// The path the six `CaServerBuilder` binaries take, and every application that
/// never writes a script: nothing spells `iocInit`, so the turn after Phase 1
/// performs the same build. Unchanged by the owner, and the reason the owner is
/// consumed rather than guarded.
#[epics_macros_rs::epics_test]
async fn a_script_that_never_spells_ioc_init_still_builds() {
    let builds = Arc::new(Mutex::new(0usize));
    let counter = builds.clone();
    let seen = Arc::new(Mutex::new(Vec::new()));
    IocApplication::new()
        .port(0)
        .register_after_init(move || *counter.lock().unwrap() += 1)
        .register_startup_command(probe(seen.clone()))
        .startup_line("probeIocState")
        .run(|_config| async move {
            assert_eq!(get_ioc_state(), IocState::Running);
            Ok(())
        })
        .await
        .unwrap();

    assert_eq!(*builds.lock().unwrap(), 1);
    assert_eq!(*seen.lock().unwrap(), vec![IocState::Void]);
}

/// A startup command that records the lifecycle state together with how many
/// `register_after_init` hooks have fired. The pair is what distinguishes a
/// built IOC from a running one from a script line's point of view: the hooks
/// are drained at the end of the run half, so between `iocBuild` and `iocRun`
/// the state is [`IocState::Built`] and the count is still zero.
fn probe_pair(into: Arc<Mutex<Vec<(IocState, usize)>>>, hooks: Arc<Mutex<usize>>) -> CommandDef {
    CommandDef::new(
        "probeBuiltState",
        vec![],
        "probeBuiltState - record lifecycle state and after-init count",
        move |_args: &[ArgValue], _ctx: &_| {
            let count = *hooks.lock().unwrap();
            into.lock().unwrap().push((get_ioc_state(), count));
            Ok(CommandOutcome::Continue)
        },
    )
}

/// C's decomposition, spelled in a script: `iocInit() = iocBuild() || iocRun()`
/// (`iocInit.c:111-113`). The lines between the two halves see the quiescent
/// state `iocBuild_3` leaves behind (`:201-207`) — built, not running.
#[epics_macros_rs::epics_test]
async fn a_script_can_build_and_run_in_two_lines() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let hooks = Arc::new(Mutex::new(0usize));
    let counter = hooks.clone();
    IocApplication::new()
        .port(0)
        .register_after_init(move || *counter.lock().unwrap() += 1)
        .register_startup_command(probe_pair(seen.clone(), hooks.clone()))
        .startup_line("iocBuild")
        .startup_line("probeBuiltState")
        .startup_line("iocRun")
        .startup_line("probeBuiltState")
        .run(|_config| async move { Ok(()) })
        .await
        .unwrap();

    assert_eq!(
        *seen.lock().unwrap(),
        vec![(IocState::Built, 0), (IocState::Running, 1)],
        "the lines between `iocBuild` and `iocRun` must see a built, quiescent \
         IOC, and the lines after `iocRun` a running one"
    );
}

/// `iocRun` before anything is built refuses and says something true — C
/// `iocInit.c:252-256`, `iocRun: WARNING IOC not paused`, which it is not. The
/// refusal must not consume the build: the post-script turn still performs it,
/// which is why the hook fires exactly once at the end.
#[epics_macros_rs::epics_test]
async fn ioc_run_before_anything_is_built_refuses_and_keeps_the_build() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let hooks = Arc::new(Mutex::new(0usize));
    let counter = hooks.clone();
    IocApplication::new()
        .port(0)
        .register_after_init(move || *counter.lock().unwrap() += 1)
        .register_startup_command(probe_pair(seen.clone(), hooks.clone()))
        .startup_script(&st_cmd("run-first.cmd", &["iocRun", "probeBuiltState"]))
        .run(|_config| async move {
            assert_eq!(get_ioc_state(), IocState::Running);
            Ok(())
        })
        .await
        .unwrap();

    assert_eq!(
        *seen.lock().unwrap(),
        vec![(IocState::Void, 0)],
        "a refused `iocRun` leaves the IOC exactly as it found it"
    );
    assert_eq!(*hooks.lock().unwrap(), 1);
}

/// A script that builds and never runs. C's `softMain` would call `iocInit()`,
/// watch `iocBuild_1` refuse from a state that is not `iocVoid`
/// (`iocInit.c:116-121`), and leave the IOC quiescent — bound to no port and
/// serving nothing, which is the defect this owner exists to remove. The port
/// finishes the transition the script started, and announces that it did.
#[epics_macros_rs::epics_test]
async fn a_script_that_builds_without_running_is_finished_after_the_script() {
    let hooks = Arc::new(Mutex::new(0usize));
    let counter = hooks.clone();
    IocApplication::new()
        .port(0)
        .register_after_init(move || *counter.lock().unwrap() += 1)
        .startup_line("iocBuild")
        .run(|_config| async move {
            assert_eq!(
                get_ioc_state(),
                IocState::Running,
                "the protocol runner must be handed a running IOC"
            );
            Ok(())
        })
        .await
        .unwrap();

    assert_eq!(
        *hooks.lock().unwrap(),
        1,
        "the run half happens once, after the script rather than in it"
    );
}

/// The build half is exactly once too, and by ownership: a second `iocBuild`
/// has no `IocBuild` left to consume, so it refuses the way C's `iocBuild_1`
/// does rather than building a second database on top of the first.
#[epics_macros_rs::epics_test]
async fn a_second_ioc_build_has_no_build_left_to_perform() {
    let hooks = Arc::new(Mutex::new(0usize));
    let counter = hooks.clone();
    let seen = Arc::new(Mutex::new(Vec::new()));
    IocApplication::new()
        .port(0)
        .register_after_init(move || *counter.lock().unwrap() += 1)
        .register_startup_command(probe_pair(seen.clone(), hooks.clone()))
        .startup_script(&st_cmd(
            "build-twice.cmd",
            &["iocBuild", "iocBuild", "probeBuiltState", "iocRun"],
        ))
        .run(|_config| async move { Ok(()) })
        .await
        .unwrap();

    assert_eq!(*seen.lock().unwrap(), vec![(IocState::Built, 0)]);
    assert_eq!(*hooks.lock().unwrap(), 1);
}
