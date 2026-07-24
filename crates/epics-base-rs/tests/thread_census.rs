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
//! this crate that build threads with `std::thread::Builder`.

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

/// Every thread this crate creates states a stack size.
///
/// `std` gives RTEMS the generic 2 MiB `DEFAULT_MIN_STACK_SIZE`
/// (`std/src/sys/thread/unix.rs`: the carve-out list names vxworks, l4re,
/// espidf and nuttx — not rtems). On the host that is lazily-committed
/// address space and costs nothing measurable; on the target it is carved
/// eagerly out of a fixed pool, which is why an unset stack size is the
/// first ceiling the IOC hits rather than a rounding error.
///
/// `spawn_dedicated_thread` is enforced by its signature — the class is a
/// parameter, so a caller cannot omit it. This covers the threads that
/// still build a `std::thread::Builder` directly.
///
/// It also bans the API that has no class to state:
/// `std::thread::spawn` cannot express a stack size at all, so a site
/// using it does not fail the `Builder` check above — it is invisible to
/// it. Same defect, different anchor. (The six bare `thread::spawn` sites
/// elsewhere in the workspace — `ca::repeater`, `ca::calink`,
/// `ca::server::ca_server`, `pva::server::pva_server` — all sit behind
/// `#[cfg(not(target_os = "rtems"))]` module gates and are therefore
/// distinct: they are not in the RTEMS closure at all. Every file listed
/// here is.)
///
/// Fails today, on Linux, with no cross toolchain.
#[test]
fn every_thread_in_this_crate_states_a_stack_size() {
    let mut unclassified = Vec::new();
    let mut checked = 0usize;
    for (label, src) in files() {
        let prod = production_scope(src);
        for (n, after) in prod.split("thread::Builder::new()").skip(1).enumerate() {
            checked += 1;
            // The class must be set before the closure is handed over;
            // `.spawn(` ends the builder chain.
            let chain = after.split(".spawn(").next().unwrap_or("");
            if !chain.contains(".stack_size(") {
                unclassified.push(format!("{label} (Builder #{})", n + 1));
            }
        }
        // The classless API. Split so this guard does not match its own
        // needle in the file it is written in.
        let bare = concat!("thread", "::spawn(");
        for (n, line) in prod.lines().enumerate() {
            let t = line.trim_start();
            if t.starts_with("//") {
                continue;
            }
            if t.contains(bare) && !t.contains("Builder") {
                unclassified.push(format!("{label}:{} (bare spawn)", n + 1));
            }
        }
    }

    assert!(
        checked >= 4,
        "expected to find the crate's Builder sites, found {checked} — \
         did a file move? update this guard's file list"
    );
    assert!(
        unclassified.is_empty(),
        "these threads take std's 2 MiB default on RTEMS: {unclassified:?}"
    );
}

/// Every thread this crate starts publishes its name to the OS.
///
/// `std` calls the platform `pthread_setname_np` from `Builder::spawn`
/// on the hosted targets it supports, and RTEMS is not one of them — so
/// there, a name set with `Builder::name` lives only in Rust's `Thread`
/// struct and the kernel shows nothing. Bring-up had to measure libbsd's
/// priority band by other means for exactly that reason.
///
/// The defect is a call that is *absent*, so this is source inspection
/// over every production `Builder` site in the crate — the same sweep
/// shape as `every_thread_in_this_crate_states_a_stack_size`, and it
/// fails the same way when a new thread forgets. Either prologue counts:
/// `enter_ioc_thread` for a thread with an EPICS band, bare
/// `name_current_thread` for one that deliberately has none.
#[test]
fn every_thread_in_this_crate_publishes_its_name() {
    let mut anonymous = Vec::new();
    let mut checked = 0usize;
    for (label, src) in files() {
        for (n, after) in production_scope(src)
            .split("thread::Builder::new()")
            .skip(1)
            .enumerate()
        {
            let (_chain, body) = after.split_once(".spawn(").unwrap_or((after, ""));
            checked += 1;
            // The prologue is the closure's first work, so look at the
            // closure, not the builder chain.
            if !body.contains("enter_ioc_thread(") && !body.contains("name_current_thread()") {
                anonymous.push(format!("{label} (Builder #{})", n + 1));
            }
        }
    }

    assert!(
        checked >= 4,
        "expected to find the crate's Builder sites, found {checked} — \
         did a file move? update this guard's file list"
    );
    assert!(
        anonymous.is_empty(),
        "these threads are invisible in an RTEMS task listing: {anonymous:?}"
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
/// permitted count is zero and any hit is a stray. The `Builder` floor is
/// what keeps that from passing vacuously after a file move, the same job
/// `seen_definition` does in the runtime half.
#[test]
fn only_the_prologue_reaches_the_banding_call() {
    let mut checked = 0usize;
    for (label, src) in files() {
        let prod = production_scope(src);
        checked += prod.split("thread::Builder::new()").skip(1).count();
        let strays: Vec<&str> = prod
            .lines()
            .map(str::trim)
            .filter(|l| l.contains("apply_to_current_thread("))
            .filter(|l| !l.starts_with("//"))
            .collect();
        assert!(
            strays.is_empty(),
            "{label}: only `enter_ioc_thread` may band a thread; \
             everything else would band an OS-anonymous one — {strays:?}"
        );
    }
    assert!(
        checked >= 4,
        "expected to find the crate's Builder sites, found {checked} — \
         did a file move? update this guard's file list"
    );
}
