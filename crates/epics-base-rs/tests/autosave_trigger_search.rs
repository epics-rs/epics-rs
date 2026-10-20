//! C `enable_list` searches a triggered list's trigger channel and waits
//! for it in `ca_pend_io(2.0)` (save_restore.c:1440-1444), so a trigger no
//! record answers holds the task 2 s each pass. The task then waits
//! MIN_DELAY for ops from the end of the pass (:1129-1131). The C oracle
//! IOC printed its list's "1 of 1 PV's connected" at 0.545 s and "timeout
//! on search of NOPE" at 2.731 s, 5.731 s, 8.732 s and 11.732 s.

use std::collections::HashMap;
use std::io::BufRead;
use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::AutosaveBuilder;
use epics_base_rs::server::autosave::save_set::{SaveSetConfig, SaveStrategy};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;

const CHILD: &str = "AUTOSAVE_TRIGGER_SEARCH_CHILD";
const TIMEOUT: &str = "save_restore:enable_list: timeout on search of NOPE";

/// A list triggered by `NOPE`, which no record answers, in the child only.
#[epics_macros_rs::epics_test]
async fn child_task() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(PvDatabase::new());
    db.add_record("A", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let cfg = SaveSetConfig {
        name: "t".into(),
        save_path: dir.path().join("t.sav"),
        request_file: None,
        request_pvs: vec!["A".into()],
        backup: BackupConfig::default(),
        macros: HashMap::new(),
        macro_string: None,
        search_paths: Vec::new(),
    };
    let mgr = Arc::new(
        AutosaveBuilder::new()
            .add_set(
                cfg,
                SaveStrategy::Triggered {
                    trigger_pv: "NOPE".into(),
                },
            )
            .build()
            .await,
    );
    let task = mgr.clone().start(db);
    // The parent kills this child once it has read enough.
    epics_base_rs::runtime::task::sleep(Duration::from_secs(30)).await;
    mgr.shutdown();
    drop(task);
}

/// The child's connect and timeout lines, each with when the parent read
/// it, up to its `n`th timeout.
fn console_until(n: usize) -> Vec<(Duration, String)> {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_task", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let mut out = Vec::new();
    for line in std::io::BufReader::new(child.stdout.take().unwrap()).lines() {
        let line = line.unwrap();
        if line.contains("PV's connected") || line.contains(TIMEOUT) {
            out.push((start.elapsed(), line));
        }
        if out.iter().filter(|(_, l)| l.contains(TIMEOUT)).count() == n {
            break;
        }
    }
    child.kill().unwrap();
    child.wait().unwrap();
    out
}

#[test]
fn a_trigger_search_that_times_out_holds_the_pass_two_seconds() {
    let lines = console_until(1);
    let at = |text: &str| lines.iter().find(|(_, l)| l.contains(text)).unwrap().0;
    let held = at(TIMEOUT) - at("1 of 1 PV's connected");
    assert!(
        held >= Duration::from_millis(1900) && held < Duration::from_secs(3),
        "{held:?} {lines:?}"
    );
}

#[test]
fn the_next_pass_starts_a_second_after_the_held_one_ends() {
    let lines = console_until(3);
    let timeouts: Vec<Duration> = lines
        .iter()
        .filter(|(_, l)| l.contains(TIMEOUT))
        .map(|(at, _)| *at)
        .collect();
    for gap in timeouts.windows(2).map(|w| w[1] - w[0]) {
        assert!(
            gap >= Duration::from_millis(2900) && gap < Duration::from_secs(4),
            "{gap:?} {lines:?}"
        );
    }
}
