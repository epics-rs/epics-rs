//! `select(2)` backend — the one both embedded triples can take.
//!
//! VxWorks 7 has no kqueue and no epoll: the SDK measured here declares
//! neither (`HAVE_KQUEUE`/`HAVE_SYS_EVENT_H`/`HAVE_EPOLL` are all `#undef` in
//! its bundled `pyconfig.h`, and no header declares `kqueue(`). Its `poll()`
//! is not a second primitive either — `libunix.a(poll.o)` references exactly
//! `bzero`, `errnoSet` and `select`, so poll is a wrapper that rebuilds fd
//! sets on every call. `select` is therefore the native call there, and this
//! backend calls it directly rather than paying for that translation.
//!
//! It is also what RTEMS takes until the kqueue backend lands, and what the
//! host takes in tests — the same code on all three, so a host test exercises
//! the bytes that run on target.
//!
//! # `fd_set` is declared here, not imported
//!
//! `libc` has no `select` and no `fd_set` for `x86_64-wrs-vxworks` at all
//! (`src/vxworks/mod.rs`: zero hits for either), so one of the two targets
//! must bring its own regardless. Declaring it once here — as a bitmap of
//! [`FD_CAPACITY`] bits, which is the layout `select` has taken since 4.2BSD —
//! is what keeps this file free of a per-target `cfg` whose arms could drift.
//! The buffer is deliberately larger than any of the three platforms' own
//! `FD_SETSIZE` (VxWorks sizes it from the VSB's `_WRS_CONFIG_FD_SET_SIZE`,
//! 2048 in the SDK measured here; Linux fixes it at 1024), because `select`
//! only ever touches the words below `nfds`, so a longer buffer is always
//! safe and a shorter one is what corrupts the stack.
//!
//! # The sets are kept, not rebuilt
//!
//! libevent's `select` backend holds `event_readset_in` and
//! `event_writeset_in` across dispatches and copies them into the sets it
//! hands the kernel, because `select` destroys what it is given; this backend
//! does the same with [`Sets`]. What that buys is not the copy — a
//! [`FD_CAPACITY`]-bit bitmap is memory bandwidth either way — but the absence
//! of the per-armed-fd loop that used to rebuild it, and of the second walk
//! over the armed set that used to find which bits came back. Both are now
//! bounded by `nfds`, so a wait costs the top of the fd space and the fds that
//! were actually ready.
//!
//! It buys no time on the target, and the kqueue backend's doc carries the
//! measurement: 7.46 s before and 7.39 s after, at 112 held connections, which
//! is inside one run's own spread. Neither of the two loops it removed was the
//! term that matters here, and that term is a registration rather than a scan:
//! libbsd's `selscan` (`sys/kern/sys_generic.c:1260`) walks every set bit
//! below `nfds` and for each fd calls `selfdalloc`, which hangs a waiter record
//! on that socket's wait queue, then `fo_poll`; `seltdclear` takes them all
//! down again when the call returns. It happens on every call because
//! `select(2)` has nowhere to remember a registration between them, which is
//! the one thing this backend cannot stop paying. The reason to keep the sets
//! anyway is that they are what a
//! changelist *is* on this backend; rebuilding them per wait would mean
//! keeping the whole armed set on the far side of the interface as well, which
//! is the design the changelist exists to retire.
//!
//! # At most one wake byte is ever in flight
//!
//! The self-pipe carries no information beyond "look at the armed set again",
//! so a second byte behind an undelivered first one buys nothing — and a full
//! pipe would block the arming thread, which is a hang in the path that exists
//! to prevent one. [`SelectBackend::interrupt`] therefore writes only on the
//! `false -> true` edge of `wake_pending`, and the poll thread clears that
//! flag after it drains — after, because a clear that preceded the read would
//! let an `interrupt` slip a byte in between for the read to swallow, and the
//! flag would then stay up over an empty pipe with nothing left to lower it.
//! The ordering that makes the late clear lossless is that
//! [`super::Inner::arm`] records its change *before* it calls `interrupt`: an
//! interrupt suppressed by the flag belongs to a change the next drain can
//! see.

use std::io;
use std::net::TcpStream;
use std::os::fd::RawFd;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{Action, Backend, Change, FD_CAPACITY, Interest, check_fd};

const WORD_BITS: usize = usize::BITS as usize;
const WORDS: usize = FD_CAPACITY / WORD_BITS;

/// A `select` descriptor bitmap. See the module doc for why it is declared
/// rather than imported.
#[repr(C)]
#[derive(Clone, Copy)]
struct FdSet {
    words: [usize; WORDS],
}

impl FdSet {
    fn zeroed() -> Self {
        Self { words: [0; WORDS] }
    }

    fn set(&mut self, fd: RawFd) {
        let fd = fd as usize;
        self.words[fd / WORD_BITS] |= 1usize << (fd % WORD_BITS);
    }

    fn clear(&mut self, fd: RawFd) {
        let fd = fd as usize;
        self.words[fd / WORD_BITS] &= !(1usize << (fd % WORD_BITS));
    }

    fn is_set(&self, fd: RawFd) -> bool {
        let fd = fd as usize;
        self.words[fd / WORD_BITS] & (1usize << (fd % WORD_BITS)) != 0
    }
}

/// What this backend is waiting on, between waits. See the module doc.
struct Sets {
    read: FdSet,
    write: FdSet,
    /// One past the highest fd ever registered, which is `select`'s `nfds` and
    /// the bound on every scan of the sets above.
    ///
    /// It grows and does not shrink, as libevent's `event_fdsz` does. A
    /// high-water that outlives the connection that set it costs a few clear
    /// words per wait, and recomputing it would cost a scan on the path that
    /// closes a connection — the one path that is already doing the most work.
    nfds: libc::c_int,
}

/// `struct timeval`. Declared for the same reason as [`FdSet`]; this backend
/// always passes a null pointer (block until something is ready), so the
/// layout is carried for the signature rather than used.
#[repr(C)]
struct TimeVal {
    tv_sec: libc::c_long,
    tv_usec: libc::c_long,
}

unsafe extern "C" {
    /// POSIX `select(2)`. Present on every target this compiles for, but
    /// absent from `libc`'s VxWorks module, so it is named here.
    fn select(
        nfds: libc::c_int,
        readfds: *mut FdSet,
        writefds: *mut FdSet,
        exceptfds: *mut FdSet,
        timeout: *mut TimeVal,
    ) -> libc::c_int;
}

pub(super) struct SelectBackend {
    /// Self-pipe: `[read end, write end]`.
    wake: [RawFd; 2],
    /// Whether a wake byte may be sitting in the pipe. See the module doc.
    wake_pending: AtomicBool,
    /// Touched only by the poll thread, which is the only caller of
    /// [`SelectBackend::wait`]; the lock is what lets that be true behind the
    /// `&self` the trait takes, and it is never contended.
    sets: Mutex<Sets>,
}

impl SelectBackend {
    pub(super) fn new() -> io::Result<Self> {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: `pipe` writes exactly two `int`s through the pointer, which
        // `fds` provides for the duration of the call.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        for fd in fds {
            if (fd as usize) >= FD_CAPACITY {
                // SAFETY: both fds are ours and open.
                unsafe {
                    libc::close(fds[0]);
                    libc::close(fds[1]);
                }
                return Err(io::Error::other(format!(
                    "self-pipe fd {fd} is beyond the readiness poller's {FD_CAPACITY}-fd capacity"
                )));
            }
        }
        // The wake pipe is a registration like any other, so it lives in the
        // persistent set rather than being added on each wait. The result walk
        // skips it by number, because it is the one fd here with no task
        // behind it.
        let mut read = FdSet::zeroed();
        read.set(fds[0]);
        Ok(Self {
            wake: fds,
            wake_pending: AtomicBool::new(false),
            sets: Mutex::new(Sets {
                read,
                write: FdSet::zeroed(),
                nfds: fds[0] + 1,
            }),
        })
    }

    fn drain_wake(&self) {
        let mut sink = [0u8; 64];
        loop {
            // SAFETY: `self.wake[0]` is our open pipe read end; `sink` backs
            // the pointer for the whole call.
            let n = unsafe {
                libc::read(
                    self.wake[0],
                    sink.as_mut_ptr() as *mut libc::c_void,
                    sink.len(),
                )
            };
            // A short read means the pipe is empty; anything else (including
            // EAGAIN on a platform that made the pipe non-blocking for us)
            // ends the drain too.
            if n < sink.len() as isize {
                break;
            }
        }
        // Cleared after the read, never before it. An `interrupt` that lands
        // between a clear and the read would raise the flag and write a byte
        // the read then swallows — the flag left up over an empty pipe, and
        // since this runs only when the pipe is readable, nothing lowers it
        // again. One that lands between the read and this clear is suppressed
        // instead, and that loses nothing: its change was recorded before it
        // was called, and the poll thread drains changes before it next
        // blocks.
        self.wake_pending.store(false, Ordering::Release);
    }
}

impl Drop for SelectBackend {
    fn drop(&mut self) {
        // SAFETY: both fds are ours, open, and unreachable after this.
        unsafe {
            libc::close(self.wake[0]);
            libc::close(self.wake[1]);
        }
    }
}

/// Push every fd whose bit survived `select` into `ready`, a word at a time.
///
/// libevent walks `0..nfds` one fd at a time here. Testing a whole word at
/// once is the same answer for a sixty-fourth of the loop, and it is what
/// makes this cost the number of *ready* fds rather than the number watched.
fn collect(
    set: &FdSet,
    words: usize,
    interest: Interest,
    skip: RawFd,
    ready: &mut Vec<(RawFd, Interest)>,
) {
    for (index, &word) in set.words[..words].iter().enumerate() {
        let mut bits = word;
        while bits != 0 {
            let bit = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let fd = (index * WORD_BITS + bit) as RawFd;
            if fd != skip {
                ready.push((fd, interest));
            }
        }
    }
}

impl Backend for SelectBackend {
    fn wait(
        &self,
        changes: &[Change],
        closing: &mut Vec<TcpStream>,
        ready: &mut Vec<(RawFd, Interest)>,
    ) -> io::Result<()> {
        // Checked before anything is applied: a half-applied changelist would
        // leave the caller's armed set and these sets disagreeing, and there
        // is no path back from that.
        for change in changes {
            check_fd(change.fd)?;
        }

        // The sets `select` is handed are destroyed by it, so it gets a copy;
        // the originals are this backend's record of what it is waiting on.
        // Applying the changes before the copy is what satisfies the
        // `Backend::wait` MUST — nothing here can block, so an interrupted
        // `select` below has already taken them.
        let (mut read_set, mut write_set, nfds) = {
            let mut sets = self.sets.lock().expect("readiness select sets poisoned");
            let Sets { read, write, nfds } = &mut *sets;
            for change in changes {
                let set = match change.interest {
                    Interest::Read => &mut *read,
                    Interest::Write => &mut *write,
                };
                match change.action {
                    Action::Arm => set.set(change.fd),
                    Action::Disarm => set.clear(change.fd),
                }
                *nfds = (*nfds).max(change.fd + 1);
            }
            (*read, *write, *nfds)
        };
        // The sockets given back with this changelist close here, with their
        // bits already clear, so the `select` below never names a closed fd.
        // It would refuse one: `EBADF` at entry on Linux, and on every rescan
        // under the BSD `kern_select` libbsd carries, where a wake-up walks
        // the sets again.
        closing.clear();

        // SAFETY: both sets outlive the call and are `nfds`-bit addressable by
        // construction (every fd was range-checked above); the null timeout is
        // the documented "block indefinitely" argument.
        let rc = unsafe {
            select(
                nfds,
                &mut read_set,
                &mut write_set,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }

        if read_set.is_set(self.wake[0]) {
            self.drain_wake();
        }
        let words = (nfds as usize).div_ceil(WORD_BITS);
        collect(&read_set, words, Interest::Read, self.wake[0], ready);
        collect(&write_set, words, Interest::Write, self.wake[0], ready);
        Ok(())
    }

    fn interrupt(&self) -> io::Result<()> {
        if self.wake_pending.swap(true, Ordering::AcqRel) {
            // A byte is already on its way; a second one would say the same
            // thing and could fill the pipe.
            return Ok(());
        }
        let byte = 1u8;
        // SAFETY: `self.wake[1]` is our open pipe write end; `byte` backs the
        // pointer for the whole call.
        let n = unsafe {
            libc::write(
                self.wake[1],
                std::ptr::from_ref(&byte) as *const libc::c_void,
                1,
            )
        };
        if n != 1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fd_set_addresses_the_whole_capacity() {
        let mut set = FdSet::zeroed();
        for fd in [0, 1, 63, 64, 1023, 1024, (FD_CAPACITY - 1) as RawFd] {
            assert!(!set.is_set(fd), "fd {fd} starts clear");
            set.set(fd);
            assert!(set.is_set(fd), "fd {fd} reads back set");
        }
        assert!(!set.is_set(2), "an untouched fd stays clear");
    }

    #[test]
    fn the_buffer_is_at_least_every_platform_fd_setsize() {
        // Linux 1024, the VxWorks SDK measured here 2048. A shorter buffer is
        // what corrupts the stack, so this is a floor, not a preference.
        const { assert!(FD_CAPACITY >= 2048) };
        const { assert!(WORDS * WORD_BITS == FD_CAPACITY) };
    }

    /// The clear must come after the read, not before it. An `interrupt`
    /// landing between a clear-first and its read raises the flag and writes
    /// a byte the read then swallows, leaving the flag up over an empty pipe;
    /// nothing ever lowers it again, and every later `interrupt` is
    /// suppressed. The observable invariant, once no drain is in progress:
    /// `wake_pending` up means a byte is in the pipe.
    ///
    /// A stress test, since the window is between two instructions on one
    /// thread: 20k interrupts against a drain loop, then the invariant is
    /// checked with the pipe polled at zero timeout.
    #[test]
    fn an_interrupt_that_races_the_drain_is_never_lost() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        let backend = Arc::new(SelectBackend::new().expect("backend"));
        let done = Arc::new(AtomicBool::new(false));
        let hammer = {
            let backend = Arc::clone(&backend);
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                for _ in 0..20_000 {
                    backend.interrupt().expect("interrupt");
                }
                done.store(true, Ordering::Release);
            })
        };
        let readable = |timeout_ms: libc::c_int| {
            let mut pfd = libc::pollfd {
                fd: backend.wake[0],
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: `pfd` backs the pointer for the whole call.
            let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
            rc > 0 && pfd.revents & libc::POLLIN != 0
        };
        while !done.load(Ordering::Acquire) {
            if readable(1) {
                backend.drain_wake();
            }
        }
        hammer.join().expect("join");
        if readable(0) {
            backend.drain_wake();
        }
        assert!(
            !backend.wake_pending.load(Ordering::Acquire),
            "wake_pending is up over an empty pipe: every later interrupt is lost"
        );
    }

    #[test]
    fn interrupt_writes_once_until_the_drain_clears_it() {
        let backend = SelectBackend::new().expect("backend");
        backend.interrupt().expect("first interrupt writes");
        assert!(backend.wake_pending.load(Ordering::Acquire));
        // Second and third are suppressed: one byte is the whole message.
        backend.interrupt().expect("second interrupt is a no-op");
        backend.interrupt().expect("third interrupt is a no-op");
        backend.drain_wake();
        assert!(!backend.wake_pending.load(Ordering::Acquire));
        backend
            .interrupt()
            .expect("interrupt writes again after a drain");
        assert!(backend.wake_pending.load(Ordering::Acquire));
    }

    fn arm(fd: RawFd, interest: Interest) -> Change {
        Change {
            fd,
            interest,
            action: Action::Arm,
        }
    }

    /// The claim the persistent sets rest on: a registration placed by one
    /// wait is still there for the next one, which is handed no changes at
    /// all.
    #[test]
    fn a_registration_outlives_the_wait_that_placed_it() {
        let backend = SelectBackend::new().expect("backend");
        let (mut a, b) = {
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
            let addr = listener.local_addr().expect("local_addr");
            let client = std::net::TcpStream::connect(addr).expect("connect");
            let (server, _) = listener.accept().expect("accept");
            (client, server)
        };
        use std::io::Write;
        use std::os::fd::AsRawFd;
        let fd = b.as_raw_fd();

        a.write_all(b"x").expect("peer write");
        let mut ready = Vec::new();
        backend
            .wait(&[arm(fd, Interest::Read)], &mut Vec::new(), &mut ready)
            .expect("first wait");
        assert_eq!(ready, [(fd, Interest::Read)]);

        // Nothing is read from the socket, so it is still level-ready. An
        // empty changelist must not mean an empty registration set.
        ready.clear();
        backend
            .wait(&[], &mut Vec::new(), &mut ready)
            .expect("second wait");
        assert_eq!(
            ready,
            [(fd, Interest::Read)],
            "the set is kept between waits, not rebuilt from the changelist"
        );

        // And a disarm takes it back out, which is what stops the poll thread
        // spinning on a level-triggered fd whose task is gone.
        ready.clear();
        let disarm = Change {
            fd,
            interest: Interest::Read,
            action: Action::Disarm,
        };
        backend.interrupt().expect("interrupt");
        backend
            .wait(&[disarm], &mut Vec::new(), &mut ready)
            .expect("third wait");
        assert!(ready.is_empty(), "the disarmed fd is gone: {ready:?}");
    }

    #[test]
    fn an_interrupt_breaks_a_blocked_wait() {
        let backend = std::sync::Arc::new(SelectBackend::new().expect("backend"));
        let waker = std::sync::Arc::clone(&backend);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            waker.interrupt().expect("interrupt");
        });
        let mut ready = Vec::new();
        // No armed fd at all: the only thing that can end this wait is the
        // self-pipe.
        backend
            .wait(&[], &mut Vec::new(), &mut ready)
            .expect("wait");
        assert!(ready.is_empty());
        handle.join().expect("join");
    }

    /// A connected pair over loopback: (client, accepted server side).
    fn pair() -> (std::net::TcpStream, std::net::TcpStream) {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let client = std::net::TcpStream::connect(addr).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        (client, server)
    }

    fn is_open(fd: RawFd) -> bool {
        // SAFETY: `F_GETFD` takes no pointer and touches nothing.
        unsafe { libc::fcntl(fd, libc::F_GETFD) != -1 }
    }

    /// The fact the deferred close is built on, pinned: a closed fd left in
    /// the set is `EBADF` from the kernel, not a quiet skip. Linux checks at
    /// entry; libbsd's `kern_select` also on every rescan, which no host test
    /// can reach.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_kernel_refuses_a_closed_fd_left_in_the_set() {
        use std::os::fd::AsRawFd;
        let backend = SelectBackend::new().expect("backend");
        let (_client, server) = pair();
        let fd = server.as_raw_fd();
        let mut ready = Vec::new();
        backend.interrupt().expect("interrupt");
        backend
            .wait(&[arm(fd, Interest::Read)], &mut Vec::new(), &mut ready)
            .expect("registered");

        drop(server);
        backend.interrupt().expect("interrupt");
        let err = backend
            .wait(&[], &mut Vec::new(), &mut ready)
            .expect_err("a closed fd in the set must not be silently skipped");
        assert_eq!(err.raw_os_error(), Some(libc::EBADF));
    }

    /// The order the invariant needs, on this backend: the delete clears the
    /// bit, then the socket closes, then `select` runs — so the call that
    /// follows a dropped connection is an ordinary wait, not the `EBADF`
    /// above.
    #[test]
    fn a_socket_given_back_with_its_delete_closes_after_the_bit_clears() {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        let backend = SelectBackend::new().expect("backend");
        let (mut client, server) = pair();
        let fd = server.as_raw_fd();
        let mut ready = Vec::new();
        client.write_all(b"x").expect("peer write");
        backend
            .wait(&[arm(fd, Interest::Read)], &mut Vec::new(), &mut ready)
            .expect("registered");
        assert_eq!(ready, [(fd, Interest::Read)]);

        // The connection is dropped: its delete and its socket arrive in one
        // changelist, and the byte it never read is still in the socket.
        let disarm = Change {
            fd,
            interest: Interest::Read,
            action: Action::Disarm,
        };
        let mut closing = vec![server];
        ready.clear();
        backend.interrupt().expect("interrupt");
        backend
            .wait(&[disarm], &mut closing, &mut ready)
            .expect("the bit is clear before the fd is closed");
        assert!(closing.is_empty(), "the backend took the close");
        assert!(!is_open(fd), "and performed it before blocking");
        assert!(ready.is_empty(), "nothing else was armed: {ready:?}");
    }
}
