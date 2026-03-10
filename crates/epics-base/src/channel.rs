use crate::types::DbFieldType;
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT_CID: AtomicU32 = AtomicU32::new(1);
static NEXT_IOID: AtomicU32 = AtomicU32::new(1);
static NEXT_SUBID: AtomicU32 = AtomicU32::new(1);

pub fn alloc_cid() -> u32 {
    NEXT_CID.fetch_add(1, Ordering::Relaxed)
}

pub fn alloc_ioid() -> u32 {
    NEXT_IOID.fetch_add(1, Ordering::Relaxed)
}

pub fn alloc_subid() -> u32 {
    NEXT_SUBID.fetch_add(1, Ordering::Relaxed)
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
