use epics_base_rs::error::{CaError, CaResult};

// CA protocol command codes
pub const CA_PROTO_VERSION: u16 = 0;
pub const CA_PROTO_EVENT_ADD: u16 = 1;
pub const CA_PROTO_EVENT_CANCEL: u16 = 2;
pub const CA_PROTO_SEARCH: u16 = 6;
pub const CA_PROTO_NOT_FOUND: u16 = 14;
pub const CA_PROTO_READ_NOTIFY: u16 = 15;
pub const CA_PROTO_CREATE_CHAN: u16 = 18;
pub const CA_PROTO_WRITE_NOTIFY: u16 = 19;
pub const CA_PROTO_HOST_NAME: u16 = 21;
pub const CA_PROTO_CLIENT_NAME: u16 = 20;
pub const CA_PROTO_ACCESS_RIGHTS: u16 = 22;
pub const CA_PROTO_ECHO: u16 = 23;
pub const CA_PROTO_REPEATER_CONFIRM: u16 = 17;
pub const CA_PROTO_REPEATER_REGISTER: u16 = 24;
pub const CA_PROTO_CLEAR_CHANNEL: u16 = 12;
pub const CA_PROTO_RSRV_IS_UP: u16 = 13;
pub const CA_PROTO_SERVER_DISCONN: u16 = 27;
pub const CA_PROTO_READ: u16 = 3; // deprecated but exists in spec
pub const CA_PROTO_WRITE: u16 = 4; // fire-and-forget write
pub const CA_PROTO_EVENTS_OFF: u16 = 8;
pub const CA_PROTO_EVENTS_ON: u16 = 9;
pub const CA_PROTO_READ_SYNC: u16 = 10; // legacy echo (used by older clients)
pub const CA_PROTO_ERROR: u16 = 11;
pub const CA_PROTO_CREATE_CH_FAIL: u16 = 26;

// Ports — re-exported from the one owner, `epics-base-rs`, which const-derives
// them from the generated `ENV_PARAM` table (`configure/CONFIG_ENV`).
pub use epics_base_rs::runtime::net::{CA_REPEATER_PORT, CA_SERVER_PORT};

/// Resolved CA repeater UDP port. Mirrors libca
/// `envGetInetPortConfigParam(&EPICS_CA_REPEATER_PORT, …)` (e.g.
/// `repeater.cpp:511`, `udpiiu.cpp:168`, `casw.cpp:103`) by delegating to
/// the one owner of that C function,
/// [`epics_base_rs::runtime::net::ca_repeater_port`]: the env var takes
/// precedence, a value that fails to parse or falls outside
/// `(IPPORT_USERRESERVED, USHRT_MAX]` is diagnosed and rejected back to
/// the compiled default [`CA_REPEATER_PORT`] (5065). Centralizing this
/// keeps the repeater daemon bind, the client REGISTER target, and
/// the beacon-monitor REGISTER target in lockstep with operator env.
pub fn repeater_port() -> u16 {
    epics_base_rs::runtime::net::ca_repeater_port()
}

// CA protocol version
/// C `CA_MAJOR_PROTOCOL_REVISION` (`caProto.h:30`).
pub const CA_MAJOR_VERSION: u16 = 4;
/// C `CA_MINOR_PROTOCOL_REVISION` (`caProto.h:31`).
pub const CA_MINOR_VERSION: u16 = 13;

/// C `ca_version()` (`access.cpp`, declared `cadef.h`) — the wire protocol
/// revision this library speaks, `"<major>.<minor>"`, and nothing else. It is a
/// libca API a caller may print or compare, so it says what C's says.
pub fn ca_version() -> String {
    format!("{CA_MAJOR_VERSION}.{CA_MINOR_VERSION}")
}

/// The `-V` banner, shared by every CA tool: C prints it from one `printf`
/// (`caget.c:404`, and the identical line in `caput.c`/`camonitor.c`/`cainfo.c`)
///
/// ```text
/// \nEPICS Version %s, CA Protocol version %s\n   EPICS_VERSION_STRING, ca_version()
/// ```
///
/// The EPICS Base version comes from the generated
/// [`epics_base_rs::runtime::version`], so a base bump moves this line without
/// anyone editing four binaries. It deliberately does NOT carry the `epics-ca-rs`
/// crate version: `-V` is a parsed interop surface, and a script that reads the
/// base release out of it must get the base release.
pub fn version_info() -> String {
    format!(
        "\nEPICS Version {}, CA Protocol version {}",
        epics_base_rs::runtime::version::EPICS_VERSION_STRING,
        ca_version()
    )
}

// Monitor masks
pub const DBE_VALUE: u16 = 1;
pub const DBE_LOG: u16 = 2;
pub const DBE_ALARM: u16 = 4;
pub const DBE_PROPERTY: u16 = 8;

// Reply flags
pub const CA_DO_REPLY: u16 = 10;
/// C `caProto.h`: `DONTREPLY = 5u`. Used by libca search requests to
/// suppress per-request NOT_FOUND replies.
pub const CA_DONT_REPLY: u16 = 5;

// ECA status codes — DEFMSG(severity, msg_no) encoding per caerr.h.
// Values match epics-base verbatim so the wire protocol is interoperable.
pub const CA_K_INFO: u32 = 3;
pub const CA_K_ERROR: u32 = 2;
pub const CA_K_SUCCESS: u32 = 1;
pub const CA_K_WARNING: u32 = 0;
pub const CA_K_SEVERE: u32 = 4;
pub const CA_K_FATAL: u32 = CA_K_ERROR | CA_K_SEVERE; // 6

pub const fn defmsg(sev: u32, num: u32) -> u32 {
    ((num << 3) & 0x0000FFF8) | (sev & 0x00000007)
}

// Full ECA table — see caerr.h for canonical definitions.
pub const ECA_NORMAL: u32 = defmsg(CA_K_SUCCESS, 0);
pub const ECA_MAXIOC: u32 = defmsg(CA_K_ERROR, 1);
pub const ECA_UKNHOST: u32 = defmsg(CA_K_ERROR, 2);
pub const ECA_UKNSERV: u32 = defmsg(CA_K_ERROR, 3);
pub const ECA_SOCK: u32 = defmsg(CA_K_ERROR, 4);
pub const ECA_CONN: u32 = defmsg(CA_K_WARNING, 5);
pub const ECA_ALLOCMEM: u32 = defmsg(CA_K_WARNING, 6);
pub const ECA_UKNCHAN: u32 = defmsg(CA_K_WARNING, 7);
pub const ECA_UKNFIELD: u32 = defmsg(CA_K_WARNING, 8);
pub const ECA_TOLARGE: u32 = defmsg(CA_K_WARNING, 9);
pub const ECA_TIMEOUT: u32 = defmsg(CA_K_WARNING, 10);
pub const ECA_NOSUPPORT: u32 = defmsg(CA_K_WARNING, 11);
pub const ECA_STRTOBIG: u32 = defmsg(CA_K_WARNING, 12);
pub const ECA_DISCONNCHID: u32 = defmsg(CA_K_ERROR, 13);
pub const ECA_BADTYPE: u32 = defmsg(CA_K_ERROR, 14);
pub const ECA_CHIDNOTFND: u32 = defmsg(CA_K_INFO, 15);
pub const ECA_CHIDRETRY: u32 = defmsg(CA_K_INFO, 16);
pub const ECA_INTERNAL: u32 = defmsg(CA_K_FATAL, 17);
pub const ECA_DBLCLFAIL: u32 = defmsg(CA_K_WARNING, 18);
pub const ECA_GETFAIL: u32 = defmsg(CA_K_WARNING, 19);
pub const ECA_PUTFAIL: u32 = defmsg(CA_K_WARNING, 20);
pub const ECA_ADDFAIL: u32 = defmsg(CA_K_WARNING, 21);
pub const ECA_BADCOUNT: u32 = defmsg(CA_K_WARNING, 22);
pub const ECA_BADSTR: u32 = defmsg(CA_K_ERROR, 23);
pub const ECA_DISCONN: u32 = defmsg(CA_K_WARNING, 24);
pub const ECA_DBLCHNL: u32 = defmsg(CA_K_WARNING, 25);
pub const ECA_EVDISALLOW: u32 = defmsg(CA_K_ERROR, 26);
pub const ECA_BUILDGET: u32 = defmsg(CA_K_WARNING, 27);
pub const ECA_NEEDSFP: u32 = defmsg(CA_K_WARNING, 28);
pub const ECA_OVEVFAIL: u32 = defmsg(CA_K_WARNING, 29);
pub const ECA_BADMONID: u32 = defmsg(CA_K_ERROR, 30);
pub const ECA_NEWADDR: u32 = defmsg(CA_K_WARNING, 31);
pub const ECA_NEWCONN: u32 = defmsg(CA_K_INFO, 32);
pub const ECA_NOCACTX: u32 = defmsg(CA_K_WARNING, 33);
pub const ECA_DEFUNCT: u32 = defmsg(CA_K_FATAL, 34);
pub const ECA_EMPTYSTR: u32 = defmsg(CA_K_WARNING, 35);
pub const ECA_NOREPEATER: u32 = defmsg(CA_K_WARNING, 36);
pub const ECA_NOCHANMSG: u32 = defmsg(CA_K_WARNING, 37);
pub const ECA_DLCKREST: u32 = defmsg(CA_K_WARNING, 38);
pub const ECA_SERVBEHIND: u32 = defmsg(CA_K_WARNING, 39);
pub const ECA_NOCAST: u32 = defmsg(CA_K_WARNING, 40);
pub const ECA_BADMASK: u32 = defmsg(CA_K_ERROR, 41);
pub const ECA_IODONE: u32 = defmsg(CA_K_INFO, 42);
pub const ECA_IOINPROGRESS: u32 = defmsg(CA_K_INFO, 43);
pub const ECA_BADSYNCGRP: u32 = defmsg(CA_K_ERROR, 44);
pub const ECA_PUTCBINPROG: u32 = defmsg(CA_K_ERROR, 45);
pub const ECA_NORDACCESS: u32 = defmsg(CA_K_WARNING, 46);
pub const ECA_NOWTACCESS: u32 = defmsg(CA_K_WARNING, 47);
pub const ECA_ANACHRONISM: u32 = defmsg(CA_K_ERROR, 48);
pub const ECA_NOSEARCHADDR: u32 = defmsg(CA_K_WARNING, 49);
pub const ECA_NOCONVERT: u32 = defmsg(CA_K_WARNING, 50);
pub const ECA_BADCHID: u32 = defmsg(CA_K_ERROR, 51);
pub const ECA_BADFUNCPTR: u32 = defmsg(CA_K_ERROR, 52);
pub const ECA_ISATTACHED: u32 = defmsg(CA_K_WARNING, 53);
pub const ECA_UNAVAILINSERV: u32 = defmsg(CA_K_WARNING, 54);
pub const ECA_CHANDESTROY: u32 = defmsg(CA_K_WARNING, 55);
pub const ECA_BADPRIORITY: u32 = defmsg(CA_K_ERROR, 56);
pub const ECA_NOTTHREADED: u32 = defmsg(CA_K_ERROR, 57);
pub const ECA_16KARRAYCLIENT: u32 = defmsg(CA_K_WARNING, 58);
pub const ECA_CONNSEQTMO: u32 = defmsg(CA_K_WARNING, 59);
pub const ECA_UNRESPTMO: u32 = defmsg(CA_K_WARNING, 60);

/// Extract the message number (caerr.h MSG_NO_OF_STATUS).
pub const fn eca_msg_no(status: u32) -> u32 {
    (status >> 3) & 0x1FFF
}

/// Extract severity bits (caerr.h SEVERITY_OF_STATUS).
pub const fn eca_severity(status: u32) -> u32 {
    status & 0x7
}

/// Human-readable text for an ECA status, mirroring libca `ca_message`.
pub fn eca_message(status: u32) -> &'static str {
    let msg_no = eca_msg_no(status) as usize;
    ECA_MESSAGE_TEXT
        .get(msg_no)
        .copied()
        .unwrap_or("Unknown ECA status")
}

/// Strings copied verbatim from `epics-base/modules/ca/src/client/access.cpp`
/// `ca_message_text[]`.
pub const ECA_MESSAGE_TEXT: &[&str] = &[
    "Normal successful completion",
    "Maximum simultaneous IOC connections exceeded",
    "Unknown internet host",
    "Unknown internet service",
    "Unable to allocate a new socket",
    "Unable to connect to internet host or service",
    "Unable to allocate additional dynamic memory",
    "Unknown IO channel",
    "Record field specified inappropriate for channel specified",
    "The requested data transfer is greater than available memory or EPICS_CA_MAX_ARRAY_BYTES",
    "User specified timeout on IO operation expired",
    "Sorry, that feature is planned but not supported at this time",
    "The supplied string is unusually large",
    "The request was ignored because the specified channel is disconnected",
    "The data type specified is invalid",
    "Remote Channel not found",
    "Unable to locate all user specified channels",
    "Channel Access Internal Failure",
    "The requested local DB operation failed",
    "Channel read request failed",
    "Channel write request failed",
    "Channel subscription request failed",
    "Invalid element count requested",
    "Invalid string",
    "Virtual circuit disconnect",
    "Identical process variable names on multiple servers",
    "Request inappropriate within subscription (monitor) update callback",
    "Database value get for that channel failed during channel search",
    "Unable to initialize without the vxWorks VX_FP_TASK task option set",
    "Event queue overflow has prevented first pass event after event add",
    "Bad event subscription (monitor) identifier",
    "Remote channel has new network address",
    "New or resumed network connection",
    "Specified task isn't a member of a CA context",
    "Attempt to use defunct CA feature failed",
    "The supplied string is empty",
    "Unable to spawn the CA repeater thread- auto reconnect will fail",
    "No channel id match for search reply- search reply ignored",
    "Resetting dead connection- will try to reconnect",
    "Server (IOC) has fallen behind or is not responding- still waiting",
    "No internet interface with broadcast available",
    "Invalid event selection mask",
    "IO operations have completed",
    "IO operations are in progress",
    "Invalid synchronous group identifier",
    "Put callback timed out",
    "Read access denied",
    "Write access denied",
    "Requested feature is no longer supported",
    "Empty PV search address list",
    "No reasonable data conversion between client and server types",
    "Invalid channel identifier",
    "Invalid function pointer",
    "Thread is already attached to a client context",
    "Not supported by attached service",
    "User destroyed channel",
    "Invalid channel priority",
    "Preemptive callback not enabled - additional threads may not join context",
    "Client's protocol revision does not support transfers exceeding 16k bytes",
    "Virtual circuit connection sequence aborted",
    "Virtual circuit unresponsive",
];

/// The absolute ceiling on a single inbound CA message body, in bytes.
///
/// **Tier 2 deviation from C, and it is NOT `EPICS_CA_MAX_ARRAY_BYTES`.**
///
/// C has no such ceiling. With `EPICS_CA_AUTO_ARRAY_BYTES=YES` — the compiled
/// default — both peers grow their body cache to whatever the *sender* announced
/// in the header: `tcpiiu::processIncoming` `realloc`s to `((m_postsize-1)|0xfff)+1`
/// (`tcpiiu.cpp:1214-1225`) and `casExpandBuffer` does the same server-side
/// (`caservertask.c:1339-1348`). A peer that announces 4 GiB gets a 4 GiB
/// allocation attempt before a single body byte is read. This port refuses to
/// reproduce that: an unauthenticated allocation sized by a remote header is a
/// denial-of-service primitive, and "C does it" is not the contract.
///
/// The bound is a compile-time constant precisely so it stays independent of
/// `EPICS_CA_MAX_ARRAY_BYTES`. That variable means exactly one thing — the
/// operator's declared largest array, C's — and it is read in exactly one place
/// ([`max_array_bytes_buffer`]); it previously ALSO stood in as this cap with a
/// second, 1024x larger default, so one name carried two numbers.
pub const MAX_FRAME_BODY_BYTES: usize = 16 * 1024 * 1024;

/// C `MAX_TCP` (`modules/ca/src/client/caProto.h:67`) — `1024 * 16u`,
/// "so waveforms fit". The floor for `maxRecvBytesTCP` and the unit of
/// libca's receive-side buffer sizing.
pub const MAX_TCP: usize = 1024 * 16;

/// C `comBufSize` (`modules/ca/src/client/comBuf.h:37`) — the size of one
/// receive buffer, i.e. one "frame" for flow-control accounting.
pub const COM_BUF_SIZE: usize = 0x4000;

/// C `contiguousMsgCountWhichTriggersFlowControl`
/// (`modules/ca/src/client/iocinf.h:62`).
pub const CONTIGUOUS_FRAMES_TRIGGERING_FLOW_CONTROL: usize = 10;

/// The array buffer `EPICS_CA_MAX_ARRAY_BYTES` sizes — the ONE meaning that
/// variable has, on both the client and the server side of C.
///
/// C computes the identical value twice under two names, from the same
/// parameter: `cac::maxRecvBytesTCP` (`cac.cpp:196-217`) and
/// `rsrvSizeofLargeBufTCP` (`caservertask.c:510-531`). Room for the extended
/// header (`sizeof(caHdr) + 2 * sizeof(ca_uint32_t)` = 24) is added so the
/// operator gets the array size they asked for, and the result is floored at
/// [`MAX_TCP`] (C errlogs "was rounded up to %u"). A rejected value leaves the
/// compiled default — 16384 — standing, exactly as `envGetLongConfigParam`'s
/// failure branch does.
pub fn max_array_bytes_buffer() -> usize {
    /// `sizeof ( caHdr ) + 2 * sizeof ( ca_uint32_t )` (`cac.cpp:204`).
    const HEADER_SIZE: usize = 16 + EXTENDED_EXTRA;
    let max_bytes = epics_base_rs::runtime::env_table::EPICS_CA_MAX_ARRAY_BYTES.long_or_default();
    // `status || maxBytesAsALong < 0` (`caservertask.c:511`) — a negative value
    // is a failed fetch, and C falls back to MAX_TCP.
    let Ok(max_bytes) = usize::try_from(max_bytes) else {
        return MAX_TCP;
    };
    max_bytes.saturating_add(HEADER_SIZE).max(MAX_TCP)
}

/// C `EPICS_CA_AUTO_ARRAY_BYTES` (`configure/CONFIG_ENV:37`, compiled default
/// `YES` since 3.16).
///
/// C reads it with `envGetBoolConfigParam` (`envSubr.c:325-333`), which is
/// literally `epicsStrCaseCmp(text, "yes") == 0` — so ONLY the word "yes"
/// (any case) enables it. `EPICS_CA_AUTO_ARRAY_BYTES=1` disables it, quirk
/// included. `unwrap_or(true)` is C's own `if (envGetBoolConfigParam(...))
/// autoMaxBytes = 1;` (`caservertask.c:534-535`), not a second copy of the
/// table default.
pub fn auto_array_bytes() -> bool {
    epics_base_rs::runtime::env_table::EPICS_CA_AUTO_ARRAY_BYTES
        .bool()
        .unwrap_or(true)
}

/// The receive-side body limit of a CA circuit — `None` means "no limit",
/// which is C's DEFAULT.
///
/// `cac::cac` (`cac.cpp:223-232`) only builds `tcpLargeRecvBufFreeList` when
/// `EPICS_CA_AUTO_ARRAY_BYTES` is off. With it on (the default),
/// `tcpiiu::processIncoming` takes the `if (!tcpLargeRecvBufFreeList)` branch
/// (`tcpiiu.cpp:1214-1225`) and `malloc`/`realloc`s the body cache to
/// `((m_postsize-1)|0xfff)+1` — whatever the server announced, no cap.
///
/// With it off, the cache is capped at [`max_array_bytes_buffer`] and an
/// over-cap response is *ignored*, not fatal: C logs once and drains the
/// body with `recvQue.removeBytes` (`tcpiiu.cpp:1269-1283`), keeping the
/// circuit. A CA circuit is NEVER closed for an oversized payload.
pub fn max_recv_body_bytes() -> Option<usize> {
    if auto_array_bytes() {
        None
    } else {
        Some(max_array_bytes_buffer())
    }
}

/// The largest body this process will ALLOCATE for, whatever a peer announces.
///
/// With `EPICS_CA_AUTO_ARRAY_BYTES` off this is C's declared array bound. With
/// it on — C's default, where C is unbounded — it is [`MAX_FRAME_BODY_BYTES`],
/// the Tier 2 refusal to size an allocation from a remote header. Either way
/// it is one number with one meaning; see [`MAX_FRAME_BODY_BYTES`].
pub fn max_frame_body_bytes() -> usize {
    max_recv_body_bytes().unwrap_or(MAX_FRAME_BODY_BYTES)
}

/// C `cac::maxContiguousFrames` (`modules/ca/src/client/cac.cpp:233-237`,
/// read back at `cac.h:419`).
///
/// The number of consecutive receive frames that must each leave bytes
/// still pending in the OS socket buffer before libca declares the circuit
/// busy and asks the server for `CA_PROTO_EVENTS_OFF`. Scaled by how many
/// receive buffers one max-size array occupies, so a circuit configured for
/// large waveforms tolerates proportionally more contiguous frames before
/// tripping.
pub fn max_contiguous_frames() -> usize {
    let bufs_per_array = max_array_bytes_buffer() / COM_BUF_SIZE;
    if bufs_per_array > 1 {
        bufs_per_array * CONTIGUOUS_FRAMES_TRIGGERING_FLOW_CONTROL
    } else {
        CONTIGUOUS_FRAMES_TRIGGERING_FLOW_CONTROL
    }
}

/// Extra bytes consumed by extended header fields.
pub const EXTENDED_EXTRA: usize = 8;

/// C `CA_V49` (`caProto.h:47`) — the peer understands the 24-byte extended
/// header (`m_postsize == 0xffff` + two trailing u32s).
pub const fn ca_v49(minor: u16) -> bool {
    minor >= 9
}

/// C `CA_V413` (`caProto.h:48`) — "Allow zero length in requests." Before
/// this, a request with `m_count == 0` is a zero-element transfer, not a
/// request for the channel's native count.
pub const fn ca_v413(minor: u16) -> bool {
    minor >= 13
}

/// The frame needs the 24-byte extended header, but the peer negotiated a
/// pre-CA_V49 minor version and has no code to parse it. C never puts such a
/// frame on the wire: the client throws `cacChannel::outOfBounds`
/// (`comQueSend.cpp:313`) → `ECA_TOLARGE`, and the server returns
/// `ECA_16KARRAYCLIENT` (`caserverio.c:266-270`) → `send_err`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtendedHeaderUnsupported;

/// C `dbr_size[dataType]` and `dbr_value_size[dataType]` (`db_access.h`) for a
/// CA *request* data type. A DBR code encodes its own native carrier, so a
/// request needs no channel context to size: `dbr_size[t]` is the body of a
/// one-element transfer (metadata + one value) and `dbr_value_size[t]` is one
/// value. Out-of-range codes are C `INVALID_DB_REQ` → `cacChannel::badType`
/// → `ECA_BADTYPE`.
fn dbr_request_sizes(dbr_type: u16) -> CaResult<(u64, u64)> {
    use epics_base_rs::types::{DbFieldType, dbr_buffer_size};
    // The four non-array DBR codes have no `value[]` array, so the generic
    // `native = dbr_type % 7` decomposition does not apply; their table
    // entries are taken straight from `db_access.h`.
    let (dbr_size, value_size) = match dbr_type {
        0..=34 => {
            let native = DbFieldType::from_u16(dbr_type % 7)
                .map_err(|_| CaError::UnsupportedType(dbr_type))?;
            (dbr_buffer_size(dbr_type, native, 1), native.element_size())
        }
        // DBR_PUT_ACKT / DBR_PUT_ACKS: a bare `dbr_put_ackt_t` (u16).
        35 | 36 => (2, 2),
        // DBR_STSACK_STRING: status/severity/ackt/acks + one 40-byte string.
        37 => (48, 40),
        // DBR_CLASS_NAME: one 40-byte string, no metadata.
        38 => (40, 40),
        _ => return Err(CaError::UnsupportedType(dbr_type)),
    };
    Ok((dbr_size as u64, value_size.max(1) as u64))
}

/// Largest element count libca will *request* from a peer at `peer_minor`.
///
/// C `tcpiiu::readNotifyRequest` (`tcpiiu.cpp:1463-1473`) and
/// `tcpiiu::subscriptionRequest` (`tcpiiu.cpp:1574-1585`):
/// ```text
/// maxBytes = CA_V49(minor) ? 0xfffffff0 : MAX_TCP;
/// maxElem  = (maxBytes - dbr_size[type]) / dbr_value_size[type];
/// if (nElem > maxElem) throw cacChannel::msgBodyCacheTooSmall();  // ECA_TOLARGE
/// ```
/// `MAX_TCP` is 16 KiB (`caProto.h:67`), so on a pre-V49 circuit this bound is
/// what actually fires — long before the 0xffff header gate could.
fn max_read_elements(dbr_size: u64, value_size: u64, peer_minor: u16) -> u64 {
    let max_bytes: u64 = if ca_v49(peer_minor) {
        0xffff_fff0
    } else {
        MAX_TCP as u64
    };
    max_bytes.saturating_sub(dbr_size) / value_size
}

/// Wire element count for a `CA_PROTO_READ_NOTIFY` framed for `peer_minor`,
/// or `ECA_TOLARGE` when the request exceeds what the circuit can carry.
///
/// Mirrors C `tcpiiu::readNotifyRequest` (`tcpiiu.cpp:1455-1484`) in its
/// order: the element bound is checked against the count the CALLER asked
/// for, and only then is a zero substituted for a pre-V413 peer —
/// `if (nElem == 0 && !CA_V413(minor)) nElem = chan.getcount();`
/// (`tcpiiu.cpp:1476`). A zero request therefore always clears the bound,
/// even when the substituted native count would not; C frames it anyway, and
/// so do we.
///
/// The zero itself means "send whatever the record currently holds"
/// (autosize), a contract introduced in CA V4.13 (`caProto.h:48`, "Allow zero
/// length in requests"). A peer below V413 has no such code and would resolve
/// `m_count == 0` to a zero-element transfer, hence the substitution.
pub fn read_notify_wire_count(
    requested: u32,
    native: u32,
    dbr_type: u16,
    peer_minor: u16,
) -> CaResult<u32> {
    let (dbr_size, value_size) = dbr_request_sizes(dbr_type)?;
    if requested as u64 > max_read_elements(dbr_size, value_size, peer_minor) {
        return Err(CaError::TooLarge);
    }
    Ok(if requested == 0 && !ca_v413(peer_minor) {
        native
    } else {
        requested
    })
}

/// C `netSubscription::getCount` (`netIO.h:241-251`) — the cached
/// `netSubscription::count` is the USER's cap; the wire count is re-derived
/// from it per request:
/// `if ((count == 0 && !allow_zero) || count > nativeCount) return nativeCount;`
///
/// This is the whole of `tcpiiu::subscriptionCancelRequest`'s count logic
/// (`tcpiiu.cpp:1659`): a cancel re-resolves like an add, but has no element
/// bound — it carries no body.
pub fn subscription_cancel_wire_count(requested: u32, native: u32, peer_minor: u16) -> u32 {
    if (requested == 0 && !ca_v413(peer_minor)) || requested > native {
        native
    } else {
        requested
    }
}

/// Wire element count for a `CA_PROTO_EVENT_ADD` framed for `peer_minor`, or
/// `ECA_TOLARGE` when it exceeds what the circuit can carry.
///
/// Mirrors C `tcpiiu::subscriptionRequest` (`tcpiiu.cpp:1558-1585`), whose
/// order is the reverse of the read path's: `getCount` runs FIRST and the
/// element bound is applied to the RESOLVED count. So an autosize
/// subscription against a pre-V49 peer whose native count overflows `MAX_TCP`
/// is rejected here, where the same request as a read would be framed.
pub fn subscription_wire_count(
    requested: u32,
    native: u32,
    dbr_type: u16,
    peer_minor: u16,
) -> CaResult<u32> {
    let (dbr_size, value_size) = dbr_request_sizes(dbr_type)?;
    let count = subscription_cancel_wire_count(requested, native, peer_minor);
    if count as u64 > max_read_elements(dbr_size, value_size, peer_minor) {
        return Err(CaError::TooLarge);
    }
    Ok(count)
}

/// Everything C `comQueSend::insertRequestWithPayLoad` refuses before a put
/// is queued, in C's order: the DBR type first, then the array bound.
///
/// The type bound is `INVALID_DB_REQ` (`comQueSend.cpp:323`, with the
/// equivalent `dataType >= comQueSendCopyDispatchSize`=39 right behind it) →
/// `cacChannel::badType` → `ECA_BADTYPE` returned to the caller with nothing
/// on the wire. It sits ABOVE the scalar/array fork (`:330`), so a scalar put
/// is bounded by it too even though a scalar has no element bound. Resolving
/// the sizes is that check: `dbr_request_sizes` is the single owner of the
/// type bound, shared with [`read_notify_wire_count`] and
/// [`subscription_wire_count`], so no request path can frame a type the
/// others would reject.
///
/// The array bound is `comQueSend.cpp:352-364`, the put path's equivalent of
/// `max_read_elements`, with three differences that are C's, not ours:
/// ```text
/// maxBytes = v49Ok ? 0xffffffff : MAX_TCP - sizeof(caHdr);
/// maxElem  = (maxBytes - sizeof(dbr_double_t) - dbr_size[type])
///                / dbr_value_size[type];
/// if (nElem >= maxElem) throw cacChannel::outOfBounds();   // ECA_BADCOUNT
/// ```
/// — the header is subtracted, a `dbr_double_t` of slack is subtracted, the
/// comparison is `>=` not `>`, and the failure is `ECA_BADCOUNT` rather than
/// `ECA_TOLARGE`. It applies only to the array branch: `nElem == 1` takes the
/// scalar path (`comQueSend.cpp:330-352`), which has no element bound.
pub fn check_write_request(count: u32, dbr_type: u16, peer_minor: u16) -> CaResult<()> {
    let (dbr_size, value_size) = dbr_request_sizes(dbr_type)?;
    if count == 1 {
        return Ok(());
    }
    let max_bytes: u64 = if ca_v49(peer_minor) {
        0xffff_ffff
    } else {
        (MAX_TCP - 16) as u64
    };
    let max_elem = max_bytes
        .saturating_sub(8) // sizeof(dbr_double_t)
        .saturating_sub(dbr_size)
        / value_size;
    if count as u64 >= max_elem {
        return Err(CaError::BadCount);
    }
    Ok(())
}

/// `MAX_STRING_SIZE` (`db_access.h:34`) — the fixed width of a
/// `DBR_STRING` element, and the bound C checks a scalar string put
/// against (`comQueSend.cpp:335`).
pub const MAX_STRING_SIZE: usize = 40;

/// C `dbr_size[DBR_STRING]` is 40, but a *scalar* string put does not
/// send 40 bytes: `comQueSend::insertRequestWithPayLoad`
/// (`comQueSend.cpp:332-341`) frames only the NUL-terminated string.
///
/// ```text
/// size        = strlen(pStr) + 1u;                 // includes the NUL
/// if (size > MAX_STRING_SIZE) throw outOfBounds;   // -> ECA_BADCOUNT
/// payloadSize = CA_MESSAGE_ALIGN(size);            // round up to 8
/// pushString(pStr, size);                          // then zero padding
/// ```
///
/// So `caput PV abc` puts an 8-byte body, not a 40-byte one. Every
/// other shape — scalar non-string (`comQueSend.cpp:344-350`,
/// `dbr_size[type]`) and any array including a string array
/// (`comQueSend.cpp:366-376`, `dbr_size_n`) — is already the natural
/// serialized length, so only this one case contracts.
///
/// A payload with no NUL inside `MAX_STRING_SIZE` is C's `size > 40`
/// case: the string does not fit a `DBR_STRING` element, and C throws
/// `cacChannel::outOfBounds` → `ECA_BADCOUNT` (`oldChannelNotify.cpp:378`)
/// without a byte reaching the wire.
fn scalar_string_put_len(payload: &[u8]) -> CaResult<usize> {
    match payload.iter().take(MAX_STRING_SIZE).position(|&b| b == 0) {
        Some(nul) => Ok(nul + 1),
        // Shorter than an element and unterminated: `strlen` would run
        // to the end of the buffer, so C frames len+1 bytes.
        None if payload.len() < MAX_STRING_SIZE => Ok(payload.len() + 1),
        None => Err(CaError::BadCount),
    }
}

/// Build a client put frame (`CA_PROTO_WRITE` / `CA_PROTO_WRITE_NOTIFY`).
///
/// The single owner of client put framing — C's `comQueSend::
/// insertRequestWithPayLoad` (`comQueSend.cpp:318-383`). It decides the
/// on-the-wire body length (see `scalar_string_put_len`), pads to the
/// 8-byte message alignment C applies to every payload
/// (`CA_MESSAGE_ALIGN`, `caProto.h:158`), and picks the 16- or 24-byte
/// header form for the peer's protocol version.
///
/// `Err(CaError::BadCount)` means C would have thrown
/// `cacChannel::outOfBounds` before queueing: an over-long scalar
/// string, or a payload/count that needs the extended header on a
/// pre-V49 circuit (`comQueSend.cpp:313`).
pub fn build_put_frame(
    cmd: u16,
    sid: u32,
    data_type: u16,
    count: u32,
    ioid: Option<u32>,
    payload: Vec<u8>,
    peer_minor: u16,
) -> CaResult<Vec<u8>> {
    let mut body = payload;
    if count == 1 && data_type == epics_base_rs::types::DBR_STRING {
        body.truncate(scalar_string_put_len(&body)?);
    }
    body.resize(align8(body.len()), 0);

    let mut hdr = CaHeader::new(cmd);
    hdr.data_type = data_type;
    hdr.cid = sid;
    if let Some(ioid) = ioid {
        hdr.available = ioid;
    }
    hdr.set_payload_size(body.len(), count, peer_minor)
        .map_err(|_| CaError::BadCount)?;

    let mut frame = hdr.to_bytes_extended();
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// 16-byte CA message header (big-endian), with optional extended fields.
#[derive(Debug, Clone, Copy)]
pub struct CaHeader {
    pub cmmd: u16,
    pub postsize: u16,
    pub data_type: u16,
    pub count: u16,
    pub cid: u32,
    pub available: u32,
    pub extended_postsize: Option<u32>,
    pub extended_count: Option<u32>,
}

impl CaHeader {
    pub const SIZE: usize = 16;

    pub fn new(cmmd: u16) -> Self {
        Self {
            cmmd,
            postsize: 0,
            data_type: 0,
            count: 0,
            cid: 0,
            available: 0,
            extended_postsize: None,
            extended_count: None,
        }
    }

    /// Whether this header uses extended form.
    ///
    /// Wire detection is by `postsize == 0xFFFF` alone, matching C
    /// `tcpiiu.cpp::processIncoming` (line 1168), `cac.cpp:1097`, and
    /// `rsrv/camessage.c:2410`. The `count == 0` field is set by the
    /// emit-side per the spec but is NOT checked on receive — a peer
    /// sending garbage in `m_count` of an extended header is still
    /// correctly parsed by C. We mirror C's lenient receive behavior.
    pub fn is_extended(&self) -> bool {
        self.postsize == 0xFFFF && self.extended_postsize.is_some()
    }

    /// Actual payload size in bytes.
    pub fn actual_postsize(&self) -> usize {
        if self.postsize == 0xFFFF {
            if let Some(ext) = self.extended_postsize {
                return ext as usize;
            }
        }
        self.postsize as usize
    }

    /// Actual element count.
    pub fn actual_count(&self) -> u32 {
        if self.postsize == 0xFFFF {
            if let Some(ext) = self.extended_count {
                return ext;
            }
        }
        self.count as u32
    }

    /// Set payload size and count, automatically switching to extended form if needed.
    /// `size` is the actual data length (unpadded). Wire-level 8-byte alignment padding
    /// is handled by the caller when writing to the socket, NOT stored in the header.
    ///
    /// Extended-form trigger matches C `comQueSend.cpp:285`:
    /// `payloadSize < 0xffff && nElem < 0xffff` → normal; equivalently,
    /// extended if `size >= 0xFFFF` OR `count >= 0xFFFF`. The previous
    /// Rust threshold (`count > 0xFFFF`) under-triggered for the exact
    /// `count == 0xFFFF` case, sending a normal-form header where C
    /// would have used extended — byte-mismatch on the wire.
    /// `peer_minor` is the CA minor version the PEER negotiated — the version
    /// of whoever will parse these bytes (the server's for a client request,
    /// the client's for a server reply). The extended form did not exist
    /// before CA_V49, so a pre-V49 peer would read the 24-byte header as 16
    /// header bytes plus 8 bytes of payload and de-sync its TCP stream. C
    /// refuses to build the frame at all in that case:
    /// `comQueSend::insertRequestHeader` throws `cacChannel::outOfBounds`
    /// (`comQueSend.cpp:313`) and `cas_copy_in_header` returns
    /// `ECA_16KARRAYCLIENT` (`caserverio.c:266-270`). Callers map
    /// [`ExtendedHeaderUnsupported`] to whichever of those their side owes.
    pub fn set_payload_size(
        &mut self,
        size: usize,
        count: u32,
        peer_minor: u16,
    ) -> Result<(), ExtendedHeaderUnsupported> {
        if size >= 0xFFFF || count >= 0xFFFF {
            if !ca_v49(peer_minor) {
                return Err(ExtendedHeaderUnsupported);
            }
            self.postsize = 0xFFFF;
            self.count = 0;
            self.extended_postsize = Some(size as u32);
            self.extended_count = Some(count);
        } else {
            self.postsize = size as u16;
            self.count = count as u16;
            self.extended_postsize = None;
            self.extended_count = None;
        }
        Ok(())
    }

    pub fn to_bytes(&self) -> [u8; 16] {
        let mut buf = [0u8; 16];
        buf[0..2].copy_from_slice(&self.cmmd.to_be_bytes());
        buf[2..4].copy_from_slice(&self.postsize.to_be_bytes());
        buf[4..6].copy_from_slice(&self.data_type.to_be_bytes());
        buf[6..8].copy_from_slice(&self.count.to_be_bytes());
        buf[8..12].copy_from_slice(&self.cid.to_be_bytes());
        buf[12..16].copy_from_slice(&self.available.to_be_bytes());
        buf
    }

    /// Serialize header, including extended fields if present.
    pub fn to_bytes_extended(&self) -> Vec<u8> {
        let mut buf = self.to_bytes().to_vec();
        if self.is_extended() {
            // SAFETY: is_extended() guarantees extended_postsize.is_some()
            buf.extend_from_slice(&self.extended_postsize.unwrap().to_be_bytes());
            // SAFETY: extended_count is always set alongside extended_postsize
            // in both set_payload_size() and from_bytes_extended()
            buf.extend_from_slice(&self.extended_count.unwrap_or(0).to_be_bytes());
        }
        buf
    }

    pub fn from_bytes(buf: &[u8]) -> CaResult<Self> {
        if buf.len() < 16 {
            return Err(CaError::Protocol(format!(
                "header too short: {} bytes",
                buf.len()
            )));
        }
        Ok(Self {
            cmmd: u16::from_be_bytes([buf[0], buf[1]]),
            postsize: u16::from_be_bytes([buf[2], buf[3]]),
            data_type: u16::from_be_bytes([buf[4], buf[5]]),
            count: u16::from_be_bytes([buf[6], buf[7]]),
            cid: u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]),
            available: u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]),
            extended_postsize: None,
            extended_count: None,
        })
    }

    /// Parse header with extended form support.
    /// Returns (header, total_bytes_consumed).
    pub fn from_bytes_extended(buf: &[u8]) -> CaResult<(Self, usize)> {
        if buf.len() < 16 {
            return Err(CaError::Protocol(format!(
                "header too short: {} bytes",
                buf.len()
            )));
        }
        let mut hdr = Self::from_bytes(buf)?;
        let mut consumed = 16;

        // C parity: extended-form detection is `m_postsize == 0xffff`
        // alone — see `tcpiiu.cpp:1168`, `cac.cpp:1097`, and
        // `rsrv/camessage.c:2410`. The `m_count == 0` half was an
        // overly-strict Rust addition that rejected legal extended
        // headers if a peer left non-zero garbage in `m_count`.
        if hdr.postsize == 0xFFFF {
            if buf.len() < 24 {
                return Err(CaError::Protocol("extended header incomplete".into()));
            }
            let ext_post = u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]);
            let ext_count = u32::from_be_bytes([buf[20], buf[21], buf[22], buf[23]]);
            // NO size policy here. An `m_postsize` of 33 MB is a
            // syntactically valid extended header, and libca's receiver
            // accepts it by default (`EPICS_CA_AUTO_ARRAY_BYTES=YES` ⇒
            // `tcpiiu.cpp:1214-1220` grows the body cache to fit). The bound
            // — and what to do at it — belongs to whichever loop owns the
            // receive buffer: the client applies [`max_recv_body_bytes`] and
            // drains, the server applies its own `maxstk` check and replies
            // ECA_TOLARGE (`camessage.c:2471-2489`, `server/tcp.rs`).
            hdr.extended_postsize = Some(ext_post);
            hdr.extended_count = Some(ext_count);
            consumed = 24;
        }

        Ok((hdr, consumed))
    }

    /// Parse a header received from a peer whose minor protocol version
    /// is `peer_minor`.
    ///
    /// C `rsrv/camessage.c:2410` reads the extended annex only when
    /// `CA_V49(client->minor_version_number) && msg.m_postsize ==
    /// 0xffff`. A pre-V49 peer that sends `m_postsize == 0xffff` takes
    /// the else branch, so `msgsize = 0xffff + 16 = 65551`, which fails
    /// the `msgsize & 0x7` alignment test at `camessage.c:2452` and gets
    /// "CAS: Missaligned protocol rejected" (ECA_INTERNAL) + disconnect.
    /// Reproduce that by keeping `postsize = 0xFFFF` on the plain header
    /// and letting the caller's alignment check reject it — never by
    /// consuming an annex the peer did not send, which would de-sync the
    /// stream by 8 bytes.
    pub fn from_bytes_for_peer(buf: &[u8], peer_minor: u16) -> CaResult<(Self, usize)> {
        if ca_v49(peer_minor) {
            return Self::from_bytes_extended(buf);
        }
        Ok((Self::from_bytes(buf)?, 16))
    }
}

/// Round up to 8-byte alignment.
/// Uses saturating_add to prevent overflow on pathological values.
pub fn align8(size: usize) -> usize {
    size.saturating_add(7) & !7
}

/// Build a padded, null-terminated, 8-byte aligned payload from a string
pub fn pad_string(s: &str) -> Vec<u8> {
    let mut bytes = s.as_bytes().to_vec();
    bytes.push(0); // null terminator
    let padded_len = align8(bytes.len());
    bytes.resize(padded_len, 0);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    // DBR_TIME_DOUBLE (20): C `dbr_time_double` is status(2) + severity(2) +
    // stamp(8) + RISC pad(4) + value(8), so `dbr_size[20]` = 24 and
    // `dbr_value_size[20]` = 8.
    const T_DOUBLE: u16 = 20;
    const T_DOUBLE_DBR_SIZE: u32 = 24;

    /// A zero (autosize) read is only legal from CA_V413 on
    /// (`caProto.h:48`). C `tcpiiu::readNotifyRequest` (`tcpiiu.cpp:1476`)
    /// substitutes the channel's native count below that; at and above it
    /// the zero travels to the wire so the server sizes the reply itself.
    #[test]
    fn read_notify_zero_count_substitutes_native_below_v413() {
        let wire = |req, native, minor| read_notify_wire_count(req, native, T_DOUBLE, minor);
        assert_eq!(wire(0, 42, 12).unwrap(), 42);
        assert_eq!(wire(0, 42, 13).unwrap(), 0);
        // The V413 boundary is the only rewrite: a non-zero request is never
        // rewritten by `readNotifyRequest`, not even one above the native
        // count (the server clamps that itself).
        assert_eq!(wire(7, 42, 12).unwrap(), 7);
        assert_eq!(wire(99, 42, 13).unwrap(), 99);
        // CA_MINIMUM_SUPPORTED_VERSION peers get the substitution too.
        assert_eq!(wire(0, 1, 4).unwrap(), 1);
    }

    /// `netSubscription::getCount` (`netIO.h:241-251`) adds a second
    /// collapse a read does not have: a cap above the native count also
    /// resolves to the native count, on every peer version. The cancel
    /// resolver is that function alone (`tcpiiu.cpp:1659`).
    #[test]
    fn subscription_wire_count_applies_both_getcount_collapses() {
        let wire = |req, native, minor| subscription_wire_count(req, native, T_DOUBLE, minor);
        // zero-count collapse, gated on CA_V413
        assert_eq!(wire(0, 42, 12).unwrap(), 42);
        assert_eq!(wire(0, 42, 13).unwrap(), 0);
        // over-large cap collapse, ungated
        assert_eq!(wire(99, 42, 12).unwrap(), 42);
        assert_eq!(wire(99, 42, 13).unwrap(), 42);
        // a cap at or below the native count travels verbatim
        assert_eq!(wire(42, 42, 13).unwrap(), 42);
        assert_eq!(wire(7, 42, 12).unwrap(), 7);
        // the cancel path resolves identically, but has no element bound
        assert_eq!(subscription_cancel_wire_count(0, 42, 12), 42);
        assert_eq!(subscription_cancel_wire_count(0, 42, 13), 0);
        assert_eq!(subscription_cancel_wire_count(99, 42, 13), 42);
    }

    /// `tcpiiu::readNotifyRequest` (`tcpiiu.cpp:1463-1473`): a pre-V49 circuit
    /// caps the request at `(MAX_TCP - dbr_size[t]) / dbr_value_size[t]` and
    /// raises `msgBodyCacheTooSmall` → ECA_TOLARGE past it. For
    /// DBR_TIME_DOUBLE that is `(16384 - 24) / 8` = 2045.
    #[test]
    fn read_bound_is_max_tcp_on_a_pre_v49_circuit() {
        const MAX_ELEM: u32 = (16384 - T_DOUBLE_DBR_SIZE) / 8; // 2045
        assert!(read_notify_wire_count(MAX_ELEM, MAX_ELEM, T_DOUBLE, 8).is_ok());
        assert!(matches!(
            read_notify_wire_count(MAX_ELEM + 1, MAX_ELEM + 1, T_DOUBLE, 8),
            Err(CaError::TooLarge)
        ));
        // A V49 circuit has effectively no bound: same request is framed.
        assert_eq!(
            read_notify_wire_count(MAX_ELEM + 1, MAX_ELEM + 1, T_DOUBLE, 13).unwrap(),
            MAX_ELEM + 1
        );
    }

    /// The read bound is applied to the count the CALLER asked for, BEFORE the
    /// pre-V413 zero substitution (`tcpiiu.cpp:1470` then `:1476`). So a zero
    /// request always clears the bound and C frames the substituted native
    /// count even when that count is over it. Quirk preserved deliberately.
    #[test]
    fn read_bound_precedes_the_zero_substitution() {
        // Minor 8 is pre-V49 (so MAX_TCP bounds the request) and therefore
        // also pre-V413 (so the zero is substituted).
        let native = (16384 - T_DOUBLE_DBR_SIZE) / 8 + 500; // past the pre-V49 bound
        assert_eq!(
            read_notify_wire_count(0, native, T_DOUBLE, 8).unwrap(),
            native
        );
        // Ask for the same count explicitly and it is rejected.
        assert!(matches!(
            read_notify_wire_count(native, native, T_DOUBLE, 8),
            Err(CaError::TooLarge)
        ));
    }

    /// `tcpiiu::subscriptionRequest` runs `getCount` FIRST and bounds the
    /// RESOLVED count (`tcpiiu.cpp:1572-1585`) — the reverse of the read
    /// path — so an autosize subscription to a pre-V49 peer whose native
    /// count overflows MAX_TCP is rejected where the same read is framed.
    #[test]
    fn subscription_bound_follows_the_zero_substitution() {
        let native = (16384 - T_DOUBLE_DBR_SIZE) / 8 + 500;
        assert!(matches!(
            subscription_wire_count(0, native, T_DOUBLE, 8),
            Err(CaError::TooLarge)
        ));
        // V413 peer: the zero is not substituted, so nothing to bound.
        assert_eq!(subscription_wire_count(0, native, T_DOUBLE, 13).unwrap(), 0);
    }

    /// `comQueSend::insertRequestWithPayLoad` (`comQueSend.cpp:352-364`):
    /// `maxElem = (MAX_TCP - sizeof(caHdr) - sizeof(dbr_double_t) -
    /// dbr_size[t]) / dbr_value_size[t]`, rejected at `>=` (not `>`) with
    /// `outOfBounds` → ECA_BADCOUNT. For DBR_DOUBLE (6) that is
    /// `(16384 - 16 - 8 - 8) / 8` = 2044.
    #[test]
    fn write_bound_is_max_tcp_minus_header_on_a_pre_v49_circuit() {
        const DBR_DOUBLE: u16 = 6;
        const MAX_ELEM: u32 = (16384 - 16 - 8 - 8) / 8; // 2044
        assert!(check_write_request(MAX_ELEM - 1, DBR_DOUBLE, 8).is_ok());
        // C's comparison is `>=`, so maxElem itself is already rejected.
        assert!(matches!(
            check_write_request(MAX_ELEM, DBR_DOUBLE, 8),
            Err(CaError::BadCount)
        ));
        // A scalar put takes C's `nElem == 1` branch, which has no bound.
        assert!(check_write_request(1, DBR_DOUBLE, 8).is_ok());
        // A V49 circuit frames the same array.
        assert!(check_write_request(MAX_ELEM, DBR_DOUBLE, 13).is_ok());
    }

    /// The type bound sits ABOVE the scalar/array fork, as C's does
    /// (`comQueSend.cpp:323` vs `:330`). A scalar has no *element* bound, but
    /// that is no reason to skip the *type* check — and it is the one this
    /// gate used to skip, because the `count == 1` early return came first.
    ///
    /// It matters more than a wasted round trip: the server treats a type
    /// past `LAST_BUFFER_TYPE` as a protocol violation and drops the circuit
    /// (`AcceptedWriteType::classify` → ECA_BADTYPE + RSRV_ERROR), so a
    /// request that leaves the client costs the connection.
    #[test]
    fn a_scalar_put_is_still_bounded_by_the_dbr_type() {
        use epics_base_rs::types::LAST_BUFFER_TYPE;
        for count in [1u32, 2, 100] {
            assert!(
                matches!(
                    check_write_request(count, LAST_BUFFER_TYPE + 1, 13),
                    Err(CaError::UnsupportedType(t)) if t == LAST_BUFFER_TYPE + 1
                ),
                "count {count}: a type past LAST_BUFFER_TYPE is ECA_BADTYPE \
                 before anything is queued, scalar or array"
            );
        }
        // The bound itself is inside it: DBR_CLASS_NAME is framable.
        assert!(check_write_request(1, LAST_BUFFER_TYPE, 13).is_ok());
    }

    /// The read and subscription paths resolve the same sizes, so they carry
    /// the same bound. Pinned so the three request paths cannot drift apart
    /// again — that divergence is what let the scalar put through.
    #[test]
    fn every_request_path_shares_one_type_bound() {
        use epics_base_rs::types::LAST_BUFFER_TYPE;
        let over = LAST_BUFFER_TYPE + 1;
        assert!(matches!(
            read_notify_wire_count(1, 1, over, 13),
            Err(CaError::UnsupportedType(_))
        ));
        assert!(matches!(
            subscription_wire_count(1, 1, over, 13),
            Err(CaError::UnsupportedType(_))
        ));
        assert!(matches!(
            check_write_request(1, over, 13),
            Err(CaError::UnsupportedType(_))
        ));
    }

    /// `comQueSend.cpp:332-341`: a scalar DBR_STRING put frames
    /// `align8(strlen + 1)`, not the fixed 40-byte element. The body is
    /// the NUL-terminated string plus zero padding to the 8-byte
    /// message alignment.
    #[test]
    fn scalar_string_put_frames_align8_of_strlen_plus_nul() {
        const DBR_STRING: u16 = 0;
        // `EpicsValue::String("abc")` serializes as 40 NUL-padded bytes;
        // C would frame 4 bytes ("abc\0") rounded up to 8.
        let mut payload = vec![0u8; 40];
        payload[..3].copy_from_slice(b"abc");
        let frame =
            build_put_frame(CA_PROTO_WRITE, 7, DBR_STRING, 1, None, payload, 13).expect("frames");
        let (hdr, consumed) = CaHeader::from_bytes_extended(&frame).expect("parses");
        assert_eq!(hdr.actual_postsize(), 8, "align8(strlen(\"abc\") + 1) == 8");
        assert_eq!(frame.len() - consumed, 8, "body is 8 bytes, not 40");
        assert_eq!(&frame[consumed..], b"abc\0\0\0\0\0");

        // Boundary: 7 chars + NUL == 8 exactly (no padding), and 8
        // chars + NUL == 9 rounds up to 16 — the first length that
        // needs a second alignment unit.
        for (s, want) in [(&b"abcdefg"[..], 8usize), (&b"abcdefgh"[..], 16)] {
            let mut payload = vec![0u8; 40];
            payload[..s.len()].copy_from_slice(s);
            let frame = build_put_frame(CA_PROTO_WRITE, 7, DBR_STRING, 1, None, payload, 13)
                .expect("frames");
            let (hdr, consumed) = CaHeader::from_bytes_extended(&frame).expect("parses");
            assert_eq!(hdr.actual_postsize(), want);
            assert_eq!(frame.len() - consumed, want);
        }

        // The empty string is 1 byte (`"\0"`) → one alignment unit.
        let frame = build_put_frame(CA_PROTO_WRITE, 7, DBR_STRING, 1, None, vec![0u8; 40], 13)
            .expect("frames");
        assert_eq!(
            CaHeader::from_bytes_extended(&frame).unwrap().0.postsize,
            8,
            "an empty string still frames one 8-byte alignment unit"
        );

        // 39 chars + NUL == 40: the full element, already 8-aligned.
        let payload = vec![b'x'; 39]
            .into_iter()
            .chain(std::iter::once(0u8))
            .collect::<Vec<u8>>();
        let frame =
            build_put_frame(CA_PROTO_WRITE, 7, DBR_STRING, 1, None, payload, 13).expect("frames");
        assert_eq!(
            CaHeader::from_bytes_extended(&frame).unwrap().0.postsize,
            40
        );

        // 40 non-NUL bytes: `strlen + 1 > MAX_STRING_SIZE`, so C throws
        // `cacChannel::outOfBounds` → ECA_BADCOUNT and nothing is sent.
        assert!(matches!(
            build_put_frame(CA_PROTO_WRITE, 7, DBR_STRING, 1, None, vec![b'x'; 40], 13),
            Err(CaError::BadCount)
        ));
    }

    /// The contraction is scalar-string ONLY. Every other shape keeps
    /// the serialized length C computes from `dbr_size` / `dbr_size_n`
    /// (`comQueSend.cpp:344-350,366-376`), padded to the 8-byte message
    /// alignment.
    #[test]
    fn only_the_scalar_string_put_contracts() {
        const DBR_STRING: u16 = 0;
        const DBR_DOUBLE: u16 = 6;

        // A DBR_STRING *array* stays 40 bytes per element.
        let frame = build_put_frame(
            CA_PROTO_WRITE_NOTIFY,
            7,
            DBR_STRING,
            2,
            Some(1),
            vec![0u8; 80],
            13,
        )
        .expect("frames");
        assert_eq!(
            CaHeader::from_bytes_extended(&frame).unwrap().0.postsize,
            80,
            "a string array is dbr_size_n = 40 * n, never contracted"
        );

        // A scalar DBR_DOUBLE is dbr_size[DBR_DOUBLE] = 8, already
        // aligned — and its zero bytes must not be read as a NUL
        // terminator.
        let frame = build_put_frame(
            CA_PROTO_WRITE,
            7,
            DBR_DOUBLE,
            1,
            None,
            0.0f64.to_be_bytes().to_vec(),
            13,
        )
        .expect("frames");
        let (hdr, consumed) = CaHeader::from_bytes_extended(&frame).expect("parses");
        assert_eq!(hdr.postsize, 8);
        assert_eq!(
            frame.len() - consumed,
            8,
            "a zero double still puts 8 bytes"
        );
    }

    #[test]
    fn test_header_roundtrip() {
        let hdr = CaHeader {
            cmmd: CA_PROTO_SEARCH,
            postsize: 16,
            data_type: 5,
            count: 13,
            cid: 42,
            available: 100,
            extended_postsize: None,
            extended_count: None,
        };
        let bytes = hdr.to_bytes();
        let hdr2 = CaHeader::from_bytes(&bytes).unwrap();
        assert_eq!(hdr.cmmd, hdr2.cmmd);
        assert_eq!(hdr.postsize, hdr2.postsize);
        assert_eq!(hdr.data_type, hdr2.data_type);
        assert_eq!(hdr.count, hdr2.count);
        assert_eq!(hdr.cid, hdr2.cid);
        assert_eq!(hdr.available, hdr2.available);
    }

    #[test]
    fn test_align8() {
        assert_eq!(align8(0), 0);
        assert_eq!(align8(1), 8);
        assert_eq!(align8(7), 8);
        assert_eq!(align8(8), 8);
        assert_eq!(align8(9), 16);
    }

    /// `repeater_port()` honours `EPICS_CA_REPEATER_PORT`, falls back
    /// to the compiled default when the env var is absent, and clamps
    /// to the default on garbage input — matching libca
    /// `envGetInetPortConfigParam(&EPICS_CA_REPEATER_PORT, …)` shape.
    ///
    /// Sequential because all three branches mutate process env. Use
    /// `serial_test::serial` to keep them out of each other's way.
    #[test]
    #[serial_test::serial]
    fn repeater_port_honours_env_with_default_fallback() {
        // Save & clear to make the test idempotent.
        let saved = std::env::var("EPICS_CA_REPEATER_PORT").ok();
        // SAFETY: serial_test::serial; mutations are confined to this
        // test and the saved value is restored in a finally block at
        // the end.
        unsafe { std::env::remove_var("EPICS_CA_REPEATER_PORT") };

        assert_eq!(
            repeater_port(),
            CA_REPEATER_PORT,
            "no env → compiled default"
        );

        // SAFETY: see comment above.
        unsafe { std::env::set_var("EPICS_CA_REPEATER_PORT", "5165") };
        assert_eq!(repeater_port(), 5165, "valid u16 env override");

        // SAFETY: see comment above.
        unsafe { std::env::set_var("EPICS_CA_REPEATER_PORT", "not-a-port") };
        assert_eq!(
            repeater_port(),
            CA_REPEATER_PORT,
            "garbage env → compiled default"
        );

        // SAFETY: see comment above. Restore for subsequent serial tests.
        unsafe {
            match saved {
                Some(v) => std::env::set_var("EPICS_CA_REPEATER_PORT", v),
                None => std::env::remove_var("EPICS_CA_REPEATER_PORT"),
            }
        }
    }

    /// The receive path's bound is `EPICS_CA_AUTO_ARRAY_BYTES`, not
    /// `EPICS_CA_MAX_ARRAY_BYTES`. C's compiled default is YES
    /// (`configure/CONFIG_ENV:37`) ⇒ `tcpLargeRecvBufFreeList` stays NULL
    /// (`cac.cpp:227-232`) ⇒ `processIncoming` grows the body cache to fit
    /// whatever arrives (`tcpiiu.cpp:1214-1220`). So a C client reads a 33 MB
    /// waveform with `EPICS_CA_MAX_ARRAY_BYTES` unset — and with it SET, too.
    ///
    /// `envGetBoolConfigParam` is `epicsStrCaseCmp(text, "yes") == 0`
    /// (`envSubr.c:331`), so anything that is not the word "yes" turns the
    /// auto sizing OFF — including "1" and "true". Quirk replicated.
    #[test]
    #[serial_test::serial]
    fn receive_body_limit_is_absent_unless_auto_array_bytes_is_disabled() {
        let saved_auto = std::env::var("EPICS_CA_AUTO_ARRAY_BYTES").ok();
        let saved_max = std::env::var("EPICS_CA_MAX_ARRAY_BYTES").ok();
        // SAFETY: serial_test::serial; both vars are restored below.
        unsafe {
            std::env::remove_var("EPICS_CA_AUTO_ARRAY_BYTES");
            std::env::remove_var("EPICS_CA_MAX_ARRAY_BYTES");
        }

        assert!(auto_array_bytes(), "unset ⇒ compiled default YES");
        assert_eq!(
            max_recv_body_bytes(),
            None,
            "C's default receive path has no cap at all"
        );

        // Setting the array-bytes cap must NOT introduce a receive bound:
        // under AUTO=YES C ignores it on receive entirely.
        unsafe { std::env::set_var("EPICS_CA_MAX_ARRAY_BYTES", "16384") };
        assert_eq!(
            max_recv_body_bytes(),
            None,
            "EPICS_CA_MAX_ARRAY_BYTES alone must not bound the receive path"
        );

        // Only "yes" (any case) keeps auto sizing on.
        unsafe { std::env::set_var("EPICS_CA_AUTO_ARRAY_BYTES", "YeS") };
        assert!(auto_array_bytes());
        assert_eq!(max_recv_body_bytes(), None);

        // Anything else — including "1" — turns it off, and only then does
        // the cap apply: `maxRecvBytesTCP` = 16384 + 24 (`cac.cpp:204`).
        unsafe { std::env::set_var("EPICS_CA_AUTO_ARRAY_BYTES", "1") };
        assert!(!auto_array_bytes(), "C: only the word \"yes\" is true");
        assert_eq!(max_recv_body_bytes(), Some(16384 + 24));

        unsafe { std::env::set_var("EPICS_CA_AUTO_ARRAY_BYTES", "NO") };
        assert_eq!(max_recv_body_bytes(), Some(16384 + 24));

        // A cap below MAX_TCP is rounded up to MAX_TCP (`cac.cpp:206-214`).
        unsafe { std::env::set_var("EPICS_CA_MAX_ARRAY_BYTES", "8") };
        assert_eq!(max_recv_body_bytes(), Some(MAX_TCP));

        // SAFETY: see above. Restore for subsequent serial tests.
        unsafe {
            match saved_auto {
                Some(v) => std::env::set_var("EPICS_CA_AUTO_ARRAY_BYTES", v),
                None => std::env::remove_var("EPICS_CA_AUTO_ARRAY_BYTES"),
            }
            match saved_max {
                Some(v) => std::env::set_var("EPICS_CA_MAX_ARRAY_BYTES", v),
                None => std::env::remove_var("EPICS_CA_MAX_ARRAY_BYTES"),
            }
        }
    }

    /// `EPICS_CA_MAX_ARRAY_BYTES` means ONE thing: the operator's declared
    /// largest array, C's meaning, with C's default of 16384. It is NOT the
    /// allocation ceiling, and it never carries a second, 1024x larger default.
    ///
    /// The boundaries, one case each:
    ///   * AUTO on  (C's default)  ⇒ ceiling is the compile-time constant, and
    ///     `EPICS_CA_MAX_ARRAY_BYTES` does not move it. (C is unbounded here;
    ///     the constant is the Tier 2 refusal to size an allocation from a
    ///     remote header — see `MAX_FRAME_BODY_BYTES`.)
    ///   * AUTO off                ⇒ ceiling IS the declared array buffer.
    ///   * unset, either way       ⇒ the table default, 16384, floored at
    ///     MAX_TCP — never 16 MiB.
    #[test]
    #[serial_test::serial]
    fn max_array_bytes_has_exactly_one_meaning() {
        let saved_auto = std::env::var("EPICS_CA_AUTO_ARRAY_BYTES").ok();
        let saved_max = std::env::var("EPICS_CA_MAX_ARRAY_BYTES").ok();
        // SAFETY: serial_test::serial; both vars are restored below.
        unsafe {
            std::env::remove_var("EPICS_CA_AUTO_ARRAY_BYTES");
            std::env::remove_var("EPICS_CA_MAX_ARRAY_BYTES");
        }

        // Unset: the table's 16384, + the 24-byte extended header (`cac.cpp:204`
        // "allow room for the protocol header so that they get the array size
        // they requested"). C's number, not 16 MiB.
        assert_eq!(max_array_bytes_buffer(), 16384 + 24);

        // AUTO on (default) — the declared array size does not move the ceiling.
        assert_eq!(max_frame_body_bytes(), MAX_FRAME_BODY_BYTES);
        unsafe { std::env::set_var("EPICS_CA_MAX_ARRAY_BYTES", "65536") };
        assert_eq!(
            max_frame_body_bytes(),
            MAX_FRAME_BODY_BYTES,
            "under AUTO=YES C bounds nothing with this variable, so neither do we"
        );
        assert_eq!(
            max_array_bytes_buffer(),
            65536 + 24,
            "the declared array buffer still tracks the variable"
        );

        // AUTO off — now the ceiling IS the declared array buffer.
        unsafe { std::env::set_var("EPICS_CA_AUTO_ARRAY_BYTES", "NO") };
        assert_eq!(max_frame_body_bytes(), 65536 + 24);

        // A negative value is a failed fetch (`caservertask.c:511`).
        unsafe { std::env::set_var("EPICS_CA_MAX_ARRAY_BYTES", "-1") };
        assert_eq!(max_array_bytes_buffer(), MAX_TCP);

        // SAFETY: see above. Restore for subsequent serial tests.
        unsafe {
            match saved_auto {
                Some(v) => std::env::set_var("EPICS_CA_AUTO_ARRAY_BYTES", v),
                None => std::env::remove_var("EPICS_CA_AUTO_ARRAY_BYTES"),
            }
            match saved_max {
                Some(v) => std::env::set_var("EPICS_CA_MAX_ARRAY_BYTES", v),
                None => std::env::remove_var("EPICS_CA_MAX_ARRAY_BYTES"),
            }
        }
    }

    /// The header codec carries no size policy: an extended header announcing
    /// a body far past `max_frame_body_bytes()` is syntactically valid and must
    /// parse. Rejecting it here is what closed the circuit on a 33 MB
    /// waveform; C's `tcpiiu` accepts the header and allocates the body.
    #[test]
    fn extended_header_with_a_huge_body_still_parses() {
        let mut hdr = CaHeader::new(CA_PROTO_READ_NOTIFY);
        hdr.postsize = 0xFFFF;
        let mut bytes = hdr.to_bytes().to_vec();
        let huge = (max_frame_body_bytes() * 3) as u32;
        bytes.extend_from_slice(&huge.to_be_bytes());
        bytes.extend_from_slice(&1u32.to_be_bytes());

        let (parsed, consumed) =
            CaHeader::from_bytes_extended(&bytes).expect("a large body is a valid header");
        assert_eq!(consumed, 24);
        assert_eq!(parsed.actual_postsize(), huge as usize);
    }

    #[test]
    fn test_pad_string() {
        let padded = pad_string("TEST");
        assert_eq!(padded.len(), 8); // "TEST\0" = 5 -> align8 -> 8
        assert_eq!(&padded[..4], b"TEST");
        assert_eq!(padded[4], 0);
    }

    #[test]
    fn test_extended_header_roundtrip() {
        let mut hdr = CaHeader::new(CA_PROTO_READ_NOTIFY);
        hdr.data_type = 6; // Double
        hdr.cid = 42;
        hdr.available = 100;
        hdr.set_payload_size(100_000, 12500, CA_MINOR_VERSION)
            .expect("modern peer accepts the extended header");
        assert!(hdr.is_extended());
        assert_eq!(hdr.actual_postsize(), 100_000);
        assert_eq!(hdr.actual_count(), 12500);

        let bytes = hdr.to_bytes_extended();
        assert_eq!(bytes.len(), 24);

        let (hdr2, consumed) = CaHeader::from_bytes_extended(&bytes).unwrap();
        assert_eq!(consumed, 24);
        assert!(hdr2.is_extended());
        assert_eq!(hdr2.actual_postsize(), 100_000);
        assert_eq!(hdr2.actual_count(), 12500);
        assert_eq!(hdr2.cmmd, CA_PROTO_READ_NOTIFY);
    }

    #[test]
    fn test_actual_postsize_normal() {
        let mut hdr = CaHeader::new(CA_PROTO_READ_NOTIFY);
        hdr.postsize = 1024;
        hdr.count = 128;
        assert!(!hdr.is_extended());
        assert_eq!(hdr.actual_postsize(), 1024);
        assert_eq!(hdr.actual_count(), 128);
    }

    #[test]
    fn test_set_payload_size_auto() {
        // Small payload — stays normal
        let mut hdr = CaHeader::new(CA_PROTO_READ_NOTIFY);
        hdr.set_payload_size(1000, 100, CA_MINOR_VERSION)
            .expect("modern peer accepts the extended header");
        assert!(!hdr.is_extended());
        assert_eq!(hdr.postsize, 1000);
        assert_eq!(hdr.count, 100);

        // Large payload — auto-extends
        hdr.set_payload_size(70_000, 8750, CA_MINOR_VERSION)
            .expect("modern peer accepts the extended header");
        assert!(hdr.is_extended());
        assert_eq!(hdr.postsize, 0xFFFF);
        assert_eq!(hdr.count, 0);
        assert_eq!(hdr.actual_postsize(), 70_000);
        assert_eq!(hdr.actual_count(), 8750);
    }

    #[test]
    fn test_extended_count_overflow() {
        // count >= 0xFFFF triggers extended even if size is small.
        // C `comQueSend.cpp:285` uses `nElem < 0xffff` as the normal
        // threshold, so exact 0xFFFF must take the extended branch.
        let mut hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
        hdr.set_payload_size(100, 100_000, CA_MINOR_VERSION)
            .expect("modern peer accepts the extended header");
        assert!(hdr.is_extended());
        assert_eq!(hdr.actual_postsize(), 100);
        assert_eq!(hdr.actual_count(), 100_000);

        // Exact 0xFFFF boundary — must trigger extended (regression
        // for the prior `count > 0xFFFF` under-trigger).
        let mut hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
        hdr.set_payload_size(100, 0xFFFF, CA_MINOR_VERSION)
            .expect("modern peer accepts the extended header");
        assert!(hdr.is_extended());
        assert_eq!(hdr.actual_count(), 0xFFFF);
    }

    /// A payload past `MAX_FRAME_BODY_BYTES` is NOT a parse error: the codec has
    /// no size policy (R6-21). The receive loop that owns the buffer applies
    /// the bound — the client via [`max_recv_body_bytes`] (unbounded by C's
    /// default), the server via its own `maxstk` check + ECA_TOLARGE reply.
    #[test]
    fn test_extended_payload_past_max_frame_body_bytes_parses() {
        let mut buf = vec![0u8; 24];
        // Set postsize=0xFFFF, count=0
        buf[2] = 0xFF;
        buf[3] = 0xFF;
        buf[6] = 0;
        buf[7] = 0;
        // Set extended_postsize to > MAX_FRAME_BODY_BYTES
        let big: u32 = (MAX_FRAME_BODY_BYTES + 1) as u32;
        buf[16..20].copy_from_slice(&big.to_be_bytes());
        buf[20..24].copy_from_slice(&1u32.to_be_bytes());

        let (hdr, consumed) = CaHeader::from_bytes_extended(&buf).expect("valid extended header");
        assert_eq!(consumed, 24);
        assert_eq!(hdr.actual_postsize(), big as usize);
    }
}
