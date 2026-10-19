use thiserror::Error;

/// One `yyerrorAbort` from the `.db` lexer, in either start condition
/// (`dbLex.l:129-155`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonLexFault {
    /// `Invalid character '%c'`; carries the character.
    Invalid(u8),
    /// `Bad character in JSON string`; carries the whole string, quotes included.
    BadString(String),
    /// `Newline in string, closing quote missing` (`dbLex.l:131-133`), an
    /// `INITIAL`-state rule; carries the text through the newline it consumed.
    NewlineInString(String),
}

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
    /// `DBF_MENU` or `DBF_ENUM` field named neither an exact choice label nor
    /// an in-range index (`dbConvert.c::putStringMenu:1216-1229`). C's converter returns
    /// this from inside `dbPut`, *before* the value is stored
    /// (`dbAccess.c:1362`), so the field keeps its old value, no monitor is
    /// posted and the record is not processed.
    #[error("illegal menu choice: {0}")]
    BadChoice(ChoiceRefusal),

    /// C `S_dbLib_badField` from `dbParseLink` — brace-delimited link text
    /// `dbJLinkParse` refused (`dbStaticLib.c:2281-2283`). The field keeps
    /// its old link.
    #[error("invalid value: {0}")]
    BadLink(LinkRefusal),

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

    /// C `S_db_noSupport` ("RSET or DSXT routine not defined") — a runtime
    /// device-link put whose device support has no extension table.
    #[error("RSET or DSXT routine not defined")]
    NoSupport,

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

    /// A put that landed and then processed the record, whose `dbProcess`
    /// returned non-zero (`dbPutField`, `dbAccess.c:1279`). The value stays
    /// written and the cycle ran; only the status is the failure. Every plain
    /// put reports it — `dbChannel_put` folds any non-zero status to -1 for
    /// rsrv (`db_access.c:980`), pvxs throws on it (`iocsource.cpp:420`) —
    /// and no put-callback does: `dbNotify` never reads the process status.
    #[error("{0}")]
    ProcessStatus(DbStatus),
}

/// Lets a put's settle path take a bare [`CaError`] or one carried by a
/// refusal (`PutRefused`).
impl AsRef<CaError> for CaError {
    fn as_ref(&self) -> &CaError {
        self
    }
}

impl CaError {
    /// A process frame's error as a put-callback sees it: `dbNotify` never
    /// reads `dbProcess`'s status (`dbNotify.c:261`), so it is no error.
    pub fn unless_process_status(self) -> CaResult<()> {
        match self {
            Self::ProcessStatus(_) => Ok(()),
            e => Err(e),
        }
    }
}

/// A non-zero C `long` status a record's `process()` returned. A `Copy` word,
/// so the cycle can carry it without allocating; its text is rendered only
/// where a server reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DbStatus(std::num::NonZeroI32);

/// C's `long status` on the cycle path: `Ok` is 0.
pub type DbResult = Result<(), DbStatus>;

impl DbStatus {
    /// The bare `-1` `dbCa`, pvalink and most device support return.
    pub const FAILED: Self = Self::of(-1);
    /// `S_db_noLSET`: a put through a link with no lset entry.
    pub const NO_LSET: Self = Self::of((511 << 16) | 21);
    /// `S_db_badDbrtype`.
    pub const BAD_DBRTYPE: Self = Self::of((511 << 16) | 3);
    /// `S_dev_missingSup`: a `process()` whose dset routine is absent.
    pub const MISSING_SUP: Self = Self::of((514 << 16) | 5);

    const fn of(status: i32) -> Self {
        match Self::new(status) {
            Some(s) => s,
            None => panic!("a status constant is non-zero"),
        }
    }

    /// The status a process frame's `Err` stands for: its own, or `-1` for a
    /// failure that never had one.
    pub fn of_error(e: &CaError) -> Self {
        match e {
            CaError::ProcessStatus(s) => *s,
            _ => Self::FAILED,
        }
    }

    /// `None` for 0, which is success and no status.
    #[must_use]
    pub const fn new(status: i32) -> Option<Self> {
        match std::num::NonZeroI32::new(status) {
            Some(s) => Some(Self(s)),
            None => None,
        }
    }

    #[must_use]
    pub const fn get(self) -> i32 {
        self.0.get()
    }

    /// C `errSymLookup` (`errSymLib.c:158-212`): a module below
    /// `MIN_MODULE_NUM` is an errno, a status in a linked symbol table is its
    /// message, anything else `errRawCopy`'s numbers — so the `-1` a soft
    /// device support returns reads "Error (65535,65535)".
    #[must_use]
    pub fn text(self) -> std::borrow::Cow<'static, str> {
        const MIN_MODULE_NUM: u32 = 501;
        let status = self.get();
        let modnum = (status as u32 >> 16) & 0xffff;
        let errnum = status as u32 & 0xffff;
        if modnum < MIN_MODULE_NUM {
            let os = std::io::Error::from_raw_os_error(status).to_string();
            // Rust appends " (os error N)" to `strerror`'s text.
            return match os.rfind(" (os error ") {
                Some(at) => os[..at].to_string().into(),
                None => os.into(),
            };
        }
        if let Some(message) = symbol_message(modnum, errnum) {
            return message.into();
        }
        format!("Error ({modnum},{errnum})").into()
    }
}

impl std::fmt::Display for DbStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text())
    }
}

/// The symbol-table messages of the modules whose statuses reach a process
/// status: `dbAccessDefs.h:177-205` (`M_dbAccess`, 511) and
/// `epicsStdlib.h:43-51` (`M_stdlib`, 504). `M_devSup` has no table in an
/// IOC, which is why `S_dev_*` read as numbers.
const fn symbol_message(modnum: u32, errnum: u32) -> Option<&'static str> {
    Some(match (modnum, errnum) {
        (511, 1) => "Process Variable Not Found",
        (511, 3) => "Illegal Database Request Type",
        (511, 5) => "Attempt to modify noMod field",
        (511, 7) => "Illegal Lock Set",
        (511, 9) => "get precision failed",
        (511, 11) => "Only one element allowed",
        (511, 13) => "Illegal choice",
        (511, 15) => "Illegal field value",
        (511, 17) => "Logic error generating lock sets",
        (511, 21) => "No link support table or entry",
        (511, 31) => "missing record support entry table",
        (511, 33) => "RSET or DSXT routine not defined",
        (511, 35) => "Subroutine not found",
        (511, 37) => "Request is pending",
        (511, 39) => "Request is Blocked",
        (511, 41) => "putFields are disabled",
        (511, 43) => "Hardware link type not on INP/OUT",
        (511, 53) => "Breakpoint already set",
        (511, 55) => "No breakpoint set in record",
        (511, 57) => "Record not stopped",
        (511, 59) => "Error in argument",
        (511, 61) => "Logic error in breakpoint routine",
        (511, 63) => "Cannot spawn dbContTask",
        (511, 65) => "Cannot resume dbContTask",
        (511, 66) => "unable to allocate data structure from pool",
        (511, 67) => "Not initialized",
        (511, 68) => "Buffer full",
        (504, 1) => "No digits to convert",
        (504, 2) => "Extraneous characters",
        (504, 3) => "Too small to represent",
        (504, 4) => "Too large to represent",
        (504, 5) => "Number base not supported",
        _ => return None,
    })
}

/// The payload of [`CaError::BadChoice`], as data: the refusal is made under
/// the put's hold and its text is formatted only when it is printed or shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoiceRefusal(Choice);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Choice {
    /// `from` names none of `field`'s `count` choices and is no index below
    /// it, with the report C prints for it while that is still owed.
    String {
        field: ReportString,
        from: ReportString,
        count: u16,
        owed: Owed,
    },
    /// `index` is past `field`'s `count` choices.
    Index {
        field: ReportString,
        index: i64,
        count: u16,
    },
    Said(&'static str),
    /// A `.db` load's refusal, made with no hold.
    Text(String),
}

/// The report a [`Choice::String`] refusal still owes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Owed {
    Nothing,
    /// `cvt_st_e` / `putStringEnum`'s
    /// `recGblRecordError(S_db_badChoice, precord, from)`.
    RecordError,
    /// `cvt_st_menu`'s `recGblDbaddrError` (`dbFastLinkConv.c:314`).
    CvtStMenu,
    /// `cvt_st_device`'s `recGblDbaddrError` (`dbFastLinkConv.c:347`).
    CvtStDevice,
}

use crate::server::recgbl::ReportString;

impl ChoiceRefusal {
    /// A refusal from `cvt_st_e` / `putStringEnum`, which report it with
    /// `recGblRecordError(status, precord, from)` (`dbFastLinkConv.c:279`,
    /// `dbConvert.c:1189`).
    pub(crate) fn enum_state(field: &str, from: &str, count: usize) -> Self {
        Self(Choice::String {
            field: ReportString::new(field),
            from: ReportString::new(from),
            count: count as u16,
            owed: Owed::RecordError,
        })
    }

    /// `from` names no menu choice of `field` and is no index below `count`.
    /// `owed` is its converter's line; the loader's `dbPutStringNum` prints
    /// none.
    pub(crate) fn menu(field: &str, from: &str, count: usize, owed: Owed) -> Self {
        Self(Choice::String {
            field: ReportString::new(field),
            from: ReportString::new(from),
            count: count as u16,
            owed,
        })
    }

    /// `index` is past `field`'s `count` menu choices.
    pub(crate) fn index(field: &str, index: i64, count: usize) -> Self {
        Self(Choice::Index {
            field: ReportString::new(field),
            index,
            count: count as u16,
        })
    }

    /// Move out the report C prints with this refusal, for the put's owner.
    pub(crate) fn take_report(&mut self) -> Option<crate::server::recgbl::PutReport> {
        use crate::server::recgbl::PutReport;
        match &mut self.0 {
            Choice::String { from, owed, .. } => {
                let report = match *owed {
                    Owed::Nothing => None,
                    Owed::RecordError => Some(PutReport::IllegalChoice { from: *from }),
                    Owed::CvtStMenu => Some(PutReport::DbaddrError {
                        status: "Illegal choice",
                        message: "dbFastLinkConv(cvt_st_menu)",
                    }),
                    Owed::CvtStDevice => Some(PutReport::DbaddrError {
                        status: "Illegal choice",
                        message: "dbFastLinkConv(cvt_st_device)",
                    }),
                };
                *owed = Owed::Nothing;
                report
            }
            _ => None,
        }
    }
}

/// The payload of [`CaError::BadLink`]: why `dbParseLink` refused the text,
/// and whether the line `dbJLinkInit` prints while it parses
/// (`dbJLink.c:262-268`) is still owed. Carried out of the hold as data, like
/// [`ChoiceRefusal`]'s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkRefusal {
    why: LinkWhy,
    owed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkWhy {
    /// A link type C registers and this port does not serve. C prints
    /// nothing of its own here.
    Unported(ReportString),
    /// `dbJLinkInit: Link type '%s' not found`.
    NotFound(ReportString),
    /// `dbJLinkInit: Support for Link type '%s' not loaded`: declared by a
    /// loaded `link()` line, bound to no support.
    NotLoaded(ReportString),
    /// `dbJLinkInit: Illegal second link key '%s'`.
    SecondKey(ReportString),
    /// A `/*` with no `*/`, at this byte.
    UnterminatedComment(usize),
    NoTypeKey,
    /// A served type's reader refused its value.
    ValueRejected(ReportString),
    /// `lnkConst` stopped the parse, printing what it said if anything.
    ConstStop(Option<&'static str>),
    /// `alloc_jlink` refused the field's link type: what the link type said,
    /// then `dbJLinkInit: Link type '%s' allocation failed. `.
    AllocFailed {
        key: &'static str,
        said: &'static str,
    },
    /// `lnkCalc`'s callbacks stopped the parse, saying why.
    CalcStop(CalcSaid),
    /// yajl's verbose error block.
    Yajl(crate::server::db_convert_json::YajlError),
    /// A `#` link whose letters name no bus.
    NoBus,
    /// This dset's `add_record` refused the new link; its line goes out
    /// through the put's own report.
    AddRecord(&'static str),
    /// This dset's `del_record` refused to let the old link go.
    DelRecord(&'static str),
    /// `dbPutFieldLink`'s `S_db_badHWaddr` (`dbAccess.c:1171-1174`): this
    /// field's link was removed by an earlier refused put, and C says
    /// nothing.
    NotDefined(&'static str),
}

/// What `lnkCalc`'s parse callbacks print before they stop
/// (`lnkCalc.c:122-370`), each after `lnkCalc: `.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CalcSaid {
    TooManyArgs,
    UnexpectedInteger(i64),
    /// `%g` of the value.
    UnexpectedDouble(ReportString),
    BadTime(ReportString),
    /// `sCalcErrorStr`'s text.
    BadExpr(&'static str),
    UnexpectedString(ReportString),
    UnexpectedMap,
    UnknownKey(ReportString),
    NoExpr,
    NoOut,
    UnexpectedArray,
    UnexpectedChild(i32),
}

impl std::fmt::Display for CalcSaid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyArgs => write!(
                f,
                "Too many input args, limit is {}",
                crate::calc::CALC_NARGS
            ),
            Self::UnexpectedInteger(value) => write!(f, "Unexpected integer {value}"),
            Self::UnexpectedDouble(g) => write!(f, "Unexpected double {}", g.as_str()),
            Self::BadTime(text) => write!(f, "Bad 'time' parameter \"{}\"", text.as_str()),
            Self::BadExpr(why) => write!(f, "Error in calc expression, {why}"),
            Self::UnexpectedString(text) => write!(f, "Unexpected string \"{}\"", text.as_str()),
            Self::UnexpectedMap => f.write_str("Unexpected map"),
            Self::UnknownKey(key) => write!(f, "Unknown key \"{}\"", key.as_str()),
            Self::NoExpr => f.write_str("No expression ('expr' key)"),
            Self::NoOut => f.write_str("No output link ('out' key)"),
            Self::UnexpectedArray => f.write_str("Unexpected array"),
            Self::UnexpectedChild(state) => {
                write!(f, "Unexpected child link, parser state = {state}")
            }
        }
    }
}

impl LinkRefusal {
    pub(crate) fn new(why: LinkWhy) -> Self {
        let owed = matches!(
            why,
            LinkWhy::NotFound(_)
                | LinkWhy::NotLoaded(_)
                | LinkWhy::SecondKey(_)
                | LinkWhy::ConstStop(Some(_))
                | LinkWhy::AllocFailed { .. }
                | LinkWhy::CalcStop(_)
                | LinkWhy::Yajl(_)
        );
        Self { why, owed }
    }

    /// A dset's `add_record` refused the link, printing nothing of its own
    /// or having printed it already.
    pub fn add_record(dset: &'static str) -> Self {
        Self::new(LinkWhy::AddRecord(dset))
    }

    /// A dset's `del_record` refused to release the old link, printing
    /// nothing of its own or having printed it already.
    pub fn del_record(dset: &'static str) -> Self {
        Self::new(LinkWhy::DelRecord(dset))
    }

    /// The errlog line still owed for this refusal, if C prints one.
    pub fn report(&self) -> Option<String> {
        if !self.owed {
            return None;
        }
        Some(match self.why {
            LinkWhy::ConstStop(Some(said)) => said.to_string(),
            LinkWhy::AllocFailed { key, said } => {
                format!("{said}\ndbJLinkInit: Link type '{key}' allocation failed. \n")
            }
            LinkWhy::Yajl(e) => format!("dbJLinkInit: {e}\n"),
            _ => format!("{self}\n"),
        })
    }

    /// Print [`Self::report`], with the hold down.
    pub(crate) fn emit(&self) {
        if let Some(line) = self.report() {
            crate::runtime::log::errlog_printf(&line);
        }
    }
}

impl std::fmt::Display for LinkRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.why {
            LinkWhy::Unported(key) => write!(
                f,
                "dbJLinkInit: Link type '{}' is registered by epics-base but not \
                 implemented by this port",
                key.as_str()
            ),
            LinkWhy::NotFound(key) => {
                write!(f, "dbJLinkInit: Link type '{}' not found", key.as_str())
            }
            LinkWhy::NotLoaded(key) => write!(
                f,
                "dbJLinkInit: Support for Link type '{}' not loaded",
                key.as_str()
            ),
            LinkWhy::SecondKey(key) => {
                write!(f, "dbJLinkInit: Illegal second link key '{}'", key.as_str())
            }
            LinkWhy::UnterminatedComment(offset) => write!(
                f,
                "dbJLinkInit: not a JSON link — unterminated block comment '/*' at byte {offset}"
            ),
            LinkWhy::NoTypeKey => f.write_str("dbJLinkInit: not a JSON link — no link type key"),
            LinkWhy::ValueRejected(key) => write!(
                f,
                "dbJLinkInit: Link type '{}' rejected its value",
                key.as_str()
            ),
            LinkWhy::ConstStop(_) => f.write_str("dbJLinkInit: lnkConst refused its value"),
            LinkWhy::AllocFailed { key, .. } => {
                write!(f, "dbJLinkInit: Link type '{key}' allocation failed.")
            }
            LinkWhy::CalcStop(said) => write!(f, "lnkCalc: {said}"),
            LinkWhy::Yajl(_) => f.write_str("dbJLinkInit: not a JSON link"),
            LinkWhy::NoBus => f.write_str("dbParseLink: the text names no hardware bus"),
            LinkWhy::AddRecord(dset) => write!(f, "{dset} (add_record) refused the link"),
            LinkWhy::DelRecord(dset) => write!(f, "{dset} (del_record) kept the link"),
            LinkWhy::NotDefined(field) => write!(f, "{field} has no link to remove"),
        }
    }
}

impl CaError {
    /// Emit the errlog line a refused put still owes ([`LinkRefusal`]). Called
    /// by a put entry once its gate is down.
    pub(crate) fn emit_put_report(&self) {
        if let CaError::BadLink(refusal) = self {
            refusal.emit();
        }
    }

    /// Move out what [`Self::emit_put_report`] would print, for a put whose
    /// error is handed back before its gate drops.
    pub(crate) fn take_put_report(&mut self) -> Option<LinkRefusal> {
        match self {
            CaError::BadLink(refusal) if refusal.owed => {
                refusal.owed = false;
                Some(LinkRefusal {
                    owed: true,
                    ..*refusal
                })
            }
            _ => None,
        }
    }
}

impl From<String> for ChoiceRefusal {
    fn from(detail: String) -> Self {
        Self(Choice::Text(detail))
    }
}

impl From<&'static str> for ChoiceRefusal {
    fn from(said: &'static str) -> Self {
        Self(Choice::Said(said))
    }
}

impl std::fmt::Display for ChoiceRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Choice::String {
                field, from, count, ..
            } => write!(
                f,
                "{}: '{}' is no choice and no index below {count}",
                field.as_str(),
                from.as_str()
            ),
            Choice::Index {
                field,
                index,
                count,
            } => write!(f, "{}: {index} is no index below {count}", field.as_str()),
            Choice::Said(said) => f.write_str(said),
            Choice::Text(text) => f.write_str(text),
        }
    }
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

#[cfg(test)]
mod db_status_tests {
    use super::DbStatus;

    fn text(status: i32) -> String {
        DbStatus::new(status).expect("non-zero").text().into_owned()
    }

    /// One case per `errSymLookup` arm. The `-1` text is what a pvxs client
    /// prints for a soft ao whose CA OUT link is down (measured on
    /// softIocPVX: "Error (65535,65535)").
    #[test]
    fn text_is_err_sym_lookup() {
        assert!(DbStatus::new(0).is_none());
        assert_eq!(text(-1), "Error (65535,65535)");
        // S_dev_missingSup: M_devSup has no table.
        assert_eq!(text((514 << 16) | 5), "Error (514,5)");
        // S_db_badField and S_stdlib_overflow: in the table.
        assert_eq!(text((511 << 16) | 15), "Illegal field value");
        assert_eq!(text((504 << 16) | 4), "Too large to represent");
        // An unlisted dbAccess number falls back to the numbers.
        assert_eq!(text((511 << 16) | 2), "Error (511,2)");
        #[cfg(unix)]
        assert_eq!(text(libc::ENOENT), "No such file or directory");
    }
}
