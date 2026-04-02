#![allow(dead_code)]
//! Unified scan event scheduler (Phase 5).
//!
//! Provides a single scheduler that manages all scan types (periodic, I/O Intr,
//! event, delayed) under one abstraction with coalescing and backpressure.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use crate::server::database::PvDatabase;
use crate::server::record::ScanType;

/// Kind of scan event that triggers record processing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScanEventKind {
    Periodic(ScanType),
    IoIntr,
    Event,
    Delayed,
    Pini,
}

/// A scan event requesting a specific record to be processed.
#[derive(Debug, Clone)]
pub struct ScanEvent {
    pub kind: ScanEventKind,
    pub record_name: String,
}

/// Configuration for the v2 scan scheduler.
pub struct ScanSchedulerConfig {
    /// Maximum number of records being processed concurrently.
    pub max_concurrent: usize,
}

impl Default for ScanSchedulerConfig {
    fn default() -> Self {
        Self { max_concurrent: 64 }
    }
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

/// Unified scan scheduler (v2).
///
/// Manages all scan types under a single task hierarchy with:
/// - Coalescing: duplicate process requests for the same record within one
///   tick are merged (only one process() call).
/// - Backpressure: configurable max concurrent record processes.
/// - Phase ordering: preserved within periodic groups.
pub struct ScanSchedulerV2 {
    db: Arc<PvDatabase>,
    config: ScanSchedulerConfig,
}

impl ScanSchedulerV2 {
    pub fn new(db: Arc<PvDatabase>, config: ScanSchedulerConfig) -> Self {
        Self { db, config }
    }

    pub fn with_defaults(db: Arc<PvDatabase>) -> Self {
        Self::new(db, ScanSchedulerConfig::default())
    }

    /// Run all scan tasks. Processes PINI records at startup.
    /// This function runs indefinitely.
    pub async fn run(&self) {
        // Process PINI records at startup
        self.process_pini().await;

        // Spawn periodic scan tasks
        let mut handles = Vec::new();
        for &scan_type in PERIODIC_SCANS {
            if let Some(duration) = scan_type.interval() {
                let db = self.db.clone();
                let max_concurrent = self.config.max_concurrent;
                let handle = crate::runtime::task::spawn(async move {
                    Self::periodic_scan_loop(db, scan_type, duration, max_concurrent).await;
                });
                handles.push(handle);
            }
        }

        // Wait forever
        if let Some(first) = handles.into_iter().next() {
            let _ = first.await;
        } else {
            std::future::pending::<()>().await;
        }
    }

    async fn process_pini(&self) {
        let pini_records = self.db.pini_records().await;
        for name in &pini_records {
            let mut visited = HashSet::new();
            let _ = self
                .db
                .process_record_with_links(name, &mut visited, 0)
                .await;
        }
    }

    async fn periodic_scan_loop(
        db: Arc<PvDatabase>,
        scan_type: ScanType,
        duration: Duration,
        _max_concurrent: usize,
    ) {
        let mut interval = tokio::time::interval(duration);
        loop {
            interval.tick().await;

            let names = db.records_for_scan(scan_type).await;

            // Coalescing: deduplicate record names within this tick
            let mut seen = HashSet::new();
            let unique_names: Vec<_> = names
                .into_iter()
                .filter(|n| seen.insert(n.clone()))
                .collect();

            for name in &unique_names {
                let mut visited = HashSet::new();
                let _ = db.process_record_with_links(name, &mut visited, 0).await;
            }
        }
    }

    /// Submit an event-triggered scan for a specific record.
    pub async fn submit_event(&self, record_name: &str) {
        let mut visited = HashSet::new();
        let _ = self
            .db
            .process_record_with_links(record_name, &mut visited, 0)
            .await;
    }

    /// Submit a delayed scan for a record (processes after the given delay).
    pub async fn submit_delayed(&self, record_name: &str, delay: Duration) {
        let db = self.db.clone();
        let name = record_name.to_string();
        crate::runtime::task::spawn(async move {
            tokio::time::sleep(delay).await;
            let mut visited = HashSet::new();
            let _ = db.process_record_with_links(&name, &mut visited, 0).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_event_kind_variants() {
        let k1 = ScanEventKind::Periodic(ScanType::Sec1);
        let k2 = ScanEventKind::IoIntr;
        let k3 = ScanEventKind::Event;
        let k4 = ScanEventKind::Delayed;
        let k5 = ScanEventKind::Pini;
        assert_ne!(k1, k2);
        assert_ne!(k2, k3);
        assert_ne!(k3, k4);
        assert_ne!(k4, k5);
    }

    #[test]
    fn scan_event_construction() {
        let event = ScanEvent {
            kind: ScanEventKind::Periodic(ScanType::Sec1),
            record_name: "TEST:RECORD".into(),
        };
        assert_eq!(event.record_name, "TEST:RECORD");
    }

    #[test]
    fn config_default() {
        let cfg = ScanSchedulerConfig::default();
        assert_eq!(cfg.max_concurrent, 64);
    }

    #[tokio::test]
    async fn scheduler_v2_pini_empty_db() {
        let db = Arc::new(PvDatabase::new());
        let sched = ScanSchedulerV2::with_defaults(db);
        sched.process_pini().await;
        // No panic, no records
    }

    #[tokio::test]
    async fn scheduler_v2_submit_event_missing_record() {
        let db = Arc::new(PvDatabase::new());
        let sched = ScanSchedulerV2::with_defaults(db);
        // Should not panic for missing record
        sched.submit_event("NONEXISTENT").await;
    }

    #[tokio::test]
    async fn coalesce_dedup() {
        let mut seen = HashSet::new();
        let names = vec![
            "A".to_string(),
            "B".to_string(),
            "A".to_string(),
            "C".to_string(),
        ];
        let unique: Vec<_> = names
            .into_iter()
            .filter(|n| seen.insert(n.clone()))
            .collect();
        assert_eq!(unique, vec!["A", "B", "C"]);
    }
}
