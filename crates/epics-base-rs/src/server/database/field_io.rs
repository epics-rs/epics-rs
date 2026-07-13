use std::collections::HashSet;

use crate::error::{CaError, CaResult};
use crate::server::snapshot::Snapshot;
use crate::types::EpicsValue;

use super::PvDatabase;

/// C `dbPutField`'s put-disable gate (`dbAccess.c:1255-1257`):
/// `precord->disp && paddr->pfield != &precord->disp` → `S_db_putDisabled`.
///
/// This is the FIRST gate an *external* put crosses. It precedes `dbPut` —
/// so it precedes the `SPC_NOMOD` rejection of PACT/LCNT/PUTF — and it
/// precedes the PROC-driven `dbProcess` (`dbAccess.c:1265-1277`), so
/// `caput REC.PROC 1` on a `DISP=1` record is refused, not force-processed.
///
/// Single owner for both external put boundaries: the CA / `dbpf` route
/// ([`PvDatabase::put_record_field_from_ca`]) and the QSRV precondition
/// check ([`PvDatabase::check_external_put_preconditions`]). Internal puts
/// (`put_pv`, link and processing writes — the `dbPut` analogue) deliberately
/// do not cross it.
fn check_put_disabled(
    instance: &crate::server::record::RecordInstance,
    field_upper: &str,
) -> CaResult<()> {
    if instance.common.disp && field_upper != "DISP" {
        return Err(CaError::PutDisabled(field_upper.to_string()));
    }
    Ok(())
}

/// C's `dbPut` no-modify gate — the put-side consumer of the SPC_NOMOD
/// declaration.
///
/// Two C rejections, both inside `dbPut` and therefore BELOW every put entry
/// point (`dbPutField` for CA/`dbpf`, `dbPutLink` for a record's OUT link,
/// `dbPutSpecial` for an internal one):
///
/// ```c
/// /* dbAccess.c:1330-1332 */
/// if (special == SPC_ATTRIBUTE) return S_db_noMod;
/// /* dbAccess.c:123-126, via dbPut -> dbPutSpecial(paddr, 0) */
/// if ((special == SPC_NOMOD) && (pass == 0)) return S_db_noMod;
/// ```
///
/// INVARIANT: a field declared `special(SPC_NOMOD)` (or `SPC_ATTRIBUTE`) MUST
/// NOT be modified by ANY runtime write, whatever route it arrives on — CA put,
/// `dbpf`, QSRV, an internal `put_pv`, or a record's OUT link. A record's own
/// `put_field` is NOT a gate: the hand-written array records write
/// NELM/FTVL/NORD there because the *load* path (`dbLoadRecords` →
/// `Record::put_field`) must set them — C likewise writes them through
/// `dbStaticLib`'s `dbPutString`, which never crosses `dbPut`.
///
/// The declaration itself lives in [`RecordInstance::is_no_mod`], which C
/// exposes as `dbChannelSpecial(...) == SPC_NOMOD` and reads from TWO places:
/// this gate (`dbPut`, dbAccess.c:123-126) and `rsrvCheckPut`
/// (camessage.c:2540-2551), which feeds the CA ACCESS_RIGHTS write bit. This
/// function is the first consumer; `epics-ca-rs`'s `compute_access` is the
/// second.
///
/// The one thing that legitimately changes ACKS/ACKT is C's alarm
/// acknowledgement, and it does NOT come through here: `dbPut` dispatches on the
/// DBR *request type* (`DBR_PUT_ACKT`/`DBR_PUT_ACKS`, `dbAccess.c:1331-1335`)
/// ABOVE this gate, into [`RecordInstance::put_ackt`] /
/// [`RecordInstance::put_acks`]. The wire route for that is
/// [`PvDatabase::put_alarm_ack_from_ca`].
///
/// `field` must already be upper-cased.
fn check_no_mod(instance: &crate::server::record::RecordInstance, field: &str) -> CaResult<()> {
    if instance.is_no_mod(field) {
        return Err(CaError::ReadOnlyField(field.to_string()));
    }
    Ok(())
}

/// Does an *external* put to `field` drive a processing cycle on this record?
///
/// C `dbPutField` (`dbAccess.c:1263-1268`) and pvxs `IOCSource::
/// doPostProcessing` (`iocsource.cpp:397-403`) ask the same question with the
/// same three terms: the `PROC` field always, else a `pp(TRUE)` field on a
/// Passive record. (`dbrType < DBR_PUT_ACKT` is subsumed: the alarm-ack fields
/// are not `pp(TRUE)`.) A caller that FORCES processing
/// (`record._options.process=true`) does not consult this at all — force is
/// the caller's own term, not the record's.
///
/// Single owner of the rule: the single-record put route
/// ([`PvDatabase::put_record_field_from_ca`]) tests it while it already holds
/// the instance, and the QSRV group PUT — whose C twin is `doPostProcessing` —
/// reaches it through [`PvDatabase::put_drives_processing`]. Neither can drift
/// from C or from the other.
///
/// `field` must already be upper-cased.
pub(crate) fn put_drives_processing_of(
    instance: &crate::server::record::RecordInstance,
    field: &str,
) -> bool {
    field == "PROC"
        || (instance.common.scan == crate::server::record::ScanType::Passive
            && instance.record.processes_after_put(field))
}

/// C `dbPutSpecial(paddr, 1)` — the after-put `special()`, paired with the drain
/// of the link writes it queued ([`Record::take_special_actions`]).
///
/// The pairing is the point: `special()` and its drain are ONE step, so a queued
/// action cannot survive the put that queued it — not even when `special()`
/// returns an error (C's `dbPut` `goto done`), because the drain happens before
/// the status is propagated. The caller executes `out` once the record lock is
/// released, ahead of the put-driven process cycle, which is where C runs it
/// (inside `dbPut`, before `dbPutField`'s `dbProcess`).
///
/// Every `dbPut` path in this module goes through here; nothing else may call
/// `Record::special(field, true)`.
///
/// Returns the scan-index delta the after-put pass produced — non-`NoChange`
/// only for the SIMM↔SSCN swap below, which the caller applies through
/// `update_scan_index` once the record lock is down.
fn special_after_put(
    instance: &mut crate::server::record::RecordInstance,
    field: &str,
    out: &mut Vec<crate::server::record::ProcessAction>,
) -> CaResult<crate::server::record::CommonFieldPutResult> {
    let status = instance.record.special(field, true);
    out.extend(instance.record.take_special_actions());
    status?;

    // C `special()`'s CONSTANT-link re-seed (`calcoutRecord.c:367-378`,
    // `sCalcoutRecord.c:512-517`, `aCalcoutRecord.c:534-540`,
    // `transformRecord.c:714-719` — the four records whose C `special()` calls
    // `recGblInitConstantLink`): a put that leaves an input link constant
    // re-runs the load into that input's value field and posts it. Without it a
    // constant link is load-once dead state — the link layer delivers nothing
    // for a constant at process time, so `caput CO.INPB 7` would store the text
    // and leave `B` at its `.db` value forever.
    //
    // The record only DECLARES the pairs (`special_reseed_input_links`); the
    // load itself is the shared `rec_gbl_init_constant_link` owner, the same one
    // the init seed uses. Records whose C `special()` does not re-seed (calc,
    // sub, sel, aSub, swait, …) declare nothing and are untouched.
    if let Some(value_field) =
        crate::server::record::reseed_constant_input_link(&mut *instance.record, field)
    {
        // The mask is the C call site's, and they disagree — calcout posts a
        // literal DBE_VALUE, transform DBE_VALUE|DBE_LOG. The record carries it.
        let mask = instance.record.special_reseed_post_mask();
        instance.notify_field(value_field, mask);
    }

    // C `special(SPC_MOD)` pass 1 on SIMM (`longinRecord.c:171-177` and the
    // identical arm in all 21 SSCN-bearing records):
    //   `recGblCheckSimm((dbCommon *)prec, &prec->sscn, prec->oldsimm, prec->simm);`
    // Paired with `special_before_put`'s pass 0 (`recGblSaveSimm`), and gated
    // per record type by `Record::uses_recgbl_simm_helpers`.
    Ok(if field == "SIMM" {
        instance.rec_gbl_check_simm()
    } else {
        crate::server::record::CommonFieldPutResult::NoChange
    })
}

/// C `dbPutSpecial(paddr, 0)` — the before-put pass, run while the record lock
/// is held and BEFORE the field's new value is stored.
///
/// The only `SPC_MOD` field in the record framework whose pass-0 does work is
/// SIMM: `recGblSaveSimm` latches the outgoing simulation mode into OLDSIMM so
/// the after-put pass can see the transition. Paired with
/// [`special_after_put`]; every `dbPut` path in this module calls both.
fn special_before_put(instance: &mut crate::server::record::RecordInstance, field: &str) {
    if field == "SIMM" {
        instance.rec_gbl_save_simm();
    }
}

/// Coerce a write `value` to a record field's stored `target` type — C
/// `dbConvert.c`'s `dbFastPutConvertRoutine[dbrType][field_type]` table.
///
/// The client-`dbPut` half of the shared converter
/// [`crate::server::record::coerce_put_value`]; the internal-delivery half is
/// `put_field_internal_default`. A `DBR_STRING` write to a `DBF_MENU` or
/// `DBF_ENUM` field has a converter of its own in C (`putStringMenu`,
/// `putStringEnum`) and must not fall through to `EpicsValue::convert_to`,
/// which is field-blind and turns any unrecognised string into index 0 —
/// C stores nothing and fails the put with `S_db_badChoice`.
fn coerce_write_value(
    record: &dyn crate::server::record::Record,
    field: &str,
    target: crate::types::DbFieldType,
    value: EpicsValue,
) -> CaResult<EpicsValue> {
    crate::server::record::coerce_put_value(record, field, target, value)
}

/// What a `dbPut` of a given value means for a given field — the single owner
/// of C `dbPut`'s value branch (`dbAccess.c:1345-1372`).
enum PutRequest {
    /// Write this value; already coerced to the field's native type.
    Write(EpicsValue),
    /// A zero-element request (`nRequest < 1`) into a **scalar** destination.
    ///
    /// C writes nothing, raises `LINK_ALARM`/`INVALID_ALARM` on the record, and
    /// returns **success** — `status` stays 0, so the put is accepted and the
    /// record's next `recGblResetAlarms` publishes the new alarm
    /// (`dbAccess.c:1370-1372`, commit `12cfd418d`, whose subject is "fix dbPut
    /// to *set* the target to INVALID/LINK alarm when writing empty arrays into
    /// scalars" — not to reject the put).
    EmptyIntoScalar,
}

/// Resolve a put into its C `dbPut` branch.
///
/// C picks the branch from the **destination's** element count
/// (`dbAccess.c:1345` `no_elements > 1`): an array field clamps `nRequest` and
/// converts — a zero-length request copies nothing and succeeds silently — while
/// a scalar field with `nRequest < 1` takes the alarm branch. The test is on the
/// request count and the destination, never on whether a type conversion happens
/// to be needed.
///
/// [`FieldDesc`](crate::server::record::FieldDesc) carries no element count, so
/// the destination's current value is the probe: an array-valued field reads
/// back as an array variant. A field the record does not own (a `dbCommon`
/// field, reached via `put_common_field`) reads back as `None` and is scalar,
/// which is also what its `DBF_*` descriptor says in C.
fn dbput_request(
    record: &dyn crate::server::record::Record,
    field: &str,
    value: EpicsValue,
) -> CaResult<PutRequest> {
    let dest_is_array = record.get_field(field).is_some_and(|v| v.is_array());
    if value.is_empty_array() && !dest_is_array {
        return Ok(PutRequest::EmptyIntoScalar);
    }
    let target = record
        .field_list()
        .iter()
        .find(|f| f.name.eq_ignore_ascii_case(field))
        .map(|f| f.dbf_type);

    // C `dbPut` clamps the request to the destination's element count —
    // `if (no_elements < nRequest) nRequest = no_elements;` (dbAccess.c:1359),
    // then converts `nRequest` elements. A multi-element request into a
    // one-element destination therefore writes element 0 and SUCCEEDS; the
    // surplus elements are dropped, not an error. Reduce the array to its
    // first element here, so the record's typed `put_field` arm — and every
    // `put_common_field` arm — sees the scalar C would have written instead of
    // rejecting the array with a `TypeMismatch`.
    //
    // One array shape is exempt: a `CharArray` into a `DBF_STRING` field is how
    // this port carries the dbChannel `$` char-array view of a string field
    // (`dbChannel.c:486-505` re-types it to `DBF_CHAR[field_size]`, i.e. an
    // ARRAY destination in C — its element count is 40, not 1). The `$` flag
    // lives on the CA channel and never reaches this layer, so the char view is
    // recognised by its shape and left to `convert_to`, which decodes the bytes
    // back into the string field.
    let is_char_string_view = matches!(value, EpicsValue::CharArray(_))
        && target == Some(crate::types::DbFieldType::String);
    let value = if !dest_is_array && value.is_array() && !is_char_string_view {
        value.first_element().unwrap_or(value)
    } else {
        value
    };

    match target {
        Some(target) if value.db_field_type() != target => Ok(PutRequest::Write(
            coerce_write_value(record, field, target, value)?,
        )),
        _ => Ok(PutRequest::Write(value)),
    }
}

/// Apply C's [`PutRequest::EmptyIntoScalar`] effect: the field is left
/// untouched and the record is driven to `LINK_ALARM`/`INVALID_ALARM`
/// (`dbAccess.c:1371` `recGblSetSevr(precord, LINK_ALARM, INVALID_ALARM)`).
fn set_empty_request_alarm(instance: &mut crate::server::record::RecordInstance) {
    crate::server::recgbl::rec_gbl_set_sevr(
        &mut instance.common,
        crate::server::recgbl::alarm_status::LINK_ALARM,
        crate::server::record::AlarmSeverity::Invalid,
    );
}

/// What the put entry point owes the caller in the way of completion — the
/// `dbPutField` / `dbPutNotify` split, plus the restart C's `dbNotify` state
/// machine performs on a put-notify that had to wait for a PACT record.
///
/// The completion *sender* travels with the request: a restarted put must
/// signal the ORIGINAL caller's receiver, which was handed out when the put
/// first arrived and was deferred, so the restart cannot mint a fresh channel.
enum NotifyRequest {
    /// C `dbPutField` — process the record, build no `putNotify`.
    None,
    /// C `dbPutNotify` arriving fresh from a client: mint the wait-set channel.
    New,
    /// C `dbNotify.c:207-231` restart: the client's receiver already exists,
    /// this replay carries its sender.
    Deferred(crate::runtime::sync::oneshot::Sender<()>),
}

impl NotifyRequest {
    fn wants_notify(&self) -> bool {
        !matches!(self, NotifyRequest::None)
    }

    /// The completion sender, plus the receiver to hand back — `Some` only for
    /// a fresh request; a restart's receiver went to the client at deferral.
    #[allow(clippy::type_complexity)]
    fn into_completion(
        self,
    ) -> Option<(
        crate::runtime::sync::oneshot::Sender<()>,
        Option<crate::runtime::sync::oneshot::Receiver<()>>,
    )> {
        match self {
            NotifyRequest::None => None,
            NotifyRequest::New => {
                let (tx, rx) = crate::runtime::sync::oneshot::channel();
                Some((tx, Some(rx)))
            }
            NotifyRequest::Deferred(tx) => Some((tx, None)),
        }
    }
}

/// Snapshot NORD before a `dbPut` writes the value field — C `put_array_info`
/// opens with `epicsUInt32 nord = prec->nord;` (`waveformRecord.c:202-216`).
///
/// `None` for a put to any field but VAL, and for a record type that has no
/// NORD (the comparison in [`post_array_info`] then reduces to "unchanged").
fn array_nord_before_put(
    instance: &crate::server::record::RecordInstance,
    field: &str,
) -> Option<EpicsValue> {
    if field == "VAL" {
        instance.record.get_field("NORD")
    } else {
        None
    }
}

/// The tail of C `put_array_info`, and the SINGLE owner of the NORD post:
///
/// ```c
/// if (nord != prec->nord)
///     db_post_events(prec, &prec->nord, DBE_VALUE | DBE_LOG);
/// ```
///
/// `put_array_info` is called from `dbPut`, so it is reached by EVERY put
/// route — CA, `dbPutLink`, internal — and by none of them conditionally. The
/// port's array records (waveform/aai/aao/subArray) re-derive NORD inside
/// `put_field("VAL")`; this is the post half, and every `dbPut` body in this
/// module calls it after the value write, passing the snapshot taken by
/// [`array_nord_before_put`].
///
/// Note this post is NOT the process cycle's monitor post: the compiled softIoc
/// on a 10-second-SCAN waveform posts `NORD = 3` the instant `caput -a WP 3
/// 1 2 3` lands, and posts no VAL at all (waveform VAL is `pp(TRUE)`, so C
/// suppresses the value-field post in `dbPut` and the scan is 10 seconds away).
fn post_array_info(
    instance: &mut crate::server::record::RecordInstance,
    old_nord: &Option<EpicsValue>,
    origin: u64,
) {
    let Some(old) = old_nord else { return };
    let moved = instance
        .record
        .get_field("NORD")
        .is_some_and(|new| new != *old);
    if moved {
        instance.notify_field_with_origin(
            "NORD",
            crate::server::recgbl::EventMask::VALUE | crate::server::recgbl::EventMask::LOG,
            origin,
        );
    }
}

impl PvDatabase {
    /// Get a PV value synchronously, from a thread that cannot `await`.
    ///
    /// Works from a plain thread with no runtime entered (an iocsh thread, a
    /// driver's own thread) and from a multi-threaded runtime worker; see
    /// [`crate::runtime::task::block_on_sync`] for which mechanism is used
    /// where.
    ///
    /// Returns an error on a **current-thread** runtime, where blocking cannot
    /// be made sound: parking that runtime's only thread halts the task holding
    /// the database lock this call awaits. Such callers must `await`
    /// [`Self::get_pv`] instead.
    pub fn get_pv_blocking(&self, name: &str) -> CaResult<EpicsValue> {
        crate::runtime::task::block_on_sync(self.get_pv(name)).unwrap_or_else(|_| {
            Err(CaError::InvalidValue(
                "get_pv_blocking cannot block a current-thread runtime; await get_pv() instead"
                    .into(),
            ))
        })
    }

    /// Get the current value of a PV or record field.
    /// Uses resolve_field for records (3-level priority).
    pub async fn get_pv(&self, name: &str) -> CaResult<EpicsValue> {
        let (base, field) = super::parse_pv_name(name);
        let field = field.to_ascii_uppercase();

        // Check simple PVs first (exact match)
        if let Some(pv) = self.inner.simple_pvs.read().await.get(name) {
            return Ok(pv.get().await);
        }

        // Records — alias-aware via `get_record` (epics-base PR #336).
        if let Some(rec) = self.get_record(base).await {
            let instance = rec.read().await;
            return instance
                .resolve_field(&field)
                .ok_or_else(|| CaError::ChannelNotFound(name.to_string()));
        }

        Err(CaError::ChannelNotFound(name.to_string()))
    }

    /// Set a PV value or record field, notifying subscribers.
    /// Tries record put_field first, then put_common_field as fallback.
    ///
    /// Acquires the record's advisory write gate.
    pub async fn put_pv(&self, name: &str, value: EpicsValue) -> CaResult<()> {
        self.put_pv_inner(name, value, true).await
    }

    /// `put_pv` variant for a caller already holding the
    /// record's advisory write gate (QSRV atomic group PUT). See
    /// [`Self::put_record_field_from_ca_already_locked`].
    pub async fn put_pv_already_locked(&self, name: &str, value: EpicsValue) -> CaResult<()> {
        self.put_pv_inner(name, value, false).await
    }

    /// C `IOCSource::doPreProcessing` gate (pvxs `iocsource.cpp:363-375`).
    ///
    /// Reject an *external* put (a PVA/CA client put routed through QSRV)
    /// that C refuses before any write: a put to a `DISP=1` record's
    /// non-DISP field (`S_db_putDisabled`) or to a read-only / `SPC_NOMOD`
    /// field (`S_db_noMod`). No value is written — this is a precondition
    /// check only. It mirrors the two gates inside
    /// [`Self::put_record_field_from_ca`] (the Passive route) so the QSRV
    /// `Force`/`Inhibit` routes — which go through [`Self::put_pv`] — enforce
    /// the same preconditions. `put_pv` itself is the internal `dbPut`
    /// analogue and deliberately does not gate DISP (internal
    /// link/processing puts must bypass it), so the gate lives at the
    /// external put boundary, exactly as C places `doPreProcessing` in the
    /// source layer rather than in `dbPut`.
    pub async fn check_external_put_preconditions(
        &self,
        record_name: &str,
        field: &str,
    ) -> CaResult<()> {
        let field_upper = field.to_ascii_uppercase();
        // A missing record is not a DISP/read-only precondition violation:
        // stay silent and let the downstream put report the not-found (for
        // QSRV, inside its own `asTrapWrite` bracket). C's `doPreProcessing`
        // only runs against an established channel — the record is
        // guaranteed present there — and a `BridgeChannel` likewise always
        // binds a real record in production.
        let Some(rec) = self.get_record(record_name).await else {
            return Ok(());
        };
        let instance = rec.read().await;
        // Read-only / SPC_NOMOD field, through the one gate owner
        // ([`check_no_mod`]). C tests SPC_ATTRIBUTE *before* `disp`
        // (iocsource.cpp:365-369), so a read-only field on a DISP=1 record
        // reports S_db_noMod, not S_db_putDisabled; the two errors carry
        // different wire text.
        check_no_mod(&instance, &field_upper)?;
        // DISP=1 blocks a put to any field except DISP itself — the shared
        // gate owner, identical to the one the CA route crosses.
        check_put_disabled(&instance, &field_upper)?;
        Ok(())
    }

    /// pvxs `IOCSource::doPostProcessing`'s record-side terms
    /// (`iocsource.cpp:397-403`): does a put to `record_name.field` drive a
    /// processing cycle on its own?
    ///
    /// The QSRV group PUT asks this for a member whose write bypassed
    /// [`Self::put_record_field_from_ca`] (a `+type:"proc"` trigger, or a
    /// member that is `changing` but has no writable leaf), so that route
    /// applies the SAME gate as a plain field put instead of processing
    /// unconditionally. `false` for an unknown record — there is nothing to
    /// process. Force (`record._options.process=true`) is the caller's term
    /// and is not asked about here; see [`put_drives_processing_of`].
    pub async fn put_drives_processing(&self, record_name: &str, field: &str) -> bool {
        let field_upper = field.to_ascii_uppercase();
        let Some(rec) = self.get_record(record_name).await else {
            return false;
        };
        let instance = rec.read().await;
        put_drives_processing_of(&instance, &field_upper)
    }

    async fn put_pv_inner(
        &self,
        name: &str,
        value: EpicsValue,
        acquire_gate: bool,
    ) -> CaResult<()> {
        let (base, field) = super::parse_pv_name(name);
        let field = field.to_ascii_uppercase();

        // Check simple PVs first
        if let Some(pv) = self.inner.simple_pvs.read().await.get(name) {
            pv.set(value).await;
            return Ok(());
        }

        // Records — alias-aware (epics-base PR #336).
        if let Some(rec) = self.get_record(base).await {
            // `base` may be an alias; resolve to the canonical record
            // name so scan-index updates target the right entry.
            let canonical_base: String = self
                .resolve_alias(base)
                .await
                .unwrap_or_else(|| base.to_string());
            // advisory write gate (`dbScanLock` analogue) so a
            // plain `put_pv` to a backing record cannot interleave
            // with an atomic group transaction holding the same gate.
            // Skipped when the caller already owns the gate.
            let _record_gate = if acquire_gate {
                Some(self.lock_record(&canonical_base).await)
            } else {
                None
            };
            let mut instance = rec.write().await;

            // C `dbPut` refuses an SPC_NOMOD / SPC_ATTRIBUTE field before it
            // converts anything (`dbAccess.c:1330-1332`). `put_pv` IS the
            // `dbPut` analogue — it sits below `dbPutLink`, so this is what
            // stops a record's OUT link from truncating a waveform's NELM.
            // The refusal is returned to the caller; `write_out_link_value`
            // (C `dbPutLink`) turns it into the writer's LINK/INVALID alarm.
            check_no_mod(&instance, &field)?;

            let request = dbput_request(&*instance.record, &field, value)?;

            // Capture the pre-put value so the metadata-cache
            // invalidation (and the downstream `DBE_PROPERTY`
            // emission) can be skipped when the put is a no-op —
            // epics-base faac1df1.
            let prev_value = instance.record.get_field(&field);
            let old_nord = array_nord_before_put(&instance, &field);

            // Link writes the record's `special()` makes itself (C runs them
            // inside `dbPut`); executed below, once the record lock is released.
            let mut special_actions = Vec::new();

            // put_pv is C EPICS dbPut: write value + special/on_put.
            // Does NOT post monitor events (use put_pv_and_post for that).
            // Does NOT clear UDF or trigger processing.
            use crate::server::record::CommonFieldPutResult;
            let common_result = match request {
                // C `dbAccess.c:1370-1372` — accept, write nothing, alarm.
                PutRequest::EmptyIntoScalar => {
                    set_empty_request_alarm(&mut instance);
                    CommonFieldPutResult::NoChange
                }
                PutRequest::Write(value) => {
                    special_before_put(&mut instance, &field);
                    match instance.record.put_field(&field, value.clone()) {
                        Ok(()) => {
                            instance.record.on_put(&field);
                            // C `dbPut` (dbAccess.c:1399-1405) keeps the value
                            // it already stored but RETURNS the after-put
                            // `dbPutSpecial(paddr, 1)` status, skipping the
                            // field's monitor post and (in `dbPutField`) the
                            // `pp(TRUE)` process. `calcRecord::special` uses
                            // that to refuse an uncompilable CALC with
                            // S_db_badField, so the status must not be dropped.
                            special_after_put(&mut instance, &field, &mut special_actions)?
                        }
                        Err(CaError::FieldNotFound(_)) => {
                            instance.put_common_field(&field, value)?
                        }
                        Err(e) => return Err(e),
                    }
                }
            };

            // Invalidate metadata cache only if the metadata-class
            // field's value actually changed (faac1df1).
            instance.notify_field_written_if_changed(&field, prev_value.as_ref());

            // The one post this body makes. `put_pv` is the `dbPutLink` route's
            // `dbPut`, and C's `put_array_info` is reached from every `dbPut` —
            // an OUT link that shortens a waveform posts NORD in C even when the
            // link is NPP and the target never processes. The value-field post
            // stays absent here (C suppresses it for a `pp(TRUE)` value field,
            // and the port's other callers of `put_pv` rely on the process cycle
            // for it); NORD has no such second path.
            post_array_info(&mut instance, &old_nord, 0);

            // The record lock must be down before the scan-index update and
            // before the `special()` link writes below, which re-enter the
            // database (they can process their target).
            drop(instance);

            // Update scan index if SCAN or PHAS changed
            match common_result {
                CommonFieldPutResult::ScanChanged {
                    old_scan,
                    new_scan,
                    phas,
                } => {
                    self.update_scan_index(&canonical_base, old_scan, new_scan, phas, phas)
                        .await;
                }
                CommonFieldPutResult::PhasChanged {
                    scan: s,
                    old_phas,
                    new_phas,
                } => {
                    self.update_scan_index(&canonical_base, s, s, old_phas, new_phas)
                        .await;
                }
                CommonFieldPutResult::NoChange => {}
            }

            // C `dbPut` runs `dbPutSpecial(paddr, 1)` to completion — the
            // `dbPutLink` calls a `special()` makes included — before it returns
            // to `dbPutField`. This is the last statement of the `dbPut`
            // analogue, so it is that point.
            self.run_special_actions(&canonical_base, &rec, special_actions)
                .await;

            // mirror the CA-write path's ASG-field notifier so
            // restore scripts / autosave / admin tools that go via
            // `put_pv` (not `put_record_field_from_ca`) also trigger
            // per-client `reeval_access_rights`. C `dbAccess.c::
            // dbPutSpecial` invokes the SPC_AS callback from dbPut
            // regardless of caller entry path.
            if field == "ASG" {
                crate::server::access_security::notify_asg_field_changed();
            }

            return Ok(());
        }

        Err(CaError::ChannelNotFound(name.to_string()))
    }

    /// Write a value and post monitor events if changed.
    /// Equivalent to C EPICS `dbPut` + `db_post_events(DBE_VALUE|DBE_LOG)`.
    ///
    /// Use for readback/status mirror PVs that are written by sequencer-style
    /// code and need to be visible to CA monitors without triggering record
    /// processing. Clears UDF/UDF_ALARM on primary field write.
    ///
    /// `origin`: writer ID for self-write filtering. Subscribers with the
    /// same `ignore_origin` will skip this event. Pass 0 to disable.
    pub async fn put_pv_and_post(&self, name: &str, value: EpicsValue) -> CaResult<()> {
        self.put_pv_and_post_with_origin(name, value, 0).await
    }

    /// Push a monitor event holding the simple PV's *current* value
    /// but with explicit alarm severity/status. Used by the gateway
    /// to surface upstream-disconnect to downstream monitor
    /// subscribers without dropping the shadow PV (which would force
    /// downstream clients into ECA_DISCONN reconnect storms on every
    /// transient hiccup). Returns `ChannelNotFound` for record-backed
    /// PVs — those carry their own `common.sevr/stat` in record
    /// processing.
    pub async fn post_alarm(&self, name: &str, severity: u16, status: u16) -> CaResult<()> {
        if let Some(pv) = self.inner.simple_pvs.read().await.get(name).cloned() {
            pv.post_alarm(severity, status).await;
            return Ok(());
        }
        Err(crate::error::CaError::ChannelNotFound(name.to_string()))
    }

    /// Propagate a full upstream snapshot (value + alarm status/severity +
    /// IOC timestamp) to a simple shadow PV and fan out to downstream
    /// monitor subscribers. Used by the CA gateway forwarding task to avoid
    /// discarding the upstream alarm and timestamp decoded from the incoming
    /// `DBR_TIME_*` frame. Returns `ChannelNotFound` for record-backed PVs
    /// (those carry their own alarm engine and are not shadow PVs).
    pub async fn put_pv_and_post_snapshot(&self, name: &str, snapshot: Snapshot) -> CaResult<()> {
        if let Some(pv) = self.inner.simple_pvs.read().await.get(name).cloned() {
            pv.set_snapshot(snapshot).await;
            return Ok(());
        }
        Err(CaError::ChannelNotFound(name.to_string()))
    }

    /// Install upstream `DBR_CTRL_*` metadata (display / control limits,
    /// enum labels) on a shadow simple PV WITHOUT posting an event.
    ///
    /// The CA gateway calls this once on upstream connect, after its initial
    /// `DBR_CTRL_*` get, so a later downstream `DBR_CTRL_*` / `DBR_GR_*` read
    /// returns the real limits instead of zeroed ones. No `DBE_PROPERTY`
    /// monitor event fires — nothing has *changed* yet, this only seeds the
    /// attribute cache. Mirrors C `gatePvData::getCB` → `runDataCB` →
    /// `vc->setPvData(dd)` (`gatePv.cc:1693-1695`), which seeds the property
    /// cache from the initial control get in both cache modes before any
    /// monitor is enabled.
    ///
    /// Returns `ChannelNotFound` for record-backed PVs — those own their own
    /// metadata via record processing and are not gateway shadow PVs.
    pub async fn set_pv_metadata(&self, name: &str, snapshot: &Snapshot) -> CaResult<()> {
        if let Some(pv) = self.inner.simple_pvs.read().await.get(name).cloned() {
            pv.set_metadata(metadata_from_snapshot(snapshot));
            return Ok(());
        }
        Err(CaError::ChannelNotFound(name.to_string()))
    }

    /// Refresh a shadow simple PV's upstream metadata AND post a
    /// `DBE_PROPERTY` monitor event carrying `snapshot` to downstream
    /// property subscribers.
    ///
    /// `snapshot` is the decoded upstream `DBR_CTRL_*` property event: it
    /// carries the control value and the upstream `status` / `severity`,
    /// and (because control DBR structs carry no timestamp) an undefined
    /// timestamp the caller must NOT replace with a fresh wall-clock. The
    /// gateway's property monitor calls this on every upstream
    /// `DBE_PROPERTY` event, mirroring C `gatePvData::propEventCB` →
    /// `runDataCB` + `setPvData` + `runValueDataCB` +
    /// `vcPostEvent(propertyEventMask())` (`gatePv.cc:1571-1607`): the
    /// attribute cache is refreshed and a property event is posted with the
    /// upstream alarm state preserved (`setStatSevr`) and the undefined
    /// control-DBR timestamp left as-is (`gatePv.cc:1594-1595`).
    ///
    /// Returns `ChannelNotFound` for record-backed PVs.
    pub async fn post_pv_property(&self, name: &str, snapshot: Snapshot) -> CaResult<()> {
        if let Some(pv) = self.inner.simple_pvs.read().await.get(name).cloned() {
            pv.set_metadata(metadata_from_snapshot(&snapshot));
            pv.post_property(snapshot).await;
            return Ok(());
        }
        Err(CaError::ChannelNotFound(name.to_string()))
    }

    /// Like `put_pv_and_post` but with explicit origin tag.
    pub async fn put_pv_and_post_with_origin(
        &self,
        name: &str,
        value: EpicsValue,
        origin: u64,
    ) -> CaResult<()> {
        let (base, field) = super::parse_pv_name(name);
        let field = field.to_ascii_uppercase();

        // Simple-PV path: PVs registered via `add_pv` (e.g. CA gateway
        // shadow PVs, IOCsh stats PVs) are stored in `simple_pvs`,
        // not `records`. Without this branch the function would
        // silently return `ChannelNotFound` for every gateway-mirrored
        // PV — `ProcessVariable::set` already does the
        // notify-subscribers fan-out internally so all we need here is
        // to delegate. The `origin` tag is a no-op for simple PVs
        // because they don't yet plumb origin through `set`.
        if let Some(pv) = self.inner.simple_pvs.read().await.get(name).cloned() {
            let _ = origin; // simple PVs don't currently honor origin tagging
            pv.set(value).await;
            return Ok(());
        }

        if let Some(rec) = self.get_record(base).await {
            // `put_pv_and_post` is a public record-write API —
            // it must take the same advisory write gate
            // (`dbScanLock` analogue) as `put_pv` /
            // `put_record_field_from_ca`, or a gateway/sequencer
            // write through this helper can still land between the
            // member writes of a QSRV atomic group or a pvalink
            // atomic scan epoch holding `lock_records`. `base` is
            // alias-resolved to the canonical record name so an alias
            // and its target share one gate. Held until return.
            let canonical_base: String = self
                .resolve_alias(base)
                .await
                .unwrap_or_else(|| base.to_string());
            let _record_gate = self.lock_record(&canonical_base).await;

            let mut instance = rec.write().await;

            // Same `dbPut` gate as `put_pv` — this is the third `dbPut` body
            // (value + monitor post), and C has ONE.
            check_no_mod(&instance, &field)?;

            let request = dbput_request(&*instance.record, &field, value)?;

            let old_value = instance.record.get_field(&field);
            let old_stat = instance.common.stat;
            let old_sevr = instance.common.sevr;
            let old_nord = array_nord_before_put(&instance, &field);

            // Link writes the record's `special()` makes itself (C runs them
            // inside `dbPut`); executed below, once the record lock is released.
            let mut special_actions = Vec::new();

            // Write value + special/on_put
            use crate::server::record::CommonFieldPutResult;
            let common_result = match request {
                // C `dbAccess.c:1370-1372` — accept, write nothing, alarm. UDF
                // is NOT cleared: C clears it at `:1409` only when the value
                // field was actually written, and this branch wrote nothing.
                PutRequest::EmptyIntoScalar => {
                    set_empty_request_alarm(&mut instance);
                    CommonFieldPutResult::NoChange
                }
                PutRequest::Write(value) => {
                    special_before_put(&mut instance, &field);
                    match instance.record.put_field(&field, value.clone()) {
                        Ok(()) => {
                            instance.record.on_put(&field);
                            // C returns the after-put special() status from
                            // `dbPut` (dbAccess.c:1399-1405) — before the UDF
                            // clear and the monitor post below, both of which
                            // `goto done` skips on a non-zero status.
                            let result =
                                special_after_put(&mut instance, &field, &mut special_actions)?;
                            // Clear UDF/UDF_ALARM on primary field write
                            if field == instance.record.primary_field() {
                                instance.common.udf = false;
                                if instance.common.stat
                                    == crate::server::recgbl::alarm_status::UDF_ALARM
                                {
                                    instance.common.stat = 0;
                                    instance.common.sevr =
                                        crate::server::record::AlarmSeverity::NoAlarm;
                                }
                            }
                            result
                        }
                        Err(CaError::FieldNotFound(_)) => {
                            instance.put_common_field(&field, value)?
                        }
                        Err(e) => return Err(e),
                    }
                }
            };

            // Invalidate metadata cache only if a metadata-class
            // field actually changed value (faac1df1 — DBE_PROPERTY
            // fires on real changes, not no-op writes).
            instance.notify_field_written_if_changed(&field, old_value.as_ref());

            // Post monitor events if value or alarm changed
            let new_value = instance.record.get_field(&field);
            let value_changed = old_value != new_value;
            let alarm_changed =
                old_stat != instance.common.stat || old_sevr != instance.common.sevr;
            let nord_changed = old_nord.is_some() && instance.record.get_field("NORD") != old_nord;
            if value_changed || alarm_changed || nord_changed {
                // Update timestamp so the snapshot carries current time
                instance.common.time = crate::runtime::general_time::get_current();
                instance.cleanup_subscribers();
                if value_changed || alarm_changed {
                    instance.notify_field_with_origin(
                        &field,
                        crate::server::recgbl::EventMask::VALUE
                            | crate::server::recgbl::EventMask::LOG
                            | crate::server::recgbl::EventMask::ALARM,
                        origin,
                    );
                }
                // The NORD post, through the one owner. Without it a CA
                // gateway forwarding upstream waveform monitors via
                // put_pv_and_post would update VAL on the shadow PV but
                // leave downstream NORD subscribers stuck at their last
                // seen length — a frozen-element-count bug that surfaces
                // in PyDM image views and similar consumers that compute
                // height = element_count / width.
                post_array_info(&mut instance, &old_nord, origin);
            }

            // The `special()` link writes re-enter the database, so the record
            // lock goes down first. C makes them inside `dbPut`, before it
            // returns to its caller.
            drop(instance);

            // Same scan-index owner every other `dbPut` path routes through:
            // a SCAN put and the SIMM↔SSCN swap (`recGblCheckSimm`) both move
            // the record between scan lists and must reach `update_scan_index`.
            match common_result {
                CommonFieldPutResult::ScanChanged {
                    old_scan,
                    new_scan,
                    phas,
                } => {
                    self.update_scan_index(&canonical_base, old_scan, new_scan, phas, phas)
                        .await;
                }
                CommonFieldPutResult::PhasChanged {
                    scan: s,
                    old_phas,
                    new_phas,
                } => {
                    self.update_scan_index(&canonical_base, s, s, old_phas, new_phas)
                        .await;
                }
                CommonFieldPutResult::NoChange => {}
            }

            self.run_special_actions(&canonical_base, &rec, special_actions)
                .await;

            // same SPC_AS parity as `put_pv` / `put_pv_no_process`
            // / the CA-write path — a gateway mirroring `.ASG` via
            // `put_pv_and_post` must still trigger per-client
            // re-eval.
            if field == "ASG" {
                crate::server::access_security::notify_asg_field_changed();
            }

            return Ok(());
        }

        Err(CaError::ChannelNotFound(name.to_string()))
    }

    /// Execute the link writes a record's `special()` queued
    /// ([`Record::take_special_actions`](crate::server::record::Record::take_special_actions)).
    ///
    /// The single consumer: every `dbPut` path in this module calls it once, at
    /// the end of the put and before any `pp(TRUE)` process cycle, which is
    /// where C runs them (`dbPut` → `dbPutSpecial(paddr, 1)` → `dbPutLink`,
    /// with `dbProcess` still ahead in `dbPutField`). The put is the root of the
    /// chain these writes start, so they get a fresh visited set, exactly like a
    /// client put entering `process_record_with_links`.
    ///
    /// Must be called with no record lock held: a `WriteDbLink` can process its
    /// target, which re-enters the database.
    async fn run_special_actions(
        &self,
        record_name: &str,
        rec: &std::sync::Arc<crate::runtime::sync::RwLock<crate::server::record::RecordInstance>>,
        actions: Vec<crate::server::record::ProcessAction>,
    ) {
        if actions.is_empty() {
            return;
        }
        let mut visited = HashSet::new();
        // A `WriteDbLink` here can land back in a `dbPut` (its target's), which
        // is the function that called us: the async cycle needs one boxed edge.
        Box::pin(self.execute_process_actions(record_name, rec, actions, &mut visited, 0)).await;
    }

    /// CA client's unified entry point for record field put.
    /// Handles DISP/PROC/PACT/LCNT checks, field put, device write, and Passive process.
    ///
    /// Acquires the record's advisory write gate
    /// (`dbScanLock` analogue) for the duration of the write.
    pub async fn put_record_field_from_ca(
        &self,
        record_name: &str,
        field: &str,
        value: EpicsValue,
    ) -> CaResult<Option<crate::runtime::sync::oneshot::Receiver<()>>> {
        self.put_record_field_from_ca_inner(record_name, field, value, true, NotifyRequest::New)
            .await
    }

    /// Variant for a caller that already owns the target
    /// record's advisory write gate — the QSRV atomic group PUT,
    /// which acquired every member-record gate up-front via
    /// [`Self::lock_records`]. The per-record `tokio::sync::Mutex`
    /// gate is NOT reentrant, so the atomic group path MUST use this
    /// `_already_locked` entry to avoid dead-locking on its own
    /// `ManyRecordWriteGuard`.
    pub async fn put_record_field_from_ca_already_locked(
        &self,
        record_name: &str,
        field: &str,
        value: EpicsValue,
    ) -> CaResult<Option<crate::runtime::sync::oneshot::Receiver<()>>> {
        self.put_record_field_from_ca_inner(record_name, field, value, false, NotifyRequest::New)
            .await
    }

    /// Fire-and-forget variant — C `dbPutField` semantics: the put
    /// processes the record but creates NO put-notify wait-set (C
    /// builds a `putNotify` only in `dbPutNotify`, i.e. for
    /// WRITE_NOTIFY). A caller that does not await the returned
    /// receiver MUST use this entry: parking a wait-set whose receiver
    /// is dropped occupies `RecordInstance::notify` until the record's
    /// async work ends (a motor's whole motion), failing every
    /// legitimate WRITE_NOTIFY on the record with ECA_PUTCBINPROG in
    /// the meantime.
    pub async fn put_record_field_from_ca_no_notify(
        &self,
        record_name: &str,
        field: &str,
        value: EpicsValue,
    ) -> CaResult<()> {
        self.put_record_field_from_ca_inner(record_name, field, value, true, NotifyRequest::None)
            .await
            .map(|_| ())
    }

    /// C `dbPut`'s alarm-acknowledge interception (`dbAccess.c:1331-1335`) —
    /// the ONLY route that may change ACKS/ACKT at runtime.
    ///
    /// ```c
    /// if (dbrType == DBR_PUT_ACKT && field_type <= DBF_DEVICE)
    ///     return putAckt(paddr, pbuffer, 1, 1, 0);
    /// else if (dbrType == DBR_PUT_ACKS && field_type <= DBF_DEVICE)
    ///     return putAcks(paddr, pbuffer, 1, 1, 0);
    /// ```
    ///
    /// The dispatch is on the DBR *request type*, not on the field: a CA client
    /// acknowledges by sending `DBR_PUT_ACKS` down its ordinary `REC` (VAL)
    /// channel. It sits ABOVE the `SPC_NOMOD` gate, which is why
    /// `caput REC.ACKS 2` is refused by C ("Write access denied", verified on
    /// softIoc 7.0.10) while `ca_put(DBR_PUT_ACKS, REC)` clears the alarm.
    ///
    /// The put-disable gate is still crossed: C tests `precord->disp` in
    /// `dbPutField`, above `dbPut` (`dbAccess.c:1255-1257`), so an ack to a
    /// `DISP=1` record is refused. `field` is the channel's field — only the
    /// DISP gate looks at it, exactly as in C.
    ///
    /// No process cycle: `dbPut` returns straight from `putAckt`/`putAcks`, and
    /// `dbPutField`'s reprocess condition requires `dbrType < DBR_PUT_ACKT`.
    pub async fn put_alarm_ack_from_ca(
        &self,
        record_name: &str,
        field: &str,
        ack: crate::server::record::AlarmAck,
        value: u16,
    ) -> CaResult<()> {
        let field_upper = field.to_ascii_uppercase();
        let rec = self
            .get_record(record_name)
            .await
            .ok_or_else(|| CaError::ChannelNotFound(record_name.to_string()))?;
        let canonical: String = self
            .resolve_alias(record_name)
            .await
            .unwrap_or_else(|| record_name.to_string());
        let _record_gate = self.lock_record(&canonical).await;

        let mut instance = rec.write().await;
        check_put_disabled(&instance, &field_upper)?;
        match ack {
            crate::server::record::AlarmAck::Transient => instance.put_ackt(value),
            crate::server::record::AlarmAck::Severity => instance.put_acks(value),
        }
        Ok(())
    }

    /// Fire-and-forget + caller-held gate: see
    /// [`Self::put_record_field_from_ca_no_notify`] and
    /// [`Self::put_record_field_from_ca_already_locked`].
    pub async fn put_record_field_from_ca_no_notify_already_locked(
        &self,
        record_name: &str,
        field: &str,
        value: EpicsValue,
    ) -> CaResult<()> {
        self.put_record_field_from_ca_inner(record_name, field, value, false, NotifyRequest::None)
            .await
            .map(|_| ())
    }

    /// C `processNotifyCommon`'s PACT arm (dbNotify.c:225-231): the put-notify
    /// landed on a busy record, so the WHOLE put is deferred — no value
    /// written, no RPRO raised, no join of the in-flight cycle's wait-set (that
    /// join is what completed the callback an entire cycle too early). The
    /// record's async completion replays it through
    /// [`Self::restart_deferred_notify_put`].
    ///
    /// A second put-notify onto a record that already owns one is C's
    /// "another processNotify owns the record" (dbNotify.c:213-217); the port
    /// reports it to the client as `PutCallbackInProgress` (C `S_db_Blocked` /
    /// `ECA_PUTCBINPROG`) rather than queueing a restart list.
    async fn defer_notify_put(
        &self,
        record_name: &str,
        rec: &std::sync::Arc<crate::runtime::sync::RwLock<crate::server::record::RecordInstance>>,
        field: String,
        value: EpicsValue,
        notify_request: NotifyRequest,
    ) -> CaResult<Option<crate::runtime::sync::oneshot::Receiver<()>>> {
        let Some((completion, completion_rx)) = notify_request.into_completion() else {
            // Fire-and-forget never reaches here: `dbPutField` on a PACT record
            // DOES write the value and set RPRO (dbAccess.c:1263-1277). Only
            // `dbPutNotify` defers.
            return Ok(None);
        };
        let mut guard = rec.write().await;
        if guard.notify.is_some() || guard.deferred_notify_put.is_some() {
            return Err(CaError::PutCallbackInProgress(record_name.to_string()));
        }
        guard.deferred_notify_put = Some(crate::server::record::DeferredNotifyPut {
            field,
            value,
            completion,
        });
        Ok(completion_rx)
    }

    /// Process a record UNCONDITIONALLY with a put-notify wait-set, returning
    /// the completion receiver — the QSRV `record[process=true,block=true]`
    /// (Force + block) barrier.
    ///
    /// C `dbProcessNotify`: pvxs routes a blocking forced put through
    /// `dbProcessNotify` (`singlesource.cpp:360-369`), whose completion fires
    /// only after the record's whole processing chain — including async device
    /// work (a motor move, an asyn-backed AO) — settles. The value is written
    /// by the caller's preceding [`Self::put_pv`] (the `dbPut` analogue, no
    /// process); this entry then mints the wait-set, registers it into the
    /// record's `notify` slot so PACT records join it, and runs the full
    /// unconditional [`Self::process_record_with_links`] cycle (C `dbProcess`,
    /// the Force analogue). A fully synchronous chain returns `Ok(None)` (the
    /// wait-set already drained); an async record returns `Ok(Some(rx))` for
    /// the caller to await. A concurrent put-callback already in flight on the
    /// record is rejected with `PutCallbackInProgress`, matching the PROC path.
    pub async fn process_record_with_notify(
        &self,
        record_name: &str,
    ) -> CaResult<Option<crate::runtime::sync::oneshot::Receiver<()>>> {
        let (completion_tx, completion_rx) = crate::runtime::sync::oneshot::channel();
        let notify = crate::server::record::NotifyWaitSet::new(completion_tx);
        {
            // Collect-then-act: clone the handle under a brief map read, drop
            // the map lock before taking the per-record write lock.
            let rec_arc = {
                let recs = self.inner.records.read().await;
                recs.get(record_name).cloned()
            };
            let Some(rec_arc) = rec_arc else {
                return Err(CaError::ChannelNotFound(record_name.to_string()));
            };
            let mut guard = rec_arc.write().await;
            if guard.notify.is_some() {
                return Err(CaError::PutCallbackInProgress(record_name.to_string()));
            }
            guard.notify = Some(notify.clone());
        }
        let mut visited = HashSet::new();
        self.process_record_with_links(record_name, &mut visited, 0)
            .await?;
        // The wait-set fires the oneshot only after the whole FLNK/OUT chain
        // (sync + async) settles. Already-completed ⟹ fully synchronous ⟹
        // report immediate success; otherwise hand the receiver back to await
        // the deferred async completion.
        if notify.completed() {
            Ok(None)
        } else {
            Ok(Some(completion_rx))
        }
    }

    /// C `dbPutField`'s put-driven process decision (`dbAccess.c:1264-1277`).
    ///
    /// Reached once the put has selected the record for processing — the `PROC`
    /// field, or a `pp(TRUE)` field on a Passive record. C then splits on PACT:
    ///
    /// * **async-active** — C sets `rpro = TRUE` and does NOT call `dbProcess`.
    ///   `recGblFwdLink` (`recGbl.c:296-300`) consumes RPRO when the device
    ///   round trip completes and queues `scanOnce`, so the value this put just
    ///   wrote still reaches the device, one cycle later. Calling `dbProcess`
    ///   here instead lands in dbProcess's own PACT guard, which bumps LCNT and
    ///   after MAX_LOCK raises SCAN_ALARM — an alarm C never raises for a client
    ///   put — while dropping the deferred reprocess entirely: on two rapid
    ///   puts to a Passive async output, C writes both values to the device and
    ///   the port wrote only the first.
    /// * **idle** — C sets `putf = TRUE` (the put-driven marker, cleared at the
    ///   tail of the process cycle / in `complete_async_record_inner`, both the
    ///   `recGblFwdLink:302` analogue) and calls `dbProcess`.
    ///
    /// Single owner of that decision for every external put: the `PROC`
    /// intercept and the `pp`-field route in
    /// [`Self::put_record_field_from_ca`] both go through it, so neither can
    /// drift from C's rule or from each other. The DB-link propagation path
    /// applies the same PACT→RPRO rule at its own targets (`processing.rs:3225`,
    /// `:4220`, `links.rs:829`).
    ///
    /// The caller already holds `record_name`'s advisory write gate (the
    /// `dbScanLock` analogue) — either `_record_gate`, or the QSRV atomic
    /// group's `lock_records` epoch when entered via
    /// `put_record_field_from_ca_already_locked`. The gate `Mutex` is not
    /// reentrant, so processing MUST use the `_already_locked` entry.
    async fn put_driven_process(&self, record_name: &str) {
        {
            let Some(rec) = self.get_record(record_name).await else {
                return;
            };
            let mut instance = rec.write().await;
            if instance.is_processing() {
                instance.common.rpro = true;
                return;
            }
            instance.common.putf = true;
        }
        let mut visited = HashSet::new();
        let _ = self
            .process_record_with_links_already_locked(record_name, &mut visited, 0)
            .await;
    }

    /// C `dbNotifyCompletion` → restart (dbNotify.c:207-231, state
    /// `notifyRestartInProgress`): replay a put-notify that landed on a PACT
    /// record, now that the record is idle. The single owner that consumes a
    /// [`DeferredNotifyPut`]; called only from the async-completion tail.
    ///
    /// The replay goes back through the ordinary put entry, so if the record
    /// has ALREADY gone active again (a scan fired between the completion and
    /// this replay), the same PACT test defers it once more rather than writing
    /// into a busy record — the deferral is closed under its own restart.
    pub(crate) async fn restart_deferred_notify_put(
        &self,
        record_name: &str,
        put: crate::server::record::DeferredNotifyPut,
    ) {
        let crate::server::record::DeferredNotifyPut {
            field,
            value,
            completion,
        } = put;
        // The client already holds the receiver; a failure here (record gone,
        // field refused) must still release it, which dropping the sender does.
        let _ = self
            .put_record_field_from_ca_inner(
                record_name,
                &field,
                value,
                true,
                NotifyRequest::Deferred(completion),
            )
            .await;
    }

    async fn put_record_field_from_ca_inner(
        &self,
        record_name: &str,
        field: &str,
        value: EpicsValue,
        acquire_gate: bool,
        notify_request: NotifyRequest,
    ) -> CaResult<Option<crate::runtime::sync::oneshot::Receiver<()>>> {
        let field = field.to_ascii_uppercase();
        let want_notify = notify_request.wants_notify();

        // Get record Arc — alias-aware (epics-base PR #336) so a CA
        // client that connects via an alias name can put fields on
        // the canonical record.
        let rec = self
            .get_record(record_name)
            .await
            .ok_or_else(|| CaError::ChannelNotFound(record_name.to_string()))?;
        // Normalise to the canonical name for the rest of this
        // function — every subsequent call (PACT/LCNT lookup,
        // `process_record_with_links`, `update_scan_index`) uses the
        // raw records map and would miss when `record_name` is an
        // alias. Resolve once up front.
        let canonical_owned;
        let record_name: &str = if let Some(target) = self.resolve_alias(record_name).await {
            canonical_owned = target;
            &canonical_owned
        } else {
            record_name
        };

        // take the record's advisory write gate — the
        // `dbScanLock(precord)` analogue. While a QSRV atomic group
        // PUT/GET holds this record's gate via `lock_records`, this
        // plain write blocks here, so a direct backing-record write
        // can no longer land between member writes of an atomic group
        // transaction. Held until the function returns. Skipped when
        // the caller (atomic group PUT) already owns the gate — the
        // gate `Mutex` is not reentrant.
        let _record_gate = if acquire_gate {
            Some(self.lock_record(record_name).await)
        } else {
            None
        };

        // Special field intercepts (read lock, then drop)
        {
            let instance = rec.read().await;

            // C `dbPutField` gate order (`dbAccess.c:1252-1277`): the DISP
            // put-disable gate runs BEFORE `dbPut` — hence before the
            // SPC_NOMOD rejection of PACT/LCNT/PUTF (`dbAccess.c:123`) — and
            // BEFORE the PROC-driven `dbProcess`. So on a `DISP=1` record
            // EVERY non-DISP field, PROC included, is refused with
            // `S_db_putDisabled` and the record does not process.
            check_put_disabled(&instance, &field)?;

            // SPC_NOMOD / read-only fields: rejected inside C's `dbPut`, i.e.
            // after the DISP gate above and before the PROC-driven process
            // below. One gate owner for every route ([`check_no_mod`]).
            check_no_mod(&instance, &field)?;

            // C `processNotifyCommon` (dbNotify.c:225-231) tests PACT ABOVE the
            // put — `if (precord->pact) { ... pnotify->state =
            // notifyRestartCallbackRequested; ... return; }` — so a put-notify
            // that lands on a busy record writes NOTHING: no value, no RPRO, no
            // join of the in-flight cycle's wait-set. The whole put is replayed
            // from `dbNotifyCompletion` when the record goes idle. Joining the
            // running cycle instead completed the callback one cycle early, on
            // work that never saw this value.
            //
            // A fire-and-forget `dbPutField` is NOT deferred: it writes and
            // raises RPRO (dbAccess.c:1263-1277). Only the notify route waits.
            if want_notify && instance.is_processing() {
                drop(instance);
                return self
                    .defer_notify_put(record_name, &rec, field, value, notify_request)
                    .await;
            }

            // PROC intercept: trigger processing on any SCAN.
            // Falls through to the put_notify_tx registration below
            // so async records (motor, asyn-backed AO) signal real
            // completion; otherwise WRITE_NOTIFY would return ECA_NORMAL
            // before the device move actually finished.
            if field == "PROC" {
                // C `dbPutField` (dbAccess.c:1265) matches the proc field by
                // pointer with NO value check: any write to PROC — including
                // 0 — processes the record (when !pact). The standard
                // `caput REC.PROC 0` / `dbpf REC.PROC 0` force-process idiom
                // must therefore not be skipped for a zero value.
                drop(instance);
                // Continue to the put-notify setup + process below
                // by jumping past the field-write step (the value
                // itself isn't stored; PROC is a trigger). A
                // fire-and-forget caller parks nothing — C `dbPutField`
                // on PROC processes the record with no putNotify.
                let parked = if let Some((completion_tx, completion_rx)) =
                    notify_request.into_completion()
                {
                    let notify = crate::server::record::NotifyWaitSet::new(completion_tx);
                    {
                        // Collect-then-act: clone the handle under a brief map
                        // read, drop the map lock before the per-record write.
                        let rec_arc = {
                            let recs = self.inner.records.read().await;
                            recs.get(record_name).cloned()
                        };
                        if let Some(rec_arc) = rec_arc {
                            let mut guard = rec_arc.write().await;
                            if guard.notify.is_some() {
                                return Err(CaError::PutCallbackInProgress(
                                    record_name.to_string(),
                                ));
                            }
                            guard.notify = Some(notify.clone());
                        }
                    }
                    Some((notify, completion_rx))
                } else {
                    None
                };
                // C `dbPutField:1265-1277`: PROC is one of the two fields that
                // selects the record for the put-driven process — with the same
                // PACT→RPRO deferral as a `pp` field. Both go through the single
                // owner.
                self.put_driven_process(record_name).await;
                // The wait-set fires the oneshot only after the whole
                // FLNK/OUT chain (sync + async) settles. If it has
                // already completed the chain was fully synchronous —
                // report immediate success; otherwise hand the receiver
                // to the CA layer to await the deferred completion.
                return match parked {
                    Some((notify, completion_rx)) => {
                        if notify.completed() {
                            Ok(None)
                        } else {
                            Ok(completion_rx)
                        }
                    }
                    None => Ok(None),
                };
            }
        }

        // Normal field put (write lock) — C `dbPut`, which does NOT touch
        // `putf`: the marker is raised only where C raises it, at the
        // put-driven process decision (`put_driven_process`).
        //
        // Link writes the record's `special()` makes itself. C runs them inside
        // `dbPut`, so they land BEFORE the `pp(TRUE)` process below — a record
        // wired to scaler `.COUTP` is processed with the scaler not yet armed
        // (scalerRecord.c:623-624, before the `:637` REQSTART).
        let mut special_actions = Vec::new();
        let common_result = {
            let mut instance = rec.write().await;

            // Coerce value to the field's native DBR type (e.g. String → Double for ao.VAL).
            // This matches C EPICS db_put_field() which converts from the CA client's type
            // to the record field's native type.
            let request = dbput_request(&*instance.record, &field, value)?;

            // Pre-write special hook (C EPICS dbPutSpecial pass=0)
            instance.record.special(&field, false)?;
            special_before_put(&mut instance, &field);

            // Capture pre-put value for faac1df1 idempotent-write suppression.
            let prev_value = instance.record.get_field(&field);
            let old_nord = array_nord_before_put(&instance, &field);

            // Try record-specific field first; fall back to common on FieldNotFound.
            // For record-owned fields, call on_put() and special() after successful put,
            // matching what put_common_field() does for common fields.
            use crate::server::record::CommonFieldPutResult;
            let common_result = match request {
                // C `dbAccess.c:1370-1372` — a zero-element request into a
                // scalar field: nothing is written, the record is driven to
                // LINK/INVALID, and `dbPut` returns 0. The client's put
                // SUCCEEDS; the record's process cycle below commits the alarm
                // and posts it, which is how a C IOC surfaces `caput -a`
                // of an empty array.
                PutRequest::EmptyIntoScalar => {
                    set_empty_request_alarm(&mut instance);
                    CommonFieldPutResult::NoChange
                }
                PutRequest::Write(value) => {
                    match instance.record.put_field(&field, value.clone()) {
                        Ok(()) => {
                            instance.record.on_put(&field);
                            // C returns the after-put special() status from
                            // `dbPut` (dbAccess.c:1399-1405); `if (status)
                            // goto done` then skips both the UDF clear below
                            // and the field's monitor post, and `dbPutField`
                            // skips the process. Propagating the error here
                            // reproduces all three.
                            let result =
                                special_after_put(&mut instance, &field, &mut special_actions)?;
                            // C `dbAccess.c::dbPut:1410-1411` clears
                            // `precord->udf = FALSE` synchronously when the
                            // put target is the record-type's primary value
                            // field (`dbIsValueField`). The clear happens
                            // BEFORE `dbProcess` runs, so any reader between
                            // the put and the process-cycle's own clear sees
                            // the new value with a consistent UDF=false.
                            //
                            // Rust's processing path also clears UDF via
                            // `clears_udf()` in process/complete_async_record,
                            // but that runs AFTER the put lock drops and the
                            // process re-acquires — leaving a small window
                            // where another reader can observe (new VAL,
                            // udf=true). For async records the window spans
                            // the entire device round trip. Clear here to
                            // close the window. The same clear already exists
                            // in `put_pv_and_post` (line 256-262); mirror it.
                            if field == instance.record.primary_field() {
                                instance.common.udf = false;
                                if instance.common.stat
                                    == crate::server::recgbl::alarm_status::UDF_ALARM
                                {
                                    instance.common.stat = 0;
                                    instance.common.sevr =
                                        crate::server::record::AlarmSeverity::NoAlarm;
                                }
                            }
                            result
                        }
                        Err(CaError::FieldNotFound(_)) => {
                            instance.put_common_field(&field, value)?
                        }
                        Err(e) => return Err(e),
                    }
                }
            };

            // Invalidate metadata cache only if the metadata-class
            // field's value actually changed (faac1df1).
            instance.notify_field_written_if_changed(&field, prev_value.as_ref());

            // `putf` is neither set nor cleared anywhere in this block: C's
            // `dbPut` does not touch it. It is raised in `put_driven_process`
            // (C `dbAccess.c:1274`) immediately before `dbProcess`, stays TRUE
            // for the whole process cycle — including an async device round
            // trip — and is cleared by the `recGblFwdLink:302` analogue at the
            // cycle's tail (`processing.rs:2997` / `complete_async_record_inner`)
            // or by the disable-alarm bail (`dbAccess.c:576`).

            instance.cleanup_subscribers();
            // C `dbPut:1408-1414` posts DBE_VALUE|DBE_LOG for the put field
            // unless `(isValueField && pfldDes->process_passive)` — the
            // immediate post is suppressed for the value field ONLY when that
            // field is `pp(TRUE)`, because then the reprocess cycle
            // (`dbPutField:1265-1268`) re-posts it via the deadband snapshot.
            // For a value field that is NOT `pp` (calc/calcout/aSub VAL), C
            // posts here and does not reprocess; the port must do the same,
            // because the `should_process` gate below skips the cycle for a
            // non-`pp` value field — without this post a direct VAL put would
            // fire no monitor at all.
            // (ACKT/ACKS have no arm here: they are SPC_NOMOD, refused by the
            // gate above. Alarm acknowledgement arrives as a DBR request type,
            // through [`Self::put_alarm_ack_from_ca`].)
            //
            // Suppress the immediate value-field post only when this put
            // will itself drive a reprocess (the cycle re-posts the field).
            // `process_passive_fields()` is total/fail-safe: a put to a
            // non-pp field — including any field of an unmodeled type
            // (`&[]`) — does not reprocess, so it is not suppressed here.
            let suppress_value_field_post = field == instance.record.primary_field()
                && instance
                    .record
                    .process_passive_fields()
                    .iter()
                    .any(|f| f.eq_ignore_ascii_case(&field));
            if !suppress_value_field_post {
                instance.notify_field(
                    &field,
                    crate::server::recgbl::EventMask::VALUE | crate::server::recgbl::EventMask::LOG,
                );
            }

            // The NORD post, through the one owner — C reaches `put_array_info`
            // from `dbPut`, so the CA route posts it exactly like the internal
            // one. It is NOT covered by the value-field post above: for a
            // waveform that post is suppressed (VAL is `pp(TRUE)`), and it is
            // not covered by the process cycle either — a `caput -a` to a
            // slow-scanned or passive-but-unprocessed waveform posts NORD now
            // and VAL only at the next scan.
            post_array_info(&mut instance, &old_nord, 0);

            // Fields a `special()` changed as a side effect of this put
            // (e.g. compress RES reset zeroing NUSE/VAL) get their monitors
            // posted here, mirroring the explicit `db_post_events` a C
            // `special()` makes — these fields are not pp(TRUE), so no
            // process cycle would otherwise post them. Each post carries
            // VALUE|LOG unless the record names the field in
            // `value_only_change_fields()` — a record whose C `special()`
            // posts the field with a literal `DBE_VALUE` (e.g. table SET,
            // tableRecord.c:659) gets the LOG bit stripped, honoring the
            // same value-only contract as the change-detection path.
            let side_effect_value_only = instance.record.value_only_change_fields();
            for sf in instance.record.monitor_side_effect_fields(&field) {
                use crate::server::recgbl::EventMask;
                let mask = if side_effect_value_only
                    .iter()
                    .any(|f| f.eq_ignore_ascii_case(sf))
                {
                    EventMask::VALUE
                } else {
                    EventMask::VALUE | EventMask::LOG
                };
                instance.notify_field(sf, mask);
            }

            // The same `special()` posts, but named by the WRITER instead of
            // by a static table: a record whose put handler re-derived a
            // partner field marks it — with the mask of the C call site that
            // posts it, and only when that field's own comparison moved
            // (sseq `special()` posts the re-rendered `STRn` after a `DOn`
            // put, `DBE_VALUE`, `only if (strcmp(str, plinkGroup->s))`,
            // sseqRecord.c:1108-1116). A static field-name list cannot
            // express "only if it changed", so it over-posts; the mark can.
            for (sf, cycle_mask) in instance.record.take_cycle_posted_fields() {
                use crate::server::record::{CyclePostMask, EventMask};
                let mask = match cycle_mask {
                    CyclePostMask::Value => EventMask::VALUE,
                    // No `monitor_mask` exists on a put path (no alarm
                    // transition is being resolved), so both LOG-carrying
                    // variants reduce to C's literal `DBE_VALUE|DBE_LOG`.
                    CyclePostMask::ValueLog | CyclePostMask::MonitorValueLog => {
                        EventMask::VALUE | EventMask::LOG
                    }
                };
                instance.notify_field(sf, mask);
            }

            common_result
        };
        // ASG-field change re-evaluation hook. C
        // `asDbLib.c:107-110,144` `asSpcAsCallback` invokes
        // `asChangeGroup` → `asAddMemberPvt` → `asComputePvt` for
        // every `ASGCLIENT` on `dbPut record.ASG NEW_ASG`. Pre-fix
        // Rust mutated `common.asg` directly with no notification,
        // so the wire ACCESS_RIGHTS the client saw still reflected
        // the OLD ASG until something else triggered re-eval. Now we
        // fire a process-wide notifier that the CA server folds into
        // its per-client `reeval_access_rights` path.
        if field == "ASG" {
            crate::server::access_security::notify_asg_field_changed();
        }
        // record lock released

        // C `dbPutField` reaches `dbProcess` only after `dbPut` — and therefore
        // after `dbPutSpecial(paddr, 1)` and every `dbPutLink` it made — has run
        // to completion. Execute them here, ahead of the `pp(TRUE)` process.
        self.run_special_actions(record_name, &rec, std::mem::take(&mut special_actions))
            .await;

        // Update scan index if SCAN or PHAS changed
        match common_result {
            crate::server::record::CommonFieldPutResult::ScanChanged {
                old_scan,
                new_scan,
                phas,
            } => {
                self.update_scan_index(record_name, old_scan, new_scan, phas, phas)
                    .await;
            }
            crate::server::record::CommonFieldPutResult::PhasChanged {
                scan: s,
                old_phas,
                new_phas,
            } => {
                self.update_scan_index(record_name, s, s, old_phas, new_phas)
                    .await;
            }
            crate::server::record::CommonFieldPutResult::NoChange => {}
        }

        // C `dbAccess.c::dbPutField:1263-1268` re-processes the
        // record on a put only when the put field is `pp(TRUE)` AND the
        // record is Passive (`SCAN == 0`). (The `PROC` field has its own
        // always-process intercept above, matching C's
        // `pfield == &precord->proc`; alarm-ack fields like ACKT/ACKS are
        // not `pp(TRUE)` so they fall out here, matching C's
        // `dbrType < DBR_PUT_ACKT`.) Processing on every put would
        // double-process scanned records and spuriously process puts to
        // non-`pp` fields (extra FLNK / monitors / device writes /
        // timestamps). `process_passive_fields()` is total and fail-safe: an
        // unmodeled type returns `&[]` (and warns once), so it processes on
        // `PROC` only — spurious processing is opt-in (a type must declare its
        // pp set), never the default.
        let should_process = {
            let instance = rec.read().await;
            put_drives_processing_of(&instance, &field)
        };

        if !should_process {
            // No processing cycle, so C never raises `putf` (and this put did
            // not either). Report immediate (synchronous) completion to a
            // WRITE_NOTIFY caller.
            return Ok(None);
        }

        // Set up the put-notify wait-set BEFORE processing. The wait-set
        // fires `completion_tx` only after the originating record AND
        // every FLNK/OUT chain target it triggers (sync or async) has
        // completed — C `dbNotify.c` `processNotify`/`dbNotifyCompletion`.
        // Refuse a second concurrent WRITE_NOTIFY on the same record:
        // C EPICS returns S_db_Blocked / ECA_PUTCBINPROG, and silently
        // overwriting the wait-set would drop the prior Sender, waking
        // the prior caller's rx with RecvError that the CA dispatcher
        // treats as success.
        //
        // A fire-and-forget put parks NOTHING — C builds a `putNotify`
        // only in `dbPutNotify`; `dbPutField` processes the record with
        // no notify state at all. It therefore neither conflicts with
        // nor disturbs a WRITE_NOTIFY already parked on the record.
        let parked = if let Some((completion_tx, completion_rx)) = notify_request.into_completion()
        {
            let notify = crate::server::record::NotifyWaitSet::new(completion_tx);
            {
                // Collect-then-act: clone the handle under a brief map read,
                // drop the map lock before the per-record write.
                let rec_arc = {
                    let recs = self.inner.records.read().await;
                    recs.get(record_name).cloned()
                };
                if let Some(rec_arc) = rec_arc {
                    let mut guard = rec_arc.write().await;
                    if guard.notify.is_some() {
                        return Err(CaError::PutCallbackInProgress(record_name.to_string()));
                    }
                    guard.notify = Some(notify.clone());
                }
            }
            Some((notify, completion_rx))
        } else {
            None
        };

        // When a CA put writes directly to VAL on an INPUT record whose
        // VAL is the engineering value, the built-in `RVAL → VAL`
        // `convert()` must be suppressed for the put-driven process —
        // re-deriving VAL from a stale RVAL would clobber the value the
        // operator just wrote (the soft ai preset-NaN case, processing.rs
        // ~line 677). The framework expresses this by calling
        // `set_device_did_compute(true)`.
        //
        // This MUST be gated on `soft_channel_skips_convert()`. Output
        // records (mbbo/mbbo_direct/bo/ao) implement
        // `set_device_did_compute` as "skip the VAL → RVAL output
        // convert" — the OPPOSITE direction. C `mbboRecord.c::process`
        // (line 217), `mbboDirectRecord.c::process` (line 198) and
        // `boRecord.c::process` (line 207) call `convert()`
        // unconditionally on every non-pact process; a CA VAL-put on an
        // output record MUST recompute RVAL/ORAW. Suppressing it there
        // left RVAL/ORAW/ORBV stale. Output records return the default
        // `false` from `soft_channel_skips_convert()`, so this gate
        // matches the identical gates in processing.rs (line 694) and
        // record_instance.rs (line 1381).
        if field == "VAL" {
            // Collect-then-act: clone the handle under a brief map read, drop
            // the map lock before the per-record write.
            let rec_arc = {
                let recs = self.inner.records.read().await;
                recs.get(record_name).cloned()
            };
            if let Some(rec_arc) = rec_arc {
                let mut guard = rec_arc.write().await;
                if guard.record.soft_channel_skips_convert() {
                    guard.record.set_device_did_compute(true);
                }
            }
        }

        // Process the record after field put — through the single owner of C's
        // `dbPutField:1269-1277` decision, so an async-active record takes the
        // RPRO deferral instead of a doomed re-entrant `dbProcess`.
        self.put_driven_process(record_name).await;

        // Is the ORIGINATING record itself still async-pending? Its
        // wait-set membership is taken + `leave`d at its own completion
        // (sync-end, or later in `complete_async_record_inner`), so a
        // lingering `notify` on its instance means its device round-trip
        // is still in flight. This gates only the originating record's
        // PUTF clear — independent of whether downstream chain targets
        // are still pending.
        //
        // A fire-and-forget put parked nothing, and a `notify` it sees
        // on the instance belongs to some other caller's WRITE_NOTIFY —
        // not evidence about THIS put. Fall through to the guarded
        // clear; its `!is_processing()` gate already preserves PUTF
        // across an async-pending device round-trip.
        let originating_pending = want_notify && {
            let rec = self.inner.records.read().await;
            if let Some(rec_arc) = rec.get(record_name) {
                rec_arc.read().await.notify.is_some()
            } else {
                false
            }
        };

        // C `recGbl.c::recGblFwdLink:302` clears `putf = FALSE` after
        // the forward-link dispatch — the marker only lives for the
        // duration of the put's processing cycle. For SYNCHRONOUS
        // completions (PACT was cleared by the time
        // `process_record_with_links` returns) clear it here. For
        // async-pending records, the clearing happens later in
        // `complete_async_record_inner` (which runs FLNK as part of
        // the completion path) so the PUTF marker survives the
        // device-write round trip.
        if !originating_pending {
            // Collect-then-act: clone the handle under a brief map read, drop
            // the map lock before the per-record write.
            let rec_arc = {
                let recs = self.inner.records.read().await;
                recs.get(record_name).cloned()
            };
            if let Some(rec_arc) = rec_arc {
                let mut guard = rec_arc.write().await;
                if !guard.is_processing() {
                    guard.common.putf = false;
                }
            }
        }

        // CA completion gates on the WHOLE chain, not just the
        // originating record: the put-notify must not report
        // done until every FLNK/OUT target it drove — including an async
        // FLNK target that the originating record's sync cycle merely
        // kicked off — has settled. `completed()` is true iff the
        // wait-set drained to zero during this call (fully synchronous
        // chain); otherwise the receiver fires later from the last
        // chain member's `leave`.
        match parked {
            Some((notify, completion_rx)) => {
                if notify.completed() {
                    Ok(None)
                } else {
                    Ok(completion_rx)
                }
            }
            None => Ok(None),
        }
    }

    /// Put a PV value without triggering process (for restore).
    pub async fn put_pv_no_process(&self, name: &str, value: EpicsValue) -> CaResult<()> {
        let (base, field) = super::parse_pv_name(name);
        let field = field.to_ascii_uppercase();

        if let Some(pv) = self.inner.simple_pvs.read().await.get(name) {
            pv.set(value).await;
            return Ok(());
        }

        // Records — alias-aware (epics-base PR #336).
        if let Some(rec) = self.get_record(base).await {
            // `put_pv_no_process` is a public record-write API
            // (autosave restore). It must take the advisory write gate
            // (`dbScanLock` analogue) so an autosave restore cannot
            // land between the member writes of a QSRV atomic group or
            // a pvalink atomic scan epoch holding `lock_records`.
            // `base` is alias-resolved so an alias and its target
            // share one gate. Held until return.
            let canonical_base: String = self
                .resolve_alias(base)
                .await
                .unwrap_or_else(|| base.to_string());
            let _record_gate = self.lock_record(&canonical_base).await;

            let mut instance = rec.write().await;
            let prev_value = instance.record.get_field(&field);
            match instance.record.put_field(&field, value.clone()) {
                Ok(()) => {}
                Err(CaError::FieldNotFound(_)) => {
                    instance.put_common_field(&field, value)?;
                }
                Err(e) => return Err(e),
            }
            // Invalidate metadata cache only if the metadata-class
            // field actually changed (faac1df1).
            instance.notify_field_written_if_changed(&field, prev_value.as_ref());
            // same SPC_AS parity as `put_pv` / the CA-write
            // path — autosave-style restores writing `.ASG` at IOC
            // startup must still trigger per-client re-eval.
            if field == "ASG" {
                crate::server::access_security::notify_asg_field_changed();
            }
            return Ok(());
        }

        Err(CaError::ChannelNotFound(name.to_string()))
    }
}

/// Project a decoded `DBR_CTRL_*` / `DBR_GR_*` snapshot's metadata fields
/// (display / control limits, enum labels) into the shadow-PV
/// [`PvMetadata`](crate::server::pv::PvMetadata) the CA gateway installs.
/// A non-metadata (TIME/STS) snapshot carries `None` in all three, which
/// clears the shadow metadata — but the gateway only ever feeds this a
/// control-class snapshot, matching C `setPvData` replacing the attribute
/// gdd wholesale from the control get/event.
fn metadata_from_snapshot(snapshot: &Snapshot) -> crate::server::pv::PvMetadata {
    crate::server::pv::PvMetadata {
        display: snapshot.display.clone(),
        control: snapshot.control.clone(),
        enums: snapshot.enums.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::PvDatabase;
    use crate::types::EpicsValue;

    /// Regression: prior to fixing B1, `put_pv_and_post` walked only
    /// `inner.records` and returned `ChannelNotFound` for everything
    /// `add_pv`-registered. The CA gateway's monitor forwarder uses
    /// `add_pv` then expects `put_pv_and_post` to fan-out to
    /// downstream subscribers — without the simple-PV branch, every
    /// upstream event was silently dropped and the gateway delivered
    /// no monitors.
    #[tokio::test]
    async fn put_pv_and_post_handles_simple_pv() {
        let db = PvDatabase::new();
        db.add_pv("gw:test", EpicsValue::Double(0.0)).await.unwrap();

        // Should NOT return ChannelNotFound.
        db.put_pv_and_post("gw:test", EpicsValue::Double(42.0))
            .await
            .expect("simple PV put_pv_and_post must succeed");

        // Value actually landed.
        let pv = db.find_pv("gw:test").await.expect("PV exists");
        assert!(matches!(pv.get().await, EpicsValue::Double(v) if v == 42.0));
    }

    /// Regression: `get_pv`, `put_pv`, `put_pv_and_post`,
    /// and `put_pv_no_process` all bypassed `get_record` and walked
    /// `self.inner.records` directly, so alias names from epics-base
    /// PR #336 silently returned `ChannelNotFound`. A later fix closed
    /// `get_record` but the same defect was hiding in field_io.rs.
    /// All four CA-server-and-bridge entry points must accept aliases.
    #[tokio::test]
    async fn field_io_entry_points_accept_aliases() {
        use crate::server::records::ai::AiRecord;

        let db = PvDatabase::new();
        db.add_record("CANON", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        db.add_alias("ALT", "CANON").await.unwrap();

        // get_pv via alias
        db.put_pv("CANON.VAL", EpicsValue::Double(1.5))
            .await
            .unwrap();
        let v = db.get_pv("ALT.VAL").await.unwrap();
        assert!(matches!(v, EpicsValue::Double(x) if x == 1.5));

        // put_pv via alias
        db.put_pv("ALT.VAL", EpicsValue::Double(7.0)).await.unwrap();
        let v = db.get_pv("CANON.VAL").await.unwrap();
        assert!(matches!(v, EpicsValue::Double(x) if x == 7.0));

        // put_pv_and_post via alias
        db.put_pv_and_post("ALT.VAL", EpicsValue::Double(11.0))
            .await
            .unwrap();
        let v = db.get_pv("CANON.VAL").await.unwrap();
        assert!(matches!(v, EpicsValue::Double(x) if x == 11.0));

        // put_pv_no_process via alias
        db.put_pv_no_process("ALT.VAL", EpicsValue::Double(13.0))
            .await
            .unwrap();
        let v = db.get_pv("ALT.VAL").await.unwrap();
        assert!(matches!(v, EpicsValue::Double(x) if x == 13.0));
    }

    /// A `DBR_STRING` menu label written to a `DBF_MENU` field resolves
    /// against THAT field's own menu (C `dbConvert` `putStringMenu`), not the
    /// field-blind global table that `EpicsValue::convert_to` would consult.
    /// Covers the `put_pv` (`put_pv_inner`) and `put_pv_and_post` coercion
    /// sites; the CA field-put path (`put_record_field_from_ca_inner`) shares
    /// the identical `coerce_write_value` helper.
    #[tokio::test]
    async fn write_path_menu_label_resolves_against_field_menu() {
        use crate::server::records::sel::SelRecord;

        let db = PvDatabase::new();
        db.add_record("SEL", Box::new(SelRecord::default()))
            .await
            .unwrap();

        // put_pv (put_pv_inner): "Specified" is selSELM index 0, NOT the
        // menuFanout index 1 the global table would have returned.
        db.put_pv("SEL.SELM", EpicsValue::String("Specified".into()))
            .await
            .unwrap();
        assert_eq!(db.get_pv("SEL.SELM").await.unwrap(), EpicsValue::Enum(0));

        // put_pv_and_post: a later choice, proving the whole menu.
        db.put_pv_and_post("SEL.SELM", EpicsValue::String("High Signal".into()))
            .await
            .unwrap();
        assert_eq!(db.get_pv("SEL.SELM").await.unwrap(), EpicsValue::Enum(1));

        // A bare numeric string still resolves (C epicsParseUInt16 fallback).
        db.put_pv("SEL.SELM", EpicsValue::String("2".into()))
            .await
            .unwrap();
        assert_eq!(db.get_pv("SEL.SELM").await.unwrap(), EpicsValue::Enum(2));
    }

    /// `set_pv_metadata` installs the upstream `DBR_CTRL_*` metadata on a
    /// shadow simple PV WITHOUT posting any event (the CA gateway's
    /// connect-time seed). A later GET-class read must then see the
    /// installed limits/units, and a `DBE_PROPERTY` subscriber must NOT
    /// have received anything (nothing *changed* yet). An unknown / record
    /// name is rejected with `ChannelNotFound`.
    #[tokio::test]
    async fn set_pv_metadata_installs_without_posting() {
        use crate::error::CaError;
        use crate::server::snapshot::{DisplayInfo, Snapshot};
        use crate::types::DbFieldType;
        use std::time::SystemTime;

        let db = PvDatabase::new();
        db.add_pv("gw:meta", EpicsValue::Double(0.0)).await.unwrap();

        // A DBE_PROPERTY subscriber attached BEFORE the seed — it must stay
        // empty, because seeding metadata is not a property *change*.
        const DBE_PROPERTY: u16 = 8;
        let pv = db.find_pv("gw:meta").await.expect("PV exists");
        let mut prop_rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_PROPERTY)
            .await
            .expect("subscriber added");

        // Build a CTRL-class snapshot carrying display metadata.
        let mut ctrl = Snapshot::new(EpicsValue::Double(0.0), 0, 0, SystemTime::UNIX_EPOCH);
        ctrl.display = Some(DisplayInfo {
            units: "mm".into(),
            precision: 3,
            upper_disp_limit: 10.0,
            lower_disp_limit: -10.0,
            ..Default::default()
        });

        db.set_pv_metadata("gw:meta", &ctrl)
            .await
            .expect("simple PV set_pv_metadata must succeed");

        // The metadata landed on the shadow PV.
        let installed = pv.metadata();
        assert_eq!(
            installed.display.expect("display metadata installed").units,
            "mm"
        );

        // No event was posted (seed != change).
        assert!(
            prop_rx.try_recv().is_err(),
            "set_pv_metadata must not post a DBE_PROPERTY event"
        );

        // Unknown / non-simple PV is rejected.
        assert!(matches!(
            db.set_pv_metadata("no:such:pv", &ctrl).await,
            Err(CaError::ChannelNotFound(_))
        ));
    }

    /// `post_pv_property` refreshes the shadow metadata AND posts a
    /// `DBE_PROPERTY` event carrying the supplied snapshot's metadata,
    /// upstream status/severity, and (undefined control-DBR) timestamp — to
    /// `DBE_PROPERTY` subscribers only. This is the DB-routing layer the
    /// gateway's property monitor drives on every upstream `DBE_PROPERTY`
    /// event. An unknown / record name is rejected with `ChannelNotFound`.
    #[tokio::test]
    async fn post_pv_property_refreshes_and_posts_property_event() {
        use crate::error::CaError;
        use crate::server::snapshot::{DisplayInfo, Snapshot};
        use crate::types::{DbFieldType, WallTime};

        const DBE_PROPERTY: u16 = 8;
        const DBE_VALUE: u16 = 1;
        const MAJOR: u16 = 2;
        const HIGH: u16 = 3;

        let db = PvDatabase::new();
        db.add_pv("gw:prop", EpicsValue::Double(0.0)).await.unwrap();
        let pv = db.find_pv("gw:prop").await.expect("PV exists");

        let mut prop_rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_PROPERTY)
            .await
            .expect("property subscriber added");
        let mut val_rx = pv
            .add_subscriber(2, DbFieldType::Double, DBE_VALUE)
            .await
            .expect("value subscriber added");

        // Upstream CTRL event: metadata + MAJOR/HIGH alarm + a fixed past
        // timestamp that is unmistakably not a fresh wall clock.
        let upstream_ts = WallTime::from_unix(2_000_000, 0);
        let mut ctrl = Snapshot::new(EpicsValue::Double(5.0), HIGH, MAJOR, upstream_ts);
        ctrl.display = Some(DisplayInfo {
            units: "V".into(),
            precision: 1,
            ..Default::default()
        });

        db.post_pv_property("gw:prop", ctrl)
            .await
            .expect("simple PV post_pv_property must succeed");

        // The metadata was refreshed on the shadow PV.
        assert_eq!(
            pv.metadata().display.expect("metadata refreshed").units,
            "V"
        );

        // The DBE_PROPERTY subscriber received the metadata-bearing event,
        // with the upstream alarm and timestamp preserved.
        let ev = prop_rx
            .try_recv()
            .expect("DBE_PROPERTY subscriber receives the property event");
        assert_eq!(
            ev.snapshot.display.expect("event carries metadata").units,
            "V"
        );
        assert_eq!(
            ev.snapshot.alarm.severity, MAJOR,
            "upstream severity preserved"
        );
        assert_eq!(ev.snapshot.alarm.status, HIGH, "upstream status preserved");
        assert_eq!(
            ev.snapshot.timestamp, upstream_ts,
            "control-DBR timestamp preserved, not a fresh wall clock"
        );

        // The DBE_VALUE-only subscriber must NOT receive a property event.
        assert!(
            val_rx.try_recv().is_err(),
            "DBE_VALUE-only subscriber must not receive a property post"
        );

        // Unknown / non-simple PV is rejected.
        let again = Snapshot::new(EpicsValue::Double(0.0), 0, 0, WallTime::UNIX_EPOCH);
        assert!(matches!(
            db.post_pv_property("no:such:pv", again).await,
            Err(CaError::ChannelNotFound(_))
        ));
    }

    /// Regression: `put_record_field_from_ca` (the CA
    /// server's main put fast path) must accept aliases. Pre-fix it
    /// only consulted `inner.records` directly. Also exercises the
    /// canonical-name normalisation that protects subsequent
    /// `process_record_with_links` / `update_scan_index` calls.
    #[tokio::test]
    async fn put_record_field_from_ca_accepts_alias() {
        use crate::server::records::ai::AiRecord;

        let db = PvDatabase::new();
        db.add_record("CANON", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        db.add_alias("ALT", "CANON").await.unwrap();

        // Put VAL via the alias name.
        let _ = db
            .put_record_field_from_ca("ALT", "VAL", EpicsValue::Double(2.5))
            .await
            .expect("put via alias must succeed");

        // Read back via canonical to confirm the value landed on the
        // right record.
        let v = db.get_pv("CANON.VAL").await.unwrap();
        assert!(matches!(v, EpicsValue::Double(x) if x == 2.5));
    }

    /// Regression: a DBR_PUT_ACKT alarm-acknowledge put posts a record-wide
    /// DBE_ALARM (C `dbAccess.c:1299` putAckt
    /// `db_post_events(precord, NULL, DBE_ALARM)`), so an alarm-mask monitor
    /// on ANY field is notified — and a DBE_VALUE-only monitor is not.
    /// Pre-fix the ack field posted only itself with DBE_VALUE|DBE_LOG, so no
    /// alarm-mask subscriber observed the acknowledgement, and the post fired
    /// on every put regardless of whether `ackt` changed.
    #[tokio::test]
    async fn alarm_ack_put_posts_record_wide_dbe_alarm() {
        use crate::server::recgbl::EventMask;
        use crate::server::records::ai::AiRecord;
        use crate::types::DbFieldType;

        let db = PvDatabase::new();
        db.add_record("A:REC", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        let rec = db.get_record("A:REC").await.expect("record exists");

        let (mut alarm_rx, mut value_rx) = {
            let mut inst = rec.write().await;
            let a = inst
                .add_subscriber("VAL", 1, DbFieldType::Double, EventMask::ALARM.bits())
                .expect("alarm subscriber");
            let v = inst
                .add_subscriber("VAL", 2, DbFieldType::Double, EventMask::VALUE.bits())
                .expect("value subscriber");
            (a, v)
        };

        // The client acknowledges through its ordinary VAL channel with a
        // DBR_PUT_ACKT request type (C `dbAccess.c:1331`). ACKT defaults YES
        // (true), so writing 0 (disable transient acknowledgement) is a real
        // change.
        db.put_alarm_ack_from_ca(
            "A:REC",
            "VAL",
            crate::server::record::AlarmAck::Transient,
            0,
        )
        .await
        .expect("ackt put");

        // The alarm-mask monitor on VAL receives the record-wide DBE_ALARM.
        assert!(
            alarm_rx.try_recv().is_ok(),
            "DBE_ALARM subscriber must receive the record-wide alarm post"
        );
        // The DBE_VALUE-only monitor on VAL must NOT: VAL's value is unchanged.
        assert!(
            value_rx.try_recv().is_err(),
            "DBE_VALUE-only subscriber must not receive the alarm post"
        );

        // Re-putting the same ACKT value is a no-op: C putAckt returns early
        // on an unchanged ackt, so no further alarm post fires.
        db.put_alarm_ack_from_ca(
            "A:REC",
            "VAL",
            crate::server::record::AlarmAck::Transient,
            0,
        )
        .await
        .expect("ackt re-put");
        assert!(
            alarm_rx.try_recv().is_err(),
            "unchanged ACKT must post nothing"
        );
    }

    /// `post_property_fields` writes each field through the internal put and
    /// posts a `DBE_PROPERTY` monitor — the C
    /// `db_post_events(precord, &precord->val, DBE_PROPERTY)` that asyn's
    /// runtime enum re-propagation drives (devAsynInt32.c callbackEnum). A
    /// `DBE_VALUE`-only subscriber on the same field must NOT receive it:
    /// re-keying enum strings is a property change, not a value change.
    #[tokio::test]
    async fn post_property_fields_writes_and_posts_dbe_property_only() {
        use crate::server::recgbl::EventMask;
        use crate::server::records::mbbi::MbbiRecord;
        use crate::types::DbFieldType;

        let db = PvDatabase::new();
        db.add_record("M:ENUM", Box::new(MbbiRecord::new(0)))
            .await
            .unwrap();
        let rec = db.get_record("M:ENUM").await.expect("record exists");

        let (mut prop_rx, mut val_rx) = {
            let mut inst = rec.write().await;
            let p = inst
                .add_subscriber("ZRST", 1, DbFieldType::String, EventMask::PROPERTY.bits())
                .expect("property subscriber");
            let v = inst
                .add_subscriber("ZRST", 2, DbFieldType::String, EventMask::VALUE.bits())
                .expect("value subscriber");
            (p, v)
        };

        let posted = db
            .post_property_fields(
                "M:ENUM",
                vec![("ZRST".to_string(), EpicsValue::String("LABEL".into()))],
            )
            .await
            .expect("post_property_fields succeeds");
        assert_eq!(posted, vec!["ZRST".to_string()]);

        // The field landed on the record.
        assert_eq!(
            db.get_pv("M:ENUM.ZRST").await.unwrap(),
            EpicsValue::String("LABEL".into())
        );

        // The DBE_PROPERTY subscriber received the event; the DBE_VALUE-only
        // subscriber did not (mask 0x08 vs 0x01, no intersection).
        assert!(
            prop_rx.try_recv().is_ok(),
            "DBE_PROPERTY subscriber must receive the property post"
        );
        assert!(
            val_rx.try_recv().is_err(),
            "DBE_VALUE-only subscriber must not receive a property post"
        );
    }

    /// Regression: a direct CA put to a record whose value field VAL is NOT
    /// `pp(TRUE)` (calc / calcout / aSub) must still fire a DBE_VALUE monitor.
    /// C `dbAccess.c::dbPut:1408-1414` posts the value field immediately
    /// unless it is `pp(TRUE)`. The port previously suppressed the immediate
    /// post for every `VAL` and — with the `should_process` gate — skipped
    /// the reprocess cycle for a non-`pp` VAL, so the operator's write fired
    /// no monitor at all. calc's VAL is not in its `pp` field set, so the
    /// immediate post is the only event that can fire.
    #[tokio::test]
    async fn ca_put_to_non_pp_val_posts_monitor() {
        use crate::server::database::db_access::DbSubscription;
        use crate::server::records::calc::CalcRecord;

        let db = PvDatabase::new();
        db.add_record("CALC1", Box::new(CalcRecord::new("0")))
            .await
            .unwrap();

        let mut sub = DbSubscription::subscribe(&db, "CALC1.VAL")
            .await
            .expect("subscribe to CALC1.VAL");

        db.put_record_field_from_ca("CALC1", "VAL", EpicsValue::Double(5.0))
            .await
            .expect("CA put to CALC1.VAL must succeed");

        let got = tokio::time::timeout(std::time::Duration::from_secs(1), sub.recv_f64())
            .await
            .expect("a DBE_VALUE monitor must fire for a direct VAL put to a non-pp record");
        assert_eq!(got, Some(5.0));
    }

    /// R8-22 (record-field path): a record monitor whose event queue runs short
    /// of room during a burst must receive its EARLIER DISTINCT queued updates
    /// and then a tail entry carrying the latest value. C `db_queue_event_log`
    /// replaces only `*pLastLog` (`dbEvent.c:812-820`); the earlier entries stay
    /// queued and each is delivered by `event_read`.
    ///
    /// Each non-pp `VAL` put posts exactly one DBE_VALUE monitor with the put
    /// value and does NOT reprocess (see `ca_put_to_non_pp_val_posts_monitor`),
    /// so N distinct puts produce a strictly increasing 1..=N stream.
    ///
    /// Before the fix the producer parked the newest value in a side coalesce
    /// slot and `next_event`, finding it set, discarded the whole queued backlog
    /// — the burst came out as one event instead of {1..=appended-1, N}.
    #[tokio::test]
    async fn r8_22_db_burst_keeps_earlier_distinct_updates() {
        use crate::server::database::db_access::DbSubscription;
        use crate::server::event_queue::{event_que_size, events_per_que};
        use crate::server::records::calc::CalcRecord;

        let db = PvDatabase::new();
        db.add_record("CALC1", Box::new(CalcRecord::new("0")))
            .await
            .unwrap();
        let mut sub = DbSubscription::subscribe(&db, "CALC1.VAL")
            .await
            .expect("subscribe to CALC1.VAL");

        // With no consumer draining, the first `appended` puts take ring entries
        // and every later put replaces the tail entry in place.
        let appended = event_que_size() - events_per_que();
        let burst = appended + 40;
        for i in 1..=burst {
            db.put_record_field_from_ca("CALC1", "VAL", EpicsValue::Double(i as f64))
                .await
                .expect("CA put to CALC1.VAL must succeed");
        }

        // Drain every immediately-available delivery; the recv past the
        // last event has nothing queued and times out, ending collection.
        let mut seq = Vec::new();
        while let Ok(Some(v)) =
            tokio::time::timeout(std::time::Duration::from_millis(200), sub.recv_f64()).await
        {
            seq.push(v);
        }
        let want: Vec<f64> = (1..appended)
            .map(|i| i as f64)
            .chain(std::iter::once(burst as f64))
            .collect();
        assert_eq!(
            seq, want,
            "record burst delivery must be {{earlier distinct backlog…, coalesced tail}}"
        );
    }
}
