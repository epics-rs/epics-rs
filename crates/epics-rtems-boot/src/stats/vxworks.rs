//! The VxWorks 7 backend, for an IOC running as an RTP.
//!
//! Unlike [`super`]'s RTEMS backend this compiles no C of its own and needs no
//! build-script cfg to say whether its symbols exist: everything below goes
//! through the `libc` crate's `vxworks` module, and an RTP links libc
//! unconditionally. `target_os = "vxworks"` alone is therefore the whole
//! selection — there is no `vxworks_boot_linked` counterpart to
//! `rtems_boot_linked`, and inventing one for symmetry would add a
//! configuration axis no build can be in.
//!
//! # What an RTP can and cannot see
//!
//! devIocStats' own vxWorks OSD reads `iosFdEntry` and the system memory
//! partition, both of which are *kernel* state. An RTP has neither: it gets its
//! own descriptor space and its own heap. That is not a gap to work around — an
//! IOC *is* the RTP, so the RTP-local numbers are the ones that predict when
//! this IOC stops being able to open a socket, which is what the readings are
//! for. The deviation is in whose table is counted, not in what the number
//! means.
//!
//! There is also no C IOC to compare against: C base 7.0.10.1 supports VxWorks
//! 6.6–6.9 only (`CONFIG.Common.vxWorksCommon:72-76` expands empty for 7, and
//! no `configure/os/*vxWorks*` file carries an x86_64 arch), while rustc
//! supports 7 only. So "parity" here means parity with the RTEMS backend beside
//! it — same classification rules, same output format, same scraper.
//!
//! # Three readings are not bound yet
//!
//! [`mem_usage`], [`dump_tasks`] and [`stack_report`] need symbols the `libc`
//! crate does not declare, so binding them means hand-written `extern "C"`
//! declarations — and on this target a declaration that is never called links
//! clean whether or not the symbol exists, so "it built" is not evidence. Each
//! is therefore left explicitly unavailable, saying so on the console rather
//! than going quiet, until the RTP-callable symbol table measured with `nm`
//! against the SDK arrives. See each one for what it is waiting on.

use std::io;

use super::{FdUsage, MemUsage};

/// Descriptors this RTP holds, and the ceiling it holds them against.
///
/// `max` is `rtpIoTableSizeGet(getpid())` — the size of *this* RTP's
/// descriptor table, which is the table the walk below reads and the wall this
/// IOC actually hits. `sysconf(_SC_OPEN_MAX)` is the POSIX limit rather than
/// the table, so it can disagree with the thing being counted; the RTEMS
/// backend reports `rtems_libio_number_iops`, the real table size, and this is
/// its counterpart rather than a nearby number.
///
/// The walk asks `fcntl(fd, F_GETFD)`, which succeeds on exactly the open
/// descriptors — the POSIX spelling of testing `LIBIO_FLAGS_OPEN` over
/// `rtems_libio_iops`, and available to an RTP, which `iosFdEntry` is not.
///
/// `None` when the table size does not read as a positive count: a walk needs
/// a bound, and reporting "unknown" is the answer the funnel's `Option` exists
/// to carry.
pub(super) fn fd_usage() -> Option<FdUsage> {
    let max = io_table_size()?;
    let mut used = 0u32;
    for fd in 0..max {
        // SAFETY: `F_GETFD` takes no variadic argument and returns the
        // descriptor flags, or -1 for a descriptor that is not open. It reads
        // no memory of ours and changes nothing about the descriptor.
        if unsafe { libc::fcntl(fd as libc::c_int, libc::F_GETFD) } != -1 {
            used += 1;
        }
    }
    Some(FdUsage { used, max })
}

/// This RTP's descriptor-table size, or [`None`] if it does not read as a
/// positive count.
///
/// Both `fd_usage` and `fd_census` bound their walk with this, so the count and
/// the listing cannot cover different ranges — the same property the RTEMS
/// backend gets from both reading `rtems_libio_number_iops`.
fn io_table_size() -> Option<u32> {
    // SAFETY: `getpid` takes nothing and `rtpIoTableSizeGet` reads only the id
    // it is given; neither touches memory of ours.
    let size = unsafe { ffi::rtpIoTableSizeGet(libc::getpid()) };
    u32::try_from(size).ok().filter(|s| *s > 0)
}

/// Not bound yet — always [`None`].
///
/// The obvious binding is `memPartInfoGet(memSysPartId, …)`, and it would be
/// **wrong**: VxWorks 7's libc allocator is mimalloc, so an RTP's `malloc` does
/// not come out of the system memory partition at all. Publishing the kernel
/// partition's free bytes as this IOC's heap would be a confident number about
/// the wrong heap — strictly worse than `NaN`, because an operator would
/// believe it.
///
/// What it needs, in order: whether the RTP's mimalloc exposes a statistics
/// entry point (`mi_process_info` or the `mi_stats_*` family) as a *defined*
/// symbol in the SDK's libc, and if so which of its counters correspond to
/// `Free.total` / `Used.total` / `Free.largest`. The third is the one worth
/// checking hardest: `largest_free` is the fragmentation signal an allocation
/// actually fails on, and a mimalloc heap's answer to it is not simply the
/// largest free run.
pub(super) fn mem_usage() -> MemUsage {
    MemUsage::default()
}

/// Not bound yet — prints one line saying so.
///
/// Silence is what the no-backend fallback does, and it is the wrong answer
/// here: a VxWorks probe image is a target that *should* have a census, so a
/// missing `TASKDUMP` block reads to a scraper exactly like a census that ran
/// and found nothing. Saying "unavailable, and here is why" is the console form
/// of the `None`-is-not-zero rule the value readers follow.
///
/// What it needs: `taskIdListGet` and `taskPriorityGet` confirmed as defined,
/// RTP-callable symbols. `taskNameGet` the `libc` crate already declares, so
/// the names are covered; it is the enumeration and the priority read that are
/// not. Both are documented as kernel-mode routines, so this may end up
/// enumerating through the RTP's own POSIX threads instead — which is a design
/// question the symbol table settles, not one to guess.
pub(super) fn dump_tasks(tag: &str) {
    println!("TASKDUMP unavailable tag={tag} reason=taskIdListGet/taskPriorityGet not bound");
}

/// Not bound yet — prints one line saying so. Same reasoning as [`dump_tasks`].
///
/// What it needs: `taskInfoGet` plus the `TASK_DESC` layout, whose stack
/// high-water fields are the report. This one carries an ABI risk the others do
/// not — a function signature that is wrong fails to link, but a struct layout
/// that is wrong links clean and reports garbage — so `TASK_DESC` wants field
/// offsets from the SDK header, not just a symbol name.
pub(super) fn stack_report(tag: &str) {
    println!("STACKUSE unavailable tag={tag} reason=taskInfoGet/TASK_DESC not bound");
}

/// Name every open descriptor, in the RTEMS backend's format.
///
/// [`fd_usage`] answers how many; this answers which, and the two walk the same
/// descriptors under the same test so they cannot disagree. Every line is the
/// one `csrc/rtems_stats.c` prints, field for field, so one scraper reads both
/// targets — a census whose format drifted per OS would make the two
/// measurements uncomparable, which is the whole point of running it on both.
///
/// The classification order is the RTEMS backend's, and the reason is a
/// measurement rather than a preference: on that target the console descriptors
/// answer `getsockopt(SO_TYPE)` with 0 and leave a value behind that reads as a
/// socket type, so a classification starting from `getsockopt` calls fd 0 a UDP
/// socket. `fstat` first, on every descriptor, is the discriminator that does
/// not lie; the raw `SO_TYPE` is printed beside it rather than trusted.
pub(super) fn fd_census(tag: &str) {
    let max = io_table_size().unwrap_or(0);

    println!("FDCENSUS begin tag={tag}");
    let mut open_count = 0u32;
    for fd in 0..max {
        let fd = fd as libc::c_int;
        // SAFETY: as in `fd_usage` — `F_GETFD` is a read of the descriptor
        // flags and touches no memory of ours.
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
            continue;
        }
        open_count += 1;

        // SAFETY: `fstat` writes only into the local it is given, which is a
        // zeroed `stat` of libc's own layout.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut st) } != 0 {
            println!(
                "FDCENSUS tag={tag} fd={fd} kind=unknown fstat_errno={}",
                errno()
            );
            continue;
        }
        let fmt = (st.st_mode as u32) & (libc::S_IFMT as u32);
        let is_sock = fmt == libc::S_IFSOCK as u32;

        let mut so_type: libc::c_int = 0;
        let mut len = size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: the option buffer and its length are a live local and its
        // real size; the call writes at most `len` bytes into it.
        let typed = is_sock
            && unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_TYPE,
                    (&raw mut so_type).cast(),
                    &mut len,
                )
            } == 0;
        if !typed {
            println!(
                "FDCENSUS tag={tag} fd={fd} kind={} mode=0{:o} rdev={}",
                match fmt {
                    f if f == libc::S_IFCHR as u32 => "chardev",
                    f if f == libc::S_IFREG as u32 => "file",
                    f if f == libc::S_IFIFO as u32 => "fifo",
                    _ if is_sock => "socket-no-type",
                    _ => "other",
                },
                st.st_mode as u32,
                st.st_rdev as i64,
            );
            continue;
        }

        let mut listening: libc::c_int = 0;
        let mut len = size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: as above.
        if unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ACCEPTCONN,
                (&raw mut listening).cast(),
                &mut len,
            )
        } != 0
        {
            listening = -1;
        }

        let local = sock_name(fd, libc::getsockname).unwrap_or_else(|| "-".to_string());
        let peer =
            sock_name(fd, libc::getpeername).unwrap_or_else(|| format!("none(errno={})", errno()));

        println!(
            "FDCENSUS tag={tag} fd={fd} kind={} so_type={so_type} listening={listening} \
             local={local} peer={peer} mode=0{:o}",
            match so_type {
                libc::SOCK_STREAM => "tcp",
                libc::SOCK_DGRAM => "udp",
                _ => "socket",
            },
            st.st_mode as u32,
        );
    }
    println!("FDCENSUS end tag={tag} open={open_count} max={max}");
}

/// One end of a socket, formatted from the `sockaddr_in` bytes.
///
/// Formatted here rather than through `inet_ntop` for the reason the RTEMS
/// backend gives: the printout must not depend on the length-byte handling that
/// has already bitten a target of this workspace once.
fn sock_name(
    fd: libc::c_int,
    get: unsafe extern "C" fn(
        libc::c_int,
        *mut libc::sockaddr,
        *mut libc::socklen_t,
    ) -> libc::c_int,
) -> Option<String> {
    // SAFETY: `addr` is a zeroed `sockaddr_storage` of libc's own layout and
    // `len` is its real size, so the call writes within it and updates `len`.
    let mut addr: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    if unsafe { get(fd, (&raw mut addr).cast(), &mut len) } != 0 {
        return None;
    }
    if libc::c_int::from(addr.ss_family) != libc::AF_INET {
        return Some(format!("family={}", addr.ss_family));
    }
    // SAFETY: `ss_family` says AF_INET, so the storage holds a `sockaddr_in`,
    // which is smaller than `sockaddr_storage` and identically aligned.
    let sin = unsafe { *(&raw const addr).cast::<libc::sockaddr_in>() };
    let b = sin.sin_addr.s_addr.to_ne_bytes();
    Some(format!(
        "{}.{}.{}.{}:{}",
        b[0],
        b[1],
        b[2],
        b[3],
        u16::from_be(sin.sin_port)
    ))
}

/// The last OS error number, in the C's `errno=%d` shape.
fn errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(-1)
}

/// Declarations the `libc` crate's vxworks module does not carry.
///
/// Every symbol here was measured DEFINED — a real `[T]` text symbol with an
/// address in an SDK RTP library, not a `U` reference. That check is the whole
/// admission requirement on this target: `killpg` linked clean in an earlier
/// bring-up purely because nothing called it, so "the image built" is not
/// evidence that a declared symbol exists.
mod ffi {
    use libc::c_int;

    unsafe extern "C" {
        /// Size of an RTP's file-descriptor table.
        ///
        /// DEFINED in `common/libc.a:ioLib.o` @0x9f0 (nm, defined-only scan
        /// over the SDK sysroot RTP libraries).
        ///
        /// The declared shape is robust to the two ways the SDK header could
        /// differ from it, which is why it is safe to write without the header
        /// in front of us. The parameter is `RTP_ID` if not `pid_t`, and both
        /// are `_Vx_OBJ_HANDLE` — `c_int` — so the argument register is the
        /// same either way. A `size_t` return rather than `int` would be read
        /// here as its low 32 bits, which is exact for any table size an RTP
        /// can have. UNCONFIRMED against the header text itself: no SDK on the
        /// build host.
        pub fn rtpIoTableSizeGet(rtp_id: c_int) -> c_int;
    }
}
