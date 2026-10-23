#![allow(dead_code)]
//! Global exception callback system.
//!
//! [`ExceptionManager`] is owned by [`crate::manager::PortManager`] and delivers
//! port-state change notifications (connect, enable, autoConnect, trace changes)
//! to all registered callbacks.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, ThreadId};

use parking_lot::{Condvar, Mutex};

/// Types of asyn exceptions (port-state changes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsynException {
    Connect,
    Enable,
    AutoConnect,
    TraceMask,
    TraceIoMask,
    TraceInfoMask,
    TraceFile,
    TraceIoTruncateSize,
    /// Port is being shut down permanently.
    Shutdown,
}

/// An exception event delivered to registered callbacks. It borrows the
/// port's name, so an announce allocates nothing, as C's does not.
#[derive(Debug, Clone)]
pub struct ExceptionEvent<'a> {
    /// Name of the port that generated this exception.
    pub port_name: &'a str,
    /// The type of exception.
    pub exception: AsynException,
    /// Sub-address (-1 for port-level events).
    pub addr: i32,
}

/// Opaque handle returned by [`ExceptionManager::add_callback`].
/// Use with [`ExceptionManager::remove_callback`] to unregister.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExceptionCallbackId(u64);

type CallbackFn = Arc<dyn Fn(&ExceptionEvent<'_>) + Send + Sync>;

#[derive(Clone)]
struct CallbackEntry {
    id: ExceptionCallbackId,
    callback: CallbackFn,
}

/// Global exception callback manager.
///
/// Owned by [`crate::manager::PortManager`]. Ports announce state changes
/// through this manager, and all registered callbacks receive the events.
pub struct ExceptionManager {
    /// An announce takes the list with one reference count and allocates
    /// nothing; an edit copies it only while an announce still holds it.
    callbacks: Mutex<Arc<Vec<CallbackEntry>>>,
    next_id: AtomicU64,
    /// The threads announcing on this list, once per nested announce: C
    /// `dpCommon.exceptionActive` (asynManager.c:621, :634).
    active: Mutex<Vec<ThreadId>>,
    idle: Condvar,
}

impl ExceptionManager {
    pub fn new() -> Self {
        Self {
            callbacks: Mutex::new(Arc::default()),
            next_id: AtomicU64::new(1),
            active: Mutex::default(),
            idle: Condvar::new(),
        }
    }

    /// Register an exception callback. Returns an ID for later removal.
    pub fn add_callback<F>(&self, callback: F) -> ExceptionCallbackId
    where
        F: Fn(&ExceptionEvent<'_>) + Send + Sync + 'static,
    {
        let id = ExceptionCallbackId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let entry = CallbackEntry {
            id,
            callback: Arc::new(callback),
        };
        Arc::make_mut(&mut self.callbacks.lock()).push(entry);
        id
    }

    /// Remove a previously registered callback. While another thread
    /// announces on this list it waits for that announcement to end, as C
    /// `exceptionCallbackRemove` waits out `exceptionActive`
    /// (asynManager.c:1456-1461); an announcing thread removes at once.
    pub fn remove_callback(&self, id: ExceptionCallbackId) {
        let me = thread::current().id();
        let mut active = self.active.lock();
        while !active.is_empty() && !active.contains(&me) {
            self.idle.wait(&mut active);
        }
        Arc::make_mut(&mut self.callbacks.lock()).retain(|e| e.id != id);
    }

    /// This list is announcing on the current thread until the scope drops.
    pub(crate) fn announcing(&self) -> Announcing<'_> {
        self.active.lock().push(thread::current().id());
        Announcing(self)
    }

    /// How many callbacks are registered — C's `exceptionUsers`, the count
    /// `asynReport` prints for a port (`ellCount(&pdpc->exceptionUserList)`,
    /// asynManager.c:1073).
    pub fn callback_count(&self) -> usize {
        self.callbacks.lock().len()
    }

    /// Announce an exception event to all registered callbacks.
    ///
    /// The list is taken under the lock and the lock is released before any
    /// callback runs, so a callback may freely call [`Self::add_callback`] /
    /// [`Self::remove_callback`] (or re-enter `announce`) without deadlocking
    /// on the non-reentrant mutex.
    pub fn announce(&self, event: &ExceptionEvent<'_>) {
        let _active = self.announcing();
        let snapshot = Arc::clone(&self.callbacks.lock());
        for entry in snapshot.iter() {
            (entry.callback)(event);
        }
    }
}

/// An announcement in progress on an [`ExceptionManager`], from
/// [`ExceptionManager::announcing`].
pub(crate) struct Announcing<'a>(&'a ExceptionManager);

impl Drop for Announcing<'_> {
    fn drop(&mut self) {
        let me = thread::current().id();
        let mut active = self.0.active.lock();
        if let Some(i) = active.iter().position(|t| *t == me) {
            active.swap_remove(i);
        }
        if active.is_empty() {
            self.0.idle.notify_all();
        }
    }
}

/// One `dpCommon`'s registration on its exception list, as
/// [`crate::port_handle::PortHandle::exception_callback_add`] made it.
/// Dropping it is C `exceptionCallbackRemove` (asynManager.c:1384-1408).
#[must_use = "dropping the guard removes the callback"]
pub struct ExceptionCallbackGuard {
    list: Arc<ExceptionManager>,
    id: ExceptionCallbackId,
}

impl ExceptionCallbackGuard {
    pub(crate) fn new(list: Arc<ExceptionManager>, id: ExceptionCallbackId) -> Self {
        Self { list, id }
    }
}

impl Drop for ExceptionCallbackGuard {
    fn drop(&mut self) {
        self.list.remove_callback(self.id);
    }
}

impl Default for ExceptionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn test_add_and_announce() {
        let mgr = ExceptionManager::new();
        let count = Arc::new(AtomicUsize::new(0));
        let count2 = count.clone();

        mgr.add_callback(move |_event| {
            count2.fetch_add(1, Ordering::Relaxed);
        });

        let event = ExceptionEvent {
            port_name: "test",
            exception: AsynException::Connect,
            addr: -1,
        };
        mgr.announce(&event);
        mgr.announce(&event);
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_remove_callback() {
        let mgr = ExceptionManager::new();
        let count = Arc::new(AtomicUsize::new(0));
        let count2 = count.clone();

        let id = mgr.add_callback(move |_| {
            count2.fetch_add(1, Ordering::Relaxed);
        });

        let event = ExceptionEvent {
            port_name: "p",
            exception: AsynException::Enable,
            addr: -1,
        };
        mgr.announce(&event);
        assert_eq!(count.load(Ordering::Relaxed), 1);

        mgr.remove_callback(id);
        mgr.announce(&event);
        assert_eq!(count.load(Ordering::Relaxed), 1); // unchanged
    }

    /// C `exceptionCallbackRemove` waits out `exceptionActive`
    /// (asynManager.c:1456-1461): a removal during another thread's
    /// announcement returns after every callback of it has run.
    #[test]
    fn a_removal_waits_for_another_threads_announcement() {
        let mgr = Arc::new(ExceptionManager::new());
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let entered_tx = parking_lot::Mutex::new(entered_tx);
        let first = mgr.add_callback(move |_| {
            let _ = entered_tx.lock().send(());
        });
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let d = Arc::clone(&done);
        mgr.add_callback(move |_| {
            std::thread::sleep(std::time::Duration::from_millis(100));
            d.store(true, Ordering::Release);
        });

        let m = Arc::clone(&mgr);
        let announcer = std::thread::spawn(move || {
            m.announce(&ExceptionEvent {
                port_name: "p",
                exception: AsynException::Connect,
                addr: -1,
            })
        });
        entered_rx.recv().unwrap();
        mgr.remove_callback(first);
        assert!(done.load(Ordering::Acquire));
        announcer.join().unwrap();
    }

    /// A callback removing one from its own list does not wait for itself.
    #[test]
    fn a_callback_removes_from_its_own_list_at_once() {
        let mgr = Arc::new(ExceptionManager::new());
        let count = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&count);
        let victim = mgr.add_callback(move |_| {
            c.fetch_add(1, Ordering::Relaxed);
        });
        let m = Arc::downgrade(&mgr);
        mgr.add_callback(move |_| {
            if let Some(m) = m.upgrade() {
                m.remove_callback(victim);
            }
        });
        let event = ExceptionEvent {
            port_name: "p",
            exception: AsynException::Enable,
            addr: -1,
        };
        mgr.announce(&event);
        mgr.announce(&event);
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_multiple_callbacks() {
        let mgr = ExceptionManager::new();
        let a = Arc::new(AtomicUsize::new(0));
        let b = Arc::new(AtomicUsize::new(0));
        let a2 = a.clone();
        let b2 = b.clone();

        mgr.add_callback(move |_| {
            a2.fetch_add(1, Ordering::Relaxed);
        });
        mgr.add_callback(move |_| {
            b2.fetch_add(10, Ordering::Relaxed);
        });

        let event = ExceptionEvent {
            port_name: "p",
            exception: AsynException::AutoConnect,
            addr: 0,
        };
        mgr.announce(&event);
        assert_eq!(a.load(Ordering::Relaxed), 1);
        assert_eq!(b.load(Ordering::Relaxed), 10);
    }

    #[test]
    fn test_event_carries_port_name() {
        let mgr = ExceptionManager::new();
        let captured = Arc::new(Mutex::new(String::new()));
        let captured2 = captured.clone();

        mgr.add_callback(move |event| {
            *captured2.lock() = event.port_name.to_string();
        });

        mgr.announce(&ExceptionEvent {
            port_name: "myport",
            exception: AsynException::Connect,
            addr: -1,
        });
        assert_eq!(&*captured.lock(), "myport");
    }
}
