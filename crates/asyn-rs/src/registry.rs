//! The process-wide asyn port registry: the single claim on a port name.
//!
//! This is the list C `asynManager` keeps as `pasynBase->asynPortList`
//! (`asynManager.c:2082-2095`), and it answers the same question for the same
//! callers: `drvAsyn*PortConfigure`, areaDetector plugin ports, hand-registered
//! driver ports and every port published through a
//! [`crate::manager::PortManager`]. `asynReport` with no port argument
//! enumerates exactly this.
//!
//! # Why it lives in the core and not under `asyn_record`
//!
//! It used to be `asyn_record::registry`, which put the framework's own name
//! claim inside the module that exists only when the `epics` feature is on. The
//! consequence was not a style problem: [`crate::manager`] is core and must
//! claim a name, so it reached into a gated module, and `--no-default-features`
//! could not compile the crate at all — five `E0433`s that had been there since
//! the module was written, because nothing ever built that configuration.
//!
//! Nothing here names `epics_base_rs` or any record type; it is `HashMap`,
//! `Mutex` and this crate's own `PortHandle`. The two functions that genuinely
//! did belong to the record layer — the `asyn` record-type factory and its
//! legacy global registration — stayed behind in [`crate::asyn_record`], which
//! re-exports these names so existing `asyn_record::get_port` callers are
//! unchanged.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::{Arc, Mutex, OnceLock};

use crate::error::{AsynError, AsynResult};
use crate::port_handle::PortHandle;
use crate::trace::TraceManager;

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

    /// Publish a port. Errors with [`AsynError::PortAlreadyRegistered`]
    /// if `name` is already present — C parity: `asynManager::registerPort`
    /// refuses a duplicate name ("port %s already registered") instead of
    /// replacing the entry. A silent overwrite would orphan the prior
    /// handle: its runtime keeps running while the name resolves to the
    /// new port, silently shadowing legitimate I/O. To replace a port,
    /// [`Self::remove`] it first.
    pub fn register(
        &self,
        name: &str,
        handle: PortHandle,
        trace: Arc<TraceManager>,
    ) -> AsynResult<()> {
        let mut reg = self.inner.lock().unwrap();
        match reg.entry(name.to_string()) {
            Entry::Occupied(_) => Err(AsynError::PortAlreadyRegistered(name.to_string())),
            Entry::Vacant(slot) => {
                slot.insert(PortEntry { handle, trace });
                Ok(())
            }
        }
    }

    pub fn get(&self, name: &str) -> Option<PortEntry> {
        let reg = self.inner.lock().ok()?;
        reg.get(name).cloned()
    }

    /// Names of every published port, in arbitrary order.
    ///
    /// C parity: `asynManager::report` with no port argument walks the
    /// global port list. Since every port creator publishes here, this is
    /// that list.
    pub fn names(&self) -> Vec<String> {
        match self.inner.lock() {
            Ok(reg) => reg.keys().cloned().collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Withdraw a port. A name that was never published is a no-op.
    pub fn remove(&self, name: &str) {
        if let Ok(mut reg) = self.inner.lock() {
            reg.remove(name);
        }
    }
}

// ===== Global fallback (backward compatibility) =====

static GLOBAL_PORT_REGISTRY: OnceLock<PortRegistry> = OnceLock::new();

fn global_registry() -> &'static PortRegistry {
    GLOBAL_PORT_REGISTRY.get_or_init(PortRegistry::new)
}

/// Register a port via the global registry.
/// Prefer using a shared `PortRegistry` instance for better test isolation.
///
/// Errors with [`AsynError::PortAlreadyRegistered`] on a duplicate name —
/// see [`PortRegistry::register`].
pub fn register_port(name: &str, handle: PortHandle, trace: Arc<TraceManager>) -> AsynResult<()> {
    global_registry().register(name, handle, trace)
}

/// Look up a port via the global registry.
pub fn get_port(name: &str) -> Option<PortEntry> {
    global_registry().get(name)
}

/// Names of every port published to the global registry.
///
/// This is the process-wide port list: driver ports, ports created by the
/// `drvAsyn*PortConfigure` iocsh commands, areaDetector plugin ports, and
/// every port registered through a [`crate::manager::PortManager`]. It is
/// what `asynReport` with no port argument enumerates.
pub fn port_names() -> Vec<String> {
    global_registry().names()
}

/// Withdraw a port from the global registry.
pub fn unregister_port(name: &str) {
    global_registry().remove(name);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interrupt::InterruptManager;

    fn dummy_handle(name: &str) -> PortHandle {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        // No actor loop runs for this handle, so its `ActorId` is never
        // published on any thread — no caller can be this port's actor,
        // which is the truth these registry tests need.
        PortHandle::new(
            tx,
            name.to_string(),
            Arc::new(InterruptManager::new(4)),
            crate::port_actor::ActorId::new(),
        )
    }

    /// C parity: `asynManager::registerPort` refuses a duplicate name
    /// instead of replacing the entry — a silent overwrite would orphan
    /// the prior handle.
    #[test]
    fn register_rejects_duplicate_name() {
        let reg = PortRegistry::new();
        reg.register(
            "regdup",
            dummy_handle("regdup"),
            Arc::new(TraceManager::new()),
        )
        .unwrap();
        match reg.register(
            "regdup",
            dummy_handle("regdup"),
            Arc::new(TraceManager::new()),
        ) {
            Err(AsynError::PortAlreadyRegistered(name)) => assert_eq!(name, "regdup"),
            other => panic!("expected PortAlreadyRegistered, got {other:?}"),
        }
        // The original entry survives the rejected attempt.
        assert!(reg.get("regdup").is_some());
    }

    /// Boundary: `remove()` frees the name for re-registration.
    #[test]
    fn removed_name_can_be_reregistered() {
        let reg = PortRegistry::new();
        reg.register(
            "regrecycle",
            dummy_handle("regrecycle"),
            Arc::new(TraceManager::new()),
        )
        .unwrap();
        reg.remove("regrecycle");
        assert!(
            reg.register(
                "regrecycle",
                dummy_handle("regrecycle"),
                Arc::new(TraceManager::new())
            )
            .is_ok(),
            "re-register after remove must succeed"
        );
    }
}
