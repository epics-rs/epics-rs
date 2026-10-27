//! Shared link-connection-status classification.
//!
//! Several records expose a `menu(...)` field per link that mirrors the C
//! `checkLinks` / `init_record` connection diagnostics. `sseq`
//! (`menu(sseqLNKV)`, `DOLnV`/`LNKnV`) and `calcout` (`menu(calcoutINAV)`,
//! `INAV`..`INUV`/`OUTV`) carry the identical four-choice menu, so the
//! choice table, the menu indices and the `classify_link` helper live here
//! once rather than being duplicated per record (C `sseqRecord.dbd:20` and
//! `calcoutRecord.dbd.pod:45-50` are byte-for-byte the same choice set). The
//! classification rule is not shared: see [`LinkRule`].

use std::cell::Cell;
use std::sync::Arc;

use crate::server::database::{AsyncDbHandle, PostItem, PostRule};
use crate::types::{DbFieldType, EpicsValue, PvString};

/// Generation gate for spawned async link-status refreshes, one generation
/// per link.
///
/// A record's `refresh_link_status` snapshots its link strings and
/// classifies them off-thread before posting the result. Two refreshes can
/// race — e.g. an init-time refresh (links still empty → `CON`) finishing
/// *after* a runtime re-point (`special()` of a `DOLn`/`LNKn`/`INPn` link →
/// `LOC`) — and the stale task could clobber the newer classification when it
/// posts last. The invariant *only the latest classification of a link may be
/// published* holds by construction: each spawn takes a [`LinkStatusToken`]
/// for the links it classifies, and the token's posts are the only way to
/// publish. A put re-points one link, so its token supersedes only that
/// link's: C's `special()` classifies the link that was put and leaves every
/// other status as it stands (`sCalcoutRecord.c:505-569`,
/// `throttleRecord.c:341-374`). Tokens are issued under the record instance's
/// write lock and checked under the same lock the post takes, so no put can
/// issue a newer token between the check and the post.
///
/// C posts every status it classifies, changed or not. A link's refresh is
/// superseded only by a later one of the same link, which posts it, so a
/// superseded status owes nothing.
#[derive(Clone, Default)]
pub struct LinkStatusGen(Arc<parking_lot::Mutex<GenState>>);

#[derive(Default)]
struct GenState {
    /// The last stamp issued.
    stamp: u64,
    /// The stamp of the last token over every link.
    all: u64,
    /// `links[i]`: the stamp of the last token naming link `i` alone.
    links: Vec<u64>,
}

impl GenState {
    fn issue(&mut self) -> u64 {
        self.stamp += 1;
        self.stamp
    }

    /// The links of `links` whose latest token is `stamp`.
    fn current(&self, stamp: u64, links: u64) -> u64 {
        (0..64)
            .filter(|&i| links & 1 << i != 0)
            .filter(|&i| self.all.max(self.links.get(i).copied().unwrap_or(0)) == stamp)
            .fold(0, |cur, i| cur | 1 << i)
    }
}

impl LinkStatusGen {
    /// Issue the token for a newly spawned refresh of EVERY link, superseding
    /// any token already in flight. Call on the issuing thread, under the
    /// record write lock, before the spawn.
    pub fn next(&self) -> LinkStatusToken {
        let mut state = self.0.lock();
        let stamp = state.issue();
        state.all = stamp;
        LinkStatusToken {
            generation: self.clone(),
            stamp,
            links: u64::MAX,
        }
    }

    /// [`next`](Self::next) for the links at bits `links` — link `i` of the
    /// caller's complete set at bit `i` — superseding only their tokens.
    pub fn next_for(&self, links: u64) -> LinkStatusToken {
        let mut state = self.0.lock();
        let stamp = state.issue();
        for i in (0..64).filter(|&i| links & 1 << i != 0) {
            if state.links.len() <= i {
                state.links.resize(i + 1, 0);
            }
            state.links[i] = stamp;
        }
        LinkStatusToken {
            generation: self.clone(),
            stamp,
            links,
        }
    }
}

/// One refresh's claim on publishing, from [`LinkStatusGen::next`] or
/// [`LinkStatusGen::next_for`].
pub struct LinkStatusToken {
    generation: LinkStatusGen,
    stamp: u64,
    links: u64,
}

impl LinkStatusToken {
    /// Post `fields` on record `name` under `mask`, unless a later token was
    /// issued for any of its links — decided under the record write lock the
    /// post takes.
    pub fn post<P: crate::server::database::PostItem>(
        self,
        handle: &AsyncDbHandle,
        name: &[u8],
        fields: Vec<P>,
        mask: crate::server::recgbl::EventMask,
    ) {
        let _ = handle.post_fields_with_mask_if(name, fields, mask, || {
            let state = self.generation.0.lock();
            state.current(self.stamp, self.links) == self.links
        });
    }

    /// Post the status of each link the token names, `(i, field, value)` for
    /// link `i`, whatever its value: a link a later token was issued for posts
    /// nothing — decided under the record write lock the post takes.
    pub fn post_each(
        self,
        handle: &AsyncDbHandle,
        name: &[u8],
        fields: Vec<(usize, String, EpicsValue)>,
        mask: crate::server::recgbl::EventMask,
    ) {
        let fields = fields
            .into_iter()
            .map(|(i, field, value)| (i, field, value, PostRule::Always))
            .collect();
        self.post_each_ruled(handle, name, fields, mask);
    }

    /// [`Self::post_each`] with each field's own rule, for bit `i` of
    /// `(i, field, value, rule)`.
    pub fn post_each_ruled(
        self,
        handle: &AsyncDbHandle,
        name: &[u8],
        fields: Vec<(usize, String, EpicsValue, PostRule)>,
        mask: crate::server::recgbl::EventMask,
    ) {
        let admitted = Cell::new(0u64);
        let items: Vec<LinkPost<'_>> = fields
            .into_iter()
            .map(|(i, field, value, rule)| LinkPost {
                field,
                value,
                rule,
                bit: 1 << i,
                admitted: &admitted,
            })
            .collect();
        let _ = handle.post_fields_with_mask_if(name, items, mask, || {
            let current = self.generation.0.lock().current(self.stamp, self.links);
            admitted.set(current);
            current != 0
        });
    }
}

/// One field of [`LinkStatusToken::post_each`]; its rule reads the links the
/// admitting check took, which `post_fields_with_mask_if` asks first.
struct LinkPost<'a> {
    field: String,
    value: EpicsValue,
    rule: PostRule,
    bit: u64,
    admitted: &'a Cell<u64>,
}

impl PostItem for LinkPost<'_> {
    fn field(&self) -> &str {
        &self.field
    }
    fn value(&self) -> &EpicsValue {
        &self.value
    }
    fn rule(&self) -> PostRule {
        if self.admitted.get() & self.bit != 0 {
            self.rule
        } else {
            PostRule::Skip
        }
    }
    fn into_parts(self) -> (String, EpicsValue) {
        (self.field, self.value)
    }
}

/// Choice labels for the link-connection-status menu, in index order.
/// C `menu(sseqLNKV)` (sseqRecord.dbd:20) and `menu(calcoutINAV)`
/// (calcoutRecord.dbd.pod:45-50): 0=Ext PV NC, 1=Ext PV OK, 2=Local PV,
/// 3=Constant.
pub const LINK_STATUS_CHOICES: &[&str] = &["Ext PV NC", "Ext PV OK", "Local PV", "Constant"];

/// Link-status menu indices. A record reading the status back must treat
/// BOTH external indices as C's `CA_LINK` (see [`link_is_external`]).
pub const LINK_EXT_NC: i16 = 0; // external PV, not connected
pub const LINK_EXT_OK: i16 = 1; // external PV, connected
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
pub(crate) fn dbf_static_code(ft: DbFieldType) -> i16 {
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

/// What a record's `init_record` makes of a link that names a PV, and which
/// links its `checkLinks` re-reads. The C records do not agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkRule {
    /// Base `calcoutRecord.c:169-189`, `:744-760`: `dbLinkIsVolatile` — every
    /// CA or PVA link reports its `dbIsLinkConnected`, a local name included
    /// when the link carries `CA`/`CP`/`CPP`. `init_record` schedules no
    /// re-check; `process()` does.
    Volatile,
    /// synApps `sCalcoutRecord.c:264`, `aCalcoutRecord.c:219`,
    /// `transformRecord.c:450`, std `throttleRecord.c:194`, and sseq's
    /// `special()` (`sseqRecord.c:1006`, `:1051`): `dbNameToAddr` of the PV
    /// name — a name this IOC holds is `Local PV` whatever its modifier, and
    /// `checkLinks` re-reads only the CA links left. An `Ext PV NC` re-checks
    /// after 1 s from `init_record`, 0.5 s from `special()`.
    Locality,
    /// sseq's `init_record` (`sseqRecord.c:208`, `:227`): only a DB link to a
    /// local name is `Local PV`; a CA link is external.
    DbLink,
}

impl LinkRule {
    /// Whether this rule's `checkLinks` visits `link`, the field's link after
    /// `dbInitLink`: `dbLinkIsVolatile` for base, `plink->type == CA_LINK`
    /// for synApps.
    pub(crate) fn visits(self, link: &crate::server::record::ParsedLink) -> bool {
        match self {
            Self::Volatile => link.external().is_some(),
            Self::Locality | Self::DbLink => {
                link.link_type_after_init() == crate::server::record::DbLinkType::CaLink
            }
        }
    }

    /// The `callbackRequestDelayed` `init_record` makes for an `Ext PV NC`.
    pub fn init_recheck(self) -> Option<std::time::Duration> {
        match self {
            Self::Volatile => None,
            Self::Locality | Self::DbLink => Some(std::time::Duration::from_secs(1)),
        }
    }
}

/// C `checkLinksCallback`'s re-arm, and synApps `special()`'s first arm.
pub const LINK_RECHECK: std::time::Duration = std::time::Duration::from_millis(500);

/// A type's `checkLinks`: link field `i` of `links` and its status field.
pub struct LinkCheckSpec {
    /// In C's `plink++`/`plinkValid++` order.
    pub links: &'static [(&'static str, &'static str)],
    pub rule: LinkRule,
    /// The record's own `db_post_events` mask for a status.
    pub mask: crate::server::recgbl::EventMask,
    /// sseq's `checkLinks` sets and re-posts `Local PV` on every DB link each
    /// pass (`sseqRecord.c:874`, `:910`); the calcs leave one alone.
    pub repost_local: bool,
    /// The record's `process()` runs `checkLinks` (`calcoutRecord.c:234`), so
    /// it takes `CycleHooks::LINK_CHECK`; sseq's runs it only from its
    /// callback.
    pub in_cycle: bool,
}

/// C's `caLinkStat != NO_CA_LINKS` and `cbScheduled`.
#[derive(Default)]
pub struct LinkCheckState {
    pub(crate) ca_links: bool,
    pub(crate) scheduled: bool,
}

/// What `checkLinks` reads and writes on one record: the status of
/// `spec.links[i]` at `statuses[i]`.
pub struct LinkCheck<'a> {
    pub spec: &'static LinkCheckSpec,
    pub statuses: &'a mut [i16],
    /// The `dbGetLinkDBFtype` of link `i`, written silently, for a record
    /// that keeps one (sseq `DTn`/`LTn`); empty otherwise.
    pub field_types: &'a mut [i16],
    pub state: &'a mut LinkCheckState,
    pub tail: Option<&'a mut dyn LinkCheckTail>,
}

/// The rest of a record's `checkLinks` pass once the links are read: sseq's
/// `waitConfigErr` (`sseqRecord.c:912-933`).
pub trait LinkCheckTail {
    /// Recompute from the links the pass found to be a `DB_LINK`, bit `i` for
    /// `spec.links[i]`; bit `i` of the result set means [`Self::fields`]`[i]`
    /// moved and posts.
    fn pass(&mut self, db_links: u64) -> u64;
    fn fields(&self) -> &'static [&'static str];
}

/// Classify one DOL/LNK/INP/OUT link string into its connection-status menu
/// index and the target field type, as `init_record` and `special()` do
/// under `rule` (sseqRecord.c:203-240; calcoutRecord.c:160-189;
/// sCalcoutRecord.c:254-285). `role` parses it as its field does: an output
/// drops `CP`/`CPP`, so `OUT "X CP"` is a DB link.
///
/// The database this reads is always the COMPLETE one: a classification issued
/// while records are still being created is queued by
/// [`AsyncDbHandle::schedule_record_init`] and only polled by `iocInit`, so a
/// forward reference — within one `.db` or across two `dbLoadRecords` calls —
/// resolves LOCAL, exactly as C's `iocInit`-time `init_record` does.
pub fn classify_link(
    handle: &AsyncDbHandle,
    link: impl AsRef<[u8]>,
    role: LinkRole,
    rule: LinkRule,
) -> (i16, i16) {
    use crate::server::record::{LinkFieldType, ParsedLink, parse_link_field};
    let ftype = match role {
        LinkRole::Input => LinkFieldType::In,
        LinkRole::Output => LinkFieldType::Out,
    };
    let parsed = parse_link_field(link.as_ref(), ftype);
    // Base calcout asks `dbLinkIsConstant`; synApps and sseq ask
    // `type == CONSTANT` and send a `{const:…}` JSON link to `dbNameToAddr`
    // of its `pv_link.pvname`, a union member a JSON link never set. That C
    // defect is not copied: a constant lset is constant.
    if crate::server::recgbl::simm::is_constant(&parsed) {
        return (LINK_CON, DBF_UNKNOWN);
    }
    // Base calcout asks the lset (`dbLink.c:220-235`): a hardware link has
    // none, so it is constant; a state link's is neither constant nor
    // volatile (`lnkState.c:213-214`), so it is local.
    if rule == LinkRule::Volatile {
        match &parsed {
            ParsedLink::Hw(_) => return (LINK_CON, DBF_UNKNOWN),
            ParsedLink::State(_) => return (LINK_LOC, DBF_UNKNOWN),
            _ => {}
        }
    }
    let local = match &parsed {
        ParsedLink::Db(_) => handle.parsed_link_target_field_type(&parsed),
        // `dbNameToAddr(pvname)`, the modifier aside.
        ParsedLink::Ca(ca) if rule == LinkRule::Locality => {
            handle.link_target_field_type(ca.pv.as_bytes())
        }
        _ => None,
    };
    // An external link is never connected here: `init_record` runs while
    // dbCaTask is paused until `dbCaRun` (dbCa.c:1173, iocInit.c:260), and
    // `special()` does not ask. Only `checkLinks` reads the connection.
    match local {
        Some(dbf) => (LINK_LOC, dbf as i16),
        None => (LINK_EXT_NC, DBF_UNKNOWN),
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
/// `links` pairs each status FIELD with the link string it describes and the
/// link's role, classified under `rule`; every caller passes its complete
/// set, link `i` at index `i`. `posts` names the links C classifies, and
/// posts even unchanged: `init_record` every one, `special()` the re-pointed
/// link alone (`aCalcoutRecord.c:528-569`). The other statuses are left as
/// they stand — an `Ext PV OK` that `checkLinks` read stays OK.
///
/// The work is scheduled through [`AsyncDbHandle::schedule_record_init`], the
/// database's `iocInit` owner: queued while records are still loading, so a
/// forward-referenced target classifies LOCAL and the result is final when
/// `iocInit` returns; spawned at once on a complete database. No-op for a
/// record with no async context.
///
/// A put on a complete database is the exception: C's `special()` sets the
/// status before the put returns, and acalcout's next `fetch_values` reads
/// INAA..INLL by it (`aCalcoutRecord.c:1078`). So the put link is classified
/// here, on the caller's thread under the record's write lock, and its status
/// returned for the caller to store; the task only posts it. Otherwise `None`.
/// `mask` is the record's own `db_post_events` mask, and it is a parameter
/// because the four records that route through here do not agree. The three
/// calcouts post a bare `DBE_VALUE` — `calcoutRecord.c:404`, `:752`, `:757`;
/// `sCalcoutRecord.c:287`, `:569`, `:1015`, `:1045`; `aCalcoutRecord.c:242`,
/// `:569`, `:1157` — while `transformRecord.c:741`, `:858`, `:863` post
/// `DBE_VALUE|DBE_LOG`. Posting one mask for all four would have been correct
/// for exactly one of them.
///
/// Six records own link-status fields, not four: `sseq` (DOLV/LNKV/DT/LT/WERR)
/// posts its own from `refresh_link_status` and `swait` (PVxV) from its
/// `recDynLink` callbacks, because their field sets are not one status enum
/// per link. Both post `DBE_VALUE` alone — `sseqRecord.c:221`, `:240`, `:910`;
/// `swaitRecord.c:527`, `:550`, `:923` — so a mask change here is only half of
/// that family.
pub fn post_link_status(
    ctx: Option<&(crate::types::RecordName, AsyncDbHandle)>,
    generation: &LinkStatusGen,
    links: Vec<(&'static str, PvString, LinkRole)>,
    rule: LinkRule,
    mask: crate::server::recgbl::EventMask,
    posts: LinkStatusPosts,
) -> Option<i16> {
    let (name, handle) = ctx?;
    let classified = posts.links(links.iter().map(|&(f, _, _)| f));
    if classified == 0 {
        return None;
    }
    let recheck = match posts {
        LinkStatusPosts::All => rule.init_recheck(),
        LinkStatusPosts::Put(_) => Some(LINK_RECHECK),
    };
    // Stamp before the spawn, on the caller's thread, under the record's
    // write lock (see `LinkStatusGen`).
    let token = generation.next_for(classified);
    let now = (matches!(posts, LinkStatusPosts::Put(_)) && handle.classifies_links_at_once())
        .then(|| classify_links(handle, &links, classified, rule));
    let stored = now.as_ref().and_then(|now| now.first()).map(|&(_, _, s)| s);
    let name = name.clone();
    let handle = handle.clone();
    let sched = handle.clone();
    let init_key = name.clone();
    sched.schedule_record_init(&init_key, async move {
        let (statuses, post) = match now {
            Some(now) => (now, PostRule::Held),
            None => (
                classify_links(&handle, &links, classified, rule),
                PostRule::Always,
            ),
        };
        let fields = statuses
            .into_iter()
            .map(|(i, field, status)| (i, field.to_string(), EpicsValue::Enum(status as u16), post))
            .collect();
        #[cfg(test)]
        tests::hold_before_post().await;
        token.post_each_ruled(&handle, &name, fields, mask);
        handle.link_check_classified(&name, recheck);
    });
    stored
}

/// `(i, field, status)` for each link `i` of `links` at a bit of `classified`.
fn classify_links(
    handle: &AsyncDbHandle,
    links: &[(&'static str, PvString, LinkRole)],
    classified: u64,
    rule: LinkRule,
) -> Vec<(usize, &'static str, i16)> {
    links
        .iter()
        .enumerate()
        .filter(|&(i, _)| classified & 1 << i != 0)
        .map(|(i, (field, link, role))| (i, *field, classify_link(handle, link, *role, rule).0))
        .collect()
}

/// Which links a link-status refresh classifies; C posts each status it
/// classified whatever its value.
#[derive(Clone, Copy)]
pub enum LinkStatusPosts {
    /// `init_record`: every one.
    All,
    /// `special()` of a link put: the link whose status field this is.
    Put(&'static str),
}

impl LinkStatusPosts {
    /// Bit `i` for the `i`th of `fields`, the caller's complete set in
    /// posting order.
    pub fn links<'a>(self, fields: impl Iterator<Item = &'a str>) -> u64 {
        let mut fields = fields.enumerate();
        match self {
            Self::All => fields.fold(0, |all, (i, _)| all | 1 << i),
            Self::Put(field) => fields.find(|&(_, f)| f == field).map_or(0, |(i, _)| 1 << i),
        }
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use tokio::sync::oneshot;

    use crate::server::database::{ProcStack, PvDatabase};
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

    /// Put 1 is held at its post until the test holds SC's write lock, then
    /// released so it reaches its post — and its under-lock check — just as
    /// put 2 issues the next token under that same lock. Returns what INAV,
    /// INBV and INCV posted.
    async fn superseded_put(put1: (&str, &str), put2: (&str, &str)) -> [Vec<EpicsValue>; 3] {
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
        let v1 = EpicsValue::String(put1.1.into());
        let held = db.lock_for_put("SC", put1.0, &v1);
        db.put_record_field_from_ca_already_locked(held, v1)
            .unwrap();
        arrived.await.expect("put 1's task reached its post");
        {
            let mut guard = rec.write();
            release.send(()).unwrap();
            resumed
                .recv_timeout(fact_budget::FACT_BUDGET)
                .expect("put 1's task resumed");
            // The few instructions from the hold to the post's lock.
            std::thread::sleep(Duration::from_millis(50));
            // Put 2, as the put path runs it under the record's hold.
            guard
                .record
                .put_field(put2.0, EpicsValue::String(put2.1.into()))
                .unwrap();
            guard
                .record
                .special(put2.0, true, &Default::default())
                .unwrap();
        }
        crate::runtime::task::sleep(Duration::from_millis(100)).await;

        let mut drain = |i: usize| {
            let mut got = Vec::new();
            while let Ok(ev) = rx[i].try_recv() {
                got.push(ev.value().as_value().cloned().unwrap());
            }
            got
        };
        [drain(0), drain(1), drain(2)]
    }

    /// Boundary of the generation gate: put 1 (`INPA SRC`) classified Local
    /// but is superseded by put 2 (`INPA ""`). Only put 2's Constant may post;
    /// a check outside the lock also posts the stale Local, in either order.
    /// The other links' statuses post nothing, as C's `special()`.
    #[epics_macros_rs::epics_test]
    async fn a_superseded_link_put_still_posts_its_status() {
        let constant = vec![EpicsValue::Enum(super::LINK_CON as u16)];
        let got = superseded_put(("INPA", "SRC"), ("INPA", "")).await;
        assert_eq!(got, [constant, vec![], vec![]]);
    }

    /// Boundary of the per-link gate: put 1 (`INPA ""`, Constant, unchanged)
    /// is followed by put 2 on another link, which leaves INAV alone. C posts
    /// INAV from put 1's `special()` whatever its value.
    #[epics_macros_rs::epics_test]
    async fn an_unchanged_put_posts_past_a_put_to_another_link() {
        let constant = vec![EpicsValue::Enum(super::LINK_CON as u16)];
        let local = vec![EpicsValue::Enum(super::LINK_LOC as u16)];
        let got = superseded_put(("INPA", ""), ("INPB", "SRC")).await;
        assert_eq!(got, [constant, local, vec![]]);
    }

    /// Boundary of the per-link gate on a whole-record refresh: SC's
    /// `init_record` classification is held while a put re-points INPA. The
    /// put's Constant is INAV's only post; init still posts every other
    /// status it classified, INCV's unchanged Constant included.
    #[epics_macros_rs::epics_test]
    async fn a_put_supersedes_only_its_own_link_of_an_init_refresh() {
        use crate::server::record::Record;
        let db = PvDatabase::new();
        db.add_record("SRC", Box::new(AiRecord::new(3.0)))
            .await
            .unwrap();
        db.ioc_init().await;
        let (arrived_tx, arrived) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        let (resumed_tx, resumed) = std::sync::mpsc::channel();
        *HOLD.lock().unwrap() = Some((arrived_tx, release_rx, resumed_tx));
        let mut sc = ScalcoutRecord::new();
        for link in ["INPA", "INPB"] {
            sc.put_field(link, EpicsValue::String("SRC".into()))
                .unwrap();
        }
        // `add_record` returns only once init has posted, so the put below
        // comes from another thread, as a CA client's would.
        let adder = db.clone();
        let added = crate::runtime::task::spawn_background(
            crate::runtime::task::CallbackPriority::Medium,
            async move { adder.add_record("SC", Box::new(sc)).await.unwrap() },
        );
        arrived.await.expect("init's task reached its post");

        let rec = db.get_record("SC").unwrap();
        let mut rx: Vec<_> = ["INAV", "INBV", "INCV"]
            .iter()
            .map(|f| {
                rec.write()
                    .add_subscriber(f, 1, DbFieldType::Enum, EventMask::VALUE.bits())
                    .expect("subscriber")
            })
            .collect();
        {
            let mut guard = rec.write();
            release.send(()).unwrap();
            resumed
                .recv_timeout(fact_budget::FACT_BUDGET)
                .expect("init's task resumed");
            std::thread::sleep(Duration::from_millis(50));
            guard
                .record
                .put_field("INPA", EpicsValue::String("".into()))
                .unwrap();
            guard
                .record
                .special("INPA", true, &Default::default())
                .unwrap();
        }
        added.await.expect("add_record returned");
        crate::runtime::task::sleep(Duration::from_millis(100)).await;

        let mut drain = |i: usize| {
            let mut got = Vec::new();
            while let Ok(ev) = rx[i].try_recv() {
                got.push(ev.value().as_value().cloned().unwrap());
            }
            got
        };
        let constant = vec![EpicsValue::Enum(super::LINK_CON as u16)];
        let local = vec![EpicsValue::Enum(super::LINK_LOC as u16)];
        assert_eq!(
            [drain(0), drain(1), drain(2)],
            [constant.clone(), local, constant]
        );
    }

    /// C `special()` stores a re-pointed link's status before the put
    /// returns, and acalcout reads INAA..INLL only while that status is EXT or
    /// LOC (`aCalcoutRecord.c:1078`). With the put's post still in flight, a
    /// process right after an INAA put reads the new link.
    #[epics_macros_rs::epics_test]
    async fn a_process_right_after_an_array_link_put_reads_it() {
        use crate::server::record::Record;
        use crate::server::records::acalcout::AcalcoutRecord;
        let db = PvDatabase::new();
        db.add_record("SRC", Box::new(AiRecord::new(3.0)))
            .await
            .unwrap();
        let mut ac = AcalcoutRecord::new();
        ac.put_field("NELM", EpicsValue::Long(4)).unwrap();
        db.add_record("AC", Box::new(ac)).await.unwrap();
        db.ioc_init().await;
        // AC's init classification must not be the task the hold catches.
        crate::runtime::task::sleep(Duration::from_millis(100)).await;

        let (arrived_tx, arrived) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        let (resumed_tx, resumed) = std::sync::mpsc::channel();
        *HOLD.lock().unwrap() = Some((arrived_tx, release_rx, resumed_tx));
        let link = EpicsValue::String("SRC".into());
        let held = db.lock_for_put("AC", "INAA", &link);
        db.put_record_field_from_ca_already_locked(held, link)
            .unwrap();
        arrived.await.expect("the put's task reached its post");

        let rec = db.get_record("AC").unwrap();
        let read = |f: &str| rec.read().record.get_field(f);
        assert_eq!(read("IAAV"), Some(EpicsValue::Enum(super::LINK_LOC as u16)));
        db.process_record_with_links("AC", &mut ProcStack::new())
            .await
            .unwrap();
        let aa = match read("AA") {
            Some(EpicsValue::DoubleArray(a)) => a.to_vec(),
            other => panic!("AA is {other:?}"),
        };
        assert_eq!(aa.first(), Some(&3.0), "AA is {aa:?}");

        release.send(()).unwrap();
        resumed
            .recv_timeout(fact_budget::FACT_BUDGET)
            .expect("the put's task resumed");
    }

    /// C `init_record` classifies every link before any process
    /// (`aCalcoutRecord.c:209-243`). A database built without `iocInit` has
    /// no barrier but `add_record`, so the first process after it reads
    /// INAA. The hold is released only once the add has had its chance to
    /// return.
    #[epics_macros_rs::epics_test]
    async fn a_process_right_after_add_record_reads_an_array_link() {
        use crate::server::record::Record;
        use crate::server::records::acalcout::AcalcoutRecord;
        let db = PvDatabase::new();
        db.add_record("SRC", Box::new(AiRecord::new(3.0)))
            .await
            .unwrap();
        let mut ac = AcalcoutRecord::new();
        ac.put_field("NELM", EpicsValue::Long(4)).unwrap();
        ac.put_field("INAA", EpicsValue::String("SRC".into()))
            .unwrap();

        let (arrived_tx, arrived) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        let (resumed_tx, _resumed) = std::sync::mpsc::channel();
        *HOLD.lock().unwrap() = Some((arrived_tx, release_rx, resumed_tx));
        let add = async {
            db.add_record("AC", Box::new(ac)).await.unwrap();
            let rec = db.get_record("AC").unwrap();
            let read = |f: &str| rec.read().record.get_field(f);
            assert_eq!(read("IAAV"), Some(EpicsValue::Enum(super::LINK_LOC as u16)));
            db.process_record_with_links("AC", &mut ProcStack::new())
                .await
                .unwrap();
            let aa = match read("AA") {
                Some(EpicsValue::DoubleArray(a)) => a.to_vec(),
                other => panic!("AA is {other:?}"),
            };
            assert_eq!(aa.first(), Some(&3.0), "AA is {aa:?}");
        };
        let unhold = async {
            arrived.await.expect("AC's init reached its post");
            release.send(()).unwrap();
        };
        tokio::join!(add, unhold);
    }
}
