//! What every program's test boots with: an iocsh line run as st.cmd runs
//! it, and a poll for a program that runs on its own threads.

use std::sync::Arc;
use std::time::Duration;

use asyn_rs::manager::PortManager;
use epics_base_rs::runtime::task::{self, BlockingBridge};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::iocsh::registry::CommandOutcome;

/// `line` on a shell over `db` that carries the asyn commands and every
/// registered one, run off the runtime's threads as st.cmd is.
pub async fn sh(db: &Arc<PvDatabase>, line: &str) {
    let db = db.clone();
    let bridge = BlockingBridge::capture();
    let owned = line.to_owned();
    let result = task::spawn_blocking(move || {
        let shell = IocShell::new(db, bridge);
        asyn_rs::iocsh::register_asyn_commands_on_shell(&shell, PortManager::global());
        shell.execute_line(&owned)
    })
    .await
    .unwrap();
    match result {
        Ok(CommandOutcome::Continue) => {}
        Ok(_) => panic!("{line}: failed"),
        Err(e) => panic!("{line}: {e}"),
    }
}

/// Poll `probe` for up to ten seconds.
pub async fn eventually(mut probe: impl FnMut() -> bool) -> bool {
    for _ in 0..1000 {
        if probe() {
            return true;
        }
        task::sleep(Duration::from_millis(10)).await;
    }
    false
}
