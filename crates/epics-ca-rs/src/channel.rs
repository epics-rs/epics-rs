use epics_base_rs::types::DbFieldType;
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT_CID: AtomicU32 = AtomicU32::new(1);
static NEXT_IOID: AtomicU32 = AtomicU32::new(1);
static NEXT_SUBID: AtomicU32 = AtomicU32::new(1);

/// R2-21 partial: skip 0 on wrap. C libca assigns IO identifiers
/// through owned tables (`ioTable.idAssignAdd` in
/// `libca/cac.cpp::writeNotifyRequest()` and `readNotifyRequest()`)
/// and channels through the CA context channel table rather than a
/// raw global counter. A full free-list / live-table collision
/// check is deferred (substantial refactor across coordinator IO
/// tables); skipping zero at least avoids returning a sentinel
/// value that several CA paths treat as "no ID", and the wrap
/// tracing::warn surfaces the rare-but-real wraparound condition
/// for high-rate long-running clients. ~11.9 h at 100k ops/s.
fn alloc_nonzero(counter: &AtomicU32) -> u32 {
    loop {
        let v = counter.fetch_add(1, Ordering::Relaxed);
        if v != 0 {
            return v;
        }
        // Wrapped through zero (2^32 allocations). Skip zero to
        // avoid collisions with the "no IO" sentinel that several
        // CA paths use. Note: this still does not detect collisions
        // with live IDs — see R2-21 for the deferred full fix.
        tracing::warn!(
            "CA ID allocator wrapped through 2^32; skipping 0 but live-table collision check is not enforced"
        );
    }
}

pub fn alloc_cid() -> u32 {
    alloc_nonzero(&NEXT_CID)
}

pub fn alloc_ioid() -> u32 {
    alloc_nonzero(&NEXT_IOID)
}

pub fn alloc_subid() -> u32 {
    alloc_nonzero(&NEXT_SUBID)
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
