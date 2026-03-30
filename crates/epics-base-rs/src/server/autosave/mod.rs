pub mod backup;
pub mod error;
pub mod format;
pub mod iocsh;
pub mod legacy;
pub mod macros;
pub mod manager;
pub mod request;
pub mod save_file;
pub mod save_set;
pub mod startup;
pub mod verify;

pub use backup::BackupConfig;
pub use error::{AutosaveError, AutosaveResult};
pub use manager::{AutosaveBuilder, AutosaveManager};
pub use save_set::{RestoreResult, SaveSet, SaveSetConfig, SaveSetStatus, SaveStrategy, TriggerMode};
pub use startup::AutosaveStartupConfig;

pub use legacy::{AutosaveConfig, parse_request_file, run_autosave, restore_from_file};
