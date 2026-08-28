//! Application-level PVA message builders.
//!
//! This module is a thin layer over [`crate::proto`] that produces the byte
//! sequences expected by clients (`build_search`, `build_get_init`, ...) and
//! servers (`build_connection_validated`).
//!
//! It is byte-exact compatible with the pvAccess wire protocol for the
//! commands we emit; see the `proto::*` module tests for the cross-check.

use std::io::Cursor;
use std::net::{IpAddr, Ipv4Addr};

use crate::proto::{
    ByteOrder, Command, HeaderFlags, PvaHeader, QosFlags, Status, WriteExt, encode_size_into,
    encode_string_into, ip_from_bytes, ip_to_bytes,
};

// Public constants (kept for backward compatibility with downstream callers).
pub use crate::proto::PVA_VERSION;
pub const CMD_SEARCH: u8 = Command::Search as u8;
pub const CMD_SEARCH_RESPONSE: u8 = Command::SearchResponse as u8;
pub const CMD_CREATE_CHANNEL: u8 = Command::CreateChannel as u8;
pub const CMD_CONNECTION_VALIDATED: u8 = Command::ConnectionValidated as u8;
pub const CMD_GET: u8 = Command::Get as u8;
pub const CMD_PUT: u8 = Command::Put as u8;
pub const CMD_MONITOR: u8 = Command::Monitor as u8;
pub const CMD_DESTROY_REQUEST: u8 = Command::DestroyRequest as u8;
pub const CMD_GET_FIELD: u8 = Command::GetField as u8;
pub const QOS_INIT: u8 = QosFlags::INIT;

/// Fixed size of a `CMD_ORIGIN_TAG` prefix: 8-byte PVA header + 16-byte
/// IPv4-mapped IPv6 destination address. Matches pvxs
/// `udp_collector.cpp::cmd_origin_tag_size`.
pub const ORIGIN_TAG_PREFIX_SIZE: usize = PvaHeader::SIZE + 16;

/// PVA message codec — manages byte order and provides message building helpers.
///
/// All encoding is fully native.
pub struct PvaCodec {
    pub big_endian: bool,
}

impl PvaCodec {
    pub fn new() -> Self {
        Self { big_endian: false }
    }

    fn order(&self) -> ByteOrder {
        if self.big_endian {
            ByteOrder::Big
        } else {
            ByteOrder::Little
        }
    }

    fn frame(&self, server: bool, command: u8, payload: Vec<u8>) -> Vec<u8> {
        let header = PvaHeader::application(server, self.order(), command, payload.len() as u32);
        let mut out = Vec::with_capacity(PvaHeader::SIZE + payload.len());
        header.write_into(&mut out);
        out.extend_from_slice(&payload);
        out
    }

    fn op_payload(sid: u32, ioid: u32, subcmd: u8, extra: &[u8], order: ByteOrder) -> Vec<u8> {
        let mut p = Vec::with_capacity(9 + extra.len());
        p.put_u32(sid, order);
        p.put_u32(ioid, order);
        p.put_u8(subcmd);
        p.put_bytes(extra);
        p
    }

    // ─── Search message (UDP) ────────────────────────────────────────────

    pub fn build_search(
        &self,
        sequence_id: u32,
        search_id: u32,
        channel_name: &str,
        response_addr: [u8; 4],
        response_port: u16,
        unicast: bool,
    ) -> Vec<u8> {
        // One-name SEARCH is just the batched form with a single entry;
        // the wire bytes (count=1 + one (cid, name)) are identical.
        self.build_search_batch(
            sequence_id,
            &[(search_id, channel_name)],
            response_addr,
            response_port,
            unicast,
        )
    }

    /// Build a SEARCH message carrying MANY channel names in one
    /// datagram. pvxs `tickSearch` (`src/client.cpp:1063-1090`) writes the
    /// channel count ONCE and then packs `(cid, name)` entries into the
    /// same packet up to `maxSearchPayload`; this is the multi-name
    /// primitive that path requires. Callers are responsible for
    /// splitting `entries` so the resulting datagram stays under the MTU
    /// guard (see the client search engine's `pack_search_frames`); this
    /// builder emits exactly the entries handed to it.
    pub fn build_search_batch(
        &self,
        sequence_id: u32,
        entries: &[(u32, &str)],
        response_addr: [u8; 4],
        response_port: u16,
        unicast: bool,
    ) -> Vec<u8> {
        let order = self.order();
        let flags: u8 = if unicast { 0x80 } else { 0x00 };
        // pvxs `tickSearch` writes the search-response address as
        // `IN6ADDR_ANY_INIT` — 16 raw zero bytes (`::`) — directly, NOT
        // through `to_wire(SockAddr)` (src/client.cpp:1082-1085). So a wildcard
        // response address must be raw zeros, not the IPv4-mapped
        // `::ffff:0.0.0.0` that `ip_to_bytes` produces for an AF_INET
        // address. A concrete (relay-substituted) responder address keeps
        // the v4-mapped form, matching pvxs `to_wire(SockAddr)`
        // (evhelper.cpp:891-909). The server SEARCH_RESPONSE / beacon addr
        // fields are a separate path: pvxs writes `to_wire(SockAddr::
        // any(AF_INET))` there, which IS v4-mapped — see
        // `build_search_response_proto`/`build_beacon`.
        let addr = if response_addr == [0, 0, 0, 0] {
            [0u8; 16]
        } else {
            ip_to_bytes(IpAddr::V4(Ipv4Addr::from(response_addr)))
        };

        let mut p = Vec::new();
        p.put_u32(sequence_id, order);
        p.put_u8(flags);
        p.extend_from_slice(&[0u8; 3]); // reserved
        p.extend_from_slice(&addr);
        p.put_u16(response_port, order);
        // Single supported protocol: "tcp"
        encode_size_into(1, order, &mut p);
        encode_string_into("tcp", order, &mut p);
        // Channel list: count (u16) emitted ONCE, then the (cid, name)
        // entries back-to-back.
        p.put_u16(entries.len() as u16, order);
        for (cid, name) in entries {
            p.put_u32(*cid, order);
            encode_string_into(name, order, &mut p);
        }

        self.frame(false, CMD_SEARCH, p)
    }

    /// Byte offset of the SEARCH flags octet within a frame built by
    /// [`Self::build_search_batch`] / [`Self::build_discover_search`]: the
    /// fixed PVA header ([`PvaHeader::SIZE`]) followed by the 4-byte
    /// `searchSequenceID`. Pinned by the
    /// `search_frame_unicast_copy_matches_builder` test.
    const SEARCH_FLAGS_OFFSET: usize = PvaHeader::SIZE + 4;

    /// Return a copy of an already-built SEARCH frame with the `Unicast`
    /// flag bit (`0x80`) set in its flags octet. pvxs reuses one SEARCH
    /// buffer and toggles this bit per destination (`src/client.cpp:1180-1187`):
    /// set for a unicast target, clear for broadcast/multicast. The
    /// `build_*` helpers emit the broadcast shape (bit clear), so the search
    /// engine sends this unicast copy to a unicast destination — a
    /// flag-reading peer (pvAccessCPP/Java) keys its local re-broadcast off
    /// the bit. Other flag bits (e.g. `MustReply` on a discover frame) are
    /// preserved.
    pub fn search_frame_unicast_copy(frame: &[u8]) -> Vec<u8> {
        let mut f = frame.to_vec();
        if let Some(b) = f.get_mut(Self::SEARCH_FLAGS_OFFSET) {
            *b |= 0x80;
        }
        f
    }

    /// Build a discover-style empty SEARCH packet — flags carry the
    /// `MustReply` bit (0x01), the protocol list is empty, and the
    /// channel list is empty. Mirrors pvxs `tickSearch(SearchKind::
    /// discover)` (src/client.cpp:1054-1074): every reachable PVA server
    /// answers with a SEARCH_RESPONSE that the engine routes back into
    /// its `Discovered` event stream. The previous regular
    /// `build_search(..., "", ...)` packet had the wrong shape (count=1
    /// with empty name + protocol="tcp" + flags=0) and most pvxs
    /// servers ignored it as malformed search — `ping_all` /
    /// `discover()` were therefore silent on the wire.
    pub fn build_discover_search(&self, sequence_id: u32, response_port: u16) -> Vec<u8> {
        let order = self.order();

        let mut p = Vec::new();
        p.put_u32(sequence_id, order);
        p.put_u8(0x01); // flags = MustReply
        p.extend_from_slice(&[0u8; 3]); // reserved
        // pvxs `tickSearch(SearchKind::discover)` writes IN6ADDR_ANY_INIT
        // (16 raw zero bytes — `::`). Don't run it through `ip_to_bytes`
        // which would emit the IPv4-mapped form (`::ffff:0.0.0.0`); some
        // pvxs versions only accept the raw-zero shape on this code
        // path and the discover packet is wire-compatibility critical.
        p.extend_from_slice(&[0u8; 16]);
        p.put_u16(response_port, order);
        // Empty protocol list (size_t(0)).
        encode_size_into(0, order, &mut p);
        // Channel count = 0 (no PV names).
        p.put_u16(0, order);

        self.frame(false, CMD_SEARCH, p)
    }

    // ─── Connection validation response ──────────────────────────────────

    pub fn build_connection_validated(&self) -> Vec<u8> {
        let payload = Status::ok().encode(self.order());
        self.frame(false, CMD_CONNECTION_VALIDATED, payload)
    }

    // ─── Create channel ──────────────────────────────────────────────────

    pub fn build_create_channel(&self, client_channel_id: u32, channel_name: &str) -> Vec<u8> {
        let order = self.order();
        let mut p = Vec::new();
        p.put_u16(1, order); // channel count
        p.put_u32(client_channel_id, order);
        encode_string_into(channel_name, order, &mut p);
        self.frame(false, CMD_CREATE_CHANNEL, p)
    }

    // ─── GET ─────────────────────────────────────────────────────────────

    pub fn build_get_init(&self, server_channel_id: u32, ioid: u32, pv_request: &[u8]) -> Vec<u8> {
        let p = Self::op_payload(server_channel_id, ioid, QOS_INIT, pv_request, self.order());
        self.frame(false, CMD_GET, p)
    }

    pub fn build_get(&self, server_channel_id: u32, ioid: u32) -> Vec<u8> {
        let p = Self::op_payload(server_channel_id, ioid, 0x00, &[], self.order());
        self.frame(false, CMD_GET, p)
    }

    // ─── PUT ─────────────────────────────────────────────────────────────

    pub fn build_put_init(&self, server_channel_id: u32, ioid: u32, pv_request: &[u8]) -> Vec<u8> {
        let p = Self::op_payload(server_channel_id, ioid, QOS_INIT, pv_request, self.order());
        self.frame(false, CMD_PUT, p)
    }

    pub fn build_put(&self, server_channel_id: u32, ioid: u32, value_data: &[u8]) -> Vec<u8> {
        let p = Self::op_payload(server_channel_id, ioid, 0x00, value_data, self.order());
        self.frame(false, CMD_PUT, p)
    }

    /// PUT `GetOPut` phase — `subcmd=0x40` (`QosFlags::GET`), no value body.
    /// pvxs `clientget.cpp:299-300` (`GPROp::GetOPut`): a get-first builder
    /// (e.g. enum-by-label / read-modify-write) reads the current value
    /// through *this* PUT op's own pvRequest mask on the same `ioid`, rather
    /// than opening a separate `ChannelGet` with an empty all-fields request.
    /// The server replies with the current value (it derives `isput =
    /// !(subcmd & 0x40)`, `serverget.cpp:364`), then the client sends the
    /// `0x00` exec frame with the built value.
    pub fn build_put_get(&self, server_channel_id: u32, ioid: u32) -> Vec<u8> {
        let p = Self::op_payload(server_channel_id, ioid, QosFlags::GET, &[], self.order());
        self.frame(false, CMD_PUT, p)
    }

    // ─── MONITOR ─────────────────────────────────────────────────────────

    /// MONITOR INIT — `subcmd=0x08` (INIT) plus the pvRequest body.
    /// When `pipeline_initial_nack` is `Some(N)`, the pipeline bit
    /// `0x80` is OR'd into subcmd and a u32 `nack` trailer is
    /// appended after the pvRequest. Mirrors pvxs
    /// `clientmon.cpp:327-342`: the pipeline negotiation happens on
    /// INIT (where the server also reads `record._options.pipeline`
    /// from the pvRequest to decide whether to enable its credit
    /// window), NOT on START.
    pub fn build_monitor_init(
        &self,
        server_channel_id: u32,
        ioid: u32,
        pv_request: &[u8],
        pipeline_initial_nack: Option<u32>,
    ) -> Vec<u8> {
        let order = self.order();
        let mut body: Vec<u8> = Vec::with_capacity(pv_request.len() + 4);
        body.extend_from_slice(pv_request);
        let subcmd = if let Some(nack) = pipeline_initial_nack {
            let bytes = match order {
                ByteOrder::Big => nack.to_be_bytes(),
                ByteOrder::Little => nack.to_le_bytes(),
            };
            body.extend_from_slice(&bytes);
            QOS_INIT | 0x80
        } else {
            QOS_INIT
        };
        let p = Self::op_payload(server_channel_id, ioid, subcmd, &body, order);
        self.frame(false, CMD_MONITOR, p)
    }

    /// MONITOR START — `subcmd=0x44` (`0x40` START | `0x04` PROCESS)
    /// with no trailing payload. pvxs `clientmon.cpp:133-142` sends
    /// START/STOP as `sid + ioid + subcmd` only. Pre-fix Rust
    /// appended a 4-byte `pipeline_size` trailer here — pipeline
    /// negotiation belongs on INIT (see [`Self::build_monitor_init`]).
    pub fn build_monitor_start(&self, server_channel_id: u32, ioid: u32) -> Vec<u8> {
        let p = Self::op_payload(server_channel_id, ioid, 0x44, &[], self.order());
        self.frame(false, CMD_MONITOR, p)
    }

    /// Pause an active monitor — pvxs `Subscription::pause(true)`
    /// (clientmon.cpp:121,133). subcmd `0x04` (STOP) tells the server to
    /// stop emitting updates; the channel + ioid remain alive.
    pub fn build_monitor_pause(&self, server_channel_id: u32, ioid: u32) -> Vec<u8> {
        let p = Self::op_payload(server_channel_id, ioid, 0x04, &[], self.order());
        self.frame(false, CMD_MONITOR, p)
    }

    /// Resume a paused monitor — pvxs `Subscription::pause(false)`
    /// (clientmon.cpp:121,133). subcmd `0x44` (START | PROCESS) restarts
    /// updates without re-sending INIT or pipeline window.
    pub fn build_monitor_resume(&self, server_channel_id: u32, ioid: u32) -> Vec<u8> {
        let p = Self::op_payload(server_channel_id, ioid, 0x44, &[], self.order());
        self.frame(false, CMD_MONITOR, p)
    }

    /// Tear down a monitor via the MONITOR command's destroy bit
    /// (`subcmd=0x10`) rather than the separate `DESTROY_REQUEST` command.
    /// pvxs accepts destroy in any non-INIT MONITOR message
    /// (`servermon.cpp:640-642`, :691-708) and pvAccessCPP clients use this
    /// form; the body is `sid + ioid + subcmd` only, no trailer.
    pub fn build_monitor_destroy(&self, server_channel_id: u32, ioid: u32) -> Vec<u8> {
        let p = Self::op_payload(server_channel_id, ioid, 0x10, &[], self.order());
        self.frame(false, CMD_MONITOR, p)
    }

    /// Subsequent pipeline-ack message: subcmd `0x80` + ack count.
    pub fn build_monitor_ack(&self, server_channel_id: u32, ioid: u32, ack_count: u32) -> Vec<u8> {
        let order = self.order();
        let extra = match order {
            ByteOrder::Big => ack_count.to_be_bytes(),
            ByteOrder::Little => ack_count.to_le_bytes(),
        };
        let p = Self::op_payload(server_channel_id, ioid, 0x80, &extra, order);
        self.frame(false, CMD_MONITOR, p)
    }

    // ─── GET_FIELD (info) ────────────────────────────────────────────────

    pub fn build_get_field(&self, server_channel_id: u32, ioid: u32, subfield: &str) -> Vec<u8> {
        let order = self.order();
        let mut p = Vec::new();
        p.put_u32(server_channel_id, order);
        p.put_u32(ioid, order);
        encode_string_into(subfield, order, &mut p);
        self.frame(false, CMD_GET_FIELD, p)
    }

    // ─── DESTROY_REQUEST ─────────────────────────────────────────────────

    // ─── ORIGIN_TAG (loopback multicast forwarding prefix) ───────────────

    /// Encode a `CMD_ORIGIN_TAG` prefix carrying `orig_dest_ip` as the
    /// original destination address. Always emits 24 bytes: an 8-byte
    /// PVA header (cmd=22, payload_length=16) followed by the 16-byte
    /// IPv4-mapped IPv6 form of the destination.
    ///
    /// Mirrors pvxs `UDPCollector::forwardM` (udp_collector.cpp:544-568).
    /// pvxs writes the prefix big-endian via `FixedBuf M(true, ...)` —
    /// we match for wire-byte equivalence. The receiver reads the
    /// header's MSB flag and decodes either way.
    ///
    /// The prefix is independent of the inner forwarded packet's byte
    /// order, so this is a free function (no `&self`).
    pub fn build_origin_tag_prefix(orig_dest_ip: Ipv4Addr) -> [u8; ORIGIN_TAG_PREFIX_SIZE] {
        let mut out = [0u8; ORIGIN_TAG_PREFIX_SIZE];
        let header = PvaHeader::application(false, ByteOrder::Big, Command::OriginTag as u8, 16);
        out[..8].copy_from_slice(&header.encode());
        out[8..].copy_from_slice(&ip_to_bytes(IpAddr::V4(orig_dest_ip)));
        out
    }

    /// Try to peel a `CMD_ORIGIN_TAG` prefix off the head of `buf`.
    /// Returns `(orig_dest, inner)` on success.
    ///
    /// `orig_dest` is `None` when the prefix carries the unspecified
    /// address (`::` / `0.0.0.0`) — the forwarder had no per-NIC info.
    /// `Some(ip)` on a concrete IPv4 destination. IPv6-only origins
    /// also yield `None` since this stack is IPv4-only.
    ///
    /// Returns `None` on any parse failure: too short, wrong magic,
    /// wrong command, payload length < 16, or buffer shorter than the
    /// declared payload. Forward-compatible with longer payloads
    /// (`payload_length > 16`): trailing bytes are skipped, matching
    /// pvxs `udp_collector.cpp::case CMD_ORIGIN_TAG`.
    pub fn try_peel_origin_tag(buf: &[u8]) -> Option<(Option<Ipv4Addr>, &[u8])> {
        if buf.len() < PvaHeader::SIZE + 16 {
            return None;
        }
        let mut cur = Cursor::new(buf);
        let header = PvaHeader::decode(&mut cur).ok()?;
        if header.command != Command::OriginTag as u8 {
            return None;
        }
        if header.flags.0 & HeaderFlags::CONTROL != 0 {
            return None;
        }
        let payload_len = header.payload_length as usize;
        if payload_len < 16 {
            return None;
        }
        let total = PvaHeader::SIZE + payload_len;
        if buf.len() < total {
            return None;
        }
        let mut addr = [0u8; 16];
        addr.copy_from_slice(&buf[PvaHeader::SIZE..PvaHeader::SIZE + 16]);
        let orig_dest = match ip_from_bytes(&addr) {
            // pvxs `originaddr.isAny()` treats both the all-zeros IPv6
            // sentinel and IPv4 0.0.0.0 (v4-mapped `::ffff:0.0.0.0`) as
            // "valid forward, no per-NIC info". Match that here.
            Some(IpAddr::V4(v4)) if !v4.is_unspecified() => Some(v4),
            _ => None,
        };
        Some((orig_dest, &buf[total..]))
    }

    pub fn build_destroy_request(&self, server_channel_id: u32, ioid: u32) -> Vec<u8> {
        // DESTROY_REQUEST payload is `sid:u32 + ioid:u32` only — no subcmd
        // byte (pvxs `Connection::sendDestroyRequest`, fixed in 1f91eb9e).
        // Don't reuse `op_payload`, which appends the subcmd that GET / PUT /
        // MONITOR / RPC frames carry.
        let order = self.order();
        let mut p = Vec::with_capacity(8);
        p.put_u32(server_channel_id, order);
        p.put_u32(ioid, order);
        self.frame(false, CMD_DESTROY_REQUEST, p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codec(big_endian: bool) -> PvaCodec {
        PvaCodec { big_endian }
    }

    #[test]
    fn search_message_has_pva_header() {
        let bytes = codec(false).build_search(1, 7, "MY:PV", [0, 0, 0, 0], 5076, false);
        assert_eq!(bytes[0], 0xCA);
        assert_eq!(bytes[1], PVA_VERSION);
        assert_eq!(bytes[2] & 0x80, 0); // little-endian
        assert_eq!(bytes[3], CMD_SEARCH);
    }

    #[test]
    fn search_wildcard_response_addr_is_in6addr_any_raw_zeros() {
        // pvxs writes the search response-address field as IN6ADDR_ANY —
        // 16 raw zero bytes — directly (src/client.cpp:1082-1085), NOT the
        // IPv4-mapped `::ffff:0.0.0.0` that an AF_INET addr would produce
        // through `to_wire(SockAddr)`. The addr field sits at frame[16..32]
        // (header 8 + seq 4 + flags 1 + reserved 3).
        let bytes = codec(false).build_search_batch(
            0x6669_6e64,
            &[(7, "MY:PV")],
            [0, 0, 0, 0],
            5076,
            false,
        );
        assert_eq!(
            &bytes[16..32],
            &[0u8; 16],
            "wildcard response addr must be 16 raw zeros (IN6ADDR_ANY), not v4-mapped"
        );

        // A concrete responder address keeps the v4-mapped form, matching
        // pvxs `to_wire(SockAddr)` (evhelper.cpp:891-909).
        let bytes = codec(false).build_search_batch(
            0x6669_6e64,
            &[(7, "MY:PV")],
            [192, 168, 1, 5],
            5076,
            false,
        );
        assert_eq!(&bytes[16..26], &[0u8; 10]);
        assert_eq!(&bytes[26..28], &[0xff, 0xff]);
        assert_eq!(&bytes[28..32], &[192, 168, 1, 5]);
    }

    /// PVX-81: `search_frame_unicast_copy` must set exactly the same
    /// `Unicast` bit the builder sets when asked for `unicast=true`, with no
    /// other byte difference. Pins `SEARCH_FLAGS_OFFSET` to the real frame
    /// layout — if the layout shifts, the copy diverges from the builder and
    /// this fails.
    #[test]
    fn search_frame_unicast_copy_matches_builder() {
        for big_endian in [false, true] {
            let c = codec(big_endian);
            let bcast =
                c.build_search_batch(0x6669_6e64, &[(7, "MY:PV")], [0, 0, 0, 0], 5076, false);
            let ucast_builder =
                c.build_search_batch(0x6669_6e64, &[(7, "MY:PV")], [0, 0, 0, 0], 5076, true);
            let ucast_copy = PvaCodec::search_frame_unicast_copy(&bcast);
            assert_eq!(
                ucast_copy, ucast_builder,
                "flag-flipped copy must equal the unicast-built frame (big_endian={big_endian})"
            );
            // And the only delta vs the broadcast frame is the flags octet.
            assert_eq!(bcast.len(), ucast_copy.len());
            for (i, (a, b)) in bcast.iter().zip(&ucast_copy).enumerate() {
                if i == PvaHeader::SIZE + 4 {
                    assert_eq!(*a & 0x80, 0);
                    assert_eq!(*b & 0x80, 0x80);
                } else {
                    assert_eq!(a, b, "byte {i} must be unchanged");
                }
            }
        }
    }

    #[test]
    fn create_channel_carries_pv_name() {
        let bytes = codec(false).build_create_channel(42, "MY:PV");
        assert_eq!(bytes[3], CMD_CREATE_CHANNEL);
        // Payload: channel_count (u16 LE) + cid (u32 LE) + string "MY:PV"
        let payload = &bytes[8..];
        assert_eq!(&payload[..2], &[0x01, 0x00]);
        assert_eq!(&payload[2..6], &[42, 0, 0, 0]);
        assert_eq!(payload[6] as usize, "MY:PV".len());
        assert_eq!(&payload[7..7 + 5], b"MY:PV");
    }

    #[test]
    fn destroy_request_payload_layout() {
        let bytes = codec(false).build_destroy_request(99, 17);
        assert_eq!(bytes[3], CMD_DESTROY_REQUEST);
        let payload = &bytes[8..];
        // pvxs spec: payload is `sid:u32 + ioid:u32` only — no subcmd byte.
        assert_eq!(payload.len(), 8);
        assert_eq!(&payload[..4], &[99, 0, 0, 0]);
        assert_eq!(&payload[4..8], &[17, 0, 0, 0]);
    }

    /// `build_origin_tag_prefix` produces the exact 24-byte shape pvxs
    /// `UDPCollector::forwardM` writes: BE PVA header `cmd=22, len=16`
    /// followed by IPv4-mapped IPv6 of the dest IP.
    #[test]
    fn origin_tag_prefix_byte_layout() {
        let prefix = PvaCodec::build_origin_tag_prefix(Ipv4Addr::new(192, 168, 1, 100));
        assert_eq!(prefix.len(), 24);
        // Header: magic, version, flags=BE-only(0x80), cmd=22, len=16 (BE)
        assert_eq!(prefix[0], 0xCA);
        assert_eq!(prefix[1], PVA_VERSION);
        assert_eq!(prefix[2], 0x80, "prefix must be big-endian per pvxs");
        assert_eq!(prefix[3], 22, "cmd must be CMD_ORIGIN_TAG");
        assert_eq!(&prefix[4..8], &[0, 0, 0, 16], "payload_length=16 BE");
        // Address bytes: 10 zeros, 0xFFFF v4-mapped marker, then the v4 octets.
        assert_eq!(&prefix[8..18], &[0u8; 10]);
        assert_eq!(&prefix[18..20], &[0xFF, 0xFF]);
        assert_eq!(&prefix[20..24], &[192, 168, 1, 100]);
    }

    /// Round-trip: `build_origin_tag_prefix` then `try_peel_origin_tag`
    /// recovers the original destination IP and exposes the trailing
    /// inner payload unchanged.
    #[test]
    fn origin_tag_round_trip() {
        let dest = Ipv4Addr::new(10, 0, 0, 42);
        let mut wire = Vec::new();
        wire.extend_from_slice(&PvaCodec::build_origin_tag_prefix(dest));
        let inner = b"\xCA\x02\x00\x03\x00\x00\x00\x00inner-payload";
        wire.extend_from_slice(inner);

        let (peeled, rest) = PvaCodec::try_peel_origin_tag(&wire).expect("valid prefix");
        assert_eq!(peeled, Some(dest));
        assert_eq!(rest, inner);
    }

    /// Unspecified `0.0.0.0` orig dest peels as `None` — pvxs
    /// `originaddr.isAny()` branch (udp_collector.cpp:511-525). Caller
    /// uses this as "valid forward, no per-NIC info available".
    #[test]
    fn origin_tag_unspecified_decodes_as_none() {
        let prefix = PvaCodec::build_origin_tag_prefix(Ipv4Addr::UNSPECIFIED);
        let (peeled, rest) = PvaCodec::try_peel_origin_tag(&prefix).expect("valid prefix");
        assert_eq!(peeled, None, "UNSPECIFIED must yield None per pvxs isAny");
        assert!(rest.is_empty());
    }

    /// Reject malformed input: too short, wrong magic, wrong command,
    /// payload length below the 16-byte minimum, or truncated payload.
    #[test]
    fn origin_tag_rejects_malformed() {
        // Too short.
        assert!(PvaCodec::try_peel_origin_tag(&[]).is_none());
        assert!(PvaCodec::try_peel_origin_tag(&[0u8; 10]).is_none());
        assert!(PvaCodec::try_peel_origin_tag(&[0u8; 23]).is_none());

        // Bad magic.
        let mut bad = PvaCodec::build_origin_tag_prefix(Ipv4Addr::LOCALHOST).to_vec();
        bad[0] = 0xAB;
        assert!(PvaCodec::try_peel_origin_tag(&bad).is_none());

        // Wrong command (SEARCH instead of OriginTag).
        let mut wrong_cmd = PvaCodec::build_origin_tag_prefix(Ipv4Addr::LOCALHOST).to_vec();
        wrong_cmd[3] = Command::Search as u8;
        assert!(PvaCodec::try_peel_origin_tag(&wrong_cmd).is_none());

        // CONTROL flag set — pvxs reserves ORIGIN_TAG for application
        // frames only; a control-flagged frame must be rejected.
        let mut ctrl = PvaCodec::build_origin_tag_prefix(Ipv4Addr::LOCALHOST).to_vec();
        ctrl[2] |= HeaderFlags::CONTROL;
        assert!(PvaCodec::try_peel_origin_tag(&ctrl).is_none());

        // payload_length = 8 (< 16).
        let mut short_payload = PvaCodec::build_origin_tag_prefix(Ipv4Addr::LOCALHOST).to_vec();
        short_payload[4..8].copy_from_slice(&8u32.to_be_bytes());
        assert!(PvaCodec::try_peel_origin_tag(&short_payload).is_none());

        // payload_length = 32 but only 16 bytes follow → truncated.
        let mut truncated = PvaCodec::build_origin_tag_prefix(Ipv4Addr::LOCALHOST).to_vec();
        truncated[4..8].copy_from_slice(&32u32.to_be_bytes());
        assert!(PvaCodec::try_peel_origin_tag(&truncated).is_none());
    }

    /// IPv6-only origin → `None`. This stack is IPv4-only; an
    /// honest-IPv6 16-byte address (not v4-mapped) carries no useful
    /// per-NIC info for our routing, so peel returns `Some((None, _))`
    /// — same as the unspecified case from the caller's perspective.
    #[test]
    fn origin_tag_ipv6_only_origin_decodes_as_none() {
        // Build a prefix manually with a real IPv6 address (::1).
        let mut wire = Vec::new();
        let header = PvaHeader::application(false, ByteOrder::Big, 22, 16);
        header.write_into(&mut wire);
        let v6 = std::net::Ipv6Addr::LOCALHOST;
        wire.extend_from_slice(&v6.octets());

        let (peeled, rest) = PvaCodec::try_peel_origin_tag(&wire).expect("valid prefix");
        assert_eq!(peeled, None, "IPv6-only origin must yield None");
        assert!(rest.is_empty());
    }

    /// Forward-compatible with payloads larger than 16 bytes: pvxs
    /// `M.skip(head.len-16u, ...)` discards trailing extension data.
    /// Verify our peel skips them too and only returns bytes after the
    /// declared payload as the "inner" slice.
    #[test]
    fn origin_tag_skips_extra_payload_bytes() {
        // Build a prefix with payload_length=24 (16 v4 + 8 trailing).
        let dest = Ipv4Addr::new(10, 1, 2, 3);
        let mut wire = Vec::new();
        let header = PvaHeader::application(false, ByteOrder::Big, 22, 24);
        header.write_into(&mut wire);
        wire.extend_from_slice(&ip_to_bytes(IpAddr::V4(dest)));
        wire.extend_from_slice(&[0xAA; 8]); // 8 extension bytes
        wire.extend_from_slice(b"INNER");

        let (peeled, rest) = PvaCodec::try_peel_origin_tag(&wire).expect("forward-compat");
        assert_eq!(peeled, Some(dest));
        assert_eq!(rest, b"INNER");
    }

    /// Discover packet wire format (pvxs `tickSearch(SearchKind::
    /// discover)`): `MustReply` flag, empty protocol list, empty
    /// channel list. Ensures `pingAll` actually solicits replies
    /// instead of being silently dropped by servers as malformed.
    #[test]
    fn discover_search_payload_layout() {
        let bytes = codec(false).build_discover_search(0xCAFE, 5076);
        assert_eq!(bytes[3], CMD_SEARCH);
        let payload = &bytes[8..];
        // sequence (u32 LE)
        assert_eq!(&payload[0..4], &[0xFE, 0xCA, 0x00, 0x00]);
        // flags = MustReply (0x01)
        assert_eq!(payload[4], 0x01, "MustReply flag must be set");
        // reserved 3 bytes
        assert_eq!(&payload[5..8], &[0u8; 3]);
        // 16-byte response addr (UNSPECIFIED)
        assert_eq!(&payload[8..24], &[0u8; 16]);
        // response_port (u16 LE)
        assert_eq!(&payload[24..26], &5076u16.to_le_bytes());
        // protocol count = 0 (single byte size_t encoding)
        assert_eq!(payload[26], 0x00, "protocol list must be empty");
        // channel count (u16 LE) = 0
        assert_eq!(&payload[27..29], &0u16.to_le_bytes());
        // No more bytes
        assert_eq!(payload.len(), 29);
    }
}
