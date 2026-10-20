pub mod backup;
pub mod config_menu;
pub mod error;
pub mod format;
pub mod macros;
pub mod manager;
pub mod nfs;
pub mod req_build;
pub mod request;
pub mod save_file;
pub mod save_set;
pub mod settings;
pub mod startup;
pub mod verify;

pub use backup::BackupConfig;
pub use error::{AutosaveError, AutosaveResult};
pub use manager::{AutosaveBuilder, AutosaveManager};
pub use save_set::{
    CaContext, RestoreMode, RestorePass, RestoreResult, SaveMethods, SaveSet, SaveSetConfig,
    SaveSetStatus, SaveStrategy,
};
pub use startup::{AutosaveStartupConfig, BootRestore};

use std::path::Path;

use crate::error::{CaError, CaResult};
use crate::server::database::PvDatabase;

/// Boot-restore PV values from a save file in `pass`. Returns the number of
/// PVs restored.
///
/// This is a convenience wrapper around
/// [`save_set::restore_from_entries_with_mode`] that handles missing files
/// gracefully (returns `Ok(0)`) and maps errors to [`CaError`].
pub async fn restore_from_file(db: &PvDatabase, path: &Path, pass: RestorePass) -> CaResult<usize> {
    restored(
        save_set::restore_from_entries_with_mode(
            db,
            path,
            RestoreMode::Boot(pass),
            &settings::SrSettings::default(),
        )
        .await,
    )
}

/// C `reboot_restore` (dbrestore.c:760-1092) for one boot-restore item,
/// leaving its status in the item's (:827-836, :994-1002, :1062-1073).
pub async fn reboot_restore(
    db: &PvDatabase,
    item: &startup::BootRestore,
    pass: RestorePass,
) -> CaResult<usize> {
    use save_set::{ListStatus, SrStatus};
    let set = |status: ListStatus| *item.status.lock().unwrap() = status;
    let trace = |line: std::fmt::Arguments<'_>| {
        crate::runtime::log::errlog_vprintf(format_args!("../dbrestore.c:reboot_restore {line}"))
    };
    let debug = item.settings.debug();
    if debug != 0 {
        let filename = &item.filename;
        trace(format_args!("entry for file '{filename}'\n"));
        trace(format_args!(
            "Found filename '{filename}' in restoreFileList.\n"
        ));
        // `concatenate_paths` unless the name is absolute (:819-823).
        if !Path::new(filename).is_absolute() {
            request::concatenated(&item.path.to_string_lossy(), &item.settings);
        }
        // initHookAfterInitDevSup, initHookAfterInitDatabase (initHooks.h:84-85).
        let (state, when) = match pass {
            RestorePass::Zero => (6, "before"),
            RestorePass::One => (7, "after"),
        };
        trace(format_args!(
            "*** restoring from '{}' at initHookState {state} ({when} record/device init) ***\n",
            item.path.display()
        ));
    }
    let Some((file, from_seq)) = backup::fopen_and_check(&item.path, &item.settings).await else {
        crate::runtime::log::errlog_vprintf(format_args!(
            "../dbrestore.c:reboot_restore Can't open save file.\n"
        ));
        set(ListStatus::new(SrStatus::Fail, "Can't open save file."));
        return Err(CaError::InvalidValue(
            "autosave restore: Can't open save file.".to_string(),
        ));
    };
    if from_seq {
        set(ListStatus::new(
            SrStatus::Warn,
            "Bad .sav(B) files; used seq. backup",
        ));
    }
    let result =
        save_set::restore_boot_file(db, &file, pass, &item.macrostring, &item.settings).await;
    if let Ok(r) = &result
        && r.aborted
    {
        set(ListStatus::new(SrStatus::Fail, "restore aborted"));
        return Err(CaError::InvalidValue(
            "autosave restore: restore aborted".to_string(),
        ));
    }
    if let Ok(r) = &result
        && r.not_connected_at_save > 0
    {
        set(ListStatus::new(
            SrStatus::Warn,
            ".sav file contained an error message",
        ));
    }
    if item.write_backup && result.is_ok() {
        // `fname`'s, whichever file was read (:1031-1050).
        let bu = if item.settings.dated_backup_files() {
            format!("_{}", backup::date_str())
        } else {
            ".bu".to_string()
        };
        let mut bu_name = item.path.as_os_str().to_os_string();
        bu_name.push(bu);
        let bu_name = std::path::PathBuf::from(bu_name);
        if debug >= 1 {
            trace(format_args!(
                "writing boot-backup file '{}'.\n",
                bu_name.display()
            ));
        }
        if backup::publish_copy(&item.path, &bu_name, &item.settings)
            .await
            .is_err()
        {
            crate::runtime::log::errlog_vprintf(format_args!(
                "../dbrestore.c:reboot_restore Can't write backup file. myFileCopy() failed for '{}' -> '{}'\n",
                item.path.display(),
                bu_name.display()
            ));
            set(ListStatus::new(SrStatus::Warn, "Can't write backup file"));
            return restored(result);
        }
    }
    // An earlier status stands (:1063-1065).
    let mut status = item.status.lock().unwrap();
    if status.code == SrStatus::Init {
        *status = match &result {
            Err(_) => ListStatus::new(SrStatus::Fail, "Can't open save file."),
            Ok(r) => match r.not_found.len() + r.failed_puts.len() + r.parse_failed.len() {
                0 => ListStatus::new(SrStatus::Ok, "No errors"),
                1 => ListStatus::new(SrStatus::Warn, "1 PV error"),
                n => ListStatus::new(SrStatus::Warn, &format!("{n} PV errors")),
            },
        };
    }
    drop(status);
    if debug != 0 {
        trace(format_args!("done with file '{}'\n\n", item.filename));
    }
    restored(result)
}

fn restored(result: AutosaveResult<RestoreResult>) -> CaResult<usize> {
    match result {
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
