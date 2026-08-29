//! The VxWorks 7 backend, for an IOC running as an RTP.
//!
//! Unlike [`super`]'s RTEMS backend this compiles no C of its own and needs no
//! build-script cfg to say whether its symbols exist: every symbol below is one
//! an RTP resolves from the C library it links unconditionally, whether the
//! `libc` crate declares it (`fcntl`, `fstat`, `getsockopt`, `taskIdSelf`) or
//! [`ffi`] does. `target_os = "vxworks"` alone is therefore the whole selection
//! — there is no `vxworks_boot_linked` counterpart to `rtems_boot_linked`, and
//! inventing one for symmetry would add a configuration axis no build can be
//! in.
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
//! # Hand-written externs, and what admits one
//!
//! Three of the five readings need symbols the `libc` crate does not declare,
//! so they go through the [`ffi`] block below. On this target a declaration
//! that is never called links clean whether or not the symbol exists — the
//! `killpg` bring-up proved that — so "it built" is not evidence a symbol is
//! there. Every entry in [`ffi`] was therefore measured DEFINED with `nm` over
//! the SDK's RTP libraries before being declared, and the one foreign *struct*
//! ([`TaskDesc`], which would link clean while reading the wrong bytes) is
//! pinned to the SDK's layout by `offset_of!` assertions that fail the build.
//!
//! Two readings stay unavailable by measurement rather than by omission:
//! `MemUsage::free` and `MemUsage::largest_free`. See [`mem_usage`].

use std::io;
use std::sync::Mutex;

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
///
/// The `size_t` the header returns is narrowed through `try_from` rather than
/// cast, which is what rejects a failure return: `ERROR` is `-1`, and a `-1`
/// arriving as `size_t` is `SIZE_MAX`, so a cast would bound the walk at four
/// billion descriptors while the conversion refuses it. Zero is refused for the
/// same reason — a walk needs a bound, and "unknown" is the answer the funnel's
/// `Option` exists to carry.
fn io_table_size() -> Option<u32> {
    // SAFETY: `getpid` takes nothing and `rtpIoTableSizeGet` reads only the id
    // it is given; neither touches memory of ours.
    let size = unsafe { ffi::rtpIoTableSizeGet(libc::getpid()) };
    u32::try_from(size).ok().filter(|s| *s > 0)
}

/// Heap usage: `used` only. `free` and `largest_free` do not exist here.
///
/// `used` is mimalloc's `current_commit` — bytes this RTP has committed —
/// which is the counter the allocator itself would answer "how much is out"
/// with. One call, no walk.
///
/// The other two are absent by measurement, not by omission:
///
/// * `memPartInfoGet(memSysPartId, …)` is the shape devIocStats' vxWorks OSD
///   uses and is **rejected twice over**. VxWorks 7's libc allocator is
///   mimalloc, so an RTP's `malloc` does not come out of the system memory
///   partition at all — and `memSysPartId` is in any case ABSENT from every
///   RTP library, so the partition cannot even be named. Publishing the kernel
///   partition's free bytes as this IOC's heap would be a confident number
///   about the wrong heap, which is worse than `NaN` because an operator would
///   believe it.
/// * `free` has no scalar counter. It is derivable only by walking
///   `mi_heap_visit_blocks` over the default heap and subtracting in-use
///   blocks from committed areas — approximate, default-heap-only, and a
///   pseudo-number by the same rule that rejects the partition read. The walk
///   is rejected; `mi_stats_get` is DEFINED but has no public struct in the
///   SDK header, so it is not callable on stable terms either.
/// * `largest_free` has no source at all. mimalloc exposes no free-run metric
///   and its block visitor reports allocated blocks only, never free runs. The
///   RTEMS backend's `Free.largest` — the fragmentation signal an allocation
///   actually fails on — has no VxWorks analogue, and `MEM_BLK` on this target
///   is `NaN` for that reason rather than because nobody wired it.
pub(super) fn mem_usage() -> MemUsage {
    let mut elapsed_msecs = 0usize;
    let mut user_msecs = 0usize;
    let mut system_msecs = 0usize;
    let mut current_rss = 0usize;
    let mut peak_rss = 0usize;
    let mut current_commit = 0usize;
    let mut peak_commit = 0usize;
    let mut page_faults = 0usize;
    // SAFETY: every argument is a pointer to a live local of the declared
    // type, which is the whole contract — `mi_process_info` writes each
    // out-param and reads nothing of ours. It has no failure return.
    unsafe {
        ffi::mi_process_info(
            &mut elapsed_msecs,
            &mut user_msecs,
            &mut system_msecs,
            &mut current_rss,
            &mut peak_rss,
            &mut current_commit,
            &mut peak_commit,
            &mut page_faults,
        );
    }
    MemUsage {
        free: None,
        used: Some(current_commit as u64),
        largest_free: None,
    }
}

/// Note this thread in the census registry. Called once per IOC thread.
///
/// The RTEMS backend needs no such call — `rtems_task_iterate` walks the
/// kernel's own thread table. VxWorks has no RTP-side equivalent: measured,
/// `taskIdListGet` and `taskEach` are ABSENT from every RTP library, kernel-mode
/// only. An RTP can ask about a task it can name, and cannot ask what tasks
/// exist, so the list has to be built as the threads are created.
///
/// `taskIdSelf()` rather than `pthread_self()`: the downstream consumer
/// `taskInfoGet` speaks `TASK_ID`, so capturing the VxWorks-native id at
/// registration avoids needing a `pthread_t` → `TASK_ID` conversion that has no
/// RTP-callable spelling either.
pub(super) fn register_task() {
    // SAFETY: `taskIdSelf` takes nothing and reads no memory of ours.
    let id = unsafe { libc::taskIdSelf() };
    registry().insert(id);
}

/// The task census — the `rt top` half.
///
/// # How far the format parity goes
///
/// The block framing is the C's: `TASKDUMP begin tag=… count=…`, one line per
/// task, `TASKDUMP end tag=…`. The per-task *fields* are not, and cannot be —
/// `rtems_stats.c:200` prints `core`, `posix`, `sc` and `obj`, which are the
/// RTEMS scheduler's own facts and have no VxWorks counterpart, while priority
/// and stack come from one `TASK_DESC` here. So a scraper reads both blocks the
/// same way (split on the framing, parse `key=value`) and gets a different set
/// of keys from each, which is the honest outcome when the two kernels expose
/// different things.
///
/// The per-task line carries `tag=` where `rtems_stats.c:200` leaves it to the
/// framing. Deliberate, and the same C's `FDCENSUS` lines already do it: two
/// censuses interleaving on one console cost nothing to tell apart if every
/// line is self-identifying, and a repeated key costs a scraper nothing.
pub(super) fn dump_tasks(tag: &str) {
    let ids = registry().snapshot();
    census_header("TASKDUMP", tag, ids.len());
    for id in &ids {
        match task_info(*id) {
            Some(d) => println!(
                "TASKDUMP tag={tag} id=0x{:08x} prio={} name={} stack_high={} stack_margin={}",
                *id as u32,
                d.td_priority,
                name_of(&d),
                d.td_stack_high,
                d.td_stack_margin,
            ),
            // A registered id whose task has exited. Reported rather than
            // skipped: "a thread that was here at boot and is gone now" is a
            // finding, and a silent skip would present it as never having run.
            None => println!("TASKDUMP tag={tag} id=0x{:08x} state=gone", *id as u32),
        }
    }
    println!("TASKDUMP end tag={tag}");
}

/// Stack high-water per task — the `rt stackuse` half.
///
/// RTEMS gets this from `rtems_stack_checker_report_usage`, the shell command's
/// own implementation. VxWorks has no such whole-system reporter callable from
/// an RTP, so it is assembled per task from the same `TASK_DESC` the census
/// above reads — one call per registered task, over the same registry, so the
/// two blocks cannot describe different sets of threads.
///
/// The body therefore diverges further from RTEMS' than [`dump_tasks`] does:
/// there, `STACKUSE begin`/`end` bracket the RTEMS reporter's own table, which
/// this backend has nothing to reproduce. The framing is the same and the
/// measurement is the same — `td_stackHigh` is the high-water mark the RTEMS
/// checker reports — but the rows are `key=value` rather than that table.
pub(super) fn stack_report(tag: &str) {
    let ids = registry().snapshot();
    census_header("STACKUSE", tag, ids.len());
    for id in &ids {
        match task_info(*id) {
            Some(d) => println!(
                "STACKUSE tag={tag} id=0x{:08x} name={} size={} current={} high={} margin={}",
                *id as u32,
                name_of(&d),
                d.td_stack_size,
                d.td_stack_current,
                d.td_stack_high,
                d.td_stack_margin,
            ),
            None => println!("STACKUSE tag={tag} id=0x{:08x} state=gone", *id as u32),
        }
    }
    println!("STACKUSE end tag={tag}");
}

/// The two header lines every registry-backed census block opens with.
///
/// The scope line is not decoration and is deliberately not a source comment:
/// this census lists what registered itself, and a reader who takes it for the
/// RTP's thread table will under-count and not know it. The RTEMS block carries
/// no such line because `rtems_task_iterate` really does see everything.
///
/// `capacity` and `dropped` are constants here rather than readings, and are
/// printed anyway. They were a real cap and a real refusal count until the
/// registry was made growable; the keys stay so this block and the RTEMS one —
/// which still truncates, and must still be able to say so — parse the same
/// way. `capacity=unbounded` is the claim a reader needs: a `count` from this
/// census is the whole registry, never a prefix of it.
fn census_header(kind: &str, tag: &str, count: usize) {
    println!(
        "{kind} begin tag={tag} count={count} capacity=unbounded dropped=0 \
         source=registry"
    );
    println!(
        "{kind} scope tag={tag} lists only threads that called \
         runtime::task::enter_ioc_thread, plus main; VxWorks has no RTP task \
         enumerator (taskIdListGet is kernel-only), so a std::thread spawned \
         outside the runtime seam is invisible here"
    );
}

/// `td_name` as a `str`, stopping at the first NUL.
///
/// Bounded by the field rather than by a trusted terminator: the array length
/// is derived from `sizeof(TASK_DESC) - offsetof(td_name)`, so if the SDK ever
/// puts another field after the name this reads at most to the struct's end and
/// never past it.
fn name_of(d: &TaskDesc) -> String {
    let bytes: Vec<u8> = d
        .td_name
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// One task's descriptor, or [`None`] if the id names no live task.
fn task_info(id: TaskId) -> Option<TaskDesc> {
    // SAFETY: `taskInfoGet` writes only into the descriptor it is given, which
    // is a zeroed `TaskDesc` whose layout is pinned to the SDK's by the
    // `offset_of!` assertions below.
    let mut desc: TaskDesc = unsafe { std::mem::zeroed() };
    let rc = unsafe { ffi::taskInfoGet(id, &mut desc) };
    (rc == 0).then_some(desc)
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

/// When to compact the registry — *not* a ceiling on it.
///
/// This was `MAX_TASKS`, a fixed capacity chosen to match the RTEMS shim's
/// `EPICS_RTEMS_DUMP_MAX_TASKS` so the two targets truncated at the same count.
/// A supported configuration exceeds it: 141 concurrent CA clients need
/// `CAS_CLIENT_POOL_CAPACITY × 2` = 282 worker slots on their own, so a
/// saturated IOC censused 192 of 301 tasks and reported `dropped=109`. A
/// capacity a supported configuration exceeds is not a capacity, and the truncation landed
/// exactly where the census is most worth reading.
///
/// So the registry grows instead, and this number keeps only its second job:
/// the point at which exited tasks are swept. The RTEMS shim cannot follow —
/// its array is filled inside a `rtems_task_iterate` visitor, where allocating
/// is not safe — but nothing here runs in that context: `register_task` is
/// called from `enter_ioc_thread` at thread startup, and `snapshot` already
/// allocates a `Vec` on the census path.
const SWEEP_THRESHOLD_MIN: usize = 192;

/// `TASK_ID` is `OBJ_HANDLE` is `int` — measured 4 bytes, and asserted below
/// beside the descriptor offsets.
type TaskId = libc::c_int;

fn registry() -> std::sync::MutexGuard<'static, TaskRegistry> {
    static TASKS: Mutex<TaskRegistry> = Mutex::new(TaskRegistry::new());
    // A panic while holding this lock would have to come from `println!`
    // inside the snapshot, which does not run under it — but a poisoned lock
    // must not silence the census either, so the guard is taken either way.
    TASKS.lock().unwrap_or_else(|e| e.into_inner())
}

struct TaskRegistry {
    ids: Vec<TaskId>,
    /// Sweep exited tasks once `ids` reaches this, then re-arm to twice the
    /// live count. Keeps the sweep — a kernel query per entry — amortised
    /// against the threads that actually exist, instead of running it on every
    /// registration once the registry is busy.
    sweep_at: usize,
}

impl TaskRegistry {
    const fn new() -> Self {
        Self {
            ids: Vec::new(),
            sweep_at: SWEEP_THRESHOLD_MIN,
        }
    }

    /// Record `id`, sweeping away exited tasks when the registry has grown
    /// past its sweep threshold.
    ///
    /// The sweep is why this is not a plain append. `TASK_ID`s accumulate: a
    /// thread that exits leaves its id behind, so over a long run the registry
    /// would fill with the dead — which, when the list was fixed, meant it
    /// stopped recording the live. It no longer costs correctness, only
    /// memory, so the sweep is now purely about not growing without bound; it
    /// is a read-only kernel query per entry, at thread startup, amortised by
    /// re-arming `sweep_at` to twice whatever survived.
    fn insert(&mut self, id: TaskId) {
        if self.ids.contains(&id) {
            return;
        }
        if self.ids.len() >= self.sweep_at {
            self.retain_live();
            self.sweep_at = self.ids.len().saturating_mul(2).max(SWEEP_THRESHOLD_MIN);
        }
        self.ids.push(id);
    }

    fn retain_live(&mut self) {
        self.ids.retain(|id| task_info(*id).is_some());
    }

    /// The ids to report.
    ///
    /// Copied out so the console printing below runs with the lock released: a
    /// census that held the registry across its own `println!`s would block
    /// every thread trying to start while the probe wrote to a serial console.
    fn snapshot(&self) -> Vec<TaskId> {
        self.ids.clone()
    }
}

/// VxWorks' `TASK_DESC`, pinned to the SDK's layout by the assertions below.
///
/// This is the one declaration in the backend where being wrong would not fail
/// to link. A mis-declared *function* is a link error; a mis-declared *struct*
/// links clean and reports garbage — a plausible stack high-water read out of
/// the wrong eight bytes. So the offsets are asserted rather than commented,
/// and the assertions are mandatory: an SDK whose layout drifts must stop the
/// build.
///
/// Only the five fields the census prints are named. The padding between them
/// carries no meaning and is named for the field it precedes; the whole run
/// 0..68 is padding because `td_id` and `td_rtpId` live there and neither is
/// read — the registry already holds the id, and an RTP's tasks are all its
/// own. Offsets measured with `wr-cc` against `taskLibCommon.h` (via
/// `<taskLib.h>`), `x86_64-wrs-vxworks`, LP64; the SDK spells these fields
/// `td_stackSize`, `td_stackCurrent`, `td_stackHigh`, `td_stackMargin`.
#[repr(C)]
struct TaskDesc {
    _pad_priority: [u8; 68],
    td_priority: i32,
    _pad_stack: [u8; 8],
    td_stack_size: usize,
    td_stack_current: usize,
    td_stack_high: usize,
    td_stack_margin: isize,
    _pad_name: [u8; 16],
    td_name: [libc::c_char; 80],
}

const _VXWORKS_TASK_DESC_LAYOUT: () = {
    use core::mem::{offset_of, size_of};
    assert!(size_of::<TaskId>() == 4, "TASK_ID is int");
    assert!(offset_of!(TaskDesc, td_priority) == 68);
    assert!(offset_of!(TaskDesc, td_stack_size) == 80);
    assert!(offset_of!(TaskDesc, td_stack_current) == 88);
    assert!(offset_of!(TaskDesc, td_stack_high) == 96);
    assert!(offset_of!(TaskDesc, td_stack_margin) == 104);
    assert!(offset_of!(TaskDesc, td_name) == 128);
    assert!(size_of::<TaskDesc>() == 208);
};

/// Declarations the `libc` crate's vxworks module does not carry.
///
/// Every symbol here was measured DEFINED — a real `[T]` text symbol with an
/// address in an SDK RTP library, not a `U` reference. That check is the whole
/// admission requirement on this target: `killpg` linked clean in an earlier
/// bring-up purely because nothing called it, so "the image built" is not
/// evidence that a declared symbol exists.
mod ffi {
    use libc::{c_int, size_t};

    unsafe extern "C" {
        /// mimalloc's process counters. DEFINED in `common/libc.a:stats.o`;
        /// prototype `mimalloc.h:160`, whose eight out-params are all
        /// `size_t*` and none of which is optional in this call.
        ///
        /// Only `current_commit` is read. The rest are named rather than
        /// passed as null because the prototype takes eight pointers and this
        /// declaration mirrors it exactly — guessing which ones tolerate null
        /// is a second assumption for no gain.
        #[allow(clippy::too_many_arguments)]
        pub fn mi_process_info(
            elapsed_msecs: *mut size_t,
            user_msecs: *mut size_t,
            system_msecs: *mut size_t,
            current_rss: *mut size_t,
            peak_rss: *mut size_t,
            current_commit: *mut size_t,
            peak_commit: *mut size_t,
            page_faults: *mut size_t,
        );

        /// Size of an RTP's file-descriptor table.
        ///
        /// DEFINED in `common/libc.a:ioLib.o` @0x9f0 (nm, defined-only scan
        /// over the SDK sysroot RTP libraries), and declared
        /// `extern size_t rtpIoTableSizeGet(RTP_ID)` at `ioLib.h:533` — the
        /// header this mirrors, read on the box rather than inferred.
        ///
        /// `RTP_ID` is `c_int` here: in RTP mode it resolves through
        /// `_Vx_OBJ_HANDLE` to `int` (`types/vxWind.h:36,43`). The
        /// struct-pointer spelling of `RTP_ID` that would make this a pointer
        /// argument exists only in the kernel-mode headers, which an RTP does
        /// not compile against.
        pub fn rtpIoTableSizeGet(rtp_id: c_int) -> size_t;

        /// One task's descriptor. DEFINED in `common/libc.a:taskInfo.o`;
        /// prototype `STATUS taskInfoGet(TASK_ID tid, TASK_DESC *pTaskDesc)`,
        /// returning `OK` (0) or `ERROR` (-1) — the latter for an id naming no
        /// live task, which is how [`super::task_info`] tells a task that has
        /// exited from one it can describe.
        ///
        /// `TASK_ID` is `_Vx_OBJ_HANDLE`, `c_int`, asserted beside the
        /// descriptor offsets.
        pub fn taskInfoGet(tid: c_int, desc: *mut super::TaskDesc) -> c_int;
    }
}
