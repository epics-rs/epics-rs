use std::sync::atomic::{AtomicU64, Ordering};

/// Generates monotonically increasing request IDs.
pub struct RequestTracker {
    next_id: AtomicU64,
}

impl RequestTracker {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
        }
    }

    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }
}

impl Default for RequestTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_ids() {
        let tracker = RequestTracker::new();
        let a = tracker.next_id();
        let b = tracker.next_id();
        let c = tracker.next_id();
        assert_eq!(a, 1);
        assert_eq!(b, 2);
        assert_eq!(c, 3);
    }
}
