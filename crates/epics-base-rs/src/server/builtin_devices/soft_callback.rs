//! `"Async Soft Channel"`'s `add_record` — the INP check C makes before an
//! asynchronous soft input record is ever processed.
//!
//! ```c
//! static long add_record(dbCommon *pcommon)
//! {
//!     aiRecord *prec = (aiRecord *)pcommon;
//!     DBLINK *plink = &prec->inp;
//!     ...
//!     if (dbLinkIsDefined(plink) && dbLinkIsConstant(plink))
//!         return 0;
//!
//!     if (plink->type != PV_LINK) {
//!         long status = S_db_badField;
//!
//!         recGblRecordError(status, prec,
//!             "devAiSoftCallback (add_record) Illegal INP field");
//!         return status;
//!     }
//! ```
//! (`devAiSoftCallback.c:76-93`, and the same shape in the six input twins.)
//!
//! Two facts decide what this port owes.
//!
//! The first early return is DEAD at IOC init. `dbLinkIsDefined` is
//! `plink->lset != 0` (`dbLink.c:215-218`), and `iocInit.c::doResolveLinks`
//! calls `add_record` BEFORE `dbInitLink` for that same link
//! (`iocInit.c:546-559`), so `lset` is still NULL and the guard cannot fire.
//! It exists for the other caller, `dbPutFieldLink` (`dbAccess.c:1196`) — a
//! DTYP changed at runtime, on a link that is already initialised. So at
//! startup the ONLY test is `plink->type != PV_LINK`, which is why a
//! `field(INP, {const:9})` earns the error and a `field(INP, "someRecord")`
//! does not.
//!
//! Only the seven INPUT flavours have an `add_record` at all. `devAoSoft-
//! Callback.c` and the nine other output files declare a plain dset with no
//! `dsxt`, so an output `"Async Soft Channel"` record is never checked —
//! matching `softIoc` R7.0.10 on `asyncSoftTest.db`, which emits exactly seven
//! of these lines and nothing for `ao1`/`bo1`/`lso1`/`so1`.

use crate::server::database::PvDatabase;
use crate::server::database::filters::ChannelCreateError;
use crate::server::database::filters::channel::db_channel_create_unreported;
use crate::server::device_support::Dpvt;
use crate::server::device_support::{SoftDtyp, served_soft};
use crate::server::record::{ParsedLink, RecordInstance, parse_link_v2};

/// The `dev<T>SoftCallback` dset whose name goes in the message, for the seven
/// input record types C ships one for, with the text its `add_record` gives a
/// target `dbChannelCreate` cannot find.
///
/// Neither is derivable from the record type — C abbreviates `int64in` to
/// `I64in`, `longin` to `Li` and `stringin` to `Si`, and the not-found text
/// differs per file (`devLiSoftCallback.c:98` even says `init_record`) — so
/// it is a table, and the table is also the gate: a record type absent from
/// it has no `add_record` in C and must not be checked.
fn callback_dset(record_type: &str) -> Option<(&'static str, &'static str)> {
    Some(match record_type {
        "ai" => ("devAiSoftCallback", "(add_record) link target not found"),
        "bi" => ("devBiSoftCallback", "(add_record) link target not found"),
        "int64in" => (
            "devI64inSoftCallback",
            "(init_record) linked record not found",
        ),
        "longin" => ("devLiSoftCallback", "(init_record) linked record not found"),
        "mbbi" => (
            "devMbbiSoftCallback",
            "(add_record) linked record not found",
        ),
        "mbbiDirect" => (
            "devMbbiDirectSoftCallback",
            "(add_record) linked record not found",
        ),
        "stringin" => ("devSiSoftCallback", "(add_record) linked record not found"),
        _ => return None,
    })
}

/// C `plink->type == PV_LINK` for a link the `.db` parser has produced and
/// `dbInitLink` has not yet touched, as the PV name `add_record` opens.
///
/// A PV link is the only thing `dbParseLink` turns into `PV_LINK`; this port
/// resolves the CA/CP/PP modifiers at parse time and so splits that one C type
/// across two variants, but both are the same `PV_LINK` here. Everything
/// else — a constant, an empty field, a JSON link (`{const:9}` included), a
/// `#C0 S0` hardware address — is one of C's other link types.
fn pv_link_name(inp: &str) -> Option<String> {
    match parse_link_v2(inp) {
        ParsedLink::Db(link) => Some(link.pvname()),
        ParsedLink::Ca(link) => Some(link.pv),
        _ => None,
    }
}

/// What `add_record` reads off INP, taken before the record's hold: whether
/// it is a PV link and, if so, its `dbChannelCreate`
/// (`devAiSoftCallback.c:95`), which looks up another record.
pub(crate) enum AddRecordInput {
    NotPvLink,
    Channel(Result<(), ChannelCreateError>),
}

/// [`AddRecordInput`] for a record whose DTYP and INP text are these, or
/// `None` when `add_record` is not this record's to run. The caller holds no
/// record lock.
pub(crate) fn add_record_input(
    db: &PvDatabase,
    record_type: &str,
    dtyp: &str,
    inp: &str,
) -> Option<AddRecordInput> {
    callback_dset(record_type)?;
    if served_soft(record_type, dtyp) != Some(SoftDtyp::Async) {
        return None;
    }
    Some(match pv_link_name(inp) {
        // An undotted name is all record name to `dbFindRecordPart`
        // (`dbStaticLib.c:1557-1566`), so `REC$` names no record.
        Some(name) if !name.contains('.') && name.contains('$') => {
            AddRecordInput::Channel(Err(ChannelCreateError::NotFound))
        }
        Some(name) => AddRecordInput::Channel(db_channel_create_unreported(db, &name).map(|_| ())),
        None => AddRecordInput::NotPvLink,
    })
}

/// [`add_record_input`] for a published record, read under its read lock and
/// resolved after the lock is down. `inp` stands in for the record's own INP
/// when a put is about to replace it.
pub(crate) fn add_record_input_of(
    db: &PvDatabase,
    rec: &crate::server::record::RecordCell,
    inp: Option<&str>,
) -> Option<AddRecordInput> {
    let (record_type, dtyp, own_inp) = {
        let instance = rec.read();
        callback_dset(instance.record.record_type())?;
        (
            instance.record.record_type(),
            instance.common.dtyp.as_str().to_string(),
            inp.is_none().then(|| instance.common.text.inp.clone()),
        )
    };
    add_record_input(db, record_type, &dtyp, inp.or(own_inp.as_deref())?)
}

/// An `add_record` refusal, printed by the caller once it holds no lock.
pub(crate) struct AddRecordRefusal {
    dset: &'static str,
    not_found: &'static str,
    channel: Option<ChannelCreateError>,
}

impl AddRecordRefusal {
    /// The refusing dset, for the status a put returns with it.
    pub(crate) fn dset(&self) -> &'static str {
        self.dset
    }

    pub(crate) fn report(&self, name: &str) {
        let (status, why) = match &self.channel {
            // `S_db_badField` is `(511 << 16) | 15`, and `dbAccessErrSymTbl`
            // IS linked, so `errSymLookup` finds it and prints the phrase
            // rather than the numeric fallback the `M_devSup` statuses take.
            None => ("Illegal field value", "(add_record) Illegal INP field"),
            Some(e) => {
                e.report();
                ("Process Variable Not Found", self.not_found)
            }
        };
        crate::server::recgbl::rec_gbl_record_error(status, name, &format!("{} {why}", self.dset));
    }
}

/// C `dev<T>SoftCallback::add_record`, at `iocInit.c::doResolveLinks` and at
/// `dbPutFieldLink`'s device-link rebind (`dbAccess.c:1196`), which C
/// reaches with the link removed, so both see the same INP.
///
/// `doResolveLinks` discards the status (`iocInit.c:551-554`), so the record
/// is built either way; the refusal survives only as the NULL `dpvt` that
/// makes every `read_xxx` return 2 unread (`devAiSoftCallback.c:171-172`).
/// `input` is [`add_record_input`]'s; without it only the PV-link test runs.
pub(crate) fn add_record(
    instance: &mut RecordInstance,
    input: Option<AddRecordInput>,
) -> Option<AddRecordRefusal> {
    // The dset gate: `add_record` is `devXxxSoftCallback`'s dsxt entry, so it
    // exists only for a record whose DTYP selected that dset. This predicate
    // is the hook's own, not its caller's — the init owner calls the hook for
    // every record at C's `doResolveLinks` point and each hook answers for
    // itself, the way C's `pdsxt` is either present on the dset or not.
    let dtyp = instance.common.dtyp.as_str();
    if served_soft(instance.record.record_type(), dtyp) != Some(SoftDtyp::Async) {
        return None;
    }
    let input = input.unwrap_or_else(|| match pv_link_name(&instance.common.text.inp) {
        Some(_) => AddRecordInput::Channel(Ok(())),
        None => AddRecordInput::NotPvLink,
    });
    match answer(instance.record.record_type(), input) {
        Ok(dpvt) => {
            instance.dpvt = dpvt;
            instance.invalidate_plan();
            None
        }
        Err(refusal) => {
            instance.common.dtyp.refuse_link();
            instance.invalidate_plan();
            Some(refusal)
        }
    }
}

/// `add_record`'s answer for a `record_type` record on `input`: the `dpvt`
/// it leaves (a fresh devPvt for a PV link it accepts), or its refusal.
fn answer(record_type: &str, input: AddRecordInput) -> Result<Dpvt, AddRecordRefusal> {
    let Some((dset, not_found)) = callback_dset(record_type) else {
        return Ok(Dpvt::Null);
    };
    let channel = match input {
        AddRecordInput::Channel(Ok(())) => return Ok(Dpvt::Callback(Box::default())),
        AddRecordInput::Channel(Err(e)) => Some(e),
        AddRecordInput::NotPvLink => None,
    };
    Err(AddRecordRefusal {
        dset,
        not_found,
        channel,
    })
}

/// `dbPutFieldLink`'s `new_dsxt->add_record` (`dbAccess.c:1196`) for an INP
/// put of `inp` to a `record_type` record whose DTYP is `dtyp`, decided
/// before the hold since it looks the target up.
pub(crate) fn put_add_record(
    db: &PvDatabase,
    record_type: &str,
    dtyp: &str,
    inp: &str,
) -> Result<Dpvt, AddRecordRefusal> {
    match add_record_input(db, record_type, dtyp, inp) {
        Some(input) => answer(record_type, input),
        None => Ok(Dpvt::Null),
    }
}
