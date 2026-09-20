//! Socket readiness for the targets with no mio selector — a poll thread, an
//! armed set, and an `AsyncRead`/`AsyncWrite` adapter over a non-blocking fd.
//!
//! # Why this exists at all
//!
//! `mio` has no selector for either embedded triple. Measured against
//! mio 1.2.3 with `-Zbuild-std`: `armv7-rtems-eabihf` and `x86_64-wrs-vxworks`
//! both fail at `error[E0583]: file not found for module 'selector'`, because
//! `mio/src/sys/unix/mod.rs` maps `target_os` to a selector file and neither
//! target appears in any of its lists. Forcing mio's own escape hatches
//! (`mio_unsupported_force_poll_poll`, `mio_unsupported_force_waker_pipe`)
//! gets the two down to 8 and 9 errors respectively, all of them missing
//! `libc` items (`accept4`, `SOCK_NONBLOCK`, `POLLRDNORM`, `sin_len`) — so the
//! path through mio is a set of PRs across three upstream crates, not a build
//! flag. Until those land this module is the readiness layer.
//!
//! It is deliberately **not** a mio clone. It carries exactly what a PVA
//! server connection needs: level-triggered read/write readiness on sockets,
//! one waker per direction, and nothing else — no edge triggering, no
//! `Interest` bitsets, no cross-thread registration of foreign event sources.
//!
//! # The armed set is owned by the poll thread, and nothing else
//!
//! > **MUST** an armed interest is one-shot: the poll thread removes
//! > `(fd, direction)` from the armed set *before* it wakes that direction's
//! > waker, and only the task that then observed `WouldBlock` may arm it
//! > again.
//! > **MUST NOT** any path remove an fd from the armed set other than
//! > `StreamInner`'s `Drop` and the one-shot removal above — and no fd may
//! > be closed while the kernel is still watching it, which is why the poll
//! > thread is the one that closes it.
//!
//! The first half is what keeps the poll thread from spinning: a
//! level-triggered backend reports a readable socket on every wait until the
//! bytes are consumed, so an interest that survived its own wake-up would
//! re-report immediately and burn the CPU that a reactor exists to save. The
//! second half is what keeps it correct: an fd left in a `select` set past
//! `close(2)` is `EBADF` from the next call — at entry on Linux, on every
//! rescan under the BSD `kern_select` libbsd carries — and a number the kernel
//! may since have handed to a different socket. So dropping the last handle to
//! a stream does not close its socket. It hands the socket to the poll thread,
//! which closes it in the same `wait` that takes the fd out of the kernel's
//! sets — after the removal and before it blocks — and a socket the kernel was
//! never told about is closed on the spot, there being nothing to remove
//! first. The order holds by construction rather than by convention: nothing
//! else holds the socket once the last handle is gone, and the delete and the
//! close travel in one changelist.
//!
//! One-shot arming is also what makes registering *after* a `WouldBlock` race-
//! free. Between the failed read and the arm, the peer may have sent the byte
//! that would have woken us; a level-triggered backend still reports the
//! socket ready on the very next wait, so the wake-up cannot be lost. An
//! edge-triggered backend would need the arm to precede the read attempt, and
//! that is the ordering this module does not use. `EV_CLEAR` is therefore
//! banned for socket interests in the kqueue backend; the one place it appears
//! there is the `EVFILT_USER` wake, which carries no readiness to lose.
//!
//! # Waiting is (changes in, ready set out)
//!
//! This is libevent's `eventop` split — `add` and `del` accumulate, `dispatch`
//! submits — which is what pvxs runs under on every platform it supports, and
//! what this module now follows. `Backend::wait` takes the changes since the
//! last call rather than the armed set, so a registration placed once stays in
//! the kernel until this module deletes it and a wait with nothing to change
//! submits an empty changelist. The cost of a wait is then the number of
//! *transitions*, where the whole-set resubmission it replaces charged the
//! number of armed fds on every call whether or not any of them had moved. On
//! `armv7-rtems-eabihf` that is worth 3.7x to `kqueue` at 112 held
//! connections and nothing at all to `select`, which re-registers every
//! watched fd inside the kernel on every call because `select(2)` has nowhere
//! to remember one between calls — a cost no changelist can reach. The kqueue
//! backend's doc carries the table.
//!
//! The one-shot rule above is what produces most of those transitions:
//! delivering `(fd, direction)` queues that direction's delete, and the task's
//! re-arm after its next `WouldBlock` queues the add. Two changes per
//! readiness event, and on a busy connection the only two.
//!
//! `Change`es accumulate under the same lock as the armed set, so the kernel
//! hears about a transition in the order the armed set recorded it. That is
//! what makes the awkward case — an fd closed and handed straight back out to
//! the next connection — fall out of the design rather than need handling, and
//! it is why both of libevent's `changelist` rules come along:
//!
//! - An add over a pending delete stays an add and never collapses to a
//!   no-op, because that delete may name an fd that has since closed and come
//!   back as a different socket under the same number.
//! - A delete of an interest the kernel was never told about *is* a no-op.
//!   `Direction::registered` is what tells the two apart, and dropping the
//!   pair is what keeps kqueue from reporting a spurious event for an
//!   add-then-delete that never left this process.
//!
//! A delete that misses in the kernel is ordinary rather than exceptional on
//! `kqueue`: that backend closes the socket before the `kevent` call carrying
//! the delete, because the same call also blocks and a close after it would
//! wait for the next wake-up; `close(2)` drops the knotes itself, so the
//! delete then finds nothing, and the backend ignores that. `select` closes
//! after clearing the bits and before blocking, which is the order its
//! kernel-side scan needs.
//!
//! New registrations arriving while the thread is blocked in the syscall are
//! what `Backend::interrupt` is for; it is the only reason this module needs
//! a self-pipe (`select`) or a user event (`kqueue`).

// RTEMS-EXEC-MODEL-ALLOW(7): the seven poller tests park a `tokio::spawn`ed
// task on a `ReadyStream` half, so the property under test is that the poller
// wakes a tokio task — the tokio flavor is the subject, not an accident. They
// run on the driver `#[tokio::test]` builds and pass in the exec-backend suite
// (measured: `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run
// -p epics-libcom-rs -E 'test(/runtime::readiness::/)'`, 22/22).

use std::collections::HashMap;
use std::io;
use std::mem::ManuallyDrop;
use std::net::TcpStream;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Waker;

#[cfg(target_os = "rtems")]
mod kqueue;
mod select;

/// Which backend a poller waits on, decided once at construction.
///
/// Both arms are compiled on RTEMS rather than `cfg`-ed apart, so a type error
/// in either fails the RTEMS gate; the choice between them is a runtime one
/// because the fact it turns on — the BSP's own RTEMS version — is only
/// knowable at runtime. See `rtems_kqueue_usable`.
enum ActiveBackend {
    Select(select::SelectBackend),
    #[cfg(target_os = "rtems")]
    Kqueue(kqueue::KqueueBackend),
}

impl ActiveBackend {
    /// Choose, open, and say which — on RTEMS, where there is a choice.
    ///
    /// The line is an `eprintln!` from the construction path for the same
    /// reason [`report_lock_protocol`](crate::runtime::sync::report_lock_protocol)
    /// is one: the RTEMS target has no iocsh to ask afterwards, and a
    /// diagnostic that reports which guarantee the process got must not
    /// itself depend on a `tracing` subscriber being installed and unfiltered.
    /// It is printed after the backend opened, so it never names one that
    /// failed.
    ///
    /// Reporting the *reason* beside the choice is what makes the line
    /// actionable — "select (RTEMS 6.0, EPICS_RTEMS_KQUEUE unset)" names the
    /// variable to set — and [`rtems_kqueue_usable`] returns both from one
    /// call so the reason cannot drift from the rule that was applied.
    #[cfg(target_os = "rtems")]
    fn new(poller: &str) -> io::Result<Self> {
        let (usable, why) = rtems_kqueue_usable();
        let (backend, name) = if usable {
            (Self::Kqueue(kqueue::KqueueBackend::new()?), "kqueue")
        } else {
            (Self::Select(select::SelectBackend::new()?), "select")
        };
        eprintln!("epics-rs: readiness poller {poller}: {name} backend ({why})");
        Ok(backend)
    }

    /// No choice to report: `select` is the only backend off RTEMS.
    #[cfg(not(target_os = "rtems"))]
    fn new(_poller: &str) -> io::Result<Self> {
        Ok(Self::Select(select::SelectBackend::new()?))
    }
}

impl Backend for ActiveBackend {
    fn wait(
        &self,
        changes: &[Change],
        closing: &mut Vec<TcpStream>,
        ready: &mut Vec<(RawFd, Interest)>,
    ) -> io::Result<()> {
        match self {
            Self::Select(b) => b.wait(changes, closing, ready),
            #[cfg(target_os = "rtems")]
            Self::Kqueue(b) => b.wait(changes, closing, ready),
        }
    }

    fn interrupt(&self) -> io::Result<()> {
        match self {
            Self::Select(b) => b.interrupt(),
            #[cfg(target_os = "rtems")]
            Self::Kqueue(b) => b.interrupt(),
        }
    }
}

#[cfg(target_os = "rtems")]
unsafe extern "C" {
    /// `rtems/version.h:90,97`. Defined in `librtemscpu.a` on both prefixes
    /// `scripts/rtems-bsp.sh` builds (`nm --defined-only`: `T
    /// rtems_version_major`), and absent from `libc`.
    fn rtems_version_major() -> libc::c_int;
    fn rtems_version_minor() -> libc::c_int;
}

/// pvxs's rule, applied to the BSP this image actually booted on.
///
/// pvxs gates kqueue at RTEMS 6.3 (`> 6.2`) — epics-base/pvxs#197 — because
/// the RTEMS 5-era workaround below that version steers libevent onto a `poll`
/// backend that never blocks on this BSP. The version is read at runtime, not
/// at build time, because a branch build reports its series' development
/// version (7.0.0 on main, 6.0.0 on the 6 branch) and only the running system
/// knows which one it is.
///
/// `EPICS_RTEMS_KQUEUE` overrides the rule in both directions. A series-6
/// prefix built by `scripts/rtems-bsp.sh` carries the libbsd and kernel fixes
/// the rule is a proxy for — the script asserts they are in the tree it builds
/// — yet reports 6.0.0, so that prefix's `epics-rs-env.sh` sets
/// `EPICS_RTEMS_KQUEUE=1` for exactly that case. It is read here in the
/// *target* process, so what carries it is the boot command line
/// (`epics_rtems_boot::boot_args`); `scripts/embedded-image.sh` bakes the
/// build environment's value into the image for that reason.
///
/// Returns the reason beside the answer so the line
/// [`ActiveBackend::new`] prints names the rule that was actually applied.
#[cfg(target_os = "rtems")]
fn rtems_kqueue_usable() -> (bool, String) {
    if let Some(raw) = super::env::get("EPICS_RTEMS_KQUEUE") {
        if let Some(forced) = parse_override(&raw) {
            return (forced, format!("EPICS_RTEMS_KQUEUE={}", raw.trim()));
        }
        // Not a third answer (see `parse_override`), but the operator set
        // something and is owed the reason it did nothing.
        eprintln!(
            "epics-rs: EPICS_RTEMS_KQUEUE={} is neither 1/yes/true nor 0/no/false; \
             the RTEMS version rule decides",
            raw.trim()
        );
    }
    // SAFETY: both are argument-free integer returns from librtemscpu.
    let (major, minor) = unsafe { (rtems_version_major(), rtems_version_minor()) };
    let usable = major > 6 || (major == 6 && minor > 2);
    (
        usable,
        format!("RTEMS {major}.{minor}, EPICS_RTEMS_KQUEUE unset"),
    )
}

/// The override's spellings. `scripts/rtems-bsp.sh` writes `1`; a value that
/// is none of these is not a third answer, so it falls through to the version
/// rule rather than being read as one direction or the other.
#[cfg(target_os = "rtems")]
fn parse_override(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "yes" | "true" => Some(true),
        "0" | "no" | "false" => Some(false),
        _ => None,
    }
}

/// The direction a task is waiting on. One waker per direction per fd — a PVA
/// connection has one reader and one writer, and they are different tasks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Interest {
    Read,
    Write,
}

/// The largest fd this module can wait on, as a bit index into the `select`
/// fd set.
///
/// `select` addresses fds by bit position, so the cost of a high fd is a
/// larger bitmap and nothing else. 4096 covers both embedded triples with
/// room: `x86_64-wrs-vxworks`'s limit is the VSB's `_WRS_CONFIG_FD_SET_SIZE`,
/// 2048 in the SDK measured here, and RTEMS sizes its own set from
/// `CONFIGURE_MAXIMUM_FILE_DESCRIPTORS`. An fd at or above this is refused at
/// registration with `EMFILE` rather than silently dropped, because a dropped
/// interest is a connection that hangs forever.
pub const FD_CAPACITY: usize = 4096;

/// One thing to tell the kernel about one direction on one fd — libevent's
/// `EV_CHANGE_ADD` / `EV_CHANGE_DEL`, narrowed to the one direction it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Change {
    fd: RawFd,
    interest: Interest,
    action: Action,
}

/// Which way a [`Change`] goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    /// Start watching `(fd, interest)`, or re-affirm a watch already placed.
    Arm,
    /// Stop watching it.
    Disarm,
}

/// Refuse an fd this module cannot address, rather than dropping it silently.
///
/// Shared by both backends because both have the same limit for the same
/// reason: [`FD_CAPACITY`] is the `select` bitmap's width, and a poller whose
/// two backends disagreed about which fds it accepts would fail differently
/// depending on which one the target picked.
fn check_fd(fd: RawFd) -> io::Result<()> {
    if fd < 0 || (fd as usize) >= FD_CAPACITY {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("fd {fd} is beyond the readiness poller's {FD_CAPACITY}-fd capacity"),
        ));
    }
    Ok(())
}

/// What a backend must do. Two calls, both from the poll thread except
/// [`Backend::interrupt`], which is called from whichever thread arms an
/// interest.
trait Backend: Send + Sync + 'static {
    /// Apply `changes`, close every socket in `closing`, then wait until at
    /// least one watched interest is ready and push the ready ones into
    /// `ready`. Returns without error and with an empty `ready` when only
    /// [`Backend::interrupt`] fired.
    ///
    /// > **MUST** every change is applied and every socket in `closing` is
    /// > closed before the call can block, so a `wait` that fails with
    /// > [`io::ErrorKind::Interrupted`] has still taken both.
    ///
    /// That is what lets [`poll_loop`] treat `Interrupted` as "go round
    /// again" without re-queueing anything: a change leaves the armed set
    /// exactly once, and a backend that deferred one past a blocking point
    /// would strand the task that asked for it with no second chance to ask.
    /// `select` satisfies it by keeping its fd sets in this process; `kqueue`
    /// by submitting the changelist in the same `kevent` call, which applies
    /// it before the wait begins.
    ///
    /// The sockets in `closing` are the ones whose deletes are in `changes`.
    /// `select` closes them after the bits are cleared, which is the order its
    /// scan needs; `kqueue` closes them before the `kevent` that carries the
    /// deletes, which then miss harmlessly, because that call is also the one
    /// that blocks.
    fn wait(
        &self,
        changes: &[Change],
        closing: &mut Vec<TcpStream>,
        ready: &mut Vec<(RawFd, Interest)>,
    ) -> io::Result<()>;

    /// Break a concurrent [`Backend::wait`] out of its syscall.
    fn interrupt(&self) -> io::Result<()>;
}

/// One direction on one fd: who to wake, what the kernel has been told, and
/// what it has yet to be told.
#[derive(Default)]
struct Direction {
    waker: Option<Waker>,
    /// Whether the kernel is currently watching this direction — libevent's
    /// `event_change::old_events`. It is the field that makes deleting
    /// something never added a no-op instead of a syscall that fails.
    registered: bool,
    /// The change no backend has been handed yet.
    pending: Option<Action>,
}

impl Direction {
    /// Nobody to wake, nothing in the kernel, nothing left to say.
    fn is_idle(&self) -> bool {
        self.waker.is_none() && !self.registered && self.pending.is_none()
    }
}

#[derive(Default)]
struct Slot {
    read: Direction,
    write: Direction,
    /// The socket, once its last handle is dropped, held until the delete the
    /// kernel is owed for it goes out — see [`Inner::disarm_all`].
    closing: Option<TcpStream>,
}

impl Slot {
    fn direction(&mut self, interest: Interest) -> &mut Direction {
        match interest {
            Interest::Read => &mut self.read,
            Interest::Write => &mut self.write,
        }
    }

    fn has_waker(&self) -> bool {
        self.read.waker.is_some() || self.write.waker.is_some()
    }

    fn is_dirty(&self) -> bool {
        self.read.pending.is_some() || self.write.pending.is_some()
    }

    fn is_idle(&self) -> bool {
        self.read.is_idle() && self.write.is_idle() && self.closing.is_none()
    }
}

/// The armed set and the changes the kernel has not been told about yet, under
/// one lock so the two cannot disagree about the order of a transition.
#[derive(Default)]
struct Armed {
    slots: HashMap<RawFd, Slot>,
    /// The fds carrying at least one pending change, in the order they came to
    /// carry one. An fd is pushed on the clean-to-dirty edge, so it lands here
    /// once per drain rather than once per change; an entry left behind by a
    /// slot that went idle before the drain is skipped there, which is cheaper
    /// than searching this vector to remove it.
    dirty: Vec<RawFd>,
}

struct Inner {
    armed: Mutex<Armed>,
    shutdown: AtomicBool,
    backend: ActiveBackend,
}

impl Inner {
    /// Arm `(fd, interest)` with `waker`, replacing whatever waker that
    /// direction held.
    fn arm(&self, fd: RawFd, interest: Interest, waker: &Waker) -> io::Result<()> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "readiness poller is shut down",
            ));
        }
        {
            let mut armed = self.armed.lock().expect("readiness armed set poisoned");
            let Armed { slots, dirty } = &mut *armed;
            let slot = slots.entry(fd).or_default();
            let was_dirty = slot.is_dirty();
            let cell = slot.direction(interest);
            match &cell.waker {
                // `will_wake` is the common case on a re-poll of the same
                // task: keep the waker we have rather than cloning again.
                Some(existing) if existing.will_wake(waker) => {}
                _ => cell.waker = Some(waker.clone()),
            }
            // An add replaces a pending delete and is never cancelled into a
            // no-op, which is libevent's rule and for libevent's reason: that
            // delete may have named an fd this number no longer stands for,
            // and the kernel dropped the registration with the `close`.
            cell.pending = Some(Action::Arm);
            if !was_dirty {
                dirty.push(fd);
            }
        }
        self.backend.interrupt()
    }

    /// Take a socket back: drop every interest on it and close it once the
    /// kernel is no longer watching it. The one path that removes an fd
    /// besides the one-shot removal in the poll loop — see the module
    /// invariant.
    ///
    /// The close is the poll thread's when the kernel holds a registration for
    /// the fd — the socket rides in the slot until the delete goes out — and
    /// immediate when it does not: an fd the kernel was never told about, or
    /// one on a poller whose thread is gone. `shutdown` is read under the
    /// armed lock because [`Inner::retire`] sets it under the same lock, so
    /// the two cannot both miss the socket.
    fn disarm_all(&self, socket: TcpStream) {
        let fd = socket.as_raw_fd();
        let close_now = {
            let mut armed = self.armed.lock().expect("readiness armed set poisoned");
            if self.shutdown.load(Ordering::Acquire) {
                Some(socket)
            } else {
                let Armed { slots, dirty } = &mut *armed;
                match slots.get_mut(&fd) {
                    None => Some(socket),
                    Some(slot) => {
                        let was_dirty = slot.is_dirty();
                        for interest in [Interest::Read, Interest::Write] {
                            let cell = slot.direction(interest);
                            cell.waker = None;
                            cell.pending = cell.registered.then_some(Action::Disarm);
                        }
                        if slot.is_dirty() {
                            slot.closing = Some(socket);
                            if !was_dirty {
                                dirty.push(fd);
                            }
                            None
                        } else {
                            // Nothing registered — an add the poll thread
                            // never drained was cancelled just above — so
                            // there is nothing to remove before the close.
                            slots.remove(&fd);
                            Some(socket)
                        }
                    }
                }
            }
        };
        drop(close_now);
        // The armed set is no longer rebuilt from scratch on every wait, so
        // this delete — and the close behind it — reaches the kernel only
        // when the poll thread next drains. Waking it now is what holds that
        // window to the wait already in flight rather than to whenever some
        // other fd is next armed.
        let _ = self.backend.interrupt();
    }

    /// Take the waker for one ready interest and stop watching that direction
    /// — the one-shot half of the module invariant, kernel side included.
    fn take_ready(&self, fd: RawFd, interest: Interest) -> Option<Waker> {
        let mut armed = self.armed.lock().expect("readiness armed set poisoned");
        let Armed { slots, dirty } = &mut *armed;
        let slot = slots.get_mut(&fd)?;
        let was_dirty = slot.is_dirty();
        let cell = slot.direction(interest);
        let waker = cell.waker.take();
        cell.pending = cell.registered.then_some(Action::Disarm);
        if slot.is_idle() {
            slots.remove(&fd);
        } else if slot.is_dirty() && !was_dirty {
            dirty.push(fd);
        }
        waker
    }

    /// How many fds hold at least one waker. See [`Poller::armed_fds`].
    fn armed_fds(&self) -> usize {
        self.armed
            .lock()
            .expect("readiness armed set poisoned")
            .slots
            .values()
            .filter(|slot| slot.has_waker())
            .count()
    }

    /// Move the pending changes into `out`, the sockets whose deletes they
    /// carry into `closing`, and record that the kernel is about to be told
    /// about them.
    ///
    /// `registered` is committed here rather than after the syscall because of
    /// the [`Backend::wait`] MUST: the only failure that leaves this poller
    /// running is `Interrupted`, and that one happens after the changelist is
    /// in.
    fn drain_changes(&self, out: &mut Vec<Change>, closing: &mut Vec<TcpStream>) {
        out.clear();
        debug_assert!(
            closing.is_empty(),
            "the backend owes every socket it was handed a close before it blocks"
        );
        let mut armed = self.armed.lock().expect("readiness armed set poisoned");
        let Armed { slots, dirty } = &mut *armed;
        for fd in dirty.drain(..) {
            let Some(slot) = slots.get_mut(&fd) else {
                continue;
            };
            for interest in [Interest::Read, Interest::Write] {
                let cell = slot.direction(interest);
                let Some(action) = cell.pending.take() else {
                    continue;
                };
                cell.registered = action == Action::Arm;
                out.push(Change {
                    fd,
                    interest,
                    action,
                });
            }
            // Both directions' deletes are in `out` now — `disarm_all` queued
            // them together with the socket — so the socket goes with them.
            if let Some(socket) = slot.closing.take() {
                closing.push(socket);
            }
            if slot.is_idle() {
                slots.remove(&fd);
            }
        }
    }

    /// The poll thread's last act, on either exit: shut the poller, wake every
    /// parked task, and close every socket held for a delete that will now
    /// never go out — the kernel stops watching when the thread stops
    /// waiting.
    ///
    /// `shutdown` is stored under the armed lock so that a concurrent
    /// [`Inner::disarm_all`] either sees it set and closes its socket itself,
    /// or has stored the socket in a slot this sweep takes. A task woken here
    /// re-polls its socket, sees `WouldBlock` and re-arms, and it is the
    /// refused re-arm that hands it the error it unwinds on.
    fn retire(&self) {
        let mut armed = self.armed.lock().expect("readiness armed set poisoned");
        self.shutdown.store(true, Ordering::Release);
        let Armed { slots, dirty } = &mut *armed;
        dirty.clear();
        for (_, slot) in slots.drain() {
            let Slot {
                read,
                write,
                closing,
            } = slot;
            for cell in [read, write] {
                if let Some(w) = cell.waker {
                    w.wake();
                }
            }
            drop(closing);
        }
    }
}

/// A readiness poller and the thread that owns its armed set.
///
/// One per server. [`Poller::shutdown`] stops the thread; dropping the last
/// handle does the same.
pub struct Poller {
    inner: Arc<Inner>,
}

impl Poller {
    /// Start a poller and its thread.
    ///
    /// `name` is the thread name, which is what an `epicsThreadShowAll`
    /// equivalent prints; keep it short.
    ///
    /// `priority` is the caller's, not this module's: the poll thread is the
    /// reactor of whatever server owns it, and a server's band is set against
    /// the *other* servers in the IOC — pvxs runs its TCP reactor at
    /// `CAServerLow-2`, CA's rsrv its own ladder from `caservertask.c`. A
    /// constant here would put every server's reactor in one band and silently
    /// undo that ordering.
    pub fn new(
        name: &str,
        priority: crate::runtime::task::ThreadPriority,
    ) -> io::Result<Arc<Self>> {
        let inner = Arc::new(Inner {
            armed: Mutex::new(Armed::default()),
            shutdown: AtomicBool::new(false),
            backend: ActiveBackend::new(name)?,
        });
        let thread_inner = Arc::clone(&inner);
        crate::runtime::task::spawn_dedicated_thread(
            name.to_string(),
            priority,
            crate::runtime::task::StackSizeClass::Small,
            move || poll_loop(&thread_inner),
        )?;
        Ok(Arc::new(Self { inner }))
    }

    /// Wrap an accepted socket so the protocol code can read and write it
    /// through this poller.
    pub fn stream(self: &Arc<Self>, stream: std::net::TcpStream) -> io::Result<ReadyStream> {
        stream.set_nonblocking(true)?;
        let fd = stream.as_raw_fd();
        if fd < 0 || fd as usize >= FD_CAPACITY {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("fd {fd} is beyond the readiness poller's {FD_CAPACITY}-fd capacity"),
            ));
        }
        Ok(ReadyStream {
            inner: Arc::new(StreamInner {
                poller: Arc::clone(self),
                stream: ManuallyDrop::new(stream),
            }),
        })
    }

    /// Stop the poll thread. Idempotent.
    pub fn shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::Release);
        let _ = self.inner.backend.interrupt();
    }

    /// How many fds currently hold at least one armed interest. Test and
    /// report surface; not a control input.
    ///
    /// An fd whose last waker has been taken but whose kernel registration is
    /// still queued for deletion is not one of them. The count is of tasks
    /// parked here, which is the question a report is asking.
    pub fn armed_fds(&self) -> usize {
        self.inner.armed_fds()
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn poll_loop(inner: &Arc<Inner>) {
    let mut ready: Vec<(RawFd, Interest)> = Vec::new();
    let mut changes: Vec<Change> = Vec::new();
    let mut closing: Vec<TcpStream> = Vec::new();
    while !inner.shutdown.load(Ordering::Acquire) {
        inner.drain_changes(&mut changes, &mut closing);
        ready.clear();
        match inner.backend.wait(&changes, &mut closing, &mut ready) {
            Ok(()) => {}
            // The changes are not re-queued, and need not be: `Backend::wait`
            // owes them and the closes to the kernel before it may block, so
            // the interrupted call is one that already took them.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                // A backend that cannot wait cannot serve anyone; `retire`
                // below is what tells the parked tasks.
                //
                // `eprintln!` for the reason `ActiveBackend::new` uses it: a
                // poller dying is a diagnostic that must not depend on a
                // `tracing` subscriber being installed.
                eprintln!("epics-rs: readiness poller cannot wait, shutting down: {e}");
                break;
            }
        }
        for &(fd, interest) in &ready {
            if let Some(waker) = inner.take_ready(fd, interest) {
                waker.wake();
            }
        }
    }
    inner.retire();
    // A wait that failed may have refused its `closing`; those sockets close
    // with the vector, after the sweep, when the kernel is watching nothing.
}

/// The socket and the poller it is registered with — the state every handle
/// to one connection shares.
///
/// Dropping the last handle does not close the socket. It hands the socket to
/// the poller ([`Inner::disarm_all`]), which closes it once the kernel is no
/// longer watching the fd — the module invariant, held by construction rather
/// than by field order.
struct StreamInner {
    poller: Arc<Poller>,
    /// Taken in `Drop`, once, to hand to the poller.
    stream: ManuallyDrop<TcpStream>,
}

impl Drop for StreamInner {
    fn drop(&mut self) {
        // SAFETY: taken exactly once, here, and `self.stream` is never read
        // again — `drop` is the last code to see `self`.
        let socket = unsafe { ManuallyDrop::take(&mut self.stream) };
        self.poller.inner.disarm_all(socket);
    }
}

impl StreamInner {
    fn arm(&self, interest: Interest, waker: &Waker) -> io::Result<()> {
        self.poller
            .inner
            .arm(self.stream.as_raw_fd(), interest, waker)
    }

    fn poll_read(
        &self,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        use std::io::Read;
        // SAFETY-adjacent, but not `unsafe`: read into the uninitialised tail
        // is avoided by going through `initialize_unfilled`, which is what
        // costs a memset and what keeps this file free of raw buffers.
        let dst = buf.initialize_unfilled();
        match (&*self.stream).read(dst) {
            Ok(n) => {
                buf.advance(n);
                std::task::Poll::Ready(Ok(()))
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                match self.arm(Interest::Read, cx.waker()) {
                    Ok(()) => std::task::Poll::Pending,
                    Err(e) => std::task::Poll::Ready(Err(e)),
                }
            }
            Err(e) => std::task::Poll::Ready(Err(e)),
        }
    }

    fn poll_write(
        &self,
        cx: &mut std::task::Context<'_>,
        src: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        use std::io::Write;
        match (&*self.stream).write(src) {
            Ok(n) => std::task::Poll::Ready(Ok(n)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                match self.arm(Interest::Write, cx.waker()) {
                    Ok(()) => std::task::Poll::Pending,
                    Err(e) => std::task::Poll::Ready(Err(e)),
                }
            }
            Err(e) => std::task::Poll::Ready(Err(e)),
        }
    }

    fn poll_shutdown(&self) -> std::task::Poll<io::Result<()>> {
        match self.stream.shutdown(std::net::Shutdown::Write) {
            Ok(()) => std::task::Poll::Ready(Ok(())),
            // The peer closing first makes our half-close a no-op, not a
            // failure the connection loop should report.
            Err(e) if e.kind() == io::ErrorKind::NotConnected => std::task::Poll::Ready(Ok(())),
            Err(e) => std::task::Poll::Ready(Err(e)),
        }
    }
}

/// A non-blocking `TcpStream` that reads and writes through a [`Poller`].
pub struct ReadyStream {
    inner: Arc<StreamInner>,
}

impl ReadyStream {
    /// Two handles to the one socket, as a connection protocol takes them: a
    /// reader and a writer that are polled by different tasks.
    ///
    /// Splitting costs nothing at the poller: read and write are already
    /// separate interests on the same fd, each with its own waker, so the two
    /// halves park independently on the one registration they share. There is
    /// no lock between them either — the halves call `read`/`write` through
    /// `&TcpStream`, and the kernel already serialises those per direction.
    pub fn split(self) -> (ReadHalf, WriteHalf) {
        (
            ReadHalf {
                inner: Arc::clone(&self.inner),
            },
            WriteHalf { inner: self.inner },
        )
    }

    /// The wrapped socket, for the options a driver sets after accept
    /// (`SO_SNDBUF`, keepalive).
    pub fn socket(&self) -> &TcpStream {
        &self.inner.stream
    }
}

/// The read half of a split [`ReadyStream`].
pub struct ReadHalf {
    inner: Arc<StreamInner>,
}

/// The write half of a split [`ReadyStream`].
pub struct WriteHalf {
    inner: Arc<StreamInner>,
}

impl tokio::io::AsyncRead for ReadyStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        self.inner.poll_read(cx, buf)
    }
}

impl tokio::io::AsyncRead for ReadHalf {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        self.inner.poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for ReadyStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        src: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        self.inner.poll_write(cx, src)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        // A TCP socket has no userspace buffer of ours to flush: every
        // accepted byte is already in the kernel's send buffer.
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        self.inner.poll_shutdown()
    }
}

impl tokio::io::AsyncWrite for WriteHalf {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        src: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        self.inner.poll_write(cx, src)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        self.inner.poll_shutdown()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::task::ThreadPriority;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A connected pair over loopback: (accepted server side, client side).
    fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let client = TcpStream::connect(addr).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        (server, client)
    }

    /// Whether `fd` is still open in this process. Sound as a test probe
    /// because nextest runs each test in a process of its own, so a number
    /// this test closed is reused only by this test.
    fn is_open(fd: RawFd) -> bool {
        // SAFETY: `F_GETFD` takes no pointer and touches nothing.
        unsafe { libc::fcntl(fd, libc::F_GETFD) != -1 }
    }

    /// Two round trips, not one: the second is what fails if the delete the
    /// first delivery queued outlives the re-arm that follows it, or if the
    /// re-arm is skipped because the kernel is thought to hold a registration
    /// it no longer has.
    #[tokio::test]
    async fn a_read_parks_until_the_peer_writes_and_then_delivers() {
        let poller = Poller::new("RDT1", ThreadPriority::CaServerHigh).expect("poller");
        let (server, mut client) = pair();
        let mut stream = poller.stream(server).expect("stream");

        let read = tokio::spawn(async move {
            let mut both = [0u8; 10];
            // Deliberately started before any byte exists: this must park in
            // the poller, not spin or return zero.
            stream.read_exact(&mut both[..5]).await.expect("first");
            stream.read_exact(&mut both[5..]).await.expect("second");
            both
        });

        for payload in [b"hello", b"again"] {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            client.write_all(payload).expect("peer write");
        }

        let got = tokio::time::timeout(std::time::Duration::from_secs(5), read)
            .await
            .expect("read completed")
            .expect("join");
        assert_eq!(&got, b"helloagain");
    }

    #[tokio::test]
    async fn b_write_parks_when_the_send_buffer_fills_and_resumes_when_it_drains() {
        const BYTES: usize = 32 * 1024 * 1024;
        let poller = Poller::new("RDT2", ThreadPriority::CaServerHigh).expect("poller");
        let (server, mut client) = pair();
        let mut stream = poller.stream(server).expect("stream");

        // Past any auto-tuned SO_SNDBUF plus the peer's receive buffer, so the
        // write cannot finish while nobody reads.
        let payload = vec![0x5au8; BYTES];
        let write = tokio::spawn(async move {
            stream.write_all(&payload).await.expect("write_all");
            stream.flush().await.expect("flush");
        });

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !write.is_finished(),
            "the write must park on the poller while the peer reads nothing"
        );

        let drained = tokio::task::spawn_blocking(move || {
            let mut sink = vec![0u8; 64 * 1024];
            let mut total = 0usize;
            while total < BYTES {
                match client.read(&mut sink) {
                    Ok(0) => break,
                    Ok(n) => total += n,
                    Err(e) => panic!("peer read: {e}"),
                }
            }
            total
        });

        tokio::time::timeout(std::time::Duration::from_secs(30), write)
            .await
            .expect("write completed once the peer drained")
            .expect("join");
        assert_eq!(drained.await.expect("join"), BYTES);
    }

    /// The invariant end to end: the drop takes the fd out of the armed set
    /// at once, and the socket closes only once the poll thread has drained
    /// the delete — so the close is the poll thread's, never the dropping
    /// task's.
    #[tokio::test]
    async fn c_dropping_the_stream_disarms_the_fd_and_the_poll_thread_closes_it() {
        let poller = Poller::new("RDT3", ThreadPriority::CaServerHigh).expect("poller");
        let (server, _client) = pair();
        let mut stream = poller.stream(server).expect("stream");
        let fd = stream.socket().as_raw_fd();

        // Park a read so the fd is genuinely armed.
        let mut buf = [0u8; 1];
        let armed =
            tokio::time::timeout(std::time::Duration::from_millis(100), stream.read(&mut buf))
                .await;
        assert!(armed.is_err(), "read must park with no byte available");
        assert_eq!(poller.armed_fds(), 1);

        drop(stream);
        assert_eq!(
            poller.armed_fds(),
            0,
            "StreamInner::drop must clear the armed set at once"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while is_open(fd) {
            assert!(
                std::time::Instant::now() < deadline,
                "the poll thread must close the socket once its delete is out"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn d_an_fd_past_capacity_is_refused_rather_than_dropped() {
        let poller = Poller::new("RDT4", ThreadPriority::CaServerHigh).expect("poller");
        let (server, _client) = pair();
        // The real guard is in `Poller::stream`; drive it by asking about a
        // number no process will hand out, which is the same branch.
        let too_high = FD_CAPACITY as RawFd + 1;
        assert!(too_high as usize >= FD_CAPACITY);
        drop(server);
        let err = poller
            .inner
            .backend
            .wait(
                &[Change {
                    fd: too_high,
                    interest: Interest::Read,
                    action: Action::Arm,
                }],
                &mut Vec::new(),
                &mut Vec::new(),
            )
            .expect_err("a fd past capacity must be an error, not a silent skip");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn f_the_two_halves_share_one_registration_until_the_last_drops() {
        let poller = Poller::new("RDT6", ThreadPriority::CaServerHigh).expect("poller");
        let (server, mut client) = pair();
        let (mut read_half, mut write_half) = poller.stream(server).expect("stream").split();

        // Park the read half; the fd is armed for read from here on.
        let mut buf = [0u8; 5];
        let parked = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            read_half.read(&mut buf),
        )
        .await;
        assert!(parked.is_err(), "the read half must park with no byte");
        assert_eq!(poller.armed_fds(), 1);

        // The write half drives the same fd while that read is parked — the
        // two directions are independent interests, not a shared lock.
        write_half.write_all(b"ping").await.expect("write half");
        let mut echo = [0u8; 4];
        client.read_exact(&mut echo).expect("peer read");
        assert_eq!(&echo, b"ping");

        // The registration belongs to neither half alone: only the last one
        // out may take the fd out of the armed set, because only then is the
        // socket about to close.
        drop(write_half);
        assert_eq!(
            poller.armed_fds(),
            1,
            "the surviving half still holds the registration"
        );
        drop(read_half);
        assert_eq!(
            poller.armed_fds(),
            0,
            "the last half dropping disarms the fd before the close"
        );
    }

    #[tokio::test]
    async fn e_shutdown_stops_the_poll_thread_and_further_arming_fails() {
        let poller = Poller::new("RDT5", ThreadPriority::CaServerHigh).expect("poller");
        let (server, _client) = pair();
        let stream = poller.stream(server).expect("stream");
        poller.shutdown();

        let err = stream
            .inner
            .arm(Interest::Read, std::task::Waker::noop())
            .expect_err("arming a shut-down poller must fail");
        assert_eq!(err.kind(), io::ErrorKind::Other);
    }

    /// A poller whose backend cannot wait is dead, and the tasks parked on it
    /// must find out. Waking them is not enough: a woken task re-polls its
    /// socket, sees `WouldBlock` again, and re-arms — and if the poller still
    /// accepts the arm it parks there for good, on a thread that has exited.
    /// The fatal path must shut the poller first, so the re-arm is refused
    /// and the read completes with an error the connection can act on.
    ///
    /// The backend is made to fail through the one input it refuses: a
    /// change naming an fd past capacity, which `Inner::arm` does not check
    /// (`Poller::stream` does) and `Backend::wait` does.
    #[tokio::test]
    async fn l_a_poller_that_cannot_wait_fails_its_parked_reads() {
        let poller = Poller::new("RDT7", ThreadPriority::CaServerHigh).expect("poller");
        let (server, _client) = pair();
        let mut stream = poller.stream(server).expect("stream");

        let read = tokio::spawn(async move {
            let mut buf = [0u8; 1];
            stream.read(&mut buf).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(poller.armed_fds(), 1, "the read is parked on the poller");

        poller
            .inner
            .arm(
                FD_CAPACITY as RawFd + 1,
                Interest::Read,
                std::task::Waker::noop(),
            )
            .expect("the armed set takes it; the backend is what refuses");

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), read)
            .await
            .expect("a read parked on a dead poller must not park forever")
            .expect("join");
        let err = outcome.expect_err("the read fails rather than pretending to wait");
        assert_eq!(err.kind(), io::ErrorKind::Other, "{err}");
    }

    /// The armed set and its changelist, without a poll thread to race the
    /// assertions. `Inner::drain_changes` is the poll thread's first act on
    /// every turn, so calling it by hand is calling the same code.
    fn bare_inner() -> Arc<Inner> {
        Arc::new(Inner {
            armed: Mutex::new(Armed::default()),
            shutdown: AtomicBool::new(false),
            backend: ActiveBackend::new("RDTC").expect("backend"),
        })
    }

    fn arm(inner: &Inner, fd: RawFd, interest: Interest) {
        inner
            .arm(fd, interest, std::task::Waker::noop())
            .expect("arm");
    }

    fn change(fd: RawFd, interest: Interest, action: Action) -> Change {
        Change {
            fd,
            interest,
            action,
        }
    }

    /// libevent's first changelist rule, and the one that matters under fd
    /// reuse: the add wins over the pending delete, and it is still an add.
    #[test]
    fn g_an_add_over_a_pending_delete_stays_an_add() {
        let inner = bare_inner();
        let mut out = Vec::new();

        let mut closing = Vec::new();

        arm(&inner, 7, Interest::Read);
        inner.drain_changes(&mut out, &mut closing);
        assert_eq!(out, [change(7, Interest::Read, Action::Arm)]);

        // Delivery queues the one-shot delete; the task re-arms before the
        // poll thread gets back round to draining it.
        assert!(inner.take_ready(7, Interest::Read).is_some());
        arm(&inner, 7, Interest::Read);
        inner.drain_changes(&mut out, &mut closing);
        assert_eq!(
            out,
            [change(7, Interest::Read, Action::Arm)],
            "the re-arm replaces the delete, and does not cancel itself with it"
        );
    }

    /// libevent's second rule: a delete of something the kernel was never told
    /// about is dropped rather than submitted — and with nothing to remove
    /// first, the socket closes on the spot.
    #[test]
    fn h_a_delete_of_an_unregistered_interest_is_cancelled() {
        let inner = bare_inner();
        let mut out = Vec::new();
        let mut closing = Vec::new();
        let (server, _client) = pair();
        let fd = server.as_raw_fd();

        arm(&inner, fd, Interest::Read);
        // The connection died before the poll thread ever drained the add.
        inner.disarm_all(server);
        assert!(
            !is_open(fd),
            "nothing in the kernel to remove: closed at once"
        );
        inner.drain_changes(&mut out, &mut closing);
        assert!(out.is_empty(), "nothing to undo in the kernel: {out:?}");
        assert!(
            closing.is_empty(),
            "nothing left for the poll thread to close"
        );
        assert_eq!(inner.armed.lock().expect("armed").slots.len(), 0);
    }

    /// The other side of that rule, and the one-shot invariant's kernel half:
    /// what the kernel does hold is deleted, exactly once — and the socket
    /// stays open until that delete is out, riding in the same drain.
    #[test]
    fn i_a_delete_of_a_registered_interest_is_emitted_once() {
        let inner = bare_inner();
        let mut out = Vec::new();
        let mut closing = Vec::new();
        let (server, _client) = pair();
        let fd = server.as_raw_fd();

        arm(&inner, fd, Interest::Write);
        inner.drain_changes(&mut out, &mut closing);
        inner.disarm_all(server);
        assert!(is_open(fd), "registered: the close waits for the delete");
        inner.drain_changes(&mut out, &mut closing);
        assert_eq!(out, [change(fd, Interest::Write, Action::Disarm)]);
        assert_eq!(closing.len(), 1, "the socket rides with its delete");
        assert!(is_open(fd), "and is still open until the backend takes it");
        // What a backend does once the delete is applied.
        closing.clear();
        assert!(!is_open(fd));

        inner.drain_changes(&mut out, &mut closing);
        assert!(
            out.is_empty(),
            "a drained change is not re-emitted: {out:?}"
        );
        assert_eq!(inner.armed.lock().expect("armed").slots.len(), 0);
    }

    /// Boundary: the poller is already shut down when the socket comes back.
    /// No poll thread will ever drain the delete, so the close is immediate,
    /// and the slot does not keep a socket nobody will take.
    #[test]
    fn o_a_socket_given_back_to_a_shut_down_poller_closes_at_once() {
        let inner = bare_inner();
        let mut out = Vec::new();
        let mut closing = Vec::new();
        let (server, _client) = pair();
        let fd = server.as_raw_fd();

        arm(&inner, fd, Interest::Read);
        inner.drain_changes(&mut out, &mut closing);
        inner.retire();
        inner.disarm_all(server);
        assert!(!is_open(fd), "no thread left to close it later");
        assert_eq!(inner.armed.lock().expect("armed").slots.len(), 0);
    }

    /// The poll thread's exit sweep: a parked task is woken, its next arm is
    /// refused, and a socket held for a delete that will never go out is
    /// closed — on both exits, since both end in `retire`.
    #[test]
    fn p_retire_wakes_the_parked_and_closes_what_it_holds() {
        struct Flag(AtomicBool);
        impl std::task::Wake for Flag {
            fn wake(self: Arc<Self>) {
                self.0.store(true, Ordering::Release);
            }
        }
        let inner = bare_inner();
        let mut out = Vec::new();
        let mut closing = Vec::new();
        let (parked, _c1) = pair();
        let parked_fd = parked.as_raw_fd();
        let (dropped, _c2) = pair();
        let dropped_fd = dropped.as_raw_fd();

        let woken = Arc::new(Flag(AtomicBool::new(false)));
        let waker = Waker::from(Arc::clone(&woken));
        inner.arm(parked_fd, Interest::Read, &waker).expect("arm");
        arm(&inner, dropped_fd, Interest::Read);
        inner.drain_changes(&mut out, &mut closing);
        inner.disarm_all(dropped);
        assert!(
            is_open(dropped_fd),
            "held for a delete the thread now never sends"
        );

        inner.retire();
        assert!(woken.0.load(Ordering::Acquire), "the parked task is woken");
        assert!(
            !is_open(dropped_fd),
            "the held socket is closed by the sweep"
        );
        assert!(is_open(parked_fd), "a socket still owned by a task is not");
        assert!(
            inner.arm(parked_fd, Interest::Read, &waker).is_err(),
            "the woken task's re-arm is refused, which is its error"
        );
        inner.disarm_all(parked);
        assert!(!is_open(parked_fd), "and its own drop closes at once");
    }

    /// The whole point of the changelist: holding connections costs nothing
    /// per wait once they are registered. This is the assertion the old
    /// whole-armed-set design could not have made.
    #[test]
    fn j_a_quiet_armed_set_submits_no_changes() {
        let inner = bare_inner();
        let mut out = Vec::new();

        let mut closing = Vec::new();

        for fd in 3..11 {
            arm(&inner, fd, Interest::Read);
        }
        inner.drain_changes(&mut out, &mut closing);
        assert_eq!(out.len(), 8, "eight arms, eight changes: {out:?}");

        inner.drain_changes(&mut out, &mut closing);
        assert!(
            out.is_empty(),
            "eight registrations already placed cost nothing to keep: {out:?}"
        );
        assert_eq!(inner.armed_fds(), 8);
    }

    /// Both directions of one fd are one entry in the dirty list and two
    /// changes out of it — the boundary the per-fd change record exists to
    /// get right.
    #[test]
    fn k_both_directions_of_one_fd_drain_together() {
        let inner = bare_inner();
        let mut out = Vec::new();

        let mut closing = Vec::new();
        let (server, _client) = pair();
        let fd = server.as_raw_fd();

        arm(&inner, fd, Interest::Read);
        arm(&inner, fd, Interest::Write);
        assert_eq!(inner.armed.lock().expect("armed").dirty, [fd]);
        inner.drain_changes(&mut out, &mut closing);
        out.sort_by_key(|c| c.interest == Interest::Write);
        assert_eq!(
            out,
            [
                change(fd, Interest::Read, Action::Arm),
                change(fd, Interest::Write, Action::Arm)
            ]
        );

        inner.disarm_all(server);
        inner.drain_changes(&mut out, &mut closing);
        out.sort_by_key(|c| c.interest == Interest::Write);
        assert_eq!(
            out,
            [
                change(fd, Interest::Read, Action::Disarm),
                change(fd, Interest::Write, Action::Disarm)
            ]
        );
        assert_eq!(closing.len(), 1, "one socket behind the two deletes");
    }
}
