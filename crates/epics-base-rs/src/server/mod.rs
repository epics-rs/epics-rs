pub mod access_security;
pub mod autosave;
pub mod builtin_devices;
pub mod cvt_bpt;
pub mod database;
pub mod db_loader;
pub mod device_support;
pub mod event_queue;
pub mod ioc_app;
pub mod ioc_builder;
pub mod iocsh;
pub mod pv;
pub mod recgbl;
pub mod record;
pub mod records;
pub mod scan;
pub mod snapshot;
pub mod status_pv;

use crate::server::record::Record;

/// Factory function type for creating device support instances.
pub type DeviceSupportFactory =
    Box<dyn Fn() -> Box<dyn device_support::DeviceSupport> + Send + Sync>;

/// Factory function type for creating record instances.
pub type RecordFactory = Box<dyn Fn() -> Box<dyn Record> + Send + Sync>;

#[cfg(test)]
mod deferred_tail_owner_tests {
    //! One owner for every deferred record tail.
    //!
    //! `runtime::task::spawn` follows the *ambient* execution model, so on a
    //! hosted build it needs a tokio runtime entered on the calling thread.
    //! Record processing does not get one: every blocking CA/PVA connection
    //! thread drives it from a plain `std::thread` through `block_on_sync` →
    //! `park_on`, and the periodic-scan threads drive it on their own banded
    //! threads. A tail spawned from there panicked with "there is no reactor
    //! running" and killed that connection's dispatch thread.
    //!
    //! So the tails go to `spawn_background`, which lands on the process-global
    //! background executor on every backend. That is a property no reviewer can
    //! keep re-checking by eye across eleven files, and it already regressed
    //! once by being restated per call site instead of enforced — hence this
    //! census. It also names the delay counterparts, because a tail on the
    //! background executor that awaits `runtime::task::sleep` has moved the
    //! panic rather than removed it: the hosted `sleep` is `tokio::time`, which
    //! needs the same runtime the tail no longer has.

    /// Every production source file that defers a record tail. Two files in
    /// `server/` are deliberately absent, each for a reason the reader should
    /// be able to check: `event_queue.rs`, whose only seam uses are inside its
    /// test module, and `autosave/manager.rs`, whose tasks are not on the
    /// record-processing chain and whose saves go through `runtime::fs` —
    /// hosted `spawn_blocking`, which needs the runtime that
    /// `IocApplication::run` already gives them. Adding a deferred tail to a
    /// file outside this list is the gap this census cannot close on its own.
    const SOURCES: &[(&str, &str)] = &[
        (
            "database/processing.rs",
            include_str!("database/processing.rs"),
        ),
        ("database/links.rs", include_str!("database/links.rs")),
        (
            "database/link_put_queue.rs",
            include_str!("database/link_put_queue.rs"),
        ),
        ("database/mod.rs", include_str!("database/mod.rs")),
        (
            "database/db_access.rs",
            include_str!("database/db_access.rs"),
        ),
        ("records/sseq.rs", include_str!("records/sseq.rs")),
        ("access_security.rs", include_str!("access_security.rs")),
        ("ioc_app.rs", include_str!("ioc_app.rs")),
        ("pv.rs", include_str!("pv.rs")),
    ];

    /// The file with its `#[cfg(test)]` modules removed — every column-0
    /// `#[cfg(test)]` through the next column-0 `}`. Test code may name the
    /// ambient seam freely; it runs under `#[tokio::test]`.
    fn production_scope(src: &str) -> String {
        let mut out = String::with_capacity(src.len());
        let mut in_test = false;
        for line in src.lines() {
            if !in_test && line.starts_with("#[cfg(test)]") {
                in_test = true;
                continue;
            }
            if in_test {
                if line.starts_with('}') {
                    in_test = false;
                }
                continue;
            }
            out.push_str(line);
            out.push('\n');
        }
        out
    }

    /// Count occurrences that are a real call, not a doc mention: the seam is
    /// always named by its full path at a call site.
    fn count(hay: &str, needle: &str) -> usize {
        hay.match_indices(needle).count()
    }

    #[test]
    fn a_deferred_record_tail_never_uses_the_ambient_seam() {
        for (name, src) in SOURCES {
            let prod = production_scope(src);

            // Fail closed: if the slicer ever eats the whole file, the counts
            // below are trivially zero and this census proves nothing.
            assert!(
                prod.contains("crate::runtime::task::"),
                "{name}: production scope lost the runtime seam entirely — \
                 the `#[cfg(test)]` slicer is wrong, not the source"
            );

            for banned in [
                concat!("crate::runtime::task::", "spawn("),
                concat!("crate::runtime::task::", "spawn_blocking("),
                concat!("crate::runtime::task::", "sleep("),
                concat!("crate::runtime::task::", "interval("),
                concat!("tokio", "::spawn("),
            ] {
                assert_eq!(
                    count(&prod, banned),
                    0,
                    "{name}: `{banned}` in production scope. A deferred record \
                     tail must use the `_background` counterpart — it runs on a \
                     thread that has no runtime. If this site is genuinely a \
                     caller's own await rather than a spawned tail, move it out \
                     of this list with the reason."
                );
            }
        }
    }
}
