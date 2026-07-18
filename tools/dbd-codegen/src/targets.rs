//! The crates that own a vendored `.dbd` directory, and where each one's
//! generated field table lands.
//!
//! A record type is declared exactly once, by its `.dbd` — and that has to hold
//! for a record type *outside* `epics-base-rs` too. The generator used to read
//! one hard-coded directory and write one hard-coded file, so the six Tier-3
//! record types that live in downstream crates (`motor`, `table`, `scaler`,
//! `epid`, `throttle`, `timestamp`) had nowhere to put a generated table and
//! kept hand-written ones instead. Their `.dbd`s are now vendored into their own
//! crates and listed here; the mechanism is the same one base uses, extended,
//! not a Tier-3 special case — including the drift gate, which walks this list.
//!
//! [`BASE`] is the base target and is load-bearing beyond its own output: every
//! other target takes `dbCommon` from it (there is one `dbCommon` declaration in
//! the port, and it is base's) and resolves the shared menus (`menuYesNo`,
//! `menuOmsl`, `menuAlarmSevr`, ...) against base's generated consts rather than
//! re-declaring them.

/// One crate's `.dbd` directory and the module its table is generated into.
pub struct Target {
    /// The vendored `.dbd` directory, relative to the workspace root.
    pub dbd_dir: &'static str,
    /// The generated record-table module, relative to the workspace root.
    pub out_file: &'static str,
    /// The generated breakpoint-table module. Only base vendors `bpt*.dbd`.
    pub bpt_out_file: Option<&'static str>,
    /// How the generated module names `epics-base-rs`: `crate` when the table
    /// is emitted *into* base, the crate name when it is emitted into a
    /// downstream crate that depends on base.
    pub base_path: &'static str,
}

impl Target {
    /// The generated table is emitted into `epics-base-rs` itself.
    pub fn is_base(&self) -> bool {
        self.base_path == "crate"
    }
}

/// `epics-base-rs` — the EPICS Base record types, and the one `dbCommon`.
pub const BASE: Target = Target {
    dbd_dir: "crates/epics-base-rs/dbd",
    out_file: "crates/epics-base-rs/src/server/record/dbd_generated.rs",
    // The breakpoint tables are emitted to their OWN file, not spliced into
    // `dbd_generated.rs`: they come from a different grammar (`breaktable(...)`,
    // not `recordtype(...)`), they are `makeBpt` output rather than a
    // hand-written spec, and keeping them separate means a `bpt*.dbd` change
    // cannot produce a diff in the record tables.
    bpt_out_file: Some("crates/epics-base-rs/src/server/record/bpt_generated.rs"),
    base_path: "crate",
};

/// Every target, base first. Base must be first: the others are generated
/// against its `dbCommon` and its menus.
pub const TARGETS: &[Target] = &[
    BASE,
    Target {
        dbd_dir: "crates/motor-rs/dbd",
        out_file: "crates/motor-rs/src/record/dbd_generated.rs",
        bpt_out_file: None,
        base_path: "epics_base_rs",
    },
    Target {
        dbd_dir: "crates/optics-rs/dbd",
        out_file: "crates/optics-rs/src/records/dbd_generated.rs",
        bpt_out_file: None,
        base_path: "epics_base_rs",
    },
    Target {
        dbd_dir: "crates/scaler-rs/dbd",
        out_file: "crates/scaler-rs/src/records/dbd_generated.rs",
        bpt_out_file: None,
        base_path: "epics_base_rs",
    },
    Target {
        dbd_dir: "crates/std-rs/dbd",
        out_file: "crates/std-rs/src/records/dbd_generated.rs",
        bpt_out_file: None,
        base_path: "epics_base_rs",
    },
    Target {
        // The synApps `mca` (multichannel analyzer) record type. `mca.FTVL` is
        // `menu(menuFtype)` and `mca.VAL`/`mca.BG` are `special(SPC_DBADDR)`
        // runtime-typed from FTVL — both resolved exactly as the other
        // downstream targets resolve base's menus and their own cvt_dbaddr rows.
        dbd_dir: "crates/mca-rs/dbd",
        out_file: "crates/mca-rs/src/record/dbd_generated.rs",
        bpt_out_file: None,
        base_path: "epics_base_rs",
    },
    Target {
        // asyn's STANDARD device support. Unlike every other target, asyn
        // declares NO `recordtype(...)` — it only adds `device(...)` lines to
        // base's record types (`ai`, `mbbo`, `waveform`, ...). The emitter keys
        // its device menus by the `device()` record name, so it emits a
        // `device_menu()` for those types anyway; base merges that menu into the
        // `DTYP` choice list at serve time (`register_device_menu`). This is what
        // gives a client the asyn DTYPs a C fat softIoc lists after loading
        // `asyn.dbd`.
        dbd_dir: "crates/asyn-rs/dbd",
        out_file: "crates/asyn-rs/src/dbd_generated.rs",
        bpt_out_file: None,
        base_path: "epics_base_rs",
    },
];
