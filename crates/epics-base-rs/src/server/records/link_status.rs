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
pub(crate) struct LinkStatusGen(Arc<AtomicU64>);

impl LinkStatusGen {
    /// Issue the token for a newly spawned refresh, superseding any token
    /// already in flight. Call on the issuing thread, before `tokio::spawn`.
    pub(crate) fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// True while `token` is still the latest issued — i.e. no later refresh
    /// has started. Check inside the spawned task immediately before posting.
    pub(crate) fn is_current(&self, token: u64) -> bool {
        self.0.load(Ordering::SeqCst) == token
    }
}

/// Choice labels for the link-connection-status menu, in index order.
/// C `menu(sseqLNKV)` (sseqRecord.dbd:20) and `menu(calcoutINAV)`
/// (calcoutRecord.dbd.pod:45-50): 0=Ext PV NC, 1=Ext PV OK, 2=Local PV,
/// 3=Constant.
pub(crate) const LINK_STATUS_CHOICES: &[&str] = &["Ext PV NC", "Ext PV OK", "Local PV", "Constant"];

/// Link-status menu indices. Index 1 (`EXT`, external PV connected) is a
/// valid menu value but is never *produced* by this port: epics-base-rs has
/// no CA/PVA client to confirm a remote link is connected, so an external
/// link always reports `EXT_NC` (see [`classify_link`]). The choice label is
/// still served via [`LINK_STATUS_CHOICES`], so the `EXT` constant is
/// intentionally omitted here — nothing emits it.
pub(crate) const LINK_EXT_NC: i16 = 0; // external PV, not connected
pub(crate) const LINK_LOC: i16 = 2; // local PV (this IOC's database)
pub(crate) const LINK_CON: i16 = 3; // constant / unset link

/// Sentinel for "no resolvable target field type", C `DBF_unknown` (-1).
/// Used for every constant, external, and unresolvable link. C
/// `init_record` further distinguishes a constant DOL (`DBF_NOACCESS`) from
/// a constant LNK (`DBF_unknown`) (sseqRecord.c:206,225); that split is
/// collapsed to a single unknown here because the Rust `DbFieldType` model
/// has no `NOACCESS` variant.
pub(crate) const DBF_UNKNOWN: i16 = -1;

/// Map a resolved [`DbFieldType`] to the C `dbStatic` `dbfType` integer
/// (dbFldTypes.h:24-43) that `DTn`/`LTn` expose — NOT the CA `DBR` wire-type
/// discriminant the Rust enum carries. C `init_record` stores
/// `pAddr->field_type` (the dbStatic index) into `dol_field_type` /
/// `lnk_field_type` (sseqRecord.c:210), and a client reading those diagnostic
/// fields expects the dbStatic numbering, where `DBF_DOUBLE` is 10 — not the
/// CA `DBR_DOUBLE` value 6. The two orderings diverge because `DBF_INT64` /
/// `DBF_UINT64` occupy slots 7/8 ahead of `DBF_FLOAT` / `DBF_DOUBLE`.
///
/// The Rust model has a single `Char` (no signed/unsigned split): it maps to
/// `DBF_CHAR` (1), the dbStatic field type CA `DBR_CHAR` resolves to
/// (dbFldTypes.h:77). A `DBF_UCHAR` (2) source is not separately
/// representable, and a `DBF_MENU` (12) field is modelled as `Enum`
/// (`DBF_ENUM` = 11) — the same kind of collapse [`DBF_UNKNOWN`] documents
/// for `DBF_NOACCESS`. The match is exhaustive so a new `DbFieldType` variant
/// forces a deliberate code assignment here rather than a silent default.
fn dbf_static_code(ft: DbFieldType) -> i16 {
    match ft {
        DbFieldType::String => 0,  // DBF_STRING
        DbFieldType::Char => 1,    // DBF_CHAR
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
pub(crate) async fn classify_link(handle: &AsyncDbHandle, link: &str) -> (i16, i16) {
    match parse_link_v2(link).link_type() {
        // Empty / constant link: C → CON, no resolvable field type.
        LinkType::Empty | LinkType::Constant => (LINK_CON, DBF_UNKNOWN),
        // Local DB link: C `dbNameToAddr` ok → LOC + the addressed field's
        // type. A DB-syntax link whose target is not on this IOC resolves to
        // `None` and falls through to EXT_NC (C `init_record` else branch).
        LinkType::Db => match handle.link_target_field_type(link).await {
            Some(ft) => (LINK_LOC, dbf_static_code(ft)),
            None => (LINK_EXT_NC, DBF_UNKNOWN),
        },
        // CA/PVA/other external link: epics-base-rs cannot introspect a
        // remote field's connection state or type — report not-connected.
        LinkType::Ca | LinkType::Other => (LINK_EXT_NC, DBF_UNKNOWN),
    }
}
