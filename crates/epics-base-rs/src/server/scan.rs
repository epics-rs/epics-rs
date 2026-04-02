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
        // Process PINI records at startup (with full link chain)
        let pini_records = self.db.pini_records().await;
        for name in &pini_records {
            let mut visited = HashSet::new();
            let _ = self
                .db
                .process_record_with_links(name, &mut visited, 0)
                .await;
        }

        // Spawn a task for each periodic scan rate
        let mut handles = Vec::new();
        for &scan_type in PERIODIC_SCANS {
            if let Some(duration) = scan_type.interval() {
                let db = self.db.clone();
                let handle = crate::runtime::task::spawn(async move {
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
                handles.push(handle);
            }
        }

        // Wait forever (scan tasks run indefinitely)
        if let Some(first) = handles.into_iter().next() {
            let _ = first.await;
        } else {
            // No periodic scans, just sleep forever
            std::future::pending::<()>().await;
        }
    }
}
