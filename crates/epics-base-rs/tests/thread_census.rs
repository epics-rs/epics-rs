//! This crate's half of the thread-prologue source census.
//!
//! The three guards below used to live in `runtime::task`'s test module and
//! swept both crates' sources through one `include_str!` file list. Splitting
//! the runtime layer out (issue #55) made that list cross a crate boundary,
//! which `include_str!` must never do — a path reaching outside the package
//! directory does not survive `cargo publish`, so the extracted crate would
//! have shipped tests that cannot build. A source-census guard belongs in the
//! same crate as the source it audits.
//!
//! So each guard was split by subject, not weakened: `epics-runtime-rs` keeps
//! the halves that audit its own files, this file audits ours, and the
//! assertions are the originals. Only the file lists and the count floors
//! differ, and the floors got *tighter* — each is now this half's exact site
//! count rather than a slack share of the combined one.
//!
//! The subject is `server/ioc_app.rs` and `server/scan.rs`: the two files in
//! this crate that create threads.
//!
//! # Why the `Builder` sweeps became a ban
//!
//! All four of this crate's thread sites are threads the IOC cannot correctly
//! run without — the scan owner, the seven `scan-%g` rates, and the two iocsh
//! boot-script runners — and all four now go through
//! `runtime::task::MandatoryThread`, whose constructor takes the name, the band
//! and the stack class. That is the same "enforced by its signature" argument
//! the sweeps already made for `spawn_dedicated_thread`: a caller cannot omit
//! what it must pass, so counting `Builder` sites and inspecting their chains
//! has nothing left to inspect here.
//!
//! Rather than let the guards pass vacuously, they became the stronger
//! statement they were approximating: **this crate creates no thread outside
//! `runtime::task`'s owners.** That covers the original two findings (2 MiB
//! default stacks, OS-anonymous threads) *and* the one that motivated the move
//! — a mandatory thread whose `spawn` failure was resolved locally with
//! `.expect`, killing one thread on a `panic = "unwind"` target and leaving the
//! IOC serving without it.

/// Everything before the first column-0 `#[cfg(test)]` — the code that
/// actually ships.
fn production_scope(src: &str) -> &str {
    match src.find("\n#[cfg(test)]") {
        Some(i) => &src[..i],
        None => src,
    }
}

/// The two files this census covers, as (label, source) pairs.
fn files() -> [(&'static str, &'static str); 2] {
    [
        (
            "server/ioc_app.rs",
            include_str!("../src/server/ioc_app.rs"),
        ),
        ("server/scan.rs", include_str!("../src/server/scan.rs")),
    ]
}

/// Source lines outside comments, with their 1-based numbers.
fn code_lines(src: &str) -> impl Iterator<Item = (usize, &str)> {
    production_scope(src)
        .lines()
        .enumerate()
        .map(|(n, l)| (n + 1, l.trim_start()))
        .filter(|(_, l)| !l.starts_with("//"))
}

/// Every thread this crate creates states a stack size, a band and a name —
/// because every one of them is created through an owner whose constructor
/// takes all three.
///
/// `std` gives RTEMS the generic 2 MiB `DEFAULT_MIN_STACK_SIZE`
/// (`std/src/sys/thread/unix.rs`: the carve-out list names vxworks, l4re,
/// espidf and nuttx — not rtems). On the host that is lazily-committed
/// address space and costs nothing measurable; on the target it is carved
/// eagerly out of a fixed pool, which is why an unset stack size is the
/// first ceiling the IOC hits rather than a rounding error. `std` likewise
/// calls the platform `pthread_setname_np` from `Builder::spawn` only on the
/// hosted targets it supports, and RTEMS is not one of them, so a name set
/// with `Builder::name` is invisible in an RTEMS task listing.
///
/// Both are parameters of `MandatoryThread::new` and of
/// `spawn_dedicated_thread`, so a caller cannot omit them. What is left to
/// check is that nothing sidesteps those constructors — including
/// `std::thread::spawn`, which cannot express a stack size at all and so was
/// invisible to the old `Builder`-chain sweep. (The bare `thread::spawn` sites
/// elsewhere in the workspace — `ca::repeater`, `ca::calink`,
/// `ca::server::ca_server`, `pva::server::pva_server`, `bridge::pvalink` — are
/// distinct: none is a mandatory IOC thread. They are the interactive iocsh
/// REPL, the best-effort in-process CA repeater fallback, and per-command
/// helpers that are joined on the next line.)
///
/// Fails today, on Linux, with no cross toolchain.
#[test]
fn this_crate_creates_no_thread_outside_the_runtime_owners() {
    // Split so this guard does not match its own needle in the file it is
    // written in.
    let bare = concat!("thread", "::spawn(");
    let mut strays = Vec::new();
    for (label, src) in files() {
        for (n, line) in code_lines(src) {
            if line.contains("thread::Builder::new()") {
                strays.push(format!("{label}:{n} (raw Builder)"));
            }
            if line.contains(bare) && !line.contains("Builder") {
                strays.push(format!("{label}:{n} (bare spawn)"));
            }
        }
    }
    assert!(
        strays.is_empty(),
        "these sites bypass `runtime::task`'s thread owners, so their stack \
         class, OS name and spawn-failure handling are stated locally or not \
         at all: {strays:?}"
    );
}

/// …and the owner is actually used, so the ban above cannot pass by the files
/// having quietly stopped creating threads.
///
/// The floor is this crate's exact site count: `scan.rs` creates the scan owner
/// and the periodic rates, `ioc_app.rs` the two iocsh boot-script runners.
#[test]
fn every_thread_in_this_crate_goes_through_mandatory_thread() {
    let mut owned = Vec::new();
    for (label, src) in files() {
        for (n, line) in code_lines(src) {
            if line.contains("MandatoryThread::new(") {
                owned.push(format!("{label}:{n}"));
            }
        }
    }
    assert!(
        owned.len() >= 4,
        "expected this crate's four mandatory-thread sites, found {} ({owned:?}) — \
         did a file move? update this guard's file list",
        owned.len()
    );
}

/// Every mandatory thread's spawn failure is resolved by its owner, never at
/// the call site.
///
/// `MandatoryThread::spawn` returns a bare `JoinHandle`, so there is nothing to
/// unwrap; `try_spawn` returns a `Result` precisely so a caller inside a
/// fallible boot step can hand the failure to whoever decides that this IOC
/// serves. Unwrapping *that* would restore the original defect: on a
/// `panic = "unwind"` target — RTEMS and VxWorks both default to unwind — the
/// panic kills only the calling thread, and the IOC goes on answering clients
/// with the work that thread owned never running.
#[test]
fn no_mandatory_spawn_failure_is_resolved_at_the_call_site() {
    let mut unwrapped = Vec::new();
    for (label, src) in files() {
        let prod = production_scope(src);
        for (n, after) in prod.split(".try_spawn(").skip(1).enumerate() {
            // The statement the `try_spawn` call belongs to: everything up to
            // the first `;` that follows it.
            let stmt = after.split_once(";").map(|(s, _)| s).unwrap_or(after);
            if stmt.contains(".expect(") || stmt.contains(".unwrap(") {
                unwrapped.push(format!("{label} (try_spawn #{})", n + 1));
            }
        }
    }
    assert!(
        unwrapped.is_empty(),
        "these sites turn a mandatory thread's spawn failure back into a \
         thread-local panic: {unwrapped:?}"
    );
}

/// The banding half of the prologue is not reachable without the naming
/// half: nothing in this crate's production scope calls
/// `runtime::task::apply_to_current_thread` at all.
///
/// Separate from the sweep above because it catches the other direction —
/// a thread that is named by `Builder` but takes its band directly, which
/// the closure-body sweep would pass if the naming call happened to be
/// somewhere else in the file.
///
/// The allowlist the runtime half carries — the definition line and the
/// prologue's own delegation — has no counterpart here: both live in
/// `epics-runtime-rs`'s `task.rs`, so on this side of the split the
/// permitted count is zero and any hit is a stray.
/// `every_thread_in_this_crate_goes_through_mandatory_thread` is what keeps
/// this from passing vacuously after a file move, the same job `seen_definition`
/// does in the runtime half.
#[test]
fn only_the_prologue_reaches_the_banding_call() {
    for (label, src) in files() {
        let strays: Vec<&str> = code_lines(src)
            .filter(|(_, l)| l.contains("apply_to_current_thread("))
            .map(|(_, l)| l)
            .collect();
        assert!(
            strays.is_empty(),
            "{label}: only `enter_ioc_thread` may band a thread; \
             everything else would band an OS-anonymous one — {strays:?}"
        );
    }
}
