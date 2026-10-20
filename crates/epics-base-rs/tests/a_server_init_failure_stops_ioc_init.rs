//! C `iocBuild` binds every server in `dbInitServers()` (`iocInit.c:222`), so
//! RSRV's `cantProceed` on a held port stops the thread running `iocInit` and
//! no later `st.cmd` line runs, at any script depth. The port bound in the
//! protocol runner after `iocRun` instead, so the script ran on past
//! `iocRun: All initialization complete` before the IOC failed.

use std::sync::{Arc, Mutex};

use epics_base_rs::error::CaError;
use epics_base_rs::server::ioc_app::{IocApplication, ServerInitArgs};
use epics_base_rs::server::iocsh::registry::{ArgValue, CommandDef, CommandOutcome};

fn mark(into: Arc<Mutex<Vec<String>>>) -> CommandDef {
    CommandDef::new(
        "markLine",
        vec![epics_base_rs::server::iocsh::registry::ArgDesc {
            name: "what",
            arg_type: epics_base_rs::server::iocsh::registry::ArgType::String,
        }],
        "markLine what - record that this line ran",
        move |args: &[ArgValue], _ctx: &_| {
            if let Some(ArgValue::String(what)) = args.first() {
                into.lock().unwrap().push(what.to_str_lossy().into_owned());
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

fn script(name: &str, lines: &[&str]) -> String {
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, format!("{}\n", lines.join("\n"))).expect("write the script");
    path.to_string_lossy().into_owned()
}

#[epics_macros_rs::epics_test]
async fn a_failed_server_init_stops_every_script_level() {
    let ran = Arc::new(Mutex::new(Vec::new()));
    let inner = script(
        "server_init_failure_inner.cmd",
        &["iocInit", "markLine inner-after"],
    );
    let outer = script(
        "server_init_failure_outer.cmd",
        &[&format!("iocshLoad {inner}"), "markLine outer-after"],
    );
    let err = IocApplication::new()
        .port(0)
        .register_startup_command(mark(ran.clone()))
        .register_server_init("probe", |_args: ServerInitArgs| async {
            Err::<(), _>(CaError::InvalidValue("probe: port held".into()))
        })
        .register_after_init(|| panic!("iocRun must not be reached"))
        .startup_script(&outer)
        .run(|_config| async { panic!("the runner must not be started") })
        .await
        .expect_err("the init's error is the IOC's");

    assert!(err.to_string().contains("probe: port held"), "got: {err}");
    assert!(
        ran.lock().unwrap().is_empty(),
        "no line after the failed `iocInit` may run, nested or not: {:?}",
        ran.lock().unwrap()
    );
}

#[epics_macros_rs::epics_test]
async fn a_server_init_hands_its_state_to_the_runner() {
    let got = Arc::new(Mutex::new(None));
    let seen = got.clone();
    IocApplication::new()
        .port(0)
        .register_server_init("probe", |args: ServerInitArgs| async move {
            Ok::<_, CaError>(args.port)
        })
        .startup_script(&script("server_init_state.cmd", &["iocInit"]))
        .run(move |mut config| async move {
            *seen.lock().unwrap() = Some((
                config.servers.take::<u16>("probe"),
                config.servers.take::<u16>("probe"),
            ));
            epics_base_rs::server::db_server::announce_serving("probe");
            Ok(())
        })
        .await
        .expect("the IOC runs");

    assert_eq!(*got.lock().unwrap(), Some((Some(0), None)));
}

/// The failed `iocInit` leaves its `_main_` parked for good, as C's
/// `cantProceed` leaves the thread. A job queued after that is new work, the
/// next IOC's softMain or a `main`'s own `iocsh(NULL)`, so it runs on a
/// `_main_` of its own instead of waiting behind the parked one forever.
#[epics_macros_rs::epics_test]
async fn a_job_after_the_failed_ioc_init_runs_on_a_main_of_its_own() {
    let err = IocApplication::new()
        .port(0)
        .register_server_init("probe", |_args: ServerInitArgs| async {
            Err::<(), _>(CaError::InvalidValue("probe: port held".into()))
        })
        .startup_script(&script("server_init_failure_then_job.cmd", &["iocInit"]))
        .run(|_config| async { panic!("the runner must not be started") })
        .await
        .expect_err("the init's error is the IOC's");
    assert!(err.to_string().contains("probe: port held"), "got: {err}");

    let later = epics_base_rs::runtime::task::run_on_main_thread(|| {
        std::thread::current().name().map(str::to_owned)
    })
    .unwrap();
    let name = epics_base_rs::runtime::task::timeout(std::time::Duration::from_secs(10), later)
        .await
        .expect("the job waited behind the parked _main_")
        .unwrap();
    assert_eq!(name.as_deref(), Some("_main_"));
}
