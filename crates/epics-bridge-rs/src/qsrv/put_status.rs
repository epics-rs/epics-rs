//! Single owner of the `Status.message` text a QSRV PUT rejection puts on
//! the wire.
//!
//! pvxs rejects a put by throwing a bare `std::runtime_error` whose `what()`
//! the server forwards verbatim as the remote error message
//! (`groupsource.cpp:656-667`, `singlesource.cpp` `onPut` catch). The texts
//! are a fixed contract:
//!
//! - `"Unable to put value: Modifications not allowed: S_db_noMod"` —
//!   `iocsource.cpp:366` (`special == SPC_ATTRIBUTE`)
//! - `"Unable to put value: Field Disabled: S_db_putDisabled"` —
//!   `iocsource.cpp:367-368` (`precord->disp`)
//! - `"Put not permitted"` — `iocsource.cpp:385` (`!securityClient.canWrite()`)
//! - `"Links not supported for put"` — `groupsource.cpp:605`
//! - `"No fields changed"` — `groupsource.cpp:658`
//!
//! None of them carries record/group/member identity, a user/host, or a
//! source citation: pvxs keeps that detail in `log_debug_printf`
//! (`groupsource.cpp:662-663`). Every QSRV put-rejection message is built
//! here so no call site can put its own text — including an internal
//! `pvxs <file>:<line>` citation — on the wire.

use epics_base_rs::error::CaError;
use epics_base_rs::server::database::PvDatabase;

use crate::error::{BridgeError, BridgeResult};

/// `iocsource.cpp:385` — write-ACF denial.
pub(crate) const PUT_NOT_PERMITTED: &str = "Put not permitted";
/// `groupsource.cpp:605` — a group member bound to a link-class field.
pub(crate) const LINKS_NOT_SUPPORTED: &str = "Links not supported for put";
/// `groupsource.cpp:658` — client marked fields but nothing was written.
pub(crate) const NO_FIELDS_CHANGED: &str = "No fields changed";
/// `iocsource.cpp:366` — `dbChannel` bound to an `SPC_ATTRIBUTE` field.
pub(crate) const UNABLE_NO_MOD: &str = "Unable to put value: Modifications not allowed: S_db_noMod";
/// `iocsource.cpp:368` — record `DISP=1`.
pub(crate) const UNABLE_PUT_DISABLED: &str =
    "Unable to put value: Field Disabled: S_db_putDisabled";

/// `IOCSource::doPreProcessing` (`iocsource.cpp:362-375`), which pvxs runs on
/// every QSRV put — single record (`singlesource.cpp:356`) and each group
/// member (`groupsource.cpp:601`) — before the write-ACF check.
///
/// The rejection detail (which record, which field) goes to the server log,
/// exactly as pvxs does; only the contract text reaches the client.
pub(crate) async fn check_preconditions(
    db: &PvDatabase,
    record_name: &str,
    field_name: &str,
) -> BridgeResult<()> {
    db.check_external_put_preconditions(record_name, field_name)
        .await
        .map_err(|e| {
            let msg = match &e {
                CaError::ReadOnlyField(_) => UNABLE_NO_MOD.to_string(),
                CaError::PutDisabled(_) => UNABLE_PUT_DISABLED.to_string(),
                // `check_external_put_preconditions` raises only those two;
                // any future precondition keeps pvxs's `"Unable to put
                // value: <detail>"` shape rather than inventing a new one.
                other => format!("Unable to put value: {other}"),
            };
            tracing::debug!("QSRV PUT rejected on {record_name}.{field_name}: {msg} ({e})");
            BridgeError::PutRejected(msg)
        })
}

/// `IOCSource::doFieldPreProcessing` (`iocsource.cpp:383-387`) — write-ACF
/// denial. `detail` (who was denied on what) is logged server-side; the wire
/// carries pvxs's bare text.
pub(crate) fn put_not_permitted(detail: &str) -> BridgeError {
    tracing::debug!("QSRV PUT rejected: {PUT_NOT_PERMITTED} ({detail})");
    BridgeError::PutRejected(PUT_NOT_PERMITTED.to_string())
}

/// `groupsource.cpp:603-606` — a group member bound to a link-class field
/// (`DBF_INLINK..DBF_FWDLINK`).
pub(crate) fn links_not_supported(detail: &str) -> BridgeError {
    tracing::debug!("QSRV PUT rejected: {LINKS_NOT_SUPPORTED} ({detail})");
    BridgeError::PutRejected(LINKS_NOT_SUPPORTED.to_string())
}

/// `groupsource.cpp:656-659` — `!didSomething && value.isMarked(true, true)`.
pub(crate) fn no_fields_changed(detail: &str) -> BridgeError {
    tracing::debug!("QSRV PUT rejected: {NO_FIELDS_CHANGED} ({detail})");
    BridgeError::PutRejected(NO_FIELDS_CHANGED.to_string())
}

/// The `Status.message` a failed QSRV operation puts on the wire.
///
/// pvxs forwards the thrown `e.what()` verbatim (`groupsource.cpp:666`,
/// `singlesource.cpp` `onPut` catch → `putOperation->error(e.what())`), so a
/// rejection message must reach the client exactly as authored above — the
/// `Display` prefix `BridgeError` puts on its variants (`"put rejected: …"`)
/// is a Rust-side log convention with no pvxs counterpart and must not be
/// serialized. Every QSRV `OpError` message is built from this function.
pub(crate) fn wire_message(e: &BridgeError) -> String {
    match e {
        BridgeError::PutRejected(msg) => msg.clone(),
        other => other.to_string(),
    }
}
