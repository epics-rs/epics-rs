//! The process-wide asyn port registry: the single claim on a port name.
//!
//! This is the list C `asynManager` keeps as `pasynBase->asynPortList`
//! (`asynManager.c:2084-2097`), and it answers the same question for the same
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
//! legacy global registration — stayed behind in `asyn_record`, which
//! re-exports these names so existing `asyn_record::get_port` callers are
//! unchanged.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::{Arc, Mutex, OnceLock};

use epics_libcom_rs::runtime::interrupt_accept::InterruptGate;

use crate::error::{AsynError, AsynResult};
use crate::port_handle::PortHandle;
use crate::trace::TraceManager;

/// Entry in the port registry. The port's trace manager is
/// [`PortHandle::trace`]: one owner, the handle the driver was bound with.
#[derive(Clone)]
pub struct PortEntry {
    pub handle: PortHandle,
}

/// A name in the registry: claimed while its port is being built, then
/// published.
enum Slot {
    Claimed,
    Published(Box<PortEntry>),
}

/// Shared port registry — can be injected into multiple IOC instances.
#[derive(Clone)]
pub struct PortRegistry {
    inner: Arc<Mutex<Ports>>,
}

/// The slots by name, each with the claim's sequence number: C's
/// `asynPortList` is in `registerPort` order (asynManager.c:2097), and the
/// claim is that call.
#[derive(Default)]
struct Ports {
    next: u64,
    slots: HashMap<String, (u64, Slot)>,
}

/// A port name held for a port still being built. C's `asynPortDriver`
/// constructor registers the port before it builds anything else
/// (asynPortDriver.cpp:4036-4040), so a refused name leaves nothing behind;
/// a claim taken first gives the same order. Dropped unpublished, it frees
/// the name; a constructor that fails after C would have registered the port
/// publishes what it built through [`PortClaim::publish_failed`] instead.
pub struct PortClaim {
    registry: PortRegistry,
    name: String,
    published: bool,
}

impl PortClaim {
    /// The claimed name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Publish `handle` under the claimed name.
    ///
    /// `owner` is the gate of the database this port serves
    /// (`ctx.db().scan_control().interrupt_gate()`): the port accepts
    /// callbacks while it is open, and no other gate is read.
    /// A port is ownerless only between construction and this call, and
    /// reads closed then, so a seed written in the constructor is kept; the
    /// owner's `false → true` edge flushes it once (C `callbackThread`,
    /// `asynPortDriver.cpp:923-937`), immediately if the owner is already
    /// open here.
    ///
    /// The entry's trace manager is the handle's own — the one the driver was
    /// bound to at `create_port_runtime` — so a mask set through the registry
    /// (`asynSetTraceMask`, asynRecord `TMSK`) is the mask the driver's
    /// `trace_print` reads. C has one `tracePvt` per port, reached through
    /// the port (asynManager.c:451-461); a registration that named a second
    /// manager published masks nothing printed against.
    pub fn publish(mut self, handle: PortHandle, owner: &Arc<InterruptGate>) -> AsynResult<()> {
        // C `registerPort` builds the port's `dpCommon` and records its
        // attributes in the same call that adds it to the port list
        // (asynManager.c:2047-2097), so the trace facility can never be asked
        // about a port whose `ASYN_MULTIDEVICE` bit it does not know.
        if !handle.interrupts().set_owner(owner.clone()) {
            return Err(AsynError::PortAlreadyOwned(self.name.clone()));
        }
        let trace = handle.trace();
        trace.register_port(&self.name, handle.is_multi_device());
        // The asynPortDriver ctor's connectDevice(pasynUserSelf, portName, 0)
        // (asynPortDriver.cpp:4099): the port's own lines print on device 0.
        trace.connect_device(&self.name, 0);
        let registry = self.registry.clone();
        let mut reg = registry.inner.lock().unwrap();
        // Only this claim's own `Drop` frees a claimed slot, so it is still ours.
        match reg.slots.get_mut(&self.name) {
            Some((_, slot @ Slot::Claimed)) => {
                *slot = Slot::Published(Box::new(PortEntry { handle }))
            }
            _ => unreachable!("claimed slot {} left its claim", self.name),
        }
        self.published = true;
        // By name, so the gate does not keep a withdrawn port's actor alive.
        let (flush, port) = (registry.clone(), self.name.clone());
        owner.on_open(move || flush.flush_in_background(&port));
        if owner.is_open() {
            // Behind the lock held here: the lookup sees the entry.
            registry.flush_in_background(&self.name);
        }
        Ok(())
    }

    /// Publish the port a constructor built before it failed, and hand back
    /// the constructor's `error`.
    ///
    /// C has no unregister: once `registerPort` has run, a driver whose
    /// constructor gives up keeps the port, answering as the half-built
    /// driver it is (`drvAsynDG645` returns before setting `init`, every
    /// transfer then refuses; `drvT4UDirect_EM` returns with its parameters
    /// created and no IO). A second Configure under the name fails and
    /// `asynReport` lists it. `handle` is that driver's runtime, in the state
    /// C's constructor left it.
    pub fn publish_failed<E: From<AsynError>>(
        self,
        handle: PortHandle,
        owner: &Arc<InterruptGate>,
        error: E,
    ) -> E {
        match self.publish(handle, owner) {
            Ok(()) => error,
            Err(e) => e.into(),
        }
    }
}

impl Drop for PortClaim {
    fn drop(&mut self) {
        if self.published {
            return;
        }
        if let Ok(mut reg) = self.registry.inner.lock()
            && matches!(reg.slots.get(&self.name), Some((_, Slot::Claimed)))
        {
            reg.slots.remove(&self.name);
        }
    }
}

impl Default for PortRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PortRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Ports::default())),
        }
    }

    /// Publish a port. Errors with [`AsynError::PortAlreadyRegistered`]
    /// if `name` is already present — C parity: `asynManager::registerPort`
    /// refuses a duplicate name ("port %s already registered") instead of
    /// replacing the entry. A silent overwrite would orphan the prior
    /// handle: its runtime keeps running while the name resolves to the
    /// new port, silently shadowing legitimate I/O. To replace a port,
    /// [`Self::remove`] it first.
    ///
    /// This claims after the port was built, so it exists only for test
    /// fixtures (`test-support`): a port creator takes [`Self::claim`] before
    /// it builds and publishes through the [`PortClaim`], so a refused name
    /// builds nothing. `owner` is as for [`PortClaim::publish`].
    #[cfg(any(test, feature = "test-support"))]
    pub fn register(
        &self,
        name: &str,
        handle: PortHandle,
        owner: &Arc<InterruptGate>,
    ) -> AsynResult<()> {
        self.claim(name)?.publish(handle, owner)
    }

    /// Hold `name` for a port about to be built. Errors with
    /// [`AsynError::PortAlreadyRegistered`] if it is published or claimed,
    /// as C `registerPort` refuses a duplicate.
    pub fn claim(&self, name: &str) -> AsynResult<PortClaim> {
        let mut reg = self.inner.lock().unwrap();
        let seq = reg.next;
        match reg.slots.entry(name.to_string()) {
            Entry::Occupied(_) => {
                report_already_registered(name);
                Err(AsynError::PortAlreadyRegistered(name.to_string()))
            }
            Entry::Vacant(slot) => {
                slot.insert((seq, Slot::Claimed));
                reg.next += 1;
                Ok(PortClaim {
                    registry: self.clone(),
                    name: name.to_string(),
                    published: false,
                })
            }
        }
    }

    /// C's `callbackThread` is its own thread so it does not block the
    /// caller that set `interruptAccept` (the scan facility on the iocRun
    /// path); the sweep uses the port actor's blocking bridge, which must not
    /// run on the scan/setup thread.
    fn flush_in_background(&self, name: &str) {
        let (registry, name) = (self.clone(), name.to_string());
        let _ = std::thread::Builder::new()
            .name("asyn-boot-flush".into())
            .spawn(move || {
                if let Some(entry) = registry.get(&name) {
                    flush_port(&entry.handle);
                }
            });
    }

    pub fn get(&self, name: &str) -> Option<PortEntry> {
        let reg = self.inner.lock().ok()?;
        match &reg.slots.get(name)?.1 {
            Slot::Published(entry) => Some(PortEntry::clone(entry)),
            Slot::Claimed => None,
        }
    }

    /// Names of every published port, in registration order.
    ///
    /// C parity: `asynManager::report` with no port argument walks the
    /// global port list. Since every port creator publishes here, this is
    /// that list.
    pub fn names(&self) -> Vec<String> {
        let Ok(reg) = self.inner.lock() else {
            return Vec::new();
        };
        let mut named: Vec<(u64, &String)> = reg
            .slots
            .iter()
            .filter(|(_, (_, slot))| matches!(slot, Slot::Published(_)))
            .map(|(name, (seq, _))| (*seq, name))
            .collect();
        named.sort_unstable();
        named.into_iter().map(|(_, name)| name.clone()).collect()
    }

    /// Withdraw a published port. A name that was never published, or is
    /// claimed by a port still being built, is a no-op.
    pub fn remove(&self, name: &str) {
        if let Ok(mut reg) = self.inner.lock()
            && let Entry::Occupied(slot) = reg.slots.entry(name.to_string())
            && matches!(slot.get().1, Slot::Published(_))
        {
            slot.remove();
        }
    }
}

// ===== Global fallback (backward compatibility) =====

static GLOBAL_PORT_REGISTRY: OnceLock<PortRegistry> = OnceLock::new();

/// C `registerPort`'s refusal of a taken name, on stdout, before it returns
/// `asynError` (asynManager.c:2056). The `asynPortDriver` constructor's own
/// line follows it (see [`crate::manager::report_port_driver_register_failure`]).
pub(crate) fn report_already_registered(name: &str) {
    println!("asynCommon:registerDriver {name} already registered");
}

fn global_registry() -> &'static PortRegistry {
    GLOBAL_PORT_REGISTRY.get_or_init(PortRegistry::new)
}

/// Register an already-built port via the global registry, owned by `owner`'s
/// database; test fixtures only, see [`PortRegistry::register`].
///
/// Errors with [`AsynError::PortAlreadyRegistered`] on a duplicate name —
/// see [`PortRegistry::register`].
#[cfg(any(test, feature = "test-support"))]
pub fn register_port(name: &str, handle: PortHandle, owner: &Arc<InterruptGate>) -> AsynResult<()> {
    global_registry().register(name, handle, owner)
}

/// Hold `name` in the global registry for a port about to be built; see
/// [`PortRegistry::claim`].
pub fn claim_port(name: &str) -> AsynResult<PortClaim> {
    global_registry().claim(name)
}

/// Deliver a port's accumulated changed params once: C
/// `callbackThread::run` (`asynPortDriver.cpp:923-937`). Blocks on the port
/// actor, so it never runs on the scan or setup thread that opened the gate.
pub(crate) fn flush_port(handle: &PortHandle) {
    // C sweeps `addr` in `0..maxAddr`; a single-device port is `maxAddr == 1`.
    for addr in 0..handle.max_addr().max(1) {
        let _ = handle.boot_param_callbacks_blocking(addr);
    }
}

/// An owner that is already open, for a hand-registered port or a test that
/// has no scan facility: its ports deliver at once.
pub fn open_owner() -> Arc<InterruptGate> {
    InterruptGate::opened()
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
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        // No actor loop runs for this handle, so its `ActorId` is never
        // published on any thread — no caller can be this port's actor,
        // which is the truth these registry tests need.
        PortHandle::new(
            tx,
            name.to_string(),
            Arc::new(InterruptManager::new()),
            crate::port_actor::ActorLink::unowned(),
            Arc::new(TraceManager::new()),
        )
    }

    /// C parity: `asynManager::registerPort` refuses a duplicate name
    /// instead of replacing the entry — a silent overwrite would orphan
    /// the prior handle.
    #[test]
    fn register_rejects_duplicate_name() {
        let reg = PortRegistry::new();
        reg.register("regdup", dummy_handle("regdup"), &open_owner())
            .unwrap();
        match reg.register("regdup", dummy_handle("regdup"), &open_owner()) {
            Err(AsynError::PortAlreadyRegistered(name)) => assert_eq!(name, "regdup"),
            other => panic!("expected PortAlreadyRegistered, got {other:?}"),
        }
        // The original entry survives the rejected attempt.
        assert!(reg.get("regdup").is_some());
    }

    /// A claim holds the name against a second claim and a register, is not
    /// a port to `get`/`names`, and frees the name when dropped unpublished.
    #[test]
    fn a_claim_holds_the_name_until_published_or_dropped() {
        let reg = PortRegistry::new();
        let claim = reg.claim("claimed").unwrap();
        assert!(matches!(
            reg.claim("claimed"),
            Err(AsynError::PortAlreadyRegistered(_))
        ));
        assert!(matches!(
            reg.register("claimed", dummy_handle("claimed"), &open_owner()),
            Err(AsynError::PortAlreadyRegistered(_))
        ));
        assert!(reg.get("claimed").is_none());
        assert!(reg.names().is_empty());
        drop(claim);
        let claim = reg
            .claim("claimed")
            .expect("a dropped claim frees the name");
        claim
            .publish(dummy_handle("claimed"), &open_owner())
            .unwrap();
        assert!(reg.get("claimed").is_some());
        assert_eq!(reg.names(), ["claimed"]);
    }

    /// `remove()` withdraws published ports only: a claim survives it, so no
    /// other creator can take the name before the claim publishes.
    /// C's `asynPortList` is in `registerPort` order, and the claim is that
    /// call: a port published later than one claimed after it still lists
    /// first, and an unpublished claim is not listed.
    #[test]
    fn names_are_in_claim_order() {
        let reg = PortRegistry::new();
        reg.register("zeta", dummy_handle("zeta"), &open_owner())
            .unwrap();
        let alpha = reg.claim("alpha").unwrap();
        let mid = reg.claim("mid").unwrap();
        let pending = reg.claim("pending").unwrap();
        mid.publish(dummy_handle("mid"), &open_owner()).unwrap();
        alpha.publish(dummy_handle("alpha"), &open_owner()).unwrap();
        assert_eq!(reg.names(), ["zeta", "alpha", "mid"]);
        drop(pending);
        reg.remove("zeta");
        reg.register("zeta", dummy_handle("zeta"), &open_owner())
            .unwrap();
        assert_eq!(reg.names(), ["alpha", "mid", "zeta"]);
    }

    #[test]
    fn remove_leaves_a_pending_claim_alone() {
        let reg = PortRegistry::new();
        let claim = reg.claim("pending").unwrap();
        reg.remove("pending");
        assert!(matches!(
            reg.claim("pending"),
            Err(AsynError::PortAlreadyRegistered(_))
        ));
        claim
            .publish(dummy_handle("pending"), &open_owner())
            .unwrap();
        assert!(reg.get("pending").is_some());
        reg.remove("pending");
        assert!(reg.get("pending").is_none());
    }

    /// Boundary: `remove()` frees the name for re-registration.
    #[test]
    fn removed_name_can_be_reregistered() {
        let reg = PortRegistry::new();
        reg.register("regrecycle", dummy_handle("regrecycle"), &open_owner())
            .unwrap();
        reg.remove("regrecycle");
        assert!(
            reg.register("regrecycle", dummy_handle("regrecycle"), &open_owner())
                .is_ok(),
            "re-register after remove must succeed"
        );
    }

    /// A port has one owner, set once: a handle that already has one — it
    /// was registered, or built owned for a hand-driven driver — is refused,
    /// and nothing is claimed for it.
    #[test]
    fn registering_an_already_owned_handle_errors() {
        let reg = PortRegistry::new();
        let handle = dummy_handle("owned_once");
        reg.register("owned_once", handle.clone(), &open_owner())
            .unwrap();

        match reg.register("owned_twice", handle, &InterruptGate::new()) {
            Err(AsynError::PortAlreadyOwned(name)) => assert_eq!(name, "owned_twice"),
            other => panic!("expected PortAlreadyOwned, got {other:?}"),
        }
        assert!(reg.get("owned_twice").is_none(), "no name was claimed");
        assert!(
            handle_owner_unchanged(&reg),
            "the first owner is still the owner"
        );
    }

    fn handle_owner_unchanged(reg: &PortRegistry) -> bool {
        reg.get("owned_once")
            .is_some_and(|e| e.handle.interrupts().accepting())
    }
}
