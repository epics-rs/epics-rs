pub mod backup;
pub mod error;
pub mod format;
pub mod iocsh;
pub mod macros;
pub mod manager;
pub mod request;
pub mod save_file;
pub mod save_set;
pub mod verify;

pub use backup::BackupConfig;
pub use error::{AutosaveError, AutosaveResult};
pub use manager::{AutosaveBuilder, AutosaveManager};
pub use save_set::{RestoreResult, SaveSet, SaveSetConfig, SaveSetStatus, SaveStrategy, TriggerMode};

/// Bridge: convert a legacy `AutosaveConfig` into autosave-rs configuration.
pub fn from_legacy_config(
    config: &epics_base_rs::server::autosave::AutosaveConfig,
) -> SaveSetConfig {
    SaveSetConfig {
        name: "legacy".to_string(),
        save_path: config.save_path.clone(),
        strategy: SaveStrategy::Periodic {
            interval: config.period,
        },
        request_file: None,
        request_pvs: config.request_pvs.clone(),
        backup: BackupConfig::default(),
        macros: std::collections::HashMap::new(),
    }
}
