pub mod backup;
pub mod error;
pub mod format;
pub mod iocsh;
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
pub use save_set::{
    RestoreResult, SaveSet, SaveSetConfig, SaveSetStatus, SaveStrategy, TriggerMode,
};
pub use startup::AutosaveStartupConfig;

use std::path::Path;

use crate::error::{CaError, CaResult};
use crate::server::database::PvDatabase;

/// Restore PV values from a save file. Returns the number of PVs restored.
///
/// This is a convenience wrapper around [`save_set::restore_from_entries`] that
/// handles missing files gracefully (returns `Ok(0)`) and maps errors to
/// [`CaError`].
pub async fn restore_from_file(db: &PvDatabase, path: &Path) -> CaResult<usize> {
    match save_set::restore_from_entries(db, path).await {
        Ok(result) => Ok(result.restored),
        Err(AutosaveError::Io(ref e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(CaError::InvalidValue(format!("autosave restore: {e}"))),
    }
}

/// Parse a .req file string (one PV name per line, `#` comments).
///
/// This is a simple helper for callers that only need PV names without
/// macro expansion or `file` include support.
pub fn parse_request_file(content: &str) -> Vec<String> {
    content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.to_string())
        .collect()
}
