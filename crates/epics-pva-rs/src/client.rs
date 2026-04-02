use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

use crate::codec::PvaCodec;
use crate::error::{PvaError, PvaResult};
use crate::protocol::*;
use crate::pvdata::*;
use crate::serialize::*;

const SEARCH_TIMEOUT: Duration = Duration::from_secs(3);
const IO_TIMEOUT: Duration = Duration::from_secs(5);

static NEXT_ID: AtomicU32 = AtomicU32::new(1);
fn alloc_id() -> u32 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// PVA search result
struct SearchResult {
    server_addr: SocketAddr,
}

/// TCP connection state after handshake
struct PvaConnection {
    stream: TcpStream,
    codec: PvaCodec,
}

/// Channel after creation
struct PvaChannel {
    server_channel_id: u32,
    conn: PvaConnection,
}

/// pvAccess client
pub struct PvaClient {
    addr_list: Vec<SocketAddr>,
}

impl PvaClient {
    pub fn new() -> PvaResult<Self> {
        let mut addrs = Vec::new();

        let broadcast_port: u16 = epics_base_rs::runtime::net::pva_broadcast_port();

        if let Some(list) = epics_base_rs::runtime::env::get("EPICS_PVA_ADDR_LIST") {
            for entry in list.split_whitespace() {
                let addr = if entry.contains(':') {
                    entry
                        .parse::<SocketAddr>()
                        .map_err(|e| PvaError::Protocol(format!("bad address '{entry}': {e}")))?
                } else {
                    let ip: Ipv4Addr = entry
                        .parse()
                        .map_err(|e| PvaError::Protocol(format!("bad IP '{entry}': {e}")))?;
                    SocketAddr::V4(SocketAddrV4::new(ip, broadcast_port))
                };
                addrs.push(addr);
            }
        }

        let auto_addr = epics_base_rs::runtime::env::get_or("EPICS_PVA_AUTO_ADDR_LIST", "YES");

        if auto_addr.eq_ignore_ascii_case("YES") || addrs.is_empty() {
            addrs.push(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::BROADCAST,
                broadcast_port,
            )));
        }

        Ok(Self { addr_list: addrs })
    }

    // ─── UDP Search ──────────────────────────────────────────────────────

    async fn search_channel(&self, pv_name: &str) -> PvaResult<SearchResult> {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        socket.set_broadcast(true)?;

        let local_addr = socket.local_addr()?;
        let local_port = local_addr.port();

        let codec = PvaCodec::new();
        let sequence_id = alloc_id();
        let search_id = alloc_id();

        let packet = codec.build_search(
            sequence_id,
            search_id,
            pv_name,
            [0, 0, 0, 0], // any address
            local_port,
            false, // broadcast
        );

        for addr in &self.addr_list {
            let _ = socket.send_to(&packet, addr).await;
        }

        let mut buf = [0u8; 2048];
        let result = tokio::time::timeout(SEARCH_TIMEOUT, async {
            loop {
                let (len, _src) = socket.recv_from(&mut buf).await?;
                if len < PvaHeader::SIZE {
                    continue;
                }

                let hdr = PvaHeader::from_bytes(&buf[..len])?;
                if hdr.is_control() || hdr.command != CMD_SEARCH_RESPONSE {
                    continue;
                }

                let be = hdr.is_big_endian();
                let payload = &buf[PvaHeader::SIZE..len];
                let mut pos = 0;

                // server GUID (12 bytes)
                if pos + 12 > payload.len() {
                    continue;
                }
                pos += 12;

                // search sequence ID
                let _seq = read_u32(payload, &mut pos, be)?;

                // server address (16 bytes, IPv6-mapped)
                if pos + 16 > payload.len() {
                    continue;
                }
                let server_ip = Ipv4Addr::new(
                    payload[pos + 12],
                    payload[pos + 13],
                    payload[pos + 14],
                    payload[pos + 15],
                );
                pos += 16;

                // server port
                let server_port = read_u16(payload, &mut pos, be)?;

                // protocol string
                let _protocol = read_string(payload, &mut pos, be)?;

                // found flag
                let found = read_u8(payload, &mut pos)?;
                if found == 0 {
                    continue;
                }

                let server_ip = if server_ip.is_unspecified() {
                    // Use source address instead
                    match _src.ip() {
                        std::net::IpAddr::V4(ip) => ip,
                        _ => server_ip,
                    }
                } else {
                    server_ip
                };

                return Ok::<SearchResult, PvaError>(SearchResult {
                    server_addr: SocketAddr::V4(SocketAddrV4::new(server_ip, server_port)),
                });
            }
        })
        .await;

        match result {
            Ok(Ok(sr)) => Ok(sr),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(PvaError::ChannelNotFound(pv_name.to_string())),
        }
    }

    // ─── TCP Connection + Handshake ──────────────────────────────────────

    async fn connect(&self, server_addr: SocketAddr) -> PvaResult<PvaConnection> {
        let mut stream = tokio::time::timeout(IO_TIMEOUT, TcpStream::connect(server_addr))
            .await
            .map_err(|_| PvaError::Timeout)?
            .map_err(|_| PvaError::ConnectionRefused)?;

        let mut codec = PvaCodec::new();
        let mut buf = vec![0u8; 8192];
        let mut accumulated = Vec::new();

        // Read SET_BYTE_ORDER + CONNECTION_VALIDATION from server
        let validation_done = tokio::time::timeout(IO_TIMEOUT, async {
            loop {
                let n = stream.read(&mut buf).await?;
                if n == 0 {
                    return Err(PvaError::Protocol(
                        "connection closed during handshake".into(),
                    ));
                }
                accumulated.extend_from_slice(&buf[..n]);

                while accumulated.len() >= PvaHeader::SIZE {
                    let hdr = PvaHeader::from_bytes(&accumulated)?;
                    let msg_len = PvaHeader::SIZE + hdr.payload_size as usize;

                    if accumulated.len() < msg_len {
                        break; // incomplete
                    }

                    if hdr.is_control() && hdr.command == CMD_SET_BYTE_ORDER {
                        codec.big_endian = hdr.is_big_endian();
                    } else if !hdr.is_control() && hdr.command == CMD_CONNECTION_VALIDATION {
                        // Parse but we don't need the values for anonymous auth
                        // Just respond with CONNECTION_VALIDATED
                        accumulated.drain(..msg_len);
                        return Ok(());
                    }

                    accumulated.drain(..msg_len);
                }
            }
        })
        .await
        .map_err(|_| PvaError::Timeout)?;
        validation_done?;

        // Send CONNECTION_VALIDATED
        let response = codec.build_connection_validated();
        stream.write_all(&response).await?;
        stream.flush().await?;

        Ok(PvaConnection { stream, codec })
    }

    // ─── Create Channel ──────────────────────────────────────────────────

    async fn create_channel(&self, conn: &mut PvaConnection, pv_name: &str) -> PvaResult<u32> {
        let client_channel_id = alloc_id();
        let msg = conn.codec.build_create_channel(client_channel_id, pv_name);
        conn.stream.write_all(&msg).await?;
        conn.stream.flush().await?;

        // Read response
        let server_channel_id = tokio::time::timeout(IO_TIMEOUT, async {
            let mut buf = vec![0u8; 8192];
            let mut accumulated = Vec::new();

            loop {
                let n = conn.stream.read(&mut buf).await?;
                if n == 0 {
                    return Err(PvaError::Protocol("connection closed".into()));
                }
                accumulated.extend_from_slice(&buf[..n]);

                while accumulated.len() >= PvaHeader::SIZE {
                    let hdr = PvaHeader::from_bytes(&accumulated)?;
                    let msg_len = PvaHeader::SIZE + hdr.payload_size as usize;

                    if accumulated.len() < msg_len {
                        break;
                    }

                    let be = conn.codec.big_endian;

                    if !hdr.is_control() && hdr.command == CMD_CREATE_CHANNEL {
                        let payload = &accumulated[PvaHeader::SIZE..msg_len];
                        let mut pos = 0;
                        let _client_id = read_u32(payload, &mut pos, be)?;
                        let server_id = read_u32(payload, &mut pos, be)?;
                        read_status(payload, &mut pos, be)?;
                        return Ok(server_id);
                    }

                    accumulated.drain(..msg_len);
                }
            }
        })
        .await
        .map_err(|_| PvaError::Timeout)??;

        Ok(server_channel_id)
    }

    // ─── Helper: connect + create channel ────────────────────────────────

    async fn connect_and_create(&self, pv_name: &str) -> PvaResult<PvaChannel> {
        let search = self.search_channel(pv_name).await?;
        let mut conn = self.connect(search.server_addr).await?;
        let server_channel_id = self.create_channel(&mut conn, pv_name).await?;
        Ok(PvaChannel {
            server_channel_id,
            conn,
        })
    }

    // ─── Helper: read messages until we find one matching command+ioid ───

    async fn read_response(
        stream: &mut TcpStream,
        codec: &PvaCodec,
        expected_cmd: u8,
        expected_ioid: u32,
    ) -> PvaResult<Vec<u8>> {
        let mut buf = vec![0u8; 65536];
        let mut accumulated = Vec::new();

        loop {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                return Err(PvaError::Protocol("connection closed".into()));
            }
            accumulated.extend_from_slice(&buf[..n]);

            while accumulated.len() >= PvaHeader::SIZE {
                let hdr = PvaHeader::from_bytes(&accumulated)?;
                let msg_len = PvaHeader::SIZE + hdr.payload_size as usize;

                if accumulated.len() < msg_len {
                    break;
                }

                if !hdr.is_control() && hdr.command == expected_cmd {
                    let payload = accumulated[PvaHeader::SIZE..msg_len].to_vec();
                    let be = codec.big_endian;
                    // First 4 bytes of payload should be the ioid
                    if payload.len() >= 4 {
                        let mut pos = 0;
                        let ioid = read_u32(&payload, &mut pos, be)?;
                        if ioid == expected_ioid {
                            accumulated.drain(..msg_len);
                            return Ok(payload);
                        }
                    }
                }

                accumulated.drain(..msg_len);
            }
        }
    }

    // ─── pvaget ──────────────────────────────────────────────────────────

    pub async fn pvaget(&self, pv_name: &str) -> PvaResult<PvStructure> {
        let mut ch = self.connect_and_create(pv_name).await?;
        let be = ch.conn.codec.big_endian;
        let ioid = alloc_id();

        // GET INIT
        let pv_request = build_pv_request(be);
        let msg = ch
            .conn
            .codec
            .build_get_init(ch.server_channel_id, ioid, &pv_request);
        ch.conn.stream.write_all(&msg).await?;
        ch.conn.stream.flush().await?;

        // Read INIT response: ioid + subcommand + status + introspection
        let init_payload = tokio::time::timeout(
            IO_TIMEOUT,
            Self::read_response(&mut ch.conn.stream, &ch.conn.codec, CMD_GET, ioid),
        )
        .await
        .map_err(|_| PvaError::Timeout)??;

        let mut pos = 4; // skip ioid (already matched)
        let _subcmd = read_u8(&init_payload, &mut pos)?;
        read_status(&init_payload, &mut pos, be)?;
        let field_desc = read_field_desc(&init_payload, &mut pos, be)?;

        // GET (fetch data)
        let msg = ch.conn.codec.build_get(ch.server_channel_id, ioid);
        ch.conn.stream.write_all(&msg).await?;
        ch.conn.stream.flush().await?;

        // Read GET response: ioid + subcommand + status + bitset + data
        let get_payload = tokio::time::timeout(
            IO_TIMEOUT,
            Self::read_response(&mut ch.conn.stream, &ch.conn.codec, CMD_GET, ioid),
        )
        .await
        .map_err(|_| PvaError::Timeout)??;

        let mut pos = 4; // skip ioid
        let _subcmd = read_u8(&get_payload, &mut pos)?;
        read_status(&get_payload, &mut pos, be)?;

        let field = read_structure_value_with_bitset(&get_payload, &mut pos, &field_desc, be)?;

        match field {
            PvField::Structure(s) => Ok(s),
            _ => Err(PvaError::Protocol("expected structure response".into())),
        }
    }

    // ─── pvaput ──────────────────────────────────────────────────────────

    pub async fn pvaput(&self, pv_name: &str, value_str: &str) -> PvaResult<()> {
        let mut ch = self.connect_and_create(pv_name).await?;
        let be = ch.conn.codec.big_endian;
        let ioid = alloc_id();

        // PUT INIT
        let pv_request = build_pv_request_value_only(be);
        let msg = ch
            .conn
            .codec
            .build_put_init(ch.server_channel_id, ioid, &pv_request);
        ch.conn.stream.write_all(&msg).await?;
        ch.conn.stream.flush().await?;

        // Read INIT response
        let init_payload = tokio::time::timeout(
            IO_TIMEOUT,
            Self::read_response(&mut ch.conn.stream, &ch.conn.codec, CMD_PUT, ioid),
        )
        .await
        .map_err(|_| PvaError::Timeout)??;

        let mut pos = 4;
        let _subcmd = read_u8(&init_payload, &mut pos)?;
        read_status(&init_payload, &mut pos, be)?;
        let field_desc = read_field_desc(&init_payload, &mut pos, be)?;

        // Determine the value's scalar type from the structure description
        let scalar_type = field_desc
            .value_scalar_type()
            .ok_or_else(|| PvaError::Protocol("no scalar 'value' field in structure".into()))?;

        // Parse the value
        let scalar_val =
            ScalarValue::parse(scalar_type, value_str).map_err(|e| PvaError::InvalidValue(e))?;

        // Build put data: bitset (bit 0 set = whole structure) + value field data
        let mut value_data = Vec::new();
        // BitSet: 1 byte length, bit 0 set (whole structure)
        write_bitset(&mut value_data, &[0x01], be);
        // Write value field inside the structure
        write_scalar_value(&mut value_data, &scalar_val, be);

        let msg = ch
            .conn
            .codec
            .build_put(ch.server_channel_id, ioid, &value_data);
        ch.conn.stream.write_all(&msg).await?;
        ch.conn.stream.flush().await?;

        // Read PUT response
        let put_payload = tokio::time::timeout(
            IO_TIMEOUT,
            Self::read_response(&mut ch.conn.stream, &ch.conn.codec, CMD_PUT, ioid),
        )
        .await
        .map_err(|_| PvaError::Timeout)??;

        let mut pos = 4;
        let _subcmd = read_u8(&put_payload, &mut pos)?;
        read_status(&put_payload, &mut pos, be)?;

        Ok(())
    }

    // ─── pvamonitor ──────────────────────────────────────────────────────

    pub async fn pvamonitor<F>(&self, pv_name: &str, mut callback: F) -> PvaResult<()>
    where
        F: FnMut(&PvStructure),
    {
        let mut ch = self.connect_and_create(pv_name).await?;
        let be = ch.conn.codec.big_endian;
        let ioid = alloc_id();

        // MONITOR INIT
        let pv_request = build_pv_request(be);
        let msg = ch
            .conn
            .codec
            .build_monitor_init(ch.server_channel_id, ioid, &pv_request);
        ch.conn.stream.write_all(&msg).await?;
        ch.conn.stream.flush().await?;

        // Read INIT response
        let init_payload = tokio::time::timeout(
            IO_TIMEOUT,
            Self::read_response(&mut ch.conn.stream, &ch.conn.codec, CMD_MONITOR, ioid),
        )
        .await
        .map_err(|_| PvaError::Timeout)??;

        let mut pos = 4;
        let _subcmd = read_u8(&init_payload, &mut pos)?;
        read_status(&init_payload, &mut pos, be)?;
        let field_desc = read_field_desc(&init_payload, &mut pos, be)?;

        // Send pipeline start
        let msg = ch
            .conn
            .codec
            .build_monitor_start(ch.server_channel_id, ioid, 16);
        ch.conn.stream.write_all(&msg).await?;
        ch.conn.stream.flush().await?;

        // Event loop
        let mut buf = vec![0u8; 65536];
        let mut accumulated = Vec::new();
        let mut pipeline_credits = 16u32;

        loop {
            let n = ch.conn.stream.read(&mut buf).await?;
            if n == 0 {
                return Err(PvaError::Protocol("connection closed".into()));
            }
            accumulated.extend_from_slice(&buf[..n]);

            while accumulated.len() >= PvaHeader::SIZE {
                let hdr = PvaHeader::from_bytes(&accumulated)?;
                let msg_len = PvaHeader::SIZE + hdr.payload_size as usize;

                if accumulated.len() < msg_len {
                    break;
                }

                if !hdr.is_control() && hdr.command == CMD_MONITOR {
                    let payload = &accumulated[PvaHeader::SIZE..msg_len];
                    let mut pos = 0;
                    let resp_ioid = read_u32(payload, &mut pos, be)?;
                    let subcmd = read_u8(payload, &mut pos)?;

                    if resp_ioid == ioid && subcmd == 0x00 {
                        // Data event
                        if let Ok(PvField::Structure(s)) =
                            read_structure_value_with_bitset(payload, &mut pos, &field_desc, be)
                        {
                            callback(&s);
                        }

                        pipeline_credits -= 1;
                        if pipeline_credits <= 4 {
                            // Replenish
                            let msg =
                                ch.conn
                                    .codec
                                    .build_monitor_start(ch.server_channel_id, ioid, 16);
                            ch.conn.stream.write_all(&msg).await?;
                            ch.conn.stream.flush().await?;
                            pipeline_credits += 16;
                        }
                    }
                }

                accumulated.drain(..msg_len);
            }
        }
    }

    // ─── pvainfo ─────────────────────────────────────────────────────────

    pub async fn pvainfo(&self, pv_name: &str) -> PvaResult<FieldDesc> {
        let mut ch = self.connect_and_create(pv_name).await?;
        let be = ch.conn.codec.big_endian;
        let ioid = alloc_id();

        // GET_FIELD
        let msg = ch
            .conn
            .codec
            .build_get_field(ch.server_channel_id, ioid, "");
        ch.conn.stream.write_all(&msg).await?;
        ch.conn.stream.flush().await?;

        // Read response
        let payload = tokio::time::timeout(
            IO_TIMEOUT,
            Self::read_response(&mut ch.conn.stream, &ch.conn.codec, CMD_GET_FIELD, ioid),
        )
        .await
        .map_err(|_| PvaError::Timeout)??;

        let mut pos = 4; // skip ioid
        read_status(&payload, &mut pos, be)?;
        let desc = read_field_desc(&payload, &mut pos, be)?;

        Ok(desc)
    }
}
