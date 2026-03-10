use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;

/// Event flag set: logical flags + wakeup mechanism for state sets.
///
/// Separates logical state (AtomicBool per flag) from wakeup delivery
/// (Notify per state set). Setting a flag wakes all state sets.
pub struct EventFlagSet {
    flags: Vec<AtomicBool>,
    /// flag_id → list of synced channel ids.
    sync_map: Vec<Vec<usize>>,
    /// One Notify per state set for wakeup.
    ss_wakeups: Vec<Arc<Notify>>,
}

impl EventFlagSet {
    pub fn new(
        num_flags: usize,
        sync_map: Vec<Vec<usize>>,
        ss_wakeups: Vec<Arc<Notify>>,
    ) -> Self {
        let flags = (0..num_flags).map(|_| AtomicBool::new(false)).collect();
        Self {
            flags,
            sync_map,
            ss_wakeups,
        }
    }

    /// Set a flag and wake all state sets.
    pub fn set(&self, ef_id: usize) {
        if let Some(flag) = self.flags.get(ef_id) {
            flag.store(true, Ordering::Release);
            for notify in &self.ss_wakeups {
                notify.notify_one();
            }
        }
    }

    /// Test a flag (non-destructive).
    pub fn test(&self, ef_id: usize) -> bool {
        self.flags
            .get(ef_id)
            .map_or(false, |f| f.load(Ordering::Acquire))
    }

    /// Clear a flag. Returns the previous value.
    pub fn clear(&self, ef_id: usize) -> bool {
        self.flags
            .get(ef_id)
            .map_or(false, |f| f.swap(false, Ordering::AcqRel))
    }

    /// Test and clear atomically. Returns the previous value.
    pub fn test_and_clear(&self, ef_id: usize) -> bool {
        self.flags
            .get(ef_id)
            .map_or(false, |f| f.swap(false, Ordering::AcqRel))
    }

    /// Get the channel ids synced to a given event flag.
    pub fn synced_channels(&self, ef_id: usize) -> &[usize] {
        self.sync_map.get(ef_id).map_or(&[], |v| v.as_slice())
    }

    pub fn num_flags(&self) -> usize {
        self.flags.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_efs(num_flags: usize, sync_map: Vec<Vec<usize>>, num_ss: usize) -> EventFlagSet {
        let wakeups = (0..num_ss).map(|_| Arc::new(Notify::new())).collect();
        EventFlagSet::new(num_flags, sync_map, wakeups)
    }

    #[test]
    fn test_set_and_test() {
        let efs = make_efs(3, vec![vec![]; 3], 1);
        assert!(!efs.test(0));
        efs.set(0);
        assert!(efs.test(0));
        assert!(!efs.test(1));
    }

    #[test]
    fn test_clear() {
        let efs = make_efs(2, vec![vec![]; 2], 1);
        efs.set(0);
        assert!(efs.clear(0));
        assert!(!efs.test(0));
        // clear again returns false
        assert!(!efs.clear(0));
    }

    #[test]
    fn test_test_and_clear() {
        let efs = make_efs(2, vec![vec![]; 2], 1);
        efs.set(1);
        assert!(efs.test_and_clear(1));
        assert!(!efs.test(1));
        assert!(!efs.test_and_clear(1));
    }

    #[test]
    fn test_synced_channels() {
        let efs = make_efs(2, vec![vec![0, 1], vec![2]], 1);
        assert_eq!(efs.synced_channels(0), &[0, 1]);
        assert_eq!(efs.synced_channels(1), &[2]);
    }

    #[test]
    fn test_invalid_flag_id() {
        let efs = make_efs(1, vec![vec![]], 1);
        assert!(!efs.test(99));
        assert!(!efs.clear(99));
        assert!(!efs.test_and_clear(99));
        assert_eq!(efs.synced_channels(99), &[] as &[usize]);
    }
}
