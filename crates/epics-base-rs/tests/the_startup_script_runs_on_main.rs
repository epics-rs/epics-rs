//! C `softMain` runs `iocsh(st.cmd)` on the process's first thread, which
//! `epicsThreadInit` names `_main_` (`osdThread.c:406-412`), and the
//! `afterIocRunning` queue runs there too, from `iocInit`'s
//! `initHookAfterIocRunning` (`afterIocRunning.c:40-56`). Every name query a
//! script line makes — `epicsThreadGetNameSelf`, the `std` name, the
//! thread-list row — must answer `_main_`.

use std::sync::{Arc, Mutex};

use epics_base_rs::runtime::task::{thread_name_self, thread_report};
use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::iocsh::registry::{ArgValue, CommandDef, CommandOutcome};

type Names = (String, Option<String>, Vec<String>);

fn probe(into: Arc<Mutex<Vec<Names>>>) -> CommandDef {
    CommandDef::new(
        "probeThreadName",
        vec![],
        "probeThreadName - record this line's thread names for this test",
        move |_args: &[ArgValue], _ctx: &_| {
            let std_name = std::thread::current().name().map(str::to_string);
            let rows = thread_report()
                .into_iter()
                .map(|t| t.name().to_string())
                .filter(|n| n.starts_with("iocsh") || n == "_main_")
                .collect();
            into.lock()
                .unwrap()
                .push((thread_name_self(), std_name, rows));
            Ok(CommandOutcome::Continue)
        },
    )
}

#[epics_macros_rs::epics_test]
async fn script_and_after_ioc_running_lines_run_on_main() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let script = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("main_thread.cmd");
    std::fs::write(
        &script,
        "probeThreadName\nafterIocRunning probeThreadName\niocInit\n",
    )
    .unwrap();
    IocApplication::new()
        .port(0)
        .register_startup_command(probe(seen.clone()))
        .startup_script(script.to_str().unwrap())
        .run(|_config| async move { Ok(()) })
        .await
        .unwrap();

    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.len(),
        2,
        "the script line and the queued line: {seen:?}"
    );
    for (name_self, std_name, rows) in seen.iter() {
        assert_eq!(name_self, "_main_");
        assert_eq!(std_name.as_deref(), Some("_main_"));
        assert!(
            rows.iter().all(|n| n == "_main_"),
            "no iocsh-named row is listed: {rows:?}"
        );
    }
}
