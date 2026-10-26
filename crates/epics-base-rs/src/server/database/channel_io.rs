//! C `db_access.c`: the channel layer between a CA-shaped consumer and the
//! database — channel create (`dbChannel_create`, `db_access.c:105-118`),
//! the DBR read (`dbChannel_get`), the put (`dbChannel_put` /
//! `db_put_process`) and the subscription install the IO objects make
//! (`db_add_event` + `db_post_single_event`).
//!
//! Two consumers, as in C: rsrv (`camessage.c`), and the in-process CA link
//! channel (`dbContext` / `dbChannelIO`), which serves a local name with no
//! network at all.

use std::sync::Arc;

use crate::server::database::filters::{ChannelCreateError, FilterChain};
use crate::server::database::{PvDatabase, PvEntry};
use crate::server::event_queue::{DeliveryContext, EventReader, EventUser};
use crate::server::pv::{ProcessVariable, PutStarted, PvWrite, SubscribeRefused};
use crate::server::record::RecordCell;
use crate::server::record::{ProcessCompletion, PutRefused};
use crate::types::{DbFieldType, EpicsValue};

/// What a database channel addresses: a simple PV, or one field of a record.
#[derive(Clone)]
pub enum ChannelTarget {
    SimplePv(Arc<ProcessVariable>),
    RecordField {
        record: Arc<RecordCell>,
        field: Arc<str>,
    },
}

impl ChannelTarget {
    /// C rsrv's `RECORD_NAME(pciu->dbch)` (`camessage.c:56`): the diagnostic
    /// every channel-scoped `send_err` carries, and what libca prints as the
    /// exception block's `ctx=`. It is the record's name, never `REC.FIELD`.
    /// Read without the record's lock set.
    pub fn rsrv_record_name(&self) -> &[u8] {
        match self {
            ChannelTarget::SimplePv(pv) => pv.name.as_bytes(),
            ChannelTarget::RecordField { record, .. } => record.name(),
        }
    }

    /// The name the audit log and the trap-write listeners record: `REC.FIELD`.
    pub fn audit_name(&self) -> Vec<u8> {
        match self {
            ChannelTarget::SimplePv(pv) => pv.name.to_vec(),
            ChannelTarget::RecordField { record, field } => {
                [&record.name()[..], b".", field.as_bytes()].concat()
            }
        }
    }

    /// Whether the database object this channel serves has been destroyed.
    ///
    /// A channel holds an `Arc` to its target, so removal from the database
    /// map does not end it — the object stays alive and reachable for as
    /// long as the channel does. Asking the target itself is what makes the
    /// sweep exact: it is object identity, not a name comparison, so an
    /// alias, a `.FIELD` suffix or a `.{filter}` suffix on the name the
    /// client opened the channel with cannot make it miss.
    pub fn is_destroyed(&self) -> bool {
        match self {
            ChannelTarget::SimplePv(pv) => pv.is_destroyed(),
            ChannelTarget::RecordField { record, .. } => record.read().is_destroyed(),
        }
    }
}

/// How a channel presents a long-string field on the CA wire. `$`-access
/// and plain access to a long-string *record* field are mutually
/// exclusive boundary conversions, so they share one mode rather than two
/// booleans — the illegal "both at once" state cannot be constructed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LongStringMode {
    /// Ordinary field: deliver the value verbatim.
    Plain,
    /// Client appended `$`: the field is delivered as C's `DBR_CHAR`
    /// address of it, `no_elements` long (`dbAccess.c:682-701`).
    DollarChar {
        no_elements: u32,
        nul_at_count: bool,
        /// The `DBR_CHAR` image the channel's post chain runs on.
        view: crate::server::database::char_view::DollarView,
    },
    /// Plain access to a long-string *record* field (lsi/lso VAL & OVAL,
    /// printf VAL): C `cvt_dbaddr` presents it as a scalar `DBF_STRING`,
    /// so the CHAR-array carrier is decoded to a scalar string before
    /// encoding.
    NativeString,
    /// Plain access to a link or an attribute, which `dbGet` reads as text
    /// for every request type (`dbAccess.c:944-952`).
    Text(crate::server::database::char_view::TextRoute),
}

/// An open database channel: C's `dbChannel` after `dbChannelOpen`.
pub struct DbAccessChannel {
    pub target: ChannelTarget,
    /// The raw `.{...}` filter suffix, re-parsed per consumer because the
    /// filters are stateful.
    pub filter_suffix: Option<String>,
    /// `dbChannelFinalCAType`.
    pub native_type: DbFieldType,
    /// `dbChannelFinalElements`.
    pub final_element_count: u32,
    /// `paddr->no_elements`, what an autosize request reads.
    pub field_element_count: u32,
    pub long_string_mode: LongStringMode,
}

impl DbAccessChannel {
    /// A fresh filter chain for one consumer of this channel: the stateful
    /// filters (`dbnd`, `dec`, `sync`) must not share state between a read
    /// and a subscription, nor between two subscriptions.
    pub fn filter_chain(&self) -> FilterChain {
        filter_chain_of(self.filter_suffix.as_deref())
    }
}

/// The chain a channel's stored `.{...}` suffix parses to. The suffix was
/// validated at [`channel_create`], so the empty fallback is unreachable.
pub fn filter_chain_of(suffix: Option<&str>) -> FilterChain {
    match suffix {
        Some(json) => {
            crate::server::database::filters::try_parse_filter_chain(json).unwrap_or_default()
        }
        None => FilterChain::new(),
    }
}

/// Why [`channel_create`] returned no channel.
#[derive(Debug)]
pub enum CreateRefusal {
    /// No record or PV of that name (`dbChannelCreate`'s `dbNameToAddr`
    /// lookup).
    NotFound,
    /// `dbChannelCreate` refused the field, `$` or filter suffix.
    Channel(ChannelCreateError),
    /// The field exists but this channel layer cannot export it.
    Refused,
}

/// C `dbChannel_create` (`db_access.c:105-118`): `dbChannelCreate`, the
/// export-type check and `dbChannelOpen`. `peer` is the requesting
/// client's address, for the gateway's host-scoped admission; an in-process
/// caller has none.
pub async fn channel_create(
    db: &PvDatabase,
    pv_name: &[u8],
    peer: Option<std::net::SocketAddr>,
) -> Result<DbAccessChannel, CreateRefusal> {
    // epics-base 3.15.7 channel-filter suffix
    // (`REC.{"dbnd":{"d":0.5}}`). Split the JSON suffix off
    // for the record lookup, but keep `pv_name` verbatim so
    // the audit log and `ChannelCreated`/`ChannelCleared`
    // events still surface the literal string the client
    // used. `filter_suffix` is stashed on the channel so
    // EVENT_ADD can build a `FilterChain` from it later.
    //
    // The `$` long-string modifier (C dbChannel.c:486-505) is peeled
    // there too, after the suffix and before the `record.FIELD`
    // split — including for a bare `REC$` with no explicit `.FIELD`,
    // which used to leave `$` on the record key so `find_entry_from`
    // missed the record entirely. `long_string` makes every delivery
    // path convert the value to DBR_CHAR with a NUL terminator.
    let parsed_channel = crate::server::database::filters::parse_channel_name(pv_name);
    let filter_suffix = parsed_channel.json_suffix;
    let long_string = parsed_channel.string_view;
    let record_path = parsed_channel.record_path.as_slice();
    let field = parsed_channel.field.clone();

    // thread the connection peer into the search
    // resolver so the CA gateway applies host-scoped `.pvlist`
    // `DENY FROM host` admission on CREATE_CHANNEL (parity with C
    // `pvExistTest` → `gateAs::findEntry(pvname, hostname)`).
    let Some(entry) = db.find_entry_from(record_path, peer).await else {
        return Err(CreateRefusal::NotFound);
    };

    // C `dbChannelCreate` returning NULL — a field or `$` the
    // record refuses, or a filter suffix `chf_parse` stops on —
    // is CREATE_CH_FAIL on the CA wire.
    let channel = match crate::server::database::filters::db_channel_create(db, pv_name) {
        Ok(channel) => channel,
        Err(e) => {
            tracing::debug!(
                pv = %String::from_utf8_lossy(pv_name),
                error = %e,
                "rejecting CREATE_CHAN: dbChannelCreate failed",
            );
            return Err(CreateRefusal::Channel(e));
        }
    };

    let (dbr_type, element_count, target, long_string_mode) = match entry {
        PvEntry::Simple(pv) => {
            let value = pv.get();
            // `$` long-string — C dbChannel.c:486-505 accepts the
            // modifier on DBF_STRING and on a link field
            // (`DBF_INLINK..DBF_FWDLINK`, `:494-499`, which a
            // SimplePv can never be); everything else falls to the
            // else at `:500-503` and gets S_dbLib_fieldNotFound
            // (CREATE_CH_FAIL). When it is a
            // string C overrides the channel to DBF_CHAR with
            // `no_elements = field_size` (= 40), as the RecordField
            // arm below does through `char_view`.
            if long_string && !matches!(value, EpicsValue::String(_)) {
                return Err(CreateRefusal::Refused);
            }
            let (dbr_type_val, element_count, mode) = if long_string {
                (
                    DbFieldType::Char,
                    40u32,
                    LongStringMode::DollarChar {
                        no_elements: 40,
                        nul_at_count: true,
                        view: crate::server::database::char_view::DollarView::of_string(
                            40,
                            &channel.chain,
                        ),
                    },
                )
            } else {
                (
                    value.dbr_type(),
                    value.count() as u32,
                    LongStringMode::Plain,
                )
            };
            (
                dbr_type_val,
                element_count,
                ChannelTarget::SimplePv(pv),
                mode,
            )
        }
        PvEntry::Record(rec) => {
            // `field` is upper-cased; C finds only the spelling
            // declared.
            let spelled = crate::server::database::parse_pv_name(record_path).1;
            if !crate::server::database::filters::spells_a_field(spelled) {
                return Err(CreateRefusal::Refused);
            }
            let (dbr_type_val, element_count, mode) =
                record_channel_shape(db, &rec, &field, long_string, &channel.chain)
                    .ok_or(CreateRefusal::Refused)?;
            (
                dbr_type_val,
                element_count,
                ChannelTarget::RecordField {
                    record: rec.clone(),
                    field: field.as_str().into(),
                },
                mode,
            )
        }
    };

    // The validated chain also yields the
    // filter-FINAL element count (C `dbChannelFinalElements`):
    // a count-reshaping filter (`arr` slice) shrinks how many
    // elements the channel can ever deliver, and the client
    // must learn that count so its READ / monitor request count
    // — and buffer allocation — match the filtered payload. An
    // empty / value-gating-only chain folds to the identity, so
    // unfiltered channels keep their native count unchanged.
    let field_element_count = element_count;
    let element_count = channel.chain.final_element_count(element_count as usize) as u32;
    // C claims `dbChannelFinalCAType`, the type the filters'
    // probe ends at (`camessage.c:1202`).
    let dbr_type = channel.final_field_type(dbr_type);

    Ok(DbAccessChannel {
        target,
        filter_suffix,
        native_type: dbr_type,
        final_element_count: element_count,
        field_element_count,
        long_string_mode,
    })
}

/// The native type, element count and long-string mode of a channel on
/// `rec`'s `field`, addressed as `dbChannelCreate` addresses it
/// (`dbChannel.c:470-505`): from the declaration and what the record settled
/// at load, init and each retyping `special`. C takes no lock here, so a held
/// record neither parks the create nor deadlocks a creator holding the set
/// its holder waits for. `None` is CREATE_CH_FAIL.
fn record_channel_shape(
    db: &PvDatabase,
    rec: &Arc<RecordCell>,
    field: &str,
    long_string: bool,
    chain: &FilterChain,
) -> Option<(DbFieldType, u32, LongStringMode)> {
    use crate::server::database::char_view::{CharAddr, DollarView, TextRoute};
    // `$` re-types a string or link field as a `DBR_CHAR` array and refuses
    // every other type (`dbAccess.c:682-701`).
    if long_string {
        let addr = CharAddr::of(db, rec, field)?;
        let no_elements = addr.no_elements as u32;
        let mode = LongStringMode::DollarChar {
            no_elements,
            nul_at_count: addr.nul_at_count(),
            view: DollarView::of_addr(addr, rec.form_string(), field, chain),
        };
        return Some((DbFieldType::Char, no_elements, mode));
    }
    let desc = crate::server::record::record_instance::field_desc_in(rec.rdes().fields, field);
    let Some(desc) = desc else {
        // `dbGetAttributePart`'s `DBF_STRING`, which `dbGet` reads as text.
        let attribute = !rec.declares_name(field)
            && db.has_record_type_attribute(rec.rdes().record_type, field);
        return attribute.then_some((
            DbFieldType::String,
            1,
            LongStringMode::Text(TextRoute::Attribute),
        ));
    };
    if desc.unreadable() {
        return None;
    }
    if crate::types::link_class_of(desc).is_some() {
        return Some((
            DbFieldType::String,
            1,
            LongStringMode::Text(TextRoute::Link),
        ));
    }
    if desc.long_string {
        // `cvt_dbaddr` makes it a scalar `DBF_STRING` (lsiRecord.c:141-143,
        // lsoRecord.c:183-185, printfRecord.c:415-417); only `$` reaches the
        // whole buffer.
        return Some((DbFieldType::String, 1, LongStringMode::NativeString));
    }
    let served = rec.addressed_field_type(field, None)?;
    let count = rec.resolve_addr(field)?;
    let dbr_type =
        DbFieldType::from_u16(served.ca_wire_type()).expect("ca_wire_type is a CA DBF code");
    Some((dbr_type, count, LongStringMode::Plain))
}

/// Read a record-field snapshot under the record's `dbScanLock` analogue — the
/// per-record advisory gate (L1, [`PvDatabase::lock_record`]).
///
/// `dbPutField` commits the field's VAL under the record's `parking_lot` write
/// lock, RELEASES it, and re-acquires a second write lock in the process cycle
/// that stamps `TIME` (`field_io.rs` drops the put guard before
/// `put_driven_process` re-locks). Only L1 — held by `acquire_put_gate` across
/// the whole put+process — spans both. A snapshot that takes only the record's
/// `RwLock` reads VAL and TIME atomically w.r.t. each other but NOT w.r.t. the
/// put: landing in that gap it captures the put's new VAL with the pre-put
/// TIME. C cannot produce that pair — `db_post_single_event` (the monitor
/// initial event) and `dbGet` both run under `dbScanLock` — so it observes the
/// pre-put or the post-process state and never the mix. Taking L1 here restores
/// that serialisation for every record-field snapshot read the CA server does
/// off the live record (initial monitor event, one-shot GET, access re-post);
/// the steady-state monitor stream is unaffected, since those frames are posted
/// by the process cycle itself, already under L1. The guard is `!Send` and
/// dropped before return — the read is fully synchronous.
pub fn record_field_snapshot_scan_locked(
    db: &PvDatabase,
    record: &Arc<RecordCell>,
    field: &str,
    string_view: bool,
) -> Option<crate::server::snapshot::Snapshot> {
    let _scan_lock = db.lock_instance(record);
    db.channel_snapshot_for_field(record, field, string_view)
}

pub fn get_full_snapshot(
    db: &PvDatabase,
    target: &ChannelTarget,
) -> Option<crate::server::snapshot::Snapshot> {
    match target {
        ChannelTarget::SimplePv(pv) => Some(pv.snapshot()),
        // `PvDatabase`, not the record: a link-backed field's units/precision/
        // limits live on the LINK TARGET's record, and only the database can
        // take that second lock. Holds no lock on entry, which is that call's
        // requirement.
        //
        // The `$` view is `false` on every CA delivery path on purpose. CA
        // does not collapse the char view back to a string the way pvxs does
        // (`iocsource.cpp:133-136`); it serves the `DBR_CHAR[40]` array C's
        // `dbChannelCreate` actually produced. So the view is applied AFTER
        // the snapshot, by the CA server's `apply_long_string_mode`
        // from the `LongStringMode` the channel stored, and its eligibility
        // gate already ran at CREATE_CHANNEL (CREATE_CH_FAIL, this file's
        // `parse_channel_name` call site).
        ChannelTarget::RecordField { record, field } => {
            record_field_snapshot_scan_locked(db, record, field, false)
        }
    }
}

/// Snapshot for a one-shot client GET (`CA_PROTO_READ` /
/// `CA_PROTO_READ_NOTIFY`).
///
/// Distinct from [`get_full_snapshot`] (used for monitor initial events
/// and access-rights re-posts) so only the *client GET* path consults a
/// PV's optional [`ReadHook`](crate::server::pv::ReadHook): the
/// CA gateway's no-cache mode installs that hook to forward each
/// downstream read to a fresh upstream fetch. For a PV without a read
/// hook — every record-backed and cached PV — this is exactly
/// `get_full_snapshot` wrapped in `Ok`. The `Err` propagates an upstream
/// get failure so the GET handler can answer `ECA_GETFAIL`, matching C
/// ca-gateway forwarding the read to the IOC under `-no_cache`
/// (`gateVc.cc:1361-1369`).
pub async fn get_read_snapshot(
    db: &PvDatabase,
    target: &ChannelTarget,
) -> Result<Option<crate::server::snapshot::Snapshot>, crate::error::CaError> {
    match target {
        ChannelTarget::SimplePv(pv) => pv.read_snapshot().await.map(Some),
        ChannelTarget::RecordField { record, field } => {
            Ok(record_field_snapshot_scan_locked(db, record, field, false))
        }
    }
}

/// Synchronous fast path for the one-shot GET snapshot, mirroring
/// [`get_read_snapshot`] but sans-io.
///
/// The outer `Option` answers "was the read resolvable synchronously?":
/// - `Some(inner)` — a fully local read (a record field always; a `SimplePv`
///   with no read hook, i.e. every cached / non-gateway PV). No `.await`, no
///   reactor. `inner` is the `Option<Snapshot>` the async path also yields:
///   `Some(snap)` for a value, `None` for an absent record field.
/// - `None` — a gateway `-no_cache` [`ReadHook`](crate::server::pv::ReadHook) is installed, whose upstream
///   GET is genuine network I/O, or the PV is
///   [`pending`](crate::server::pv::ProcessVariable::pending); the caller must
///   fall back to the async [`get_read_snapshot`]. Only `SimplePv` can reach
///   this.
///
/// This is the item-3 sans-io boundary: local-record READ reply production
/// never touches this crate's async runtime, while the gateway-upstream read
/// stays async and separated.
pub fn try_get_read_snapshot_local(
    db: &PvDatabase,
    target: &ChannelTarget,
) -> Option<Option<crate::server::snapshot::Snapshot>> {
    match target {
        ChannelTarget::RecordField { record, field } => {
            Some(record_field_snapshot_scan_locked(db, record, field, false))
        }
        // `read_snapshot_local` is `None` exactly when the read must wait — a
        // read hook's upstream GET or a pending PV — so it maps straight to
        // this fn's "not sync-resolvable" `None`; otherwise a `SimplePv` always
        // yields a snapshot, so its `Some(snap)` maps to `Some(Some(snap))`
        // and never collides with the async-signal `None`.
        ChannelTarget::SimplePv(pv) => pv.read_snapshot_local().map(Some),
    }
}

/// A `DBR_STSACK_STRING`'s ACKS and ACKT, which `getOptions` reads from the
/// record whatever the field log holds (`dbAccess.c:361-362`).
pub fn read_ack_state(target: &ChannelTarget, alarm: &mut crate::server::snapshot::AlarmInfo) {
    if let ChannelTarget::RecordField { record, .. } = target {
        let inst = record.read();
        if let Some(EpicsValue::Short(v)) = inst.resolve_field("ACKT") {
            alarm.ackt = Some(v as u16);
        }
        if let Some(EpicsValue::Short(v)) = inst.resolve_field("ACKS") {
            alarm.acks = Some(v as u16);
        }
    }
}

/// The put C reaches through `dbChannel_put` (a plain write, `dbPutField`,
/// `notify == false`) or through `dbProcessNotify` → `db_put_process` (a put
/// callback, `notify == true`). `ctx` names the writer for a simple PV's
/// write hook, the gateway's forwarded put, which alone can be postponed. A
/// refusal says whether the put callback still processed the record.
pub async fn put(
    db: &PvDatabase,
    target: &ChannelTarget,
    long_string_mode: LongStringMode,
    write: impl Into<PvWrite>,
    notify: bool,
    ctx: impl FnOnce() -> crate::server::pv::WriteContext,
) -> Result<PutStarted, PutRefused> {
    let write = write.into();
    match target {
        ChannelTarget::SimplePv(pv) => match (pv.write_hook(), write) {
            (Some(hook), write) => match hook(write, ctx()).await {
                Ok(forward) => Ok(pv.forward_write(forward, notify)?),
                Err(status) => Ok(PutStarted::Failed(status)),
            },
            (None, PvWrite::Value(value)) => {
                pv.set(value);
                Ok(PutStarted::Started(ProcessCompletion::Unprocessed))
            }
            // pcas's own `casPV::write` (casPV.cc:107-110).
            (None, PvWrite::AlarmAck(..)) => Ok(PutStarted::Failed(
                crate::error::DbStatus::CAS_APP_NO_SUPPORT,
            )),
        },
        ChannelTarget::RecordField { record, field } => {
            let name: &[u8] = record.name();
            let value = match write {
                PvWrite::Value(value) => value,
                // `dbPut` dispatches on the request type (`dbAccess.c:1333-1336`)
                // and `putAckt`/`putAcks` read the buffer's first element.
                PvWrite::AlarmAck(ack, levels) => {
                    let level = match &levels {
                        EpicsValue::Short(v) => *v as u16,
                        EpicsValue::ShortArray(a) => a.first().map_or(0, |v| *v as u16),
                        _ => 0,
                    };
                    return Ok(db
                        .put_alarm_ack_from_ca(name, field, ack, level)
                        .await
                        .map(|()| PutStarted::Started(ProcessCompletion::Sync))?);
                }
            };
            // A `$` channel puts by its `$` name, which the database
            // resolves to C's DBR_CHAR address of the field.
            let dollar;
            let field: &str = match long_string_mode {
                LongStringMode::DollarChar { .. } => {
                    dollar = format!("{field}$");
                    &dollar
                }
                _ => field,
            };
            if notify {
                db.put_record_field_notify(name, field, value)
                    .await
                    .map(PutStarted::Started)
            } else {
                // C `write_action` (`rsrv/camessage.c:781-789`)
                // routes CA_PROTO_WRITE through `dbPutField` —
                // no putNotify is ever built. Parking a wait-set
                // whose receiver this fire-and-forget arm drops
                // would occupy the record's notify slot until
                // any async processing it starts settles (a
                // motor's whole motion), failing every
                // legitimate WRITE_NOTIFY on the record with
                // ECA_PUTCBINPROG in the meantime.
                Ok(db
                    .put_record_field_from_ca_no_notify(name, field, value)
                    .await
                    .map(|()| PutStarted::Started(ProcessCompletion::Sync))?)
            }
        }
    }
}

/// C `db_add_event` (`dbEvent.c`) followed by `db_post_single_event`: attach
/// a subscriber for `target` to `user`'s event queue with the channel's pre
/// chain, and queue the current value as its first entry. `Err` is
/// `db_add_event` returning NULL.
#[allow(clippy::too_many_arguments)]
pub fn add_event(
    db: &PvDatabase,
    target: &ChannelTarget,
    user: &EventUser,
    sid: u32,
    native_type: DbFieldType,
    mask: u16,
    context: &DeliveryContext,
    pre_chain: FilterChain,
) -> Result<EventReader, SubscribeRefused> {
    match target {
        // The same chain a record-field monitor runs, sourced from the
        // channel's one filter owner.
        ChannelTarget::SimplePv(pv) => {
            pv.add_monitor_on(user, sid, native_type, mask, context, pre_chain)
        }
        ChannelTarget::RecordField { record, field } => {
            // rsrv posts the first event before `db_event_enable`
            // (`camessage.c:1907-1913`), so a post in between is lost and the
            // stale first event stands. One hold across the attach and the
            // first event leaves no in-between.
            let _scan_lock = db.lock_instance(record);
            let rx = {
                let mut instance = record.write();
                let rx =
                    instance.add_subscriber_on(user, field, sid, native_type, mask, context)?;
                for filt in pre_chain {
                    instance.attach_filter_to_last_subscriber(field, filt);
                }
                rx
            };
            #[cfg(test)]
            tests::AFTER_ATTACH.with(|hook| hook.get().map(|hook| hook(record)));
            post_single_event(db, record, field, rx.id());
            Ok(rx)
        }
    }
}

/// C `db_post_single_event` (`dbEvent.c:914-927`) for the `field` subscriber
/// holding slot `id`: the read and the queue are one scan-lock hold, so no
/// post lands between them and leaves the subscriber's last event older than
/// the field. The read takes the link target's lock for a link-backed
/// field's metadata, so it runs with no record guard held.
pub fn post_single_event(
    db: &PvDatabase,
    record: &Arc<RecordCell>,
    field: &str,
    id: crate::server::event_queue::AttachId,
) {
    let _scan_lock = db.lock_instance(record);
    if let Some(snap) = db.channel_snapshot_for_field(record, field, false) {
        record.read().post_single_event(field, id, Arc::new(snap));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::recgbl::EventMask;

    thread_local! {
        /// Runs in [`add_event`] between the attach and the first event.
        pub(super) static AFTER_ATTACH: std::cell::Cell<Option<fn(&Arc<RecordCell>)>> =
            const { std::cell::Cell::new(None) };
    }

    /// A post landing between the attach and the first event reaches the new
    /// subscriber, and no event it is handed is older than that post.
    #[epics_macros_rs::epics_test]
    async fn a_post_after_the_attach_is_delivered_and_the_first_event_is_not_stale() {
        let db = PvDatabase::new();
        db.add_record(
            "X",
            Box::new(crate::server::records::ai::AiRecord::new(1.0)),
        )
        .await
        .unwrap();
        let record = db.get_record("X").unwrap();
        AFTER_ATTACH.with(|hook| {
            hook.set(Some(|record| {
                // Nothing else can post here: the subscribe holds the record.
                assert_eq!(crate::server::database::scan_lock_held(), Some(true));
                let mut instance = record.write();
                instance
                    .record
                    .put_field("VAL", EpicsValue::Double(2.0))
                    .unwrap();
                instance.notify_field("VAL", EventMask::VALUE);
            }))
        });
        let user = EventUser::new();
        let mut rx = add_event(
            &db,
            &ChannelTarget::RecordField {
                record: record.clone(),
                field: "VAL".into(),
            },
            &user,
            1,
            DbFieldType::Double,
            EventMask::VALUE.bits(),
            crate::server::event_queue::no_context(),
            FilterChain::default(),
        )
        .unwrap();
        AFTER_ATTACH.with(|hook| hook.set(None));
        let mut seen = Vec::new();
        while let Ok(event) = rx.try_recv() {
            seen.push(event.value().as_value().and_then(EpicsValue::to_f64));
        }
        assert!(!seen.is_empty());
        assert!(seen.iter().all(|v| *v == Some(2.0)), "{seen:?}");
    }

    /// One record of every base type, the array ones in several element
    /// types, the long-string ones at several sizes.
    const EVERY_TYPE: &str = r#"
record(ai, "ai") {}
record(ao, "ao") {}
record(bi, "bi") { field(ZNAM, "z") field(ONAM, "o") }
record(bo, "bo") {}
record(stringin, "stringin") { info(Q:form, "String") }
record(stringout, "stringout") {}
record(longin, "longin") {}
record(longout, "longout") {}
record(int64in, "int64in") {}
record(int64out, "int64out") {}
record(lsi, "lsi") { field(SIZV, "100") }
record(lso, "lso") { field(SIZV, "20") info(Q:form, "String") }
record(mbbi, "mbbi") { field(ZRST, "zero") }
record(mbbo, "mbbo") { field(ZRST, "zero") field(ONST, "one") }
record(mbbo, "mbbo0") {}
record(mbbiDirect, "mbbiDirect") {}
record(mbboDirect, "mbboDirect") {}
record(event, "event") {}
record(printf, "printf") { field(SIZV, "60") }
record(waveform, "wfS") { field(FTVL, "STRING") field(NELM, "4") }
record(waveform, "wfC") { field(FTVL, "CHAR") field(NELM, "16") info(Q:form, "String") }
record(waveform, "wfUC") { field(FTVL, "UCHAR") field(NELM, "16") }
record(waveform, "wfD") { field(FTVL, "DOUBLE") field(NELM, "8") }
record(waveform, "wfI64") { field(FTVL, "INT64") field(NELM, "3") }
record(waveform, "wfUL") { field(FTVL, "ULONG") field(NELM, "3") }
record(aai, "aai") { field(FTVL, "STRING") field(NELM, "2") }
record(aao, "aao") { field(FTVL, "SHORT") field(NELM, "5") }
record(subArray, "subArray") { field(FTVL, "FLOAT") field(MALM, "10") field(NELM, "4") }
record(calc, "calc") { field(CALC, "A+1") }
record(fanout, "fanout") {}
record(seq, "seq") {}
record(calcout, "calcout") { field(CALC, "A") }
record(dfanout, "dfanout") {}
record(compress, "compress") { field(NSAM, "6") }
record(histogram, "histogram") { field(NELM, "5") }
record(sel, "sel") {}
record(sub, "sub") {}
record(aSub, "aSub") {
    field(FTA, "STRING") field(NOA, "4")
    field(FTB, "CHAR") field(NOB, "10")
    field(FTVA, "DOUBLE") field(NOVA, "3")
    field(FTVB, "STRING")
}
record(permissive, "permissive") {}
record(state, "state") {}
"#;

    /// What the create read under the record's lock before it took none.
    fn shape_under_lock(
        instance: &crate::server::record::RecordInstance,
        field: &str,
        long_string: bool,
        chain: &FilterChain,
    ) -> Option<(DbFieldType, u32, LongStringMode)> {
        use crate::server::database::char_view::{DollarView, char_view, read_route};
        use crate::server::record::FieldDeclaration;
        let v = instance.client_field_value(field)?;
        if long_string {
            let view = char_view(instance, field)?;
            let no_elements = view.no_elements as u32;
            let mode = LongStringMode::DollarChar {
                no_elements,
                nul_at_count: view.nul_at_count(),
                view: DollarView::of(instance, field, chain)?,
            };
            return Some((DbFieldType::Char, no_elements, mode));
        }
        if let Some(route) = read_route(instance, field).and_then(|s| s.text_route()) {
            return Some((v.dbr_type(), 1, LongStringMode::Text(route)));
        }
        if instance.record.field_is_long_string(field) {
            return Some((DbFieldType::String, 1, LongStringMode::NativeString));
        }
        if let Some(native) = instance.record.field_native_count(field) {
            return Some((v.dbr_type(), native, LongStringMode::Plain));
        }
        Some((v.dbr_type(), v.count() as u32, LongStringMode::Plain))
    }

    /// [`record_channel_shape`] answers with no lock what the create read
    /// under it: every field of every base type, with `$` and without, with a
    /// filter chain and without.
    #[epics_macros_rs::epics_test]
    async fn the_lock_free_shape_is_the_one_read_under_the_lock() {
        let (db, _) = crate::server::ioc_builder::IocBuilder::new()
            .db_string(EVERY_TYPE, &std::collections::HashMap::new())
            .unwrap()
            .build()
            .await
            .unwrap();
        db.ioc_init().await;
        let arr = crate::server::database::filters::db_channel_create(
            &db,
            br#"ai.VAL{"arr":{"s":1,"e":2}}"#,
        )
        .unwrap()
        .chain;
        let chains = [FilterChain::default(), arr];
        let names = EVERY_TYPE
            .lines()
            .filter(|l| l.starts_with("record("))
            .filter_map(|l| l.split('"').nth(1))
            .collect::<Vec<_>>();
        let mut differ = Vec::new();
        for name in names {
            let cell = db.get_record(name).unwrap();
            let fields = cell
                .rdes()
                .fields
                .iter()
                .chain(crate::server::record::dbd_generated::DB_COMMON_FIELDS)
                .map(|f| f.name)
                .chain(["RTYP", "NOPE"]);
            for field in fields {
                for long_string in [false, true] {
                    for chain in &chains {
                        let free = record_channel_shape(&db, &cell, field, long_string, chain);
                        let held = shape_under_lock(&cell.read(), field, long_string, chain);
                        if free != held {
                            let dollar = if long_string { "$" } else { "" };
                            differ.push(format!("{name}.{field}{dollar}: {free:?} != {held:?}"));
                        }
                    }
                }
            }
        }
        assert!(differ.is_empty(), "{}", differ.join("\n"));
    }

    /// `PvDatabase::parsed_link_target_field_type` answers with no lock what
    /// it read under the target's lock: every field of every base type.
    #[epics_macros_rs::epics_test]
    async fn the_lock_free_link_target_type_is_the_one_read_under_the_lock() {
        let (db, _) = crate::server::ioc_builder::IocBuilder::new()
            .db_string(EVERY_TYPE, &std::collections::HashMap::new())
            .unwrap()
            .build()
            .await
            .unwrap();
        db.ioc_init().await;
        let names = EVERY_TYPE
            .lines()
            .filter(|l| l.starts_with("record("))
            .filter_map(|l| l.split('"').nth(1))
            .collect::<Vec<_>>();
        let mut differ = Vec::new();
        for name in names {
            let cell = db.get_record(name).unwrap();
            let fields = cell
                .rdes()
                .fields
                .iter()
                .chain(crate::server::record::dbd_generated::DB_COMMON_FIELDS)
                .map(|f| f.name)
                .chain(["RTYP", "NOPE"]);
            for field in fields {
                let link =
                    crate::server::record::parse_link_v2(format!("{name}.{field}").as_bytes());
                let free = db.parsed_link_target_field_type(&link);
                let inst = cell.read();
                let held = crate::server::record::record_instance::addressed_field_type_of(
                    inst.record.as_ref(),
                    field,
                )
                .or_else(|| inst.attribute(field).map(|_| DbFieldType::String));
                drop(inst);
                if free != held {
                    differ.push(format!("{name}.{field}: {free:?} != {held:?}"));
                }
            }
        }
        assert!(differ.is_empty(), "{}", differ.join("\n"));
    }
}
