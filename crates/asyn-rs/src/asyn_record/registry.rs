//! Port registry for asynRecord — maps port names to handles.
//!
//! Provides both a shared `PortRegistry` instance (preferred) and
//! a global static fallback for backward compatibility.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::port_handle::PortHandle;
use crate::trace::TraceManager;

// ===== Port Registry =====

/// Entry in the port registry.
#[derive(Clone)]
pub struct PortEntry {
    pub handle: PortHandle,
    pub trace: Arc<TraceManager>,
}

/// Shared port registry — can be injected into multiple IOC instances.
#[derive(Clone)]
pub struct PortRegistry {
    inner: Arc<Mutex<HashMap<String, PortEntry>>>,
}

impl Default for PortRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PortRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn register(&self, name: &str, handle: PortHandle, trace: Arc<TraceManager>) {
        let mut reg = self.inner.lock().unwrap();
        reg.insert(name.to_string(), PortEntry { handle, trace });
    }

    pub fn get(&self, name: &str) -> Option<PortEntry> {
        let reg = self.inner.lock().ok()?;
        reg.get(name).cloned()
    }
}

// ===== Global fallback (backward compatibility) =====

static GLOBAL_PORT_REGISTRY: OnceLock<PortRegistry> = OnceLock::new();

fn global_registry() -> &'static PortRegistry {
    GLOBAL_PORT_REGISTRY.get_or_init(PortRegistry::new)
}

/// Register a port via the global registry.
/// Prefer using a shared `PortRegistry` instance for better test isolation.
pub fn register_port(name: &str, handle: PortHandle, trace: Arc<TraceManager>) {
    global_registry().register(name, handle, trace);
}

/// Look up a port via the global registry.
pub fn get_port(name: &str) -> Option<PortEntry> {
    global_registry().get(name)
}

/// Return the asyn record type factory for injection into IocBuilder.
pub fn asyn_record_factory() -> (&'static str, epics_base_rs::server::RecordFactory) {
    ("asyn", Box::new(|| Box::new(super::AsynRecord::default())))
}

/// Register the "asyn" record type via the global registry (legacy).
/// Prefer `asyn_record_factory()` with `IocBuilder::register_record_type()`.
pub fn register_asyn_record_type() {
    epics_base_rs::server::db_loader::register_record_type(
        "asyn",
        Box::new(|| Box::new(super::AsynRecord::default())),
    );
}

// ===== Transfer Mode =====
