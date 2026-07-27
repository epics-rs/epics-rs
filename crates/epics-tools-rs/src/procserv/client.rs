//! Per-client console connection.
//!
//! Mirrors C `clientItem` (`clientFactory.cc`). One instance per
//! TCP/UNIX socket. Each client has two tasks:
//! - **Read task**: `socket → TelnetParser::feed → InboundEvent::Data
//!   { ... }` (or `Reply` → outbound). Emits `Disconnected` on EOF.
//! - **Write task**: drains the supervisor's outbound mpsc, IAC-
//!   escapes `Bytes`, writes `RawIac` verbatim, exits on `Disconnect`.
//!
//! Read-only clients (`readonly: true`) silently drop their input
//! after IAC stripping, matching `clientItem::readFromFd:192`.

use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::procserv::telnet::{TelnetEvent, TelnetParser, iac_escape};

/// A freshly accepted socket, handed from the listener to the
/// supervisor.
#[derive(Debug)]
pub struct IncomingClient {
    pub stream: ClientStream,
    pub peer: ClientPeer,
    pub readonly: bool,
}

/// Either a TCP or UNIX socket, or — in foreground mode — the
/// launching terminal. Hides the difference from the supervisor: C
/// attaches fd 0 with the same `clientFactory` it uses for accepted
/// sockets (`procServ.cc:568`), so all three take the same client path.
pub enum ClientStream {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    Console(crate::procserv::console::ConsoleStream),
}

impl std::fmt::Debug for ClientStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tcp(s) => f.debug_tuple("Tcp").field(s).finish(),
            #[cfg(unix)]
            Self::Unix(s) => f.debug_tuple("Unix").field(s).finish(),
            Self::Console(_) => f.write_str("Console"),
        }
    }
}

/// Origin of the client. Used in the welcome banner + audit log.
#[derive(Debug, Clone)]
pub enum ClientPeer {
    Tcp(SocketAddr),
    #[cfg(unix)]
    Unix(Option<std::path::PathBuf>),
    /// The launching terminal, attached in foreground mode
    /// (C `clientFactory(0)`, `procServ.cc:568`).
    Console,
}

/// Direction of the per-client mpsc — supervisor-to-client. The
/// supervisor sends [`OutboundFrame`]s; the write task IAC-encodes
/// `Bytes` and writes `RawIac` verbatim.
#[derive(Debug, Clone)]
pub enum OutboundFrame {
    /// Plain payload (PTY output, peer echo, banner text).
    Bytes(Vec<u8>),
    /// Raw IAC reply emitted by [`super::telnet`] (negotiation
    /// responses); already correctly formatted, do NOT re-escape.
    RawIac(Vec<u8>),
    /// Disconnect this client gracefully. Write task drains queued
    /// frames first, then closes the socket.
    Disconnect,
}

/// Direction of the per-client mpsc — client-to-supervisor.
#[derive(Debug)]
pub enum InboundEvent {
    /// User typed bytes (after IAC strip). Supervisor scans for menu
    /// keys then forwards to the party-line.
    Data { bytes: Vec<u8> },
    /// Telnet reply that the parser produced as a side effect of
    /// negotiation handling. The supervisor routes these straight
    /// back to the same client's outbound (RawIac) — they never
    /// participate in fan-out.
    TelnetReply { bytes: Vec<u8> },
    /// Client disconnected (EOF or IO error).
    Disconnected,
}

/// Stable identifier for one client. Used by the supervisor to
/// route outbound frames + book-keep the readonly/user/logger
/// counts shown in the welcome banner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientId(u64);

impl ClientId {
    pub fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    /// Numeric form for log/audit fields.
    pub fn raw(self) -> u64 {
        self.0
    }
}

impl Default for ClientId {
    fn default() -> Self {
        Self::new()
    }
}

/// State the supervisor needs for this client beyond just routing:
/// readonly flag (gates inbound forwarding), peer identity (for
/// audit + welcome banner), client id.
#[derive(Debug, Clone)]
pub struct ClientMeta {
    pub id: ClientId,
    pub peer: ClientPeer,
    pub readonly: bool,
}

/// Spawn the per-client read+write task pair. Returns metadata + an
/// outbound mpsc the supervisor uses to push frames to this client.
///
/// The two tasks share the socket via `tokio::io::split`. The read
/// task takes the read-half, feeds bytes into a [`TelnetParser`],
/// and forwards `Data`/`Reply` events to the supervisor's
/// `inbound_tx`. The write task drains `outbound_rx` and writes to
/// the write-half, IAC-escaping payload bytes.
pub fn spawn_client(
    incoming: IncomingClient,
    inbound_tx: mpsc::Sender<(ClientId, InboundEvent)>,
) -> (ClientMeta, mpsc::Sender<OutboundFrame>) {
    let id = ClientId::new();
    let meta = ClientMeta {
        id,
        peer: incoming.peer,
        readonly: incoming.readonly,
    };
    let (out_tx, out_rx) = mpsc::channel::<OutboundFrame>(64);

    match incoming.stream {
        ClientStream::Tcp(s) => {
            // C enables SO_KEEPALIVE on every accepted client socket
            // (clientFactory.cc:146) so a silently-dropped peer (cable
            // pull) is eventually surfaced as a write error instead of
            // pending forever. UNIX sockets have no meaningful keepalive,
            // so this is TCP-only, as in C.
            set_keepalive(&s);
            spawn_split(s, id, incoming.readonly, inbound_tx, out_rx)
        }
        #[cfg(unix)]
        ClientStream::Unix(s) => spawn_split(s, id, incoming.readonly, inbound_tx, out_rx),
        // The terminal gets the same read/write task pair — including the
        // telnet parser, which C also runs on its fd-0 client
        // (`clientFactory.cc:167` `telnet_init`).
        ClientStream::Console(s) => spawn_split(s, id, incoming.readonly, inbound_tx, out_rx),
    }

    (meta, out_tx)
}

/// Enable `SO_KEEPALIVE` on a TCP client socket (C `clientFactory.cc:146`).
/// tokio's `TcpStream` exposes no keepalive setter, so go through the raw
/// fd. A failure is logged and tolerated — keepalive is a liveness
/// optimization, not a correctness requirement.
fn set_keepalive(stream: &TcpStream) {
    use std::os::fd::AsRawFd;
    let on: libc::c_int = 1;
    // SAFETY: `stream` owns a valid open socket fd for the duration of the
    // borrow; `setsockopt` with a `c_int` optval is the standard
    // SO_KEEPALIVE call and does not retain the pointer.
    let rc = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_KEEPALIVE,
            std::ptr::addr_of!(on).cast(),
            std::mem::size_of_val(&on) as libc::socklen_t,
        )
    };
    if rc != 0 {
        tracing::debug!(
            error = %std::io::Error::last_os_error(),
            "procserv-rs: SO_KEEPALIVE failed"
        );
    }
}

/// Generic helper that splits any AsyncRead+AsyncWrite stream and
/// spawns the read+write tasks. Monomorphized once per stream type.
fn spawn_split<S>(
    stream: S,
    id: ClientId,
    readonly: bool,
    inbound_tx: mpsc::Sender<(ClientId, InboundEvent)>,
    mut outbound_rx: mpsc::Receiver<OutboundFrame>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);

    // Read task: pump socket → telnet parser → inbound events.
    let inbound = inbound_tx.clone();
    tokio::spawn(async move {
        let mut parser = TelnetParser::new();
        let mut buf = vec![0u8; 1024];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break, // EOF
                Ok(n) => {
                    // A logger (readonly) client's input never reaches the
                    // telnet state machine: C only calls `telnet_recv` for
                    // `!_readonly` (clientFactory.cc:192). We still read (to
                    // detect EOF/disconnect) but discard the bytes without
                    // parsing, so a logger that sends IAC gets no telnet
                    // replies and no data is forwarded (PS-38).
                    if readonly {
                        continue;
                    }
                    for ev in parser.feed(&buf[..n]) {
                        match ev {
                            TelnetEvent::Data(d) => {
                                if inbound
                                    .send((id, InboundEvent::Data { bytes: d }))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            TelnetEvent::Reply(r) => {
                                if inbound
                                    .send((id, InboundEvent::TelnetReply { bytes: r }))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!(client = id.raw(), error = %e, "procserv-rs: client read error");
                    break;
                }
            }
        }
        let _ = inbound.send((id, InboundEvent::Disconnected)).await;
    });

    // Write task: drain outbound_rx → IAC-escape → socket. The telnet
    // negotiation is NOT a write-task prelude — C writes the greeting/info
    // banner first and only then calls `telnet_negotiate`
    // (clientFactory.cc:153-174), so the supervisor enqueues the banner
    // `Bytes` frame followed by the negotiation `RawIac` frame (PS-26).
    // This loop just drains them in order.
    tokio::spawn(async move {
        while let Some(frame) = outbound_rx.recv().await {
            match frame {
                OutboundFrame::Bytes(b) => {
                    let escaped = iac_escape(&b);
                    if writer.write_all(&escaped).await.is_err() {
                        break;
                    }
                }
                OutboundFrame::RawIac(b) => {
                    if writer.write_all(&b).await.is_err() {
                        break;
                    }
                }
                OutboundFrame::Disconnect => break,
            }
            if writer.flush().await.is_err() {
                break;
            }
        }
        let _ = writer.shutdown().await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio::time::timeout;

    /// Helper: a paired (server-side accepted, client-side connected)
    /// loopback TcpStream.
    async fn paired_streams() -> (TcpStream, TcpStream) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connect = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });
        let (server, _) = listener.accept().await.unwrap();
        let client = connect.await.unwrap();
        (server, client)
    }

    /// PS-25: `set_keepalive` must actually set `SO_KEEPALIVE` on the
    /// socket. Read it back with `getsockopt`.
    #[tokio::test]
    async fn set_keepalive_enables_so_keepalive() {
        use std::os::fd::AsRawFd;
        let (server, _client) = paired_streams().await;
        set_keepalive(&server);

        let mut val: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: valid fd, correctly-sized optval/optlen out-params.
        let rc = unsafe {
            libc::getsockopt(
                server.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_KEEPALIVE,
                std::ptr::addr_of_mut!(val).cast(),
                &mut len,
            )
        };
        assert_eq!(rc, 0, "getsockopt failed");
        assert_ne!(val, 0, "SO_KEEPALIVE not enabled");
    }

    #[tokio::test]
    async fn read_data_propagates_inbound_event() {
        let (server, mut client) = paired_streams().await;
        let (in_tx, mut in_rx) = mpsc::channel(8);
        let (_meta, out_tx) = spawn_client(
            IncomingClient {
                stream: ClientStream::Tcp(server),
                peer: ClientPeer::Tcp("127.0.0.1:1".parse().unwrap()),
                readonly: false,
            },
            in_tx,
        );

        // The write task no longer emits a negotiation prelude (PS-26:
        // the supervisor enqueues the banner then the IAC frame); a raw
        // `spawn_client` sends nothing until a frame is queued, so there
        // is nothing to skip here.
        client.write_all(b"hi\n").await.unwrap();
        let event = timeout(Duration::from_secs(1), in_rx.recv())
            .await
            .unwrap()
            .unwrap();
        match event {
            (_, InboundEvent::Data { bytes }) => assert_eq!(bytes, b"hi\n"),
            other => panic!("unexpected event: {other:?}"),
        }
        drop(out_tx);
    }

    #[tokio::test]
    async fn readonly_drops_input() {
        let (server, mut client) = paired_streams().await;
        let (in_tx, mut in_rx) = mpsc::channel(8);
        let (_meta, _out_tx) = spawn_client(
            IncomingClient {
                stream: ClientStream::Tcp(server),
                peer: ClientPeer::Tcp("127.0.0.1:1".parse().unwrap()),
                readonly: true,
            },
            in_tx,
        );

        client.write_all(b"ignored\n").await.unwrap();

        // No Data event should arrive; allow up to 200ms.
        let res = timeout(Duration::from_millis(200), in_rx.recv()).await;
        assert!(res.is_err(), "readonly client must not produce Data events");
    }

    /// PS-38: a readonly (logger) client's input never reaches the telnet
    /// state machine, so an IAC negotiation it sends gets no reply — C only
    /// runs `telnet_recv` for `!_readonly` (clientFactory.cc:192). The same
    /// bytes DO produce a reply on a control client, proving the readonly
    /// gate is what suppresses it (not inert input).
    #[tokio::test]
    async fn readonly_client_telnet_negotiation_gets_no_reply() {
        // IAC DO <unsupported opt 0x42> — a control client replies WONT.
        const IAC_DO_UNSUPPORTED: &[u8] = &[0xFF, 0xFD, 0x42];

        // Control client: the negotiation yields a TelnetReply.
        {
            let (server, mut client) = paired_streams().await;
            let (in_tx, mut in_rx) = mpsc::channel(8);
            let (_meta, _out_tx) = spawn_client(
                IncomingClient {
                    stream: ClientStream::Tcp(server),
                    peer: ClientPeer::Tcp("127.0.0.1:1".parse().unwrap()),
                    readonly: false,
                },
                in_tx,
            );
            client.write_all(IAC_DO_UNSUPPORTED).await.unwrap();
            let event = timeout(Duration::from_secs(1), in_rx.recv())
                .await
                .expect("control client should reply to IAC")
                .unwrap();
            assert!(
                matches!(event, (_, InboundEvent::TelnetReply { .. })),
                "control client must produce a telnet reply, got {event:?}"
            );
        }

        // Readonly client: the same bytes produce nothing.
        {
            let (server, mut client) = paired_streams().await;
            let (in_tx, mut in_rx) = mpsc::channel(8);
            let (_meta, _out_tx) = spawn_client(
                IncomingClient {
                    stream: ClientStream::Tcp(server),
                    peer: ClientPeer::Tcp("127.0.0.1:1".parse().unwrap()),
                    readonly: true,
                },
                in_tx,
            );
            client.write_all(IAC_DO_UNSUPPORTED).await.unwrap();
            let res = timeout(Duration::from_millis(200), in_rx.recv()).await;
            assert!(
                res.is_err(),
                "readonly client must not produce a telnet reply"
            );
        }
    }

    #[tokio::test]
    async fn write_iac_escapes_payload_bytes() {
        let (server, mut client) = paired_streams().await;
        let (in_tx, _in_rx) = mpsc::channel(8);
        let (_meta, out_tx) = spawn_client(
            IncomingClient {
                stream: ClientStream::Tcp(server),
                peer: ClientPeer::Tcp("127.0.0.1:1".parse().unwrap()),
                readonly: false,
            },
            in_tx,
        );

        // No negotiation prelude (PS-26): the first bytes on the wire are
        // this payload, IAC-escaped.
        // Send a payload containing a literal 0xFF — must be doubled
        // on the wire.
        out_tx
            .send(OutboundFrame::Bytes(vec![0x41, 0xFF, 0x42]))
            .await
            .unwrap();

        let mut got = [0u8; 4];
        client.read_exact(&mut got).await.unwrap();
        assert_eq!(got, [0x41, 0xFF, 0xFF, 0x42]);
    }
}
