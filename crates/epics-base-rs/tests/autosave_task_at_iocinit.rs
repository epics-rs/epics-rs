//! C creates the save_restore task with the first set (save_restore.c:2335-
//! 2351), so the script's lines after `iocInit` run with the task saving,
//! whatever runner serves the IOC: this one serves and never starts the
//! task itself.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::sleep;
use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::{IocApplication, RunOutcome};

#[epics_macros_rs::epics_test]
async fn the_script_after_iocinit_runs_with_the_task_saving() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("a.db"), "record(ao, \"A\") { field(VAL, \"2\") }\n").unwrap();
    std::fs::write(d.join("s.req"), "A\n").unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_requestfile_path(\"{d}\")\n\
         set_savefile_path(\"{d}\")\n\
         create_monitor_set(\"s.req\", 1, \"\")\n\
         iocInit\n\
         dbpf A 3\n",
        d = d.display()
    );
    std::fs::write(d.join("st.cmd"), script).unwrap();
    let sav = d.join("s.sav");
    let outcome = IocApplication::new()
        .port(0)
        .autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())))
        .startup_script(d.join("st.cmd").to_str().unwrap())
        .run_outcome(move |_| async move {
            epics_base_rs::server::db_server::announce_serving("probe");
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let text = std::fs::read_to_string(&sav).unwrap_or_default();
                if text.lines().any(|l| l == "A 3") {
                    return Ok(());
                }
                assert!(Instant::now() < deadline, "{text:?}");
                sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
    assert_eq!(outcome, RunOutcome::Served);
}
