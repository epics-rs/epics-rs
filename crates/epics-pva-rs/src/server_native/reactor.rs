//! Reactor PVA server driver — one poll thread, one task per connection.
//!
//! The third driver, beside `super::accept` (hosted, tokio's reactor) and
//! [`super::blocking`] (thread-per-connection, no reactor at all). It serves N
//! connections on a **fixed** three threads:
//!
//! ```text
//!   accept thread ──accepted socket──> Poller::stream ──split──> two halves
//!   UDP responder thread                                            |
//!   poll thread ── wait(armed) ── wake ──> connection task: handle_connection_io
//! ```
//!
//! # Why it exists
//!
//! [`super::blocking`] costs three threads per connection — reader pump,
//! writer pump and the operation body — so an N-client server is `3N+2`
//! threads, measured at 1,589,554 bytes of stack and control block per
//! connection on `armv7-rtems-eabihf`. That is the price of having no reactor,
//! and on a target with a few megabytes of RAM it is what sets
//! `max_connections`. Here the per-connection cost is a task object on the
//! process-global background executor, which multiplexes an unbounded number
//! of mostly-idle futures over a bounded worker set
//! (`epics_base_rs::runtime::background::future_exec`), so the thread budget
//! stops growing with the client count.
//!
//! # The reactor is ours, not mio's
//!
//! `tokio::net` cannot serve either embedded triple, and the obstacle is mio:
//! `mio-1.2.3/src/sys/unix/mod.rs:20-59` maps `target_os` to a selector
//! implementation and names neither `rtems` nor `vxworks`, so an
//! `os-poll`+`net` build fails with 29 errors on `armv7-rtems-eabihf` and 35 on
//! `x86_64-wrs-vxworks` (`E0583`, no selector and no waker). Forcing its
//! internal poll/pipe fallbacks (`--cfg mio_unsupported_force_poll_poll --cfg
//! mio_unsupported_force_waker_pipe`) leaves 8 and 9, and `socket2` — which
//! `tokio::net` also needs — adds 17 and 10 of its own. So the readiness
//! primitive is [`epics_base_rs::runtime::readiness`]: `select(2)` everywhere,
//! `kqueue(2)` on an RTEMS BSP whose version passes pvxs's "kqueue from 6.3"
//! rule. VxWorks 7 stays on `select` because it has no kqueue at all — its own
//! `poll()` is a `select()` wrapper (`libunix.a(poll.o)` references exactly
//! `bzero`, `errnoSet` and `select`).
//!
//! # The protocol is untouched, again
//!
//! `handle_connection_io` takes its reader and writer as
//! `Box<dyn AsyncRead/AsyncWrite>`, so this driver — like the blocking one —
//! adds *implementors* rather than a second protocol. Here they are the two
//! halves of a [`ReadyStream`](epics_base_rs::runtime::readiness::ReadyStream),
//! which share one registration in the poller: read and write are separate
//! interests on the same fd, each with its own waker, so the connection's read
//! side and its writer task park independently.
//!
//! # Where the threads' priorities come from, and the one place they do not
//!
//! The accept loop and the poll thread both take `PVA_SERVER_PRIORITY` —
//! pvxs's `PVXTCP` band, `CAServerLow-2` = 18 — because between them they are
//! what pvxs runs on that one thread. The UDP responder keeps its own lower
//! band through `handle_udp_search_blocking`, so a SEARCH flood cannot starve
//! established connections.
//!
//! The connection futures run at 18 as well, and that is what
//! `pva_task_executor` is for. A bare `Reactor::spawn` on `exec_backend` lands
//! a future on the process-global callback pool's `Medium` band — 64, where
//! C's `callbackRequest` work runs — so `serve` first rebinds the reactor it
//! was handed to this server's own executor. Everything downstream inherits
//! that through its clone of the reactor, the blocking driver's per-connection
//! writer task included, since it spawns through the same seam.
//!
//! # Shutdown: one owner for "may this connection still run"
//!
//! > **MUST** every connection task's abort handle reaches `ConnTable`
//! > before the accept loop takes the next connection.
//! > **MUST NOT** anything abort a connection except `ConnTable::stop`; the
//! > table's own pruning only drops handles whose task already reported
//! > finished.
//!
//! The window that would otherwise leak a connection past `shutdown` — spawn
//! completes, `stop` runs, registration follows — is closed inside the table's
//! lock: `ConnTable::install` aborts the handle it was just given when the
//! table is already stopped, rather than storing it.

// RTEMS-EXEC-MODEL-ALLOW(6): the six accept-loop tests drive a real client
// against a running server, so they need the multi-thread tokio flavor they
// ask for — that is the subject, not an accident. They run on the driver
// `#[tokio::test]` builds and pass in the exec-backend suite (measured:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p epics-pva-rs
// -E 'test(/server_native::reactor::/)'`, 7/7).

use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use epics_base_rs::runtime::accept::AcceptBackoff;
use epics_base_rs::runtime::readiness::Poller;
use epics_base_rs::runtime::task::{Reactor, TaskAbortHandle, enter_ioc_thread};
use tracing::{debug, warn};

use super::blocking::{PVA_SERVER_PRIORITY, handle_udp_search_blocking, tx_limit_bytes};
use super::config::PvaServerConfig;
use super::peers::{PeerEntry, PeerRegistry};
use super::search_engine::random_guid;
use super::source::{ChannelInvalidator, DynSource};
use super::tcp::{ConnInit, handle_connection_io};
use crate::error::PvaResult;

/// The live connections — see the module doc's invariant.
///
/// One table holds both halves of a connection's server-side existence: the
/// entry that says it is still here, and the abort handle that says it may
/// still run. They are not two facts, so they are not two structures — a
/// separate counter beside this one would answer "how many connections" with
/// whichever of the two a teardown path happened to update first.
struct ConnTable {
    state: Mutex<ConnTableState>,
}

struct ConnTableState {
    next_id: u64,
    /// `None` is a connection whose task has been admitted but whose abort
    /// handle has not been installed yet — the accept loop is between
    /// [`ConnTable::admit`] and [`ConnTable::install`]. It is still a live
    /// connection, and [`ConnTable::install`] is where a `stop` that crossed
    /// that window catches it.
    live: std::collections::HashMap<u64, Option<TaskAbortHandle>>,
    /// Latched by [`ConnTable::stop`]. Terminal: a stopped server does not
    /// start serving again.
    stopped: bool,
}

/// What [`ConnTable::admit`] decided. The two refusals are different events —
/// one is the operator's configured limit, the other is a server that is
/// already stopping — and only the first is worth a log line.
enum Admit {
    Ok(ConnSlot),
    AtCapacity,
    Stopped,
}

impl ConnTable {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ConnTableState {
                next_id: 0,
                live: std::collections::HashMap::new(),
                stopped: false,
            }),
        })
    }

    /// Recover from a poisoned lock rather than propagating the panic.
    ///
    /// The rule the blocking driver's registry takes too: a panic in one
    /// connection must not leave the server unable to stop the others, and
    /// nothing in this state can be left half-updated.
    fn state(&self) -> std::sync::MutexGuard<'_, ConnTableState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Admit one connection, under one lock: the limit is checked and the
    /// place is taken together, so there is no window where two accepts read
    /// the same count.
    ///
    /// The returned [`ConnSlot`] *is* the place — dropping it releases the
    /// entry and the peer-registry entry, so an error between here and the
    /// spawn gives both back with no unwind path of its own.
    fn admit(self: &Arc<Self>, peers: &Arc<PeerRegistry>, peer: SocketAddr, limit: usize) -> Admit {
        let mut state = self.state();
        if state.stopped {
            return Admit::Stopped;
        }
        if state.live.len() >= limit {
            return Admit::AtCapacity;
        }
        let id = state.next_id;
        state.next_id += 1;
        state.live.insert(id, None);
        drop(state);
        // Plain TCP only: the peer entry is never a TLS one here.
        let entry = PeerEntry::new(false);
        peers.insert(peer, entry.clone());
        Admit::Ok(ConnSlot {
            table: Arc::clone(self),
            id,
            peers: Arc::clone(peers),
            peer,
            entry,
        })
    }

    /// Give the admitted connection its abort handle.
    ///
    /// Three outcomes, and the first is the one the invariant turns on: a
    /// `stop` that ran between the spawn and this call aborts the task here
    /// rather than storing a handle nobody will ever use.
    fn install(&self, id: u64, handle: TaskAbortHandle) {
        let mut state = self.state();
        if state.stopped {
            handle.abort();
            return;
        }
        match state.live.get_mut(&id) {
            Some(slot) => *slot = Some(handle),
            // The task finished before the accept loop got back here; its slot
            // guard has already released the entry and there is nothing left
            // to abort.
            None => {}
        }
    }

    /// Release one connection's place. [`ConnSlot::drop`] is the only caller —
    /// see the module doc's invariant.
    fn release(&self, id: u64) {
        self.state().live.remove(&id);
    }

    fn stop(&self) {
        let mut state = self.state();
        state.stopped = true;
        // The entries stay: a connection is gone when its slot guard says so,
        // not when the abort is issued, and `live` is what the report reads.
        for handle in state.live.values().flatten() {
            handle.abort();
        }
    }

    /// Connections that still hold their place — the connection count, and the
    /// only one.
    fn live(&self) -> usize {
        self.state().live.len()
    }
}

/// One connection's place in the [`ConnTable`] and in the peer registry, given
/// back however the connection ends — return, error, panic, or abort.
///
/// It is created before the task is spawned and moved into it, so there is no
/// state the accept loop must remember to undo on its own error paths.
struct ConnSlot {
    table: Arc<ConnTable>,
    id: u64,
    peers: Arc<PeerRegistry>,
    peer: SocketAddr,
    entry: Arc<PeerEntry>,
}

impl ConnSlot {
    fn id(&self) -> u64 {
        self.id
    }

    /// The registry entry this connection reports through, as [`ConnInit`]
    /// takes it.
    fn peer_entry(&self) -> Arc<PeerEntry> {
        self.entry.clone()
    }
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        self.peers.remove(self.peer);
        self.table.release(self.id);
    }
}

/// A PVA TCP server driven by a readiness poller: one task per connection,
/// three threads in total.
///
/// The counterpart of [`super::blocking::BlockingPvaServer`] for targets that
/// have no tokio reactor but can afford one of their own — which is both
/// embedded triples, since [`Poller`] needs nothing but `select(2)`.
///
/// TLS is not served: plain TCP only, as the blocking driver, so every
/// connection is registered with `tls = false` and authenticates through
/// CONNECTION_VALIDATION.
pub struct ReactorPvaServer {
    listener: TcpListener,
    source: DynSource,
    config: PvaServerConfig,
    peers: Arc<PeerRegistry>,
    channel_invalidator: ChannelInvalidator,
    connections: Arc<ConnTable>,
    poller: Arc<Poller>,
    /// The executor connection futures spawn onto — see
    /// [`pva_task_executor`](super::blocking::pva_task_executor). `None` on
    /// `tokio_backend`, where the runtime is the executor.
    task_exec: Option<Arc<epics_base_rs::runtime::background::DedicatedExecutor>>,
    tcp_port: u16,
    shutdown: AtomicBool,
}

impl ReactorPvaServer {
    /// Bind the accept socket and start the poll thread.
    ///
    /// Both happen here for the reason [`super::blocking::BlockingPvaServer::bind`]
    /// binds its listener here: a constructed server is a listening server, so
    /// [`tcp_port`](Self::tcp_port) is answerable before any connection exists
    /// and the port is never probed, released and re-bound.
    pub fn bind<A: ToSocketAddrs>(
        addr: A,
        source: DynSource,
        config: PvaServerConfig,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        // On RTEMS `bind` succeeds and `local_addr` fails (libc omits the BSD
        // `sin_len` byte), so the two failures must stay distinguishable
        // rather than both surfacing as "cannot bind".
        let tcp_port = listener
            .local_addr()
            .map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("bind() succeeded; local_addr() on the listener failed: {e}"),
                )
            })?
            .port();
        // One GUID per server, stamped at construction so the UDP responder
        // and the TCP-circuit SEARCH handler cannot advertise two identities;
        // `?` rather than a fallback because every consumer of a GUID
        // collision degrades silently.
        let mut config = config;
        config.guid = random_guid()?;
        let source = crate::server_native::server_info::compose_with_server_info(source)
            .map_err(io::Error::other)?;
        let channel_invalidator = ChannelInvalidator::new();
        source.set_channel_invalidator(channel_invalidator.clone());
        // `PVASPOLL`: RTEMS truncates thread names at 16 bytes.
        let poller = Poller::new("PVASPOLL", PVA_SERVER_PRIORITY)?;
        // Started here, not on first accept: a server that cannot run its
        // connections is a `bind` failure, like a poll thread that will not
        // start.
        let task_exec = super::blocking::pva_task_executor()?;
        Ok(Self {
            listener,
            source,
            config,
            peers: PeerRegistry::new(),
            channel_invalidator,
            connections: ConnTable::new(),
            poller,
            task_exec,
            tcp_port,
            shutdown: AtomicBool::new(false),
        })
    }

    /// `reactor` bound to this server's own executor, or `reactor` unchanged
    /// where there is none to bind (`tokio_backend`).
    ///
    /// Called once per `serve`, at the top, so nothing below it can reach the
    /// caller's reactor by accident: the band is a property of the server, not
    /// of the call site that happens to spawn.
    fn conn_reactor(&self, reactor: &Reactor) -> Reactor {
        match &self.task_exec {
            Some(exec) => reactor.with_executor(exec),
            None => reactor.clone(),
        }
    }

    /// The actual bound address — the value to use when the configured port
    /// was 0.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// The bound TCP port, as SEARCH replies must advertise it.
    pub fn tcp_port(&self) -> u16 {
        self.tcp_port
    }

    /// This server's GUID, as both SEARCH paths advertise it.
    pub fn guid(&self) -> [u8; 12] {
        self.config.guid
    }

    /// Live per-connection accounting, for the server report (`pvxsr`).
    pub fn peers(&self) -> &Arc<PeerRegistry> {
        &self.peers
    }

    /// Connections currently being served — `ConnTable`'s count, which is
    /// the only one.
    pub fn active_connections(&self) -> usize {
        self.connections.live()
    }

    /// Answer UDP SEARCHes on `socket` until [`shutdown`](Self::shutdown).
    ///
    /// The same blocking responder the thread-per-connection driver runs, on a
    /// thread of its own: one socket with a 200 ms stop tick is not what the
    /// poller exists for, and sharing the proven loop keeps the two drivers'
    /// SEARCH answers byte-identical.
    ///
    /// Blocks the calling thread and takes it to the UDP band, which is below
    /// the accept loop's — give it a *different* thread from
    /// [`serve`](Self::serve).
    pub fn serve_udp_search(&self, socket: UdpSocket) -> PvaResult<()> {
        handle_udp_search_blocking(
            socket,
            &self.source,
            &self.config,
            self.tcp_port,
            &self.shutdown,
        )
    }

    /// Accept until [`shutdown`](Self::shutdown).
    ///
    /// Blocks the calling thread and takes it to `PVA_SERVER_PRIORITY`, so
    /// give it a thread of its own. The accept socket stays blocking rather
    /// than joining the poller: an accept loop that parks in `accept()` is
    /// woken by [`shutdown`](Self::shutdown)'s self-connect, which is one
    /// mechanism instead of two.
    pub fn serve(&self, reactor: &Reactor) {
        let _ = enter_ioc_thread(PVA_SERVER_PRIORITY);
        // From here down `reactor` is the server's own: every connection
        // future, and every task those futures go on to spawn through their
        // clone of it, runs on this server's band rather than the callback
        // pool's.
        let reactor = &self.conn_reactor(reactor);
        let mut backoff = AcceptBackoff::new();
        for stream in self.listener.incoming() {
            match stream {
                Ok(stream) => {
                    backoff.accepted();
                    // `shutdown` wakes this parked `accept` by dialling our own
                    // socket; that throwaway connection arrives here and is
                    // dropped with the flag already set.
                    if self.shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    let peer = match stream.peer_addr() {
                        Ok(peer) => peer,
                        Err(e) => {
                            warn!(error = %e, "reactor PVA server: peer_addr failed, dropping connection");
                            continue;
                        }
                    };
                    if let Err(e) = self.start_connection(reactor, stream, peer) {
                        warn!(?peer, error = %e, "reactor PVA server: connection not started");
                    }
                }
                Err(e) => {
                    if self.shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    warn!(error = %e, "reactor PVA server: accept failed");
                    // The failed connection is still queued, so an immediate
                    // retry spins at 100% CPU exactly when the machine is out
                    // of fds or memory.
                    thread::sleep(backoff.failed());
                    // `shutdown()` may have been asked for while we slept, and
                    // its self-dial cannot wake an `accept()` that is failing.
                    if self.shutdown.load(Ordering::Acquire) {
                        break;
                    }
                }
            }
        }
    }

    /// Register the socket with the poller and spawn its connection task.
    ///
    /// Admission is a plain read-then-compare because the accept loop is the
    /// count's only producer: nothing else can raise it between the two
    /// operations, and a connection retiring concurrently only lowers it,
    /// which admits a client the limit would have allowed a moment later.
    fn start_connection(
        &self,
        reactor: &Reactor,
        stream: TcpStream,
        peer: SocketAddr,
    ) -> io::Result<()> {
        let slot = match self
            .connections
            .admit(&self.peers, peer, self.config.max_connections)
        {
            Admit::Ok(slot) => slot,
            Admit::AtCapacity => {
                // Dropping the stream closes it — a refused client must see a
                // closed socket, not a held-open one.
                //
                // `warn!`, not `debug!`: a client that cannot connect is an
                // operator-visible event, and at `debug!` this is below every
                // default filter including the IOC console subscriber.
                warn!(
                    ?peer,
                    limit = self.config.max_connections,
                    "reactor PVA server: refusing connection, max_connections reached"
                );
                return Ok(());
            }
            // The server is stopping; this is the self-connect or a client that
            // raced it, and neither is an event.
            Admit::Stopped => return Ok(()),
        };

        let ready = self.poller.stream(stream)?;
        let tx_limit = tx_limit_bytes(ready.socket());
        let (reader, writer) = ready.split();

        let source = self.source.clone();
        let config = self.config.clone();
        // Plain TCP only: no x509 identity, authentication is through
        // CONNECTION_VALIDATION.
        let init = ConnInit {
            peer_entry: slot.peer_entry(),
            x509_identity: None,
            channel_invalidator: self.channel_invalidator.clone(),
            tx_limit_bytes: tx_limit,
        };
        let conn_reactor = reactor.clone();
        let id = slot.id();
        let handle = reactor.spawn(async move {
            let _slot = slot;
            let outcome = handle_connection_io(
                conn_reactor,
                source,
                Box::new(reader),
                Box::new(writer),
                peer,
                config,
                init,
            )
            .await;
            if let Err(e) = outcome {
                debug!(?peer, error = %e, "reactor PVA connection ended with error");
            }
        });
        self.connections.install(id, handle.abort_handle());
        Ok(())
    }

    /// Stop accepting, stop answering SEARCHes, and end the connections
    /// already running. Idempotent.
    ///
    /// Three operations, in the order that leaves nothing parked: the flag plus
    /// a self-connect returns the accept loop, `ConnTable::stop` aborts every
    /// live connection task, and only then is the poll thread stopped — a task
    /// that survives the abort finds its next `arm` refused and unwinds through
    /// its own error path rather than parking on a dead poller.
    ///
    /// Returns once the aborts are issued; a connection observes cancellation
    /// at its next suspension point, so a caller that must see them gone
    /// watches [`active_connections`](Self::active_connections).
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        if let Ok(addr) = self.listener.local_addr() {
            let _ = TcpStream::connect(addr);
        }
        self.connections.stop();
        self.poller.shutdown();
    }
}

impl Drop for ReactorPvaServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{ByteOrder, Command, ReadExt};
    // The frame rig, the test source and the harness runtime are the blocking
    // driver's: both drivers must answer the *same* bytes, so a second copy of
    // the client would let them drift apart without a test noticing.
    use super::super::blocking::tests::{
        TestClient, app_frame, create_channel_payload, eventually, harness_reactor,
        isolated_config, test_source,
    };
    use epics_base_rs::runtime::task::{StackSizeClass, spawn_dedicated_thread};
    use std::io::{Cursor, Read};
    use std::net::Ipv4Addr;
    use std::time::Duration;

    fn start_server(config: PvaServerConfig) -> (Arc<ReactorPvaServer>, thread::JoinHandle<()>) {
        let server = Arc::new(
            ReactorPvaServer::bind((Ipv4Addr::LOCALHOST, 0), test_source(), config)
                .expect("bind the reactor PVA server"),
        );
        let serving = server.clone();
        let accept = spawn_dedicated_thread(
            "test-PVAR-accept".into(),
            PVA_SERVER_PRIORITY,
            StackSizeClass::Medium,
            move || serving.serve(&harness_reactor()),
        )
        .expect("accept thread spawned");
        (server, accept)
    }

    /// Shut the server down and collect its accept thread under a bound —
    /// `JoinHandle::join` cannot time out, so a regression in `shutdown` would
    /// hang the run rather than fail it.
    fn stop_and_join(server: &ReactorPvaServer, accept: thread::JoinHandle<()>) {
        server.shutdown();
        eventually("shutdown returns the accept loop", || accept.is_finished());
        accept.join().expect("accept thread joined");
    }

    /// The whole arg assembly, end to end: a connection served through the
    /// poller must answer CREATE_CHANNEL exactly as the other two drivers do.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_accept_loop_serves_a_real_channel() {
        let order = ByteOrder::Little;
        let (server, accept) = start_server(isolated_config());
        let mut client = TestClient::connect(server.local_addr().expect("addr"));

        client.send(&app_frame(
            Command::CreateChannel,
            order,
            create_channel_payload(21, "dut", order),
        ));
        let body = client.read_until(Command::CreateChannel);
        let mut cur = Cursor::new(&body[..]);
        assert_eq!(cur.get_u32(order).expect("cid"), 21, "cid echoed");
        let _sid = cur.get_u32(order).expect("sid");
        assert_eq!(
            cur.get_u8().expect("status"),
            0xFF,
            "a connection accepted by the reactor driver must serve channels"
        );

        stop_and_join(&server, accept);
    }

    /// The peer registry and the connection count are the accept loop's job,
    /// and both come back through the task's slot guard however it ends.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_connection_is_tracked_in_the_peer_registry_and_released() {
        let (server, accept) = start_server(isolated_config());
        let client = TestClient::connect(server.local_addr().expect("addr"));

        eventually("the peer registry gains the connection", || {
            server.peers().snapshot().len() == 1
        });
        assert_eq!(server.active_connections(), 1);
        let (_peer, snap) = server.peers().snapshot().remove(0);
        assert!(!snap.tls, "the reactor driver serves plain TCP only");

        client.close();
        eventually("the peer registry releases the connection", || {
            server.peers().snapshot().is_empty()
        });
        eventually("the connection slot is returned", || {
            server.active_connections() == 0
        });

        stop_and_join(&server, accept);
    }

    /// Boundary: shutdown with a live connection that is not going anywhere by
    /// itself. The abort is what ends it — there is no socket shutdown here,
    /// because a task can be cancelled where a thread cannot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_ends_a_live_connection() {
        let order = ByteOrder::Little;
        let (server, accept) = start_server(isolated_config());
        let mut client = TestClient::connect(server.local_addr().expect("addr"));

        // Drive one exchange so the connection is provably established and
        // then silent: nothing but `shutdown` can end it.
        client.send(&app_frame(
            Command::CreateChannel,
            order,
            create_channel_payload(31, "dut", order),
        ));
        let _ = client.read_until(Command::CreateChannel);
        eventually("the connection is tracked", || {
            server.active_connections() == 1
        });
        assert_eq!(server.connections.live(), 1);

        server.shutdown();
        eventually("shutdown ends the live connection", || {
            server.active_connections() == 0
        });
        assert!(
            server.peers().snapshot().is_empty(),
            "its peer entry goes with it"
        );
        assert_eq!(server.connections.live(), 0);
        eventually("shutdown returns the accept loop", || accept.is_finished());
        accept.join().expect("accept thread joined");
    }

    /// Boundary: the abort handle arrives after `stop` has latched. The
    /// window between the spawn and the install is the one way a connection
    /// could outlive the server, and the table closes it by aborting the
    /// handle it was just given instead of storing it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_abort_handle_installed_after_stop_aborts_at_once() {
        let table = ConnTable::new();
        let peers = PeerRegistry::new();
        let peer: SocketAddr = "127.0.0.1:65000".parse().expect("peer");
        let slot = match table.admit(&peers, peer, 8) {
            Admit::Ok(slot) => slot,
            _ => panic!("an empty table must admit"),
        };
        let reactor = harness_reactor();
        let handle = reactor.spawn(async {
            // Long enough that only an abort can end it inside this test.
            epics_base_rs::runtime::task::sleep(Duration::from_secs(3600)).await;
        });
        let abort = handle.abort_handle();

        table.stop();
        table.install(slot.id(), abort.clone());

        eventually("the late install aborts the task", || abort.is_finished());
        assert_eq!(
            table.live(),
            1,
            "the connection still holds its place until its slot guard drops"
        );
        drop(slot);
        assert_eq!(table.live(), 0);
        assert!(peers.snapshot().is_empty(), "and its peer entry with it");
    }

    /// Boundary: `admit` after `stop`. A client that arrives while the server
    /// is stopping — the self-connect included — must not be served at all.
    #[test]
    fn admit_after_stop_refuses_without_taking_a_place() {
        let table = ConnTable::new();
        let peers = PeerRegistry::new();
        let peer: SocketAddr = "127.0.0.1:65001".parse().expect("peer");
        table.stop();
        assert!(matches!(table.admit(&peers, peer, 8), Admit::Stopped));
        assert_eq!(table.live(), 0);
        assert!(peers.snapshot().is_empty());
    }

    /// Boundary: at the connection limit. The refusal must close the socket
    /// rather than queue it, and must not consume a slot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn max_connections_refuses_the_next_client() {
        let config = PvaServerConfig {
            max_connections: 1,
            ..isolated_config()
        };
        let (server, accept) = start_server(config);
        let first = TestClient::connect(server.local_addr().expect("addr"));
        eventually("the first connection is served", || {
            server.active_connections() == 1
        });

        let mut refused = TestClient::connect(server.local_addr().expect("addr"));
        let mut byte = [0u8; 1];
        assert_eq!(
            refused
                .0
                .read(&mut byte)
                .expect("read on the refused socket"),
            0,
            "over the limit the server must close the socket, not hold it open"
        );
        assert_eq!(
            server.active_connections(),
            1,
            "a refused connection must not take a slot"
        );

        // And the slot frees up, so the limit is a limit and not a latch.
        first.close();
        eventually("the first connection releases its slot", || {
            server.active_connections() == 0
        });

        stop_and_join(&server, accept);
    }

    /// The claim this driver exists for, measured rather than asserted: the
    /// thread count must not grow with the client count.
    ///
    /// The blocking driver costs three threads per connection, so eight
    /// clients there are twenty-four new threads. Here the connections are
    /// task objects multiplexed over an executor that already exists, so the
    /// budget is the accept thread, the poll thread, and whatever the executor
    /// had before. Measured here it is **zero** new threads for eight clients;
    /// the assertion is looser than the measurement on purpose, since a hosted
    /// run may grow a tokio worker for reasons that have nothing to do with
    /// this driver, and what must fail is per-connection growth.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_thread_count_does_not_grow_with_the_client_count() {
        const CLIENTS: usize = 8;
        fn threads() -> usize {
            std::fs::read_dir("/proc/self/task")
                .expect("/proc/self/task")
                .count()
        }

        let config = PvaServerConfig {
            max_connections: 32,
            ..isolated_config()
        };
        let (server, accept) = start_server(config);
        let order = ByteOrder::Little;

        // One client first, so every lazily-started facility the first
        // connection touches is already up when the baseline is read.
        let mut warmup = TestClient::connect(server.local_addr().expect("addr"));
        warmup.send(&app_frame(
            Command::CreateChannel,
            order,
            create_channel_payload(1, "dut", order),
        ));
        let _ = warmup.read_until(Command::CreateChannel);
        let before = threads();

        let mut clients = Vec::new();
        for i in 0..CLIENTS {
            let mut client = TestClient::connect(server.local_addr().expect("addr"));
            client.send(&app_frame(
                Command::CreateChannel,
                order,
                create_channel_payload(100 + i as u32, "dut", order),
            ));
            let _ = client.read_until(Command::CreateChannel);
            clients.push(client);
        }
        eventually("every client is served", || {
            server.active_connections() == CLIENTS + 1
        });

        let after = threads();
        assert!(
            after - before < CLIENTS,
            "{CLIENTS} connections added {} threads ({before} -> {after}); the \
             thread-per-connection driver this replaces would add {}",
            after - before,
            CLIENTS * 3
        );

        for client in &clients {
            client.close();
        }
        stop_and_join(&server, accept);
    }
}
