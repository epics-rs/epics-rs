use epics_base_rs::types::DbFieldType;
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};

/// CA-FR-2: allocate the next non-zero id from `counter`, skipping any
/// value that `is_live` reports as still in use. C libca assigns IO
/// identifiers through owned tables (`ioTable.idAssignAdd` in
/// `libca/cac.cpp::writeNotifyRequest()` / `readNotifyRequest()`) and
/// channels through the CA context channel table; this is the Rust
/// equivalent — the owning registry supplies `is_live` so a counter
/// that wraps through 2^32 (≈11.9 h at 100k ops/s) cannot reissue an
/// id whose operation is still pending and wake the wrong waiter. `0`
/// is always skipped because several CA paths treat it as the "no IO"
/// sentinel.
pub(crate) fn alloc_nonzero_probe(
    counter: &AtomicU32,
    mut is_live: impl FnMut(u32) -> bool,
) -> u32 {
    loop {
        let v = counter.fetch_add(1, Ordering::Relaxed);
        if v == 0 {
            tracing::warn!("CA ID allocator wrapped through 2^32; skipping 0");
            continue;
        }
        if is_live(v) {
            tracing::warn!(
                id = v,
                "CA ID allocator wrapped onto a live id; skipping to avoid collision"
            );
            continue;
        }
        return v;
    }
}

/// Access rights for a channel
#[derive(Debug, Clone, Copy)]
pub struct AccessRights {
    pub read: bool,
    pub write: bool,
}

impl AccessRights {
    pub fn from_u32(v: u32) -> Self {
        Self {
            read: v & 1 != 0,
            write: v & 2 != 0,
        }
    }
}

impl fmt::Display for AccessRights {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.read, self.write) {
            (true, true) => write!(f, "read/write"),
            (true, false) => write!(f, "read-only"),
            (false, true) => write!(f, "write-only"),
            (false, false) => write!(f, "no access"),
        }
    }
}

/// Channel metadata returned by cainfo
#[derive(Debug)]
pub struct ChannelInfo {
    pub pv_name: String,
    pub server_addr: SocketAddr,
    pub native_type: DbFieldType,
    pub element_count: u32,
    pub access_rights: AccessRights,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn alloc_nonzero_probe_skips_zero_on_wrap() {
        // Counter sitting on the wrap boundary returns 0 first; the
        // allocator must skip it (CA "no IO" sentinel) and return 1.
        let counter = AtomicU32::new(0);
        assert_eq!(alloc_nonzero_probe(&counter, |_| false), 1);
    }

    #[test]
    fn alloc_nonzero_probe_skips_live_ids() {
        // ids 5 and 6 are live; the next free id is 7.
        let counter = AtomicU32::new(5);
        let live: HashSet<u32> = [5, 6].into_iter().collect();
        assert_eq!(alloc_nonzero_probe(&counter, |v| live.contains(&v)), 7);
    }

    #[test]
    fn alloc_nonzero_probe_returns_free_id_directly() {
        let counter = AtomicU32::new(42);
        assert_eq!(alloc_nonzero_probe(&counter, |_| false), 42);
        // counter advanced
        assert_eq!(alloc_nonzero_probe(&counter, |_| false), 43);
    }
}
