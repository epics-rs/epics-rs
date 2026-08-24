//! Per-client console connection.
//!
//! Mirrors C `clientItem` (`clientFactory.cc`). One instance per
//! TCP/UNIX socket. Each client has two tasks:
//! - **Read task**: `socket → TelnetParser::feed → InboundEvent::Data
//!   { ... }` (or `Reply` → outbound). Emits `Disconnected` on EOF.
//! - **Write task**: drains the supervisor's outbound mpsc, IAC-
//!   escapes `Bytes`, writes `RawIac` verbatim.
//!
//! Both tasks belong to a [`ClientHandle`], which is the whole of C's
//! `clientItem` lifetime: dropping it is `~clientItem`
//! (`clientFactory.cc:73-80`).
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

/// C's `SO_SNDTIMEO` on an accepted client socket: 10 s
/// (`clientFactory.cc:104-105` builds the `timeval`, `:147` arms it).
/// C's fds are blocking, so a `write()` to a peer that has stopped
/// reading returns -1 once this elapses and `writeToFd` marks the client
/// `_markedForDeletion` (`clientFactory.cc:283-290`).
///
/// The deadline belongs on the socket write, which is where C arms it —
/// not on the queue in front of it. A queue-level deadline lets the write
/// itself pend forever, which is the whole of PS-25: the supervisor drops
/// the roster entry while the write task is still parked in `write_all`,
/// so the fd and both tasks outlive the client.
const CLIENT_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The supervisor's sole handle on one client: the outbound queue plus
/// both of the client's tasks.
///
/// C parity: `clientItem` and its destructor (`clientFactory.cc:73-80`,
/// `shutdown(_fd, SHUT_RDWR); close(_fd)`). C reaches that destructor from
/// every path that removes a connection — `DeleteConnection`
/// (`procServ.cc:837-852`) and the shutdown sweep (`procServ.cc:688-692`)
/// — and so does this: the handle is the only way to reach the client, so
/// a client that leaves the supervisor's roster is torn down by
/// construction rather than by each removal site remembering to.
///
/// Dropping it closes the outbound channel and aborts the read task. The
/// write task then writes what is already queued and shuts the socket, at
/// which point both halves of the split stream are gone and the fd is
/// closed. It cannot outlive that by more than the queue depth times
/// `CLIENT_SEND_TIMEOUT`, because every write it makes carries that
/// deadline — which is the whole reason the deadline sits on the write.
///
/// Draining rather than aborting is what C does, though C gets it for
/// free: `SendToAll` writes synchronously, so everything the supervisor
/// wrote is in the kernel before the item is ever deleted. The port queues
/// those writes instead, and cutting the queue at removal loses the last
/// thing the operator is told — the `@@@ ... server will exit` line on the
/// shutdown path, which is emitted and then immediately followed by the
/// roster going away.
pub struct ClientHandle {
    out_tx: mpsc::Sender<OutboundFrame>,
    read_task: tokio::task::JoinHandle<()>,
}

impl ClientHandle {
    /// Queue one frame for this client. `false` ⟹ the write task is gone
    /// (socket dead, or a write missed `CLIENT_SEND_TIMEOUT`) and the
    /// supervisor should drop the client — C's `_status = -1` out of
    /// `writeToFd`.
    pub async fn send(&self, frame: OutboundFrame) -> bool {
        self.out_tx.send(frame).await.is_ok()
    }
}

impl Drop for ClientHandle {
    fn drop(&mut self) {
        // `out_tx` drops with the rest of the struct, which is what lets the
        // write task finish. The read task has no such signal — it is parked
        // in `read()` on a peer that may never send another byte or close —
        // so it has to be aborted, and it holds the read half of the split
        // stream, without which the fd cannot close.
        self.read_task.abort();
    }
}

/// Spawn the per-client read+write task pair. Returns metadata + the
/// [`ClientHandle`] that owns them.
///
/// The two tasks share the socket via `tokio::io::split`. The read
/// task takes the read-half, feeds bytes into a [`TelnetParser`],
/// and forwards `Data`/`Reply` events to the supervisor's
/// `inbound_tx`. The write task drains `outbound_rx` and writes to
/// the write-half, IAC-escaping payload bytes.
pub fn spawn_client(
    incoming: IncomingClient,
    inbound_tx: mpsc::Sender<(ClientId, InboundEvent)>,
) -> (ClientMeta, ClientHandle) {
    spawn_client_with_deadline(incoming, inbound_tx, CLIENT_SEND_TIMEOUT)
}

/// Body of [`spawn_client`] with the socket send deadline as a parameter,
/// so the teardown tests can exercise it without a real ten-second wait.
fn spawn_client_with_deadline(
    incoming: IncomingClient,
    inbound_tx: mpsc::Sender<(ClientId, InboundEvent)>,
    send_timeout: std::time::Duration,
) -> (ClientMeta, ClientHandle) {
    let id = ClientId::new();
    let meta = ClientMeta {
        id,
        peer: incoming.peer,
        readonly: incoming.readonly,
    };
    let (out_tx, out_rx) = mpsc::channel::<OutboundFrame>(64);

    let [read_task, write_task] = match incoming.stream {
        ClientStream::Tcp(s) => {
            // C enables SO_KEEPALIVE on every accepted client socket
            // (clientFactory.cc:146) so a silently-dropped peer (cable
            // pull) is eventually surfaced as a write error instead of
            // pending forever. UNIX sockets have no meaningful keepalive,
            // so this is TCP-only, as in C.
            set_keepalive(&s);
            spawn_split(s, id, incoming.readonly, inbound_tx, out_rx, send_timeout)
        }
        #[cfg(unix)]
        ClientStream::Unix(s) => {
            spawn_split(s, id, incoming.readonly, inbound_tx, out_rx, send_timeout)
        }
        // The terminal gets the same read/write task pair — including the
        // telnet parser, which C also runs on its fd-0 client
        // (`clientFactory.cc:167` `telnet_init`).
        ClientStream::Console(s) => {
            spawn_split(s, id, incoming.readonly, inbound_tx, out_rx, send_timeout)
        }
    };
    // The write task is deliberately detached: it must outlive the handle
    // just long enough to drain the queue, and it terminates on its own
    // when the channel closes or a write misses `send_timeout`.
    drop(write_task);

    (meta, ClientHandle { out_tx, read_task })
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

/// Run one socket operation under C's `SO_SNDTIMEO`. `false` ⟹ the write
/// errored or missed its deadline — C `writeToFd` taking the `-1` branch
/// and setting `_markedForDeletion` (`clientFactory.cc:283-290`).
async fn under_send_deadline<F>(deadline: std::time::Duration, op: F) -> bool
where
    F: std::future::Future<Output = std::io::Result<()>>,
{
    matches!(tokio::time::timeout(deadline, op).await, Ok(Ok(())))
}

/// Generic helper that splits any AsyncRead+AsyncWrite stream and
/// spawns the read+write tasks. Monomorphized once per stream type.
/// Returns the two `JoinHandle`s for [`ClientHandle`] to own; `send_timeout`
/// is [`CLIENT_SEND_TIMEOUT`] in production and a short value in the tests
/// that exercise the deadline.
fn spawn_split<S>(
    stream: S,
    id: ClientId,
    readonly: bool,
    inbound_tx: mpsc::Sender<(ClientId, InboundEvent)>,
    mut outbound_rx: mpsc::Receiver<OutboundFrame>,
    send_timeout: std::time::Duration,
) -> [tokio::task::JoinHandle<()>; 2]
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);

    // Read task: pump socket → telnet parser → inbound events.
    let inbound = inbound_tx.clone();
    let read_task = tokio::spawn(async move {
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
    //
    // Every socket operation carries `send_timeout`, C's `SO_SNDTIMEO`: a
    // peer that stops reading wedges `write_all` forever otherwise, and a
    // wedged write task never returns to `recv()`, so it never notices the
    // supervisor letting go of the client.
    let write_task = tokio::spawn(async move {
        while let Some(frame) = outbound_rx.recv().await {
            let payload = match frame {
                OutboundFrame::Bytes(b) => iac_escape(&b),
                OutboundFrame::RawIac(b) => b,
            };
            if !under_send_deadline(send_timeout, writer.write_all(&payload)).await
                || !under_send_deadline(send_timeout, writer.flush()).await
            {
                break;
            }
        }
        let _ = tokio::time::timeout(send_timeout, writer.shutdown()).await;
    });

    [read_task, write_task]
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
        let (_meta, handle) = spawn_client(
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
        drop(handle);
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
        let (_meta, handle) = spawn_client(
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
        assert!(
            handle
                .send(OutboundFrame::Bytes(vec![0x41, 0xFF, 0x42]))
                .await
        );

        let mut got = [0u8; 4];
        client.read_exact(&mut got).await.unwrap();
        assert_eq!(got, [0x41, 0xFF, 0xFF, 0x42]);
    }

    /// I1: a client that stops reading must not outlive its removal from
    /// the supervisor's roster. C `~clientItem` (`clientFactory.cc:73-80`)
    /// does `shutdown(_fd, SHUT_RDWR); close(_fd)`, so the peer's socket is
    /// gone once the item leaves the connection list. With both tasks
    /// detached instead, the write task stays parked in `write_all`, the
    /// read task stays parked in `read`, and the fd survives for the
    /// supervisor's whole lifetime — repeat the connect-and-stall and
    /// procServ runs out of descriptors.
    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_the_handle_closes_a_stalled_clients_socket() {
        let (sock, mut peer) = tokio::net::UnixStream::pair().unwrap();
        let (in_tx, mut in_rx) = mpsc::channel(8);
        // Keep the inbound channel drained so the read task never parks on
        // a full `inbound_tx` — the socket must be held open by the tasks
        // themselves, not by back-pressure from this test.
        tokio::spawn(async move { while in_rx.recv().await.is_some() {} });
        let (_meta, handle) = spawn_client_with_deadline(
            IncomingClient {
                stream: ClientStream::Unix(sock),
                peer: ClientPeer::Unix(None),
                readonly: false,
            },
            in_tx,
            Duration::from_millis(100),
        );

        // The peer never reads, so the write task wedges in `write_all` and
        // the 64-deep channel backs up behind it.
        for _ in 0..64 {
            if timeout(
                Duration::from_millis(100),
                handle.send(OutboundFrame::Bytes(vec![0u8; 64 * 1024])),
            )
            .await
            .is_err()
            {
                break;
            }
        }

        drop(handle);

        // The peer must see the socket go away. It stays readable (bytes
        // already delivered are still queued), but writing to a socket whose
        // peer has closed fails — whereas a surviving read task would go on
        // consuming these single bytes for as long as the test waits.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let mut closed = false;
        while tokio::time::Instant::now() < deadline {
            if peer.write_all(b"x").await.is_err() {
                closed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            closed,
            "dropping the ClientHandle must end both tasks and close the \
             socket, as C ~clientItem does"
        );
    }

    /// I1: C arms `SO_SNDTIMEO` at 10 s on every accepted client socket
    /// (`clientFactory.cc:104-105` builds the `timeval`, `:147` arms it),
    /// so one stalled `write()` is enough to finish the client off —
    /// `writeToFd` takes the -1 branch and marks it (`:283-290`). Without a
    /// deadline on the write itself the task parks in `write_all` for good,
    /// never returns to `recv()`, and so never notices the supervisor
    /// letting go of the outbound channel.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_write_that_misses_its_deadline_ends_the_write_task() {
        // `_peer` stays alive and never reads: closing it would fail the
        // write for the wrong reason.
        let (sock, _peer) = tokio::net::UnixStream::pair().unwrap();
        let (in_tx, _in_rx) = mpsc::channel(8);
        let (out_tx, out_rx) = mpsc::channel::<OutboundFrame>(64);
        let tasks = spawn_split(
            sock,
            ClientId::new(),
            true,
            in_tx,
            out_rx,
            Duration::from_millis(100),
        );

        // The peer never reads, so once the socket buffers fill `write_all`
        // pends and the 64-deep channel backs up behind it.
        for _ in 0..64 {
            if out_tx
                .try_send(OutboundFrame::Bytes(vec![0u8; 64 * 1024]))
                .is_err()
            {
                break;
            }
        }

        let ended = timeout(Duration::from_secs(3), out_tx.closed()).await;
        for task in tasks {
            task.abort();
        }
        assert!(
            ended.is_ok(),
            "a write past SO_SNDTIMEO must end the write task, which closes \
             the outbound channel and tells the supervisor the client is dead"
        );
    }
}
