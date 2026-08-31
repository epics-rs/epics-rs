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
/// (camessage.c:2540-2551), which feeds the CA ACCESS_RIGHTS write bit. This
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

/// [`check_no_mod`] as `dbPut` runs it — the refusal that also SPEAKS.
///
/// C's SPC_NOMOD arm is not a bare `return`: it reports before it returns.
///
/// ```c
/// /* dbAccess.c:122-127, dbPutSpecial(paddr, 0) */
/// if ((special == SPC_NOMOD) && (pass == 0)) {
///     status = S_db_noMod;
///     recGblDbaddrError(status, paddr, "dbPut");
///     return status;
/// }
/// ```
///
/// `errSymLookup(S_db_noMod)` is `"Attempt to modify noMod field"`
/// (`dbAccessDefs.h:179`), so the console line is exactly
/// `recGblDbaddrError: dbPut Attempt to modify noMod field PV: REC.FIELD` —
/// measured on softIoc @`R7.0.10` for `dbpf T:SUB.INAM` and `dbpf T:SEL.VAL`,
/// where stdout still shows the unchanged read-back and the refusal is
/// announced only here. A port that returns the status and says nothing loses
/// the whole report, because `dbpf` itself prints no diagnostic of its own.
///
/// INVARIANT: every `dbPut`-layer SPC_NOMOD refusal MUST write this line, and
/// no other layer may. The gate above stays silent so
/// [`PvDatabase::check_external_put_preconditions`] — pvxs `doPreProcessing`,
/// which refuses ABOVE `dbPut` and never reaches `dbPutSpecial` — cannot emit
/// a line C does not write, nor double it on the routes that go on to put.
fn check_no_mod_in_db_put(
    instance: &crate::server::record::RecordInstance,
    field: &str,
) -> CaResult<()> {
    check_no_mod(instance, field).inspect_err(|_| {
        crate::server::recgbl::rec_gbl_dbaddr_error(
            "Attempt to modify noMod field",
            &instance.name,
            field,
            "dbPut",
        );
    })
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

/// C `dbPutFieldLink`'s parse-and-type gate (`dbAccess.c:1098-1135`) — the
/// half of `dbPutField` that only a link field reaches.
///
/// `dbPutField` routes on the DECLARED DBF class and on nothing else: `if
/// (dbfType >= DBF_INLINK && dbfType <= DBF_FWDLINK) return
/// dbPutFieldLink(...)` (`dbAccess.c:1259-1260`). `dbPutFieldLink` then parses
/// the text (`dbParseLink`, `:1098`) and holds it to `dbCanSetLink` (`:1131`)
/// against the device support the record's CURRENT `DTYP` binds — only `INP`
/// and `OUT` have one, so every other link field is held to `CONSTANT`, which
/// `dbCanSetLink` treats as interchangeable with `PV_LINK` and `JSON_LINK`
/// (`dbStaticLib.c:2408-2416`). A mismatch is `S_dbLib_badField`: the field
/// keeps the text it had, the record is not touched, and NOTHING is printed —
/// the refusal reaches the caller only as the put's status.
///
/// The port ran this rule from `put_common_field`'s `INP` and `OUT` arms, and
/// a pair of arms is a name list: `SDIS`, `TSEL`, `FLNK`, `SIML`, `SIOL`,
/// `calc.INPA`, `ao.DOL` and `fanout.LNK1` all stored what C refuses, through
/// `dbpf` and through CA alike. Keyed on the class here, beside
/// [`check_not_link_field`], both `dbPutField`-analogue bodies take the rule
/// from one owner and a link field the generator adds is covered the day it is
/// declared rather than the day someone remembers it.
///
/// Runs ABOVE [`check_no_mod`], because C's link route leaves `dbPutField`
/// before `dbPut` is called at all (`dbAccess.c:1259` vs `:1262`) and the
/// `SPC_NOMOD` refusal a link field can still take arrives later, from
/// `dbPutSpecial(paddr, 0)` at `dbAccess.c:1174`. `aSub.SUBL` is the one field
/// in the vendored population where that order is visible — `DBF_INLINK` and
/// `special(SPC_NOMOD)` both (`aSubRecord.dbd.pod:148-153`), pinned by
/// `asub_subl_is_the_only_read_only_link_field` — and it is visible as the
/// STATUS: C answers `S_dbLib_badField` to `dbpf ASUB.SUBL "@instio p"`, where
/// a no-mod-first order would answer `S_db_noMod`. A type-valid text on the
/// same field reaches the no-mod gate and is refused there, as in C.
///
/// Takes the record's DECLARATION pair rather than the instance, because that
/// is the whole of C's input here — `dbPutFieldLink` reads `paddr->precord`
/// only for its `DTYP`-bound `devSup` — and because a rule stated over
/// `(record_type, dtyp)` can be swept over every type the generator emits
/// without instantiating any of them, which is what
/// `every_declared_link_class_field_is_gated_by_class` does.
///
/// Returns the value to STORE rather than a bare verdict, because in C the
/// text the gate parses and the text the field ends up holding are the same
/// buffer — `dbParseLink` and `dbSetLink` both read `pstring`
/// (`dbAccess.c:1098`, `:1176`). Answering only "may this proceed?" and
/// letting the store path re-derive its own text is what let a
/// `DBR_CHAR` NUL — C's way of spelling "clear this link" — land as the
/// literal `"0"`. `None` means `field` is not a link field and the caller's
/// own value stands.
///
/// `dtyp` is the record's `DTYP` text, empty when the `.db` never spelled it;
/// `field` must already be upper-cased.
fn check_link_put(
    record_type: &str,
    dtyp: &str,
    field: &str,
    value: &EpicsValue,
) -> CaResult<Option<EpicsValue>> {
    if crate::types::dbf_link_class(record_type, field).is_none() {
        return Ok(None);
    }
    // C's request-type switch (`dbAccess.c:1084-1096`), which is the whole of
    // what a link field accepts: `DBR_STRING`, or a `DBR_CHAR`/`DBR_UCHAR`
    // buffer whose LAST element is the NUL — `pstring[nRequest - 1] != '\0'`
    // is `S_db_badDbrtype`, and so is every other request type. Measured
    // against softIoc R7.0.10 on SDIS, INPA, OUT and LNK1: a `DBR_DOUBLE`,
    // `DBR_LONG`, `DBR_SHORT` or `DBR_ENUM` put fails the channel write and
    // leaves the link alone, where the same put to a plain `DBF_STRING` field
    // (`DESC`) is converted and stored.
    let bad_type = || {
        CaError::BadDbrType(format!(
            "dbPutFieldLink: {field} takes a string or a NUL-terminated char array"
        ))
    };
    let text = match value {
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        // `DBR_STRING` with `nRequest > 1`: C reads `pstring` and so takes the
        // first string, without objecting to the count.
        EpicsValue::StringArray(v) => v
            .first()
            .map(|s| s.as_str_lossy().into_owned())
            .unwrap_or_default(),
        // `nRequest == 1`, so the one byte IS `pstring[nRequest - 1]` and must
        // be the terminator; the link text is then empty, which is how a CA
        // client clears a link with a single NUL.
        EpicsValue::Char(b) | EpicsValue::UChar(b) => {
            if *b != 0 {
                return Err(bad_type());
            }
            String::new()
        }
        EpicsValue::CharArray(b) | EpicsValue::UCharArray(b) => {
            if b.last() != Some(&0) {
                return Err(bad_type());
            }
            let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
            String::from_utf8_lossy(&b[..end]).into_owned()
        }
        _ => return Err(bad_type()),
    };
    crate::server::record::check_link_assignment(record_type, Some(dtyp), field, &text)?;
    Ok(Some(EpicsValue::String(text.into())))
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
fn emit_cycle_posts(
    instance: &mut crate::server::record::RecordInstance,
    backing: crate::server::database::LinkBacking<'_>,
) {
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
        instance.notify_field_backed(sf, mask, backing);
    }
}

/// The registry half of a SNAM `special()` — C `subRecord.c::special`
/// (`:170-194`) and `aSubRecord.c::special` (`:552-578`), which resolve
/// `prec->snam` through `registryFunctionFind` and assign `prec->sadr`.
///
/// C runs it as `dbPutSpecial(paddr, 1)`, AFTER the field is stored
/// (`dbAccess.c:1350-1403`), so the name resolved is the STORED one:
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
/// used to write SNAM while running NEITHER `special()` pass and so left the
/// binding stale; it reaches this owner through [`special_after_put`] like
/// every other put route. The remaining writers are `#[test]` fixtures binding
/// a routine without a registry.
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
        // succeeds (`aSubRecord.c:560-561`, stored at `:575`). sub's C returns
        // at `subRecord.c:182-186` — `epicsPrintf`, `pact = TRUE` — WITHOUT
        // reaching the `sadr` assignment, so the routine stays bound and the
        // park is what stops it running. `parks_pact()` is that distinction:
        // the record answering yes has just entered the parked state this put
        // created. Clearing there too would make `RecordInstance::subroutine`
        // mean either "nothing bound" or "a parked record's retained routine"
        // depending on the record type.
        if instance.record.parks_pact() {
            // The other half of that same C branch, and the reason a client
            // can see for the park: `epicsPrintf("%s.SNAM is empty\n",
            // prec->name)` (`subRecord.c:183`), which is `errlogPrintf`
            // (`errlog.h:90`) — the errlog, not stderr. It fires on EVERY put
            // that leaves SNAM empty, including empty -> empty, because pass 0
            // released the park before the store and this pass re-takes it.
            //
            // `parks_pact()` gates the line for the same reason it gates the
            // retained binding above: in C the print and `pact = TRUE` are one
            // block, and the only other record whose `special()` reaches an
            // empty subroutine name is aSub, which is silent AND park-free
            // (`aSubRecord.c:560-561`). A record that answers `parks_pact()`
            // therefore owes this line; `enter_pact()` in `special_after_put`
            // takes the park itself, on the same predicate, straight after.
            crate::runtime::log::errlog_printf(&format!("{}.SNAM is empty\n", instance.name));
        } else {
            instance.subroutine = None;
        }
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
/// It runs whether or not the store landed. C brackets it with
/// `/* Always do special processing if needed */` (`dbAccess.c:1398`) and
/// `if (status2) status = status2;` (`:1401-1402`), so on a refused store the
/// pass still runs and its status REPLACES the store's; `if (status) goto done`
/// (`:1404`) then skips the UDF clear and the field's monitor post. That is
/// also what keeps the pair balanced: [`special_before_put`] has already
/// latched OLDSIMM through `recGblSaveSimm`, and a store error returning
/// between the two would leave the latch with nothing to consume it.
///
/// Returns the scan-index delta the after-put pass produced — non-`NoChange`
/// only for the SIMM↔SSCN swap below, which the caller applies through
/// `update_scan_index` once the record lock is down.
fn special_after_put(
    db: &PvDatabase,
    instance: &mut crate::server::record::RecordInstance,
    field: &str,
    out: &mut Vec<crate::server::record::ProcessAction>,
    backing: crate::server::database::LinkBacking<'_>,
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
    // C's `prec->udf = FALSE` written from inside `special()` — histogram's
    // `clear_histogram` (`histogramRecord.c:361`) is the only one. Drained here
    // and unconditionally for the same reason the actions above are: C performs
    // the assignment before any status can divert `dbPut`.
    if instance.record.take_udf_clear() {
        instance.common.udf = 0;
    }
    if status.is_err() {
        // The POSTS `special()` made are drained on the failing path for the same
        // reason its ACTIONS are: C's `special()` calls `db_post_events` BEFORE it
        // returns nonzero — aCalcout's NUSE arm posts the clamped value with
        // `DBE_VALUE` and only then `return (-1)` (`aCalcoutRecord.c:495-499`) —
        // and `dbPut`'s `goto done` skips only dbPut's OWN post. A refused put
        // that repaired a field must still tell the subscribers what it repaired.
        emit_cycle_posts(instance, backing);
    }
    status?;

    // C `special()`'s CONSTANT-link re-seed (`calcoutRecord.c:373-378`,
    // `sCalcoutRecord.c:513-518`, `aCalcoutRecord.c:533-538`,
    // `transformRecord.c:715-723` — the four records whose C `special()` calls
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
        // `value_field` is the calc-class value slot behind the link just
        // written (`INPA` -> `A`), i.e. exactly a link-backed field.
        instance.notify_field_backed(value_field, mask, backing);
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
/// [`Record::parks_pact`](crate::server::record::Record::parks_pact) and this performs the transition.
///
/// The release returns NOTHING to the caller, deliberately. C's `special` here
/// is the two stores and no more — it does not call `dbNotifyCompletion`, and
/// nothing else on a `dbPut` path does either. `restartCheck` has six call sites
/// in `dbNotify.c`, and none is reachable from a put body: `:461` and `:465` are
/// `dbNotifyCompletion`, the cycle tail; `:430` and `:434` are `dbNotifyCancel`
/// (the symbol is `dbNotifyCancel`, `:385` — there is no `dbProcessNotifyCancel`
/// in base) and `:290` is `notifyCallback`'s `cancelWait` branch, so a cancel;
/// `:266` is neither — it sits in `processNotifyCommon` on the
/// `notifyRestartCallbackRequested` arm, which is entered only from the callback
/// (`:298`, `first == 0`) or from `dbProcessNotify` (`:382`, `first == 1`) and
/// never from a field store. A put-notify parked on this record therefore stays
/// parked across the release and is replayed by the record's NEXT cycle tail,
/// through `PvDatabase::end_process_cycle` → `apply_pact_exit`, the single drain
/// of `RecordInstance::notify_restart_list`.
///
/// So the `PactExit` this mints is dropped on purpose and loses nothing: the
/// queue is on the record, and `RecordInstance::pact_exit_without_release`
/// re-derives the bit at that tail. Do not re-add a put-body consumer for it —
/// arming here is what made a `caput SUB.SNAM` complete a put-callback C leaves
/// outstanding, and the cycle-ordering patch that contained the damage
/// (a caller-declared `RestartOwner`) promised a cycle two frames before
/// `write_db_link_value` decides whether one runs.
fn special_before_put(instance: &mut crate::server::record::RecordInstance, field: &str) {
    if field == "SIMM" {
        instance.rec_gbl_save_simm();
    }
    if pact_park_field(&*instance.record, field)
        && instance.record.parks_pact()
        && instance.is_processing()
    {
        instance.common.rpro = 0;
        let _ = instance.leave_pact();
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
/// recGbl.c:212 — set iff any alarm-class field moved this cycle).
///
/// Shared by `dbPut`'s success tail and its rejected-conversion path: C runs
/// `dbPutSpecial(paddr, 1)` on BOTH (dbAccess.c:1398-1404, "Always do special
/// processing if needed", before the `goto done` that bails on a failed put) —
/// a conversion that sets `status` at :1362/:1386 does NOT jump over it.
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
/// The client-`dbPut` entry
/// ([`crate::server::record::dbput_coerce_value`]), which renders the request
/// in the destination field's shape and then runs the type row; the
/// internal-delivery entry is `put_field_internal_default`. A `DBR_STRING` write to a `DBF_MENU` or
/// `DBF_ENUM` field has a converter of its own in C (`putStringMenu`,
/// `putStringEnum`) and must not fall through to `EpicsValue::convert_to`,
/// which is field-blind and turns any unrecognised string into index 0 —
/// C stores nothing and fails the put with `S_db_badChoice`.
fn coerce_write_value(
    record: &dyn crate::server::record::Record,
    field: &str,
    target: crate::types::DbFieldType,
    value: EpicsValue,
) -> CaResult<crate::types::c_parse::Converted> {
    crate::server::record::dbput_coerce_value(record, field, target, value)
}

/// What a `dbPut` of a given value means for a given field — the single owner
/// of C `dbPut`'s value branch (`dbAccess.c:1350-1391`, tag `R7.0.10`).
enum PutRequest {
    /// Write this value; already coerced to the field's native type.
    Write(EpicsValue),
    /// C stored nothing and the put still returns **success** — `status` stays
    /// 0 either way. Two converters reach it: the ZERO-element request below,
    /// and `cvt_st_ul`'s skipped store
    /// ([`crate::types::c_parse::Converted::Unchanged`]), where the double
    /// fallback parsed but landed outside `0..=UINT_MAX`.
    ///
    /// `alarm` is the SCALAR arm's `recGblSetSevr(precord, LINK_ALARM,
    /// INVALID_ALARM)` (`dbAccess.c:1370-1371`, commit `12cfd418d`, whose
    /// subject is "fix dbPut to *set* the target to INVALID/LINK alarm when
    /// writing empty arrays into scalars" — not to reject the put). The ARRAY
    /// arm never reaches that line, so a zero-length request into a
    /// `special(SPC_DBADDR)` field raises nothing.
    StoreNothing { alarm: bool },
}

/// Resolve a put into its C `dbPut` branch.
///
/// C picks the arm at `dbAccess.c:1350`:
/// `if (nRequest>1 || paddr->pfldDes->special == SPC_DBADDR)`. Two independent
/// tests, and neither is `no_elements`, which appears only in the ARRAY arm's
/// clamp at `:1360` (`if (no_elements < nRequest) nRequest = no_elements;`).
/// The `SPC_DBADDR` disjunct is the half that matters here: it sends a
/// ZERO-length request into the array arm, where `dbPutConvertRoutine`
/// (`:1362`) copies nothing, `put_array_info(paddr, 0)` (`:1367`) drops the
/// valid length, `status` stays 0 and the put falls through to the UDF clear.
/// The scalar arm's `recGblSetSevr(LINK_ALARM, INVALID_ALARM)` at `:1371` is
/// therefore unreachable for an `SPC_DBADDR` field.
///
/// So the branch asks [`crate::server::record::FieldDeclaration::field_is_dbaddr`], the port's owner of
/// the `.dbd` declaration. Keying it on the destination's CURRENT VALUE SHAPE
/// instead is what made `caput -a MBBO.VAL 0` — `mbbo.VAL` is
/// `special(SPC_DBADDR)` (`mbboRecord.dbd.pod:194`) but stored as a scalar —
/// take the scalar arm and come back LINK/INVALID where the C IOC returns
/// NO_ALARM.
///
/// The clamp keeps the value-shape probe, because that is the question C asks
/// there: `no_elements` is the destination's capacity, and a field this port
/// stores as a scalar has capacity 1. C's array arm with `nRequest > 1` into
/// such a field clamps to one element and writes element 0, which is what
/// reducing the array to [`EpicsValue::first_element`] produces.
fn dbput_request(
    record: &dyn crate::server::record::Record,
    field: &str,
    value: EpicsValue,
) -> CaResult<PutRequest> {
    use crate::server::record::FieldDeclaration;
    // C `no_elements` for this destination: the count the ARRAY arm clamps to.
    let dest_is_array = record.get_field(field).is_some_and(|v| v.is_array());
    if value.is_empty_array() {
        if !record.field_is_dbaddr(field) {
            return Ok(PutRequest::StoreNothing { alarm: true });
        }
        // Array arm, `nRequest == 0`. A field this port stores as a Vec models
        // `put_array_info(paddr, 0)` by storing the empty array — its NORD
        // follows the value — and falls through to the coercion below. A
        // `special(SPC_DBADDR)` field stored as a SCALAR (`mbbo.VAL`, an `aSub`
        // channel at `NOA == 1`) has no valid length to drop, so it keeps the
        // value C's zero-element copy left untouched.
        if !dest_is_array {
            return Ok(PutRequest::StoreNothing { alarm: false });
        }
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

    // Both remaining halves of the branch belong to `coerce_write_value`, and
    // this body must not re-derive either of them: the request is rendered in
    // the destination field's shape and then converted, in that order, by the
    // one `dbPut` entry (`record::dbput_coerce_value`).
    //
    // UNCONDITIONALLY. Skipping the converter when the request already carried
    // the destination's DBF is what let a scalar reach a buffer field
    // unrendered — the shape row lives inside the entry, so the type match says
    // nothing about whether there is work to do — and a String target had to be
    // named as an exception for the same reason (`putStringString` truncates to
    // `field_size - 1`, so it is not a no-op on a type match either). With no
    // gate there is no exception list to keep in step.
    match target {
        Some(target) => match coerce_write_value(record, field, target, value)? {
            crate::types::c_parse::Converted::Stored(v) => Ok(PutRequest::Write(v)),
            // C's `cvt_st_ul` returned success without storing: the same
            // "nothing stored, status still 0" arm the zero-element request
            // takes, and with no `recGblSetSevr` — the converter never reaches
            // `dbAccess.c:1371`.
            crate::types::c_parse::Converted::Unchanged => {
                Ok(PutRequest::StoreNothing { alarm: false })
            }
        },
        // Neither a current value nor a declaration says what this field takes,
        // so there is no destination to render or convert against.
        None => Ok(PutRequest::Write(value)),
    }
}

/// C `dbAccess.c:1409-1410` — `isValueField = dbIsValueField(pfldDes);
/// if (isValueField) precord->udf = FALSE;`.
///
/// The single owner of `dbPut`'s UDF clear, because in C there is one: the two
/// lines sit AFTER the value branch has joined, so the arm that converted zero
/// elements reaches them exactly as the arm that stored a value does. The only
/// thing that skips them is a non-zero `status` — the `goto done` at `:1404`,
/// which in this port is the early return out of `put_field` /
/// `special_after_put`.
///
/// It clears `udf` and NOTHING else: stat/sevr keep their old UDF_ALARM until
/// the record's own process cycle recomputes them (`rec_gbl_check_udf` no
/// longer raises it once udf is clear, `rec_gbl_reset_alarms` commits the new
/// state). A value put that drives no process therefore leaves the stale UDF
/// alarm standing, as C does.
fn clear_udf_on_value_put(instance: &mut crate::server::record::RecordInstance, field: &str) {
    if instance.record.is_udf_defining_put(field) {
        instance.common.udf = 0;
    }
}

/// Apply the SCALAR arm's [`PutRequest::StoreNothing`] alarm: the field is left
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
    /// C `dbNotify.c:213-224` restart: the client's receiver already exists,
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
    /// `restartCheck` (dbNotify.c:149-170).
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

/// The teardown owner for a blocking external client PUT — the PVA/QSRV
/// counterpart of C `rsrvFreePutNotify` (`camessage.c:1630-1638`), the CA
/// server's own client-teardown call into `dbNotifyCancel`.
///
/// # Invariant (CONTRACT)
///
/// A blocking external PUT whose awaiting future is dropped before the
/// completion arrives MUST release the record's put-notify slot before it
/// goes. Leaving that to the next put's arrival-time sweep is not enough: a
/// client already parked on `notify_restart_list` is not an arrival, so it
/// waits behind a completion that can never come.
///
/// pvxs has no separate hook to mirror because a blocking put IS the
/// operation's future there: the native PVA server spawns the PUT EXEC body
/// and stores its abort handle on the op (`server_native::tcp`,
/// `finish_exec_data_task`), so DESTROY_CHANNEL and connection teardown alike
/// drop `ChannelState::ops`, abort the task and drop this future mid-await.
/// The source's `notify_channel_close` is deliberately NOT the owner: an
/// abort only *marks* the task, and the future is dropped whenever the
/// executor next reaches it, so a sweep run from the close callback can find
/// the receiver still alive and release nothing.
///
/// The receiver is owned HERE and closed by hand because `Drop::drop` runs
/// BEFORE a struct's own fields drop — asking the database first would find
/// `Sender::is_closed()` still false and sweep nothing.
struct ClientAwaitingNotify<'a> {
    db: &'a PvDatabase,
    record: &'a str,
    rx: crate::runtime::sync::oneshot::Receiver<()>,
    /// Set once the await returns, which disarms the release: the completion
    /// path owns the slot from that point.
    answered: bool,
}

impl Drop for ClientAwaitingNotify<'_> {
    fn drop(&mut self) {
        if self.answered {
            return;
        }
        self.rx.close();
        self.db.cancel_unanswerable_notify(self.record);
    }
}

/// The one claim on a record's put-notify slot for the whole gate-held CA put
/// body, and the single finalizer every exit path out of it passes through.
///
/// C takes the ownership test (`if (precord->ppn ...)`, dbNotify.c:213) and
/// the install (`precord->ppn = ppn`, `:257`) inside one `dbScanLock` region
/// held across `putCallback`, and a record's whole link chain is in that same
/// lock set, so nothing can observe the interval between them. This port's L1
/// gate is per-RECORD, so `join_put_notify` — reached from ANOTHER record's
/// chain (`links.rs:1517`, C `dbNotifyAdd`) — does not take it and could slip
/// into that interval and take the slot. Claiming in the same critical section
/// as the test closes the window: the slot is occupied from the test onward,
/// which is why the "somebody took it while we were writing" arm the two
/// dispatched park sites used to carry is gone rather than tested.
///
/// **Release is the default.** The wait-set is on the record from the claim
/// onward — ahead of the DISP gate, the SPC_NOMOD gate, the conversion,
/// `special()`, and every `?` in them — so a path that abandons the put must
/// clear it or the record stays owned with no owner. `Drop` does that, and
/// [`NotifyClaim::commit`] is the only way to keep it: none of the early
/// returns carries a line of cleanup, and a new one cannot forget to.
#[must_use = "dropping the claim releases the record's put-notify slot"]
struct NotifyClaim<'a> {
    rec: &'a std::sync::Arc<parking_lot::RwLock<crate::server::record::RecordInstance>>,
    /// `None` once committed — the record processes under the set and owns its
    /// release from then on (`complete_put_notify`, C `dbNotifyCompletion`).
    set: Option<std::sync::Arc<crate::server::record::NotifyWaitSet>>,
}

impl NotifyClaim<'_> {
    /// The put reached its process cycle: hand the wait-set over and stop
    /// releasing it. From here the record's completion owns it
    /// (`complete_put_notify`, C `dbNotifyCompletion`).
    fn commit(mut self) -> std::sync::Arc<crate::server::record::NotifyWaitSet> {
        self.set
            .take()
            .expect("a claim is committed exactly once, by value")
    }

    /// Same commit, for the two paths that drive a process cycle and then hand
    /// the client an `Err` instead of the completion: a rejected PROC
    /// conversion and "Cause B", a rejected write on the notify route.
    ///
    /// Committing is not a courtesy here, it is C: `putCallback` returns
    /// `didPut = 1` even when the write failed (`dbNotify.c:528-530`), so
    /// `processNotifyCommon` reaches `doProcess`, assigns `precord->ppn = ppn`
    /// and processes (`:243-256`). The record therefore owns the set and its
    /// own completion releases it; releasing here as well would clear a slot
    /// the cycle is using.
    fn commit_without_waiting(self) {
        let _ = self.commit();
    }
}

impl Drop for NotifyClaim<'_> {
    fn drop(&mut self) {
        if let Some(set) = self.set.take() {
            self.rec.write().abandon_put_notify(&set);
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
    backing: crate::server::database::LinkBacking<'_>,
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
            backing,
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
fn dbput_post_put_field(
    instance: &mut crate::server::record::RecordInstance,
    field: &str,
    backing: crate::server::database::LinkBacking<'_>,
) {
    let suppress = field == instance.record.primary_field()
        && instance
            .record
            .process_passive_fields()
            .iter()
            .any(|f| f.eq_ignore_ascii_case(field));
    if !suppress {
        instance.cleanup_subscribers();
        instance.notify_field_backed(
            field,
            crate::server::recgbl::EventMask::VALUE | crate::server::recgbl::EventMask::LOG,
            backing,
        );
    }
}

/// Does `record_type` declare a field called `field`?
///
/// C's `dbFindFieldPart` searches `papFldDes`, which is every field the
/// `.dbd` declared including the spliced-in dbCommon and the `DBF_NOACCESS`
/// internals — exactly what `record_declaration_order` carries. A record type
/// the generated table does not know declares nothing, which is what C's
/// `!precordType` guard answers too.
fn declares_field(record_type: &str, field: &str) -> bool {
    crate::server::record::dbd_generated::record_declaration_order(record_type)
        .is_some_and(|names| names.contains(&field))
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
            // C `pvNameLookup` (`dbChannel.c:311-329`) resolves a field name
            // against the record type's DECLARED field list first and falls
            // through to `dbGetAttributePart` only on
            // `S_dbLib_fieldNotFound`. So a record type that declares a field
            // of the attribute's name — `motor.VERS` — shadows the attribute,
            // and `RTYP`, which no record type declares, never is shadowed.
            //
            // The two tests are in this order because the attribute map holds
            // two entries per type and the declared list holds hundreds: the
            // cheap lookup decides whether the expensive one is needed at all.
            let record_type = instance.record.record_type();
            if let Some(value) = self.record_type_attribute(record_type, &field)
                && !declares_field(record_type, &field)
            {
                return Ok(EpicsValue::String(value.into()));
            }
            if let Some(value) = instance.resolve_field(&field) {
                return Ok(value);
            }
            // Resolve-ok-but-read-fail is a state of its own, and one
            // `ChannelNotFound` cannot hold it. C `dbNameToAddr` resolves any
            // field the `.dbd` declares, `DBF_NOACCESS` ones included, so a
            // declared field's read reaches `dbGet` and fails THERE — its
            // validity gate refuses `field_type > DBF_DEVICE` with
            // `S_db_badDbrtype` — while an undeclared name never resolves at
            // all. `dbgf` shows the two apart: `dbgf REC.TIME` prints a type
            // header and then `failed.` (`dbTest.c:994-997`, reached only
            // because the address resolved), where `dbgf REC.NOSUCH` prints
            // "not found".
            //
            // The rule is the declaration, not the `DBF_NOACCESS` class: a
            // field this record type declares but does not serve is the same
            // state — present, unreadable — and C would likewise resolve it
            // and fail the get. Naming the class here instead would put a
            // second rule at the boundary.
            return Err(if declares_field(record_type, &field) {
                CaError::BadDbrType(format!(
                    "dbGet: {name} is declared but has no readable value"
                ))
            } else {
                CaError::ChannelNotFound(name.to_string())
            });
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
        self.put_pv_body(name, value)
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
    /// short-circuits them past the notify machinery (`dbNotify.c:337-354`),
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

    fn put_pv_body(&self, name: &str, value: EpicsValue) -> CaResult<()> {
        let (base, field) = super::parse_pv_name(name);
        let field = field.to_ascii_uppercase();
        // C `dbPutFieldLink` (`dbAccess.c:1261`): a write to a DBF link field
        // relinks the target's lock set. The obligation is taken out here, so
        // that every exit path below discharges it; see
        // `record_lock.rs`'s `LinkFieldWrite`.
        let _relink = self.link_field_write(base, &field);

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
            // (`record_lock.rs`, "Rust port").
            // Scoped guard: everything the put commits happens under this
            // write guard, which ends (releasing the `!Send` parking_lot
            // guard) before the tails below, which re-enter the database.
            // Yields the owned outputs those tails consume. Note this is the
            // record's DATA lock coming down, not the advisory gate above.
            use crate::server::record::CommonFieldPutResult;
            // C reads a link-backed field's metadata live inside the rset,
            // under the TARGET record's lock; every poster below holds THIS
            // record's lock and cannot reach for a second one. Resolved here,
            // where no record lock is held, and handed down as a borrowed
            // value that dies with this body — so a `caput CALC.A` carries the
            // metadata `INPA`'s target has NOW, not what it had when the
            // source last processed. Empty after one read lock for every
            // record type but calc, calcout, sub, aSub and seq.
            let link_backing = self.resolve_link_backed_metadata(&rec);
            let link_backing = crate::server::database::LinkBacking::resolved(&link_backing);
            let (common_result, special_actions) = {
                let mut instance = rec.write();

                // C `dbPut` refuses an SPC_NOMOD / SPC_ATTRIBUTE field before it
                // converts anything (`dbAccess.c:1330-1332`). `put_pv` IS the
                // `dbPut` analogue — it sits below `dbPutLink`, so this is what
                // stops a record's OUT link from truncating a waveform's NELM.
                // The refusal is returned to the caller; `write_out_link_value`
                // (C `dbPutLink`) turns it into the writer's LINK/INVALID alarm.
                check_no_mod_in_db_put(&instance, &field)?;

                // C `dbPut` refuses a link-field target the same way, before
                // conversion (`dbAccess.c:1340`) — see `check_not_link_field`.
                check_not_link_field(&instance, &field)?;

                let request = dbput_request(&*instance.record, &field, value)?;

                // Pre-write special hook (C EPICS dbPutSpecial pass=0).
                // C `dbPut` runs it on EVERY entry path — dbPutField and
                // dbPutLink alike (dbAccess.c) — so the OUT-link route
                // through this body must call it too (motor's drive-field
                // DMOV blink, motorRecord.cc:2591-2620, fires on put-links
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
                    // C `dbAccess.c:1362` converted zero elements — nothing is
                    // stored and the put is accepted. `alarm` is the scalar arm's
                    // `:1371` only.
                    PutRequest::StoreNothing { alarm } => {
                        if alarm {
                            set_empty_request_alarm(&mut instance);
                        }
                        clear_udf_on_value_put(&mut instance, &field);
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
                                let result = special_after_put(
                                    self,
                                    &mut instance,
                                    &field,
                                    &mut special_actions,
                                    link_backing,
                                )?;
                                clear_udf_on_value_put(&mut instance, &field);
                                result
                            }
                            Err(CaError::FieldNotFound(_)) => {
                                instance.put_common_field(&field, value)?
                            }
                            // A refused store does NOT skip the pass above:
                            // C runs it either way (`dbAccess.c:1398`) and lets
                            // its status win, then `goto done` — the `return`.
                            Err(e) => {
                                special_after_put(
                                    self,
                                    &mut instance,
                                    &field,
                                    &mut special_actions,
                                    link_backing,
                                )?;
                                return Err(e);
                            }
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
                instance.notify_field_written_if_changed(&field, prev_value.as_ref(), link_backing);

                // C `dbPut:1408-1413`'s field-monitor post, through the one owner
                // (`dbput_post_put_field`, shared with the CA route). `put_pv`
                // is the `dbPutLink` route's `dbPut` and the internal driver-put
                // entry, and C posts DBE_VALUE|DBE_LOG from *every* `dbPut` —
                // an NPP OUT link writing a calc's A, an autosave restore, a
                // status pusher, all post immediately. Pre-fix this body posted
                // nothing at all, so camonitor on anything written via `put_pv`
                // went silent forever.
                dbput_post_put_field(&mut instance, &field, link_backing);

                // C's `put_array_info` is likewise reached from every `dbPut` —
                // an OUT link that shortens a waveform posts NORD in C even when
                // the link is NPP and the target never processes, and the
                // value-field post above is suppressed for a waveform (VAL is
                // `pp(TRUE)`); NORD has no such second path.
                post_array_info(&mut instance, &old_nord, 0, link_backing);

                (common_result, special_actions)
            };
            // No put-notify is armed here: C's restart owner is the cycle
            // tail (`end_process_cycle`), never the `pact = FALSE` store.
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
        // C `dbPutFieldLink` (`dbAccess.c:1261`): a write to a DBF link field
        // relinks the target's lock set. The obligation is taken out here, so
        // that every exit path below discharges it; see
        // `record_lock.rs`'s `LinkFieldWrite`.
        let _relink = self.link_field_write(base, &field);

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
            // move, so both tails below stay inside the window.
            let canonical_base: String =
                self.resolve_alias(base).unwrap_or_else(|| base.to_string());
            let _record_gate = self.lock_record(&canonical_base);

            // Guarded: the value write + monitor post. The record's DATA guard
            // is released at the block close before the tails below, which
            // re-enter the database (`parking_lot` guards are `!Send`); the
            // advisory `_record_gate` still holds the processing-exclusion
            // window across the whole helper.
            use crate::server::record::CommonFieldPutResult;
            // C reads a link-backed field's metadata live inside the rset,
            // under the TARGET record's lock; every poster below holds THIS
            // record's lock and cannot reach for a second one. Resolved here,
            // where no record lock is held, and handed down as a borrowed
            // value that dies with this body — so a `caput CALC.A` carries the
            // metadata `INPA`'s target has NOW, not what it had when the
            // source last processed. Empty after one read lock for every
            // record type but calc, calcout, sub, aSub and seq.
            let link_backing = self.resolve_link_backed_metadata(&rec);
            let link_backing = crate::server::database::LinkBacking::resolved(&link_backing);
            let (common_result, special_actions) = {
                let mut instance = rec.write();

                // Same `dbPut` gate as `put_pv` — this is the third `dbPut` body
                // (value + monitor post), and C has ONE.
                check_no_mod_in_db_put(&instance, &field)?;
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
                    // C `dbAccess.c:1362` converted zero elements — nothing is
                    // stored and the put is accepted. `alarm` is the scalar arm's
                    // `:1371` only.
                    PutRequest::StoreNothing { alarm } => {
                        if alarm {
                            set_empty_request_alarm(&mut instance);
                        }
                        clear_udf_on_value_put(&mut instance, &field);
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
                                let result = special_after_put(
                                    self,
                                    &mut instance,
                                    &field,
                                    &mut special_actions,
                                    link_backing,
                                )?;
                                clear_udf_on_value_put(&mut instance, &field);
                                result
                            }
                            Err(CaError::FieldNotFound(_)) => {
                                instance.put_common_field(&field, value)?
                            }
                            // A refused store does NOT skip the pass above:
                            // C runs it either way (`dbAccess.c:1398`) and lets
                            // its status win, then `goto done` — the `return`.
                            Err(e) => {
                                special_after_put(
                                    self,
                                    &mut instance,
                                    &field,
                                    &mut special_actions,
                                    link_backing,
                                )?;
                                return Err(e);
                            }
                        }
                    }
                };

                // Invalidate metadata cache only if a metadata-class
                // field actually changed value (faac1df1 — DBE_PROPERTY
                // fires on real changes, not no-op writes).
                instance.notify_field_written_if_changed(&field, old_value.as_ref(), link_backing);

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
                            link_backing,
                        );
                    }
                    // The NORD post, through the one owner. Without it a CA
                    // gateway forwarding upstream waveform monitors via
                    // put_pv_and_post would update VAL on the shadow PV but
                    // leave downstream NORD subscribers stuck at their last
                    // seen length — a frozen-element-count bug that surfaces
                    // in PyDM image views and similar consumers that compute
                    // height = element_count / width.
                    post_array_info(&mut instance, &old_nord, origin, link_backing);
                }

                // The `special()` link writes re-enter the database, so the record
                // lock goes down first (the block close releases it). C makes them
                // inside `dbPut`, before it returns to its caller.
                (common_result, special_actions)
            };
            // No put-notify is armed here: C's restart owner is the cycle
            // tail (`end_process_cycle`), never the `pact = FALSE` store.

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
    /// finding.
    ///
    /// The one call that used to make it a genuine suspension is gone:
    /// `write_out_link_value` → `write_external_pv` does not call
    /// `LinkSet::put_value` from this thread. It stages the write on the
    /// database's link-put queue and returns, exactly as C `dbCaPutLink`
    /// stages into `pca->pputNative`, calls `addAction` and returns
    /// (`dbCa.c:515-602`); the `ca://` / `pva://` round trip runs on the
    /// queue's owner task, C's `dbCaTask` (`dbCa.c:1161-1183`). See
    /// [`super::link_put_queue`]. What is left inside the window is the
    /// re-entrant database work C also does under `dbScanLock`:
    /// `execute_process_actions` → `write_out_link_value` →
    /// `write_db_link_value` re-entering `put_pv_already_locked` and
    /// `process_target`, plus the cached-state `LinkSet::put_admission` probe
    /// both production lsets answer from a map lookup and an atomic — C's
    /// `if (!pca->isConnected …)` (`dbCa.c:529-532`).
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
    /// `dbNotify.c:337-354`), and the `dbPut`-analogue bodies refuse link
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
                    self.await_completion(record_name, completion).await;
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
                    self.await_completion(record_name, completion).await;
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

    /// Await a blocking external PUT's completion under
    /// [`ClientAwaitingNotify`], so a client that goes away mid-put hands the
    /// record back instead of wedging every put queued behind it.
    async fn await_completion(
        &self,
        record_name: &str,
        completion: crate::server::record::ProcessCompletion,
    ) {
        let crate::server::record::ProcessCompletion::Async(rx) = completion else {
            return;
        };
        let mut pending = ClientAwaitingNotify {
            db: self,
            record: record_name,
            rx,
            answered: false,
        };
        // Either outcome ends the wait. `Err` means the sender was dropped
        // without sending, and only the completion path does that — it takes
        // the sender out of the set first, so the sweep would no-op anyway.
        let _ = (&mut pending.rx).await;
        pending.answered = true;
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

        // Resolved before the record lock: the record-wide `DBE_ALARM` post
        // below reaches every subscribed field, a link-backed one included.
        let backing = self.resolve_link_backed_metadata(&rec);
        let backing = crate::server::database::LinkBacking::resolved(&backing);

        let mut instance = rec.write();
        check_put_disabled(&instance, &field_upper)?;
        match ack {
            crate::server::record::AlarmAck::Transient => instance.put_ackt(value, backing),
            crate::server::record::AlarmAck::Severity => instance.put_acks(value, backing),
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
    /// applies the same PACT→RPRO rule at its own targets
    /// (`processing.rs:4781-4790`, `:5783-5792`, `links.rs:1519`).
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
        //
        // Resolved, not raw: C addresses a notify by `dbCommon *`, so an alias
        // is the same record (`dbNotify.c:492-499` and the park test at
        // `:225-232` compare record pointers). The map is keyed by the
        // canonical name, and the caller's `acquire_put_gate` already locked
        // THAT name -- a raw lookup here missed the record whose gate it was
        // standing behind.
        let rec_arc = self
            .get_record(record_name)
            .ok_or_else(|| CaError::ChannelNotFound(record_name.to_string()))?;
        self.cancel_unanswerable_notify(record_name);
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

    /// C `dbNotifyCancel` (`dbNotify.c:385-430`) as `rsrvFreePutNotify` reaches
    /// it (`camessage.c:1630-1638`): a put-notify whose client is gone leaves
    /// EVERY record it owns, and each record's restart-list head takes it
    /// (`restartCheck`).
    ///
    /// # Invariant (CONTRACT)
    ///
    /// A wait-set that can never answer MUST NOT leave any record's put-notify
    /// slot occupied. **This is the single owner of that release.** Without it
    /// a record type that legitimately withholds its `ca_put_callback` (`busy`
    /// at VAL=1, `mca` mid-acquisition) is wedged by the first client that
    /// gives up: the slot stays taken forever and every later put queues behind
    /// it unwritten.
    ///
    /// The sweep is over the set's own membership
    /// ([`NotifyWaitSet::joined_records`]), not over the entry record, because
    /// C's is (`dbNotify.c:428-430` empties the whole wait list, and only then
    /// does `:433` deal with the entry). An entry-only release left the same
    /// wedge one hop down the chain, on the record most likely to be holding a
    /// set it will never leave: an FLNK target that is itself a `busy` at
    /// VAL=1 declines its own `recGblFwdLink` by contract, so its membership
    /// outlives the client that started the chain.
    ///
    /// Takes no lock of its own on entry, and holds only one record's write
    /// lock at a time, so it can be called from a client-teardown `Drop` and
    /// cannot deadlock against a chain that locks records in the other order.
    ///
    /// [`NotifyWaitSet::joined_records`]: crate::server::record::NotifyWaitSet
    pub fn cancel_unanswerable_notify(&self, record_name: &str) {
        let Some(rec) = self.get_record(record_name) else {
            return;
        };
        let Some(dead) = rec.read().unanswerable_notify() else {
            return;
        };
        // C `dbNotifyCancel`: the whole wait list, then the entry. `joined`
        // already carries the entry, so one pass covers both arms.
        for name in dead.joined_records() {
            let Some(member) = self.get_record(&name) else {
                continue;
            };
            let exit = {
                let mut guard = member.write();
                if !guard.release_notify(&dead) {
                    continue;
                }
                guard.pact_exit_without_release()
            };
            // `apply_pact_exit` takes no record lock, by construction — the
            // drain it spawns takes the record's write gate itself.
            self.apply_pact_exit(&name, &member, exit);
        }
    }

    /// Claim `record_name`'s put-notify slot for this put, in the SAME
    /// critical section that just tested it.
    ///
    /// The caller has already established that the record is free — the whole
    /// point is that the test and the claim are one critical section, so this
    /// takes the guard it tested under rather than re-reading the record.
    fn claim_put_notify<'a>(
        rec: &'a std::sync::Arc<parking_lot::RwLock<crate::server::record::RecordInstance>>,
        guard: &mut crate::server::record::RecordInstance,
        completion: crate::runtime::sync::oneshot::Sender<()>,
    ) -> NotifyClaim<'a> {
        // Through the one install owner, which is also what makes the
        // `expect` safe: it queues instead of installing only when the slot is
        // occupied, and the caller's test — under this same guard — said it is
        // not.
        let set = guard
            .install_or_queue_notify(completion)
            .expect("the ownership test ran in this critical section, so the slot is free");
        NotifyClaim {
            rec,
            set: Some(set),
        }
    }

    /// The gate-held body of the whole CA field-put family — a `fn`, so
    /// nothing in it can suspend while the L1 gate is held. The gate is the
    /// caller's; see `acquire_put_gate`.
    fn put_record_field_from_ca_body(
        &self,
        record_name: &str,
        field: &str,
        mut value: EpicsValue,
        notify_request: NotifyRequest,
    ) -> CaResult<crate::server::record::ProcessCompletion> {
        let field = field.to_ascii_uppercase();
        let want_notify = notify_request.wants_notify();
        // C `dbPutFieldLink` (`dbAccess.c:1261`): a write to a DBF link field
        // relinks the target's lock set. The obligation is taken out here, so
        // that every exit path below discharges it; see
        // `record_lock.rs`'s `LinkFieldWrite`.
        let _relink = self.link_field_write(record_name, &field);

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

            // C `dbPutField` hands a DBF link field to `dbPutFieldLink`
            // instead of `dbPut` (`dbAccess.c:1259-1260`), which parses the
            // text and holds it to `dbCanSetLink` — see [`check_link_put`].
            // ABOVE the no-mod gate because that is where C puts it: the link
            // route never enters `dbPut`, and the `SPC_NOMOD` refusal a link
            // field can still take comes from `dbPutSpecial(paddr, 0)`
            // (`dbAccess.c:1174` -> `:124`), which runs AFTER `dbCanSetLink`.
            if let Some(text) = check_link_put(
                instance.record.record_type(),
                instance.common.dtyp.as_str(),
                &field,
                &value,
            )? {
                value = text;
            }

            // SPC_NOMOD / read-only fields: rejected inside C's `dbPut`, i.e.
            // after the DISP gate above and before the PROC-driven process
            // below. One gate owner for every route
            // ([`check_no_mod_in_db_put`]).
            check_no_mod_in_db_put(&instance, &field)?;
        }

        // C `processNotifyCommon` (dbNotify.c:225-232) tests PACT ABOVE the
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
        // "another processNotify owns the record" (dbNotify.c:213-220): it joins
        // the SAME queue, at the back (`ellSafeAdd`). Both tests are one arm
        // here because both have the same answer — the put waits, unwritten.
        // Refusing it instead (`S_db_Blocked` / `ECA_PUTCBINPROG`) drops the
        // client's value, and C never sends that status from this path.
        //
        // A fire-and-forget `dbPutField` is NOT deferred: it writes and raises
        // RPRO (dbAccess.c:1263-1277). Only the notify route waits.
        //
        // C `dbProcessNotify` (dbNotify.c:337-354) handles a put-notify to a
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
        // The claim on this record's put-notify slot, held for the rest of the
        // body. `None` for a fire-and-forget put, which parks nothing — C
        // builds a `putNotify` only in `dbPutNotify`. See [`NotifyClaim`] for
        // why the claim is taken HERE, in the same critical section as the
        // ownership test below, and not at the process cycle it arms.
        let mut claim: Option<(
            NotifyClaim<'_>,
            Option<crate::runtime::sync::oneshot::Receiver<()>>,
        )> = None;
        if want_notify {
            let is_restart = notify_request.is_restart();
            self.cancel_unanswerable_notify(record_name);
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
                // a refusal, deleted since along with its `CaError` variant.
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
                // `notifyWaitForRestart`, dbNotify.c:213-220, 225-232). `Deferred` replays
                // carry only the sender, so `completion_rx` is `None` there and
                // this maps to `Sync` — but that path is the internal restart,
                // not a fresh client put.
                return Ok(crate::server::record::ProcessCompletion::from_signal(
                    completion_rx,
                ));
            }
            // Not deferred, so this put owns the record — take the slot now,
            // under the guard that just said it is free. Everything below runs
            // with the wait-set installed, and every way out of this function
            // that is not a process cycle drops the claim.
            if let Some((completion, completion_rx)) = notify_request.into_completion() {
                claim = Some((
                    Self::claim_put_notify(&rec, &mut guard, completion),
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
            // C `dbPut:1413` posts DBE_VALUE|DBE_LOG for the put field (PROC is
            // not the record's value field, so the pp-suppression never applies).
            // Store the raw PROC byte (C `dbChannelPut`). A bad conversion
            // (`caput REC.PROC 256` / non-numeric) refuses the store AND the
            // client's put — but, exactly as C's `putCallback` returns
            // `didPut = 1` while setting `notifyError` (`dbNotify.c:528-530`),
            // the PROC `pp(TRUE)`-driven `dbProcess` (`dbNotify.c:243-264`) still
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
                    // The cycle runs under this put's wait-set, so the claim is
                    // committed before it starts — see `commit_without_waiting`.
                    if let Some((c, _rx)) = claim.take() {
                        c.commit_without_waiting();
                    }
                    let _ = self.put_driven_process_already_locked(record_name);
                }
                return Err(e);
            }
            // A fire-and-forget caller parks nothing — C `dbPutField` on PROC
            // processes the record with no putNotify. A notify caller commits
            // the claim it has held since the entry gate; there is no third
            // answer to reach, because nothing could have taken the slot from
            // under it.
            let parked = claim.take().map(|(c, rx)| (c.commit(), rx));
            // C `dbPutField:1264-1277`: PROC is one of the two fields that
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
        // C reads a link-backed field's metadata live inside the rset,
        // under the TARGET record's lock; every poster below holds THIS
        // record's lock and cannot reach for a second one. Resolved here,
        // where no record lock is held, and handed down as a borrowed
        // value that dies with this body — so a `caput CALC.A` carries the
        // metadata `INPA`'s target has NOW, not what it had when the
        // source last processed. Empty after one read lock for every
        // record type but calc, calcout, sub, aSub and seq.
        let link_backing = self.resolve_link_backed_metadata(&rec);
        let link_backing = crate::server::database::LinkBacking::resolved(&link_backing);
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
                special_before_put(&mut instance, &field);

                // Capture pre-put value for faac1df1 idempotent-write suppression.
                let prev_value = instance.record.get_field(&field);
                let old_nord = array_nord_before_put(&instance, &field);

                // Try record-specific field first; fall back to common on FieldNotFound.
                // For record-owned fields, call on_put() and special() after successful put,
                // matching what put_common_field() does for common fields.
                use crate::server::record::CommonFieldPutResult;
                let common_result = match request {
                    // C `dbAccess.c:1362` converted zero elements: nothing is
                    // written and `dbPut` returns 0, so the client's put SUCCEEDS.
                    // On the SCALAR arm it also drives the record to LINK/INVALID
                    // (`:1371`) and the process cycle below commits and posts that
                    // alarm — which is how a C IOC surfaces `caput -a` of an empty
                    // array into a scalar. The ARRAY arm raises nothing.
                    PutRequest::StoreNothing { alarm } => {
                        if alarm {
                            set_empty_request_alarm(&mut instance);
                        }
                        clear_udf_on_value_put(&mut instance, &field);
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
                                    link_backing,
                                )?;
                                // The clear happens BEFORE `dbProcess` runs, so
                                // any reader between the put and the process cycle
                                // sees the new value with a consistent udf=false.
                                clear_udf_on_value_put(&mut instance, &field);
                                result
                            }
                            Err(CaError::FieldNotFound(_)) => {
                                instance.put_common_field(&field, value)?
                            }
                            // A refused store does NOT skip the pass above:
                            // C runs it either way (`dbAccess.c:1398`) and lets
                            // its status win, then `goto done` — the `return`.
                            Err(e) => {
                                special_after_put(
                                    self,
                                    &mut instance,
                                    &field,
                                    &mut special_actions,
                                    link_backing,
                                )?;
                                return Err(e);
                            }
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
                instance.notify_field_written_if_changed(&field, prev_value.as_ref(), link_backing);

                // `putf` is neither set nor cleared anywhere in this block: C's
                // `dbPut` does not touch it. It is raised in `put_driven_process`
                // (C `dbAccess.c:1275`) immediately before `dbProcess`, stays TRUE
                // for the whole process cycle — including an async device round
                // trip — and is cleared by the `recGblFwdLink:302` analogue at the
                // cycle's tail (`processing.rs:5346` / `complete_async_record_inner`)
                // or by the disable-alarm bail (`dbAccess.c:575`).

                // C `dbPut:1408-1413`'s field-monitor post, through the one
                // owner (`dbput_post_put_field`, shared with `put_pv`). On
                // this route the pp-value-field suppression pairs with the
                // `should_process` gate below: a suppressed field is exactly
                // one the reprocess cycle re-posts via the deadband snapshot.
                // (ACKT/ACKS have no arm here: they are SPC_NOMOD, refused by
                // the gate above. Alarm acknowledgement arrives as a DBR
                // request type, through [`Self::put_alarm_ack_from_ca`].)
                dbput_post_put_field(&mut instance, &field, link_backing);

                // The NORD post, through the one owner — C reaches `put_array_info`
                // from `dbPut`, so the CA route posts it exactly like the internal
                // one. It is NOT covered by the value-field post above: for a
                // waveform that post is suppressed (VAL is `pp(TRUE)`), and it is
                // not covered by the process cycle either — a `caput -a` to a
                // slow-scanned or passive-but-unprocessed waveform posts NORD now
                // and VAL only at the next scan.
                post_array_info(&mut instance, &old_nord, 0, link_backing);

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
                emit_cycle_posts(&mut instance, link_backing);

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
                    // C `dbAccess.c:1398-1403` runs `dbPutSpecial(paddr, 1)` UNCONDITIONALLY
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
                    // that special runs UNCONDITIONALLY in `dbPut` (dbAccess.c:1398-1400,
                    // "Always do special processing") even when the value conversion
                    // failed — BEFORE the `if (status) goto done`. So a rejected
                    // `caput -c mbboDirect.Bn 256`/`notanumber` still clears UDF, and
                    // the notify-process that follows recomputes STAT/SEVR to
                    // NO_ALARM instead of the born-UDF INVALID (verified live against
                    // the C softIoc: fresh record → rejected Bn put → NO_ALARM,
                    // udf=0). The success path clears UDF for this same field set via
                    // `is_udf_defining_put` (the `udf = 0` at the tail of the put
                    // body). The primary VAL field is EXCLUDED here: its UDF clear is
                    // `isValueField` (dbAccess.c:1409-1410), which runs AFTER the status
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
        // No put-notify is armed here: C's restart owner is the cycle tail
        // (`end_process_cycle`), never the `pact = FALSE` store.

        let common_result = match outcome {
            Ok(cr) => cr,
            Err((e, should_process)) => {
                if should_process {
                    // Same rule as the PROC refusal above: the cycle runs under
                    // the wait-set, so commit before driving it.
                    if let Some((c, _rx)) = claim.take() {
                        c.commit_without_waiting();
                    }
                    let _ = self.put_driven_process_already_locked(record_name);
                }
                return Err(e);
            }
        };
        // ASG-field change re-evaluation hook. C
        // `asDbLib.c:107-110` `asSpcAsCallback` (registered at `:144`)
        // invokes `asChangeGroup` → `asAddMemberPvt` → `asComputePvt` for
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
        // record" with `ellSafeAdd` onto `restartList` (dbNotify.c:213-220)
        // and carries no refusal arm: `S_db_Blocked` is raised by a test
        // record's own put hook (test/ioc/db/xRecord.c:89), never by the
        // notify machinery, and `ECA_PUTCBINPROG` has exactly one sender in
        // all of base — the put-callback timeout in `write_notify_action`
        // (rsrv/camessage.c:1701 at R7.0.10).
        // Overwriting the slot instead would drop the prior Sender, waking
        // the prior caller's rx with RecvError that the CA dispatcher treats
        // as success, so the install goes through the one owner.
        //
        // A fire-and-forget put parks NOTHING — C builds a `putNotify`
        // only in `dbPutNotify`; `dbPutField` processes the record with
        // no notify state at all. It therefore neither conflicts with
        // nor disturbs a WRITE_NOTIFY already parked on the record.
        let parked = claim.take().map(|(c, rx)| (c.commit(), rx));

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
        // `dbPutField:1268-1277` decision, so an async-active record takes the
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
    pub async fn put_pv_no_process(&self, name: &str, mut value: EpicsValue) -> CaResult<()> {
        let (base, field) = super::parse_pv_name(name);
        let field = field.to_ascii_uppercase();
        // C `dbPutFieldLink` (`dbAccess.c:1261`): a write to a DBF link field
        // relinks the target's lock set. The obligation is taken out here, so
        // that every exit path below discharges it; see
        // `record_lock.rs`'s `LinkFieldWrite`.
        let _relink = self.link_field_write(base, &field);

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

            // C reads a link-backed field's metadata live inside the rset,
            // under the TARGET record's lock; every poster below holds THIS
            // record's lock and cannot reach for a second one. Resolved here,
            // where no record lock is held, and handed down as a borrowed
            // value that dies with this body — so a `caput CALC.A` carries the
            // metadata `INPA`'s target has NOW, not what it had when the
            // source last processed. Empty after one read lock for every
            // record type but calc, calcout, sub, aSub and seq.
            let link_backing = self.resolve_link_backed_metadata(&rec);
            let link_backing = crate::server::database::LinkBacking::resolved(&link_backing);
            let mut special_actions = Vec::new();

            let common_result = {
                let mut instance = rec.write();

                // The SPC_NOMOD half of that same pass-0 call, refused before
                // the rset's `special()` is ever dispatched
                // (`dbPutSpecial`, `dbAccess.c:122-127`). `dbPutField` — the
                // call this body models — reaches it like every other entry,
                // through `dbPut` (`dbAccess.c:1265`), so an autosave restore
                // cannot write a field the `.dbd` declares immutable. This
                // route was the one dbPut-analogue body that skipped the gate,
                // which left asynRecord's twelve `special(SPC_NOMOD)` fields
                // (OMAX, AINP, TINP, IMAX, NORD, EOMR, I32INP, UI32INP, F64INP,
                // SPR, OPTR, IPTR) restorable, and dbCommon's with them.
                // An autosave restore is a `dbPutField` (`reboot_restore`), so
                // a saved link text is held to `dbCanSetLink` exactly as a
                // `caput` is — that arm of `dbStaticLib.c:2634-2642` is the
                // one autosave takes. Above the no-mod gate for the reason
                // given at the other body: C's link route is above `dbPut`.
                if let Some(text) = check_link_put(
                    instance.record.record_type(),
                    instance.common.dtyp.as_str(),
                    &field,
                    &value,
                )? {
                    value = text;
                }
                check_no_mod_in_db_put(&instance, &field)?;
                // C `dbPutSpecial(paddr, 0)` — the pre-store pass, which
                // `dbPut` runs on every entry path including the `dbPutField`
                // this body models (autosave's `reboot_restore`). A non-zero
                // status returns before the store (`dbAccess.c:1345-1348`), so a
                // restore cannot write a field the record is currently refusing
                // (mbboDirect B0..B1F while OMSL=closed_loop,
                // `mbboDirectRecord.c:263-269`).
                instance.record.special(&field, false)?;
                special_before_put(&mut instance, &field);

                let prev_value = instance.record.get_field(&field);
                // C's `reboot_restore` writes through `dbPutField` → `dbPut`,
                // which renders the request in the destination field's shape
                // AND THEN runs the `dbPutConvertRoutine` type row
                // (`dbAccess.c:1350-1391`) — a saved one-element `histogram`
                // VAL is a buffer here, not a scalar the record's arm refuses,
                // and a saved `DBF_STRING` reaches a menu/enum/numeric field
                // through `putStringMenu`/`Enum`/`<numeric>` rather than the
                // field-blind `convert_to` that stored `0` for a label and
                // `32767` for a refused `PREC 32768`. A field the record
                // SERVES routes through the same single owner the client
                // `dbPut` path uses (`dbput_coerce_value` = shape THEN type);
                // a `dbCommon` field the record does not serve has no served
                // type to convert against, so it keeps the shape-only reshape
                // its `put_common_field` fallback needs.
                //
                // A refused store is HELD, not returned: C runs the after pass
                // below either way (`dbAccess.c:1398`) and lets its status win.
                let refused = match instance.record.get_field(&field).map(|v| v.db_field_type()) {
                    Some(target) => match crate::server::record::dbput_coerce_value(
                        &*instance.record,
                        &field,
                        target,
                        value,
                    ) {
                        Ok(crate::types::c_parse::Converted::Stored(v)) => {
                            match instance.record.put_field(&field, v.clone()) {
                                Ok(()) => None,
                                Err(CaError::FieldNotFound(_)) => {
                                    instance.put_common_field(&field, v)?;
                                    None
                                }
                                Err(e) => Some(e),
                            }
                        }
                        // C's converter returned success without storing
                        // (`cvt_st_ul`'s skipped store): the field keeps its
                        // old value and the restore still succeeds.
                        Ok(crate::types::c_parse::Converted::Unchanged) => None,
                        // A convert failure is HELD like a refused store, so the
                        // unconditional after pass still runs before it returns.
                        Err(e) => Some(e),
                    },
                    None => {
                        let value = crate::server::record::put_value_in_field_shape(
                            &*instance.record,
                            &field,
                            value,
                        );
                        match instance.record.put_field(&field, value.clone()) {
                            Ok(()) => None,
                            Err(CaError::FieldNotFound(_)) => {
                                instance.put_common_field(&field, value)?;
                                None
                            }
                            Err(e) => Some(e),
                        }
                    }
                };

                // C `dbPutSpecial(paddr, 1)` — the after-store pass, which
                // `dbPut` runs UNCONDITIONALLY ("Always do special processing
                // if needed", `dbAccess.c:1398-1404`) and whose status it
                // ADOPTS (`if (status2) status = status2;`). It is the pass
                // that re-derives the field's DEPENDENT state: `calcRecord.c`
                // recompiles RPCL from CALC, `subRecord.c:188` /
                // `aSubRecord.c:563-575` re-resolve SNAM through the registry,
                // SIMM runs `recGblCheckSimm`. Running only pass 0 left a
                // restored CALC evaluating the old expression and a restored
                // SNAM running the old routine, with the restore reporting
                // success — the link route C takes for a DBF link field
                // (`dbPutFieldLink`, `dbAccess.c:1174,1178`) runs the same pair.
                let result = special_after_put(
                    self,
                    &mut instance,
                    &field,
                    &mut special_actions,
                    link_backing,
                )?;

                // `if (status) goto done` (`dbAccess.c:1404`) — the refused
                // store's status stands where the pass above did not replace
                // it, and skips the UDF clear and the posts below.
                if let Some(e) = refused {
                    return Err(e);
                }

                // An autosave restore of VAL defines the record in C, and a
                // record left UDF here reports the born UDF_ALARM on its first
                // process cycle.
                clear_udf_on_value_put(&mut instance, &field);
                // Invalidate metadata cache only if the metadata-class
                // field actually changed (faac1df1).
                instance.notify_field_written_if_changed(&field, prev_value.as_ref(), link_backing);
                // The drain of the posts the after pass queued — `special()`
                // and its post are ONE step, so a mark cannot outlive the put
                // that made it and be emitted by some later cycle.
                emit_cycle_posts(&mut instance, link_backing);
                result
            };

            // No put-notify is armed here: C's restart owner is the cycle
            // tail (`end_process_cycle`), never the `pact = FALSE` store.

            // A restored SCAN or PHAS moves the record between scan lists.
            match common_result {
                crate::server::record::CommonFieldPutResult::ScanChanged {
                    old_scan,
                    new_scan,
                    phas,
                } => {
                    self.update_scan_index(&canonical_base, old_scan, new_scan, phas, phas);
                }
                crate::server::record::CommonFieldPutResult::PhasChanged {
                    scan: s,
                    old_phas,
                    new_phas,
                } => {
                    self.update_scan_index(&canonical_base, s, s, old_phas, new_phas);
                }
                crate::server::record::CommonFieldPutResult::NoChange => {}
            }

            // The link writes the after pass queued, run once the record lock
            // is down — C reaches them from inside `dbPut`.
            self.run_special_actions(&canonical_base, &rec, special_actions);

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

    /// A read of a DECLARED field that has no value is a read FAILURE, not a
    /// missing channel — and the two must leave `get_pv` by different doors.
    ///
    /// Measured against softIoc R7.0.10. `dbgf T:AI.TIME` and
    /// `dbgf T:AI.BKPT` resolve and then fail: C prints
    /// `recGblDbaddrError: … Illegal Database Request Type PV: T:AI.TIME` and
    /// a `failed.` line, because `dbNameToAddr` resolves every declared field
    /// — `dbCommon.dbd.pod:543-548` and `:564-569` declare `BKPT` and `TIME`
    /// as `DBF_NOACCESS` — and `dbGet`'s validity gate then refuses
    /// `field_type > DBF_DEVICE` with `S_db_badDbrtype`. `dbgf T:AI.NOSUCH`
    /// prints `PV 'T:AI.NOSUCH' not found` instead: nothing resolved.
    ///
    /// The port answered `ChannelNotFound` to all three, so `dbgf REC.TIME`
    /// was indistinguishable from a typo. Note this holds today WITHOUT a
    /// `FieldDesc` for `TIME`/`BKPT` — `record_declaration_order` already
    /// lists them, which is what `declares_field` reads.
    #[epics_macros_rs::epics_test]
    async fn a_declared_field_with_no_value_reads_as_a_failure_not_a_missing_channel() {
        use crate::error::CaError;
        use crate::server::records::ai::AiRecord;

        let db = PvDatabase::new();
        db.add_record("T:AI", Box::new(AiRecord::new(1.5)))
            .await
            .unwrap();

        for field in ["TIME", "BKPT"] {
            let name = format!("T:AI.{field}");
            match db.get_pv(&name) {
                Err(CaError::BadDbrType(msg)) => assert!(
                    msg.contains(&name),
                    "the status must name the field, got {msg:?}"
                ),
                other => panic!(
                    "{name} is declared and unreadable, so it must fail the READ; got {other:?}"
                ),
            }
        }

        // Undeclared field, and a record that does not exist: both are
        // genuinely absent and keep C's `dbNameToAddr` answer.
        for name in ["T:AI.NOSUCH", "T:NOSUCHREC.VAL"] {
            assert!(
                matches!(db.get_pv(name), Err(CaError::ChannelNotFound(_))),
                "{name} resolves to nothing and must stay not-found, got {:?}",
                db.get_pv(name)
            );
        }

        // The readable controls are untouched, including the `DBF_UINT64`
        // `UTAG` that sits between `TIME` and `BKPT` in `dbCommon` and reads
        // fine in both.
        assert_eq!(db.get_pv("T:AI.VAL").unwrap(), EpicsValue::Double(1.5));
        assert_eq!(db.get_pv("T:AI.UTAG").unwrap(), EpicsValue::UInt64(0));
        assert_eq!(
            db.get_pv("T:AI.NAME").unwrap(),
            EpicsValue::String("T:AI".into())
        );
        assert_eq!(
            db.get_pv("T:AI.RTYP").unwrap(),
            EpicsValue::String("ai".into())
        );
    }

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

    /// `post_property` writes the `setEnums` block silently and posts a
    /// single `DBE_PROPERTY` monitor on VAL — the C
    /// `db_post_events(precord, &precord->val, DBE_PROPERTY)` that asyn's
    /// runtime enum re-propagation drives (devAsynInt32.c callbackEnum). Two
    /// halves, and only the pair pins the shape: a `DBE_VALUE`-only VAL
    /// subscriber must NOT receive it (re-keying enum strings is a property
    /// change, not a new reading), and a `DBE_PROPERTY` subscriber on the
    /// written state field must not either (`setEnums` posts on nothing).
    #[epics_macros_rs::epics_test]
    async fn post_property_writes_the_block_and_posts_dbe_property_on_val() {
        use crate::server::device_support::PropertyPost;
        use crate::server::recgbl::EventMask;
        use crate::server::records::mbbi::MbbiRecord;
        use crate::types::DbFieldType;

        let db = PvDatabase::new();
        db.add_record("M:ENUM", Box::new(MbbiRecord::new(0)))
            .await
            .unwrap();
        let rec = db.get_record("M:ENUM").expect("record exists");

        let (mut val_prop_rx, mut val_value_rx, mut zrst_prop_rx) = {
            let mut inst = rec.write();
            let vp = inst
                .add_subscriber("VAL", 1, DbFieldType::Enum, EventMask::PROPERTY.bits())
                .expect("VAL property subscriber");
            let vv = inst
                .add_subscriber("VAL", 2, DbFieldType::Enum, EventMask::VALUE.bits())
                .expect("VAL value subscriber");
            let zp = inst
                .add_subscriber("ZRST", 3, DbFieldType::String, EventMask::PROPERTY.bits())
                .expect("ZRST property subscriber");
            (vp, vv, zp)
        };

        let written = db
            .post_property(
                "M:ENUM",
                PropertyPost {
                    writes: vec![("ZRST".to_string(), EpicsValue::String("LABEL".into()))],
                    post_field: "VAL".to_string(),
                },
            )
            .expect("post_property succeeds");
        assert_eq!(written, vec!["ZRST".to_string()]);

        // The field landed on the record.
        assert_eq!(
            db.get_pv("M:ENUM.ZRST").unwrap(),
            EpicsValue::String("LABEL".into())
        );

        assert!(
            val_prop_rx.try_recv().is_ok(),
            "the DBE_PROPERTY VAL subscriber is the one C posts to"
        );
        assert!(
            val_value_rx.try_recv().is_err(),
            "DBE_VALUE-only subscriber must not receive a property post"
        );
        assert!(
            zrst_prop_rx.try_recv().is_err(),
            "setEnums rewrites the state fields and posts on none of them"
        );
    }

    /// Regression: a direct CA put to a record whose value field VAL is NOT
    /// `pp(TRUE)` (calc / calcout / aSub) must still fire a DBE_VALUE monitor.
    /// C `dbAccess.c::dbPut:1408-1413` posts the value field immediately
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
    /// replaces only `*pLastLog` (`dbEvent.c:812-827`); the earlier entries stay
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
    /// C cannot reach the occupied one: `restartCheck` (dbNotify.c:149-170)
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

    /// The claim's owner path: a put-notify that reaches its process cycle
    /// commits, so the wait-set on the record is the one the client waits on
    /// and nothing releases it behind the cycle's back.
    ///
    /// This replaces the three `park_put_notify` boundary tests. Two of their
    /// boundaries no longer exist to test: "the slot was taken while we were
    /// writing" cannot happen now that the claim is taken in the same critical
    /// section as the ownership test, and "the record vanished between the put
    /// and the park" cannot happen now that the record handle is resolved once
    /// at the top of the body and used throughout.
    #[epics_macros_rs::epics_test]
    async fn a_put_notify_that_processes_commits_its_claim() {
        use crate::server::records::ai::AiRecord;

        let db = PvDatabase::new();
        db.add_record("CLAIM:PROC", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        let rec = db.get_record("CLAIM:PROC").expect("record exists");

        let done = db
            .put_record_field_from_ca("CLAIM:PROC", "VAL", EpicsValue::Double(3.5))
            .await
            .expect("a VAL put is accepted");
        // A passive ai with no chain settles inside the cycle, so the wait-set
        // has already fired and the record no longer owns it — the completion
        // path (C `dbNotifyCompletion`) took it, not the claim.
        assert!(matches!(
            done,
            crate::server::record::ProcessCompletion::Sync
        ));
        assert!(
            !rec.read().has_notify(),
            "the completed cycle must leave the slot free"
        );
        assert_eq!(
            rec.read().record.get_field("VAL"),
            Some(EpicsValue::Double(3.5))
        );
    }

    /// A formerly-bypassing early return: the put drives no process cycle, so
    /// the claim is never committed and `Drop` has to put the slot back.
    ///
    /// `DESC` is not `pp(TRUE)`, so `put_drives_processing_of` is false and the
    /// body returns `Ok(Sync)` at the last early return before the park. Before
    /// the claim moved to the entry gate nothing was installed on this path, so
    /// there was nothing to leak; now there is, and only the finalizer stops
    /// the record from being owned by a put that has already returned.
    #[epics_macros_rs::epics_test]
    async fn a_put_notify_that_drives_no_process_leaves_no_owner() {
        use crate::server::records::ai::AiRecord;

        let db = PvDatabase::new();
        db.add_record("CLAIM:NOPROC", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        let rec = db.get_record("CLAIM:NOPROC").expect("record exists");

        db.put_record_field_from_ca(
            "CLAIM:NOPROC",
            "DESC",
            EpicsValue::String("a description".into()),
        )
        .await
        .expect("a DESC put is accepted");
        assert!(
            !rec.read().has_notify(),
            "a put that processed nothing must not leave the record owned"
        );
        assert!(
            !rec.read().notify_restart_pending(),
            "nothing was queued, so nothing may be left on the restart list"
        );
        // The record is free for the next put-notify, which is the property the
        // leak would destroy: an owned record queues every later notify.
        db.put_record_field_from_ca("CLAIM:NOPROC", "VAL", EpicsValue::Double(1.0))
            .await
            .expect("the next put-notify must not be queued behind a stale owner");
        assert_eq!(
            rec.read().record.get_field("VAL"),
            Some(EpicsValue::Double(1.0))
        );
    }

    /// The other formerly-bypassing early return: the write is refused, so the
    /// body returns `Err` — and because C still processes on a refused notify
    /// put (`didPut = 1`, dbNotify.c:528-530 → `:243-256`), the claim is
    /// committed to that cycle rather than released under it.
    ///
    /// Either way the record must not stay owned once the call returns; the
    /// two outcomes differ in WHO releases the set, not in whether it is
    /// released.
    #[epics_macros_rs::epics_test]
    async fn a_refused_put_notify_leaves_no_owner() {
        use crate::server::records::ai::AiRecord;

        let db = PvDatabase::new();
        db.add_record("CLAIM:REFUSED", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        let rec = db.get_record("CLAIM:REFUSED").expect("record exists");

        let refused = db
            .put_record_field_from_ca(
                "CLAIM:REFUSED",
                "VAL",
                EpicsValue::String("not a number".into()),
            )
            .await;
        assert!(refused.is_err(), "a non-numeric VAL put must be refused");
        assert!(
            !rec.read().has_notify(),
            "a refused put-notify must not leave the record owned"
        );
        assert!(
            !rec.read().notify_restart_pending(),
            "a refused put-notify must queue nothing"
        );
    }

    /// The discriminator `a_refused_put_notify_leaves_no_owner` cannot supply.
    /// Commit-then-cycle and release-then-cycle agree only once the cycle has
    /// COMPLETED, so an end-state assertion cannot separate them. Give the
    /// cycle somewhere to stop — a calcout with `ODLY > 0` returns
    /// `AsyncPendingNotify` and finishes on the delayed callback — and they
    /// disagree while it is in flight.
    ///
    /// C is unambiguous: `putCallback` returns `didPut = 1` on the refused
    /// write (`dbNotify.c:528-530`), so `processNotifyCommon` assigns
    /// `precord->ppn = ppn` and calls `dbProcess` (`:243-256`) — the record
    /// processes UNDER the notify, and `dbNotifyCompletion` is what ends it.
    /// Releasing the claim first runs the same cycle with no notifier attached.
    #[epics_macros_rs::epics_test]
    async fn a_refused_put_notify_stays_installed_while_its_cycle_is_async() {
        use crate::server::records::calcout::CalcoutRecord;

        let db = PvDatabase::new();
        db.add_record("CLAIM:ODLY", Box::new(CalcoutRecord::default()))
            .await
            .unwrap();
        let rec = db.get_record("CLAIM:ODLY").expect("record exists");
        // Long enough that the delayed continuation cannot land mid-test.
        db.put_record_field_from_ca("CLAIM:ODLY", "ODLY", EpicsValue::Double(3600.0))
            .await
            .expect("ODLY is settable");
        assert!(
            !rec.read().has_notify(),
            "the ODLY put drives no cycle, so it owns nothing"
        );

        // C `caput REC.PROC <non-numeric>`: the store is refused and the
        // `pp(TRUE)` process still runs.
        let refused = db
            .put_record_field_from_ca("CLAIM:ODLY", "PROC", EpicsValue::String("nope".into()))
            .await;
        assert!(refused.is_err(), "a non-numeric PROC put must be refused");
        assert!(
            rec.read().is_processing(),
            "ODLY > 0 defers the calcout output, so the cycle is still in flight"
        );
        assert!(
            rec.read().has_notify(),
            "the async cycle runs under the refused put's wait-set: releasing \
             the claim before driving it leaves that cycle with no notifier"
        );
    }

    /// The same boundary for the OTHER commit-before-the-cycle site: "Cause B",
    /// a refused conversion on a `pp` field that still drives the record. The
    /// PROC intercept above has its own copy of this rule, so a discriminator
    /// for one does not cover the other.
    #[epics_macros_rs::epics_test]
    async fn a_refused_pp_field_put_stays_installed_while_its_cycle_is_async() {
        use crate::server::records::calcout::CalcoutRecord;

        let db = PvDatabase::new();
        db.add_record("CLAIM:ODLYB", Box::new(CalcoutRecord::default()))
            .await
            .unwrap();
        let rec = db.get_record("CLAIM:ODLYB").expect("record exists");
        db.put_record_field_from_ca("CLAIM:ODLYB", "ODLY", EpicsValue::Double(3600.0))
            .await
            .expect("ODLY is settable");

        // `A` is `pp(TRUE)` for calcout (C `calcoutRecord.dbd`), so a refused
        // conversion on it takes the Cause-B arm rather than returning early.
        let refused = db
            .put_record_field_from_ca("CLAIM:ODLYB", "A", EpicsValue::String("nope".into()))
            .await;
        assert!(refused.is_err(), "a non-numeric A put must be refused");
        assert!(
            rec.read().is_processing(),
            "ODLY > 0 defers the calcout output, so the cycle is still in flight"
        );
        assert!(
            rec.read().has_notify(),
            "Cause B commits to its cycle for the same reason the PROC refusal \
             does: C reaches doProcess with didPut = 1 and assigns precord->ppn"
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

    /// The gate is keyed on the DECLARED DBF class, so the claim to prove is
    /// over the CLASS and not over the ten fields the defect was found on.
    ///
    /// `#C0 S0 @p` parses to `VME_IO` (C `dbParseLink`'s `hwid == "CS"` arm,
    /// `dbStaticLib.c:2315`, which does not consult the field type), and no
    /// vendored `device()` line in this workspace declares a bus at all — the
    /// generated tables carry `CONSTANT` and `INST_IO` and nothing else. So C
    /// refuses that text on EVERY `DBF_INLINK`/`DBF_OUTLINK`/`DBF_FWDLINK`
    /// field of every record type, and every one of them must be refused here.
    ///
    /// The exempt set is named rather than counted loosely: a record type whose
    /// `.dbd` declares no `device()` has no `devSup` for `INP`/`OUT`, so
    /// `declared_link_type` answers `None` — nothing to compare, and C would
    /// have a device support there that this port does not (see
    /// `declared_link_type`'s own note). Those are the ONLY fields the sweep
    /// may skip, and they are all `INP`/`OUT`.
    #[test]
    fn every_declared_link_class_field_is_gated_by_class() {
        use crate::server::record::dbd_generated::RECORD_TYPES;
        use crate::server::record::{declared_fields, declared_link_type};

        let hw = EpicsValue::String("#C0 S0 @p".into());
        let mut gated = 0usize;
        let mut exempt = Vec::new();
        let mut accepted = Vec::new();
        for record_type in RECORD_TYPES {
            for desc in declared_fields(record_type) {
                if crate::types::dbf_link_class(record_type, desc.name).is_none() {
                    continue;
                }
                if declared_link_type(record_type, None, desc.name).is_none() {
                    exempt.push(format!("{record_type}.{}", desc.name));
                    continue;
                }
                if super::check_link_put(record_type, "", desc.name, &hw).is_err() {
                    gated += 1;
                } else {
                    accepted.push(format!("{record_type}.{}", desc.name));
                }
            }
        }
        assert!(
            accepted.is_empty(),
            "a hardware link no vendored device() declares was accepted on {accepted:?}"
        );
        assert!(
            exempt
                .iter()
                .all(|f| f.ends_with(".INP") || f.ends_with(".OUT")),
            "only the device link of a type with no vendored device() may be exempt: {exempt:?}"
        );
        // The generator emits 265 DBF_INLINK, 103 DBF_OUTLINK and 17
        // DBF_FWDLINK declarations across its record tables, plus dbCommon's
        // TSEL/SDIS/FLNK once per type. A drop here means the sweep stopped
        // seeing the population, not that the population shrank.
        assert_eq!(
            gated + exempt.len(),
            LINK_CLASS_FIELD_COUNT,
            "the sweep must reach every declared link-class field"
        );
        assert!(gated > 400, "only {gated} fields reached the gate");

        // The ten fields the defect was cited on are inside what was just
        // swept — the sample, shown to be part of the population.
        for (record_type, field) in [
            ("ai", "SDIS"),
            ("ai", "TSEL"),
            ("ai", "FLNK"),
            ("ai", "SIML"),
            ("ai", "SIOL"),
            ("ai", "INP"),
            ("calc", "INPA"),
            ("ao", "DOL"),
            ("ao", "OUT"),
            ("fanout", "LNK1"),
        ] {
            assert!(
                super::check_link_put(record_type, "", field, &hw).is_err(),
                "{record_type}.{field} must refuse a VME_IO link"
            );
        }
    }

    /// The population the sweep above walks, counted once so a change in the
    /// generator's output shows up as one failure rather than as a silently
    /// smaller sweep.
    const LINK_CLASS_FIELD_COUNT: usize = 505;

    /// The gate order is observable exactly where a link-class field is also
    /// `special(SPC_NOMOD)`: C runs `dbCanSetLink` first and reaches the
    /// no-mod refusal only from `dbPutSpecial(paddr, 0)` afterwards, so on
    /// such a field a type-invalid text answers `S_dbLib_badField` and a
    /// type-valid one answers `S_db_noMod`. `aSub.SUBL` is the only field in
    /// the vendored population that is both, and the count is pinned rather
    /// than assumed: a second one appearing changes which orders are
    /// equivalent, and this is the test that says so.
    #[test]
    fn asub_subl_is_the_only_read_only_link_field() {
        use crate::server::record::dbd_generated::RECORD_TYPES;
        use crate::server::record::{Special, declared_fields};

        let mut offenders = Vec::new();
        for record_type in RECORD_TYPES {
            for desc in declared_fields(record_type) {
                if crate::types::dbf_link_class(record_type, desc.name).is_none() {
                    continue;
                }
                if desc.read_only
                    || desc.special == Special::NoMod
                    || desc.declared_special == Special::NoMod
                {
                    offenders.push(format!("{record_type}.{}", desc.name));
                }
            }
        }
        assert_eq!(
            offenders,
            ["aSub.SUBL"],
            "the set of link fields where the gate order is observable has changed"
        );
    }

    /// The wiring: both `dbPutField`-analogue bodies refuse the link C refuses,
    /// across all three DBF classes and both storage kinds (a `dbCommon` link
    /// held in `CommonFields`, and one declared by the record type itself), and
    /// the refused field keeps the text it had.
    ///
    /// `@instio p` is the text to use because it is what a user actually types
    /// — every asyn device support takes that form — and because it is refused
    /// for the reason C gives: these fields have no `devSup`, so
    /// `dbCanSetLink` holds them to `CONSTANT` (`dbStaticLib.c:2403`).
    #[epics_macros_rs::epics_test]
    async fn both_db_put_field_bodies_refuse_a_device_link_on_every_class() {
        use crate::server::records::ao::AoRecord;
        use crate::server::records::calc::CalcRecord;
        use crate::server::records::fanout::FanoutRecord;

        let db = PvDatabase::new();
        db.add_record("LK:CALC", Box::new(CalcRecord::new("A")))
            .await
            .unwrap();
        db.add_record("LK:AO", Box::new(AoRecord::new(0.0)))
            .await
            .unwrap();
        db.add_record("LK:FO", Box::new(FanoutRecord::new()))
            .await
            .unwrap();

        // (record, field, DBF class, storage) — INLINK/OUTLINK/FWDLINK each
        // appear in both a `dbCommon` and a record-declared row.
        let cases = [
            ("LK:CALC", "SDIS"), // DBF_INLINK,   dbCommon
            ("LK:CALC", "TSEL"), // DBF_INLINK,   dbCommon
            ("LK:CALC", "INPA"), // DBF_INLINK,   record-declared
            ("LK:AO", "DOL"),    // DBF_INLINK,   record-declared
            ("LK:AO", "SIML"),   // DBF_INLINK,   record-declared
            ("LK:AO", "SIOL"),   // DBF_OUTLINK,  record-declared
            ("LK:AO", "OUT"),    // DBF_OUTLINK,  dbCommon
            ("LK:CALC", "FLNK"), // DBF_FWDLINK,  dbCommon
            ("LK:FO", "LNK1"),   // DBF_FWDLINK,  record-declared
        ];

        for (name, field) in cases {
            let before = db
                .get_pv(&format!("{name}.{field}"))
                .unwrap_or_else(|e| panic!("{name}.{field} must be readable: {e}"));

            let ca = db
                .put_record_field_from_ca_no_notify(
                    name,
                    field,
                    EpicsValue::String("@instio p".into()),
                )
                .await;
            assert!(
                ca.is_err(),
                "{name}.{field}: a caput of an INST_IO link must be refused, as C's \
                 dbPutFieldLink refuses it"
            );

            let restore = db
                .put_pv_no_process(
                    &format!("{name}.{field}"),
                    EpicsValue::String("@instio p".into()),
                )
                .await;
            assert!(
                restore.is_err(),
                "{name}.{field}: an autosave restore is a dbPutField too"
            );

            let after = db
                .get_pv(&format!("{name}.{field}"))
                .unwrap_or_else(|e| panic!("{name}.{field} must still be readable: {e}"));
            assert_eq!(
                format!("{before:?}"),
                format!("{after:?}"),
                "{name}.{field} must keep the text it had — C leaves the link untouched"
            );
        }

        // The control: the same fields take a link of the type they DO expect,
        // so the gate refuses by type and not by class membership.
        db.put_record_field_from_ca_no_notify(
            "LK:CALC",
            "INPA",
            EpicsValue::String("SRC.VAL".into()),
        )
        .await
        .expect("a PV_LINK is what CONSTANT accepts (dbStaticLib.c:2408-2416)");
        let stored = db.get_pv("LK:CALC.INPA").unwrap();
        let EpicsValue::String(stored) = stored else {
            panic!("a link field serves as DBF_STRING, got {stored:?}");
        };
        assert!(
            stored.as_str_lossy().starts_with("SRC.VAL"),
            "the accepted link is the one that was written, got {stored:?}"
        );
    }

    /// C's request-type switch for a link field (`dbAccess.c:1084-1096`):
    /// `DBR_STRING`, or `DBR_CHAR`/`DBR_UCHAR` whose last element is the NUL,
    /// and `S_db_badDbrtype` for everything else. Measured against softIoc
    /// R7.0.10 over CA on `SDIS`, `INPA`, `OUT` and `LNK1` — a `DBR_DOUBLE`,
    /// `DBR_LONG`, `DBR_SHORT` or `DBR_ENUM` put fails the channel write and
    /// leaves the link text alone, while a lone NUL byte succeeds and CLEARS
    /// the link. The port used to convert every one of them to a string and
    /// store it, so `caput` of a number turned a link into `"5"`.
    ///
    /// `DESC` is the control: it is `DBF_STRING` and not a link, so the same
    /// requests are converted and stored there, in C and here alike. That is
    /// what makes this a rule about the link route rather than about strings.
    #[epics_macros_rs::epics_test]
    async fn a_link_field_takes_only_a_string_or_a_nul_terminated_char_array() {
        use crate::server::records::calc::CalcRecord;

        let db = PvDatabase::new();
        db.add_record("TY:CALC", Box::new(CalcRecord::new("A")))
            .await
            .unwrap();
        db.put_record_field_from_ca_no_notify(
            "TY:CALC",
            "SDIS",
            EpicsValue::String("SRC.VAL".into()),
        )
        .await
        .expect("a DBR_STRING link put is what C accepts");

        for bad in [
            EpicsValue::Double(5.0),
            EpicsValue::Long(5),
            EpicsValue::Short(5),
            EpicsValue::Enum(1),
            EpicsValue::Float(5.0),
            EpicsValue::Int64(5),
            // `pstring[nRequest - 1] != '\0'` with `nRequest == 1`.
            EpicsValue::Char(b'S'),
            EpicsValue::UChar(b'S'),
            // Same test with a longer buffer: the LAST element must be the NUL,
            // an interior one does not save it.
            EpicsValue::CharArray(b"SRC.VAL".to_vec()),
            EpicsValue::CharArray(b"SRC\0VAL".to_vec()),
        ] {
            let ca = db
                .put_record_field_from_ca_no_notify("TY:CALC", "SDIS", bad.clone())
                .await;
            assert!(
                matches!(ca, Err(crate::server::database::CaError::BadDbrType(_))),
                "a {bad:?} put to a link field is S_db_badDbrtype in C, got {ca:?}"
            );
            let restore = db.put_pv_no_process("TY:CALC.SDIS", bad.clone()).await;
            assert!(
                matches!(
                    restore,
                    Err(crate::server::database::CaError::BadDbrType(_))
                ),
                "the autosave body is a dbPutField too, got {restore:?} for {bad:?}"
            );
            let held = db.get_pv("TY:CALC.SDIS").unwrap();
            assert!(
                matches!(&held, EpicsValue::String(s) if s.as_str_lossy().starts_with("SRC.VAL")),
                "a refused put must leave the link text alone, got {held:?}"
            );
        }

        // The NUL-terminated char array is C's other accepted form, and the
        // text is the C string it holds — not the byte values, and not the
        // bytes past the terminator.
        db.put_record_field_from_ca_no_notify(
            "TY:CALC",
            "SDIS",
            EpicsValue::CharArray(b"OTHER.VAL\0".to_vec()),
        )
        .await
        .expect("a NUL-terminated DBR_CHAR buffer is accepted");
        let held = db.get_pv("TY:CALC.SDIS").unwrap();
        assert!(
            matches!(&held, EpicsValue::String(s) if s.as_str_lossy().starts_with("OTHER.VAL")),
            "the stored text is the C string in the buffer, got {held:?}"
        );

        // A lone NUL is `nRequest == 1` with the terminator in place: accepted,
        // and the link text is empty. Storing the byte's decimal spelling
        // (`"0"`) is the bug this arm closes.
        db.put_record_field_from_ca_no_notify("TY:CALC", "SDIS", EpicsValue::Char(0))
            .await
            .expect("a lone NUL clears the link, as in C");
        assert_eq!(
            db.get_pv("TY:CALC.SDIS").unwrap(),
            EpicsValue::String("".into()),
            "C stores the empty C string, not the byte's decimal spelling"
        );

        // Control: DESC is DBF_STRING and not a link, so C converts and stores.
        db.put_record_field_from_ca_no_notify("TY:CALC", "DESC", EpicsValue::Double(5.0))
            .await
            .expect("a non-link string field converts, in C and here");
        assert!(
            matches!(db.get_pv("TY:CALC.DESC").unwrap(),
                     EpicsValue::String(s) if s.as_str_lossy() == "5"),
            "the control must show the refusal is the link route, not the string type"
        );
    }

    /// An autosave restore is a `dbPutField`, so it must run the
    /// `dbPutConvertRoutine` type row — not just the shape arm. A saved
    /// `DBF_DOUBLE` field stored as a string (`"3.5"`) has to reach the record
    /// through `putStringDouble`'s parse, the same row the client `dbPut` path
    /// runs, instead of being handed to `put_field` as a raw `String` that its
    /// numeric arm refuses. Before the type row was wired into
    /// `put_pv_no_process`, the positive restore below failed with
    /// `TypeMismatch` and the field kept its default `0.0`.
    #[epics_macros_rs::epics_test]
    async fn an_autosave_restore_runs_the_dbputconvertroutine_type_row() {
        use crate::server::records::calc::CalcRecord;

        let db = PvDatabase::new();
        db.add_record("TY:CALC", Box::new(CalcRecord::new("A+1")))
            .await
            .unwrap();

        // A served DBF_DOUBLE field restored from its saved string spelling is
        // PARSED through the type row, exactly as `caput CALC.A 3.5` would be.
        db.put_pv_no_process("TY:CALC.A", EpicsValue::String("3.5".into()))
            .await
            .expect("a numeric string restore parses through the convert row");
        assert!(
            matches!(db.get_pv("TY:CALC.A").unwrap(),
                     EpicsValue::Double(v) if (v - 3.5).abs() < 1e-9),
            "the restored A must be the parsed number 3.5, got {:?}",
            db.get_pv("TY:CALC.A")
        );

        // And an unparseable one is REFUSED by `epicsParseFloat64`, leaving the
        // field alone — the field-blind path would have stored `0.0` instead.
        let bad = db
            .put_pv_no_process("TY:CALC.A", EpicsValue::String("not_a_number".into()))
            .await;
        assert!(
            bad.is_err(),
            "an unparseable numeric restore is refused, got {bad:?}"
        );
        assert!(
            matches!(db.get_pv("TY:CALC.A").unwrap(),
                     EpicsValue::Double(v) if (v - 3.5).abs() < 1e-9),
            "a refused restore leaves A at 3.5, got {:?}",
            db.get_pv("TY:CALC.A")
        );
    }
}
