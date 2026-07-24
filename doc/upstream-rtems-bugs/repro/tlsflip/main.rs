#![feature(cfg_target_thread_local)]
//! has_thread_local ADOPTION probe (#2) for armv7-rtems-eabihf.
//!
//! Rig #1 (`../tlsdtor/`) measured spawn/join, a `thread_local!` with `Drop`,
//! and cross-thread isolation, and named three cases it did not exercise.
//! This rig is those three, plus rig #1's plain spawn/join phase kept as a
//! control.
//!
//! Same source, two images per phase:
//!   * stock  target `armv7-rtems-eabihf`   (no target_thread_local)
//!   * native `./armv7-rtems-tls.json`      (stock spec + "has-thread-local": true)
//!
//! ONE PHASE PER IMAGE, selected by the `TLSDTOR_PHASE` build-time env var.
//! This is not tidiness — it is forced by a measured result. On the stock
//! key-based image the leak is contingent on the order in which pthread keys
//! are CREATED: RTEMS's exit sweep pops the root of a per-thread RBTree
//! (`_POSIX_Keys_Run_destructors`), so whether std's `CURRENT` pair is still
//! present when the deferred `CLEANUP(RUN)` destructor reads it depends on
//! that tree's shape. Measured on one unchanged stock image: touching this
//! rig's `SLOT` thread_local first gives 136.16 B/thread; calling
//! `std::thread::current()` first instead gives 0.00. A four-phase boot
//! therefore measures the second phase in a key layout the first phase
//! created, and the slopes are not comparable. One phase per image removes
//! the ordering variable, leaving the spec flag as the only difference
//! between the two images of a pair.
//!
//! Phases:
//!   plain     — spawn/join. Control; must reproduce rig #1's 136 / 0.
//!   unwind    — the thread body PANICS and unwinds out of the closure.
//!   cthread   — the thread is created by raw `pthread_create` (a C-created
//!               thread, the libbsd-callback shape) and its body touches Rust
//!               `thread_local!` and `std::thread::current()`.
//!   constinit — `thread_local!` with a `const { ... }` initialiser, both a
//!               plain `Cell` and a `Drop` type.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

// RTEMS `malloc_info` fills a Heap_Information_block:
// free{number,largest,total} then used{number,largest,total}, all `uintptr_t`.
unsafe extern "C" {
    fn malloc_info(i: *mut usize) -> i32;
}

#[derive(Clone, Copy, Default)]
struct Heap {
    free_number: usize,
    free_total: usize,
    used_number: usize,
    used_total: usize,
}

fn heap() -> Heap {
    let mut i = [0usize; 40];
    let rc = unsafe { malloc_info(i.as_mut_ptr()) };
    assert_eq!(rc, 0, "malloc_info failed");
    Heap {
        free_number: i[0],
        free_total: i[2],
        used_number: i[3],
        used_total: i[5],
    }
}

fn sample(tag: &str, h: Heap) {
    println!(
        "leaksample[{tag}] used.number={} used.total={} free.number={} free.total={}",
        h.used_number, h.used_total, h.free_number, h.free_total
    );
}

// ---------------------------------------------------------------------------
// TLS payloads

static DTOR_RUNS: AtomicUsize = AtomicUsize::new(0);
static DTOR_LAST_ID: AtomicU64 = AtomicU64::new(0);
static ISO_OK: AtomicUsize = AtomicUsize::new(0);
static ISO_BAD: AtomicUsize = AtomicUsize::new(0);

/// A TLS payload with a real destructor. The destructor reads its own field,
/// so it also proves the value is intact when the destructor runs.
struct Marker {
    id: Cell<u64>,
}

impl Drop for Marker {
    fn drop(&mut self) {
        DTOR_LAST_ID.store(self.id.get(), Ordering::SeqCst);
        DTOR_RUNS.fetch_add(1, Ordering::SeqCst);
    }
}

thread_local! {
    static SLOT: Marker = Marker { id: Cell::new(0) };
}

// const-init, no destructor: the codegen shape `const { }` exists for.
thread_local! {
    static CI_PLAIN: Cell<u64> = const { Cell::new(0) };
}

static CI_DTOR_RUNS: AtomicUsize = AtomicUsize::new(0);
static CI_DTOR_LAST_ID: AtomicU64 = AtomicU64::new(0);

struct ConstMarker {
    id: Cell<u64>,
}

impl Drop for ConstMarker {
    fn drop(&mut self) {
        CI_DTOR_LAST_ID.store(self.id.get(), Ordering::SeqCst);
        CI_DTOR_RUNS.fetch_add(1, Ordering::SeqCst);
    }
}

// const-init WITH a destructor: the combination that decides whether
// `const { }` changes the destructor-registration path.
thread_local! {
    static CI_SLOT: ConstMarker = const { ConstMarker { id: Cell::new(0) } };
}

const MAIN_SENTINEL: u64 = 0xdead_beef;

// ---------------------------------------------------------------------------
// plain — the control phase, identical in shape to rig #1's unnamed phase.

fn plain_cycle(n: u64) {
    let h = std::thread::Builder::new()
        .spawn(move || {
            let start = SLOT.with(|s| s.id.get());
            SLOT.with(|s| s.id.set(n));
            std::thread::sleep(std::time::Duration::from_millis(1));
            let back = SLOT.with(|s| s.id.get());
            if back == n && start == 0 {
                ISO_OK.fetch_add(1, Ordering::SeqCst);
            } else {
                ISO_BAD.fetch_add(1, Ordering::SeqCst);
            }
        })
        .expect("spawn");
    h.join().expect("join");
}

// ---------------------------------------------------------------------------
// unwind — a thread that panics and unwinds out of its closure.

static UNWIND_FRAME_DROPS: AtomicUsize = AtomicUsize::new(0);
static UNWIND_CAUGHT: AtomicUsize = AtomicUsize::new(0);
static UNWIND_JOIN_ERR: AtomicUsize = AtomicUsize::new(0);

/// Dropped only if the panic actually UNWINDS the frame. Under panic=abort
/// this never runs, so its count is the abort/unwind discriminator.
struct FrameGuard;

impl Drop for FrameGuard {
    fn drop(&mut self) {
        UNWIND_FRAME_DROPS.fetch_add(1, Ordering::SeqCst);
    }
}

fn unwind_cycle(n: u64) {
    let h = std::thread::Builder::new()
        .spawn(move || {
            SLOT.with(|s| s.id.set(n));
            // Inner catch_unwind: proves the unwind is catchable (a real
            // unwind, not an abort) and that TLS is readable in the landing pad.
            let caught = std::panic::catch_unwind(|| {
                let _g = FrameGuard;
                std::panic::panic_any(1u32);
            });
            if caught.is_err() && SLOT.with(|s| s.id.get()) == n {
                UNWIND_CAUGHT.fetch_add(1, Ordering::SeqCst);
            }
            // Now unwind out of the thread closure itself, with a live frame
            // guard, so the thread's EXIT path is the panicking one.
            let _g = FrameGuard;
            std::panic::panic_any(2u32);
        })
        .expect("spawn");
    if h.join().is_err() {
        UNWIND_JOIN_ERR.fetch_add(1, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// cthread — threads created by raw pthread_create, never by std::thread.

static C_TLS_OK: AtomicUsize = AtomicUsize::new(0);
static C_TLS_BAD: AtomicUsize = AtomicUsize::new(0);
static C_CURRENT_DISTINCT: AtomicUsize = AtomicUsize::new(0);
static C_STARTED: AtomicUsize = AtomicUsize::new(0);
static LAST_TID: std::sync::Mutex<Option<std::thread::ThreadId>> = std::sync::Mutex::new(None);

/// Entry point for a thread created by `pthread_create` directly — no
/// `std::thread`, so std never gets its own chance to set the thread up. This
/// is the shape an RTEMS/libbsd C callback context has.
extern "C" fn c_entry(arg: *mut core::ffi::c_void) -> *mut core::ffi::c_void {
    C_STARTED.fetch_add(1, Ordering::SeqCst);
    let n = arg as usize as u64;

    let start = SLOT.with(|s| s.id.get());
    SLOT.with(|s| s.id.set(n));
    CI_PLAIN.with(|c| c.set(n));
    CI_SLOT.with(|s| s.id.set(n));

    // Force std's own per-thread handle onto a foreign thread — this is the
    // allocation the stock image leaks. A ThreadId distinct from the previous
    // C thread's proves std minted a real per-thread handle rather than
    // handing back a shared one.
    let tid = std::thread::current().id();
    {
        let mut last = LAST_TID.lock().expect("LAST_TID");
        if *last != Some(tid) {
            C_CURRENT_DISTINCT.fetch_add(1, Ordering::SeqCst);
        }
        *last = Some(tid);
    }

    let back = SLOT.with(|s| s.id.get());
    let ci_back = CI_PLAIN.with(|c| c.get());
    let cid_back = CI_SLOT.with(|s| s.id.get());
    if start == 0 && back == n && ci_back == n && cid_back == n {
        C_TLS_OK.fetch_add(1, Ordering::SeqCst);
    } else {
        C_TLS_BAD.fetch_add(1, Ordering::SeqCst);
    }
    core::ptr::null_mut()
}

fn cthread_cycle(n: u64) {
    let mut tid: libc::pthread_t = 0;
    let rc = unsafe {
        libc::pthread_create(
            &mut tid,
            core::ptr::null(),
            c_entry,
            n as usize as *mut core::ffi::c_void,
        )
    };
    assert_eq!(rc, 0, "pthread_create failed rc={rc}");
    let rc = unsafe { libc::pthread_join(tid, core::ptr::null_mut()) };
    assert_eq!(rc, 0, "pthread_join failed rc={rc}");
}

// ---------------------------------------------------------------------------
// dialattempt — the per-connection-attempt DIAL cost, in a noise-free rig.
//
// The in-repo differential (doc §"Measured numbers": 176.0/179.1 B per
// connection ATTEMPT) is 136 B bare-thread handle + ~40-43 B of other
// per-attempt allocation (socket, dial request). A live IOC's heap churns by
// KB per C6 tick, so a single-image MEM_USED slope cannot resolve 40 B — the
// doc calls that "a documented false confirmation". This phase reproduces the
// pre-DialPool `CAC-connect` shape — one dedicated thread created per attempt
// that opens a TCP socket to a refused address and drops it — in a rig whose
// only heap activity is the attempt itself, so the per-attempt slope is
// resolvable to the byte. Measured stock-vs-native, it shows what the flip
// removes (the thread handle) and what, if anything, it leaves (the socket /
// non-handle residue).

static DIAL_REFUSED: AtomicUsize = AtomicUsize::new(0);
static DIAL_OTHER: AtomicUsize = AtomicUsize::new(0);

fn dialattempt_cycle(n: u64) {
    // One thread per attempt — the per-attempt-CREATE shape DialPool replaced.
    let h = std::thread::Builder::new()
        .name(format!("CAC-connect {n}"))
        .spawn(move || {
            SLOT.with(|s| s.id.set(n));
            // Refused address: nothing listens on 127.0.0.1:9 (discard), so the
            // connect returns ECONNREFUSED immediately — a fast, allocation-
            // bounded dial, no timeout wait, no lingering socket.
            let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 9));
            match std::net::TcpStream::connect(addr) {
                Ok(_s) => DIAL_OTHER.fetch_add(1, Ordering::SeqCst),
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                    DIAL_REFUSED.fetch_add(1, Ordering::SeqCst)
                }
                Err(_) => DIAL_OTHER.fetch_add(1, Ordering::SeqCst),
            };
        })
        .expect("spawn");
    h.join().expect("join");
}

// ---------------------------------------------------------------------------
// constinit — const-initialised thread_local!, with and without a destructor.

static CI_ISO_OK: AtomicUsize = AtomicUsize::new(0);
static CI_ISO_BAD: AtomicUsize = AtomicUsize::new(0);

fn constinit_cycle(n: u64) {
    let h = std::thread::Builder::new()
        .spawn(move || {
            let p0 = CI_PLAIN.with(|c| c.get());
            let d0 = CI_SLOT.with(|s| s.id.get());
            CI_PLAIN.with(|c| c.set(n));
            CI_SLOT.with(|s| s.id.set(n));
            std::thread::sleep(std::time::Duration::from_millis(1));
            let p1 = CI_PLAIN.with(|c| c.get());
            let d1 = CI_SLOT.with(|s| s.id.get());
            if p0 == 0 && d0 == 0 && p1 == n && d1 == n {
                CI_ISO_OK.fetch_add(1, Ordering::SeqCst);
            } else {
                CI_ISO_BAD.fetch_add(1, Ordering::SeqCst);
            }
        })
        .expect("spawn");
    h.join().expect("join");
}

// ---------------------------------------------------------------------------

const WARMUP: u64 = 5;

fn run_phase(label: &str, count: u64, cycle: &dyn Fn(u64)) {
    // Warm-up cycles absorb one-time lazy initialisation, so the reported
    // slope is steady-state rather than first-thread cost.
    for i in 0..WARMUP {
        cycle(900_000 + i);
    }
    let h0 = heap();
    sample(&format!("{label}-n0"), h0);

    for i in 0..count / 2 {
        cycle(i + 1);
    }
    let h1 = heap();
    sample(&format!("{label}-n{}", count / 2), h1);

    for i in count / 2..count {
        cycle(i + 1);
    }
    let h2 = heap();
    sample(&format!("{label}-n{count}"), h2);

    let half = (count / 2) as f64;
    let s1 = (h1.used_total as i64 - h0.used_total as i64) as f64 / half;
    let s2 = (h2.used_total as i64 - h1.used_total as i64) as f64 / half;
    let sall = (h2.used_total as i64 - h0.used_total as i64) as f64 / count as f64;
    println!(
        "SLOPE {label} N={count} \
         d_used_total_first_half={} d_used_total_second_half={} \
         d_used_number_total={} \
         bytes_per_thread_first={s1:.2} bytes_per_thread_second={s2:.2} \
         bytes_per_thread_overall={sall:.2} blocks_per_thread={:.3}",
        h1.used_total as i64 - h0.used_total as i64,
        h2.used_total as i64 - h1.used_total as i64,
        h2.used_number as i64 - h0.used_number as i64,
        (h2.used_number as i64 - h0.used_number as i64) as f64 / count as f64,
    );
}

fn main() {
    let which = env!("TLSDTOR_PHASE");
    println!(
        "TLSDTOR2-BEGIN build={} phase={which}",
        env!("TLSDTOR_BUILD")
    );
    println!(
        "TLSDTOR2-CFG target_thread_local={} panic_can_unwind={}",
        cfg!(target_thread_local),
        cfg!(panic = "unwind")
    );
    // Referencing the shim crate is what makes cargo forward its `-lbsd -lm -lz`
    // to this binary's link (see epics_rtems_boot::contract).
    println!(
        "TLSDTOR2-SHIM mem_usage_is_some={}",
        epics_rtems_boot::stats::mem_usage().is_some()
    );

    // The main thread's own slot. Touched before any other key so this rig's
    // key-creation order matches rig #1's, which is the order that reproduces
    // rig #1's 136 B/thread on the stock image (see the module comment).
    SLOT.with(|s| s.id.set(MAIN_SENTINEL));

    let n: u64 = 50;
    let expected = (n + WARMUP) as usize;

    // Silence the panic hook: 55 panic reports would drown the console and the
    // default hook's own formatting allocates on every panic.
    std::panic::set_hook(Box::new(|_| {}));

    let d0 = DTOR_RUNS.load(Ordering::SeqCst);
    let ci0 = CI_DTOR_RUNS.load(Ordering::SeqCst);

    match which {
        "plain" => run_phase("plain", n, &plain_cycle),
        "unwind" => run_phase("unwind", n, &unwind_cycle),
        "cthread" => run_phase("cthread", n, &cthread_cycle),
        "dialattempt" => run_phase("dialattempt", n, &dialattempt_cycle),
        "constinit" => run_phase("constinit", n, &constinit_cycle),
        other => panic!("unknown TLSDTOR_PHASE={other}"),
    }

    let dtor_runs = DTOR_RUNS.load(Ordering::SeqCst) - d0;
    let ci_dtor_runs = CI_DTOR_RUNS.load(Ordering::SeqCst) - ci0;
    let _ = std::panic::take_hook();

    println!(
        "TLSDTOR2-DTOR phase={which} slot_dtor_runs={dtor_runs} ci_dtor_runs={ci_dtor_runs} \
         expected_per_touched_tls={expected} slot_last_id={} ci_last_id={}",
        DTOR_LAST_ID.load(Ordering::SeqCst),
        CI_DTOR_LAST_ID.load(Ordering::SeqCst),
    );

    // Per-phase verdict. Only the selected phase's counters are asserted; the
    // others are printed as zeros and are not part of the verdict.
    let ok = match which {
        "plain" => {
            println!(
                "TLSDTOR2-PLAIN iso_ok={} iso_bad={} expected={expected}",
                ISO_OK.load(Ordering::SeqCst),
                ISO_BAD.load(Ordering::SeqCst),
            );
            ISO_OK.load(Ordering::SeqCst) == expected
                && ISO_BAD.load(Ordering::SeqCst) == 0
                && dtor_runs == expected
        }
        "unwind" => {
            println!(
                "TLSDTOR2-UNWIND frame_drops={} expected={} caught={} join_err={} expected_each={expected}",
                UNWIND_FRAME_DROPS.load(Ordering::SeqCst),
                2 * expected,
                UNWIND_CAUGHT.load(Ordering::SeqCst),
                UNWIND_JOIN_ERR.load(Ordering::SeqCst),
            );
            UNWIND_FRAME_DROPS.load(Ordering::SeqCst) == 2 * expected
                && UNWIND_CAUGHT.load(Ordering::SeqCst) == expected
                && UNWIND_JOIN_ERR.load(Ordering::SeqCst) == expected
                && dtor_runs == expected
        }
        "cthread" => {
            println!(
                "TLSDTOR2-CTHREAD started={} expected={expected} tls_ok={} tls_bad={} current_distinct={}",
                C_STARTED.load(Ordering::SeqCst),
                C_TLS_OK.load(Ordering::SeqCst),
                C_TLS_BAD.load(Ordering::SeqCst),
                C_CURRENT_DISTINCT.load(Ordering::SeqCst),
            );
            C_STARTED.load(Ordering::SeqCst) == expected
                && C_TLS_OK.load(Ordering::SeqCst) == expected
                && C_TLS_BAD.load(Ordering::SeqCst) == 0
                && C_CURRENT_DISTINCT.load(Ordering::SeqCst) == expected
                && dtor_runs == expected
                && ci_dtor_runs == expected
        }
        "dialattempt" => {
            println!(
                "TLSDTOR2-DIALATTEMPT refused={} other={} expected_refused={expected}",
                DIAL_REFUSED.load(Ordering::SeqCst),
                DIAL_OTHER.load(Ordering::SeqCst),
            );
            // The slope is the artefact here, not a pass/fail count; the verdict
            // only asserts the attempts actually happened as intended (all
            // refused) and TLS destructors ran, so a boot that silently did no
            // real dialing cannot masquerade as a clean slope.
            DIAL_REFUSED.load(Ordering::SeqCst) == expected
                && DIAL_OTHER.load(Ordering::SeqCst) == 0
                && dtor_runs == expected
        }
        "constinit" => {
            println!(
                "TLSDTOR2-CONSTINIT iso_ok={} iso_bad={} expected={expected}",
                CI_ISO_OK.load(Ordering::SeqCst),
                CI_ISO_BAD.load(Ordering::SeqCst),
            );
            CI_ISO_OK.load(Ordering::SeqCst) == expected
                && CI_ISO_BAD.load(Ordering::SeqCst) == 0
                && ci_dtor_runs == expected
        }
        _ => unreachable!(),
    };

    let main_slot = SLOT.with(|s| s.id.get());
    println!(
        "TLSDTOR2-MAIN slot=0x{main_slot:x} intact={}",
        main_slot == MAIN_SENTINEL
    );
    println!(
        "TLSDTOR2-VERDICT phase={which} ok={ok} main_ok={}",
        main_slot == MAIN_SENTINEL
    );
    println!("TLSDTOR2-DONE");
}
