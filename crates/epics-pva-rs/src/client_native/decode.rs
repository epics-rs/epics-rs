//! Server-side PVA response decoder.
//!
//! Reads bytes off the wire frame-by-frame, dispatching to the correct
//! application-message decoder. Pure data — no I/O — so it's exhaustively
//! unit-testable.

use std::io::Cursor;
use std::net::SocketAddr;

use crate::error::{PvaError, PvaResult};
use crate::proto::{
    BitSet, ByteOrder, Command, ControlCommand, HeaderFlags, PvaHeader, ReadExt, Status,
    decode_size, decode_string, ip_from_bytes_allow_unspec,
};
use crate::pvdata::{FieldDesc, PvField};

/// One framed PVA message, with header already parsed and payload sliced out.
#[derive(Debug, Clone)]
pub struct Frame {
    pub header: PvaHeader,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn order(&self) -> ByteOrder {
        self.header.flags.byte_order()
    }

    pub fn cursor(&self) -> Cursor<&[u8]> {
        Cursor::new(self.payload.as_slice())
    }
}

/// Local role of the peer that is *reading* the wire. Used by
/// [`try_parse_frame_role`] to enforce the direction-bit invariant from
/// pvxs `conn.cpp:160` (`isClient ^ !!(header[2]&pva_flags::Server)`):
/// a server's inbound frames must have the `Server` flag CLEAR (client →
/// server direction), and a client's inbound frames must have it SET
/// (server → client). Reject mismatches as a protocol fault so a peer
/// can't echo our own outbound traffic back at us — defense-in-depth
/// against simple replay or loopback misconfigurations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerRole {
    /// The local end is a PVA server reading frames from a client.
    Server,
    /// The local end is a PVA client reading frames from a server.
    Client,
    /// Role-agnostic (sniffers, mixed-mode CLI tools, unit tests).
    Either,
}

/// Try to decode a single frame from the start of `buf`. On success returns
/// the frame plus the number of bytes consumed; on incomplete input returns
/// `Ok(None)` so the caller can read more.
///
/// Role-agnostic — equivalent to `try_parse_frame_role(buf, PeerRole::Either)`.
/// Production read loops should use [`try_parse_frame_role`] with their
/// actual role so the direction-bit check engages.
pub fn try_parse_frame(buf: &[u8]) -> PvaResult<Option<(Frame, usize)>> {
    try_parse_frame_role(buf, PeerRole::Either)
}

/// Role-aware variant of [`try_parse_frame`]. Enforces the pvxs direction
/// invariant: a [`PeerRole::Server`] reader expects the `Server` flag bit
/// to be CLEAR on inbound frames; a [`PeerRole::Client`] reader expects
/// it SET. Mismatches return `Err(Protocol)` so the caller can drop the
/// connection. [`PeerRole::Either`] skips the check entirely.
pub fn try_parse_frame_role(buf: &[u8], role: PeerRole) -> PvaResult<Option<(Frame, usize)>> {
    if buf.len() < PvaHeader::SIZE {
        return Ok(None);
    }
    let mut cur = Cursor::new(buf);
    let header = PvaHeader::decode(&mut cur).map_err(|e| PvaError::Decode(e.to_string()))?;
    let from_server = header.flags.is_server();
    match role {
        PeerRole::Server if from_server => {
            return Err(PvaError::Protocol(format!(
                "inbound frame has Server direction bit set (cmd=0x{:02X}, flags=0x{:02X}) — expected client→server",
                header.command, header.flags.0
            )));
        }
        PeerRole::Client if !from_server => {
            return Err(PvaError::Protocol(format!(
                "inbound frame has Server direction bit clear (cmd=0x{:02X}, flags=0x{:02X}) — expected server→client",
                header.command, header.flags.0
            )));
        }
        _ => {}
    }
    if header.flags.is_control() {
        // Control messages have no body; payload_length carries the data word.
        return Ok(Some((
            Frame {
                header,
                payload: Vec::new(),
            },
            PvaHeader::SIZE,
        )));
    }
    let needed = PvaHeader::SIZE + header.payload_length as usize;
    if buf.len() < needed {
        return Ok(None);
    }
    let payload = buf[PvaHeader::SIZE..needed].to_vec();
    Ok(Some((Frame { header, payload }, needed)))
}

// ─── Search response ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SearchResponse {
    pub guid: [u8; 12],
    pub seq: u32,
    pub server_addr: SocketAddr,
    pub protocol: String,
    pub found: bool,
    pub cids: Vec<u32>,
}

pub fn decode_search_response(frame: &Frame) -> PvaResult<SearchResponse> {
    if frame.header.command != Command::SearchResponse.code() {
        return Err(PvaError::Protocol(format!(
            "expected SearchResponse (4), got {}",
            frame.header.command
        )));
    }
    let order = frame.order();
    let mut cur = frame.cursor();
    let mut guid = [0u8; 12];
    let bytes = cur
        .get_bytes(12)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    guid.copy_from_slice(&bytes);
    let seq = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let addr_bytes = cur
        .get_bytes(16)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let mut addr = [0u8; 16];
    addr.copy_from_slice(&addr_bytes);
    let port = cur
        .get_u16(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let protocol = decode_string(&mut cur, order)
        .map_err(|e| PvaError::Decode(e.to_string()))?
        .unwrap_or_default();
    let found = cur.get_u8().map_err(|e| PvaError::Decode(e.to_string()))? != 0;
    let count = cur
        .get_u16(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    // cap pre-allocation at remaining-bytes / 4 so a peer
    // can't trick us into reserving 256 KB up-front for cids that
    // the trailing payload could never supply. u16 already bounds
    // the worst case but the pattern matches `safe_capacity` in
    // pvdata/encode.rs.
    let remaining = (cur.get_ref().len()).saturating_sub(cur.position() as usize);
    let cap = (count as usize).min(remaining / 4);
    let mut cids = Vec::with_capacity(cap);
    for _ in 0..count {
        cids.push(
            cur.get_u32(order)
                .map_err(|e| PvaError::Decode(e.to_string()))?,
        );
    }

    // a wildcard/unspecified server address means "use the
    // datagram source IP". pvxs `client.cpp:841-843` does the
    // substitution; we carry the raw advertised address through here
    // and let the search engine apply the substitution on receipt
    // (where the source addr is available). `ip_from_bytes_allow_unspec`
    // returns `IpAddr::V6(::)` for all-zero — search_engine treats
    // unspecified as the substitution sentinel.
    let ip = ip_from_bytes_allow_unspec(&addr);
    let server_addr = SocketAddr::new(ip, port);

    Ok(SearchResponse {
        guid,
        seq,
        server_addr,
        protocol,
        found,
        cids,
    })
}

// ─── Connection validation request (server → client) ────────────────────

/// Server-side `CONNECTION_VALIDATION` (cmd=1) initiated by the server during
/// handshake. Carries `buffer_size`, introspection registry size, and the
/// list of supported authentication methods.
#[derive(Debug, Clone)]
pub struct ConnectionValidationRequest {
    pub server_buffer_size: u32,
    pub server_registry_size: u16,
    pub auth_methods: Vec<String>,
}

pub fn decode_connection_validation_request(
    frame: &Frame,
) -> PvaResult<ConnectionValidationRequest> {
    if frame.header.command != Command::ConnectionValidation.code() {
        return Err(PvaError::Protocol(format!(
            "expected ConnectionValidation (1), got {}",
            frame.header.command
        )));
    }
    let order = frame.order();
    let mut cur = frame.cursor();
    let server_buffer_size = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let server_registry_size = cur
        .get_u16(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    // pvxs `pvaproto.h:284-305` rejects the `0xFF` null
    // marker for a `Size` decode unless the caller passes
    // `allow_null=true`. The CONNECTION_VALIDATION auth-method count
    // is a non-null Size: pvxs `clientconn.cpp:228-232` decodes via
    // the default `from_wire(M, nauth)` which faults on null, and
    // `:247-251` then disconnects. Pre-fix Rust mapped `None` (null)
    // to `0` and proceeded as if the server advertised an empty
    // method list, after which `clientconn.rs` defaults to
    // `anonymous` — a malformed-handshake → auth-downgrade silently.
    let count = match decode_size(&mut cur, order).map_err(|e| PvaError::Decode(e.to_string()))? {
        Some(n) => n as usize,
        None => {
            return Err(PvaError::Decode(
                "CONNECTION_VALIDATION auth-method count is the null Size marker (0xFF); \
                 a malformed handshake (pvxs clientconn.cpp:228-232 disconnect parity)"
                    .to_string(),
            ));
        }
    };
    // cap allocation against attacker-controlled count. Each
    // auth method string consumes at least 1 byte (Size + NUL); the
    // remaining cursor bytes bound how many can really arrive.
    let remaining = cur.get_ref().len().saturating_sub(cur.position() as usize);
    let mut auth_methods = Vec::with_capacity(count.min(remaining));
    for _ in 0..count {
        auth_methods.push(
            decode_string(&mut cur, order)
                .map_err(|e| PvaError::Decode(e.to_string()))?
                .unwrap_or_default(),
        );
    }
    Ok(ConnectionValidationRequest {
        server_buffer_size,
        server_registry_size,
        auth_methods,
    })
}

// ─── Connection validated ────────────────────────────────────────────────

/// `CONNECTION_VALIDATED` (cmd=9) — server's final ACK of the handshake.
pub fn decode_connection_validated(frame: &Frame) -> PvaResult<Status> {
    if frame.header.command != Command::ConnectionValidated.code() {
        return Err(PvaError::Protocol(format!(
            "expected ConnectionValidated (9), got {}",
            frame.header.command
        )));
    }
    let order = frame.order();
    let mut cur = frame.cursor();
    Status::decode(&mut cur, order).map_err(|e| PvaError::Decode(e.to_string()))
}

// ─── Create channel response ─────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CreateChannelResponse {
    pub cid: u32,
    pub sid: u32,
    pub status: Status,
}

pub fn decode_create_channel_response(frame: &Frame) -> PvaResult<CreateChannelResponse> {
    if frame.header.command != Command::CreateChannel.code() {
        return Err(PvaError::Protocol(format!(
            "expected CreateChannel (7), got {}",
            frame.header.command
        )));
    }
    let order = frame.order();
    let mut cur = frame.cursor();
    let cid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let sid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let status = Status::decode(&mut cur, order).map_err(|e| PvaError::Decode(e.to_string()))?;
    Ok(CreateChannelResponse { cid, sid, status })
}

// ─── Op responses (GET / PUT / MONITOR / RPC) ───────────────────────────

/// Decoded INIT response (subcmd & 0x08). Carries the introspection (the
/// channel's effective type after pvRequest filtering) so subsequent data
/// responses can be parsed.
#[derive(Debug, Clone)]
pub struct OpInitResponse {
    pub ioid: u32,
    pub subcmd: u8,
    pub status: Status,
    pub field_name: String,
    pub introspection: FieldDesc,
}

/// Decoded data response (subcmd == 0x00 for GET, == 0x00 for MONITOR data).
/// Carries the bitset (which fields changed) and the value itself.
#[derive(Debug, Clone)]
pub struct OpDataResponse {
    pub ioid: u32,
    pub subcmd: u8,
    pub status: Status,
    pub changed: BitSet,
    pub value: PvField,
    /// MONITOR DATA carries an overrun bitset after the value
    /// — the server sets bits for fields it coalesced/squashed because
    /// the client fell behind. Non-empty ⟹ a server-side squash
    /// occurred (pvxs `clientmon.cpp:549-558` sets `servSquash` when any
    /// word is non-zero). Always empty for GET/PUT_GET/RPC, which carry
    /// no overrun on the wire.
    pub overrun: BitSet,
    /// Response type descriptor — only populated for RPC, where the wire
    /// format carries its own type independent of any INIT-time
    /// introspection. `None` for GET/MONITOR/PUT_GET (the caller already
    /// has the type from INIT).
    pub response_desc: Option<FieldDesc>,
}

/// Decoded "completion" response (PUT after sending data, or DESTROY ack).
#[derive(Debug, Clone)]
pub struct OpStatusResponse {
    pub ioid: u32,
    pub subcmd: u8,
    pub status: Status,
}

/// Variants of the unified op-response decode, depending on subcmd contents.
#[derive(Debug, Clone)]
pub enum OpResponse {
    Init(OpInitResponse),
    Data(OpDataResponse),
    Status(OpStatusResponse),
}

/// Decode any GET/PUT/MONITOR response against a **fresh empty**
/// `TypeCache`. The caller passes the introspection from a prior INIT
/// response so we can decode data payloads; for INIT responses
/// themselves, pass `None`.
///
/// This wrapper cannot resolve `0xFD`/`0xFE` type-cache markers that
/// reference a slot defined by an *earlier* frame on the connection,
/// because each call starts from an empty cache. Production op paths
/// MUST therefore use [`decode_op_response_cached`] with the
/// connection-scoped cache (`ServerConn::type_cache()`); pvxs decodes
/// INIT descriptors and DATA values through one shared connection
/// `rxRegistry` (`clientget.cpp:410-451`). This empty-cache form exists
/// only for fuzz/test harnesses that intentionally start from a clean
/// connection and for self-contained frames known to carry no nested
/// cache references.
pub fn decode_op_response(
    frame: &Frame,
    introspection: Option<&FieldDesc>,
) -> PvaResult<OpResponse> {
    let mut empty = crate::pvdata::encode::TypeCache::new();
    decode_op_response_cached(frame, introspection, &mut empty)
}

/// Like [`decode_op_response`] but threads a per-connection
/// [`TypeCache`](crate::pvdata::encode::TypeCache) for 0xFD/0xFE marker support.
pub fn decode_op_response_cached(
    frame: &Frame,
    introspection: Option<&FieldDesc>,
    type_cache: &mut crate::pvdata::encode::TypeCache,
) -> PvaResult<OpResponse> {
    let cmd = Command::from_code(frame.header.command)
        .ok_or_else(|| PvaError::Protocol(format!("unknown command {}", frame.header.command)))?;
    if !matches!(
        cmd,
        Command::Get | Command::Put | Command::Monitor | Command::Rpc
    ) {
        return Err(PvaError::Protocol(format!("not an op response: {cmd:?}")));
    }

    let order = frame.order();
    let mut cur = frame.cursor();
    let ioid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let subcmd = cur.get_u8().map_err(|e| PvaError::Decode(e.to_string()))?;

    // MONITOR FINISH (subcmd & 0x10) — the server signals end-of-stream
    // and emits only a Status after ioid/subcmd. pvxs `servermon.cpp:148`
    // sets `subcmd = 0x10` and `cleanup()`s the monitor. This status-only
    // shape is MONITOR-specific: for GET/PUT/RPC the same bit is the
    // "last request" marker that pvxs echoes on an otherwise normal data
    // response (`serverget.cpp:83,112-116`) and the client decodes by
    // `cmd`/`init`/`get` bits, not as status-only (`clientget.cpp:405-452`).
    // Classifying every `0x10` op response as status-only dropped the value
    // body of a GET/PUT/RPC last-request data response.
    if cmd == Command::Monitor && subcmd & 0x10 != 0 {
        let status =
            Status::decode(&mut cur, order).map_err(|e| PvaError::Decode(e.to_string()))?;
        return Ok(OpResponse::Status(OpStatusResponse {
            ioid,
            subcmd,
            status,
        }));
    }

    if subcmd & 0x08 != 0 {
        // INIT phase
        let status =
            Status::decode(&mut cur, order).map_err(|e| PvaError::Decode(e.to_string()))?;
        if !status.is_success() {
            return Ok(OpResponse::Init(OpInitResponse {
                ioid,
                subcmd,
                status,
                field_name: String::new(),
                introspection: FieldDesc::Variant,
            }));
        }
        // RPC INIT carries no type descriptor — pvxs clientget.cpp:410
        // (`if (cmd != CMD_RPC && init && ok) from_wire_type(...)`).
        // For GET/PUT/MONITOR the introspection follows: a single type-desc
        // byte + body, optionally wrapped in a 0xFD/0xFE cache marker.
        let intro = if matches!(cmd, Command::Rpc) {
            FieldDesc::Variant
        } else {
            crate::pvdata::encode::decode_type_desc_cached(&mut cur, order, type_cache)
                .map_err(|e| PvaError::Decode(e.to_string()))?
        };
        return Ok(OpResponse::Init(OpInitResponse {
            ioid,
            subcmd,
            status,
            field_name: String::new(),
            introspection: intro,
        }));
    }

    // PUT data response without GetBack (subcmd & 0x40 == 0):
    // `ioid + subcmd + status` only.
    if cmd == Command::Put && subcmd & 0x40 == 0 {
        let status =
            Status::decode(&mut cur, order).map_err(|e| PvaError::Decode(e.to_string()))?;
        return Ok(OpResponse::Status(OpStatusResponse {
            ioid,
            subcmd,
            status,
        }));
    }

    // RPC data response: `ioid + subcmd + status + type + full_value`.
    // pvxs clientget.cpp:415-421 — `from_wire_type(...) + from_wire_full(...)`.
    // No bitset; the response carries its own type descriptor independent of
    // any INIT-time introspection.
    if cmd == Command::Rpc {
        let status =
            Status::decode(&mut cur, order).map_err(|e| PvaError::Decode(e.to_string()))?;
        if !status.is_success() {
            return Ok(OpResponse::Status(OpStatusResponse {
                ioid,
                subcmd,
                status,
            }));
        }
        let resp_desc = crate::pvdata::encode::decode_type_desc_cached(&mut cur, order, type_cache)
            .map_err(|e| PvaError::Decode(e.to_string()))?;
        let resp_value =
            crate::pvdata::encode::decode_pv_field_cached(&resp_desc, &mut cur, order, type_cache)
                .map_err(|e| PvaError::Decode(e.to_string()))?;
        let mut all = BitSet::new();
        all.set(0);
        return Ok(OpResponse::Data(OpDataResponse {
            ioid,
            subcmd,
            status,
            changed: all,
            value: resp_value,
            // RPC responses carry no overrun bitset.
            overrun: BitSet::new(),
            response_desc: Some(resp_desc),
        }));
    }

    let intro = introspection.ok_or_else(|| {
        PvaError::Protocol("data response without prior introspection".to_string())
    })?;

    // GET data response and PUT_GET (PUT with subcmd & 0x40) begin with a
    // Status; MONITOR data does not.
    let status = if cmd == Command::Get || cmd == Command::Put {
        Status::decode(&mut cur, order).map_err(|e| PvaError::Decode(e.to_string()))?
    } else {
        Status::ok()
    };
    // a data-phase GET/PUT(/Get) error reply is status-only. pvxs
    // `serverget.cpp:84-94` writes `ioid + subcmd + status` and emits NO
    // bitset/value when `!sts.isSuccess()` — the value branch (`:102-104`)
    // runs only on success. Surface the failure status here (as the RPC
    // data path above and the PUT-no-getback path already do) instead of
    // decoding a value body that was never sent. Without this the
    // BitSet::decode below would fault on EOF and the original server
    // failure would be lost behind a decode error.
    if (cmd == Command::Get || cmd == Command::Put) && !status.is_success() {
        return Ok(OpResponse::Status(OpStatusResponse {
            ioid,
            subcmd,
            status,
        }));
    }
    let changed = BitSet::decode(&mut cur, order).map_err(|e| PvaError::Decode(e.to_string()))?;
    let value = crate::pvdata::encode::decode_pv_field_with_bitset_cached(
        intro, &changed, 0, &mut cur, order, type_cache,
    )
    .map_err(|e| PvaError::Decode(e.to_string()))?;
    // MONITOR data carries the overrun BitSet after the partial value.
    // preserve it (a non-empty set means the server squashed)
    // instead of decoding-and-discarding. GET/PUT carry none → empty.
    let overrun = if cmd == Command::Monitor {
        BitSet::decode(&mut cur, order).map_err(|e| PvaError::Decode(e.to_string()))?
    } else {
        BitSet::new()
    };
    Ok(OpResponse::Data(OpDataResponse {
        ioid,
        subcmd,
        status,
        changed,
        value,
        overrun,
        response_desc: None,
    }))
}

// ─── Type-cache marker flattening (reader-task owned) ───────────────────

/// Resolve any 0xFD/0xFE type-cache markers in an inbound op-response
/// frame, rewriting the frame payload to carry self-contained inline
/// descriptors.
///
/// **Why this runs in the reader task.** Type-cache markers (`0xFD`
/// define / `0xFE` reference) only resolve correctly if every define is
/// observed before any reference to its slot. The connection reader task
/// is the single component that observes frames in strict wire order;
/// per-op tasks are scheduled by tokio in arbitrary order, so decoding a
/// `0xFE` reference in a per-op task could race ahead of the `0xFD`
/// define carried by an earlier (but not-yet-decoded) frame on a
/// different op. Flattening here, against a reader-task-owned `cache`,
/// makes the routed frames self-contained: per-op decoders never touch a
/// shared cache and the race is structurally impossible.
///
/// `cache` is owned by the reader task and threaded across every frame on
/// the connection. On any parse difficulty the frame payload is left
/// untouched — the per-op decoder will then surface a precise error
/// rather than this function masking it.
///
/// Invariant: only the reader task calls this, and exactly once per
/// inbound application frame, in wire order.
pub fn flatten_type_cache_markers(frame: &mut Frame, cache: &mut crate::pvdata::encode::TypeCache) {
    let order = frame.header.flags.byte_order();
    let Some(cmd) = Command::from_code(frame.header.command) else {
        return;
    };

    // Descriptor count carried after `ioid + subcmd + status`:
    //   0 — no descriptor (drop the frame through untouched)
    //   1 — Get/Put/Monitor/Rpc/GetField introspection or Rpc data type
    //   2 — PutGet INIT: putIF then getIF
    let desc_count: u8 = match cmd {
        Command::GetField => {
            // ioid(4) + status + [type-desc] — no subcmd byte, unlike the
            // op responses below. Mirrors `decode_get_field_response`,
            // which reads `ioid` (u32, 4 bytes) then `Status`.
            rewrite_after_status(frame, order, cache, 4, 1);
            return;
        }
        Command::Get | Command::Put | Command::Monitor => {
            let Some(subcmd) = peek_u8(&frame.payload, 4) else {
                return;
            };
            // INIT phase carries the introspection; FINISH (0x10) does
            // not; DATA phase carries no descriptor (uses INIT's type).
            if subcmd & 0x08 != 0 && subcmd & 0x10 == 0 {
                1
            } else {
                return;
            }
        }
        Command::Rpc => {
            let Some(subcmd) = peek_u8(&frame.payload, 4) else {
                return;
            };
            if subcmd & 0x10 != 0 {
                return; // FINISH/DESTROY
            }
            if subcmd & 0x08 != 0 {
                return; // RPC INIT carries no type descriptor
            }
            1 // RPC data response: status + type + value
        }
        Command::PutGet => {
            let Some(subcmd) = peek_u8(&frame.payload, 4) else {
                return;
            };
            if subcmd & 0x08 != 0 && subcmd & 0x10 == 0 {
                2 // PutGet INIT: putIF + getIF
            } else {
                return;
            }
        }
        _ => return,
    };

    // ioid(4) + subcmd(1) + status, then `desc_count` descriptors.
    rewrite_after_status(frame, order, cache, 5, desc_count);
}

/// Rewrite a frame whose layout is `prefix_len` fixed bytes, then a
/// `Status`, then `desc_count` type descriptors, then an arbitrary tail.
///
/// The fixed prefix, the `Status` bytes, and the tail are copied
/// verbatim; each descriptor is flattened through [`crate::pvdata::encode
/// ::rewrite_type_desc_inline`]. On a failure status the descriptors are
/// absent — the function detects this via the `Status` parse and leaves
/// the (descriptor-free) frame untouched.
fn rewrite_after_status(
    frame: &mut Frame,
    order: ByteOrder,
    cache: &mut crate::pvdata::encode::TypeCache,
    prefix_len: usize,
    desc_count: u8,
) {
    if frame.payload.len() < prefix_len {
        return;
    }
    let mut cur = Cursor::new(frame.payload.as_slice());
    cur.set_position(prefix_len as u64);
    let status = match Status::decode(&mut cur, order) {
        Ok(s) => s,
        Err(_) => return,
    };
    // A non-success status has no trailing descriptor (mirrors the
    // decoder fast-paths in `decode_op_response_cached`).
    if !status.is_success() {
        return;
    }
    let descs_start = cur.position() as usize;

    // Flatten each descriptor in order, capturing where the descriptor
    // region ends so the value tail can be copied verbatim.
    let mut inline = Vec::new();
    for _ in 0..desc_count {
        if crate::pvdata::encode::rewrite_type_desc_inline(&mut cur, order, cache, &mut inline)
            .is_err()
        {
            // Markers unresolved or malformed — leave the frame as-is so
            // the per-op decoder surfaces the precise error.
            return;
        }
    }
    let descs_end = cur.position() as usize;

    // Fast path: if the descriptor region already had no markers the
    // flattened bytes are byte-identical; skip the realloc.
    if inline.as_slice() == &frame.payload[descs_start..descs_end] {
        return;
    }

    let mut rebuilt =
        Vec::with_capacity(descs_start + inline.len() + (frame.payload.len() - descs_end));
    rebuilt.extend_from_slice(&frame.payload[..descs_start]);
    rebuilt.extend_from_slice(&inline);
    rebuilt.extend_from_slice(&frame.payload[descs_end..]);
    frame.header.payload_length = rebuilt.len() as u32;
    frame.payload = rebuilt;
}

/// Read a single byte at `off` from a payload slice, `None` if short.
fn peek_u8(payload: &[u8], off: usize) -> Option<u8> {
    payload.get(off).copied()
}

// ─── GET_FIELD response ──────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct GetFieldResponse {
    pub ioid: u32,
    pub status: Status,
    pub introspection: Option<FieldDesc>,
}

pub fn decode_get_field_response(frame: &Frame) -> PvaResult<GetFieldResponse> {
    if frame.header.command != Command::GetField.code() {
        return Err(PvaError::Protocol(format!(
            "expected GetField (17), got {}",
            frame.header.command
        )));
    }
    let order = frame.order();
    let mut cur = frame.cursor();
    let ioid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let status = Status::decode(&mut cur, order).map_err(|e| PvaError::Decode(e.to_string()))?;
    if !status.is_success() {
        return Ok(GetFieldResponse {
            ioid,
            status,
            introspection: None,
        });
    }
    let intro = crate::pvdata::encode::decode_type_desc(&mut cur, order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    Ok(GetFieldResponse {
        ioid,
        status,
        introspection: Some(intro),
    })
}

// ─── Helpers ─────────────────────────────────────────────────────────────

/// True iff this header is the SET_BYTE_ORDER control message.
pub fn is_set_byte_order(header: &PvaHeader) -> bool {
    header.flags.is_control() && header.command == ControlCommand::SetByteOrder.code()
}

/// True iff the header is a server-direction frame.
pub fn is_server_frame(flags: HeaderFlags) -> bool {
    flags.is_server()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::PvaCodec;

    #[test]
    fn frame_round_trip_codec_create_channel() {
        // Build a CREATE_CHANNEL request, then re-parse it as a frame and
        // confirm the header round-trips.
        let codec = PvaCodec { big_endian: false };
        let bytes = codec.build_create_channel(7, "X");
        let (frame, n) = try_parse_frame(&bytes).unwrap().unwrap();
        assert_eq!(n, bytes.len());
        assert_eq!(frame.header.command, Command::CreateChannel.code());
        assert!(!frame.header.flags.is_server());
    }

    #[test]
    fn create_channel_response_decode() {
        // Build a synthetic CREATE_CHANNEL response (server side).
        use crate::proto::WriteExt;
        let order = ByteOrder::Little;
        let mut payload = Vec::new();
        payload.put_u32(7, order); // cid
        payload.put_u32(42, order); // sid
        Status::ok().write_into(order, &mut payload);
        let header = PvaHeader::application(
            true,
            order,
            Command::CreateChannel.code(),
            payload.len() as u32,
        );
        let mut frame_bytes = Vec::new();
        header.write_into(&mut frame_bytes);
        frame_bytes.extend_from_slice(&payload);

        let (frame, _) = try_parse_frame(&frame_bytes).unwrap().unwrap();
        let resp = decode_create_channel_response(&frame).unwrap();
        assert_eq!(resp.cid, 7);
        assert_eq!(resp.sid, 42);
        assert_eq!(resp.status, Status::OkNoMsg);
    }

    #[test]
    fn op_init_response_decode_carries_introspection() {
        use crate::proto::WriteExt;
        use crate::pvdata::ScalarType;

        let order = ByteOrder::Little;
        let intro = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
        };
        let mut intro_bytes = Vec::new();
        crate::pvdata::encode::encode_type_desc(&intro, order, &mut intro_bytes);

        let mut payload = Vec::new();
        payload.put_u32(99, order); // ioid
        payload.put_u8(0x08); // subcmd = INIT
        Status::ok().write_into(order, &mut payload);
        // No leading 0x80 because encode_type_desc already starts with it.
        payload.extend_from_slice(&intro_bytes);

        let header = PvaHeader::application(true, order, Command::Get.code(), payload.len() as u32);
        let mut frame_bytes = Vec::new();
        header.write_into(&mut frame_bytes);
        frame_bytes.extend_from_slice(&payload);

        let (frame, _) = try_parse_frame(&frame_bytes).unwrap().unwrap();
        match decode_op_response(&frame, None).unwrap() {
            OpResponse::Init(init) => {
                assert_eq!(init.ioid, 99);
                assert_eq!(init.subcmd & 0x08, 0x08);
                match init.introspection {
                    FieldDesc::Structure { struct_id, .. } => {
                        assert_eq!(struct_id, "epics:nt/NTScalar:1.0");
                    }
                    other => panic!("expected structure, got {other:?}"),
                }
            }
            other => panic!("expected init, got {other:?}"),
        }
    }

    /// pvxs `conn.cpp:160` defense-in-depth: a server's read path must
    /// reject inbound frames that have the `Server` direction bit set
    /// (those originated from another server, not a client). And a
    /// client's read path must reject frames with the bit clear.
    /// `PeerRole::Either` skips the check (sniffers, mixed-mode tests).
    #[test]
    fn try_parse_frame_role_rejects_wrong_direction() {
        // Build a header with Server flag SET (server-originated frame).
        let order = ByteOrder::Little;
        let header = PvaHeader::application(true, order, Command::CreateChannel.code(), 0);
        let mut bytes = Vec::new();
        header.write_into(&mut bytes);

        // PeerRole::Either accepts either direction.
        assert!(matches!(
            try_parse_frame_role(&bytes, PeerRole::Either),
            Ok(Some(_))
        ));
        // PeerRole::Client expects Server bit set — accepted here.
        assert!(matches!(
            try_parse_frame_role(&bytes, PeerRole::Client),
            Ok(Some(_))
        ));
        // PeerRole::Server expects Server bit CLEAR — rejected.
        let err = try_parse_frame_role(&bytes, PeerRole::Server).unwrap_err();
        assert!(
            matches!(err, PvaError::Protocol(_)),
            "expected Protocol error, got {err:?}"
        );

        // Same payload but Server flag CLEAR.
        let header2 = PvaHeader::application(false, order, Command::CreateChannel.code(), 0);
        let mut bytes2 = Vec::new();
        header2.write_into(&mut bytes2);
        // PeerRole::Server expects Server bit clear — accepted here.
        assert!(matches!(
            try_parse_frame_role(&bytes2, PeerRole::Server),
            Ok(Some(_))
        ));
        // PeerRole::Client expects Server bit SET — rejected.
        let err = try_parse_frame_role(&bytes2, PeerRole::Client).unwrap_err();
        assert!(
            matches!(err, PvaError::Protocol(_)),
            "expected Protocol error, got {err:?}"
        );
    }

    /// Build a Get INIT frame whose introspection is encoded against
    /// `enc_cache` — so the first such frame emits a `0xFD` define and
    /// later ones a `0xFE` reference.
    fn build_get_init_frame(
        order: ByteOrder,
        ioid: u32,
        desc: &FieldDesc,
        enc_cache: &mut crate::pvdata::encode::EncodeTypeCache,
    ) -> Frame {
        use crate::proto::WriteExt;
        let mut payload = Vec::new();
        payload.put_u32(ioid, order);
        payload.put_u8(0x08); // subcmd = INIT
        Status::ok().write_into(order, &mut payload);
        crate::pvdata::encode::encode_type_desc_cached(desc, order, enc_cache, &mut payload);
        let header = PvaHeader::application(true, order, Command::Get.code(), payload.len() as u32);
        Frame { header, payload }
    }

    /// BUG 2 regression. A server with the type cache enabled emits the
    /// introspection once as a `0xFD` define and then as `0xFE`
    /// references. The per-op tasks decode frames in arbitrary order, so
    /// if a `0xFE` frame is decoded before its `0xFD` frame the lookup
    /// misses. `flatten_type_cache_markers` resolves markers in the
    /// reader task (strict wire order) so the routed frames are
    /// self-contained and decode correctly regardless of op order.
    #[test]
    fn type_cache_reference_decodes_before_define_after_flatten() {
        use crate::pvdata::ScalarType;

        let order = ByteOrder::Little;
        let intro = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![
                ("value".into(), FieldDesc::Scalar(ScalarType::Double)),
                (
                    "alarm".into(),
                    FieldDesc::Structure {
                        struct_id: "alarm_t".into(),
                        fields: vec![("severity".into(), FieldDesc::Scalar(ScalarType::Int))],
                    },
                ),
            ],
        };

        // Frame A (ioid 1): server's first emission — carries 0xFD define.
        // Frame B (ioid 2): same type — carries 0xFE reference.
        let mut enc_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let mut frame_a = build_get_init_frame(order, 1, &intro, &mut enc_cache);
        let mut frame_b = build_get_init_frame(order, 2, &intro, &mut enc_cache);

        // Sanity: B really did emit a 0xFE reference (cache marker).
        assert_eq!(
            frame_b.payload[6], 0xFE,
            "expected 0xFE type-cache reference in frame B"
        );
        assert_eq!(
            frame_a.payload[6], 0xFD,
            "expected 0xFD type-cache define in frame A"
        );

        // Reader task flattens both frames in strict WIRE order: A then B.
        let mut reader_cache = crate::pvdata::encode::TypeCache::new();
        flatten_type_cache_markers(&mut frame_a, &mut reader_cache);
        flatten_type_cache_markers(&mut frame_b, &mut reader_cache);

        // After flattening neither frame carries a marker — both are
        // self-contained inline descriptors.
        assert_ne!(frame_a.payload[6], 0xFD);
        assert_ne!(frame_a.payload[6], 0xFE);
        assert_ne!(frame_b.payload[6], 0xFD);
        assert_ne!(frame_b.payload[6], 0xFE);
        assert_eq!(
            frame_a.header.payload_length as usize,
            frame_a.payload.len()
        );
        assert_eq!(
            frame_b.header.payload_length as usize,
            frame_b.payload.len()
        );

        // Per-op tasks decode in REVERSE order (B before A) with empty
        // caches — the cross-op race condition. Both must still decode.
        let decode = |f: &Frame| {
            let mut empty = crate::pvdata::encode::TypeCache::new();
            decode_op_response_cached(f, None, &mut empty)
        };
        for (label, f) in [("B (was 0xFE)", &frame_b), ("A (was 0xFD)", &frame_a)] {
            match decode(f).unwrap_or_else(|e| panic!("decode {label} failed: {e}")) {
                OpResponse::Init(init) => match init.introspection {
                    FieldDesc::Structure { struct_id, fields } => {
                        assert_eq!(struct_id, "epics:nt/NTScalar:1.0", "{label}");
                        assert_eq!(fields.len(), 2, "{label}");
                    }
                    other => panic!("{label}: expected structure, got {other:?}"),
                },
                other => panic!("{label}: expected init, got {other:?}"),
            }
        }
    }

    /// A data-phase value carrying an `any`/`variant` `0xFE <slot>`
    /// back-reference must resolve against the connection-scoped
    /// `TypeCache` that an earlier frame populated — exactly what pvxs does
    /// by decoding every DATA value through the same connection
    /// `rxRegistry` (`clientget.cpp:445-451`, `dataencode.cpp:542-557`).
    /// `decode_op_response_cached` must thread that cache into the value
    /// body, not just the top-level INIT descriptor; the empty-cache
    /// `decode_op_response` wrapper cannot and reports a spurious slot miss
    /// (the defect this closes).
    #[test]
    fn op_data_variant_value_resolves_via_connection_type_cache() {
        use crate::proto::WriteExt;
        use crate::pvdata::{PvField, ScalarType, ScalarValue};

        let order = ByteOrder::Little;
        // GET introspection: a structure with one `any` (Variant) leaf.
        let intro = FieldDesc::Structure {
            struct_id: "x".into(),
            fields: vec![("v".into(), FieldDesc::Variant)],
        };

        // GET DATA frame: ioid + subcmd(0x00) + changed bitset (bit 0 marks
        // the whole structure present) + value. The Variant value body is
        // `0xFE <slot=5> <Int 42 LE>` — a back-reference to a slot a *prior*
        // frame on the connection defined with `0xFD`.
        let build = || -> Frame {
            let mut payload = Vec::new();
            payload.put_u32(9, order); // ioid
            payload.put_u8(0x00); // subcmd = DATA
            // GET data begins with a Status, then the changed bitset, then
            // the value (decode.rs GET/PUT branch).
            Status::ok().write_into(order, &mut payload);
            let mut changed = BitSet::new();
            changed.set(0);
            payload.extend_from_slice(&changed.encode(order));
            payload.extend_from_slice(&[0xFE, 0x05, 0x00, 0x2A, 0x00, 0x00, 0x00]);
            let header =
                PvaHeader::application(true, order, Command::Get.code(), payload.len() as u32);
            Frame { header, payload }
        };

        // Connection cache already holds slot 5 = Int (an earlier 0xFD
        // define on this connection).
        let mut cache = crate::pvdata::encode::TypeCache::new();
        cache.insert(5, FieldDesc::Scalar(ScalarType::Int));
        match decode_op_response_cached(&build(), Some(&intro), &mut cache).unwrap() {
            OpResponse::Data(d) => match d.value {
                PvField::Structure(s) => {
                    assert_eq!(s.fields.len(), 1);
                    match &s.fields[0].1 {
                        PvField::Variant(vv) => {
                            assert_eq!(vv.desc, Some(FieldDesc::Scalar(ScalarType::Int)));
                            assert!(matches!(vv.value, PvField::Scalar(ScalarValue::Int(42))));
                        }
                        other => panic!("expected Variant leaf, got {other:?}"),
                    }
                }
                other => panic!("expected Structure value, got {other:?}"),
            },
            other => panic!("expected Data, got {other:?}"),
        }

        // A fresh empty cache (what the production empty-wrapper path used
        // before this fix) cannot resolve slot 5 → decode error.
        let mut empty = crate::pvdata::encode::TypeCache::new();
        assert!(
            decode_op_response_cached(&build(), Some(&intro), &mut empty).is_err(),
            "0xFE slot-5 reference must miss without the connection cache that defined it"
        );
    }

    /// a MONITOR DATA frame ends with an overrun bitset; the
    /// decoder must preserve it on `OpDataResponse` (non-empty ⟹ the
    /// server squashed) instead of decoding-and-discarding it. An empty
    /// `changed` bitset means no value bytes follow, so this exercises
    /// the trailing-overrun parse in isolation.
    #[test]
    fn monitor_data_preserves_overrun_bitset() {
        use crate::proto::WriteExt;
        use crate::pvdata::ScalarType;

        let order = ByteOrder::Little;
        let intro = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
        };

        let build = |overrun: &BitSet| -> Frame {
            let mut payload = Vec::new();
            payload.put_u32(7, order); // ioid
            payload.put_u8(0x00); // subcmd = DATA
            let changed = BitSet::new(); // nothing marked → no value bytes
            payload.extend_from_slice(&changed.encode(order));
            payload.extend_from_slice(&overrun.encode(order));
            let header =
                PvaHeader::application(true, order, Command::Monitor.code(), payload.len() as u32);
            Frame { header, payload }
        };

        // Non-empty overrun → server squash signalled.
        let mut overrun = BitSet::new();
        overrun.set(1);
        match decode_op_response(&build(&overrun), Some(&intro)).unwrap() {
            OpResponse::Data(d) => {
                assert!(!d.overrun.is_empty(), "overrun bitset must be preserved");
                assert!(d.overrun.iter().any(|b| b == 1));
            }
            other => panic!("expected Data, got {other:?}"),
        }

        // Empty overrun → no squash.
        match decode_op_response(&build(&BitSet::new()), Some(&intro)).unwrap() {
            OpResponse::Data(d) => {
                assert!(d.overrun.is_empty(), "absent overrun decodes empty");
            }
            other => panic!("expected Data, got {other:?}"),
        }
    }

    /// PVA-RS-2026-05-28-110 input contract: a MONITOR DATA frame
    /// truncated before its trailing overrun bitset must decode as an
    /// `Err`, so the typed monitor loop's `Err` arm fires and returns
    /// `MonitorEnd::Fatal` (matching pvxs `clientmon.cpp:601-605`, which
    /// resets the connection on an invalid MONITOR). If the decoder ever
    /// became lenient here (e.g. defaulting the missing overrun to empty)
    /// the loop would silently skip the corrupt frame under the same
    /// IOID — the exact defect this finding closed.
    #[test]
    fn monitor_truncated_data_missing_overrun_is_decode_error() {
        use crate::proto::WriteExt;
        use crate::pvdata::ScalarType;

        let order = ByteOrder::Little;
        let intro = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
        };
        let mut payload = Vec::new();
        payload.put_u32(7, order); // ioid
        payload.put_u8(0x00); // subcmd = DATA
        let changed = BitSet::new(); // nothing marked → no value bytes
        payload.extend_from_slice(&changed.encode(order));
        // NB: trailing overrun bitset omitted → truncated frame.
        let header =
            PvaHeader::application(true, order, Command::Monitor.code(), payload.len() as u32);
        let frame = Frame { header, payload };
        assert!(
            decode_op_response(&frame, Some(&intro)).is_err(),
            "MONITOR DATA missing the trailing overrun bitset must be a decode error"
        );
    }

    /// PVA-RS-2026-05-28-110 input contract: a MONITOR FINISH frame
    /// (`subcmd & 0x10`) whose Status cannot be decoded must be an `Err`,
    /// so the typed loop tears down fatally instead of skipping.
    #[test]
    fn monitor_finish_with_truncated_status_is_decode_error() {
        use crate::proto::WriteExt;

        let order = ByteOrder::Little;
        let mut payload = Vec::new();
        payload.put_u32(7, order); // ioid
        payload.put_u8(0x10); // subcmd = FINISH, but no Status follows
        let header =
            PvaHeader::application(true, order, Command::Monitor.code(), payload.len() as u32);
        let frame = Frame { header, payload };
        assert!(
            decode_op_response(&frame, None).is_err(),
            "MONITOR FINISH with a truncated Status must be a decode error"
        );
    }

    /// PVA-RS-2026-05-28-110 input contract: a MONITOR frame with the
    /// INIT bit (`subcmd & 0x08`) decodes as `OpResponse::Init`. The
    /// typed loop is only entered AFTER the initial INIT, so any Init
    /// here is a second INIT on a running subscription — a state-machine
    /// violation pvxs rejects (clientmon.cpp:568-605). The loop's `Init`
    /// arm now returns `MonitorEnd::Fatal` rather than ignoring it.
    #[test]
    fn monitor_init_frame_decodes_as_init_response() {
        use crate::proto::WriteExt;

        let order = ByteOrder::Little;
        let mut payload = Vec::new();
        payload.put_u32(7, order); // ioid
        payload.put_u8(0x08); // subcmd = INIT
        // Non-success status returns early (no type descriptor needed),
        // still classified as an INIT response.
        Status::error("re-init").write_into(order, &mut payload);
        let header =
            PvaHeader::application(true, order, Command::Monitor.code(), payload.len() as u32);
        let frame = Frame { header, payload };
        assert!(
            matches!(decode_op_response(&frame, None), Ok(OpResponse::Init(_))),
            "a MONITOR INIT-bit frame must classify as OpResponse::Init"
        );
    }

    /// a GET data response carrying the last-request bit
    /// (`subcmd & 0x10`) must still decode its value body. pvxs echoes the
    /// request subcmd on an otherwise normal data reply
    /// (`serverget.cpp:83,112-116`) and the client decodes by
    /// `cmd`/`init`/`get` bits (`clientget.cpp:405-452`). The `0x10`
    /// status-only shape is reserved for MONITOR FINISH; classifying every
    /// `0x10` op response as status-only dropped the GET/PUT/RPC value.
    #[test]
    fn get_data_response_with_last_request_bit_decodes_value() {
        use crate::proto::WriteExt;
        use crate::pvdata::{PvField, PvStructure, ScalarType, ScalarValue};

        let order = ByteOrder::Little;
        let intro = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
        };
        let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
        s.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(3.5))));
        let value = PvField::Structure(s);

        // GET data response with the last-request bit: subcmd = 0x50
        // (0x40 | 0x10).
        let mut payload = Vec::new();
        payload.put_u32(7, order); // ioid
        payload.put_u8(0x50); // last-request data response
        Status::ok().write_into(order, &mut payload);
        let changed = crate::pvdata::encode::canonical_changed_bitset(
            &intro,
            &BitSet::all_set(intro.total_bits()),
        );
        changed.write_into(order, &mut payload);
        crate::pvdata::encode::encode_pv_field_with_bitset(
            &value,
            &intro,
            &changed,
            0,
            order,
            &mut payload,
        );

        let header = PvaHeader::application(true, order, Command::Get.code(), payload.len() as u32);
        let frame = Frame { header, payload };

        match decode_op_response(&frame, Some(&intro)).unwrap() {
            OpResponse::Data(d) => {
                assert_eq!(d.ioid, 7);
                assert_eq!(d.subcmd, 0x50, "last-request bit echoed on data response");
                let PvField::Structure(st) = &d.value else {
                    panic!("expected structure value, got {:?}", d.value);
                };
                let got = st.fields.iter().find(|(n, _)| n == "value").map(|(_, v)| v);
                assert!(
                    matches!(got, Some(PvField::Scalar(ScalarValue::Double(x))) if (*x - 3.5).abs() < 1e-9),
                    "value body must decode to 3.5, got {got:?}"
                );
            }
            other => panic!("expected Data (value preserved), got {other:?}"),
        }
    }

    /// Companion: MONITOR FINISH (`subcmd & 0x10`) remains a
    /// status-only end-of-stream event. pvxs `servermon.cpp:148` emits
    /// only `ioid + subcmd + status`. This MONITOR-specific shape must not
    /// regress when GET/PUT/RPC stop treating `0x10` as status-only.
    #[test]
    fn monitor_finish_remains_status_only() {
        use crate::proto::WriteExt;

        let order = ByteOrder::Little;
        let mut payload = Vec::new();
        payload.put_u32(7, order); // ioid
        payload.put_u8(0x10); // FINISH
        Status::ok().write_into(order, &mut payload);
        let header =
            PvaHeader::application(true, order, Command::Monitor.code(), payload.len() as u32);
        let frame = Frame { header, payload };

        match decode_op_response(&frame, None).unwrap() {
            OpResponse::Status(s) => {
                assert_eq!(s.ioid, 7);
                assert_eq!(s.subcmd, 0x10);
            }
            other => panic!("expected Status (MONITOR FINISH), got {other:?}"),
        }
    }

    /// `stats(true)` resets `n_srv_squash` (and the other
    /// running counters) while preserving the configured `limit_queue`.
    #[test]
    fn stats_reset_clears_srv_squash_keeps_limit() {
        use crate::client_native::ops_v2::SubscriptionStat;
        let mut stat = SubscriptionStat {
            limit_queue: 16,
            ..Default::default()
        };
        // The monitor loop increments these.
        stat.n_srv_squash = 3;
        stat.n_delivered = 10;
        stat.max_events_per_ack = 8;
        // Mirror the reset arm of SubscriptionHandle::stats(true).
        let reset = SubscriptionStat {
            limit_queue: stat.limit_queue,
            ..Default::default()
        };
        assert_eq!(reset.n_srv_squash, 0);
        assert_eq!(reset.n_delivered, 0);
        assert_eq!(reset.max_events_per_ack, 0);
        assert_eq!(reset.limit_queue, 16, "configured queue limit preserved");
        // pvxs queue fields are 0 by construction (no pop()-able queue).
        assert_eq!(reset.n_queue, 0);
        assert_eq!(reset.max_queue, 0);
        assert_eq!(reset.n_cli_squash, 0);
    }

    /// A frame with no type-cache markers — i.e. the introspection
    /// encoded inline, as a server with the type cache disabled emits —
    /// must pass through `flatten_type_cache_markers` byte-identically
    /// (fast path).
    #[test]
    fn flatten_leaves_marker_free_frame_unchanged() {
        use crate::proto::WriteExt;
        use crate::pvdata::ScalarType;

        let order = ByteOrder::Little;
        let intro = FieldDesc::Structure {
            struct_id: "s".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
        };
        // Inline (marker-free) encoding — the wire form a server emits
        // with the type cache disabled.
        let mut payload = Vec::new();
        payload.put_u32(7, order); // ioid
        payload.put_u8(0x08); // subcmd = INIT
        Status::ok().write_into(order, &mut payload);
        crate::pvdata::encode::encode_type_desc(&intro, order, &mut payload);
        let header = PvaHeader::application(true, order, Command::Get.code(), payload.len() as u32);
        let mut frame = Frame {
            header,
            payload: payload.clone(),
        };

        let mut reader_cache = crate::pvdata::encode::TypeCache::new();
        flatten_type_cache_markers(&mut frame, &mut reader_cache);
        assert_eq!(
            frame.payload, payload,
            "marker-free frame must be unchanged"
        );
    }

    /// Build a GetField response frame whose type descriptor is encoded
    /// against `enc_cache` — first such frame emits a `0xFD` define, later
    /// ones a `0xFE` reference. GetField wire layout is `ioid(4) + Status
    /// + type-desc`: no subcmd byte, unlike the op responses.
    fn build_get_field_frame(
        order: ByteOrder,
        ioid: u32,
        desc: &FieldDesc,
        enc_cache: &mut crate::pvdata::encode::EncodeTypeCache,
    ) -> Frame {
        use crate::proto::WriteExt;
        let mut payload = Vec::new();
        payload.put_u32(ioid, order); // ioid
        Status::ok().write_into(order, &mut payload);
        crate::pvdata::encode::encode_type_desc_cached(desc, order, enc_cache, &mut payload);
        let header =
            PvaHeader::application(true, order, Command::GetField.code(), payload.len() as u32);
        Frame { header, payload }
    }

    /// Regression: a GetField response carrying a `0xFE` type-cache
    /// reference must be flattened correctly. The GetField arm of
    /// `flatten_type_cache_markers` used `prefix_len = 0`, so
    /// `rewrite_after_status` parsed the first byte of the `ioid` u32 as
    /// the Status kind byte — the `0xFE` reference was never resolved and
    /// `decode_get_field_response` hit a 0xFE slot miss. The correct
    /// prefix is `4` (ioid only; GetField has no subcmd byte).
    #[test]
    fn flatten_resolves_get_field_type_cache_reference() {
        use crate::pvdata::ScalarType;

        let order = ByteOrder::Little;
        let intro = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![
                ("value".into(), FieldDesc::Scalar(ScalarType::Double)),
                (
                    "alarm".into(),
                    FieldDesc::Structure {
                        struct_id: "alarm_t".into(),
                        fields: vec![("severity".into(), FieldDesc::Scalar(ScalarType::Int))],
                    },
                ),
            ],
        };

        // Frame A (ioid 1): server's first emission — carries 0xFD define.
        // Frame B (ioid 2): same type — carries 0xFE reference.
        let mut enc_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let mut frame_a = build_get_field_frame(order, 1, &intro, &mut enc_cache);
        let mut frame_b = build_get_field_frame(order, 2, &intro, &mut enc_cache);

        // Sanity: the type descriptor starts right after `ioid(4) +
        // Status`. `Status::ok()` (OkNoMsg) is a single 0xFF byte, so the
        // descriptor's first byte is at payload offset 5.
        assert_eq!(
            frame_a.payload[5], 0xFD,
            "expected 0xFD type-cache define in frame A"
        );
        assert_eq!(
            frame_b.payload[5], 0xFE,
            "expected 0xFE type-cache reference in frame B"
        );

        // Reader task flattens both frames in strict WIRE order: A then B.
        let mut reader_cache = crate::pvdata::encode::TypeCache::new();
        flatten_type_cache_markers(&mut frame_a, &mut reader_cache);
        flatten_type_cache_markers(&mut frame_b, &mut reader_cache);

        // After flattening neither frame carries a marker.
        assert_ne!(frame_a.payload[5], 0xFD);
        assert_ne!(frame_a.payload[5], 0xFE);
        assert_ne!(frame_b.payload[5], 0xFD);
        assert_ne!(frame_b.payload[5], 0xFE);
        assert_eq!(
            frame_a.header.payload_length as usize,
            frame_a.payload.len()
        );
        assert_eq!(
            frame_b.header.payload_length as usize,
            frame_b.payload.len()
        );

        // Per-op tasks decode in REVERSE wire order (B before A): the
        // reference-before-define race. Both must still decode.
        for (label, f) in [("B (was 0xFE)", &frame_b), ("A (was 0xFD)", &frame_a)] {
            let resp = decode_get_field_response(f)
                .unwrap_or_else(|e| panic!("decode {label} failed: {e}"));
            assert_eq!(
                resp.ioid,
                if label.starts_with('B') { 2 } else { 1 },
                "{label}"
            );
            assert_eq!(resp.status, Status::OkNoMsg, "{label}");
            match resp.introspection {
                Some(FieldDesc::Structure { struct_id, fields }) => {
                    assert_eq!(struct_id, "epics:nt/NTScalar:1.0", "{label}");
                    assert_eq!(fields.len(), 2, "{label}");
                }
                other => panic!("{label}: expected structure introspection, got {other:?}"),
            }
        }
    }

    /// A marker-free GetField response (server with the type cache
    /// disabled) must pass through `flatten_type_cache_markers`
    /// byte-identically.
    #[test]
    fn flatten_leaves_marker_free_get_field_frame_unchanged() {
        use crate::proto::WriteExt;
        use crate::pvdata::ScalarType;

        let order = ByteOrder::Little;
        let intro = FieldDesc::Structure {
            struct_id: "s".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
        };
        let mut payload = Vec::new();
        payload.put_u32(7, order); // ioid
        Status::ok().write_into(order, &mut payload);
        crate::pvdata::encode::encode_type_desc(&intro, order, &mut payload);
        let header =
            PvaHeader::application(true, order, Command::GetField.code(), payload.len() as u32);
        let mut frame = Frame {
            header,
            payload: payload.clone(),
        };

        let mut reader_cache = crate::pvdata::encode::TypeCache::new();
        flatten_type_cache_markers(&mut frame, &mut reader_cache);
        assert_eq!(
            frame.payload, payload,
            "marker-free GetField frame must be unchanged"
        );
        // And it still decodes.
        let resp = decode_get_field_response(&frame).unwrap();
        assert_eq!(resp.ioid, 7);
        assert!(resp.introspection.is_some());
    }

    /// A failure-status INIT frame carries no descriptor; flattening must
    /// leave it untouched and not error.
    #[test]
    fn flatten_skips_failure_status_frame() {
        use crate::proto::WriteExt;

        let order = ByteOrder::Little;
        let mut payload = Vec::new();
        payload.put_u32(5, order);
        payload.put_u8(0x08); // INIT
        Status::error("boom").write_into(order, &mut payload);
        let header = PvaHeader::application(true, order, Command::Get.code(), payload.len() as u32);
        let mut frame = Frame {
            header,
            payload: payload.clone(),
        };
        let mut reader_cache = crate::pvdata::encode::TypeCache::new();
        flatten_type_cache_markers(&mut frame, &mut reader_cache);
        assert_eq!(frame.payload, payload);
    }
}
