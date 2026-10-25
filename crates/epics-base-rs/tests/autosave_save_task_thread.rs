//! C runs save_restore on its own epicsThread, created at `taskPriority`
//! (save_restore.c:335, :2345), and `set_saveTask_priority` moves a running
//! one (:2644); here the move lands at the task's next pass.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::{sleep, thread_by_name};
use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::{IocApplication, RunOutcome};

/// The save_restore thread's priority once it is `want`, or the last one
/// seen after 30 s.
async fn priority_reaching(want: u32) -> Option<u32> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let seen = thread_by_name("save_restore").map(|t| t.epics_priority());
        if seen == Some(want) || Instant::now() > deadline {
            return seen;
        }
        sleep(Duration::from_millis(50)).await;
    }
}

async fn run_ioc(after_init: &str, want: [u32; 2]) {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("a.db"), "record(ao, \"A\") {}\n").unwrap();
    std::fs::write(d.join("s.req"), "A\n").unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_requestfile_path(\"{d}\")\n\
         set_savefile_path(\"{d}\")\n\
         create_monitor_set(\"s.req\", 5, \"\")\n\
         iocInit\n\
         {after_init}\n",
        d = d.display()
    );
    std::fs::write(d.join("st.cmd"), script).unwrap();
    let outcome = IocApplication::new()
        .port(0)
        .autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())))
        .startup_script(d.join("st.cmd").to_str().unwrap())
        .run_outcome(move |_| async move {
            assert_eq!(priority_reaching(want[0]).await, Some(want[0]));
            epics_base_rs::server::db_server::announce_serving("probe");
            assert_eq!(priority_reaching(want[1]).await, Some(want[1]));
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(outcome, RunOutcome::Served);
}

#[epics_macros_rs::epics_test]
async fn the_task_is_its_own_thread_at_cs_default_priority() {
    run_ioc("", [20, 20]).await;
}

#[epics_macros_rs::epics_test]
async fn a_priority_set_after_start_moves_the_thread() {
    run_ioc("set_saveTask_priority(30)", [20, 30]).await;
}
