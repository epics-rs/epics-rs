pub mod access_security;
pub mod autosave;
pub mod builtin_devices;
pub mod cvt_bpt;
pub mod database;
pub mod db_convert_json;
pub mod db_loader;
pub mod db_server;
pub mod device_support;
pub mod driver_support;
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
    //!
    //! The rule itself lives in `record-seam-gate`, not here: `include_str!`
    //! may not escape a published crate's package directory, so a census
    //! written inline stops at the crate boundary — and the defect crossed it,
    //! into `std-rs` and `asyn-rs`. This module now supplies only the root and
    //! the exemptions, which are the part that is genuinely this crate's.
    //!
    //! It supplies a *root* and not a list because the list could only be as
    //! complete as its author's memory, and it was not: it named nine of the
    //! 115 sources under `server/`, so a deferred tail added to any of the
    //! other 106 was invisible to the census whose whole purpose was to make
    //! the rule impossible to break by eye. `record_trait.rs` was one of those
    //! 106, and it spent that time telling the reader the framework used
    //! `tokio::spawn` for `ReprocessAfter` — the exact opposite of the rule.
    //! The walk covers a new file the moment it exists.

    /// The one source under `server/` that may name the ambient seam.
    ///
    /// `autosave/manager.rs`'s tasks are not on the record-processing chain:
    /// every save goes through `runtime::fs` — hosted `spawn_blocking`, which
    /// needs the very runtime `IocApplication::run` already gives them — and
    /// all four save strategies are pollers started from there, none of them
    /// reached from a record callback.
    ///
    /// `event_queue.rs` used to need a line here too. It no longer does: its
    /// seam uses are inside its test module, which the slicer drops, so the
    /// walk clears it without a waiver. One fewer hand-written claim to trust.
    const EXEMPT: &[(&str, &str)] = &[(
        "src/server/autosave/manager.rs",
        "off the record-processing chain; saves go through runtime::fs \
         (hosted spawn_blocking), which needs IocApplication::run's runtime",
    )];

    #[test]
    fn a_deferred_record_tail_never_uses_the_ambient_seam() {
        record_seam_gate::assert_no_ambient_seam_in_tree(
            env!("CARGO_MANIFEST_DIR"),
            &["src/server"],
            EXEMPT,
        );
    }
}
