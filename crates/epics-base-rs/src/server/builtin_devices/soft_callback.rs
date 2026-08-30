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

use crate::server::record::{ParsedLink, RecordInstance, parse_link_v2};

/// The `dev<T>SoftCallback` dset whose name goes in the message, for the seven
/// input record types C ships one for.
///
/// The name is not derivable from the record type — C abbreviates `int64in` to
/// `I64in`, `longin` to `Li` and `stringin` to `Si` — so it is a table, and the
/// table is also the gate: a record type absent from it has no `add_record` in
/// C and must not be checked.
fn callback_dset(record_type: &str) -> Option<&'static str> {
    Some(match record_type {
        "ai" => "devAiSoftCallback",
        "bi" => "devBiSoftCallback",
        "int64in" => "devI64inSoftCallback",
        "longin" => "devLiSoftCallback",
        "mbbi" => "devMbbiSoftCallback",
        "mbbiDirect" => "devMbbiDirectSoftCallback",
        "stringin" => "devSiSoftCallback",
        _ => return None,
    })
}

/// C `plink->type == PV_LINK` for a link the `.db` parser has produced and
/// `dbInitLink` has not yet touched.
///
/// A PV link is the only thing `dbParseLink` turns into `PV_LINK`; this port
/// resolves the CA/CP/PP modifiers at parse time and so splits that one C type
/// across two variants, but both are the same `PV_LINK` here. Everything
/// else — a constant, an empty field, a JSON link (`{const:9}` included), a
/// `#C0 S0` hardware address — is one of C's other link types.
fn is_pv_link(inp: &str) -> bool {
    matches!(parse_link_v2(inp), ParsedLink::Db(_) | ParsedLink::Ca(_))
}

/// C `dev<T>SoftCallback::add_record`, for one record, at the point
/// `iocInit.c::doResolveLinks` calls it.
///
/// Reports and returns; `doResolveLinks` discards the status
/// (`iocInit.c:551-554`), so the record is built either way and the operator's
/// only warning that its INP can never be read is this line.
pub(crate) fn add_record(instance: &RecordInstance, name: &str) {
    // The dset gate: `add_record` is `devXxxSoftCallback`'s dsxt entry, so it
    // exists only for a record whose DTYP selected that dset. This predicate
    // is the hook's own, not its caller's — the init owner calls the hook for
    // every record at C's `doResolveLinks` point and each hook answers for
    // itself, the way C's `pdsxt` is either present on the dset or not.
    if crate::server::device_support::classify_soft(&instance.common.dtyp)
        != Some(crate::server::device_support::SoftDtyp::Async)
    {
        return;
    }
    let Some(dset) = callback_dset(instance.record.record_type()) else {
        return;
    };
    if is_pv_link(&instance.common.inp) {
        return;
    }
    // `S_db_badField` is `(511 << 16) | 15`, and `dbAccessErrSymTbl` IS linked,
    // so `errSymLookup` finds it and prints the phrase rather than the numeric
    // fallback the `M_devSup` statuses take.
    crate::server::recgbl::rec_gbl_record_error(
        "Illegal field value",
        name,
        &format!("{dset} (add_record) Illegal INP field"),
    );
}
