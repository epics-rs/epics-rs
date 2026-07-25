//! IOC statistics, read out of the OS the IOC is running on.
//!
//! Two kinds, and they are two kinds because they answer different questions:
//!
//! * **Values** — [`fd_usage`] and [`mem_usage`], the readings the status PVs
//!   publish. These are the numbers a target build cannot get from `std`, and
//!   each OS keeps them somewhere different.
//! * **The console census** — [`dump_tasks`], [`stack_report`] and
//!   [`fd_census`], which print rather than return. They exist because a target
//!   has no shell to ask: with no `iocsh` and no console input path, the only
//!   way to get `rt top`, `rt stackuse` or a descriptor listing off the board
//!   is for the image to print them itself.
//! * **The thread-census hook** — [`register_task`], which is neither a reading
//!   nor a printer but the one thing a backend may need *before* it can print
//!   one. It is here rather than in a consumer because which OS needs it is a
//!   backend's business, and the caller is the same either way.
//!
//! This module is the *funnel* for both: the types, and one entry point per
//! reading that every consumer calls. The per-OS half lives in a `backend`
//! module selected below, and there are three — `rtems.rs`, the C in
//! `csrc/rtems_stats.c` ported from devIocStats' `os/RTEMS/osdFdUsage.c` and
//! `osdMemUsage.c`; `vxworks.rs`, POSIX through `libc` for an IOC running as an
//! RTP; and `unsupported.rs`, which reports no reading and prints nothing.
//!
//! # Why the OS fork is a module and not a `#[cfg]` arm
//!
//! Nothing below carries a `#[cfg]` of its own, and the
//! `the_os_fork_happens_only_at_the_backend_selection` test below keeps it that
//! way. The alternative — a `#[cfg]` set inside each entry point — costs one
//! new arm per function per OS, and every consumer that wanted the same reading
//! would reproduce the same fork at its own call site. That is the shape
//! devIocStats avoids too: one record set, one OSD file per OS.
//!
//! The census in particular was reproduced per consumer before this: each of
//! the two target IOC binaries carried its own `extern "C"` block *and* its own
//! `#[cfg(target_os = …)]` / `#[cfg(not(…))]` wrapper around every call, so
//! `realtime-pva-ioc` had drifted to declaring two of the three functions and
//! calling neither [`fd_census`] nor anything in its place. Two copies of a
//! per-OS rule is how they come to disagree.
//!
//! So a second OS is a new file plus one `mod` line here. It is not a change to
//! any consumer, and it cannot become one: value consumers see `Option`, which
//! already means "this build has no reading", and census consumers see a call
//! whose whole contract is that it prints — a backend with nothing to print
//! says so on its own line rather than making the caller ask.
//!
//! # `None` means unavailable, not zero
//!
//! Every reading is an [`Option`] and is [`None`] on any build whose OS has no
//! backend — a whole [`Option<FdUsage>`](FdUsage) for descriptors, which are one
//! measurement, and a per-field one for [`MemUsage`], whose three come from one
//! call on RTEMS but not everywhere. See `unsupported.rs` for why the
//! distinction from zero is load-bearing rather than decorative, and
//! [`MemUsage`] for why the optionality is on the fields.
//!
//! # Cost
//!
//! [`fd_usage`] walks the descriptor table, so it is O(descriptor-table size) —
//! 150 entries on the RTEMS BSP. [`mem_usage`] walks the heap under the
//! allocator's lock on RTEMS; devIocStats' own header comment says gathering
//! heap statistics "could be expensive" and warns against running it too often.
//! On VxWorks it is one counter read, no walk. Neither is on any serving path;
//! both are called once per tick by the status pusher, whose interval is one
//! second.
//!
//! [`register_task`] is O(registered threads) only when the registry is full,
//! and O(1) otherwise. It runs once per thread, at thread start.

// The one place this module knows what OS it is on. Exactly one arm is live in
// any build, so everything below is written once.
#[cfg(all(target_os = "rtems", rtems_boot_linked))]
#[path = "rtems.rs"]
mod backend;
#[cfg(target_os = "vxworks")]
#[path = "vxworks.rs"]
mod backend;
#[cfg(not(any(all(target_os = "rtems", rtems_boot_linked), target_os = "vxworks")))]
#[path = "unsupported.rs"]
mod backend;

/// Open file descriptors and the ceiling they are counted against.
///
/// On RTEMS `max` is `rtems_libio_number_iops`, i.e.
/// `CONFIGURE_MAXIMUM_FILE_DESCRIPTORS` from `csrc/rtems_config.c`. Measured on
/// the bring-up box, this is the limit the IOC hits *first*: at 142 concurrent
/// CA connections the 143rd was refused by the libbsd socket zone, which is
/// sized from this cap — ahead of the heap ceiling, which had room for roughly
/// ten more connections at that moment.
///
/// The cap of 150 is base's own score-arm value, run on a target where base
/// itself compiles the POSIX arm and runs 64 — a deviation in which arm's
/// number we take, not an invented number. `doc/rtems-fd-ceiling-deviation.md`
/// carries the measurements, including why `free()` at idle is numerically the
/// connection ceiling and why `CA_REFUSED_CNT` never sees this wall.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FdUsage {
    /// Descriptors currently open, across the whole image — sockets, the
    /// console and anything the filesystem holds, not only CA/PVA connections.
    pub used: u32,
    /// The configured size of the descriptor table.
    pub max: u32,
}

impl FdUsage {
    /// Descriptors still available.
    ///
    /// Saturating, so a table that reports more open than configured (which
    /// would be a kernel accounting bug, not a state this can be in) reads as
    /// zero free rather than wrapping to four billion.
    pub fn free(&self) -> u32 {
        self.max.saturating_sub(self.used)
    }
}

/// Malloc-heap usage: three independent measurements, each present or not.
///
/// The RTEMS *workspace* is a separate allocator and is not counted here;
/// devIocStats keeps it in its own OSD file for the same reason.
///
/// # Why the fields are optional and the struct is not
///
/// The three come from one call on RTEMS but not everywhere: VxWorks 7's
/// mimalloc reports committed bytes and has no free-run metric at all, so
/// `used` is a real reading on a target where `largest_free` does not exist.
/// An all-or-nothing `Option<MemUsage>` forces a backend in that position to
/// throw away a measurement it has, or to invent the two it does not.
///
/// Optional *fields* also leave exactly one way to say "no reading". An
/// `Option<MemUsage>` whose fields were plain `u64` would have two — an absent
/// struct and a struct of zeros — and the second is indistinguishable from a
/// genuinely exhausted heap, which is the reading an operator most needs to
/// believe.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemUsage {
    /// Bytes free in the malloc heap, summed over every free block.
    pub free: Option<u64>,
    /// Bytes currently allocated from the malloc heap.
    pub used: Option<u64>,
    /// The largest single free block.
    ///
    /// Reported separately from [`free`](Self::free) because it is the
    /// fragmentation signal: an allocation fails on this number, not on the
    /// total. A heap with megabytes free in kilobyte fragments cannot start a
    /// connection whose thread wants a 512 KiB stack, and only this field says
    /// so.
    pub largest_free: Option<u64>,
}

impl MemUsage {
    /// Free plus allocated, or [`None`] unless both are readings.
    ///
    /// Derived here rather than in a backend, because it is arithmetic on two
    /// measurements rather than a third measurement. devIocStats computes the
    /// same sum (`osdMemUsage.c:73`) and its own header comment flags that this
    /// is *not* the true total the allocator was given — the difference is
    /// allocator overhead.
    ///
    /// A sum over one known part and one unknown one would be a lower bound
    /// wearing a total's name, so a missing part makes the total missing too.
    pub fn total(&self) -> Option<u64> {
        Some(self.free?.saturating_add(self.used?))
    }
}

/// Read descriptor usage, or [`None`] on a build whose OS has no backend.
pub fn fd_usage() -> Option<FdUsage> {
    backend::fd_usage()
}

/// Read heap usage. Every field is [`None`] on a build whose OS has no
/// backend, and individual fields are [`None`] where the OS cannot measure
/// them.
pub fn mem_usage() -> MemUsage {
    backend::mem_usage()
}

/// Print the task census — thread count, kernel names, effective priorities —
/// tagged with `tag`. No output on a build whose OS has no backend.
///
/// This is the `rt top` half of what a shell would give an operator, produced
/// from inside the image because the target has no shell task and no console
/// input path at all. The tag is what pairs a block with the phase of a
/// measurement that produced it.
pub fn dump_tasks(tag: &str) {
    backend::dump_tasks(tag)
}

/// Print the stack high-water report, tagged with `tag`. No output on a build
/// whose OS has no backend.
///
/// The `rt stackuse` half. On RTEMS this is the shell command's own
/// implementation called directly, which is why the output format is the
/// shell's rather than something this crate invents.
pub fn stack_report(tag: &str) {
    backend::stack_report(tag)
}

/// Print one line per open descriptor, classified, tagged with `tag`. No
/// output on a build whose OS has no backend.
///
/// [`fd_usage`] answers *how many* descriptors are open, which is the number
/// that predicts the connection ceiling. It cannot answer *which*, and an
/// outage measurement needs exactly that: a client whose circuits are all down
/// still holds one descriptor more than it held at boot, and "one unexplained
/// fd" is only a finding once the other seven are named.
///
/// The walk is the same one [`fd_usage`] does, so the census cannot disagree
/// with the count beside it. Read-only on every descriptor it touches: it can
/// run while the pumps own their sockets.
pub fn fd_census(tag: &str) {
    backend::fd_census(tag)
}

/// Note the calling thread in the backend's thread census. A no-op on every
/// backend whose OS can enumerate its own threads.
///
/// [`dump_tasks`] and [`stack_report`] need a list of threads to describe.
/// RTEMS hands one over — `rtems_task_iterate` walks the kernel's own table —
/// and needs nothing from the caller. An RTP on VxWorks 7 cannot ask: measured,
/// `taskIdListGet` and `taskEach` are kernel-mode only and absent from every RTP
/// library, so a task can be described once it is named but the set of tasks
/// cannot be discovered. There the list has to be built as the threads start,
/// and this is the call that builds it.
///
/// # Where this is called from, and why only there
///
/// `epics_libcom_rs::runtime::task::enter_ioc_thread`, plus `main` in each
/// target IOC binary, which does not go through it. That is one site because
/// `enter_ioc_thread` is already the single owner of the thread-transition it
/// rides on: every IOC thread calls it to take its scheduling band, so "every
/// thread that bands itself registers itself" is the same invariant with one
/// more consequence, not a new rule to remember at each spawn.
///
/// A thread that starts *outside* that seam is therefore invisible to the
/// VxWorks census. That is a real limitation and it is printed in the census
/// output's own header rather than left in this comment, because a reader who
/// takes the block for the RTP's thread table would under-count and have no way
/// to know.
///
/// Idempotent, so a thread that ends up here twice is registered once.
pub fn register_task() {
    backend::register_task()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The funnel's own source lines — this file down to the test module, with
    /// comments dropped.
    ///
    /// Both halves matter. The `#[cfg(test)]` split keeps the tests below out
    /// of scope, since they legitimately carry the attributes and call the
    /// paths the guards forbid above them. Dropping comment lines keeps the
    /// *prose* out too: the module docs explain the backend selection by
    /// quoting the shapes it replaced, and a guard that reads its own
    /// explanation as code fails on the sentence describing it. Measured — both
    /// guards below failed exactly that way on their first run.
    fn funnel_code() -> impl Iterator<Item = &'static str> {
        include_str!("mod.rs")
            .split_once("#[cfg(test)]")
            .expect("the test module is still here")
            .0
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
    }

    /// The host has no backend, so both readers must say so rather than
    /// inventing a reading. A host build that reported `0` free descriptors
    /// would look like an exhausted IOC.
    #[test]
    fn a_host_build_reports_no_reading_rather_than_a_made_up_one() {
        assert_eq!(fd_usage(), None);
        assert_eq!(mem_usage(), MemUsage::default());
        assert_eq!(mem_usage().total(), None);
    }

    /// `MemUsage::default()` is what a backend returns when it has nothing, so
    /// it must be three absent readings and not three zeros. A zeroed default
    /// would publish "0 bytes free" — an exhausted heap — on every host build.
    #[test]
    fn the_empty_heap_reading_is_absent_rather_than_zero() {
        let none = MemUsage::default();
        assert_eq!(none.free, None);
        assert_eq!(none.used, None);
        assert_eq!(none.largest_free, None);
    }

    /// `free` is the number an operator watches approach zero, so it must not
    /// wrap into a huge "plenty available" on an impossible input.
    #[test]
    fn descriptor_headroom_saturates_instead_of_wrapping() {
        assert_eq!(
            FdUsage {
                used: 142,
                max: 150
            }
            .free(),
            8
        );
        assert_eq!(
            FdUsage {
                used: 150,
                max: 150
            }
            .free(),
            0
        );
        assert_eq!(
            FdUsage {
                used: 151,
                max: 150
            }
            .free(),
            0,
            "more open than configured is a kernel accounting bug; reading it \
             as 4 billion free is worse than reading it as none"
        );
    }

    /// The heap total is the sum of the two measurements, and nothing else —
    /// this is the property that keeps it from drifting from its parts.
    #[test]
    fn the_heap_total_is_exactly_free_plus_used() {
        let m = MemUsage {
            free: Some(3_000_000),
            used: Some(5_000_000),
            largest_free: Some(1_500_000),
        };
        assert_eq!(m.total(), Some(8_000_000));
    }

    /// A backend that knows `used` but not `free` — VxWorks, whose mimalloc
    /// reports committed bytes and has no free-run metric — must not publish a
    /// total. Adding a known part to an unknown one yields a lower bound, and
    /// `MEM_MAX` is read as a capacity, so a lower bound there reads as an IOC
    /// with less memory than it has.
    #[test]
    fn a_partial_heap_reading_has_no_total() {
        let partial = MemUsage {
            free: None,
            used: Some(5_000_000),
            largest_free: None,
        };
        assert_eq!(partial.total(), None);
        assert_eq!(partial.used, Some(5_000_000), "the known part survives");
    }

    /// The largest free block is a distinct measurement, not a restatement of
    /// the free total: the gap between them is the fragmentation an allocation
    /// actually fails on.
    #[test]
    fn the_largest_free_block_is_independent_of_the_free_total() {
        let fragmented = MemUsage {
            free: Some(4_000_000),
            used: Some(1_000_000),
            largest_free: Some(40_000),
        };
        assert!(
            fragmented.largest_free < fragmented.free,
            "a heap with 4 MB free in 40 KB fragments cannot start a connection \
             whose thread wants a 512 KiB stack, and only this field says so"
        );
    }

    /// The funnel's whole claim is that the OS fork happens once, at the
    /// `backend` selection, and nowhere else. A `#[cfg]` anywhere else in this
    /// file is a second fork — which is how the two readers come to disagree
    /// about which OS they are on, and how a consumer learns to fork too.
    ///
    /// Counted rather than pattern-matched, so the assertion cannot be
    /// satisfied by a `#[cfg]` that merely *looks* like a backend selection.
    #[test]
    fn the_os_fork_happens_only_at_the_backend_selection() {
        assert_eq!(
            funnel_code()
                .filter(|l| l.contains("#[cfg("))
                .collect::<Vec<_>>()
                .len(),
            funnel_code().filter(|l| l.contains("mod backend;")).count(),
            "every `#[cfg]` in the funnel must be a backend selection"
        );
    }

    /// Every backend file, paired with its contents. The census below is only
    /// as good as this list, so the list is checked against `mod.rs` rather
    /// than trusted.
    const BACKENDS: &[(&str, &str)] = &[
        ("rtems.rs", include_str!("rtems.rs")),
        ("vxworks.rs", include_str!("vxworks.rs")),
        ("unsupported.rs", include_str!("unsupported.rs")),
    ];

    /// A backend file that is `#[cfg]`ed out is not compiled, so nothing about
    /// it is checked by building — the host build type-checks exactly one of
    /// them, and a missing function in any other would surface only on that
    /// OS's own build, which on this workspace means on the bring-up box.
    ///
    /// So the surface is a census instead: every entry point the funnel calls
    /// must exist in every backend file, and every backend file `mod.rs`
    /// selects must be in the list above. Both directions, because either one
    /// alone can be satisfied while the other quietly is not.
    #[test]
    fn every_backend_implements_the_whole_funnel_surface() {
        let declared: Vec<&str> = funnel_code()
            .filter_map(|l| l.trim().strip_prefix("#[path = \""))
            .map(|r| &r[..r.find('"').expect("the path attribute is terminated")])
            .collect();
        let listed: Vec<&str> = BACKENDS.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            declared, listed,
            "every backend `mod.rs` selects must be listed in BACKENDS, in order"
        );

        // Taken from the funnel's own calls, so a new entry point cannot be
        // added here without every backend growing it.
        let required: Vec<&str> = funnel_code()
            .filter_map(|l| l.trim().strip_prefix("backend::"))
            .map(|r| &r[..r.find('(').expect("a backend call is a call")])
            .collect();
        assert!(
            !required.is_empty(),
            "the funnel must delegate to the backend"
        );

        for (name, body) in BACKENDS {
            for f in &required {
                assert!(
                    body.contains(&format!("fn {f}(")),
                    "backend `{name}` does not implement `{f}`, which the funnel calls"
                );
            }
        }
    }

    /// The C is compiled on exactly one configuration, and the backend that
    /// declares its symbols must not outrun it — a declaration on a wider cfg
    /// leaves an undefined symbol in the toolchain-free portability build.
    ///
    /// Asserted on the `mod` line rather than inside `rtems.rs`, because that
    /// is now where the gate is: gating the whole file at its declaration is
    /// what makes it impossible to add an `extern` to it on a wider cfg.
    #[test]
    fn the_rtems_backend_is_scoped_to_the_configuration_that_compiles_the_c() {
        let src = include_str!("mod.rs");
        let cfg = concat!("#[cfg(all(target_os = \"rtems\", ", "rtems_boot_linked))]");
        assert!(
            src.contains(&format!("{cfg}\n#[path = \"rtems.rs\"]\nmod backend;")),
            "the RTEMS backend must sit directly under the linked-image cfg"
        );
    }

    /// The shim must stay in the build script's file list and its change list;
    /// dropping either leaves the RTEMS backend pointing at nothing, or leaves
    /// a stale object behind after an edit.
    #[test]
    fn the_build_script_compiles_and_watches_the_shim() {
        let build = include_str!("../../build.rs");
        assert!(build.contains(".file(\"csrc/rtems_stats.c\")"));
        assert!(build.contains("cargo::rerun-if-changed=csrc/rtems_stats.c"));
    }

    /// The two deviations from devIocStats are forced by RTEMS 6 and were
    /// measured against this BSP's installed headers. If someone "restores"
    /// upstream's spelling the file stops compiling, so the note is what keeps
    /// the next reader from trying.
    ///
    /// Searched from the first `#include` onward, never the whole file: the
    /// file's header comment *names* both upstream spellings in order to
    /// explain why they are gone, so a whole-file search would find the prose
    /// and pass while the code said the opposite. Measured — the negative
    /// assertion below failed exactly that way on its first run.
    #[test]
    fn the_shim_uses_the_rtems_6_atomic_flag_accessor() {
        let code = include_str!("../../csrc/rtems_stats.c")
            .split_once("#include <stddef.h>")
            .expect("the shim still starts its code with the includes")
            .1;
        assert!(
            code.contains("rtems_libio_iop_flags(&rtems_libio_iops[i])"),
            "`iop->flags` is Atomic_Uint in RTEMS 6, so devIocStats' direct \
             `.flags &` does not compile here"
        );
        assert!(
            !code.contains("rtems_region_get_information"),
            "the pre-4.8 region fallback is dead code on RTEMS 6"
        );
    }
}
