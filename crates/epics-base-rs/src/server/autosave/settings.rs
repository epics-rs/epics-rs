//! C `save_restore.c`'s globals (`:197`, `:291`, `:374-383`, `:458-459`):
//! one set per IOC, written by the `save_restoreSet_*` commands at any time
//! and read by the save task and the restores at each use.

use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::time::Duration;

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
    /// C `save_restoreIoErrors`: `write_it`'s failed opens since its last
    /// write.
    pub io_errors: AtomicI32,
    /// C `save_restoreNFSOK`, 1 at start: the file system was mounted
    /// before autosave started (osdNfs.c:15).
    pub nfs_ok: AtomicI32,
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
        }
    }
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

    // The setters (save_restore.c:453-491).

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
        println!(
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
            println!(
                "save_restoreCallbackTimeout must be either negative (forever) or >= {MIN_PERIOD} seconds"
            );
        }
    }
}
