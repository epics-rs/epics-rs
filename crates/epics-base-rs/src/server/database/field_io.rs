use std::collections::HashSet;

use crate::error::{CaError, CaResult};
use crate::server::snapshot::Snapshot;
use crate::types::EpicsValue;

use super::PvDatabase;

/// pvxs's `record._options.process` term (`ioc/iocsource.cpp:426-448`)
/// as the database sees it: how much processing an EXTERNAL client put
/// drives. The one enum behind both PVA sources and the QSRV group put,
/// so `True`/`False`/`Unset` are spelled once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProcessMode {
    /// pvxs `Unset` — process only when the record's own rules say so
    /// (C `dbPutField`'s `pp(TRUE)` + `SCAN=Passive` test).
    #[default]
    Passive,
    /// pvxs `True` — force a processing cycle after the write.
    Force,
    /// pvxs `False` — write the field and stop.
    Inhibit,
}

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
    if instance.common.disp != 0 && field_upper != "DISP" {
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
/// The declaration itself lives in [`RecordInstance::is_no_mod`](crate::server::record::RecordInstance::is_no_mod), which C
/// exposes as `dbChannelSpecial(...) == SPC_NOMOD` and reads from TWO places:
/// this gate (`dbPut`, dbAccess.c:123-126) and `rsrvCheckPut`
/// (camessage.c:2608-2619), which feeds the CA ACCESS_RIGHTS write bit. This
/// function is the first consumer; `epics-ca-rs`'s `compute_access` is the
/// second.
///
/// The one thing that legitimately changes ACKS/ACKT is C's alarm
/// acknowledgement, and it does NOT come through here: `dbPut` dispatches on the
/// DBR *request type* (`DBR_PUT_ACKT`/`DBR_PUT_ACKS`, `dbAccess.c:1331-1335`)
/// ABOVE this gate, into [`RecordInstance::put_ackt`](crate::server::record::RecordInstance::put_ackt) /
/// [`RecordInstance::put_acks`](crate::server::record::RecordInstance::put_acks). The wire route for that is
/// [`PvDatabase::put_alarm_ack_from_ca`].
///
/// `field` must already be upper-cased.
fn check_no_mod(instance: &crate::server::record::RecordInstance, field: &str) -> CaResult<()> {
    if instance.is_no_mod(field) {
        return Err(CaError::ReadOnlyField(field.to_string()));
    }
    Ok(())
}

/// C `dbPut`'s link-field refusal (`field_type > DBF_DEVICE` →
/// `S_db_badDbrtype`, `dbAccess.c:1340-1347`): only `dbPutField` may change a
/// DBF_INLINK/OUTLINK/FWDLINK field — it routes them through `dbPutFieldLink`
/// (`dbAccess.c:1261-1262`) — so every `dbPut`-analogue body refuses them
/// before converting anything. This is what stops a record's DB OUT link
/// (`dbPutLink` → `dbDbPutValue` → `dbPut`) from silently rewiring another
/// record's link field on every process. The port's `dbPutField` analogue is
/// the `put_record_field_from_ca` family, whose ordinary write path re-parses
/// the link (`RecordInstance::put_common_field`'s INP/OUT/FLNK arms);
/// `put_pv_no_process` is the autosave-restore entry whose C analogue is
/// likewise `dbPutField` (`reboot_restore`), so it stays link-writable.
///
/// `field` must already be upper-cased.
fn check_not_link_field(
    instance: &crate::server::record::RecordInstance,
    field: &str,
) -> CaResult<()> {
    if crate::types::dbf_link_class(instance.record.record_type(), field).is_some() {
        return Err(CaError::BadDbrType(format!(
            "dbPut: {field} is a link field; only dbPutField changes link fields"
        )));
    }
    Ok(())
}

/// Does an *external* put to `field` drive a processing cycle on this record?
///
/// C `dbPutField` (`dbAccess.c:1263-1268`) and pvxs `IOCSource::
/// doPostProcessing` (`iocsource.cpp:397-403`) ask the same question with the
/// same terms as C `processNotifyCommon` (dbNotify.c:243-246): the `PROC` field
/// always, else a `pp(TRUE)` field on a Passive record. (`dbrType <
/// DBR_PUT_ACKT` is subsumed: the alarm-ack fields are not `pp(TRUE)`.) A caller
/// that FORCES processing (`record._options.process=true`) does not consult this
/// at all — force is the caller's own term, not the record's.
///
/// `PROC` and `UDF` are the ONLY two `dbCommon` `pp(TRUE)` fields
/// (`dbCommon.dbd.pod`: PROC line 243, UDF line 552); every other `pp(TRUE)`
/// field is declared per record TYPE and reached through
/// [`Record::processes_after_put`](crate::server::record::Record::processes_after_put). Because the two `dbCommon` fields are NOT in
/// any type's `process_passive_fields()` table, they are named here directly:
/// `PROC` unconditionally (force-process on any SCAN), `UDF` on the Passive
/// branch (an ordinary `pp` field, so it processes only when `SCAN == 0`, unlike
/// PROC). Both `dbCommon` `pp(TRUE)` fields are thus handled at this one owner
/// gate, uniformly for every record type. A put to UDF is accepted+stored by
/// `put_common_field`; this gate only adds the process cycle, after which the
/// record recomputes alarms → NO_ALARM (C ends STAT/SEVR=NO_ALARM likewise).
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
            && (field == "UDF" || instance.record.processes_after_put(field)))
}

/// Drain the record's per-cycle post marks ([`Record::take_cycle_posted_fields`](crate::server::record::Record::take_cycle_posted_fields))
/// into monitor posts — the put-path counterpart of the `db_post_events` calls a
/// C `special()` makes by hand.
///
/// One owner, so the two put-path drains (the normal tail, and the failing
/// `special()` above) cannot disagree about the mask mapping.
fn emit_cycle_posts(instance: &mut crate::server::record::RecordInstance) {
    use crate::server::record::{CyclePostMask, EventMask};
    for (sf, cycle_mask) in instance.record.take_cycle_posted_fields() {
        let mask = match cycle_mask {
            CyclePostMask::Value => EventMask::VALUE,
            // No `monitor_mask` exists on a put path (no alarm transition is
            // being resolved), so both LOG-carrying variants reduce to C's
            // literal `DBE_VALUE|DBE_LOG`.
            CyclePostMask::ValueLog | CyclePostMask::MonitorValueLog => {
                EventMask::VALUE | EventMask::LOG
            }
        };
        instance.notify_field(sf, mask);
    }
}

/// The registry half of a SNAM `special()` — C `subRecord.c::special`
/// (`:170-195`) and `aSubRecord.c::special` (`:552-578`), which resolve
/// `prec->snam` through `registryFunctionFind` and assign `prec->sadr`.
///
/// C runs it as `dbPutSpecial(paddr, 1)`, AFTER the field is stored
/// (`dbAccess.c:1355-1404`), so the name resolved is the STORED one:
/// `putStringString` truncates a DBF_STRING put to `field_size - 1` — 39 for
/// sub's `size(40)`, 40 for aSub's `size(41)` — and C looks up whatever
/// survived that. Resolving the value the caller handed in would bind a
/// routine whose name the record does not hold.
///
/// `sadr` takes the lookup result UNCONDITIONALLY, NULL included, and only then
/// does the status decide: a non-empty unregistered name is `S_db_BadSub`,
/// which `dbPut` adopts as the put's status (`if (status2) status = status2;`)
/// and whose `goto done` skips the UDF clear, the field's monitor post and — in
/// `dbPutField` — the process. The name stays stored either way.
///
/// INVARIANT: `RecordInstance::subroutine` is the registry resolution of the
/// record's current SNAM, C's `prec->sadr`. Three owners perform that
/// transition and nothing else under `src/` writes the field:
///
/// - `IocApp` (`ioc_app.rs`) and `IocBuilder` (`ioc_builder.rs`) at iocInit —
///   C's `init_record`, a different function running the same lookup.
/// - `apply_asub_dynamic_sub` (`processing.rs`) for aSub `LFLG=READ`, whose C
///   rule deliberately differs: `fetch_values` (`aSubRecord.c:262-266`) returns
///   `S_db_BadSub` BEFORE assigning `sadr`, so READ mode KEEPS the old routine
///   on an unregistered name. Routing it through this owner would clear it.
/// - this function, for every `dbPut` route, because [`special_after_put`] is
///   the one caller and every route goes through that.
///
/// `RecordInstance::new` initialises the field to `None`, which is the initial
/// state and not a transition. `put_pv_no_process` (the autosave-restore entry)
/// writes SNAM while running NEITHER `special()` pass, so it leaves the binding
/// stale — a whole missing `dbPutSpecial`, not this rule's half. The remaining
/// writers are `#[test]` fixtures binding a routine without a registry.
fn snam_special_after_put(
    db: &PvDatabase,
    instance: &mut crate::server::record::RecordInstance,
    field: &str,
) -> CaResult<()> {
    if !instance.record.is_subroutine_name_field(field) {
        return Ok(());
    }
    let Some(EpicsValue::String(stored)) = instance.record.get_field(field) else {
        return Ok(());
    };
    let name = stored.as_str_lossy();
    if name.is_empty() {
        // aSub: `pfunc = 0` with no error, so `caput X.SNAM ""` unbinds and
        // succeeds (`aSubRecord.c:560-561`, stored at `:575`). sub's C leaves
        // `sadr` alone and parks PACT instead (`subRecord.c:182-186`); this
        // port clears, because
        // the put-side park is not implemented here and a retained routine would
        // keep RUNNING every scan where C's parked record does nothing at all.
        instance.subroutine = None;
        return Ok(());
    }
    let resolved = db.find_subroutine_named(name.as_ref());
    let bad_sub = resolved.is_none();
    instance.subroutine = resolved;
    if bad_sub {
        return Err(CaError::BadField("SNAM: Subroutine not found".into()));
    }
    Ok(())
}

/// C `dbPutSpecial(paddr, 1)` — the after-put `special()`, paired with the drain
/// of the link writes it queued ([`Record::take_special_actions`](crate::server::record::Record::take_special_actions)).
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
    db: &PvDatabase,
    instance: &mut crate::server::record::RecordInstance,
    field: &str,
    out: &mut Vec<crate::server::record::ProcessAction>,
) -> CaResult<crate::server::record::CommonFieldPutResult> {
    let mut status = instance.record.special(field, true);
    // The record's half above and the registry half here are ONE C function,
    // `special(paddr, 1)`, so they share the action drain, the error-path post
    // emission and the status. The record cannot do the lookup itself — the
    // function registry belongs to the database, not to the record.
    if status.is_ok() {
        status = snam_special_after_put(db, instance, field);
    }
    out.extend(instance.record.take_special_actions());
    if status.is_err() {
        // The POSTS `special()` made are drained on the failing path for the same
        // reason its ACTIONS are: C's `special()` calls `db_post_events` BEFORE it
        // returns nonzero — aCalcout's NUSE arm posts the clamped value with
        // `DBE_VALUE` and only then `return (-1)` (`aCalcoutRecord.c:495-499`) —
        // and `dbPut`'s `goto done` skips only dbPut's OWN post. A refused put
        // that repaired a field must still tell the subscribers what it repaired.
        emit_cycle_posts(instance);
    }
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
    // C `subRecord.c::special` pass 1 (`:189-193`): the value is stored, so ask
    // again — an SNAM that is still empty re-parks PACT, a real one leaves the
    // record released by `special_before_put`. Only `enter_pact` here; C's pass 1
    // never clears PACT, and neither may this.
    if pact_park_field(&*instance.record, field) && instance.record.parks_pact() {
        instance.enter_pact();
    }

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
///
/// The other pass-0 body is `subRecord.c::special`'s park release:
/// `if (prec->snam[0] == 0 && prec->pact) { prec->pact = FALSE; prec->rpro =
/// FALSE; }` (`subRecord.c:175-178`) — the record is parked exactly while it
/// cannot process, so the store about to happen gets a clean slate and
/// [`special_after_put`] re-takes the park if the NEW value still leaves the
/// record unable to run. The record cannot reach PACT, so it answers
/// [`Record::parks_pact`](crate::server::record::Record::parks_pact) and this performs the transition; the returned
/// [`PactExit`](crate::server::record::PactExit) carries any put-notify the release freed.
fn special_before_put(
    instance: &mut crate::server::record::RecordInstance,
    field: &str,
) -> Option<crate::server::record::PactExit> {
    if field == "SIMM" {
        instance.rec_gbl_save_simm();
    }
    if pact_park_field(&*instance.record, field)
        && instance.record.parks_pact()
        && instance.is_processing()
    {
        instance.common.rpro = 0;
        return Some(instance.leave_pact());
    }
    None
}

/// Who arms the queued put-notify restart that a PACT release owes.
///
/// C reaches `restartCheck` only from `dbNotifyCompletion` ← `recGblFwdLink`
/// (`recGbl.c:295`) — the tail of a process CYCLE, never the `pact = FALSE`
/// store itself. A put body that arms it directly is therefore only correct
/// when no cycle follows the put; when one does, the replay races it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RestartOwner {
    /// The put is the whole transaction. Nothing else will reach the record,
    /// so the put body is the only site that can arm the restart.
    ThisPut,
    /// The caller drives a process cycle on the SAME record the moment this
    /// put returns — an OUT link's `processTarget`, QSRV's `Force` mode. That
    /// cycle's tail is C's owner; arming here lets the replay take the
    /// record's gate first and the record then processes the replayed put
    /// BEFORE the put that released the park (measured: `bump` twice, VAL 5.0
    /// where C gives 4.0).
    TheCycleThisPutOwes,
}

/// The finalizer for the PACT release [`special_before_put`] performs.
///
/// Declared BEFORE the `rec.write()` guard at every put body, so Rust's
/// reverse-declaration drop order puts the record's DATA lock down first and
/// this second. `PvDatabase::apply_pact_exit` re-enters the record it is handed;
/// `parking_lot::RwLock` is not reentrant, so arming the restart from inside the
/// put's own write guard is a deadlock, not an error.
///
/// A guard and not a call because the release sits ABOVE the fallible tail of
/// the put — `special_after_put`, `put_common_field`, the rejected-conversion
/// `Err` — and C `dbNotifyCompletion` is reached from `recGblFwdLink` on EVERY
/// path that ends the cycle. Nothing reports a token that never reaches the
/// consumer: [`PactExit`](crate::server::record::PactExit) has no `Drop`, and
/// its `#[must_use]` fires only on an unused *expression*, never on a
/// `let`-bound token left behind by a `?`. The guard is the whole enforcement.
///
/// The cycle body has the same debt at a different scope; its owner is
/// `processing::CycleEndGuard`, which pays the full `end_process_cycle` tail
/// rather than the restart drain alone.
struct PactExitGuard<'a> {
    db: &'a PvDatabase,
    name: &'a str,
    rec: &'a std::sync::Arc<parking_lot::RwLock<crate::server::record::RecordInstance>>,
    exit: Option<crate::server::record::PactExit>,
    owner: RestartOwner,
}

impl<'a> PactExitGuard<'a> {
    fn new(
        db: &'a PvDatabase,
        name: &'a str,
        rec: &'a std::sync::Arc<parking_lot::RwLock<crate::server::record::RecordInstance>>,
        owner: RestartOwner,
    ) -> Self {
        PactExitGuard {
            db,
            name,
            rec,
            exit: None,
            owner,
        }
    }

    /// Take the token [`special_before_put`] produced, if it released a park.
    fn arm(&mut self, exit: Option<crate::server::record::PactExit>) {
        self.exit = exit;
    }
}

impl Drop for PactExitGuard<'_> {
    fn drop(&mut self) {
        let Some(exit) = self.exit.take() else {
            return;
        };
        // `TheCycleThisPutOwes` drops the token on purpose and loses nothing:
        // the tail re-derives it from the record
        // (`RecordInstance::pact_exit_without_release` reads
        // `notify_restart_pending()` at cycle end), so the restart is armed
        // once, by C's owner, in C's order.
        if self.owner == RestartOwner::ThisPut {
            self.db.apply_pact_exit(self.name, self.rec, exit);
        }
    }
}

/// Is `field` one C marks `special(SPC_MOD)` for this record's PACT park?
fn pact_park_field(record: &dyn crate::server::record::Record, field: &str) -> bool {
    record
        .pact_park_fields()
        .iter()
        .any(|f| f.eq_ignore_ascii_case(field))
}

/// The `recGblResetAlarms` half of a C `monitor()` that a `special()` invokes
/// (compress SPC_RESET, [`crate::server::record::Record::special_commits_alarms`]).
///
/// C's `monitor()` opens with `recGblResetAlarms(prec)` (compressRecord.c:103),
/// committing `nsta`/`nsev` into `stat`/`sevr` — this is what clears the
/// born-UDF alarm of a never-processed record the moment a reset field is put.
/// Commits the alarm, posts any STAT/SEVR/AMSG/ACKS transition through the one
/// owner ([`crate::server::database::processing::alarm_field_posts`]), and
/// returns the `DBE_ALARM` mask C ORs into the value posts (`val_mask`,
/// recGbl.c:213 — set iff any alarm-class field moved this cycle).
///
/// Shared by `dbPut`'s success tail and its rejected-conversion path: C runs
/// `dbPutSpecial(paddr, 1)` on BOTH (dbAccess.c:83-88, "Always do special
/// processing if needed", before the `goto done` that bails on a failed put).
fn commit_special_reset_alarm(
    instance: &mut crate::server::record::RecordInstance,
) -> crate::server::recgbl::EventMask {
    use crate::server::recgbl::EventMask;
    let alarm_result = crate::server::recgbl::rec_gbl_reset_alarms(&mut instance.common);
    for (af, mask) in
        crate::server::database::processing::alarm_field_posts(&instance.common, &alarm_result)
    {
        instance.notify_field(af, mask);
    }
    if alarm_result.alarm_changed || alarm_result.amsg_changed {
        EventMask::ALARM
    } else {
        EventMask::NONE
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
    // The coercion target is the type the record STORES, not the type it
    // SERVES — `put_field`'s arms match on what is stored. A `menu()` field is
    // declared `DBF_MENU` and served as `DBR_ENUM` with its choices, but held as
    // a bare `Short` choice index; coercing an incoming `Short` up to `Enum`
    // because the `.dbd` says `DBF_MENU` would make its `Short` arm unreachable.
    // Same rule as `put_field_internal_default` and `db_loader::apply_fields`.
    // The `.dbd` type is the fallback for a field with no current value.
    let target = record
        .get_field(field)
        .map(|v| v.db_field_type())
        .or_else(|| crate::server::record::record_instance::declared_field_type_of(record, field));

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
        // A String target always runs the converter even on a type match: C's
        // `putStringString` is not a no-op, it truncates to `field_size - 1`
        // (see `coerce_put_value`).
        Some(target)
            if value.db_field_type() != target || target == crate::types::DbFieldType::String =>
        {
            Ok(PutRequest::Write(coerce_write_value(
                record, field, target, value,
            )?))
        }
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

/// Which of C `processNotifyCommon`'s two entries a put-notify arrives on
/// (its `first` argument, dbNotify.c:207).
///
/// The two differ only in what defers them, and that difference is why the
/// entry test cannot live inside the install owner: a fresh arrival must not
/// jump a queue, a replay IS the queue head.
#[derive(Clone, Copy)]
enum NotifyArrival {
    /// `dbProcessNotify` -> `processNotifyCommon(ppn, precord, 1)`: reaching
    /// the record for the first time.
    Fresh,
    /// `notifyCallback` -> `processNotifyCommon(ppn, precord, 0)`: already
    /// popped off the restart list by `take_next_notify_restart`, C
    /// `restartCheck` (dbNotify.c:158-168).
    Replay,
}

impl NotifyArrival {
    /// Must this arrival queue rather than take the slot?
    ///
    /// A FRESH notify defers on the full test (owned, PACT, or someone already
    /// queued) -- C `processNotifyCommon:213` plus the PACT arm at `:225`, and
    /// the restart-list term so a notify arriving between a completion and the
    /// restart check cannot jump the queue. A REPLAY defers only on an
    /// occupied slot: it must take the record with its successors still queued
    /// behind it, exactly as `restartCheck` assigns `precord->ppn = pfirst`
    /// while leaving the rest of `restartList` in place.
    fn defers(self, record: &crate::server::record::RecordInstance) -> bool {
        match self {
            NotifyArrival::Fresh => record.notify_put_is_owned(),
            NotifyArrival::Replay => record.notify.is_some(),
        }
    }
}

impl NotifyRequest {
    fn wants_notify(&self) -> bool {
        !matches!(self, NotifyRequest::None)
    }

    /// C `pnotifyPvt->state == notifyRestartCallbackRequested` (dbNotify.c:213)
    /// — this put already owns the record, so the ownership test that stops a
    /// fresh arrival does not apply to it.
    fn is_restart(&self) -> bool {
        matches!(self, NotifyRequest::Deferred(_))
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

/// C `dbPut`'s field-monitor tail (`dbAccess.c:1408-1418`) — **the one post
/// rule every `dbPut` body shares**, reached by every put route (CA,
/// `dbPutLink`, internal `put_pv`):
///
/// ```c
/// if (precord->mlis.count &&
///     !(isValueField && pfldDes->process_passive))
///     db_post_events(precord, pfieldsave, DBE_VALUE | DBE_LOG);
/// ```
///
/// The immediate post is suppressed for the value field ONLY when that field
/// is `pp(TRUE)`: for the routes that then process (`dbPutField`'s pp gate,
/// a ` PP` OUT link's `processTarget`), the cycle re-posts it via the
/// deadband snapshot with a fresh timestamp; for the routes that do not (an
/// NPP OUT link, a bare `dbPut` from driver code), C is silent until the
/// next scan — measured against softIoc in `array_put_posts_nord.rs`'s
/// header. For a value field that is NOT `pp` (calc/calcout/aSub VAL), this
/// post is the only one there is.
///
/// The suppression is a static property of the field, never of the caller's
/// intent — C's `pfldDes->process_passive` is DBD data — which is what makes
/// it shareable: `put_pv` (which never processes) and the CA route (which
/// may) apply the identical rule, exactly as C's one `dbPut` serves both
/// `dbPutLink` and `dbPutField`. `process_passive_fields()` is total and
/// fail-safe: any field of an unmodeled type (`&[]`) posts.
fn dbput_post_put_field(instance: &mut crate::server::record::RecordInstance, field: &str) {
    let suppress = field == instance.record.primary_field()
        && instance
            .record
            .process_passive_fields()
            .iter()
            .any(|f| f.eq_ignore_ascii_case(field));
    if !suppress {
        instance.cleanup_subscribers();
        instance.notify_field(
            field,
            crate::server::recgbl::EventMask::VALUE | crate::server::recgbl::EventMask::LOG,
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
    /// Kept as a distinct entry point for source compatibility with the
    /// callers that predate [`Self::get_pv`] becoming a `fn`; there is no
    /// blocking left to do, so there is no current-thread-runtime failure mode
    /// either. C `dbGetField` is likewise a plain call from any thread.
    pub fn get_pv_blocking(&self, name: &str) -> CaResult<EpicsValue> {
        self.get_pv(name)
    }

    /// Get the current value of a PV or record field.
    /// Uses resolve_field for records (3-level priority).
    pub fn get_pv(&self, name: &str) -> CaResult<EpicsValue> {
        let (base, field) = super::parse_pv_name(name);
        let field = field.to_ascii_uppercase();

        // Check simple PVs first (exact match)
        let simple = self.inner.simple_pvs.lock().get(name).cloned();
        if let Some(pv) = simple {
            return Ok(pv.get());
        }

        // Records — alias-aware via `get_record` (epics-base PR #336).
        if let Some(rec) = self.get_record(base) {
            let instance = rec.read();
            return instance
                .resolve_field(&field)
                .ok_or_else(|| CaError::ChannelNotFound(name.to_string()));
        }

        Err(CaError::ChannelNotFound(name.to_string()))
    }

    /// Set a PV value or record field — the C `dbPut` analogue
    /// (`dbAccess.c:1316-1419`), whole: value write + `special`/`on_put`, the
    /// value-field UDF clear, and the field's `DBE_VALUE|DBE_LOG` monitor
    /// post (`dbput_post_put_field`, suppressed only for a `pp(TRUE)` value
    /// field exactly as C's tail suppresses it). Tries record `put_field`
    /// first, then `put_common_field` as fallback.
    ///
    /// Does NOT process the record — `dbPutField`'s pp gate is
    /// [`Self::put_record_field_from_ca`] — so, as with a bare C `dbPut`, a
    /// `pp(TRUE)` value field (ai/ao/waveform VAL …) posts nothing here and a
    /// caller that needs a monitor on such a field must either drive a
    /// process or use [`Self::put_pv_and_post`].
    ///
    /// Acquires the record's advisory write gate.
    pub async fn put_pv(&self, name: &str, value: EpicsValue) -> CaResult<()> {
        let _record_gate = self.acquire_put_gate(name);
        self.put_pv_already_locked(name, value)
    }

    /// `put_pv` variant for a caller already holding the
    /// record's advisory write gate (QSRV atomic group PUT). See
    /// [`Self::put_record_field_from_ca_already_locked`].
    ///
    /// This is the whole `dbPut` body — the gate-held region, and it is a
    /// `fn`. See `acquire_put_gate`.
    pub fn put_pv_already_locked(&self, name: &str, value: EpicsValue) -> CaResult<()> {
        self.put_pv_body(name, value, RestartOwner::ThisPut)
    }

    /// [`Self::put_pv_already_locked`] for a caller that drives a process
    /// cycle on the SAME record the moment this returns — an OUT link's
    /// `processTarget`, QSRV's `Force` mode.
    ///
    /// The only difference is who arms a queued put-notify restart when this
    /// put releases a PACT park: that cycle's tail, as in C, rather than this
    /// put body. See `RestartOwner`.
    pub fn put_pv_already_locked_before_process(
        &self,
        name: &str,
        value: EpicsValue,
    ) -> CaResult<()> {
        self.put_pv_body(name, value, RestartOwner::TheCycleThisPutOwes)
    }

    /// Take the L1 advisory write gate a put to `name` needs, if any.
    ///
    /// The gate boundary of the whole put family: EVERY `_already_locked`
    /// entry is the body below this line and contains no `.await`, so an
    /// caller that already owns the gate calls the body directly and a caller
    /// that does not calls this first. C's shape exactly — `dbPutField` does
    /// `dbScanLock(precord)` … `dbScanUnlock(precord)` around a `dbPut` that
    /// itself never blocks (`dbAccess.c:1246-1300`).
    ///
    /// `None` when `name` names no record: a simple PV has no `dbCommon` and
    /// therefore no `dbScanLock` in C either. The record lookup is repeated by
    /// the body — a map read, and records are never removed once loaded.
    fn acquire_put_gate(&self, name: &str) -> Option<super::record_lock::RecordWriteGuard> {
        let (base, _) = super::parse_pv_name(name);
        self.get_record(base)?;
        let canonical: String = self.resolve_alias(base).unwrap_or_else(|| base.to_string());
        Some(self.lock_record(&canonical))
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
        let Some(rec) = self.get_record(record_name) else {
            return Ok(());
        };
        let instance = rec.read();
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
    /// and is not asked about here; see `put_drives_processing_of`.
    pub fn put_drives_processing(&self, record_name: &str, field: &str) -> bool {
        let field_upper = field.to_ascii_uppercase();
        let Some(rec) = self.get_record(record_name) else {
            return false;
        };
        let instance = rec.read();
        put_drives_processing_of(&instance, &field_upper)
    }

    /// Is `record_name.field` a DBF link field (INLINK/OUTLINK/FWDLINK)?
    ///
    /// The one owner of the classification lookup
    /// ([`crate::types::dbf_link_class`] keyed by the record's type). C
    /// callers split on it before choosing a put entry: `dbPutField` sends
    /// link fields to `dbPutFieldLink` (`dbAccess.c:1261`), `dbProcessNotify`
    /// short-circuits them past the notify machinery (`dbNotify.c:337-353`),
    /// and pvxs QSRV picks `dbChannelPutField` over `dbChannelPut` for them
    /// (`iocsource.cpp:451-458`). Port callers with a dbPutField-shaped
    /// entry make the same split against the `dbPut`-analogue bodies' refusal
    /// (`check_not_link_field`). `false` for an unknown record or a non-link
    /// field.
    pub fn is_dbf_link_field(&self, record_name: &str, field: &str) -> bool {
        let field_upper = field.to_ascii_uppercase();
        let Some(rec) = self.get_record(record_name) else {
            return false;
        };
        let guard = rec.read();
        crate::types::dbf_link_class(guard.record.record_type(), &field_upper).is_some()
    }

    fn put_pv_body(&self, name: &str, value: EpicsValue, owner: RestartOwner) -> CaResult<()> {
        let (base, field) = super::parse_pv_name(name);
        let field = field.to_ascii_uppercase();

        // Check simple PVs first. The lookup is its own statement so the
        // `!Send` directory guard is down before `pv.set(…).await` — see the
        // `simple_pvs` field doc in `database/mod.rs`.
        let simple = self.inner.simple_pvs.lock().get(name).cloned();
        if let Some(pv) = simple {
            pv.set(value);
            return Ok(());
        }

        // Records — alias-aware (epics-base PR #336).
        if let Some(rec) = self.get_record(base) {
            // `base` may be an alias; resolve to the canonical record
            // name so scan-index updates target the right entry.
            let canonical_base: String =
                self.resolve_alias(base).unwrap_or_else(|| base.to_string());
            // The caller holds the advisory write gate (`dbScanLock`
            // analogue) for `canonical_base` — see `acquire_put_gate`.
            // It is held to the `return Ok(())` below, and that is the point:
            // C's `dbScanLock` covers `dbPut` *including*
            // `dbPutSpecial(paddr, 1)` and the scan-list move, so the tails
            // below are inside the exclusion window in C and must be inside it
            // here. Shrinking the window to end at the value write would
            // re-open exactly the interleaving `lock_records` exists to close
            // (`record_lock.rs`, "Rust port"). See
            // `doc/rtems-priority-locks-design.md` §2, "the semantic question".
            // Scoped guard: everything the put commits happens under this
            // write guard, which ends (releasing the `!Send` parking_lot
            // guard) before the tails below, which re-enter the database.
            // Yields the owned outputs those tails consume. Note this is the
            // record's DATA lock coming down, not the advisory gate above.
            use crate::server::record::CommonFieldPutResult;
            let mut pact_exit = PactExitGuard::new(self, base, &rec, owner);
            let (common_result, special_actions) = {
                let mut instance = rec.write();

                // C `dbPut` refuses an SPC_NOMOD / SPC_ATTRIBUTE field before it
                // converts anything (`dbAccess.c:1330-1332`). `put_pv` IS the
                // `dbPut` analogue — it sits below `dbPutLink`, so this is what
                // stops a record's OUT link from truncating a waveform's NELM.
                // The refusal is returned to the caller; `write_out_link_value`
                // (C `dbPutLink`) turns it into the writer's LINK/INVALID alarm.
                check_no_mod(&instance, &field)?;

                // C `dbPut` refuses a link-field target the same way, before
                // conversion (`dbAccess.c:1340`) — see `check_not_link_field`.
                check_not_link_field(&instance, &field)?;

                let request = dbput_request(&*instance.record, &field, value)?;

                // Pre-write special hook (C EPICS dbPutSpecial pass=0).
                // C `dbPut` runs it on EVERY entry path — dbPutField and
                // dbPutLink alike (dbAccess.c) — so the OUT-link route
                // through this body must call it too (motor's drive-field
                // DMOV blink, motorRecord.cc:2582-2608, fires on put-links
                // in C). A non-zero status aborts the put like C.
                instance.record.special(&field, false)?;

                // Capture the pre-put value so the metadata-cache
                // invalidation (and the downstream `DBE_PROPERTY`
                // emission) can be skipped when the put is a no-op —
                // epics-base faac1df1.
                let prev_value = instance.record.get_field(&field);
                let old_nord = array_nord_before_put(&instance, &field);

                // Link writes the record's `special()` makes itself (C runs them
                // inside `dbPut`); executed below, once the record lock is released.
                let mut special_actions = Vec::new();

                // put_pv is C EPICS dbPut: write value + special/on_put, clear
                // UDF on a value-field put, and post the field's DBE_VALUE|
                // DBE_LOG monitor per `dbPut`'s tail (dbAccess.c:1408-1418).
                // Does NOT trigger processing (that is `dbPutField`'s pp gate,
                // this port's `put_record_field_from_ca`), so — exactly like a
                // bare C `dbPut` — a pp(TRUE) value field's post stays
                // suppressed and its stale UDF *alarm* stands until a process
                // cycle recomputes stat/sevr.
                let common_result = match request {
                    // C `dbAccess.c:1370-1372` — accept, write nothing, alarm.
                    PutRequest::EmptyIntoScalar => {
                        set_empty_request_alarm(&mut instance);
                        CommonFieldPutResult::NoChange
                    }
                    PutRequest::Write(value) => {
                        pact_exit.arm(special_before_put(&mut instance, &field));
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
                                let result = special_after_put(
                                    self,
                                    &mut instance,
                                    &field,
                                    &mut special_actions,
                                )?;
                                // C `dbAccess.c::dbPut:1410-1411` clears ONLY
                                // `precord->udf = FALSE` on a value-field put —
                                // the same clear (and the same
                                // `is_udf_defining_put` predicate) as the CA
                                // route and `put_pv_and_post`. stat/sevr are NOT
                                // touched: see the comment block above.
                                if instance.record.is_udf_defining_put(&field) {
                                    instance.common.udf = 0;
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

                // C `dbPut` runs `dbPutSpecial(paddr, 1)` regardless of caller entry
                // path, so a put via this internal route commits/writes the same
                // `special()`-driven alarm the CA route does. State only — unlike
                // the CA path these commit the alarm without the STAT/SEVR posts
                // (C's bare `dbPut` runs no `monitor()`, so it posts no alarm
                // transition either).
                //
                // compress SPC_RESET: `monitor()`'s `recGblResetAlarms` commits the
                // born-UDF alarm (compressRecord.c:103).
                if instance.record.special_commits_alarms(&field) {
                    let _ = crate::server::recgbl::rec_gbl_reset_alarms(&mut instance.common);
                }
                // histogram SGNL SPC_MOD: `add_count` raises the inverted-limits
                // alarm (histogramRecord.c:329-334). The port routes it through
                // `nsta`/`nsev` (CBUG-F12 refused), so this monitor-less special
                // path must commit it — check then reset — for the SOFT/INVALID to
                // be observable, matching the process path.
                if instance.record.special_checks_alarms(&field) {
                    let inst = &mut *instance;
                    inst.record.check_alarms(&mut inst.common);
                    let _ = crate::server::recgbl::rec_gbl_reset_alarms(&mut inst.common);
                }

                // Invalidate metadata cache only if the metadata-class
                // field's value actually changed (faac1df1).
                instance.notify_field_written_if_changed(&field, prev_value.as_ref());

                // C `dbPut:1408-1414`'s field-monitor post, through the one owner
                // (`dbput_post_put_field`, shared with the CA route). `put_pv`
                // is the `dbPutLink` route's `dbPut` and the internal driver-put
                // entry, and C posts DBE_VALUE|DBE_LOG from *every* `dbPut` —
                // an NPP OUT link writing a calc's A, an autosave restore, a
                // status pusher, all post immediately. Pre-fix this body posted
                // nothing at all, so camonitor on anything written via `put_pv`
                // went silent forever (doc/calink-rtems-design.md §11.7 item 2).
                dbput_post_put_field(&mut instance, &field);

                // C's `put_array_info` is likewise reached from every `dbPut` —
                // an OUT link that shortens a waveform posts NORD in C even when
                // the link is NPP and the target never processes, and the
                // value-field post above is suppressed for a waveform (VAL is
                // `pp(TRUE)`); NORD has no such second path.
                post_array_info(&mut instance, &old_nord, 0);

                (common_result, special_actions)
            };
            // Lock down: arm any put-notify the SNAM park release freed.
            drop(pact_exit);
            // The record DATA lock is now down (scope ended above) before the
            // scan-index update and the `special()` link writes below, which
            // re-enter the database (they can process their target). The
            // advisory gate is still held — see its comment above.

            // Update scan index if SCAN or PHAS changed. Synchronous as of
            // step 4, so this half of the tail adds no suspension point to the
            // gate-held window; C reaches `scanDelete`/`scanAdd` from inside
            // `dbScanLock` the same way.
            match common_result {
                CommonFieldPutResult::ScanChanged {
                    old_scan,
                    new_scan,
                    phas,
                } => {
                    self.update_scan_index(&canonical_base, old_scan, new_scan, phas, phas);
                }
                CommonFieldPutResult::PhasChanged {
                    scan: s,
                    old_phas,
                    new_phas,
                } => {
                    self.update_scan_index(&canonical_base, s, s, old_phas, new_phas);
                }
                CommonFieldPutResult::NoChange => {}
            }

            // C `dbPut` runs `dbPutSpecial(paddr, 1)` to completion — the
            // `dbPutLink` calls a `special()` makes included — before it returns
            // to `dbPutField`. This is the last statement of the `dbPut`
            // analogue, so it is that point.
            self.run_special_actions(&canonical_base, &rec, special_actions);

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
    pub fn post_alarm(&self, name: &str, severity: u16, status: u16) -> CaResult<()> {
        let simple = self.inner.simple_pvs.lock().get(name).cloned();
        if let Some(pv) = simple {
            pv.post_alarm(severity, status);
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
        let simple = self.inner.simple_pvs.lock().get(name).cloned();
        if let Some(pv) = simple {
            pv.set_snapshot(snapshot);
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
        let simple = self.inner.simple_pvs.lock().get(name).cloned();
        if let Some(pv) = simple {
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
        let simple = self.inner.simple_pvs.lock().get(name).cloned();
        if let Some(pv) = simple {
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
        // PV — `ProcessVariable::set_with_origin` already does the
        // notify-subscribers fan-out internally (tagging the event with
        // `origin`, same self-write contract as the record branch) so
        // all we need here is to delegate.
        let simple = self.inner.simple_pvs.lock().get(name).cloned();
        if let Some(pv) = simple {
            pv.set_with_origin(value, origin);
            return Ok(());
        }

        if let Some(rec) = self.get_record(base) {
            // `put_pv_and_post` is a public record-write API —
            // it must take the same advisory write gate
            // (`dbScanLock` analogue) as `put_pv` /
            // `put_record_field_from_ca`, or a gateway/sequencer
            // write through this helper can still land between the
            // member writes of a QSRV atomic group or a pvalink
            // atomic scan epoch holding `lock_records`. `base` is
            // alias-resolved to the canonical record name so an alias
            // and its target share one gate. Held until return — same
            // reasoning as `put_pv_inner`'s gate: C's `dbScanLock` covers
            // `dbPut` including `dbPutSpecial(paddr, 1)` and the scan-list
            // move, so both tails below stay inside the window
            // (`doc/rtems-priority-locks-design.md` §2).
            let canonical_base: String =
                self.resolve_alias(base).unwrap_or_else(|| base.to_string());
            let _record_gate = self.lock_record(&canonical_base);

            // Guarded: the value write + monitor post. The record's DATA guard
            // is released at the block close before the tails below, which
            // re-enter the database (`parking_lot` guards are `!Send`); the
            // advisory `_record_gate` still holds the processing-exclusion
            // window across the whole helper.
            use crate::server::record::CommonFieldPutResult;
            let mut pact_exit = PactExitGuard::new(self, base, &rec, RestartOwner::ThisPut);
            let (common_result, special_actions) = {
                let mut instance = rec.write();

                // Same `dbPut` gate as `put_pv` — this is the third `dbPut` body
                // (value + monitor post), and C has ONE.
                check_no_mod(&instance, &field)?;
                check_not_link_field(&instance, &field)?;

                let request = dbput_request(&*instance.record, &field, value)?;

                // Pre-write special hook (C EPICS dbPutSpecial pass=0) —
                // C `dbPut` runs it on every entry path; this is the third
                // `dbPut` body and must match the other two.
                instance.record.special(&field, false)?;

                let old_value = instance.record.get_field(&field);
                let old_stat = instance.common.stat;
                let old_sevr = instance.common.sevr;
                let old_nord = array_nord_before_put(&instance, &field);

                // Link writes the record's `special()` makes itself (C runs them
                // inside `dbPut`); executed below, once the record lock is released.
                let mut special_actions = Vec::new();

                // Write value + special/on_put
                let common_result = match request {
                    // C `dbAccess.c:1370-1372` — accept, write nothing, alarm. UDF
                    // is NOT cleared: C clears it at `:1409` only when the value
                    // field was actually written, and this branch wrote nothing.
                    PutRequest::EmptyIntoScalar => {
                        set_empty_request_alarm(&mut instance);
                        CommonFieldPutResult::NoChange
                    }
                    PutRequest::Write(value) => {
                        pact_exit.arm(special_before_put(&mut instance, &field));
                        match instance.record.put_field(&field, value.clone()) {
                            Ok(()) => {
                                instance.record.on_put(&field);
                                // C returns the after-put special() status from
                                // `dbPut` (dbAccess.c:1399-1405) — before the UDF
                                // clear and the monitor post below, both of which
                                // `goto done` skips on a non-zero status.
                                let result = special_after_put(
                                    self,
                                    &mut instance,
                                    &field,
                                    &mut special_actions,
                                )?;
                                // C `dbAccess.c::dbPut:1411` clears ONLY `precord->udf
                                // = FALSE` on a value-field put, and nothing else. It
                                // does NOT touch stat/sevr: the UDF_ALARM stays until
                                // the record's own process cycle recomputes it
                                // (`rec_gbl_check_udf` no longer raises it now udf is
                                // clear, `rec_gbl_reset_alarms` commits the new state).
                                // A value put that does not drive a process therefore
                                // leaves the stale UDF alarm exactly as C does — the
                                // earlier synchronous stat/sevr clear here diverged
                                // from C and reported NO_ALARM where C keeps UDF/INVALID.
                                if instance.record.is_udf_defining_put(&field) {
                                    instance.common.udf = 0;
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
                let nord_changed =
                    old_nord.is_some() && instance.record.get_field("NORD") != old_nord;
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
                // lock goes down first (the block close releases it). C makes them
                // inside `dbPut`, before it returns to its caller.
                (common_result, special_actions)
            };
            // Lock down: arm any put-notify the SNAM park release freed.
            drop(pact_exit);

            // Same scan-index owner every other `dbPut` path routes through:
            // a SCAN put and the SIMM↔SSCN swap (`recGblCheckSimm`) both move
            // the record between scan lists and must reach `update_scan_index`.
            match common_result {
                CommonFieldPutResult::ScanChanged {
                    old_scan,
                    new_scan,
                    phas,
                } => {
                    self.update_scan_index(&canonical_base, old_scan, new_scan, phas, phas);
                }
                CommonFieldPutResult::PhasChanged {
                    scan: s,
                    old_phas,
                    new_phas,
                } => {
                    self.update_scan_index(&canonical_base, s, s, old_phas, new_phas);
                }
                CommonFieldPutResult::NoChange => {}
            }

            self.run_special_actions(&canonical_base, &rec, special_actions);

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
    ///
    /// # Synchronous, and it has to stay that way
    ///
    /// This whole tail runs inside the caller's L1 gate window, and L1 is a
    /// blocking priority-inheritance mutex whose guard is `!Send`
    /// (`server::database::record_lock`). An `.await` anywhere reachable from
    /// here is therefore a compile error at the spawn sites, not a review
    /// finding — see `doc/rtems-priority-locks-design.md` §5 steps 5–6.
    ///
    /// The one call that used to make it a genuine suspension is gone:
    /// `write_out_link_value` → `write_external_pv` does not call
    /// `LinkSet::put_value` from this thread. It stages the write on the
    /// database's link-put queue and returns, exactly as C `dbCaPutLink`
    /// stages into `pca->pputNative`, calls `addAction` and returns
    /// (`dbCa.c:544-631`); the `ca://` / `pva://` round trip runs on the
    /// queue's owner task, C's `dbCaTask` (`dbCa.c:1226-1248`). See
    /// [`super::link_put_queue`]. What is left inside the window is the
    /// re-entrant database work C also does under `dbScanLock`:
    /// `execute_process_actions` → `write_out_link_value` →
    /// `write_db_link_value` re-entering `put_pv_already_locked` and
    /// `process_target`, plus the cached-state `LinkSet::put_admission` probe
    /// both production lsets answer from a map lookup and an atomic — C's
    /// `if (!pca->isConnected …)` (`dbCa.c:558-561`).
    fn run_special_actions(
        &self,
        record_name: &str,
        rec: &std::sync::Arc<parking_lot::RwLock<crate::server::record::RecordInstance>>,
        actions: Vec<crate::server::record::ProcessAction>,
    ) {
        if actions.is_empty() {
            return;
        }
        let mut visited = HashSet::new();
        self.execute_process_actions(record_name, rec, actions, &mut visited, 0);
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
    ) -> CaResult<crate::server::record::ProcessCompletion> {
        let _record_gate = self.acquire_put_gate(record_name);
        self.put_record_field_from_ca_body(record_name, field, value, NotifyRequest::New)
    }

    /// Variant for a caller that already owns the target
    /// record's advisory write gate — the QSRV atomic group PUT,
    /// which acquired every member-record gate up-front via
    /// [`Self::lock_records`]. The per-record gate is NOT reentrant, so the
    /// atomic group path MUST use this `_already_locked` entry to avoid
    /// dead-locking on its own `ManyRecordWriteGuard`.
    pub fn put_record_field_from_ca_already_locked(
        &self,
        record_name: &str,
        field: &str,
        value: EpicsValue,
    ) -> CaResult<crate::server::record::ProcessCompletion> {
        self.put_record_field_from_ca_body(record_name, field, value, NotifyRequest::New)
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
        self.put_record_field_from_ca_no_notify_with_origin(record_name, field, value, 0)
            .await
    }

    /// [`Self::put_record_field_from_ca_no_notify`] for an in-process writer
    /// with a self-write-filtering origin (a ported SNL state machine's
    /// `DbChannel`): every event the put's synchronous process cascade posts
    /// is tagged with `origin`, so the writer's own filtered subscriptions
    /// skip them. The ambient scope is sound here because the body below is
    /// fully synchronous — there is no await between entering the scope and
    /// the cascade's last post.
    pub async fn put_record_field_from_ca_no_notify_with_origin(
        &self,
        record_name: &str,
        field: &str,
        value: EpicsValue,
        origin: u64,
    ) -> CaResult<()> {
        let _record_gate = self.acquire_put_gate(record_name);
        let _origin_scope = crate::server::record::ambient_write_origin_scope(origin);
        self.put_record_field_from_ca_body(record_name, field, value, NotifyRequest::None)
            .map(|_| ())
    }

    /// The one router for an EXTERNAL client PUT that carries pvxs's
    /// `record._options.process` / `.block` terms — QSRV's whole
    /// `onPut` decision tree (`ioc/singlesource.cpp:346-384`,
    /// `ioc/iocsource.cpp:397-419`) in one place, so the QSRV bridge
    /// channel and the native PVA source cannot disagree about what
    /// `process=false` or `block=true` means.
    ///
    /// A DBF link field ignores the requested mode: pvxs sends it down
    /// `dbPutField` whatever the client asked (`iocsource.cpp:451-458`,
    /// `dbNotify.c:337-353`), and the `dbPut`-analogue bodies refuse link
    /// fields outright, so the Passive route is the only one that can
    /// carry it.
    ///
    /// `doPreProcessing`'s two gates (`SPC_ATTRIBUTE` → `S_db_noMod`,
    /// `DISP` → `S_db_putDisabled`) run here for every mode. The Passive
    /// route re-checks them inside `put_record_field_from_ca`, but the
    /// Force / Inhibit routes go through `put_pv` — the internal `dbPut`
    /// analogue, which by design does not gate `DISP` — so the gate has
    /// to be at this boundary for the invariant to hold by construction.
    /// A caller that needs the rejection to precede its own ACF check
    /// (pvxs runs `doPreProcessing` before `doFieldPreProcessing`) still
    /// calls [`Self::check_external_put_preconditions`] itself; the check
    /// is idempotent.
    pub async fn put_field_from_client(
        &self,
        record_name: &str,
        field: &str,
        value: EpicsValue,
        process: ProcessMode,
        block: bool,
    ) -> CaResult<()> {
        let process = if self.is_dbf_link_field(record_name, field) {
            ProcessMode::Passive
        } else {
            process
        };
        self.check_external_put_preconditions(record_name, field)
            .await?;
        match process {
            ProcessMode::Inhibit => self.put_pv(&format!("{record_name}.{field}"), value).await,
            ProcessMode::Passive => {
                if block {
                    let completion = self
                        .put_record_field_from_ca(record_name, field, value)
                        .await?;
                    Self::await_completion(completion).await;
                    Ok(())
                } else {
                    self.put_record_field_from_ca_no_notify(record_name, field, value)
                        .await
                }
            }
            ProcessMode::Force => {
                self.put_pv(&format!("{record_name}.{field}"), value)
                    .await?;
                if block {
                    // A blocking forced put is C `dbProcessNotify`
                    // (`singlesource.cpp:360-369`): the reply waits for the
                    // whole chain, async device completion included. The
                    // bare `process_record_with_links` returns as soon as
                    // the record goes PACT.
                    let completion = self.process_record_with_notify(record_name).await?;
                    Self::await_completion(completion).await;
                    Ok(())
                } else {
                    // `doPostProcessing(forceProcessing == True)`
                    // (`iocsource.cpp:404-419`) splits on PACT: an
                    // async-active record takes `rpro = TRUE` and does not
                    // process, an idle one takes `putf = TRUE` and does.
                    // `put_driven_process` is that transition's owner.
                    self.put_driven_process(record_name).await
                }
            }
        }
    }

    async fn await_completion(completion: crate::server::record::ProcessCompletion) {
        if let crate::server::record::ProcessCompletion::Async(rx) = completion {
            let _ = rx.await;
        }
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
            .ok_or_else(|| CaError::ChannelNotFound(record_name.to_string()))?;
        let canonical: String = self
            .resolve_alias(record_name)
            .unwrap_or_else(|| record_name.to_string());
        let _record_gate = self.lock_record(&canonical);

        let mut instance = rec.write();
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
    pub fn put_record_field_from_ca_no_notify_already_locked(
        &self,
        record_name: &str,
        field: &str,
        value: EpicsValue,
    ) -> CaResult<()> {
        self.put_record_field_from_ca_body(record_name, field, value, NotifyRequest::None)
            .map(|_| ())
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
    /// the caller to await. A notify already in flight on the record does not
    /// refuse this one: it joins the record's restart queue and replays when
    /// the record frees, which is C's only outcome here.
    pub async fn process_record_with_notify(
        &self,
        record_name: &str,
    ) -> CaResult<crate::server::record::ProcessCompletion> {
        let (completion_tx, completion_rx) = crate::runtime::sync::oneshot::channel();
        // The gate wraps the install AND the cycle it arms, as C's does:
        // `dbProcessNotify` takes `dbScanLock(precord)` (dbNotify.c:355) and
        // `processNotifyCommon` assigns `precord->ppn` and calls `dbProcess`
        // before the matching `dbScanUnlock` (`:257-262`). Installing ahead of
        // the gate let a gate-holding put's cycle reach `complete_put_notify`
        // on a slot it did not fill, `take` this client's wait-set and `leave`
        // it -- firing a `block=true` completion for a cycle the client never
        // requested, and leaving this one to run unarmed.
        let installed = {
            let _record_gate = self.acquire_put_gate(record_name);
            self.install_notify_and_process_already_locked(
                record_name,
                completion_tx,
                NotifyArrival::Fresh,
            )
        }?;
        // The wait-set fires the oneshot only after the whole FLNK/OUT chain
        // (sync + async) settles. Already-completed ==> fully synchronous ==>
        // report immediate success. Queued (`None`) or still pending ==> hand
        // the receiver back to await the deferred completion.
        match installed {
            Some(notify) if notify.completed() => {
                Ok(crate::server::record::ProcessCompletion::Sync)
            }
            _ => Ok(crate::server::record::ProcessCompletion::Async(
                completion_rx,
            )),
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
    /// Two entries, one rule. `put_driven_process` acquires `record_name`'s
    /// advisory write gate (the `dbScanLock` analogue) itself;
    /// [`Self::put_driven_process_already_locked`] is for a caller that
    /// already owns it — `_record_gate` on the CA put path, or the QSRV
    /// atomic group's `lock_records` epoch. The gate is not
    /// reentrant, so a caller holding it MUST take the `_already_locked`
    /// entry.
    ///
    /// QSRV's group PUT is the second external caller (pvxs
    /// `IOCSource::doPostProcessing`, `iocsource.cpp:397-420`, whose PACT
    /// branch is this same RPRO deferral): it reaches the decision by its own
    /// route — `record._options.process`, a `+type:"proc"` member, a `pp` field
    /// — but once the answer is "process", the transition is this owner's, in
    /// both gate modes.
    /// The PACT (RPRO) branch is success, as it is in C: `dbPutField` returns
    /// the `dbProcess` status only on the branch that ran it.
    pub async fn put_driven_process(&self, record_name: &str) -> CaResult<()> {
        let _record_gate = self.acquire_put_gate(record_name);
        self.put_driven_process_already_locked(record_name)
    }

    /// [`Self::put_driven_process`] for a caller that already owns the
    /// record's advisory write gate. The gate-held body — no `.await`.
    pub fn put_driven_process_already_locked(&self, record_name: &str) -> CaResult<()> {
        {
            let Some(rec) = self.get_record(record_name) else {
                return Ok(());
            };
            let mut instance = rec.write();
            if instance.is_processing() {
                instance.common.rpro = 1;
                return Ok(());
            }
            instance.common.putf = true;
        }
        let mut visited = HashSet::new();
        self.process_record_with_links_already_locked(record_name, &mut visited, 0)
    }

    /// C `restartCheck` (dbNotify.c:149-170) plus the restarted
    /// `processNotifyCommon` it queues: pop the oldest put-notify waiting on
    /// `record_name` and replay it whole — value, process, callback.
    ///
    /// The pop happens **after** the record's advisory write gate is taken and
    /// **before** the replay releases it, so the promoted put owns the record
    /// across the whole promotion, as C's `precord->ppn = pfirst` does. Without
    /// that, a client put arriving in the gap would take the record and the
    /// longer-waiting notify would end up behind it.
    ///
    /// The replay goes back through the ordinary put entry, so if the record has
    /// ALREADY gone active again (a scan fired between the completion and this
    /// replay), the same test queues it once more rather than writing into a
    /// busy record — the deferral is closed under its own restart. Called only
    /// from `PvDatabase::apply_pact_exit`, the single drain owner.
    pub(crate) async fn restart_next_notify_put(&self, record_name: &str) {
        // The clients already hold their receivers; a failure here (record gone,
        // field refused) must still release them, which dropping the senders
        // does — the same completion a `dbNotifyCancel` gives the C client.
        let _record_gate = self.acquire_put_gate(record_name);
        let Some(rec) = self.get_record(record_name) else {
            return;
        };
        let Some(queued) = rec.write().take_next_notify_restart() else {
            return;
        };
        match queued {
            crate::server::record::DeferredNotify::Put(
                crate::server::record::DeferredNotifyPut {
                    field,
                    value,
                    completion,
                },
            ) => {
                let _ = self.put_record_field_from_ca_body(
                    record_name,
                    &field,
                    value,
                    NotifyRequest::Deferred(completion),
                );
            }
            // C `processGetRequest` on restart: `processNotifyCommon` re-enters
            // with no `putCallback`, so the replay is the process alone. The
            // gate is already held, so this takes the already-locked entry.
            crate::server::record::DeferredNotify::Process { completion } => {
                let _ = self.install_notify_and_process_already_locked(
                    record_name,
                    completion,
                    NotifyArrival::Replay,
                );
            }
        }
    }

    /// Arm `record_name`'s wait-set around `completion` and drive one process
    /// cycle -- the gate-held body behind every put-notify entry in this file,
    /// and the only one that reaches the record's `notify` slot.
    ///
    /// **The caller MUST hold `record_name`'s advisory write gate.** That is
    /// the whole of C's gated region: both entries take the record lock before
    /// `processNotifyCommon` runs -- `dbProcessNotify` at dbNotify.c:355 for a
    /// fresh arrival, `notifyCallback` at `:282` for a replay -- and hold it
    /// across the `precord->ppn` assignment and the `dbProcess` it arms
    /// (`:257-262`). Being an `_already_locked` body this contains no `.await`,
    /// so the gate cannot be held across a suspension.
    ///
    /// Returns the wait-set so the caller can ask whether the chain settled
    /// synchronously. `Ok(None)` means this call drove nothing -- the notify
    /// queued behind the record's current owner, and the replay is what
    /// processes, so processing here too would run one client request twice.
    fn install_notify_and_process_already_locked(
        &self,
        record_name: &str,
        completion: crate::runtime::sync::oneshot::Sender<()>,
        arrival: NotifyArrival,
    ) -> CaResult<Option<std::sync::Arc<crate::server::record::NotifyWaitSet>>> {
        // Collect-then-act: clone the handle under a brief map read, drop the
        // map lock before taking the per-record write lock.
        let rec_arc = {
            let recs = self.inner.records.read();
            recs.get(record_name).cloned()
        }
        .ok_or_else(|| CaError::ChannelNotFound(record_name.to_string()))?;
        let notify = {
            let mut guard = rec_arc.write();
            if arrival.defers(&guard) {
                guard.queue_notify_put(crate::server::record::DeferredNotify::Process {
                    completion,
                });
                return Ok(None);
            }
            // Through the one install owner. Assigning the slot here instead
            // would drop the prior client's Sender, and its receiver then wakes
            // with the RecvError the CA dispatcher reads as success.
            match guard.install_or_queue_notify(completion) {
                Some(notify) => notify,
                None => return Ok(None),
            }
        };
        let mut visited = HashSet::new();
        self.process_record_with_links_already_locked(record_name, &mut visited, 0)?;
        Ok(Some(notify))
    }

    /// The gate-held body of the whole CA field-put family — a `fn`, so
    /// nothing in it can suspend while the L1 gate is held. The gate is the
    /// caller's; see `acquire_put_gate`.
    fn put_record_field_from_ca_body(
        &self,
        record_name: &str,
        field: &str,
        value: EpicsValue,
        notify_request: NotifyRequest,
    ) -> CaResult<crate::server::record::ProcessCompletion> {
        let field = field.to_ascii_uppercase();
        let want_notify = notify_request.wants_notify();

        // Get record Arc — alias-aware (epics-base PR #336) so a CA
        // client that connects via an alias name can put fields on
        // the canonical record.
        let rec = self
            .get_record(record_name)
            .ok_or_else(|| CaError::ChannelNotFound(record_name.to_string()))?;
        // Normalise to the canonical name for the rest of this
        // function — every subsequent call (PACT/LCNT lookup,
        // `process_record_with_links`, `update_scan_index`) uses the
        // raw records map and would miss when `record_name` is an
        // alias. Resolve once up front.
        let canonical_owned;
        let record_name: &str = if let Some(target) = self.resolve_alias(record_name) {
            canonical_owned = target;
            &canonical_owned
        } else {
            record_name
        };

        // The caller holds the record's advisory write gate — the
        // `dbScanLock(precord)` analogue, taken by `acquire_put_gate`
        // or (QSRV atomic group PUT) by `lock_records` over the whole member
        // set. While it is held a plain write to the same record blocks, so a
        // direct backing-record write cannot land between member writes of an
        // atomic group transaction. It is held until the function returns.

        // Special field intercepts (read lock, then drop)
        {
            let instance = rec.read();

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
        }

        // C `processNotifyCommon` (dbNotify.c:225-231) tests PACT ABOVE the
        // put — `if (precord->pact) { ... pnotify->state =
        // notifyRestartCallbackRequested; ... return; }` — so a put-notify that
        // lands on a busy record writes NOTHING: no value, no RPRO, no join of
        // the in-flight cycle's wait-set. The whole put is replayed by the
        // `PactExit` the record's PACT release hands to its `recGblFwdLink`
        // tail (C `dbNotifyCompletion`). Joining the running cycle instead
        // completed the callback one cycle early, on work that never saw this
        // value.
        //
        // The ownership test and the enqueue are ONE critical section (C holds
        // `dbScanLock` across both): a put queued onto a record that went idle
        // in between would wait for a restart check that already ran. A record
        // that goes idle in the window falls through and takes the put the
        // ordinary way.
        //
        // A second put-notify onto a record that already owns one is C's
        // "another processNotify owns the record" (dbNotify.c:213-217): it joins
        // the SAME queue, at the back (`ellSafeAdd`). Both tests are one arm
        // here because both have the same answer — the put waits, unwritten.
        // Refusing it instead (`S_db_Blocked` / `ECA_PUTCBINPROG`) drops the
        // client's value, and C never sends that status from this path.
        //
        // A fire-and-forget `dbPutField` is NOT deferred: it writes and raises
        // RPRO (dbAccess.c:1263-1277). Only the notify route waits.
        //
        // C `dbProcessNotify` (dbNotify.c:337-353) handles a put-notify to a
        // DBF link field (INLINK/OUTLINK/FWDLINK) as a dedicated early case,
        // ABOVE the PACT logic and the whole `processNotifyCommon` machinery:
        // "Only dbPutField will change link fields. Also the record is not
        // processed as a result." It writes the value via `dbPutField`
        // (`putFieldType`) and fires the done callback IMMEDIATELY — it never
        // reaches the PACT test, never processes, never defers. So a link
        // field always takes the value even on a busy or permanently-parked
        // record: a bare `sub` (empty `SNAM`) parks PACT=TRUE forever
        // (subRecord.c:119-122), and parking its link-field put on a `PactExit`
        // that never comes drops the value — `caput <sub>.INPA '0'` then reads
        // back "" instead of C's "0". The ordinary write path below already
        // reproduces C's link semantics for these fields (writes the value;
        // `put_drives_processing_of` is false — no link field is `pp` or PROC —
        // so it processes nothing and returns immediate completion), so the
        // only correction the special case needs is to keep a link field OUT
        // of the notify PACT-defer park.
        let is_dbf_link_field = self.is_dbf_link_field(record_name, &field);
        if want_notify {
            let is_restart = notify_request.is_restart();
            let mut guard = rec.write();
            // A restart is already the record's owner, so only PACT can stop it
            // (C skips dbNotify.c:213 for it and falls straight to :225).
            let must_wait = if is_restart {
                guard.is_processing()
            } else if is_dbf_link_field {
                // Ownership only — see `notify_put_has_owner`. Keeping link
                // fields out of the whole decision (not just the PACT arm) left
                // an owned record's link put falling through to the wait-set
                // install below, whose only answer to an occupied slot was
                // `PutCallbackInProgress`.
                guard.notify_put_has_owner()
            } else {
                guard.notify_put_is_owned()
            };
            if must_wait {
                let Some((completion, completion_rx)) = notify_request.into_completion() else {
                    // Unreachable: `want_notify` is exactly "this request

                    // carries a completion".
                    return Ok(crate::server::record::ProcessCompletion::Sync);
                };
                let put = crate::server::record::DeferredNotifyPut {
                    field,
                    value,
                    completion,
                };
                let put = crate::server::record::DeferredNotify::Put(put);
                if is_restart {
                    guard.requeue_notify_put(put);
                } else {
                    guard.queue_notify_put(put);
                }
                // A queued put-notify IS async: it replays and completes on the
                // restart check that frees it (C `notifyRestartInProgress` /
                // `notifyWaitForRestart`, dbNotify.c:213-231). `Deferred` replays
                // carry only the sender, so `completion_rx` is `None` there and
                // this maps to `Sync` — but that path is the internal restart,
                // not a fresh client put.
                return Ok(crate::server::record::ProcessCompletion::from_signal(
                    completion_rx,
                ));
            }
        }

        // PROC intercept: trigger processing on any SCAN.
        // Falls through to the put_notify_tx registration below
        // so async records (motor, asyn-backed AO) signal real
        // completion; otherwise WRITE_NOTIFY would return ECA_NORMAL
        // before the device move actually finished.
        //
        // C `dbPutField` (dbAccess.c:1265) matches the proc field by pointer
        // with NO value check: any write to PROC — including 0 — processes the
        // record (when !pact). The standard `caput REC.PROC 0` / `dbpf REC.PROC
        // 0` force-process idiom must therefore not be skipped for a zero value.
        if field == "PROC" {
            // C `dbCommon.dbd` declares `field(PROC,DBF_UCHAR){ pp(TRUE) }`, so
            // a put to PROC does BOTH: `dbPut` stores the raw byte in
            // `prec->proc` (retained — C never resets it), AND `pp(TRUE)` drives
            // the reprocess below. The prior port kept only the reprocess and
            // dropped the byte, so `caput REC.PROC v; caget REC.PROC` always
            // read 0. Store the byte through the SAME `DBF_UCHAR` common-field
            // path DISP/RPRO use (coercion + signed readback: `caput PROC 255` →
            // `caget` = -1) in its own brief write lock so both the notify and
            // fire-and-forget paths take it, then fall through to force-process.
            // C `dbPut:1408` posts DBE_VALUE|DBE_LOG for the put field (PROC is
            // not the record's value field, so the pp-suppression never applies).
            // Store the raw PROC byte (C `dbChannelPut`). A bad conversion
            // (`caput REC.PROC 256` / non-numeric) refuses the store AND the
            // client's put — but, exactly as C's `putCallback` returns
            // `didPut = 1` while setting `notifyError` (`dbNotify.c:528-530`),
            // the PROC `pp(TRUE)`-driven `dbProcess` (`dbNotify.c:243-261`) still
            // runs on the NOTIFY path. This is the SAME rule the general put path
            // applies for a rejected pp-field conversion (`field_io.rs:1748-1806`,
            // "Cause B"); mirror it here so PROC does not diverge from UDF: carry
            // the refusal, force-process when `want_notify`, then hand the Err
            // back so the client still sees `ECA_PUTFAIL`.
            let proc_store: CaResult<()> = {
                let rec_arc = {
                    let recs = self.inner.records.read();
                    recs.get(record_name).cloned()
                };
                if let Some(rec_arc) = rec_arc {
                    let mut guard = rec_arc.write();
                    match guard.put_common_field("PROC", value) {
                        Ok(_) => {
                            guard.notify_field(
                                "PROC",
                                crate::server::recgbl::EventMask::VALUE
                                    | crate::server::recgbl::EventMask::LOG,
                            );
                            Ok(())
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    Ok(())
                }
            };
            if let Err(e) = proc_store {
                // `want_notify` ⇒ C `ca_put_callback`: the PROC process runs
                // despite the rejected conversion (`didPut == 1`). Fire-and-forget
                // ⇒ C plain `dbPutField`, which returns before `dbProcess` on a
                // non-zero `dbPut` status (`dbAccess.c:1263-1264`), so it must NOT
                // process. Either way the client is answered `ECA_PUTFAIL`.
                if want_notify {
                    let _ = self.put_driven_process_already_locked(record_name);
                }
                return Err(e);
            }
            // A fire-and-forget caller parks nothing — C `dbPutField` on PROC
            // processes the record with no putNotify.
            let parked = if let Some((completion_tx, completion_rx)) =
                notify_request.into_completion()
            {
                // Collect-then-act: clone the handle under a brief map
                // read, drop the map lock before the per-record write.
                let rec_arc = {
                    let recs = self.inner.records.read();
                    recs.get(record_name).cloned()
                };
                match rec_arc {
                    Some(rec_arc) => {
                        match rec_arc.write().install_or_queue_notify(completion_tx) {
                            Some(notify) => Some((notify, completion_rx)),
                            // Queued: the entry gate let this put through, then
                            // another notify took the slot before the install
                            // (`process_record_with_notify` does not hold the
                            // put gate). C queues here rather than refusing, and
                            // the replay is what processes — so return without
                            // driving the record.
                            None => {
                                return Ok(crate::server::record::ProcessCompletion::from_signal(
                                    completion_rx,
                                ));
                            }
                        }
                    }
                    // Record gone between the put and the park: nothing to
                    // install on. Dropping the wait-set releases the client,
                    // which is the completion C gives a `dbNotifyCancel`.
                    None => Some((
                        crate::server::record::NotifyWaitSet::new(completion_tx),
                        completion_rx,
                    )),
                }
            } else {
                None
            };
            // C `dbPutField:1265-1277`: PROC is one of the two fields that
            // selects the record for the put-driven process — with the same
            // PACT→RPRO deferral as a `pp` field. Both go through the single
            // owner (R19-43).
            //
            // The ALREADY-LOCKED entry, unconditionally — NOT `acquire_gate`
            // passed through. By the time control reaches here the record's
            // advisory gate is held on both paths: this function took it above
            // when `acquire_gate`, and the caller (an atomic group PUT) holds it
            // when not. The gate is not reentrant, so acquiring it again
            // here deadlocks every PROC put.
            let _ = self.put_driven_process_already_locked(record_name);
            // The wait-set fires the oneshot only after the whole
            // FLNK/OUT chain (sync + async) settles. If it has
            // already completed the chain was fully synchronous —
            // report immediate success; otherwise hand the receiver
            // to the CA layer to await the deferred completion.
            return match parked {
                Some((notify, completion_rx)) => {
                    if notify.completed() {
                        Ok(crate::server::record::ProcessCompletion::Sync)
                    } else {
                        Ok(crate::server::record::ProcessCompletion::from_signal(
                            completion_rx,
                        ))
                    }
                }
                None => Ok(crate::server::record::ProcessCompletion::Sync),
            };
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
        // Scoped guard: the put body (closure + failure handling) runs under
        // this write guard, whose scope ends (releasing the !Send parking_lot
        // guard) before the notify-process / scan-index awaits below. Yields
        // either the success `CommonFieldPutResult`, or `(error, should_process)`
        // so the notify-driven process on a rejected put runs guard-free.
        let mut pact_exit = PactExitGuard::new(self, record_name, &rec, RestartOwner::ThisPut);
        let outcome: Result<crate::server::record::CommonFieldPutResult, (CaError, bool)> = {
            let mut instance = rec.write();

            // C `db_put_process` (db_access.c:1025-1043) returns 1 (didPut) even
            // when the internal `dbChannelPut` FAILS — a rejected conversion, an
            // SPC_NOMOD refusal, or an after-put `special()` error all set
            // `ppn->status = notifyError` yet still `return 1` — so
            // `processNotifyCommon` (dbNotify.c:243-246) still runs `dbProcess`
            // when the gate passes. The whole put write is therefore wrapped so
            // that ANY failure inside it — `dbput_request`, `special()` pass 0,
            // `put_field`, `special_after_put`, `put_common_field` — is caught at
            // ONE place below: on the notify path we evaluate the SAME process gate
            // the success path uses and process the record as a side effect, then
            // hand the original Err back to the client. On the failing conversion
            // path no field is written — `dbChannelPut` wrote nothing either.
            //
            // On SUCCESS this closure is just C `dbPut`: the monitor posts at its
            // tail run only when the put fully succeeded (C's `goto done` skips
            // them on failure).
            let block_result: CaResult<crate::server::record::CommonFieldPutResult> = (|| {
                // Coerce value to the field's native DBR type (e.g. String → Double for ao.VAL).
                // This matches C EPICS db_put_field() which converts from the CA client's type
                // to the record field's native type.
                let request = dbput_request(&*instance.record, &field, value)?;

                // Pre-write special hook (C EPICS dbPutSpecial pass=0)
                instance.record.special(&field, false)?;
                pact_exit.arm(special_before_put(&mut instance, &field));

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
                                let result = special_after_put(
                                    self,
                                    &mut instance,
                                    &field,
                                    &mut special_actions,
                                )?;
                                // C `dbAccess.c::dbPut:1410-1411` clears
                                // `precord->udf = FALSE` synchronously when the
                                // put target is the record-type's primary value
                                // field (`dbIsValueField`), and clears NOTHING
                                // else. The clear happens BEFORE `dbProcess` runs,
                                // so any reader between the put and the process
                                // cycle sees the new value with a consistent
                                // udf=false — but stat/sevr keep their old
                                // UDF_ALARM until the process cycle recomputes
                                // them. A value put that drives no process leaves
                                // the stale UDF alarm, matching C; the process
                                // path's own `rec_gbl_check_udf` (now a no-op with
                                // udf clear) + `rec_gbl_reset_alarms` clears it
                                // when the record does process. The earlier
                                // synchronous stat/sevr clear here diverged from C.
                                if instance.record.is_udf_defining_put(&field) {
                                    instance.common.udf = 0;
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

                // C `add_count` raises the inverted-limits alarm during a SGNL
                // SPC_MOD `special()` (histogramRecord.c:329-334). The port raises
                // it through `nsta`/`nsev` (CBUG-F12 refused, not C's direct write),
                // so this monitor-less special path commits it — check then reset —
                // to make STAT=SOFT/INVALID observable, matching the process path.
                // Gated on `special_checks_alarms` (histogram SGNL only). No STAT
                // post: C's `add_count` posts nothing, and the special path has no
                // monitor — the alarm shows on the next caget's field read.
                if instance.record.special_checks_alarms(&field) {
                    let inst = &mut *instance;
                    inst.record.check_alarms(&mut inst.common);
                    let _ = crate::server::recgbl::rec_gbl_reset_alarms(&mut inst.common);
                }

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

                // C `dbPut:1408-1414`'s field-monitor post, through the one
                // owner (`dbput_post_put_field`, shared with `put_pv`). On
                // this route the pp-value-field suppression pairs with the
                // `should_process` gate below: a suppressed field is exactly
                // one the reprocess cycle re-posts via the deadband snapshot.
                // (ACKT/ACKS have no arm here: they are SPC_NOMOD, refused by
                // the gate above. Alarm acknowledgement arrives as a DBR
                // request type, through [`Self::put_alarm_ack_from_ca`].)
                dbput_post_put_field(&mut instance, &field);

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
                //
                // C's `monitor()` runs `recGblResetAlarms(prec)` BEFORE those
                // `db_post_events`, and OR-adds the alarm bit it returns into the
                // value posts (compressRecord.c:103-110). The port mirrors that
                // order: commit the alarm here (posting any STAT/SEVR/AMSG/ACKS
                // transition through the one owner, `alarm_field_posts`) and carry
                // the resulting DBE_ALARM into the side-effect posts below. Records
                // whose `special()` does not run `monitor()` return false and skip
                // this entirely (no spurious alarm commit on an unrelated put).
                let side_effect_alarm_mask = if instance.record.special_commits_alarms(&field) {
                    commit_special_reset_alarm(&mut instance)
                } else {
                    crate::server::recgbl::EventMask::NONE
                };

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
                    instance.notify_field(sf, mask | side_effect_alarm_mask);
                }

                // The same `special()` posts, but named by the WRITER instead of
                // by a static table: a record whose put handler re-derived a
                // partner field marks it — with the mask of the C call site that
                // posts it, and only when that field's own comparison moved
                // (sseq `special()` posts the re-rendered `STRn` after a `DOn`
                // put, `DBE_VALUE`, `only if (strcmp(str, plinkGroup->s))`,
                // sseqRecord.c:1108-1116). A static field-name list cannot
                // express "only if it changed", so it over-posts; the mark can.
                emit_cycle_posts(&mut instance);

                Ok(common_result)
            })(
            );

            // Cause B: a put-NOTIFY whose write was rejected must still process.
            // C `db_put_process` returned 1 (didPut) despite the failure above, so
            // `processNotifyCommon` runs `dbProcess` whenever the gate passes.
            // Reuse the SAME `put_drives_processing_of` gate the success tail uses,
            // process the record as a side effect, then return the ORIGINAL Err —
            // the CA layer maps it to PUTFAIL (C `notifyError`) and `put_accepted`
            // stays False, while STAT/SEVR recompute to match C. The notify path
            // ONLY: a plain `dbPutField` failure processes nothing (dbAccess.c:1263
            // processes only when `dbPut` status==0), so `want_notify == false`
            // keeps its Err-without-process behavior. The instance write lock must
            // drop before `put_driven_process_already_locked` re-acquires it.
            match block_result {
                Ok(cr) => Ok(cr),
                Err(e) => {
                    // C `dbPut:83-88` runs `dbPutSpecial(paddr, 1)` UNCONDITIONALLY
                    // ("Always do special processing if needed") — even when the
                    // conversion above failed — before the `goto done` that skips
                    // the udf clear and the field's monitor post. For a compress
                    // SPC_RESET field that means `special()` still runs `monitor()`
                    // → `recGblResetAlarms`, committing the born-UDF alarm to
                    // NO_ALARM though the RES/N put is rejected (a caget then sees
                    // stat/sevr=NO_ALARM with udf still 1, matching C softIoc).
                    // Run the after-put `special()` and its alarm commit here, then
                    // hand back the ORIGINAL Err so the client still sees PUTFAIL.
                    // Gated on `special_commits_alarms` (compress only) so no other
                    // special record runs its after-put hook on a failed conversion.
                    if instance.record.special_commits_alarms(&field) {
                        let _ = instance.record.special(&field, true);
                        let alarm_mask = commit_special_reset_alarm(&mut instance);
                        let value_only = instance.record.value_only_change_fields();
                        for sf in instance.record.monitor_side_effect_fields(&field) {
                            use crate::server::recgbl::EventMask;
                            let mask = if value_only.iter().any(|f| f.eq_ignore_ascii_case(sf)) {
                                EventMask::VALUE
                            } else {
                                EventMask::VALUE | EventMask::LOG
                            };
                            instance.notify_field(sf, mask | alarm_mask);
                        }
                    }
                    // The same `dbPutSpecial(paddr, 1)`-on-reject rule for a field
                    // whose special() raises the inverted-limits alarm (histogram
                    // SGNL → add_count): C still runs add_count when the SGNL
                    // conversion fails, so STAT=SOFT/INVALID appears even for a
                    // rejected `caput .SGNL notanumber`. The port raises it through
                    // `nsta`/`nsev` (CBUG-F12 refused) and commits it here — check
                    // then reset — since no process follows.
                    if instance.record.special_checks_alarms(&field) {
                        let inst = &mut *instance;
                        inst.record.check_alarms(&mut inst.common);
                        let _ = crate::server::recgbl::rec_gbl_reset_alarms(&mut inst.common);
                    }
                    // The same `dbPutSpecial(paddr, 1)`-on-reject rule for a field
                    // whose special() clears UDF: C `mbboDirectRecord.c::special`
                    // (after==1, B0..B1F, line 290) sets `prec->udf = FALSE`, and
                    // that special runs UNCONDITIONALLY in `dbPut` (dbAccess.c:1401,
                    // "Always do special processing") even when the value conversion
                    // failed — BEFORE the `if (status) goto done`. So a rejected
                    // `caput -c mbboDirect.Bn 256`/`notanumber` still clears UDF, and
                    // the notify-process that follows recomputes STAT/SEVR to
                    // NO_ALARM instead of the born-UDF INVALID (verified live against
                    // the C softIoc: fresh record → rejected Bn put → NO_ALARM,
                    // udf=0). The success path clears UDF for this same field set via
                    // `is_udf_defining_put` (the `udf = 0` at the tail of the put
                    // body). The primary VAL field is EXCLUDED here: its UDF clear is
                    // `isValueField` (dbAccess.c:1408), which runs AFTER the status
                    // check, so a rejected VAL put keeps UDF — matching C. Only
                    // mbboDirect overrides `is_udf_defining_put` to add non-primary
                    // fields, so this is a no-op for every other record type.
                    if instance.record.is_udf_defining_put(&field)
                        && field != instance.record.primary_field()
                    {
                        instance.common.udf = 0;
                    }
                    let should_process = want_notify && put_drives_processing_of(&instance, &field);
                    Err((e, should_process))
                }
            }
        };
        // Lock down: arm any put-notify the SNAM park release freed.
        drop(pact_exit);

        let common_result = match outcome {
            Ok(cr) => cr,
            Err((e, should_process)) => {
                if should_process {
                    let _ = self.put_driven_process_already_locked(record_name);
                }
                return Err(e);
            }
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
        self.run_special_actions(record_name, &rec, std::mem::take(&mut special_actions));

        // Update scan index if SCAN or PHAS changed
        match common_result {
            crate::server::record::CommonFieldPutResult::ScanChanged {
                old_scan,
                new_scan,
                phas,
            } => {
                self.update_scan_index(record_name, old_scan, new_scan, phas, phas);
            }
            crate::server::record::CommonFieldPutResult::PhasChanged {
                scan: s,
                old_phas,
                new_phas,
            } => {
                self.update_scan_index(record_name, s, s, old_phas, new_phas);
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
            let instance = rec.read();
            put_drives_processing_of(&instance, &field)
        };

        if !should_process {
            // No processing cycle, so C never raises `putf` (and this put did
            // not either). Report immediate (synchronous) completion to a
            // WRITE_NOTIFY caller.
            return Ok(crate::server::record::ProcessCompletion::Sync);
        }

        // Set up the put-notify wait-set BEFORE processing. The wait-set
        // fires `completion_tx` only after the originating record AND
        // every FLNK/OUT chain target it triggers (sync or async) has
        // completed — C `dbNotify.c` `processNotify`/`dbNotifyCompletion`.
        // An occupied slot is QUEUED, never refused. C
        // `processNotifyCommon` answers "another processNotify owns the
        // record" with `ellSafeAdd` onto `restartList` (dbNotify.c:211-217)
        // and carries no refusal arm: `S_db_Blocked` is raised by a test
        // record's own put hook (test/ioc/db/xRecord.c:89), never by the
        // notify machinery, and `ECA_PUTCBINPROG` has exactly one sender in
        // all of base — the put-callback timeout at rsrv/camessage.c:1745.
        // Overwriting the slot instead would drop the prior Sender, waking
        // the prior caller's rx with RecvError that the CA dispatcher treats
        // as success, so the install goes through the one owner.
        //
        // A fire-and-forget put parks NOTHING — C builds a `putNotify`
        // only in `dbPutNotify`; `dbPutField` processes the record with
        // no notify state at all. It therefore neither conflicts with
        // nor disturbs a WRITE_NOTIFY already parked on the record.
        let parked = if let Some((completion_tx, completion_rx)) = notify_request.into_completion()
        {
            // Collect-then-act: clone the handle under a brief map read,
            // drop the map lock before the per-record write.
            let rec_arc = {
                let recs = self.inner.records.read();
                recs.get(record_name).cloned()
            };
            match rec_arc {
                Some(rec_arc) => match rec_arc.write().install_or_queue_notify(completion_tx) {
                    Some(notify) => Some((notify, completion_rx)),
                    // Queued behind the slot's current owner — see the PROC
                    // path above. The replay drives the record; returning here
                    // is what keeps one client request to one process cycle.
                    None => {
                        return Ok(crate::server::record::ProcessCompletion::from_signal(
                            completion_rx,
                        ));
                    }
                },
                None => Some((
                    crate::server::record::NotifyWaitSet::new(completion_tx),
                    completion_rx,
                )),
            }
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
                let recs = self.inner.records.read();
                recs.get(record_name).cloned()
            };
            if let Some(rec_arc) = rec_arc {
                let mut guard = rec_arc.write();
                if guard.record.soft_channel_skips_convert() {
                    guard.record.set_device_did_compute(true);
                }
            }
        }

        // Process the record after field put — through the single owner of C's
        // `dbPutField:1269-1277` decision, so an async-active record takes the
        // RPRO deferral instead of a doomed re-entrant `dbProcess`.
        let _ = self.put_driven_process_already_locked(record_name);

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
            let rec = self.inner.records.read();
            if let Some(rec_arc) = rec.get(record_name) {
                rec_arc.read().notify.is_some()
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
                let recs = self.inner.records.read();
                recs.get(record_name).cloned()
            };
            if let Some(rec_arc) = rec_arc {
                let mut guard = rec_arc.write();
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
                    Ok(crate::server::record::ProcessCompletion::Sync)
                } else {
                    Ok(crate::server::record::ProcessCompletion::from_signal(
                        completion_rx,
                    ))
                }
            }
            None => Ok(crate::server::record::ProcessCompletion::Sync),
        }
    }

    /// Put a PV value without triggering process (for restore).
    ///
    /// Unlike the `dbPut`-analogue bodies (`put_pv`, `put_pv_and_post`) this
    /// entry accepts DBF link fields: its production caller is the autosave
    /// restore, whose C analogue (`reboot_restore`) writes via `dbPutField` —
    /// and `dbPutField` on a link field is `dbPutFieldLink`, a re-parse
    /// write that never processes. That is exactly this body's behavior
    /// (`put_common_field`'s INP/OUT/FLNK arms, no process), so the
    /// `check_not_link_field` refusal does not apply here.
    pub async fn put_pv_no_process(&self, name: &str, value: EpicsValue) -> CaResult<()> {
        let (base, field) = super::parse_pv_name(name);
        let field = field.to_ascii_uppercase();

        let simple = self.inner.simple_pvs.lock().get(name).cloned();
        if let Some(pv) = simple {
            pv.set(value);
            return Ok(());
        }

        // Records — alias-aware (epics-base PR #336).
        if let Some(rec) = self.get_record(base) {
            // `put_pv_no_process` is a public record-write API
            // (autosave restore). It must take the advisory write gate
            // (`dbScanLock` analogue) so an autosave restore cannot
            // land between the member writes of a QSRV atomic group or
            // a pvalink atomic scan epoch holding `lock_records`.
            // `base` is alias-resolved so an alias and its target
            // share one gate. Held until return.
            let canonical_base: String =
                self.resolve_alias(base).unwrap_or_else(|| base.to_string());
            let _record_gate = self.lock_record(&canonical_base);

            let mut instance = rec.write();

            // C `dbPutSpecial(paddr, 0)` — the pre-store pass, which
            // `dbPut` runs on every entry path including the `dbPutField`
            // this body models (autosave's `reboot_restore`). A non-zero
            // status returns before the store (`dbAccess.c:1350-1352`), so a
            // restore cannot write a field the record is currently refusing
            // (mbboDirect B0..B1F while OMSL=closed_loop,
            // `mbboDirectRecord.c:263-269`). The other two `dbPut` bodies in
            // this module already ran it; this one did not.
            instance.record.special(&field, false)?;

            let prev_value = instance.record.get_field(&field);
            match instance.record.put_field(&field, value.clone()) {
                Ok(()) => {}
                Err(CaError::FieldNotFound(_)) => {
                    instance.put_common_field(&field, value)?;
                }
                Err(e) => return Err(e),
            }
            // C `dbAccess.c::dbPut:1414` `if (isValueField) precord->udf =
            // FALSE;` — the same clear, through the same predicate, as the
            // three other put bodies. An autosave restore of VAL defines the
            // record in C, and a record that is left UDF here reports the
            // born UDF_ALARM on its first process cycle.
            if instance.record.is_udf_defining_put(&field) {
                instance.common.udf = 0;
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
    use super::NotifyArrival;
    use crate::types::EpicsValue;

    /// Regression: prior to fixing B1, `put_pv_and_post` walked only
    /// `inner.records` and returned `ChannelNotFound` for everything
    /// `add_pv`-registered. The CA gateway's monitor forwarder uses
    /// `add_pv` then expects `put_pv_and_post` to fan-out to
    /// downstream subscribers — without the simple-PV branch, every
    /// upstream event was silently dropped and the gateway delivered
    /// no monitors.
    #[epics_macros_rs::epics_test]
    async fn put_pv_and_post_handles_simple_pv() {
        let db = PvDatabase::new();
        db.add_pv("gw:test", EpicsValue::Double(0.0)).await.unwrap();

        // Should NOT return ChannelNotFound.
        db.put_pv_and_post("gw:test", EpicsValue::Double(42.0))
            .await
            .expect("simple PV put_pv_and_post must succeed");

        // Value actually landed.
        let pv = db.find_pv("gw:test").await.expect("PV exists");
        assert!(matches!(pv.get(), EpicsValue::Double(v) if v == 42.0));
    }

    /// Regression: `get_pv`, `put_pv`, `put_pv_and_post`,
    /// and `put_pv_no_process` all bypassed `get_record` and walked
    /// `self.inner.records` directly, so alias names from epics-base
    /// PR #336 silently returned `ChannelNotFound`. A later fix closed
    /// `get_record` but the same defect was hiding in field_io.rs.
    /// All four CA-server-and-bridge entry points must accept aliases.
    #[epics_macros_rs::epics_test]
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
        let v = db.get_pv("ALT.VAL").unwrap();
        assert!(matches!(v, EpicsValue::Double(x) if x == 1.5));

        // put_pv via alias
        db.put_pv("ALT.VAL", EpicsValue::Double(7.0)).await.unwrap();
        let v = db.get_pv("CANON.VAL").unwrap();
        assert!(matches!(v, EpicsValue::Double(x) if x == 7.0));

        // put_pv_and_post via alias
        db.put_pv_and_post("ALT.VAL", EpicsValue::Double(11.0))
            .await
            .unwrap();
        let v = db.get_pv("CANON.VAL").unwrap();
        assert!(matches!(v, EpicsValue::Double(x) if x == 11.0));

        // put_pv_no_process via alias
        db.put_pv_no_process("ALT.VAL", EpicsValue::Double(13.0))
            .await
            .unwrap();
        let v = db.get_pv("ALT.VAL").unwrap();
        assert!(matches!(v, EpicsValue::Double(x) if x == 13.0));
    }

    /// A `DBR_STRING` menu label written to a `DBF_MENU` field resolves
    /// against THAT field's own menu (C `dbConvert` `putStringMenu`), not the
    /// field-blind global table that `EpicsValue::convert_to` would consult.
    /// Covers the `put_pv` (`put_pv_inner`) and `put_pv_and_post` coercion
    /// sites; the CA field-put path (`put_record_field_from_ca_inner`) shares
    /// the identical `coerce_write_value` helper.
    #[epics_macros_rs::epics_test]
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
        assert_eq!(db.get_pv("SEL.SELM").unwrap(), EpicsValue::Enum(0));

        // put_pv_and_post: a later choice, proving the whole menu.
        db.put_pv_and_post("SEL.SELM", EpicsValue::String("High Signal".into()))
            .await
            .unwrap();
        assert_eq!(db.get_pv("SEL.SELM").unwrap(), EpicsValue::Enum(1));

        // A bare numeric string still resolves (C epicsParseUInt16 fallback).
        db.put_pv("SEL.SELM", EpicsValue::String("2".into()))
            .await
            .unwrap();
        assert_eq!(db.get_pv("SEL.SELM").unwrap(), EpicsValue::Enum(2));
    }

    /// `set_pv_metadata` installs the upstream `DBR_CTRL_*` metadata on a
    /// shadow simple PV WITHOUT posting any event (the CA gateway's
    /// connect-time seed). A later GET-class read must then see the
    /// installed limits/units, and a `DBE_PROPERTY` subscriber must NOT
    /// have received anything (nothing *changed* yet). An unknown / record
    /// name is rejected with `ChannelNotFound`.
    #[epics_macros_rs::epics_test]
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
    #[epics_macros_rs::epics_test]
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
            .expect("property subscriber added");
        let mut val_rx = pv
            .add_subscriber(2, DbFieldType::Double, DBE_VALUE)
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
            ev.snapshot
                .display
                .clone()
                .expect("event carries metadata")
                .units,
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
    #[epics_macros_rs::epics_test]
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
        let v = db.get_pv("CANON.VAL").unwrap();
        assert!(matches!(v, EpicsValue::Double(x) if x == 2.5));
    }

    /// Regression: a DBR_PUT_ACKT alarm-acknowledge put posts a record-wide
    /// DBE_ALARM (C `dbAccess.c:1299` putAckt
    /// `db_post_events(precord, NULL, DBE_ALARM)`), so an alarm-mask monitor
    /// on ANY field is notified — and a DBE_VALUE-only monitor is not.
    /// Pre-fix the ack field posted only itself with DBE_VALUE|DBE_LOG, so no
    /// alarm-mask subscriber observed the acknowledgement, and the post fired
    /// on every put regardless of whether `ackt` changed.
    #[epics_macros_rs::epics_test]
    async fn alarm_ack_put_posts_record_wide_dbe_alarm() {
        use crate::server::recgbl::EventMask;
        use crate::server::records::ai::AiRecord;
        use crate::types::DbFieldType;

        let db = PvDatabase::new();
        db.add_record("A:REC", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        let rec = db.get_record("A:REC").expect("record exists");

        let (mut alarm_rx, mut value_rx) = {
            let mut inst = rec.write();
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
    #[epics_macros_rs::epics_test]
    async fn post_property_fields_writes_and_posts_dbe_property_only() {
        use crate::server::recgbl::EventMask;
        use crate::server::records::mbbi::MbbiRecord;
        use crate::types::DbFieldType;

        let db = PvDatabase::new();
        db.add_record("M:ENUM", Box::new(MbbiRecord::new(0)))
            .await
            .unwrap();
        let rec = db.get_record("M:ENUM").expect("record exists");

        let (mut prop_rx, mut val_rx) = {
            let mut inst = rec.write();
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
            .expect("post_property_fields succeeds");
        assert_eq!(posted, vec!["ZRST".to_string()]);

        // The field landed on the record.
        assert_eq!(
            db.get_pv("M:ENUM.ZRST").unwrap(),
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
    #[epics_macros_rs::epics_test]
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

        let got = crate::runtime::task::timeout(std::time::Duration::from_secs(1), sub.recv_f64())
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
    #[epics_macros_rs::epics_test]
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
            crate::runtime::task::timeout(std::time::Duration::from_millis(200), sub.recv_f64())
                .await
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

    /// The notify slot has exactly two states when a restart replay installs
    /// on it, and both must be handled by the one install owner.
    ///
    /// C cannot reach the occupied one: `restartCheck` (dbNotify.c:149-168)
    /// pops the head and assigns `precord->ppn = pfirst` under a single lock,
    /// so a restarted notify already owns the record before its callback runs.
    /// The port pops under the put gate and installs a moment later, both
    /// through the one gate-held owner. Assigning over the slot would drop the
    /// prior client's
    /// Sender, and its receiver then wakes with `RecvError`, which the CA
    /// dispatcher reads as a successful put-callback: the client is told its
    /// write completed on a cycle that never ran.
    #[epics_macros_rs::epics_test]
    async fn a_replay_onto_a_taken_notify_slot_queues_instead_of_overwriting() {
        use crate::server::records::ai::AiRecord;

        let db = PvDatabase::new();
        db.add_record("REPLAY:TAKEN", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        let rec = db.get_record("REPLAY:TAKEN").expect("record exists");

        let (owner_tx, _owner_rx) = crate::runtime::sync::oneshot::channel();
        let owner = rec
            .write()
            .install_or_queue_notify(owner_tx)
            .expect("a free slot installs");

        let (client_tx, _client_rx) = crate::runtime::sync::oneshot::channel();
        assert!(
            db.install_notify_and_process_already_locked(
                "REPLAY:TAKEN",
                client_tx,
                NotifyArrival::Replay
            )
            .expect("the record is loaded")
            .is_none(),
            "the slot is owned, so the replay must drive no process cycle"
        );
        assert!(
            rec.read()
                .notify
                .as_ref()
                .is_some_and(|n| std::sync::Arc::ptr_eq(n, &owner)),
            "the owner's wait-set must survive the replay"
        );
        assert!(
            rec.read().notify_restart_pending(),
            "the replay must be queued behind the owner, not dropped"
        );
    }

    /// The other boundary value of the same slot: free, so the replay installs
    /// and drives the cycle.
    #[epics_macros_rs::epics_test]
    async fn a_replay_onto_a_free_notify_slot_installs_and_drives_the_cycle() {
        use crate::server::records::ai::AiRecord;

        let db = PvDatabase::new();
        db.add_record("REPLAY:FREE", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        let rec = db.get_record("REPLAY:FREE").expect("record exists");

        let (client_tx, _client_rx) = crate::runtime::sync::oneshot::channel();
        assert!(
            db.install_notify_and_process_already_locked(
                "REPLAY:FREE",
                client_tx,
                NotifyArrival::Replay
            )
            .expect("the record is loaded")
            .is_some(),
            "a free slot installs and processes"
        );
        assert!(
            !rec.read().notify_restart_pending(),
            "nothing is queued when the replay took the slot"
        );
    }
}
