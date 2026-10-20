//! C `save_restore.c`'s globals (`:197`, `:291`, `:374-383`, `:458-459`):
//! one set per IOC, written by the `save_restoreSet_*` commands at any time
//! and read by the save task and the restores at each use.

use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// C `MIN_PERIOD` (save_restore.c:394).
const MIN_PERIOD: i32 = 4;

#[derive(Debug)]
pub struct SrSettings {
    /// C `save_restoreDebug`.
    pub debug: AtomicI32,
    /// C `save_restoreNumSeqFiles`.
    pub num_seq_files: AtomicI32,
    /// C `save_restoreSeqPeriodInSeconds`.
    pub seq_period_in_seconds: AtomicI32,
    /// C `save_restoreIncompleteSetsOk`.
    pub incomplete_sets_ok: AtomicI32,
    /// C `save_restoreDatedBackupFiles`.
    pub dated_backup_files: AtomicI32,
    /// C `save_restorePeriodicDatedBackups`.
    pub periodic_dated_backups: AtomicI32,
    /// C `save_restoreDatedBackupPeriod`, in seconds.
    pub dated_backup_period: AtomicI32,
    /// C `file_permissions`.
    pub file_permissions: AtomicU32,
    /// C `mustSetPermissions`.
    pub must_set_permissions: AtomicI32,
    /// C `save_restoreRetrySeconds`.
    pub retry_seconds: AtomicI32,
    /// C `save_restoreUseStatusPVs`.
    pub use_status_pvs: AtomicI32,
    /// C `save_restoreCAReconnect`.
    pub ca_reconnect: AtomicI32,
    /// C `save_restoreCallbackTimeout`.
    pub callback_timeout: AtomicI32,
    /// C `save_restoreIoErrors`: failed opens since the last good write.
    /// Only [`Self::note_io_error`] and [`Self::note_write_ok`] change it.
    io_errors: AtomicI32,
    /// C `save_restoreNFSOK`, 1 at start: the file system was mounted
    /// before autosave started (osdNfs.c:15).
    nfs_ok: AtomicI32,
    /// C `save_restoreRemountThreshold` (nfs_utils.c:12).
    pub remount_threshold: AtomicI32,
    /// The SR_recentlyStr event `note_io_error` raises, stamped as
    /// [`super::SaveSet::recent`] is.
    io_recent: Mutex<Option<(Instant, String)>>,
    /// C `taskPriority`, the save_restore thread's priority.
    pub task_priority: AtomicU32,
    /// C `openReqFile`'s static `recentlyFound`/`recentlyNotFound`.
    pub(super) recent_req_files: Mutex<RecentReqFiles>,
}

/// C `NUMRECENT` and `RECENTCHARS` (save_restore.c:3478-3479).
const NUMRECENT: usize = 5;
const RECENTCHARS: usize = 100;

/// The request-file names `openReqFile` last found and failed to find,
/// oldest first, each cut to `RECENTCHARS - 1` bytes (save_restore.c:3485-3534).
/// An existence check answers from them without looking again.
#[derive(Debug, Default)]
pub(super) struct RecentReqFiles {
    found: [Vec<u8>; NUMRECENT],
    not_found: [Vec<u8>; NUMRECENT],
}

impl RecentReqFiles {
    fn key(name: &str) -> &[u8] {
        &name.as_bytes()[..name.len().min(RECENTCHARS - 1)]
    }

    /// Whether `name` was last found, if it is remembered (:3493-3503).
    pub(super) fn lookup(&self, name: &str) -> Option<bool> {
        let key = Self::key(name);
        (0..NUMRECENT).find_map(|i| {
            if !self.found[i].is_empty() && self.found[i] == key {
                Some(true)
            } else if !self.not_found[i].is_empty() && self.not_found[i] == key {
                Some(false)
            } else {
                None
            }
        })
    }

    /// A search's answer for `name` (:3525-3532). C writes a not-found
    /// name over slot 0 after shifting the list (:3530), losing the oldest
    /// remaining name; a C defect, not copied.
    pub(super) fn record(&mut self, name: &str, found: bool) {
        let list = if found {
            &mut self.found
        } else {
            &mut self.not_found
        };
        list.rotate_left(1);
        list[NUMRECENT - 1] = Self::key(name).to_vec();
    }
}

impl Default for SrSettings {
    fn default() -> Self {
        Self {
            debug: AtomicI32::new(0),
            num_seq_files: AtomicI32::new(3),
            seq_period_in_seconds: AtomicI32::new(60),
            incomplete_sets_ok: AtomicI32::new(1),
            dated_backup_files: AtomicI32::new(1),
            periodic_dated_backups: AtomicI32::new(0),
            dated_backup_period: AtomicI32::new(0),
            // S_IRUSR | S_IWUSR | S_IRGRP | S_IWGRP | S_IROTH
            file_permissions: AtomicU32::new(0o664),
            must_set_permissions: AtomicI32::new(0),
            retry_seconds: AtomicI32::new(60),
            use_status_pvs: AtomicI32::new(1),
            ca_reconnect: AtomicI32::new(0),
            callback_timeout: AtomicI32::new(600),
            io_errors: AtomicI32::new(0),
            nfs_ok: AtomicI32::new(1),
            remount_threshold: AtomicI32::new(10),
            io_recent: Mutex::new(None),
            // epicsThreadPriorityCAServerLow (save_restore.c:335).
            task_priority: AtomicU32::new(20),
            recent_req_files: Mutex::new(RecentReqFiles::default()),
        }
    }
}

/// The settings `var` reaches. C's six are process globals; here they are
/// the config's that registered the autosave commands last.
static VAR_SETTINGS: Mutex<Option<Arc<SrSettings>>> = Mutex::new(None);

/// asSupport.dbd's `variable()` globals (asSupport.dbd:19-24,
/// save_restore.c:284-388, nfs_utils.c:14), written as `var` writes an int:
/// as given, the task clamping at its next pass.
pub(super) fn register_dbd_variables(settings: &Arc<SrSettings>) {
    use crate::server::iocsh::vars::{VarAccess, VarDef, register_variable};
    *VAR_SETTINGS.lock().unwrap() = Some(settings.clone());
    fn knob(pick: fn(&SrSettings) -> &AtomicI32) -> Option<i32> {
        VAR_SETTINGS
            .lock()
            .unwrap()
            .as_deref()
            .map(|s| get(pick(s)))
    }
    fn store(pick: fn(&SrSettings) -> &AtomicI32, v: i64) {
        if let Some(s) = VAR_SETTINGS.lock().unwrap().as_deref() {
            pick(s).store(v as i32, Ordering::Relaxed);
        }
    }
    macro_rules! var {
        ($name:literal, $field:ident) => {
            register_variable(VarDef {
                name: $name,
                access: VarAccess::Int {
                    get: || knob(|s| &s.$field).unwrap_or(0).into(),
                    set: |v| store(|s| &s.$field, v),
                },
            })
        };
    }
    var!("save_restoreDebug", debug);
    var!("save_restoreNumSeqFiles", num_seq_files);
    var!("save_restoreSeqPeriodInSeconds", seq_period_in_seconds);
    var!("save_restoreIncompleteSetsOk", incomplete_sets_ok);
    var!("save_restoreDatedBackupFiles", dated_backup_files);
    var!("save_restoreRemountThreshold", remount_threshold);
}

fn get(v: &AtomicI32) -> i32 {
    v.load(Ordering::Relaxed)
}

fn secs(n: i32) -> Duration {
    Duration::from_secs(n.max(0) as u64)
}

impl SrSettings {
    /// `save_restoreNumSeqFiles` as the task holds it, `MIN(10, MAX(0, n))`
    /// (save_restore.c:906).
    pub fn seq_files(&self) -> usize {
        get(&self.num_seq_files).clamp(0, 10) as usize
    }

    /// `save_restoreSeqPeriodInSeconds` as the task holds it, `MAX(10, n)`
    /// (save_restore.c:905).
    pub fn seq_period(&self) -> Duration {
        secs(get(&self.seq_period_in_seconds).max(10))
    }

    /// What each pass writes back (save_restore.c:905-906), so
    /// `save_restoreShow` prints the values in force.
    pub fn hold_seq_settings(&self) {
        self.num_seq_files
            .store(self.seq_files() as i32, Ordering::Relaxed);
        self.seq_period_in_seconds
            .store(self.seq_period().as_secs() as i32, Ordering::Relaxed);
    }

    pub fn io_errors(&self) -> i32 {
        get(&self.io_errors)
    }

    pub fn nfs_ok(&self) -> bool {
        get(&self.nfs_ok) != 0
    }

    /// A file that would not open: `write_it`'s `open_error` and
    /// `myFileCopy`'s source (save_restore.c:1987-1990, dbrestore.c:184).
    /// Past the threshold the file system is taken as failed, which only
    /// [`Self::note_write_ok`] clears.
    pub fn note_io_error(&self, recently: bool) {
        if self.io_errors.fetch_add(1, Ordering::Relaxed) + 1 > get(&self.remount_threshold) {
            self.nfs_ok.store(0, Ordering::Relaxed);
            if recently {
                *self.io_recent.lock().unwrap() =
                    Some((Instant::now(), "Too many I/O errors".into()));
            }
        }
    }

    /// A `write_it` that passed its checks (save_restore.c:1961-1962).
    pub fn note_write_ok(&self) {
        self.nfs_ok.store(1, Ordering::Relaxed);
        self.io_errors.store(0, Ordering::Relaxed);
    }

    pub fn io_recent(&self) -> Option<(Instant, String)> {
        self.io_recent.lock().unwrap().clone()
    }

    /// C `save_restoreDebug`, read at each trace site.
    pub fn debug(&self) -> i32 {
        get(&self.debug)
    }

    pub fn incomplete_sets_ok(&self) -> bool {
        get(&self.incomplete_sets_ok) != 0
    }

    pub fn use_status_pvs(&self) -> bool {
        get(&self.use_status_pvs) != 0
    }

    pub fn ca_reconnect(&self) -> bool {
        get(&self.ca_reconnect) != 0
    }

    pub fn dated_backup_files(&self) -> bool {
        get(&self.dated_backup_files) != 0
    }

    /// The period of the dated backups, when there are any
    /// (save_restore.c:1016-1017).
    pub fn periodic_dated_backup(&self) -> Option<Duration> {
        (get(&self.periodic_dated_backups) != 0).then(|| secs(get(&self.dated_backup_period)))
    }

    pub fn retry(&self) -> Duration {
        secs(get(&self.retry_seconds))
    }

    /// The callback timeout, when one is in force: C tests it only above
    /// `MIN_PERIOD` (save_restore.c:977), so a negative one is forever.
    pub fn callback_timeout(&self) -> Option<Duration> {
        let t = get(&self.callback_timeout);
        (t > MIN_PERIOD).then(|| secs(t))
    }

    pub fn task_priority(&self) -> u32 {
        self.task_priority.load(Ordering::Relaxed)
    }

    // The setters (save_restore.c:453-491).

    /// C `set_saveTask_priority` (save_restore.c:2636-2646); a running
    /// task takes it at its next pass.
    pub fn set_task_priority(&self, priority: i32) {
        let (min, max) = (
            crate::runtime::task::PRIORITY_MIN,
            crate::runtime::task::PRIORITY_MAX,
        );
        match u32::try_from(priority) {
            Ok(p) if (min..=max).contains(&p) => self.task_priority.store(p, Ordering::Relaxed),
            _ => crate::runtime::log::errlog_vprintf(format_args!(
                "save_restore - priority must be >= {min} and <= {max}\n"
            )),
        }
    }

    pub fn set_debug(&self, level: i32) {
        self.debug.store(level, Ordering::Relaxed);
    }

    pub fn set_num_seq_files(&self, n: i32) {
        self.num_seq_files.store(n, Ordering::Relaxed);
    }

    pub fn set_seq_period_in_seconds(&self, period: i32) {
        self.seq_period_in_seconds
            .store(period.max(10), Ordering::Relaxed);
    }

    pub fn set_incomplete_sets_ok(&self, ok: i32) {
        self.incomplete_sets_ok.store(ok, Ordering::Relaxed);
    }

    pub fn set_dated_backup_files(&self, ok: i32) {
        self.dated_backup_files.store(ok, Ordering::Relaxed);
    }

    pub fn set_periodic_dated_backups(&self, period_in_minutes: i32) {
        if period_in_minutes > 0 {
            self.periodic_dated_backups.store(1, Ordering::Relaxed);
            self.dated_backup_period
                .store(period_in_minutes.wrapping_mul(60), Ordering::Relaxed);
        } else {
            self.periodic_dated_backups.store(0, Ordering::Relaxed);
        }
    }

    /// Prints as C does.
    pub fn set_file_permissions(&self, permissions: i32) {
        self.file_permissions
            .store(permissions as u32, Ordering::Relaxed);
        self.must_set_permissions.store(1, Ordering::Relaxed);
        crate::epics_println!(
            "save_restore: File permissions set to 0{:o}",
            permissions as u32
        );
    }

    pub fn set_retry_seconds(&self, seconds: i32) {
        if seconds >= 0 {
            self.retry_seconds.store(seconds, Ordering::Relaxed);
        }
    }

    pub fn set_use_status_pvs(&self, ok: i32) {
        self.use_status_pvs.store(ok, Ordering::Relaxed);
    }

    pub fn set_ca_reconnect(&self, ok: i32) {
        self.ca_reconnect.store(ok, Ordering::Relaxed);
    }

    /// Prints as C does when refused.
    pub fn set_callback_timeout(&self, t: i32) {
        if t < 0 || t >= MIN_PERIOD {
            self.callback_timeout.store(t, Ordering::Relaxed);
        } else {
            crate::epics_println!(
                "save_restoreCallbackTimeout must be either negative (forever) or >= {MIN_PERIOD} seconds"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A miss is the newest name, as a find is. C writes it over slot 0
    /// (save_restore.c:3530), where the lookup's walk from slot 0
    /// (:3493-3503) reaches it before a later find of the same name: a C
    /// defect, not copied.
    #[test]
    fn a_find_after_a_miss_answers_found() {
        let mut recent = RecentReqFiles::default();
        recent.record("x", false);
        recent.record("x", true);
        assert_eq!(recent.lookup("x"), Some(true));
    }

    #[test]
    fn each_of_the_last_five_names_is_remembered() {
        for found in [false, true] {
            let mut recent = RecentReqFiles::default();
            for name in ["a", "b", "c", "d", "e", "f"] {
                recent.record(name, found);
            }
            assert_eq!(recent.lookup("a"), None, "{found}");
            for name in ["b", "c", "d", "e", "f"] {
                assert_eq!(recent.lookup(name), Some(found), "{name} {found}");
            }
        }
    }

    /// The threshold is a count passed, not reached: the tenth failure
    /// leaves the file system OK and the eleventh fails it.
    #[test]
    fn the_failed_open_past_the_threshold_fails_the_file_system() {
        let settings = SrSettings::default();
        for _ in 0..10 {
            settings.note_io_error(true);
        }
        assert!(settings.nfs_ok());
        assert!(settings.io_recent().is_none());
        settings.note_io_error(true);
        assert!(!settings.nfs_ok());
        assert_eq!(settings.io_recent().unwrap().1, "Too many I/O errors");
        settings.note_write_ok();
        assert!(settings.nfs_ok());
        assert_eq!(settings.io_errors(), 0);
    }

    /// `myFileCopy`'s count fails the file system but says nothing in
    /// SR_recentlyStr (dbrestore.c:184).
    #[test]
    fn a_copy_failure_past_the_threshold_leaves_no_recent_line() {
        let settings = SrSettings::default();
        settings.remount_threshold.store(0, Ordering::Relaxed);
        settings.note_io_error(false);
        assert!(!settings.nfs_ok());
        assert!(settings.io_recent().is_none());
    }
}
