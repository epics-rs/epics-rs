//! PVA response decoder — frames and the server→client application
//! messages.
//!
//! Reads bytes off the wire frame-by-frame, dispatching to the correct
//! application-message decoder. Pure data — no I/O, no sockets, no tokio —
//! so it's exhaustively unit-testable.
//!
//! Lived under `client_native` until 2026-07-21. It is the *client's*
//! decoder by direction (it parses what a server sends), but it is not
//! client I/O: `server_native::tcp` frames its own reads with
//! [`try_parse_frame_role`], `server_native::peers` accounts received frames
//! with [`Frame`], and both server unit tests and the fuzz targets decode
//! replies with it. Keeping it inside the client I/O modules meant a
//! server-only build (design doc §9 phase 6, item 2) had to drag the whole
//! client — search engine, connection pool, operation tasks — along for a
//! pure codec. `client_native::decode` remains as a re-export so the
//! existing public path keeps resolving.

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
/// This is the production op-decode entry. It is sound with an empty
/// cache because the reader side ([`flatten_type_cache_markers`]) has
/// already rewritten every `0xFD`/`0xFE` type-cache marker — both the
/// INIT descriptor and any `any`/`variant` value markers — into a single
/// self-contained frame in wire order before the frame ever reaches an op
/// task. With the
/// markers resolved into inline types, no shared connection-level registry
/// is needed here, so op tasks carry no cross-frame decode state and cannot
/// race over a shared cache. pvxs keeps one `rxRegistry` per connection
/// (`clientget.cpp:410-451`); we move that single-owner resolution entirely
/// to the reader so the per-op decode is pure.
///
/// [`decode_op_response_cached`] remains for the value-decode plumbing
/// (which threads a `&mut TypeCache` down to `decode_pv_field_*_cached`)
/// and for tests that intentionally pre-seed a cache to exercise the
/// marker path directly.
///
/// [`flatten_type_cache_markers`]: crate::decode::flatten_type_cache_markers
pub fn decode_op_response(
    frame: &Frame,
    introspection: Option<&FieldDesc>,
) -> PvaResult<OpResponse> {
    let mut empty = crate::pvdata::encode::TypeCache::new();
    decode_op_response_cached(frame, introspection, &mut empty)
}

/// The MONITOR FINISH rule, and its only owner.
///
/// A final MONITOR frame (`subcmd & 0x10`) always carries a `Status` after
/// `ioid + subcmd`, and pvxs still decodes a trailing update from it whenever
/// bytes remain: `clientmon.cpp:504-511` runs
/// `if(!sts.isSuccess()) { } else if(init) { … } else if(!final || !M.empty())
/// { … from_wire_valid(M, rxRegistry, data); from_wire(M, overrun); }`. The
/// update is queued and only then is the `Finished()` marker appended
/// (`clientmon.cpp:692-707`), so a subscriber sees the last update followed by
/// the end of stream. `servermon.cpp:176-178` is the server side of the same
/// shape.
///
/// Returns the FINISH `Status` plus the offset at which its trailing
/// `changed | value | overrun` body begins, or `None` when there is no body.
/// The arm order above is what makes a body absent: a failed status carries
/// none, an INIT frame's post-status bytes are a type descriptor rather than an
/// update (pvxs's `else if(init)` wins over the update arm), and an empty tail
/// is the ordinary status-only FINISH.
///
/// Every consumer of a FINISH frame — the typed decode below, the reader-task
/// marker flattening ([`flatten_type_cache_markers`]), and the raw-forwarding
/// monitor loop — asks this instead of re-deriving "is there a body?", so the
/// three cannot disagree about whether a final update exists.
///
/// [`flatten_type_cache_markers`]: crate::decode::flatten_type_cache_markers
pub(crate) fn monitor_finish_body(
    payload: &[u8],
    order: ByteOrder,
) -> Result<(Status, Option<usize>), String> {
    let subcmd = *payload
        .get(4)
        .ok_or_else(|| format!("MONITOR FINISH frame too short: {} bytes", payload.len()))?;
    let mut cur = Cursor::new(payload);
    cur.set_position(5);
    let status = Status::decode(&mut cur, order)
        .map_err(|e| format!("MONITOR FINISH status decode failed: {e}"))?;
    let pos = cur.position() as usize;
    let body = (status.is_success() && subcmd & 0x08 == 0 && pos < payload.len()).then_some(pos);
    Ok((status, body))
}

/// Offset of the `changed BitSet | value | [overrun]` region of an op DATA
/// payload, or `None` when the frame carries no value body. GET/PUT/PUT_GET
/// put a `Status` before it (and a PUT without GetBack has no body at all),
/// MONITOR DATA starts it right after `ioid + subcmd`, and a MONITOR FINISH
/// defers to [`monitor_finish_body`].
fn op_data_body_start(payload: &[u8], order: ByteOrder, cmd: Command, subcmd: u8) -> Option<usize> {
    if cmd == Command::Monitor {
        if subcmd & 0x10 != 0 {
            return monitor_finish_body(payload, order).ok()?.1;
        }
        // MONITOR DATA carries no Status.
        return Some(5);
    }
    // A PUT data response without GetBack (subcmd & 0x40 == 0) is status-only.
    if cmd == Command::Put && subcmd & 0x40 == 0 {
        return None;
    }
    let mut cur = Cursor::new(payload);
    cur.set_position(5);
    match Status::decode(&mut cur, order) {
        // A non-success status is a status-only reply — no value follows.
        Ok(s) if s.is_success() => Some(cur.position() as usize),
        _ => None,
    }
}

/// Like [`decode_op_response`] but threads a
/// [`TypeCache`](crate::pvdata::encode::TypeCache) for 0xFD/0xFE marker support.
///
/// Production op paths use [`decode_op_response`] (empty cache) because the
/// reader has already flattened markers into self-contained frames; this
/// form exists for the internal value-decode plumbing and for tests that
/// pre-seed a cache to exercise the raw marker path.
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

    // MONITOR FINISH (subcmd & 0x10) — the server signals end-of-stream with a
    // Status after ioid/subcmd, and MAY append one last update after it
    // ([`monitor_finish_body`], pvxs `clientmon.cpp:504-511`). With a body, the
    // frame is decoded as ordinary MONITOR data (the cursor is parked at the
    // changed BitSet and the FINISH status carried down); without one it is the
    // status-only end of stream.
    //
    // This FINISH shape is MONITOR-specific: for GET/PUT/RPC the same bit is
    // the "last request" marker that pvxs echoes on an otherwise normal data
    // response (`serverget.cpp:83,112-116`) and the client decodes by
    // `cmd`/`init`/`get` bits (`clientget.cpp:405-452`).
    let mut finish_status = None;
    if cmd == Command::Monitor && subcmd & 0x10 != 0 {
        let (status, body) =
            monitor_finish_body(&frame.payload, order).map_err(PvaError::Decode)?;
        match body {
            None => {
                return Ok(OpResponse::Status(OpStatusResponse {
                    ioid,
                    subcmd,
                    status,
                }));
            }
            Some(start) => {
                cur.set_position(start as u64);
                finish_status = Some(status);
            }
        }
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
        // The reply type may be the NULL (`0xFF`) code: pvxs's no-argument
        // `ExecOp::reply()` (`srvcommon.h:108`) writes exactly that, with no
        // value body (`serverget.cpp:105-109`, `dataencode.cpp:29-33`), and
        // its own client accepts it — `from_wire_type(M, rxRegistry, data);
        // if(data) from_wire_full(...)` (`clientget.cpp:415-421`) leaves
        // `data` an empty `Value`. Decoding the value body only when the
        // descriptor is present mirrors that `if(data)` guard.
        let resp_desc =
            crate::pvdata::encode::decode_type_desc_cached_opt(&mut cur, order, type_cache)
                .map_err(|e| PvaError::Decode(e.to_string()))?;
        let resp_value = match &resp_desc {
            Some(desc) => {
                crate::pvdata::encode::decode_pv_field_cached(desc, &mut cur, order, type_cache)
                    .map_err(|e| PvaError::Decode(e.to_string()))?
            }
            None => PvField::Null,
        };
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
            // `None` is the empty reply — kept distinct from
            // `Some(FieldDesc::Variant)` + `PvField::Null`, which is a
            // *present* `any` field holding nothing.
            response_desc: resp_desc,
        }));
    }

    let intro = introspection.ok_or_else(|| {
        PvaError::Protocol("data response without prior introspection".to_string())
    })?;

    // GET data response and PUT_GET (PUT with subcmd & 0x40) begin with a
    // Status; MONITOR data does not — except a FINISH-carried update, whose
    // (success) Status was consumed above by `monitor_finish_body`.
    let status = if let Some(s) = finish_status {
        s
    } else if cmd == Command::Get || cmd == Command::Put {
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

/// Resolve 0xFD/0xFE type-cache markers in an inbound op-response frame —
/// both in the leading FieldDesc region (INIT introspection, PUT_GET
/// putIF/getIF, GET_FIELD type, RPC data type) AND inside `any`
/// (Variant / VariantArray) value payloads of DATA frames — rewriting the
/// payload to carry self-contained inline descriptors.
///
/// **Why this runs in the reader task.** Type-cache markers (`0xFD`
/// define / `0xFE` reference) only resolve correctly if every define is
/// observed before any reference to its slot. The connection reader task
/// is the single component that observes frames in strict wire order;
/// per-op tasks are scheduled by tokio in arbitrary order, so decoding a
/// `0xFE` reference in a per-op task could race ahead of the `0xFD`
/// define carried by an earlier (but not-yet-decoded) frame on a
/// different op. Flattening here, against the reader-task-owned `cache`,
/// makes the routed frames self-contained: per-op decoders decode with an
/// empty cache and the race is structurally impossible. pvxs resolves both
/// INIT descriptors and value-embedded `any` types through the one
/// connection `rxRegistry` (`dataencode.cpp:542`, `clientget.cpp:410-451`);
/// folding the value markers into the same reader-owned cache is the parity
/// equivalent.
///
/// `cache` holds the type-cache slots (pvxs `rxRegistry`); `introspection`
/// maps an in-flight IOID to the FieldDesc captured from its INIT response,
/// so a later DATA frame's `any`-bearing value can be walked. Both are
/// owned by the reader task and threaded across every frame in wire order.
/// The introspection entry is dropped on MONITOR FINISH here, and on any
/// op's teardown via
/// [`ServerConn::unregister_ioid`](crate::client_native::server_conn::ServerConn::unregister_ioid).
///
/// ChannelArray (`Command::Array`) is intentionally NOT handled here: its
/// DATA frame layout is sub-op dependent (getArray value vs getLength size
/// vs empty) and cannot be determined from the frame alone, so its INIT and
/// DATA legs resolve markers against an op-local cache in
/// [`op_array_data`](crate::client_native::ops_v2). That op runs both legs
/// on one task in wire order, so the op-local cache is correctly ordered;
/// the narrow consequence is that an Array value may not resolve a slot a
/// *different* op defined on the same connection.
///
/// On any parse difficulty the frame payload is left untouched — the per-op
/// decoder will then surface a precise error rather than this masking it.
///
/// Invariant: only the reader task calls this, exactly once per inbound
/// application frame, in wire order.
pub fn flatten_type_cache_markers(
    frame: &mut Frame,
    cache: &mut crate::pvdata::encode::TypeCache,
    introspection: &dashmap::DashMap<u32, FieldDesc>,
) {
    let order = frame.header.flags.byte_order();
    let Some(cmd) = Command::from_code(frame.header.command) else {
        return;
    };
    let Some(ioid) = peek_u32(&frame.payload, 0, order) else {
        return;
    };

    match cmd {
        Command::GetField => {
            // ioid(4) + status + [type-desc] — no subcmd byte. There is no
            // DATA value follow-up to value-flatten, so no introspection
            // capture is needed.
            rewrite_after_status(frame, order, cache, 4, 1);
        }
        Command::Get | Command::Put | Command::Monitor => {
            let Some(subcmd) = peek_u8(&frame.payload, 4) else {
                return;
            };
            // MONITOR FINISH (MONITOR-specific) ends the op. It may still
            // carry one last update ([`monitor_finish_body`]) whose `any`
            // payloads can hold 0xFD/0xFE markers, so flatten that body
            // BEFORE dropping the introspection the walk needs — otherwise
            // the trailing update reaches the per-op decoder with markers
            // and an empty cache. For GET/PUT the same 0x10 bit is the
            // last-request marker on an otherwise normal DATA response, so
            // it must NOT short-circuit.
            if cmd == Command::Monitor && subcmd & 0x10 != 0 {
                flatten_op_data_value(frame, order, cache, cmd, subcmd, ioid, introspection);
                introspection.remove(&ioid);
                return;
            }
            if subcmd & 0x08 != 0 {
                // INIT: flatten the single introspection descriptor and
                // capture it for the matching DATA frame(s).
                if let Some(descs) = rewrite_after_status(frame, order, cache, 5, 1) {
                    if let Some(d) = descs.into_iter().next() {
                        introspection.insert(ioid, d);
                    }
                }
                return;
            }
            // DATA: flatten markers inside the value using captured intro.
            flatten_op_data_value(frame, order, cache, cmd, subcmd, ioid, introspection);
        }
        Command::Rpc => {
            let Some(subcmd) = peek_u8(&frame.payload, 4) else {
                return;
            };
            if subcmd & 0x10 != 0 {
                introspection.remove(&ioid);
                return; // FINISH/DESTROY
            }
            if subcmd & 0x08 != 0 {
                return; // RPC INIT carries no type descriptor
            }
            // RPC DATA: ioid+subcmd+status+type+full_value. The type is in
            // the frame; flatten it, then walk the fully-present value.
            flatten_rpc_data_value(frame, order, cache);
        }
        Command::PutGet => {
            let Some(subcmd) = peek_u8(&frame.payload, 4) else {
                return;
            };
            if subcmd & 0x10 != 0 {
                introspection.remove(&ioid);
                return;
            }
            if subcmd & 0x08 != 0 {
                // INIT: putIF + getIF. Capture getIF — the DATA value type.
                if let Some(descs) = rewrite_after_status(frame, order, cache, 5, 2) {
                    if let Some(get_if) = descs.into_iter().nth(1) {
                        introspection.insert(ioid, get_if);
                    }
                }
                return;
            }
            // DATA: ioid+subcmd+status+getChanged+getValue → flatten value.
            flatten_op_data_value(frame, order, cache, cmd, subcmd, ioid, introspection);
        }
        _ => {}
    }
}

/// Rewrite a frame whose layout is `prefix_len` fixed bytes, then a
/// `Status`, then `desc_count` type descriptors, then an arbitrary tail.
///
/// The fixed prefix, the `Status` bytes, and the tail are copied verbatim;
/// each descriptor is decoded against `cache` (resolving 0xFD/0xFE markers)
/// and re-emitted inline. Returns the decoded descriptors in wire order so
/// the caller can capture an op's introspection. Returns `None` when the
/// frame is short, the status is non-success (descriptors absent), or a
/// descriptor fails to resolve — in which case the frame is left untouched.
fn rewrite_after_status(
    frame: &mut Frame,
    order: ByteOrder,
    cache: &mut crate::pvdata::encode::TypeCache,
    prefix_len: usize,
    desc_count: u8,
) -> Option<Vec<FieldDesc>> {
    if frame.payload.len() < prefix_len {
        return None;
    }
    let mut cur = Cursor::new(frame.payload.as_slice());
    cur.set_position(prefix_len as u64);
    let status = Status::decode(&mut cur, order).ok()?;
    // A non-success status has no trailing descriptor (mirrors the
    // decoder fast-paths in `decode_op_response_cached`).
    if !status.is_success() {
        return None;
    }
    let descs_start = cur.position() as usize;

    // Decode + re-emit each descriptor inline, capturing it so the caller
    // can record the op's introspection.
    let mut inline = Vec::new();
    let mut descs = Vec::with_capacity(desc_count as usize);
    for _ in 0..desc_count {
        let d = crate::pvdata::encode::decode_type_desc_cached(&mut cur, order, cache).ok()?;
        crate::pvdata::encode::encode_type_desc(&d, order, &mut inline);
        descs.push(d);
    }
    let descs_end = cur.position() as usize;

    // Fast path: if the descriptor region already had no markers the
    // flattened bytes are byte-identical; skip the realloc.
    if inline.as_slice() != &frame.payload[descs_start..descs_end] {
        let mut rebuilt =
            Vec::with_capacity(descs_start + inline.len() + (frame.payload.len() - descs_end));
        rebuilt.extend_from_slice(&frame.payload[..descs_start]);
        rebuilt.extend_from_slice(&inline);
        rebuilt.extend_from_slice(&frame.payload[descs_end..]);
        frame.header.payload_length = rebuilt.len() as u32;
        frame.payload = rebuilt;
    }
    Some(descs)
}

/// Flatten 0xFD/0xFE markers embedded in `any` payloads of a
/// GET/PUT/MONITOR/PUT_GET DATA value, in place. `cmd`/`subcmd` select the
/// frame layout (via [`op_data_body_start`], which also answers for a MONITOR
/// FINISH that carries a trailing update); the value type is the introspection
/// captured from the op's INIT. On any difficulty — including a frame with no
/// value body at all — the frame is left as-is for the per-op decoder.
fn flatten_op_data_value(
    frame: &mut Frame,
    order: ByteOrder,
    cache: &mut crate::pvdata::encode::TypeCache,
    cmd: Command,
    subcmd: u8,
    ioid: u32,
    introspection: &dashmap::DashMap<u32, FieldDesc>,
) {
    // Clone the captured introspection and drop the DashMap read guard
    // before the (potentially multi-MiB) value walk — never hold a shard
    // lock across the walk.
    let Some(intro) = introspection.get(&ioid).map(|r| r.value().clone()) else {
        return;
    };
    // Cheap gate: a value with no Variant/VariantArray anywhere cannot
    // carry an embedded marker — keep the zero-copy verbatim path.
    if !crate::pvdata::encode::desc_contains_variant(&intro) {
        return;
    }

    // Locate the `changed | value | [overrun]` region, then decode the changed
    // BitSet that precedes the value. Any early return leaves the frame as-is.
    let Some(body_start) = op_data_body_start(&frame.payload, order, cmd, subcmd) else {
        return;
    };
    let (value_start, changed) = {
        let mut cur = Cursor::new(frame.payload.as_slice());
        cur.set_position(body_start as u64);
        let changed = match BitSet::decode(&mut cur, order) {
            Ok(b) => b,
            Err(_) => return,
        };
        (cur.position() as usize, changed)
    };

    match crate::pvdata::encode::flatten_value_markers(
        &frame.payload,
        value_start,
        &intro,
        &changed,
        order,
        cache,
    ) {
        Ok((value_end, Some(rewritten))) => {
            // Tail (MONITOR overrun BitSet, else nothing) copied verbatim.
            let mut rebuilt = Vec::with_capacity(
                value_start + rewritten.len() + (frame.payload.len() - value_end),
            );
            rebuilt.extend_from_slice(&frame.payload[..value_start]);
            rebuilt.extend_from_slice(&rewritten);
            rebuilt.extend_from_slice(&frame.payload[value_end..]);
            frame.header.payload_length = rebuilt.len() as u32;
            frame.payload = rebuilt;
        }
        // No marker rewritten → value already self-contained, route verbatim.
        Ok((_, None)) => {}
        // Malformed → leave as-is; the per-op decoder surfaces the error.
        Err(_) => {}
    }
}

/// Flatten an RPC DATA frame: `ioid+subcmd+status+type+full_value`. The type
/// descriptor is in the frame; flatten it (capturing the decoded FieldDesc),
/// then walk the fully-present value against the same connection cache.
fn flatten_rpc_data_value(
    frame: &mut Frame,
    order: ByteOrder,
    cache: &mut crate::pvdata::encode::TypeCache,
) {
    // Flatten the in-frame type descriptor (after ioid+subcmd+status) first,
    // registering its 0xFD defines into `cache` so a value-tail 0xFE can
    // resolve them.
    let Some(resp_desc) =
        rewrite_after_status(frame, order, cache, 5, 1).and_then(|descs| descs.into_iter().next())
    else {
        return;
    };
    if !crate::pvdata::encode::desc_contains_variant(&resp_desc) {
        return;
    }
    // Re-locate the value start in the (possibly rebuilt) frame: skip the
    // status and the now-inline type descriptor.
    let value_start = {
        let mut cur = Cursor::new(frame.payload.as_slice());
        cur.set_position(5);
        if Status::decode(&mut cur, order).is_err() {
            return;
        }
        if crate::pvdata::encode::decode_type_desc(&mut cur, order).is_err() {
            return;
        }
        cur.position() as usize
    };

    match crate::pvdata::encode::flatten_value_markers_full(
        &frame.payload,
        value_start,
        &resp_desc,
        order,
        cache,
    ) {
        Ok((value_end, Some(rewritten))) => {
            let mut rebuilt = Vec::with_capacity(
                value_start + rewritten.len() + (frame.payload.len() - value_end),
            );
            rebuilt.extend_from_slice(&frame.payload[..value_start]);
            rebuilt.extend_from_slice(&rewritten);
            rebuilt.extend_from_slice(&frame.payload[value_end..]);
            frame.header.payload_length = rebuilt.len() as u32;
            frame.payload = rebuilt;
        }
        Ok((_, None)) => {}
        Err(_) => {}
    }
}

/// Read a single byte at `off` from a payload slice, `None` if short.
fn peek_u8(payload: &[u8], off: usize) -> Option<u8> {
    payload.get(off).copied()
}

/// Read a u32 at `off` from a payload slice in `order`, `None` if short.
fn peek_u32(payload: &[u8], off: usize, order: ByteOrder) -> Option<u32> {
    let b = payload.get(off..off.checked_add(4)?)?;
    let arr = [b[0], b[1], b[2], b[3]];
    Some(match order {
        ByteOrder::Big => u32::from_be_bytes(arr),
        ByteOrder::Little => u32::from_le_bytes(arr),
    })
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

    /// A pvxs server answering an RPC with the no-argument `ExecOp::reply()`
    /// (`srvcommon.h:108`) sends `ioid + subcmd + Status` followed by a bare
    /// NULL type code and NO value body (`serverget.cpp:105-109` +
    /// `dataencode.cpp:29-33`). Its own client accepts that — `from_wire_type`
    /// leaves an invalid `Value` and the `if(data)` guard skips the body
    /// (`clientget.cpp:415-421`). Decoding it must succeed with no value, not
    /// fail on the `0xFF` type code.
    #[test]
    fn rpc_reply_with_a_null_type_code_decodes_to_no_value() {
        use crate::proto::WriteExt;

        let order = ByteOrder::Little;
        let mut payload = Vec::new();
        payload.put_u32(11, order); // ioid
        payload.put_u8(0x00); // subcmd = EXEC (not INIT)
        Status::ok().write_into(order, &mut payload);
        payload.put_u8(crate::pvdata::encode::TAG_NULL); // null desc, no body

        let header = PvaHeader::application(true, order, Command::Rpc.code(), payload.len() as u32);
        let mut frame_bytes = Vec::new();
        header.write_into(&mut frame_bytes);
        frame_bytes.extend_from_slice(&payload);

        let (frame, _) = try_parse_frame(&frame_bytes).unwrap().unwrap();
        match decode_op_response(&frame, None).expect("a NULL-typed RPC reply must decode") {
            OpResponse::Data(d) => {
                assert_eq!(d.ioid, 11);
                assert!(
                    d.response_desc.is_none(),
                    "no descriptor accompanies the pvxs no-value reply"
                );
                assert_eq!(d.value, PvField::Null);
            }
            other => panic!("expected an RPC data response, got {other:?}"),
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
        let reader_intro = dashmap::DashMap::new();
        flatten_type_cache_markers(&mut frame_a, &mut reader_cache, &reader_intro);
        flatten_type_cache_markers(&mut frame_b, &mut reader_cache, &reader_intro);

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

    /// Unit test of the low-level [`decode_op_response_cached`] value-body
    /// marker plumbing: a data-phase `any`/`variant` value carrying a
    /// `0xFE <slot>` back-reference resolves against the `TypeCache` passed
    /// in, and an empty cache misses. This is the decode-side mirror of what
    /// the reader's `flatten_value_markers` does in wire order.
    ///
    /// In *production*
    /// op tasks no longer thread any shared cache: the reader has already
    /// flattened value markers into self-contained inline types before the
    /// frame is routed, so the per-op decode runs with an empty cache. See
    /// `reader_flatten_resolves_variant_value_marker_into_self_contained_frame`
    /// for the end-to-end regression that exercises that path.
    #[test]
    fn decode_op_cached_resolves_variant_value_marker() {
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

    /// End-to-end regression.
    ///
    /// The reader task flattens BOTH the INIT introspection descriptor AND
    /// the `any`/`variant` `0xFE <slot>` markers inside a later DATA value
    /// into one reader-owned cache, in wire order. After flattening, the
    /// routed DATA frame is self-contained, so the per-op decoder resolves
    /// the variant with an EMPTY cache and no shared connection state.
    ///
    /// Pre-fix the reader flattened only the descriptor region; the DATA
    /// value's `0xFE <slot>` was copied verbatim and the per-op decode (empty
    /// cache, after `ServerConn::type_cache` removal) reported a spurious
    /// slot miss → connection close. With the value flatten disabled this
    /// test fails on the empty-cache decode below.
    #[test]
    fn reader_flatten_resolves_variant_value_marker_into_self_contained_frame() {
        use crate::proto::WriteExt;
        use crate::pvdata::{PvField, ScalarType, ScalarValue};

        let order = ByteOrder::Little;
        let ioid = 9u32;
        // GET introspection: a structure with one `any` (Variant) leaf.
        let intro = FieldDesc::Structure {
            struct_id: "x".into(),
            fields: vec![("v".into(), FieldDesc::Variant)],
        };

        // Reader-owned state, threaded across frames in strict wire order.
        let mut reader_cache = crate::pvdata::encode::TypeCache::new();
        let reader_intro = dashmap::DashMap::new();
        // Slot 5 was defined earlier on this connection (a prior 0xFD
        // define); it lives in the reader cache — the single owner.
        reader_cache.insert(5, FieldDesc::Scalar(ScalarType::Int));

        // Frame 1: INIT (subcmd 0x08) carrying the introspection inline. The
        // reader captures it for this IOID so a later DATA value can be
        // walked.
        let mut init = Vec::new();
        init.put_u32(ioid, order);
        init.put_u8(0x08);
        Status::ok().write_into(order, &mut init);
        crate::pvdata::encode::encode_type_desc(&intro, order, &mut init);
        let mut init_frame = Frame {
            header: PvaHeader::application(true, order, Command::Get.code(), init.len() as u32),
            payload: init,
        };
        flatten_type_cache_markers(&mut init_frame, &mut reader_cache, &reader_intro);
        assert!(
            reader_intro.contains_key(&ioid),
            "reader must capture INIT introspection per-IOID"
        );

        // Frame 2: DATA (subcmd 0x00). value = Variant whose body is
        // `0xFE <slot=5> <Int 42 LE>` — a back-reference to slot 5.
        let mut data = Vec::new();
        data.put_u32(ioid, order);
        data.put_u8(0x00);
        Status::ok().write_into(order, &mut data);
        let mut changed = BitSet::new();
        changed.set(0); // whole structure present
        data.extend_from_slice(&changed.encode(order));
        data.extend_from_slice(&[0xFE, 0x05, 0x00, 0x2A, 0x00, 0x00, 0x00]);
        let mut data_frame = Frame {
            header: PvaHeader::application(true, order, Command::Get.code(), data.len() as u32),
            payload: data,
        };
        let pre = data_frame.payload.clone();
        flatten_type_cache_markers(&mut data_frame, &mut reader_cache, &reader_intro);
        assert_ne!(
            data_frame.payload, pre,
            "reader must rewrite the variant value marker inline"
        );
        assert_eq!(
            data_frame.header.payload_length as usize,
            data_frame.payload.len(),
            "rewritten frame length must match payload"
        );

        // The routed DATA frame is self-contained: the per-op decoder
        // resolves the variant with an EMPTY cache (no shared state).
        match decode_op_response(&data_frame, Some(&intro)).unwrap() {
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
    }

    /// Two concurrent ops, wire-order value-marker resolution.
    /// One op's DATA value
    /// *defines* a slot inline via `0xFD`; a later op's DATA value
    /// *references* it via `0xFE`. Because the reader flattens in strict
    /// wire order through one owned cache, the define is folded before the
    /// reference, and BOTH routed frames are self-contained — the per-op
    /// decoders (empty cache, arbitrary order) both succeed. This is the
    /// value-marker analogue of the descriptor-region cross-op race fixed by
    /// `type_cache_reference_decodes_before_define_after_flatten`.
    #[test]
    fn reader_flatten_value_markers_resolve_in_wire_order_across_ioids() {
        use crate::proto::WriteExt;
        use crate::pvdata::{PvField, ScalarType, ScalarValue};

        let order = ByteOrder::Little;
        let intro_a = FieldDesc::Structure {
            struct_id: "a".into(),
            fields: vec![("v".into(), FieldDesc::Variant)],
        };
        let intro_b = FieldDesc::Structure {
            struct_id: "b".into(),
            fields: vec![("u".into(), FieldDesc::Variant)],
        };

        let mut reader_cache = crate::pvdata::encode::TypeCache::new();
        let reader_intro = dashmap::DashMap::new();

        // Build a GET frame (server direction) with the given payload.
        let mk = |payload: Vec<u8>| Frame {
            header: PvaHeader::application(true, order, Command::Get.code(), payload.len() as u32),
            payload,
        };
        let init_frame = |ioid: u32, desc: &FieldDesc| -> Frame {
            let mut p = Vec::new();
            p.put_u32(ioid, order);
            p.put_u8(0x08);
            Status::ok().write_into(order, &mut p);
            crate::pvdata::encode::encode_type_desc(desc, order, &mut p);
            mk(p)
        };
        // DATA frame `ioid` whose single Variant leaf carries `variant_body`.
        let data_frame = |ioid: u32, variant_body: &[u8]| -> Frame {
            let mut p = Vec::new();
            p.put_u32(ioid, order);
            p.put_u8(0x00);
            Status::ok().write_into(order, &mut p);
            let mut changed = BitSet::new();
            changed.set(0);
            p.extend_from_slice(&changed.encode(order));
            p.extend_from_slice(variant_body);
            mk(p)
        };

        // Inline Int descriptor bytes for the `0xFD` define body.
        let mut int_desc = Vec::new();
        crate::pvdata::encode::encode_type_desc(
            &FieldDesc::Scalar(ScalarType::Int),
            order,
            &mut int_desc,
        );

        // ioid 10's value DEFINES slot 5 = Int inline, value 7.
        let mut def_body = vec![0xFD, 0x05, 0x00];
        def_body.extend_from_slice(&int_desc);
        def_body.extend_from_slice(&7i32.to_le_bytes());
        // ioid 9's value REFERENCES slot 5, value 42.
        let ref_body = [0xFE, 0x05, 0x00, 0x2A, 0x00, 0x00, 0x00];

        // WIRE ORDER: INIT 9, INIT 10, DATA 10 (define), DATA 9 (reference).
        let mut f_init9 = init_frame(9, &intro_a);
        let mut f_init10 = init_frame(10, &intro_b);
        let mut f_data10 = data_frame(10, &def_body);
        let mut f_data9 = data_frame(9, &ref_body);
        flatten_type_cache_markers(&mut f_init9, &mut reader_cache, &reader_intro);
        flatten_type_cache_markers(&mut f_init10, &mut reader_cache, &reader_intro);
        flatten_type_cache_markers(&mut f_data10, &mut reader_cache, &reader_intro);
        flatten_type_cache_markers(&mut f_data9, &mut reader_cache, &reader_intro);

        // Per-op decode in REVERSE order (9 before 10) with empty caches:
        // both routed frames are self-contained, so order does not matter.
        let leaf_int = |frame: &Frame, intro: &FieldDesc| -> i32 {
            match decode_op_response(frame, Some(intro)).unwrap() {
                OpResponse::Data(d) => match d.value {
                    PvField::Structure(s) => match &s.fields[0].1 {
                        PvField::Variant(vv) => {
                            assert_eq!(vv.desc, Some(FieldDesc::Scalar(ScalarType::Int)));
                            match vv.value {
                                PvField::Scalar(ScalarValue::Int(n)) => n,
                                ref other => panic!("expected Int leaf, got {other:?}"),
                            }
                        }
                        other => panic!("expected Variant leaf, got {other:?}"),
                    },
                    other => panic!("expected Structure value, got {other:?}"),
                },
                other => panic!("expected Data, got {other:?}"),
            }
        };
        assert_eq!(leaf_int(&f_data9, &intro_a), 42, "reference op (ioid 9)");
        assert_eq!(leaf_int(&f_data10, &intro_b), 7, "define op (ioid 10)");
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

    /// Input contract: a MONITOR DATA frame
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

    /// Input contract: a MONITOR FINISH frame
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

    /// R6-35: a MONITOR FINISH frame whose Status is followed by more bytes
    /// carries one last update. pvxs decodes it — `else if(!final ||
    /// !M.empty())` (`clientmon.cpp:504-511`) — queues it, and only then
    /// appends the `Finished()` marker (`:701-707`). The Rust decoder must
    /// surface the update as DATA (with the final bit still set on `subcmd`, so
    /// the monitor loop ends the stream after delivering it), not discard the
    /// body as a status-only end.
    #[test]
    fn monitor_finish_with_a_trailing_update_decodes_the_update() {
        use crate::proto::WriteExt;
        use crate::pvdata::{ScalarType, ScalarValue};

        let order = ByteOrder::Little;
        let intro = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
        };

        let mut payload = Vec::new();
        payload.put_u32(7, order); // ioid
        payload.put_u8(0x10); // subcmd = FINISH
        Status::ok().write_into(order, &mut payload);
        // …followed by an ordinary monitor update: changed | value | overrun.
        let mut changed = BitSet::new();
        changed.set(1); // field 1 = `value`
        payload.extend_from_slice(&changed.encode(order));
        crate::pvdata::encode::encode_pv_field_with_bitset(
            &PvField::Structure({
                let mut s = crate::pvdata::PvStructure::new("epics:nt/NTScalar:1.0");
                s.set("value", PvField::Scalar(ScalarValue::Double(2.5)));
                s
            }),
            &intro,
            &changed,
            0,
            order,
            &mut payload,
        );
        let mut overrun = BitSet::new();
        overrun.set(1);
        payload.extend_from_slice(&overrun.encode(order));

        let header =
            PvaHeader::application(true, order, Command::Monitor.code(), payload.len() as u32);
        let frame = Frame { header, payload };

        match decode_op_response(&frame, Some(&intro)).expect("FINISH+update must decode") {
            OpResponse::Data(d) => {
                assert_eq!(d.ioid, 7);
                assert_eq!(
                    d.subcmd & 0x10,
                    0x10,
                    "the final bit stays set so the loop can end the stream after delivery"
                );
                assert!(d.status.is_success());
                match &d.value {
                    PvField::Structure(s) => match s.get_field("value") {
                        Some(PvField::Scalar(ScalarValue::Double(v))) => assert_eq!(*v, 2.5),
                        other => panic!("unexpected FINISH update value: {other:?}"),
                    },
                    other => panic!("unexpected FINISH update: {other:?}"),
                }
                assert!(
                    !d.overrun.is_empty(),
                    "the FINISH update's trailing overrun bitset is decoded too"
                );
            }
            other => panic!("expected the trailing update as Data, got {other:?}"),
        }
    }

    /// Input contract: a MONITOR frame with the
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
    ///
    /// The one test in this module that reaches into the client — the type
    /// under test is the client's subscription counter — so it rides the
    /// `client` feature while the decoder itself does not.
    #[cfg(feature = "client")]
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
        let reader_intro = dashmap::DashMap::new();
        flatten_type_cache_markers(&mut frame, &mut reader_cache, &reader_intro);
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
        let reader_intro = dashmap::DashMap::new();
        flatten_type_cache_markers(&mut frame_a, &mut reader_cache, &reader_intro);
        flatten_type_cache_markers(&mut frame_b, &mut reader_cache, &reader_intro);

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
        let reader_intro = dashmap::DashMap::new();
        flatten_type_cache_markers(&mut frame, &mut reader_cache, &reader_intro);
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
        let reader_intro = dashmap::DashMap::new();
        flatten_type_cache_markers(&mut frame, &mut reader_cache, &reader_intro);
        assert_eq!(frame.payload, payload);
    }
}
