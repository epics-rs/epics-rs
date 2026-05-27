use std::collections::HashSet;
use std::sync::Arc;

use crate::server::database::PvDatabase;
use crate::server::record::ScanType;

/// Scan scheduler that processes records at their configured scan rates.
pub struct ScanScheduler {
    db: Arc<PvDatabase>,
}

/// All periodic scan types and their corresponding ScanType.
const PERIODIC_SCANS: &[ScanType] = &[
    ScanType::Sec01,
    ScanType::Sec02,
    ScanType::Sec05,
    ScanType::Sec1,
    ScanType::Sec2,
    ScanType::Sec5,
    ScanType::Sec10,
];

impl ScanScheduler {
    pub fn new(db: Arc<PvDatabase>) -> Self {
        Self { db }
    }

    /// Run all scan tasks. Also processes PINI records at startup.
    /// This function runs indefinitely.
    pub async fn run(&self) {
        self.run_with_hooks(Vec::new()).await;
    }

    /// Run all scan tasks with post-PINI hooks.
    ///
    /// After PINI records are processed, the hooks are invoked before
    /// periodic scan tasks begin. This ensures pollers start only after
    /// the initial record processing burst is complete.
    ///
    /// If another `ScanScheduler` has already started for the same DB (e.g.
    /// CA server already running when PVA server starts in a QSRV setup),
    /// this call still runs the provided hooks but does NOT spawn duplicate
    /// scan tasks. It then awaits forever so the caller's `tokio::select!`
    /// behaves as expected.
    pub async fn run_with_hooks(&self, hooks: Vec<Box<dyn FnOnce() + Send>>) {
        let is_first = self.db.try_claim_scan_start();

        if is_first {
            // Process PINI records at startup (with full link chain)
            let pini_records = self.db.pini_records().await;
            for name in &pini_records {
                let mut visited = HashSet::new();
                let _ = self
                    .db
                    .process_record_with_links(name, &mut visited, 0)
                    .await;
            }
            // Release non-owner schedulers so they can run their hooks now.
            self.db.mark_pini_done();
        } else {
            // Non-owner: wait for the owner to finish PINI before running hooks.
            // This preserves the "PINI before after-init hooks" contract.
            self.db.wait_for_pini().await;
        }

        // Run the caller's after-init hooks (protocol-specific, e.g. registering
        // PVA PVs after the DB is loaded). Always AFTER PINI is done.
        for hook in hooks {
            hook();
        }

        if !is_first {
            // Another ScanScheduler already owns the periodic tasks for this DB.
            // Avoid spawning duplicates; just park this future.
            std::future::pending::<()>().await;
            return;
        }

        // Spawn a task per periodic scan rate into a `JoinSet`.
        // The pre-fix code kept `handles.into_iter().next()`
        // and dropped the remaining JoinHandles — Tokio's
        // `JoinHandle::drop` does NOT abort, it detaches. So 6 of
        // the 7 periodic scan tasks orphaned on every scheduler
        // shutdown. `JoinSet::drop` aborts every still-running
        // member, so cancelling `run_with_hooks` (via tokio::select!
        // or runtime teardown) now tears down every periodic scan
        // cleanly.
        let mut join_set: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        for &scan_type in PERIODIC_SCANS {
            if let Some(duration) = scan_type.interval() {
                let db = self.db.clone();
                join_set.spawn(async move {
                    let mut interval = tokio::time::interval(duration);
                    loop {
                        interval.tick().await;
                        let names = db.records_for_scan(scan_type).await;
                        for name in &names {
                            let mut visited = HashSet::new();
                            let _ = db.process_record_with_links(name, &mut visited, 0).await;
                        }
                    }
                });
            }
        }

        if join_set.is_empty() {
            std::future::pending::<()>().await;
        } else {
            // Drain — periodic scans never return naturally, so this
            // suspends forever. On cancellation the JoinSet drops
            // and aborts every member.
            while join_set.join_next().await.is_some() {}
        }
    }
}
