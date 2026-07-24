//! Shared link-connection-status classification.
//!
//! Several records expose a `menu(...)` field per link that mirrors the C
//! `checkLinks` / `init_record` connection diagnostics. `sseq`
//! (`menu(sseqLNKV)`, `DOLnV`/`LNKnV`) and `calcout` (`menu(calcoutINAV)`,
//! `INAV`..`INUV`/`OUTV`) carry the identical four-choice menu and the
//! identical classification rule, so the choice table, the menu indices and
//! the `classify_link` helper live here once rather than being duplicated
//! per record (C `sseqRecord.dbd:20` and `calcoutRecord.dbd.pod:45-50` are
//! byte-for-byte the same choice set).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::server::database::AsyncDbHandle;
use crate::server::record::{LinkType, parse_link_v2};
use crate::types::DbFieldType;

/// Monotonic generation gate for spawned async link-status refreshes.
///
/// A record's `refresh_link_status` snapshots its link strings and
/// classifies them off-thread (`tokio::spawn`) before posting the result.
/// Two refreshes can race — e.g. an init-time refresh (links still empty →
/// `CON`) finishing *after* a runtime re-point (`special()` of a
/// `DOLn`/`LNKn`/`INPn` link → `LOC`) — and the stale task could clobber the
/// newer classification when it posts last. This gate makes the invariant
/// *only the latest classification may be published* hold by construction:
/// each spawn takes a token via [`next`](Self::next), and the spawned task
/// posts only while [`is_current`](Self::is_current) still holds for that
/// token. Every refresh call site runs under the record instance's write
/// lock, so the `fetch_add` that issues tokens is serialized and tokens are
/// strictly increasing.
#[derive(Clone, Default)]
pub struct LinkStatusGen(Arc<AtomicU64>);

impl LinkStatusGen {
    /// Issue the token for a newly spawned refresh, superseding any token
    /// already in flight. Call on the issuing thread, before `tokio::spawn`.
    pub fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// True while `token` is still the latest issued — i.e. no later refresh
    /// has started. Check inside the spawned task immediately before posting.
    pub fn is_current(&self, token: u64) -> bool {
        self.0.load(Ordering::SeqCst) == token
    }
}

/// Choice labels for the link-connection-status menu, in index order.
/// C `menu(sseqLNKV)` (sseqRecord.dbd:20) and `menu(calcoutINAV)`
/// (calcoutRecord.dbd.pod:45-50): 0=Ext PV NC, 1=Ext PV OK, 2=Local PV,
/// 3=Constant.
pub const LINK_STATUS_CHOICES: &[&str] = &["Ext PV NC", "Ext PV OK", "Local PV", "Constant"];

/// Link-status menu indices. Index 1 (`EXT`, external PV connected) is a
/// valid menu value that this port never *produces*: epics-base-rs has no
/// CA/PVA client to confirm a remote link is connected, so an external link
/// always reports `EXT_NC` (see [`classify_link`]). It is still named here
/// because a record reading the status back must treat BOTH external indices
/// as C's `CA_LINK` (see [`link_is_external`]).
pub const LINK_EXT_NC: i16 = 0; // external PV, not connected
pub const LINK_EXT_OK: i16 = 1; // external PV, connected (never produced here)
pub const LINK_LOC: i16 = 2; // local PV (this IOC's database)
pub const LINK_CON: i16 = 3; // constant / unset link

/// True iff a classified link status names an EXTERNAL PV — C's `CA_LINK`
/// (`dbInitLink` classifies by LOCALITY: a name that is not a record of this
/// IOC is reached over Channel Access). Both external menu indices count.
///
/// The classification is the caller's cached status: a record that has not
/// been classified yet reads its default (`LINK_CON`), which is NOT external
/// — a link is only a CA link once its status says so.
pub fn link_is_external(status: i16) -> bool {
    status == LINK_EXT_NC || status == LINK_EXT_OK
}

/// Sentinel for "no resolvable target field type", C `DBF_unknown` (-1): an
/// external or unresolvable link of either direction, and a constant OUTPUT link
/// (sseqRecord.c:225).
pub(crate) const DBF_UNKNOWN: i16 = -1;

/// C `DBF_NOACCESS` (dbFldTypes.h — dbStatic index 17), the code `init_record`
/// stores for a constant INPUT link (sseqRecord.c:206). It is not "unknown": the
/// constant HAS been consumed, once, by `recGblInitConstantLink` at init, and the
/// code exists to make the per-cycle read switch fall to `default: break` so the
/// constant is never re-read over a client's value. `DTn` is a wire-visible
/// diagnostic field, so the code itself has to be C's — the port served -1 where
/// C serves 17.
pub(crate) const DBF_NOACCESS: i16 = 17;

/// Which end of the record a link is attached to. C's `init_record` classifies a
/// CONSTANT link differently by direction — an input constant is loaded and marked
/// `DBF_NOACCESS` (sseqRecord.c:203-207), an output constant has no target at all
/// and stays `DBF_unknown` (:224-226) — so the classifier cannot answer without
/// being told which it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkRole {
    /// `DOL`/`INP`: read from.
    Input,
    /// `LNK`/`OUT`: written to.
    Output,
}

/// Map a resolved [`DbFieldType`] to the C `dbStatic` `dbfType` integer
/// (dbFldTypes.h:24-43) that `DTn`/`LTn` expose — NOT the CA `DBR` wire-type
/// discriminant the Rust enum carries. C `init_record` stores
/// `pAddr->field_type` (the dbStatic index) into `dol_field_type` /
/// `lnk_field_type` (sseqRecord.c:210), and a client reading those diagnostic
/// fields expects the dbStatic numbering, where `DBF_DOUBLE` is 10 — not the
/// CA `DBR_DOUBLE` value 6. The two orderings diverge because `DBF_INT64` /
/// `DBF_UINT64` occupy slots 7/8 ahead of `DBF_FLOAT` / `DBF_DOUBLE`.
///
/// The Rust model distinguishes signed `Char` (`DBF_CHAR` = 1, the dbStatic
/// field type CA `DBR_CHAR` resolves to, dbFldTypes.h:77) from unsigned
/// `UChar` (`DBF_UCHAR` = 2). A `DBF_MENU` (12) field is modelled as `Enum`
/// (`DBF_ENUM` = 11) — the same kind of collapse [`DBF_UNKNOWN`] documents
/// for `DBF_NOACCESS`. The match is exhaustive so a new `DbFieldType` variant
/// forces a deliberate code assignment here rather than a silent default.
fn dbf_static_code(ft: DbFieldType) -> i16 {
    match ft {
        DbFieldType::String => 0,  // DBF_STRING
        DbFieldType::Char => 1,    // DBF_CHAR
        DbFieldType::UChar => 2,   // DBF_UCHAR
        DbFieldType::Short => 3,   // DBF_SHORT
        DbFieldType::UShort => 4,  // DBF_USHORT
        DbFieldType::Long => 5,    // DBF_LONG
        DbFieldType::ULong => 6,   // DBF_ULONG
        DbFieldType::Int64 => 7,   // DBF_INT64
        DbFieldType::UInt64 => 8,  // DBF_UINT64
        DbFieldType::Float => 9,   // DBF_FLOAT
        DbFieldType::Double => 10, // DBF_DOUBLE
        DbFieldType::Enum => 11,   // DBF_ENUM
    }
}

/// Classify one DOL/LNK/INP/OUT link string into its connection-status menu
/// index and the target field type, mirroring C `checkLinks`/`init_record`
/// (sseqRecord.c:862-941,202-250; calcoutRecord.c:160-189).
///
/// Returns `(status, field_type)`. An external (CA/PVA) link is reported as
/// not-connected: epics-base-rs has no client to confirm a remote field's
/// connection state or type.
/// The database this reads is always the COMPLETE one: a classification issued
/// while records are still being created is queued by
/// [`AsyncDbHandle::schedule_record_init`] and only polled by `iocInit`, so a
/// forward reference — within one `.db` or across two `dbLoadRecords` calls —
/// resolves LOCAL, exactly as C's `iocInit`-time `init_record` does. There is
/// no gate to observe here because there is no way to run this against a
/// half-built database.
pub fn classify_link(handle: &AsyncDbHandle, link: &str, role: LinkRole) -> (i16, i16) {
    match parse_link_v2(link).link_type() {
        // Empty / constant link: C → CON. The field-type code is the direction's
        // (see [`DBF_NOACCESS`]).
        LinkType::Empty | LinkType::Constant => (
            LINK_CON,
            match role {
                LinkRole::Input => DBF_NOACCESS,
                LinkRole::Output => DBF_UNKNOWN,
            },
        ),
        // Local DB link: C `dbNameToAddr` ok → LOC + the addressed field's
        // type. A DB-syntax link whose target is not on this IOC resolves to
        // `None` and falls through to EXT_NC (C `init_record` else branch).
        LinkType::Db => match handle.link_target_field_type(link) {
            Some(ft) => (LINK_LOC, dbf_static_code(ft)),
            None => (LINK_EXT_NC, DBF_UNKNOWN),
        },
        // CA/PVA/other external link: epics-base-rs cannot introspect a
        // remote field's connection state or type — report not-connected.
        LinkType::Ca | LinkType::Other => (LINK_EXT_NC, DBF_UNKNOWN),
    }
}

/// Choice labels for swait's PV-status menu, in index order. C
/// `menu(swaitINAV)` (swaitRecord.dbd:18-22) — a DIFFERENT menu from
/// [`LINK_STATUS_CHOICES`]: swait reaches all of its links through
/// `recDynLink` (a CA-style dynamic link), so it reports connection state,
/// never "Local PV"/"Constant". The label at index 1 reads "PV BAD" while the
/// C code constant for the same index is `PV_NC` (swaitRecord.c:199-201).
pub const SWAIT_PV_STATUS_CHOICES: &[&str] = &["PV OK", "PV BAD", "No PV"];

/// swait PV-status menu indices (C `swaitRecord.c:199-201`).
pub const SWAIT_PV_OK: i16 = 0;
pub const SWAIT_PV_NC: i16 = 1;
pub const SWAIT_NO_PV: i16 = 2;

/// Classify one swait PV name (`INAN`..`INLN`, `DOLN`, `OUTN`) into its
/// `menu(swaitINAV)` status.
///
/// C `swaitRecord.c::init_record` (338-373) and `::special` (507-553) drive
/// this: a blank name is `NO_PV`, and any other name is set `PV_NC` and handed
/// to `recDynLinkAddInput`/`AddOutput`, after which `pvSearchCallback`
/// (900-928) flips it to `PV_OK` once the search connects and back to `PV_NC`
/// when it does not. epics-base-rs has no CA client, so "connected" is
/// "resolves to a field on this IOC": a DB-syntax name that addresses a local
/// record is `PV_OK`, and every name that cannot be resolved here — an unknown
/// record, a CA/PVA target, a bare constant that is not a PV name at all —
/// stays at the `PV_NC` C leaves it in until a search succeeds.
pub fn classify_swait_pv(handle: &AsyncDbHandle, name: &str) -> i16 {
    if name.trim().is_empty() {
        return SWAIT_NO_PV;
    }
    // Same `iocInit` boundary as `classify_link`: this only ever runs against
    // the completed database, so a forward-referenced local record is PV_OK.
    match parse_link_v2(name).link_type() {
        LinkType::Db => match handle.link_target_field_type(name) {
            Some(_) => SWAIT_PV_OK,
            None => SWAIT_PV_NC,
        },
        _ => SWAIT_PV_NC,
    }
}
