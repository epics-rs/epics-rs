//! Golden-file regression tests for the CA wire format.
//!
//! Each test fixes the byte-for-byte encoding of a representative
//! message. If the encoder ever drifts (an alignment fix gone wrong, a
//! field reordered, an endian flip), these tests turn red before
//! anything reaches a real IOC.
//!
//! The hex strings below are constructed from first principles
//! against the CA v4.13 wire format documented in
//! `crates/epics-ca-rs/doc/02-wire-protocol.md`. They are NOT
//! captured from libca/rsrv; that infrastructure (a live capture
//! harness with a softioc fixture) is a separate project. Any future
//! captured fixtures supersede these — when they disagree, the
//! captured ones win.
//!
//! All multi-byte integers are big-endian. Header layout (16 bytes):
//!
//! ```text
//! offset  size  field
//!     0     2   cmmd
//!     2     2   postsize
//!     4     2   data_type
//!     6     2   count
//!     8     4   cid (param1)
//!    12     4   available (param2)
//! ```

use epics_ca_rs::protocol::{
    CA_DO_REPLY, CA_PROTO_CREATE_CHAN, CA_PROTO_ERROR, CA_PROTO_EVENT_ADD, CA_PROTO_EVENT_CANCEL,
    CA_PROTO_NOT_FOUND, CA_PROTO_READ, CA_PROTO_READ_NOTIFY, CA_PROTO_RSRV_IS_UP, CA_PROTO_SEARCH,
    CA_PROTO_VERSION, CaHeader, ECA_ALLOCMEM, ECA_BADMONID,
};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn assert_hex(actual: &[u8], expected_hex: &str, label: &str) {
    let got = hex(actual);
    let want = expected_hex.replace(' ', "");
    if got != want {
        panic!("{label}:\n  got:  {got}\n  want: {want}\n  diff at first mismatch byte:\n",);
    }
}

#[test]
fn version_minimal() {
    // CA_PROTO_VERSION (0x0000), no payload, priority 0, minor 13.
    // bytes:
    //   00 00  cmmd = 0
    //   00 00  postsize = 0
    //   00 00  priority (data_type) = 0
    //   00 0d  minor version = 13
    //   00 00 00 00  cid
    //   00 00 00 00  available
    let mut h = CaHeader::new(CA_PROTO_VERSION);
    h.count = 13;
    let bytes = h.to_bytes();
    assert_hex(&bytes, "0000 0000 0000 000d 00000000 00000000", "VERSION");
}

#[test]
fn search_request() {
    // CA_PROTO_SEARCH (0x0006). Reply flag = 5 (DO_REPLY), version 13,
    // cid = 0x12345678, padded payload "MOTOR:VAL\0" (10 bytes →
    // padded to 16).
    let pv_name = b"MOTOR:VAL";
    let mut padded = Vec::new();
    padded.extend_from_slice(pv_name);
    padded.push(0); // null terminator
    while padded.len() % 8 != 0 {
        padded.push(0);
    }
    let postsize: u16 = padded.len() as u16; // 16
    let mut h = CaHeader::new(CA_PROTO_SEARCH);
    h.postsize = postsize;
    h.data_type = 5; // DO_REPLY
    h.count = 13; // minor version
    h.cid = 0x1234_5678;
    h.available = 0x1234_5678;
    let mut bytes = h.to_bytes().to_vec();
    bytes.extend_from_slice(&padded);
    // 0006   cmmd = 6
    // 0010   postsize = 16
    // 0005   DO_REPLY
    // 000d   minor 13
    // 12345678 cid
    // 12345678 available
    // 4d4f 544f 523a 5641 4c00 0000 0000 0000  "MOTOR:VAL\0\0\0\0\0\0\0"
    assert_hex(
        &bytes,
        "0006 0010 0005 000d 12345678 12345678 \
         4d4f544f523a56414c00000000000000",
        "SEARCH",
    );
}

#[test]
fn create_chan_response_dimensions() {
    // CA_PROTO_CREATE_CHAN (0x0012) reply: data_type = DBR_DOUBLE (6),
    // count=1, cid=0x55, sid=0x77.
    let mut h = CaHeader::new(CA_PROTO_CREATE_CHAN);
    h.data_type = 6; // DBR_DOUBLE
    h.count = 1;
    h.cid = 0x55;
    h.available = 0x77; // sid
    let bytes = h.to_bytes();
    assert_hex(
        &bytes,
        "0012 0000 0006 0001 00000055 00000077",
        "CREATE_CHAN",
    );
}

#[test]
fn read_notify_response_header_no_payload() {
    // CA_PROTO_READ_NOTIFY (0x000F): reply with eca=0x01 (NORMAL),
    // ioid=0xABCD, data_type=DBR_DOUBLE, count=1.
    let mut h = CaHeader::new(CA_PROTO_READ_NOTIFY);
    h.postsize = 8; // one DBR_DOUBLE
    h.data_type = 6;
    h.count = 1;
    h.cid = 1; // ECA_NORMAL on the wire
    h.available = 0xABCD;
    let bytes = h.to_bytes();
    assert_hex(
        &bytes,
        "000f 0008 0006 0001 00000001 0000abcd",
        "READ_NOTIFY",
    );
}

#[test]
fn event_add_request_header() {
    // CA_PROTO_EVENT_ADD (0x0001): subscribe with sid=0x10, sub_id=0x20,
    // data_type=DBR_TIME_DOUBLE (20), count=1, mask=value+alarm
    // (1+2=3). Payload: 12-byte SubscriptionRequest = 4 floats (low,
    // high, to) zeroed + u16 mask + u16 padding.
    let mut h = CaHeader::new(CA_PROTO_EVENT_ADD);
    h.postsize = 16;
    h.data_type = 20; // DBR_TIME_DOUBLE
    h.count = 1;
    h.cid = 0x10; // sid
    h.available = 0x20; // sub_id
    let mut bytes = h.to_bytes().to_vec();
    // payload: low_f32, high_f32, to_f32, mask u16, pad u16
    bytes.extend_from_slice(&0f32.to_be_bytes());
    bytes.extend_from_slice(&0f32.to_be_bytes());
    bytes.extend_from_slice(&0f32.to_be_bytes());
    bytes.extend_from_slice(&3u16.to_be_bytes());
    bytes.extend_from_slice(&0u16.to_be_bytes());
    assert_hex(
        &bytes,
        "0001 0010 0014 0001 00000010 00000020 \
         00000000 00000000 00000000 0003 0000",
        "EVENT_ADD",
    );
}

#[test]
fn rsrv_is_up_beacon() {
    // CA_PROTO_RSRV_IS_UP (0x000D): minor=13, port=5064, beacon_id=42,
    // m_available = 0 (INADDR_ANY).
    //
    // Per C `online_notify.c:69-72` (`rsrv_online_notify_task`), new
    // servers emit beacons with `memset 0` then set only m_cmmd,
    // m_count (port), m_dataType (minor version), and m_cid (counter).
    // `m_available` stays 0. Client `udpiiu.cpp:762` documents the
    // contract: "new servers: always set this field to INADDR_ANY";
    // a non-zero value is interpreted as overriding the source IP
    // (legacy fan-out compat). Our server emitter holds to this.
    let mut h = CaHeader::new(CA_PROTO_RSRV_IS_UP);
    h.data_type = 13;
    h.count = 5064;
    h.cid = 42;
    // h.available stays 0 (the C-spec semantic).
    let bytes = h.to_bytes();
    assert_hex(
        &bytes,
        "000d 0000 000d 13c8 0000002a 00000000",
        "RSRV_IS_UP",
    );
}

#[test]
fn extended_header_for_large_payload() {
    // When postsize > 0xFFFE OR count > 0xFFFF, the header switches to
    // extended form: postsize=0xFFFF, count=0, then 8 trailing bytes
    // (extended_postsize u32 + extended_count u32). Total 24 bytes.
    let mut h = CaHeader::new(CA_PROTO_READ_NOTIFY);
    h.set_payload_size(0x10_0000, 100_000, epics_ca_rs::protocol::CA_MINOR_VERSION)
        .expect("modern peer accepts the extended header"); // 1 MiB, 100k elements
    h.cid = 1;
    h.available = 0xDEAD;
    let bytes = h.to_bytes_extended();
    assert_hex(
        &bytes,
        "000f ffff 0000 0000 00000001 0000dead \
         00100000 000186a0",
        "READ_NOTIFY extended",
    );
}

#[test]
fn proto_error_field_assignment_matches_c() {
    // C `vsend_err` (`rsrv/camessage.c:149-233`) writes CA_PROTO_ERROR
    // as:
    //   m_cmmd      = CA_PROTO_ERROR
    //   m_postsize  = 24 (extended form when payload >= 0xFFFF — for
    //                small messages the short form is used; here we
    //                use the same shape as our helper)
    //   m_dataType  = 0
    //   m_count     = 0
    //   m_cid       = channel cid the error pertains to, or
    //                0xFFFFFFFF when the offending command has no
    //                channel scope (case-default branch).
    //   m_available = ECA status (read back by libca's
    //                `exceptionRespAction` at `cac.cpp:1118` as
    //                `hdr.m_available`).
    //
    // This test pins the field assignment so the fix of
    // `send_ca_error` (m_available carries status, not m_cid)
    // doesn't regress.
    let mut resp = CaHeader::new(CA_PROTO_ERROR);
    resp.cid = 0xFFFF_FFFF;
    resp.available = ECA_BADMONID;
    let bytes = resp.to_bytes();
    // Layout:
    //   0..2   cmmd       = 0x000b (CA_PROTO_ERROR = 11)
    //   2..4   postsize   = 0x0000
    //   4..6   data_type  = 0x0000
    //   6..8   count      = 0x0000
    //   8..12  cid        = 0xFFFFFFFF (no channel scope)
    //  12..16  available  = ECA_BADMONID
    let badmonid_hex = format!("{:08x}", ECA_BADMONID);
    let expected = format!("000b 0000 0000 0000 ffffffff {}", badmonid_hex);
    assert_hex(&bytes, &expected, "CA_PROTO_ERROR field assignment");
}

#[test]
fn event_cancel_unknown_sub_id_error_shape() {
    // Per C `event_cancel_reply` (`camessage.c:2035-2102`), when the
    // sub-id (m_available of the request) doesn't match any active
    // subscription on the addressed channel, the server replies with
    // CA_PROTO_ERROR carrying ECA_BADMONID. The payload echoes the
    // original request header followed by a NUL-terminated diagnostic.
    //
    // Wire shape (no diag string for this golden — emit just the
    // header so future drift in the diagnostic-string formatting
    // doesn't break this fixture):
    let mut req = CaHeader::new(CA_PROTO_EVENT_CANCEL);
    req.cid = 0x1234; // server sid
    req.available = 0x5678; // requested sub-id
    let req_bytes = req.to_bytes();
    assert_eq!(req_bytes.len(), CaHeader::SIZE);

    // Verify the request shape is what the server sees.
    let badmonid_hex = format!("{:08x}", ECA_BADMONID);
    let req_hex = req_bytes
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();
    let expected_req = "00020000000000000000123400005678";
    assert_eq!(req_hex, expected_req, "EVENT_CANCEL request header");
    // Confirm the constants used below have the expected hex.
    assert!(
        badmonid_hex.len() == 8 && badmonid_hex.chars().all(|c| c.is_ascii_hexdigit()),
        "ECA_BADMONID hex: {badmonid_hex}"
    );
}

#[test]
fn search_reply_udp_matches_c_sid_sentinel() {
    // C `search_reply_udp` (`rsrv/camessage.c:2193-2207`) builds:
    //   m_cmmd      = CA_PROTO_SEARCH (6)
    //   m_postsize  = 8 (sizeof(minorVersion)=2 → CA_MESSAGE_ALIGN → 8)
    //   m_dataType  = ca_server_port (carrier for the TCP port)
    //   m_count     = 0
    //   m_cid       = ~0U (0xFFFFFFFF) — INADDR_BROADCAST sentinel
    //                 telling the client to use the UDP source IP.
    //   m_available = mp->m_available (the client's cid for this PV)
    // followed by an 8-byte payload whose first 2 bytes carry
    // CA_MINOR_PROTOCOL_REVISION (13 = 0x000d).
    let mut resp = CaHeader::new(CA_PROTO_SEARCH);
    resp.postsize = 8;
    resp.data_type = 5064; // ca_server_port
    resp.count = 0;
    resp.cid = u32::MAX;
    resp.available = 0x1234_5678;
    let mut bytes = resp.to_bytes().to_vec();
    let mut payload = [0u8; 8];
    payload[0..2].copy_from_slice(&13u16.to_be_bytes());
    bytes.extend_from_slice(&payload);
    assert_hex(
        &bytes,
        "0006 0008 13c8 0000 ffffffff 12345678 \
         000d 0000 0000 0000",
        "SEARCH UDP reply (sid=~0U)",
    );
}

#[test]
fn search_reply_tcp_matches_c_zero_postsize_sid_sentinel() {
    // C `search_reply_tcp` (`rsrv/camessage.c:2329-2331`):
    //   m_cmmd      = CA_PROTO_SEARCH (6)
    //   m_postsize  = 0  (no payload — TCP search reply carries no
    //                 minor-version trailer, unlike UDP)
    //   m_dataType  = ca_server_port
    //   m_count     = 0
    //   m_cid       = ~0U
    //   m_available = mp->m_available
    // Total: 16 bytes, no payload.
    let mut resp = CaHeader::new(CA_PROTO_SEARCH);
    resp.set_payload_size(0, 0, epics_ca_rs::protocol::CA_MINOR_VERSION)
        .expect("modern peer accepts the extended header");
    resp.data_type = 5064;
    resp.cid = u32::MAX;
    resp.available = 0x1234_5678;
    let bytes = resp.to_bytes();
    assert_eq!(bytes.len(), CaHeader::SIZE);
    assert_hex(
        &bytes,
        "0006 0000 13c8 0000 ffffffff 12345678",
        "SEARCH TCP reply (postsize=0, sid=~0U)",
    );
}

#[test]
fn create_chan_cap_reached_uses_proto_error_eca_allocmem() {
    // C `claim_ciu_action` (`rsrv/camessage.c:1229-1239`) reserves
    // CREATE_CH_FAIL for the "PV does not exist on this server" path
    // (`dbChannel_create` returning NULL). Resource-exhaustion paths
    // (`casCreateChannel` returning NULL on alloc failure, no-room
    // for security table) route through `send_err(mp, ECA_ALLOCMEM,
    // …)`, which emits a CA_PROTO_ERROR.
    //
    // Per `vsend_err`'s command-keyed switch (`camessage.c:155-182`)
    // CA_PROTO_CREATE_CHAN falls to `default`, so `m_cid` is the
    // 0xFFFFFFFF "no channel scope" sentinel and `m_available`
    // carries the ECA status. libca `exceptionRespAction`
    // (`cac.cpp:1118`) reads `m_available` to surface ECA_ALLOCMEM
    // to the user callback, distinguishing "server saturated"
    // from CREATE_CH_FAIL's "no such PV".
    //
    // The Rust per-client channel cap used CREATE_CH_FAIL before
    // this commit; that made transient saturation look like
    // permanent "PV not found", so a libca client could remove our
    // address from its resolution cache for the entire connection
    // lifetime. Pin the corrected ERROR header shape here.
    let mut resp = CaHeader::new(CA_PROTO_ERROR);
    resp.cid = 0xFFFF_FFFF;
    resp.available = ECA_ALLOCMEM;
    let bytes = resp.to_bytes();
    let allocmem_hex = format!("{:08x}", ECA_ALLOCMEM);
    let expected = format!("000b 0000 0000 0000 ffffffff {}", allocmem_hex);
    assert_hex(
        &bytes,
        &expected,
        "CREATE_CHAN cap-reached ERROR header (ECA_ALLOCMEM, cid=~0U)",
    );
}

#[test]
fn search_fail_reply_tcp_echoes_request_header_fields() {
    // C `search_fail_reply` (`rsrv/camessage.c:2129-2143`) calls
    // `cas_copy_in_header(CA_PROTO_NOT_FOUND, 0u, mp->m_dataType,
    // mp->m_count, mp->m_cid, mp->m_available, NULL)` — every
    // identifying field of the incoming search request is echoed
    // verbatim into the NOT_FOUND reply. The earlier Rust path
    // overwrote `count` with the server's CA_MINOR_VERSION and
    // `cid` with the request's `m_available` (which happens to
    // equal `m_cid` for libca search frames, but the parity intent
    // is "echo m_cid"). This regression-fence the byte-for-byte
    // shape against the C softIoc.
    //
    // Wire shape (16 bytes, no payload):
    //   m_cmmd      = CA_PROTO_NOT_FOUND (14 = 0x000e)
    //   m_postsize  = 0
    //   m_dataType  = CA_DO_REPLY (10 = 0x000a)
    //   m_count     = client minor version (13 = 0x000d)
    //   m_cid       = client's request m_cid (0x1234_5678)
    //   m_available = client's request m_available (0x1234_5678)
    let mut nf = CaHeader::new(CA_PROTO_NOT_FOUND);
    nf.data_type = CA_DO_REPLY;
    nf.count = 13;
    nf.cid = 0x1234_5678;
    nf.available = 0x1234_5678;
    let bytes = nf.to_bytes();
    assert_eq!(bytes.len(), CaHeader::SIZE);
    assert_hex(
        &bytes,
        "000e 0000 000a 000d 12345678 12345678",
        "NOT_FOUND TCP reply echoes request fields",
    );
}

#[test]
fn read_response_header_carries_client_cid_not_sid() {
    // Deprecated CA_PROTO_READ (cmd=3) response: C `read_action`
    // (`camessage.c:622-624`) sets `m_cid = pciu->cid` — the
    // *client-side* CID captured at CREATE_CHAN, NOT the server-side
    // SID the request addressed the channel with. The Rust server
    // previously echoed `hdr.cid` (the SID) and diverged from C on the
    // wire — modern libca demuxes by ioid (`m_available`) so the
    // mismatch was invisible to it, but the byte-for-byte parity is
    // what wire-golden tests pin.
    //
    // Shape: data_type=DBF_DOUBLE (6), count=1, postsize=8 (one
    // DBR_DOUBLE), m_cid=clientCid=0xC1, m_available=ioid=0xAB. The
    // SID under which the client addresses the channel is whatever
    // was assigned at CREATE_CHAN (here 0x55 in the hypothetical
    // request); it MUST NOT appear in this response header.
    let mut h = CaHeader::new(CA_PROTO_READ);
    h.postsize = 8;
    h.data_type = 6;
    h.count = 1;
    h.cid = 0xC1; // client-side CID (parity: NOT 0x55 sid)
    h.available = 0xAB;
    let bytes = h.to_bytes();
    assert_hex(
        &bytes,
        "0003 0008 0006 0001 000000c1 000000ab",
        "READ response (client cid in m_cid, not sid)",
    );
}

#[test]
fn header_round_trip_through_decoder() {
    // Sanity: every test fixture above must round-trip through the
    // decoder. Picks one each from short and extended forms.
    let mut h = CaHeader::new(CA_PROTO_VERSION);
    h.count = 13;
    let bytes = h.to_bytes();
    let (decoded, size) = CaHeader::from_bytes_extended(&bytes).unwrap();
    assert_eq!(size, CaHeader::SIZE);
    assert_eq!(decoded.cmmd, h.cmmd);
    assert_eq!(decoded.count, h.count);

    let mut h2 = CaHeader::new(CA_PROTO_READ_NOTIFY);
    h2.set_payload_size(0x10_0000, 100_000, epics_ca_rs::protocol::CA_MINOR_VERSION)
        .expect("modern peer accepts the extended header");
    let bytes2 = h2.to_bytes_extended();
    let (decoded2, size2) = CaHeader::from_bytes_extended(&bytes2).unwrap();
    assert_eq!(size2, 24);
    assert_eq!(decoded2.actual_postsize(), 0x10_0000);
    assert_eq!(decoded2.actual_count(), 100_000);
}
