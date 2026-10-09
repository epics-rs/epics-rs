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
use crate::types::{DbFieldType, EpicsValue};

/// Monotonic generation gate for spawned async link-status refreshes.
///
/// A record's `refresh_link_status` snapshots its link strings and
/// classifies them off-thread before posting the result. Two refreshes can
/// race — e.g. an init-time refresh (links still empty → `CON`) finishing
/// *after* a runtime re-point (`special()` of a `DOLn`/`LNKn`/`INPn` link →
/// `LOC`) — and the stale task could clobber the newer classification when it
/// posts last. The invariant *only the latest classification may be
/// published* holds by construction: each spawn takes a [`LinkStatusToken`]
/// via [`next`](Self::next), and the token's [`post`](LinkStatusToken::post)
/// is the only way to publish. Tokens are issued under the record instance's
/// write lock and checked under the same lock the post takes, so no put can
/// issue a newer token between the check and the post.
#[derive(Clone, Default)]
pub struct LinkStatusGen(Arc<AtomicU64>);

impl LinkStatusGen {
    /// Issue the token for a newly spawned refresh, superseding any token
    /// already in flight. Call on the issuing thread, under the record write
    /// lock, before the spawn.
    pub fn next(&self) -> LinkStatusToken {
        let token = self.0.fetch_add(1, Ordering::SeqCst) + 1;
        LinkStatusToken {
            generation: self.clone(),
            token,
        }
    }
}

/// One refresh's claim on publishing, from [`LinkStatusGen::next`].
pub struct LinkStatusToken {
    generation: LinkStatusGen,
    token: u64,
}

impl LinkStatusToken {
    /// Post `fields` on record `name` under `mask`, unless a later token was
    /// issued — decided under the record write lock the post takes.
    pub fn post<P: crate::server::database::PostItem>(
        self,
        handle: &AsyncDbHandle,
        name: &str,
        fields: Vec<P>,
        mask: crate::server::recgbl::EventMask,
    ) {
        let _ = handle.post_fields_with_mask_if(name, fields, mask, || {
            self.generation.0.load(Ordering::SeqCst) == self.token
        });
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

/// Sentinel for "no resolvable target field type", C `DBF_unknown` (-1): what
/// `dbGetLinkDBFtype` answers for an external, unresolvable or constant link.
pub(crate) const DBF_UNKNOWN: i16 = -1;

/// C `DBF_NOACCESS` (dbFldTypes.h — dbStatic index 17), the code sseq's
/// `init_record` alone stores for a constant DOL (sseqRecord.c:206); `special`
/// and `checkLinks` overwrite it with `DBF_unknown` (:1014, :881).
pub(crate) const DBF_NOACCESS: i16 = 17;

/// Which end of the record a link is attached to.
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
/// (sseqRecord.c:863-950,203-240; calcoutRecord.c:160-189).
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
pub fn classify_link(handle: &AsyncDbHandle, link: &str) -> (i16, i16) {
    match parse_link_v2(link).link_type() {
        // Empty / constant link: C → CON, no target type.
        LinkType::Empty | LinkType::Constant => (LINK_CON, DBF_UNKNOWN),
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

/// Classify a record's links and publish the resulting connection-status menu
/// fields — the shared body of C's `init_record` link loop and the synApps
/// records' `special()` re-classification (`calcoutRecord.c:160-189`,
/// `transformRecord.c:444-473` and `:709-742`, `sCalcoutRecord.c:254-288` and
/// `:508-569`, `aCalcoutRecord.c:209-243` and `:528-569`). Base calcout's
/// `special()` is not one: it asks `dbLinkIsConstant` before `dbAddLink` has
/// set the `lset` (dbAccess.c:1181-1208), so a runtime put reads Constant
/// whatever the target and nothing is classified.
///
/// `links` pairs each status FIELD with the link string it describes. Every caller passes its complete set, so a refresh is
/// always a whole-record classification, never a per-field patch — which is
/// what makes the [`LinkStatusGen`] gate sufficient to keep "only the latest
/// classification is published" true.
///
/// The work is scheduled through [`AsyncDbHandle::schedule_record_init`], the
/// database's `iocInit` owner: queued while records are still loading, so a
/// forward-referenced target classifies LOCAL and the result is final when
/// `iocInit` returns; spawned at once on a complete database. No-op for a
/// record with no async context.
/// `mask` is the record's own `db_post_events` mask, and it is a parameter
/// because the four records that route through here do not agree. The three
/// calcouts post a bare `DBE_VALUE` — `calcoutRecord.c:404`, `:752`, `:757`;
/// `sCalcoutRecord.c:287`, `:569`, `:1015`, `:1045`; `aCalcoutRecord.c:242`,
/// `:569`, `:1157` — while `transformRecord.c:741`, `:858`, `:863` post
/// `DBE_VALUE|DBE_LOG`. Posting one mask for all four would have been correct
/// for exactly one of them.
///
/// Six records own link-status fields, not four: `sseq` (DOLV/LNKV/DT/LT/WERR)
/// and `swait` (PVxV) post theirs from their own `refresh_link_status`, because
/// their field sets are not one status enum per link. Both post `DBE_VALUE`
/// alone — `sseqRecord.c:221`, `:240`, `:910`; `swaitRecord.c:527`, `:550`,
/// `:923` — so a mask change here is only half of that family.
pub fn post_link_status(
    ctx: Option<&(String, AsyncDbHandle)>,
    generation: &LinkStatusGen,
    links: Vec<(&'static str, String)>,
    mask: crate::server::recgbl::EventMask,
) {
    let Some((name, handle)) = ctx else {
        return;
    };
    let name = name.clone();
    let handle = handle.clone();
    // Stamp before the spawn, on the caller's thread, under the record's
    // write lock (see `LinkStatusGen`).
    let token = generation.next();
    let sched = handle.clone();
    let init_key = name.clone();
    sched.schedule_record_init(&init_key, async move {
        let fields: Vec<(String, EpicsValue)> = links
            .into_iter()
            .map(|(field, link)| {
                let (status, _ft) = classify_link(&handle, &link);
                (field.to_string(), EpicsValue::Enum(status as u16))
            })
            .collect();
        #[cfg(test)]
        tests::hold_before_post().await;
        token.post(&handle, &name, fields, mask);
    });
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
/// C `swaitRecord.c::init_record` (338-373) and `::special` (507-559) drive
/// this: a blank name is `NO_PV`, and any other name is set `PV_NC` and handed
/// to `recDynLinkAddInput`/`AddOutput`, after which `pvSearchCallback`
/// (901-926) flips it to `PV_OK` once the search connects and back to `PV_NC`
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use tokio::sync::oneshot;

    use crate::server::database::PvDatabase;
    use crate::server::recgbl::EventMask;
    use crate::server::records::ai::AiRecord;
    use crate::server::records::scalcout::ScalcoutRecord;
    use crate::types::{DbFieldType, EpicsValue};

    /// `(arrived, release, resumed)` for the next classification task to
    /// reach its post: it signals `arrived`, parks until `release` fires, and
    /// signals `resumed` on its way to the post.
    type Hold = (
        oneshot::Sender<()>,
        oneshot::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    );
    static HOLD: Mutex<Option<Hold>> = Mutex::new(None);

    pub(super) async fn hold_before_post() {
        let hold = HOLD.lock().unwrap().take();
        if let Some((arrived, release, resumed)) = hold {
            let _ = arrived.send(());
            let _ = release.await;
            let _ = resumed.send(());
        }
    }

    /// Boundary of the generation gate: put 1 (`INPA SRC`) has classified
    /// Local and is released while the test holds SC's write lock, so it
    /// reaches its post — and, were the token checked before the lock, its
    /// check — before put 2 (`INPA ""`) issues the next token under that same
    /// lock. Only put 2's whole Constant classification may post; a check
    /// outside the lock also posts the stale Local, in either order.
    #[epics_macros_rs::epics_test]
    async fn a_superseded_link_put_still_posts_its_status() {
        let db = PvDatabase::new();
        db.add_record("SRC", Box::new(AiRecord::new(3.0)))
            .await
            .unwrap();
        db.add_record("SC", Box::new(ScalcoutRecord::new()))
            .await
            .unwrap();
        db.ioc_init().await;
        // SC's init classification is spawned and may still be in flight; it
        // must not be the task the hold catches.
        crate::runtime::task::sleep(Duration::from_millis(100)).await;

        let rec = db.get_record("SC").unwrap();
        let mut rx: Vec<_> = ["INAV", "INBV", "INCV"]
            .iter()
            .map(|f| {
                rec.write()
                    .add_subscriber(f, 1, DbFieldType::Enum, EventMask::VALUE.bits())
                    .expect("subscriber")
            })
            .collect();

        let (arrived_tx, arrived) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        let (resumed_tx, resumed) = std::sync::mpsc::channel();
        *HOLD.lock().unwrap() = Some((arrived_tx, release_rx, resumed_tx));
        let src = EpicsValue::String("SRC".into());
        let held = db.lock_for_put("SC", "INPA", &src);
        db.put_record_field_from_ca_already_locked(held, src)
            .unwrap();
        arrived.await.expect("put 1's task reached its post");
        {
            let mut guard = rec.write();
            release.send(()).unwrap();
            resumed
                .recv_timeout(Duration::from_secs(5))
                .expect("put 1's task resumed");
            // The few instructions from the hold to the post's lock.
            std::thread::sleep(Duration::from_millis(50));
            // Put 2, as the put path runs it under the record's hold.
            guard
                .record
                .put_field("INPA", EpicsValue::String("".into()))
                .unwrap();
            guard.record.special("INPA", true).unwrap();
        }
        crate::runtime::task::sleep(Duration::from_millis(100)).await;

        let mut drain = |i: usize| {
            let mut got = Vec::new();
            while let Ok(ev) = rx[i].try_recv() {
                got.push(ev.snapshot.value.clone());
            }
            got
        };
        let constant = vec![EpicsValue::Enum(super::LINK_CON as u16)];
        assert_eq!(drain(0), constant, "INAV");
        assert_eq!(drain(1), constant, "INBV");
        assert_eq!(drain(2), constant, "INCV");
    }
}
