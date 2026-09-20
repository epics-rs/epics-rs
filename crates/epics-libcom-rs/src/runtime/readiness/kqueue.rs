//! `kqueue(2)` backend — RTEMS only, and only where the BSP's libbsd carries a
//! usable one.
//!
//! RTEMS has a real kqueue: both prefixes built by `scripts/rtems-bsp.sh`
//! export `kqueue` and `kevent` from `libbsd.a` (`nm --defined-only`: `T
//! kqueue`, `T kevent` on the 6 and the 7 prefix alike), and the installed
//! `sys/event.h:366` declares `int kqueue(void)`. What it does not have is a
//! `libc` binding — `libc`'s newlib tree has zero hits for `kqueue`, `kevent`
//! or `EVFILT_` — so the ABI is declared here against that header rather than
//! imported. Field for field, from the prefix's own
//! `sys/event.h`:
//!
//! ```c
//! struct kevent {
//!     __uintptr_t     ident;
//!     short           filter;
//!     unsigned short  flags;
//!     unsigned int    fflags;
//!     __int64_t       data;
//!     void           *udata;
//!     __uint64_t      ext[4];
//! };
//! ```
//!
//! [`KEvent`] mirrors that declaration exactly and lets `repr(C)` place the
//! padding, which is what keeps it right on a 32-bit `armv7-rtems-eabihf`
//! (where the `__int64_t` forces an alignment hole the struct does not name)
//! without a hand-counted filler field to get wrong.
//!
//! # `EV_ADD` and `EV_DELETE`, never `EV_CLEAR`
//!
//! The module invariant one level up is that a socket interest is one-shot and
//! *level*-triggered: the task arms only after it has seen `WouldBlock`, so a
//! byte that arrives in between must still be reported on the next wait.
//! `EV_CLEAR` is the edge-triggered flag and would lose that arrival, so it
//! appears in this file only in this sentence and on the wake event below.
//!
//! One-shot is enforced here the way libevent enforces it, with an explicit
//! `EV_DELETE` on delivery rather than with `EV_ONESHOT`. The two are the same
//! kernel behaviour and a different cost: `EV_ONESHOT` makes the registration
//! die with the event it delivers, so every wait has to resubmit the whole
//! armed set to put back the ones that did *not* fire, while an explicit
//! delete leaves those alone and charges only the fd that moved. That is the
//! whole reason this backend takes a changelist — see the module doc one level
//! up for the two coalescing rules that come with it.
//!
//! The one exception is the wake event, which is not a socket: `EVFILT_USER`
//! is registered once with `EV_CLEAR` because that is how a user event resets
//! itself after delivery. It carries no readiness, only "look at the armed set
//! again".
//!
//! # What it is worth, measured
//!
//! Nothing on an idle IOC, and six-fold at 112 connections. Thirty sequential
//! PVA get round-trips against `realtime-pva-ioc` on `armv7-rtems-eabihf`
//! under qemu, with N connections already held open, median of seven reps.
//! All four images come from one tree and all four columns from one sitting,
//! because the run-to-run spread on this box is wider than several of the
//! differences below:
//!
//! | held | `select` before | after  | `kqueue` before | after  |
//! |-----:|----------------:|-------:|----------------:|-------:|
//! |    0 |          1.19 s | 1.01 s |          0.98 s | 1.00 s |
//! |   48 |          1.65 s | 1.46 s |          1.34 s | 1.19 s |
//! |  112 |          7.46 s | 7.39 s |          4.53 s | 1.22 s |
//!
//! "Before" is the `EV_ONESHOT` design this file replaced, where a delivered
//! event took its own registration with it and every wait therefore handed the
//! kernel one `EV_ADD` per armed fd to put back the ones that had *not* fired.
//! That is what the 112-connection column was paying: 112 knote operations to
//! be told which one socket had a byte. With the changelist the curve goes
//! flat — 1.22 s against 1.00 s idle, so 112 held connections now cost 0.22 s
//! across thirty round trips rather than 3.5 s.
//!
//! `select` does not move, and that is the useful half of the table. The same
//! changelist took away this process's rebuild of the two bitmaps and its
//! second walk over the armed set to find which bits came back, and neither
//! was ever the dominant term, because `select` re-registers too — the kernel
//! does it, and the API leaves no way not to. libbsd's `selscan`
//! (`sys/kern/sys_generic.c:1260`) walks every set bit below `nfds` and for
//! each fd looks it up, calls `selfdalloc` to hang a fresh waiter record on
//! that socket's wait queue and `fo_poll` to ask its state, and `seltdclear`
//! unwinds the lot when the call returns. At the bottom row that is 112 waiter
//! registrations per wait — the same shape of cost this backend used to pay in
//! `EV_ADD`s, except on the far side of the syscall where no changelist can
//! reach it. Its spread across the seven reps at 112 is 94 ms on 7.39 s —
//! fixed per-wait work, not contention, which is what makes the remaining 6.1x
//! the backend's and not the rig's.
//!
//! Which is also why `EPICS_RTEMS_KQUEUE` is worth setting on a busy IOC and
//! not worth arguing about on a quiet one.

use std::io;
use std::os::fd::RawFd;
use std::sync::Mutex;

use super::{Action, Backend, Change, Interest, check_fd};

/// `sys/event.h:35,36,45`.
const EVFILT_READ: i16 = -1;
const EVFILT_WRITE: i16 = -2;
const EVFILT_USER: i16 = -11;

/// `sys/event.h:135,136,143,154,172`, read from the `arm-rtems6` and
/// `arm-rtems7` prefixes `scripts/rtems-bsp.sh` builds, which agree.
const EV_ADD: u16 = 0x0001;
const EV_DELETE: u16 = 0x0002;
const EV_CLEAR: u16 = 0x0020;
const EV_ERROR: u16 = 0x4000;
const NOTE_TRIGGER: u32 = 0x0100_0000;

/// The `udata` an add carries and a delete does not, so that an `EV_ERROR` can
/// be told apart by which one failed. libevent's `ADD_UDATA`, kept for its
/// reason: a failed add has a task parked behind it that has to hear about it,
/// a failed delete has nobody.
const ADD_UDATA: usize = 1;

/// The `ident` the wake event is registered under. `EVFILT_USER` idents share
/// no namespace with fds, so any constant does; zero is the one a reader of a
/// `kevent` dump recognises fastest.
const WAKE_IDENT: usize = 0;

/// How many ready events one `kevent` call may return, before the changelist
/// raises it. A PVA server wakes a handful of connections per turn; a full
/// buffer simply means the next wait returns immediately with the rest,
/// because the events it did not deliver were never consumed.
const EVENTS_PER_WAIT: usize = 64;

/// `struct kevent` from the BSP's `sys/event.h`. See the module doc.
#[repr(C)]
#[derive(Clone, Copy)]
struct KEvent {
    ident: usize,
    filter: i16,
    flags: u16,
    fflags: u32,
    data: i64,
    udata: *mut libc::c_void,
    ext: [u64; 4],
}

impl KEvent {
    fn zeroed() -> Self {
        Self {
            ident: 0,
            filter: 0,
            flags: 0,
            fflags: 0,
            data: 0,
            udata: std::ptr::null_mut(),
            ext: [0; 4],
        }
    }

    fn set(ident: usize, filter: i16, flags: u16, fflags: u32) -> Self {
        Self {
            ident,
            filter,
            flags,
            fflags,
            ..Self::zeroed()
        }
    }

    /// One [`Change`] as the kernel takes it.
    fn change(fd: RawFd, interest: Interest, action: Action) -> Self {
        let filter = match interest {
            Interest::Read => EVFILT_READ,
            Interest::Write => EVFILT_WRITE,
        };
        let (flags, udata) = match action {
            Action::Arm => (EV_ADD, ADD_UDATA as *mut libc::c_void),
            Action::Disarm => (EV_DELETE, std::ptr::null_mut()),
        };
        Self {
            udata,
            ..Self::set(fd as usize, filter, flags, 0)
        }
    }
}

// `kevent` is passed across the FFI boundary by pointer and holds no
// reference of ours; `udata` carries [`ADD_UDATA`], a constant this module
// only ever compares against null, and never a pointer into this process.
unsafe impl Send for KEvent {}
unsafe impl Sync for KEvent {}

unsafe extern "C" {
    /// `sys/event.h:366`. Absent from `libc`'s newlib module, hence declared.
    fn kqueue() -> libc::c_int;
    /// `sys/event.h:369`.
    fn kevent(
        kq: libc::c_int,
        changelist: *const KEvent,
        nchanges: libc::c_int,
        eventlist: *mut KEvent,
        nevents: libc::c_int,
        timeout: *const libc::timespec,
    ) -> libc::c_int;
}

/// The two `kevent` arrays, kept across waits so that a steady set of
/// connections allocates nothing.
struct Bufs {
    changes: Vec<KEvent>,
    events: Vec<KEvent>,
}

pub(super) struct KqueueBackend {
    kq: RawFd,
    /// Touched only by the poll thread, which is the only caller of
    /// [`KqueueBackend::wait`]; the lock is what lets that be true behind the
    /// `&self` the trait takes, and it is never contended.
    bufs: Mutex<Bufs>,
}

impl KqueueBackend {
    pub(super) fn new() -> io::Result<Self> {
        // SAFETY: no arguments, no pointers.
        let kq = unsafe { kqueue() };
        if kq < 0 {
            return Err(io::Error::last_os_error());
        }
        let backend = Self {
            kq,
            bufs: Mutex::new(Bufs {
                changes: Vec::new(),
                events: vec![KEvent::zeroed(); EVENTS_PER_WAIT],
            }),
        };
        let wake = KEvent::set(WAKE_IDENT, EVFILT_USER, EV_ADD | EV_CLEAR, 0);
        backend.apply(&[wake])?;
        Ok(backend)
    }

    /// Submit changes with no wait — `nevents == 0`, so the kernel applies the
    /// changelist and returns.
    fn apply(&self, changes: &[KEvent]) -> io::Result<()> {
        // SAFETY: `changes` outlives the call; a null eventlist is valid when
        // `nevents` is zero, and a null timeout with nothing to wait for
        // returns immediately.
        let rc = unsafe {
            kevent(
                self.kq,
                changes.as_ptr(),
                changes.len() as libc::c_int,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for KqueueBackend {
    fn drop(&mut self) {
        // SAFETY: `self.kq` is ours, open, and unreachable after this.
        unsafe { libc::close(self.kq) };
    }
}

impl Backend for KqueueBackend {
    fn wait(&self, changes: &[Change], ready: &mut Vec<(RawFd, Interest)>) -> io::Result<()> {
        // Checked before anything is submitted: a half-applied changelist
        // would leave the caller's armed set and the kqueue disagreeing, and
        // there is no path back from that.
        for change in changes {
            check_fd(change.fd)?;
        }

        let mut bufs = self.bufs.lock().expect("readiness kqueue buffers poisoned");
        let Bufs {
            changes: submit,
            events,
        } = &mut *bufs;
        submit.clear();
        submit.extend(
            changes
                .iter()
                .map(|c| KEvent::change(c.fd, c.interest, c.action)),
        );
        // libevent grows its event list to the change list's length before
        // every dispatch, and this is the same guard for the same reason: the
        // kernel reports a failed change as an `EV_ERROR` entry only if there
        // is room for one, and turns the whole call into -1 if there is not.
        // A batch of connections closing at once is exactly such a burst of
        // failing deletes, because each fd is already closed by the time this
        // runs — see the comment on `EV_ERROR` below.
        if events.len() < submit.len() {
            events.resize(submit.len(), KEvent::zeroed());
        }

        // SAFETY: both buffers outlive the call; a null timeout is the
        // documented "block until an event" argument. Submitting the
        // changelist here rather than in a call of its own is what satisfies
        // the `Backend::wait` MUST: `kevent` applies every change before it
        // begins to wait, so an interrupted wait has still taken them.
        let rc = unsafe {
            kevent(
                self.kq,
                submit.as_ptr(),
                submit.len() as libc::c_int,
                events.as_mut_ptr(),
                events.len() as libc::c_int,
                std::ptr::null(),
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }

        for event in &events[..rc as usize] {
            if event.flags & EV_ERROR != 0 {
                // A delete that missed is the ordinary case, not a defect.
                // `Registration`'s `Drop` queues it and the socket closes
                // immediately after, and closing an fd already takes its
                // registrations out of the kqueue — so by the time this
                // submits, there is usually nothing left to delete and the
                // kernel says ENOENT or EBADF. libevent skips exactly these.
                //
                // A failed *add* is the opposite: a task is parked behind it.
                // Reporting it as readiness is what gets that task its own
                // syscall and its own error, with the connection context to
                // act on it; this thread could only fail the whole poller,
                // which would take every other connection with it.
                if event.udata.is_null() {
                    continue;
                }
            } else if event.filter == EVFILT_USER {
                continue;
            }
            let interest = match event.filter {
                EVFILT_READ => Interest::Read,
                EVFILT_WRITE => Interest::Write,
                // A filter this module never arms. Dropping it is right:
                // waking a task for an event it did not ask for would have it
                // re-arm forever.
                _ => continue,
            };
            ready.push((event.ident as RawFd, interest));
        }
        Ok(())
    }

    fn interrupt(&self) -> io::Result<()> {
        let trigger = KEvent::set(WAKE_IDENT, EVFILT_USER, 0, NOTE_TRIGGER);
        self.apply(&[trigger])
    }
}

/// The one claim about this file that can be checked without a target: that
/// the struct handed to the kernel is the one the BSP header describes. These
/// fail the RTEMS build itself rather than a test run, which is the same
/// mechanism `epics-rtems-boot`'s libc layout asserts use — and the only one
/// available, since no host can run this backend.
const _: () = {
    use std::mem::{align_of, offset_of, size_of};
    assert!(
        align_of::<KEvent>() == 8,
        "the __int64_t sets the alignment"
    );
    assert!(size_of::<KEvent>() == 64);
    assert!(offset_of!(KEvent, ident) == 0);
    assert!(offset_of!(KEvent, filter) == size_of::<usize>());
    assert!(offset_of!(KEvent, flags) == size_of::<usize>() + 2);
    assert!(offset_of!(KEvent, fflags) == size_of::<usize>() + 4);
    // `data` is 8-aligned, so on 32-bit `armv7-rtems-eabihf` it starts at 16
    // behind a 4-byte hole after `fflags`, and on a 64-bit target it starts at
    // 16 with none. Both are what `repr(C)` lays out from the field list.
    assert!(offset_of!(KEvent, data) == 16);
    assert!(offset_of!(KEvent, udata) == 24);
    assert!(offset_of!(KEvent, ext) == 32);
};

/// `EV_CLEAR` is the edge-triggered flag the module doc bans for socket
/// interests. The wake event is the one user of it, and it is registered in
/// `new`, not in `wait`.
const _: () = {
    assert!(EV_ADD & EV_CLEAR == 0);
    assert!(EV_DELETE & EV_CLEAR == 0);
};
