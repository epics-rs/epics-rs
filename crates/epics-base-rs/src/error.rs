use thiserror::Error;

#[derive(Error, Debug)]
pub enum CaError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("timeout waiting for response")]
    Timeout,

    #[error("channel not found: {0}")]
    ChannelNotFound(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("unsupported DBR type: {0}")]
    UnsupportedType(u16),

    #[error("write failed: ECA status {0:#06x}")]
    WriteFailed(u32),

    #[error("field not found: {0}")]
    FieldNotFound(String),

    #[error("field is read-only: {0}")]
    ReadOnlyField(String),

    #[error("type mismatch for field {0}")]
    TypeMismatch(String),

    #[error("invalid value: {0}")]
    InvalidValue(String),

    /// C `S_db_badField` ("Illegal RECORD FIELD") — a record's `special()`
    /// refused the value that `dbPut` had already stored, e.g.
    /// `calcRecord.c:146-152` returning it for an uncompilable `CALC`. The
    /// value stays written, the field's monitor is not posted, the record is
    /// not processed, and the status propagates to the client (rsrv
    /// `write_action` → `ECA_PUTFAIL`).
    #[error("illegal record field: {0}")]
    BadField(String),

    /// C `S_db_badChoice` ("Illegal choice") — a `DBR_STRING` write to a
    /// `DBF_MENU` field named neither an exact choice label nor an in-range
    /// index (`dbConvert.c::putStringMenu:1216-1229`). C's converter returns
    /// this from inside `dbPut`, *before* the value is stored
    /// (`dbAccess.c:1362`), so the field keeps its old value, no monitor is
    /// posted and the record is not processed.
    #[error("illegal menu choice: {0}")]
    BadChoice(String),

    /// C `S_db_badDbrtype` ("Illegal Database Request Type") — `dbPut`
    /// refusing a put to a DBF link field (`field_type > DBF_DEVICE`,
    /// `dbAccess.c:1340-1347`). Only `dbPutField` changes link fields, by
    /// routing them through `dbPutFieldLink` (`dbAccess.c:1261-1262`); a
    /// `dbPut` reached from a record's OUT link (`dbPutLink` →
    /// `dbDbPutValue`) or from internal code refuses, so a DB link cannot
    /// silently rewire another record's link field. rsrv answers it with
    /// ECA_PUTFAIL like every non-zero put status — and with ECA_GETFAIL when
    /// the same refusal comes back from a get, which is why
    /// [`CaError::to_eca_status`] takes the direction.
    #[error("illegal database request type: {0}")]
    BadDbrType(String),

    #[error("put disabled (DISP=1) for field {0}")]
    PutDisabled(String),

    #[error("link error: {0}")]
    LinkError(String),

    /// A `.db`/`.dbd` parse abort, carried as C `yyerror` prints it
    /// (`dbYacc.y:370-383`): the line, the sentence, and `yytext` — the token
    /// the lexer had matched when the parser rejected it, which is what C
    /// quotes in its ` at or before '%s'` clause. An empty `token` is a
    /// failure raised where no token was matched, and prints no such clause.
    #[error("DB parse error at line {line}: {message}")]
    DbParseError {
        line: usize,
        token: String,
        message: String,
    },

    /// C `dbLoadRecords` returning non-zero after `yyerror(NULL)`
    /// recovered from a bad item (`dbAccess.c:795-813`). The records
    /// that parsed are still there; the load's *status* is the failure,
    /// and `softMain` exits 2 on it (`softMain.cpp:198,274-278`).
    #[error("Failed to load '{0}'")]
    DbLoadFailed(String),

    #[error("calc error: {0}")]
    CalcError(String),

    #[error("channel disconnected")]
    Disconnected,

    #[error("client shut down")]
    Shutdown,

    /// Server-emitted ECA status carried out-of-band on an otherwise
    /// data-shaped frame — used by libca `cac::eventAddRespAction`
    /// (`cac.cpp:973-977`) when a monitor frame's `m_cid` is non-
    /// NORMAL (e.g. `ECA_NORDACCESS` from `no_read_access_event`
    /// after an ACF reload). Routed to the per-subscription
    /// callback as `Err(CaError::ServerError(eca_status))` so the
    /// subscriber surfaces the status instead of seeing the bogus
    /// zeroed payload that travels with the frame.
    #[error("server reported ECA status {0:#06x}")]
    ServerError(u32),

    /// Request cannot be framed for the peer: it needs the extended
    /// (24-byte) CA header, and the peer's protocol version predates
    /// CA_V49, or the element count exceeds what the peer can carry.
    /// libca raises this locally — `comQueSend::insertRequestHeader`
    /// throws `cacChannel::outOfBounds()` (`comQueSend.cpp:299,313`)
    /// and `ca_array_get`/`ca_array_put` return `ECA_TOLARGE` — so no
    /// byte reaches the wire.
    #[error("request too large for the peer's CA protocol version (ECA_TOLARGE)")]
    TooLarge,

    /// Element count out of bounds for the request C would build. libca's
    /// put path throws `cacChannel::outOfBounds()` for an array that cannot
    /// fit the peer's message-body limit (`comQueSend.cpp:361`) or that
    /// needs an extended header the peer cannot parse
    /// (`comQueSend.cpp:313`); `oldChannelNotify.cpp:309,378,453` map that
    /// to `ECA_BADCOUNT`. Raised locally — no byte reaches the wire.
    #[error("element count out of bounds for this CA circuit (ECA_BADCOUNT)")]
    BadCount,

    /// A get conversion returned a non-zero status: C
    /// `dbGetConvertRoutine`/`dbFastGetConvertRoutine` refusing to render a
    /// field in the requested DBR type, which for the `DBF_STRING` row means
    /// `epicsParse*` rejecting the stored text (`cvt_st_d`,
    /// `dbFastLinkConv.c:233-244`). `dbChannel_get` turns it into -1
    /// (`db_access.c:816`) and rsrv answers the read with a ZEROED payload and
    /// `m_cid = ECA_GETFAIL` (`camessage.c:545-561`) rather than a value.
    /// Distinct from [`Self::InvalidValue`], which the put direction raises and
    /// rsrv answers `ECA_PUTFAIL`.
    #[error("get conversion failed: {0}")]
    GetConvertFailed(String),
}

// ECA status constants (originally from protocol.rs, now in epics-ca-rs)
const ECA_TIMEOUT: u32 = 80; // defmsg(CA_K_WARNING, 10)
const ECA_NOWTACCESS: u32 = 376; // defmsg(CA_K_WARNING, 47)
const ECA_PUTFAIL: u32 = 160; // defmsg(CA_K_WARNING, 20)
const ECA_BADTYPE: u32 = 114; // defmsg(CA_K_ERROR, 14)
const ECA_DISCONN: u32 = 192; // defmsg(CA_K_WARNING, 24)
const ECA_TOLARGE: u32 = 72; // defmsg(CA_K_WARNING, 9)
const ECA_BADCOUNT: u32 = 176; // defmsg(CA_K_WARNING, 22)
const ECA_GETFAIL: u32 = 152; // defmsg(CA_K_WARNING, 19)

/// Which CA operation failed — C's `read_action` or `write_action`
/// (`rsrv/camessage.c`).
///
/// C never lets the error KIND choose the status once a request has reached
/// the database: `read_action` answers a negative `dbChannel_get` with
/// `ECA_GETFAIL` (`camessage.c:647-651`) and `write_action` answers a negative
/// `dbChannel_put` with `ECA_PUTFAIL` (`camessage.c:781-789`), throwing the
/// `dbStatus` away in both. The same underlying failure therefore has two
/// correct answers, one per direction, and a mapping that sees only the error
/// cannot pick between them. Naming the direction at the call is what makes a
/// read unable to reach a put status: no arm reachable under
/// [`CaOp::Read`] yields `ECA_PUTFAIL`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CaOp {
    /// C `read_action` — `ca_get`, `ca_array_get_callback`, a monitor update.
    Read,
    /// C `write_action` / `write_notify_action` — `ca_put`, `ca_put_callback`.
    Write,
}

impl CaOp {
    /// The status C's action reports once the DATABASE has refused, whatever
    /// the underlying `dbStatus` was.
    const fn failed(self) -> u32 {
        match self {
            CaOp::Read => ECA_GETFAIL,
            CaOp::Write => ECA_PUTFAIL,
        }
    }
}

impl CaError {
    /// The ECA status a CA CLIENT reports for this error on `op`.
    ///
    /// Layered as C's `read_action`/`write_action` are. A status the error
    /// already carries, or that a gate ABOVE the database produced, is the
    /// same word whichever way the request was going; everything the database
    /// itself refused is decided by `op` alone, because that is the point at
    /// which C stops looking at the status.
    ///
    /// This is the client-side table, which sees libca's local statuses too.
    /// The server's reply table is `PutStatus::of_failure`
    /// (`epics-ca-rs/src/server/tcp.rs`) and is deliberately narrower: by the
    /// time rsrv reaches `dbChannel_put`, the gates above it have already
    /// answered, so every error left there is a database refusal.
    pub fn to_eca_status(&self, op: CaOp) -> u32 {
        match self {
            // Raised by libca locally, before any database is reached — the
            // request never became a read or a write.
            CaError::Timeout => ECA_TIMEOUT,
            CaError::TooLarge => ECA_TOLARGE,
            CaError::BadCount => ECA_BADCOUNT,
            // C's DBR-type gates: `INVALID_DB_REQ` above the read
            // (`camessage.c:616-620`) and `caNetConvert` on both sides. They
            // run before the database is touched and answer ECA_BADTYPE
            // either way.
            CaError::TypeMismatch(_) | CaError::UnsupportedType(_) => ECA_BADTYPE,
            // Disconnection / shutdown are surfaced as ECA_DISCONN so a
            // downstream client (e.g. caput on a CA gateway whose upstream
            // just dropped) sees the actionable "CA channel disconnected"
            // message rather than a request-failed status. I/O errors usually
            // mean the circuit is wedged and read the same way.
            CaError::Disconnected | CaError::Shutdown | CaError::Io(_) => ECA_DISCONN,
            // Already an ECA status, decided by the peer or by libca.
            // Re-deriving it would discard what was actually said.
            CaError::WriteFailed(code) | CaError::ServerError(code) => *code,
            // C's `rsrvCheckPut` gate, above the put (`camessage.c:741-751`).
            // Its read-side twin ECA_NORDACCESS has no variant of its own —
            // that one arrives from the wire as `ServerError`.
            CaError::ReadOnlyField(_) => ECA_NOWTACCESS,
            // Everything else is the database refusing: a value the field's
            // converter rejected, a menu string naming no choice, a link field
            // a get cannot render, a record-side veto. C answers all of them
            // by direction, so listing any of them here would only be a way to
            // get one of the two directions wrong.
            _ => op.failed(),
        }
    }
}

pub type CaResult<T> = Result<T, CaError>;
