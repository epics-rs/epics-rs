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
    decode_size, decode_string, ip_from_bytes,
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
    // A-G2: cap pre-allocation at remaining-bytes / 4 so a peer
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

    let ip = ip_from_bytes(&addr)
        .ok_or_else(|| PvaError::Protocol("search response unspecified address".to_string()))?;
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
    let count = decode_size(&mut cur, order)
        .map_err(|e| PvaError::Decode(e.to_string()))?
        .unwrap_or(0) as usize;
    // P-G22: cap allocation against attacker-controlled count. Each
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

/// Decode any GET/PUT/MONITOR response. The caller passes the introspection
/// from a prior INIT response so we can decode data payloads; for INIT
/// responses themselves, pass `None`.
///
/// `type_cache` is a per-connection [`TypeCache`] used to resolve
/// 0xFD (define) / 0xFE (lookup) markers in INIT responses. Pass an
/// empty cache initially; the same cache must be reused across all
/// frames on a single connection for cache references to resolve.
pub fn decode_op_response(
    frame: &Frame,
    introspection: Option<&FieldDesc>,
) -> PvaResult<OpResponse> {
    let mut empty = crate::pvdata::encode::TypeCache::new();
    decode_op_response_cached(frame, introspection, &mut empty)
}

/// Like [`decode_op_response`] but threads a per-connection
/// [`TypeCache`] for 0xFD/0xFE marker support.
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

    // FINISH/DESTROY (subcmd & 0x10) — server signals end-of-stream
    // (typically MONITOR after the source closes its broadcast channel).
    // pvxs servermon.cpp:148 sets `subcmd = 0x10` and emits only a Status
    // after ioid/subcmd. Surface this as `OpResponse::Status` so the
    // caller can tear down cleanly.
    if subcmd & 0x10 != 0 {
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
        let resp_value = crate::pvdata::encode::decode_pv_field(&resp_desc, &mut cur, order)
            .map_err(|e| PvaError::Decode(e.to_string()))?;
        let mut all = BitSet::new();
        all.set(0);
        return Ok(OpResponse::Data(OpDataResponse {
            ioid,
            subcmd,
            status,
            changed: all,
            value: resp_value,
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
    let changed = BitSet::decode(&mut cur, order).map_err(|e| PvaError::Decode(e.to_string()))?;
    let value =
        crate::pvdata::encode::decode_pv_field_with_bitset(intro, &changed, 0, &mut cur, order)
            .map_err(|e| PvaError::Decode(e.to_string()))?;
    // MONITOR data carries the overrun BitSet after the partial value.
    if cmd == Command::Monitor {
        let _overrun =
            BitSet::decode(&mut cur, order).map_err(|e| PvaError::Decode(e.to_string()))?;
    }
    Ok(OpResponse::Data(OpDataResponse {
        ioid,
        subcmd,
        status,
        changed,
        value,
        response_desc: None,
    }))
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
}
