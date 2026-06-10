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

use crate::server::database::AsyncDbHandle;
use crate::server::record::{LinkType, parse_link_v2};

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
            Some(ft) => (LINK_LOC, ft as i16),
            None => (LINK_EXT_NC, DBF_UNKNOWN),
        },
        // CA/PVA/other external link: epics-base-rs cannot introspect a
        // remote field's connection state or type — report not-connected.
        LinkType::Ca | LinkType::Other => (LINK_EXT_NC, DBF_UNKNOWN),
    }
}
