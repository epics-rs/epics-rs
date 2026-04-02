use crate::protocol::*;
use crate::serialize::*;

/// PVA message codec — manages byte order and provides message building helpers
pub struct PvaCodec {
    pub big_endian: bool,
}

impl PvaCodec {
    pub fn new() -> Self {
        Self { big_endian: false } // default LE, server will specify
    }

    /// Build a complete PVA message: header + payload
    pub fn build_message(&self, command: u8, payload: &[u8]) -> Vec<u8> {
        let flags = if self.big_endian {
            FLAGS_APP_NONSEG_BE
        } else {
            FLAGS_APP_NONSEG_LE
        };
        let mut hdr = PvaHeader::new(command, flags);
        hdr.payload_size = payload.len() as u32;
        let mut msg = Vec::with_capacity(PvaHeader::SIZE + payload.len());
        msg.extend_from_slice(&hdr.to_bytes(self.big_endian));
        msg.extend_from_slice(payload);
        msg
    }

    // ─── Search message (UDP) ────────────────────────────────────────────

    /// Build a CMD_SEARCH UDP packet
    pub fn build_search(
        &self,
        sequence_id: u32,
        search_id: u32,
        channel_name: &str,
        response_addr: [u8; 4],
        response_port: u16,
        unicast: bool,
    ) -> Vec<u8> {
        let be = self.big_endian;
        let mut payload = Vec::new();

        // search sequence ID
        write_u32(&mut payload, sequence_id, be);

        // flags: 0x00=broadcast, 0x80=unicast (0x81 for reply required)
        let search_flags: u8 = if unicast { 0x80 } else { 0x00 };
        write_u8(&mut payload, search_flags);

        // reserved 3 bytes
        payload.extend_from_slice(&[0, 0, 0]);

        // response address: IPv6-mapped IPv4 (16 bytes)
        // ::ffff:a.b.c.d
        payload.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF]);
        payload.extend_from_slice(&response_addr);

        // response port
        write_u16(&mut payload, response_port, be);

        // protocols: 1 protocol = "tcp"
        write_u8(&mut payload, 1);
        write_string(&mut payload, "tcp", be);

        // channel count
        write_u16(&mut payload, 1, be);

        // channel: search_id + name
        write_u32(&mut payload, search_id, be);
        write_string(&mut payload, channel_name, be);

        self.build_message(CMD_SEARCH, &payload)
    }

    // ─── Connection validation response ──────────────────────────────────

    /// Build CMD_CONNECTION_VALIDATED response
    pub fn build_connection_validated(&self) -> Vec<u8> {
        let be = self.big_endian;
        let mut payload = Vec::new();
        // status: OK (0xFF)
        write_status_ok(&mut payload);
        // auth plugin: anonymous (empty string)
        write_string(&mut payload, "", be);
        self.build_message(CMD_CONNECTION_VALIDATED, &payload)
    }

    // ─── Create channel ──────────────────────────────────────────────────

    pub fn build_create_channel(&self, client_channel_id: u32, channel_name: &str) -> Vec<u8> {
        let be = self.big_endian;
        let mut payload = Vec::new();
        // count = 1
        write_u16(&mut payload, 1, be);
        // client channel ID
        write_u32(&mut payload, client_channel_id, be);
        // channel name
        write_string(&mut payload, channel_name, be);
        self.build_message(CMD_CREATE_CHANNEL, &payload)
    }

    // ─── GET ─────────────────────────────────────────────────────────────

    /// Build GET INIT request
    pub fn build_get_init(&self, server_channel_id: u32, ioid: u32, pv_request: &[u8]) -> Vec<u8> {
        let be = self.big_endian;
        let mut payload = Vec::new();
        write_u32(&mut payload, server_channel_id, be);
        write_u32(&mut payload, ioid, be);
        write_u8(&mut payload, QOS_INIT);
        payload.extend_from_slice(pv_request);
        self.build_message(CMD_GET, &payload)
    }

    /// Build GET (fetch data) request
    pub fn build_get(&self, server_channel_id: u32, ioid: u32) -> Vec<u8> {
        let be = self.big_endian;
        let mut payload = Vec::new();
        write_u32(&mut payload, server_channel_id, be);
        write_u32(&mut payload, ioid, be);
        write_u8(&mut payload, 0x00); // default subcommand
        self.build_message(CMD_GET, &payload)
    }

    // ─── PUT ─────────────────────────────────────────────────────────────

    /// Build PUT INIT request
    pub fn build_put_init(&self, server_channel_id: u32, ioid: u32, pv_request: &[u8]) -> Vec<u8> {
        let be = self.big_endian;
        let mut payload = Vec::new();
        write_u32(&mut payload, server_channel_id, be);
        write_u32(&mut payload, ioid, be);
        write_u8(&mut payload, QOS_INIT);
        payload.extend_from_slice(pv_request);
        self.build_message(CMD_PUT, &payload)
    }

    /// Build PUT (write data) request with bitset + value
    pub fn build_put(&self, server_channel_id: u32, ioid: u32, value_data: &[u8]) -> Vec<u8> {
        let be = self.big_endian;
        let mut payload = Vec::new();
        write_u32(&mut payload, server_channel_id, be);
        write_u32(&mut payload, ioid, be);
        write_u8(&mut payload, 0x00); // default subcommand
        payload.extend_from_slice(value_data);
        self.build_message(CMD_PUT, &payload)
    }

    // ─── MONITOR ─────────────────────────────────────────────────────────

    /// Build MONITOR INIT request
    pub fn build_monitor_init(
        &self,
        server_channel_id: u32,
        ioid: u32,
        pv_request: &[u8],
    ) -> Vec<u8> {
        let be = self.big_endian;
        let mut payload = Vec::new();
        write_u32(&mut payload, server_channel_id, be);
        write_u32(&mut payload, ioid, be);
        write_u8(&mut payload, QOS_INIT);
        payload.extend_from_slice(pv_request);
        self.build_message(CMD_MONITOR, &payload)
    }

    /// Build MONITOR pipeline start
    pub fn build_monitor_start(
        &self,
        server_channel_id: u32,
        ioid: u32,
        pipeline_size: u32,
    ) -> Vec<u8> {
        let be = self.big_endian;
        let mut payload = Vec::new();
        write_u32(&mut payload, server_channel_id, be);
        write_u32(&mut payload, ioid, be);
        write_u8(&mut payload, 0x80); // pipeline start subcommand
        write_u32(&mut payload, pipeline_size, be);
        self.build_message(CMD_MONITOR, &payload)
    }

    // ─── GET_FIELD (info) ────────────────────────────────────────────────

    pub fn build_get_field(&self, server_channel_id: u32, ioid: u32, subfield: &str) -> Vec<u8> {
        let be = self.big_endian;
        let mut payload = Vec::new();
        write_u32(&mut payload, server_channel_id, be);
        write_u32(&mut payload, ioid, be);
        write_string(&mut payload, subfield, be);
        self.build_message(CMD_GET_FIELD, &payload)
    }

    // ─── DESTROY_REQUEST ─────────────────────────────────────────────────

    pub fn build_destroy_request(&self, server_channel_id: u32, ioid: u32) -> Vec<u8> {
        let be = self.big_endian;
        let mut payload = Vec::new();
        write_u32(&mut payload, server_channel_id, be);
        write_u32(&mut payload, ioid, be);
        self.build_message(CMD_DESTROY_REQUEST, &payload)
    }
}
