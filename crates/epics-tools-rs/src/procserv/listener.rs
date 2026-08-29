//! Connection acceptors.
//!
//! Spawns one task per configured listener (TCP + UNIX) that accepts
//! incoming connections and hands each fresh socket off to the
//! supervisor as a new [`super::client::IncomingClient`]. Mirrors
//! C `acceptFactory.cc` (`acceptItemTCP` / `acceptItemUNIX`).

use std::net::SocketAddr;
use std::net::TcpListener as StdTcpListener;
use std::os::unix::net::UnixListener as StdUnixListener;

use tokio::net::{TcpListener, TcpSocket, UnixListener};
use tokio::sync::mpsc;

use crate::procserv::client::{ClientPeer, ClientStream, IncomingClient};
use crate::procserv::config::ListenConfig;
use crate::procserv::endpoint::{Endpoint, UnixEndpoint};
use crate::procserv::error::{ProcServError, ProcServResult};

/// `listen(2)` backlog. C uses 5 (`acceptFactory.cc:216,363`); the Rust
/// port keeps tokio's larger default so a connection burst is queued
/// rather than refused past the 6th — a deliberate, strictly-more-tolerant
/// divergence (PS-34), not bug-copied down to 5.
const LISTEN_BACKLOG: u32 = 1024;

/// Backoff after a transient accept error. A *persistent* failure (e.g.
/// `EMFILE` until a descriptor frees) would otherwise tight-loop
/// `accept → warn → accept`, burning a core until an fd is available. C
/// re-enters its 0.5s `pselect` between attempts; we keep the bound
/// listener (unlike C's `remakeConnection` teardown, `acceptFactory.cc:396`)
/// and just pause briefly so a stuck error doesn't busy-spin (PS-35).
const ACCEPT_ERROR_BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);

/// Removes a filesystem UNIX-socket node when its listener task ends,
/// mirroring C's `~acceptItemUNIX` destructor (`acceptFactory.cc:331-335`,
/// `unlink(addr.sun_path)` for non-abstract sockets). Held across the
/// accept loop, it fires on normal return AND on task abort / runtime
/// teardown alike — the future, and this guard with it, is dropped at the
/// suspended `.await` — so the socket file never outlives the listener.
/// Abstract sockets have no filesystem presence and get no guard (PS-36).
struct UnlinkOnDrop(std::path::PathBuf);

impl Drop for UnlinkOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A listener that is bound but not yet accepting — the bind half split
/// from the accept loop. Holding the `std` listener (whose fd survives
/// `fork(2)`) lets the bind happen in the foreground parent *before*
/// `daemon::fork_and_go`, so a bind failure fail-fasts there instead of
/// leaving a daemonized-but-headless IOC (PS-49). C binds every
/// `acceptItem` before `forkAndGo` and `exit(error)`s on failure
/// (`procServ.cc:513-543,551`).
enum BoundEndpoint {
    /// The `SocketAddr` is read back from the kernel right after the bind
    /// (C `getsockname`, `acceptFactory.cc:222`), so a `:0` config carries
    /// the real assigned port from the moment the listener exists.
    Tcp(StdTcpListener, SocketAddr),
    Unix(StdUnixListener, UnixEndpoint),
}

/// A bound listener's address, for publishing (info file / `PROCSERV_INFO`).
/// TCP carries the kernel-reported address; UNIX carries the endpoint spec
/// (C `writeAddress` prints `addr.sun_path`, which is the bound path).
pub enum BoundAddr<'a> {
    Tcp(SocketAddr),
    Unix(&'a UnixEndpoint),
}

/// One bound-but-not-accepting listener plus the metadata its accept loop
/// needs. Produced by [`bind_endpoints`] (eager, fail-fast) and consumed by
/// [`PreboundListener::accept`] (the async loop). Not `Clone` — it owns a
/// listening fd.
pub struct PreboundListener {
    listener: BoundEndpoint,
    readonly: bool,
    role: &'static str,
}

impl PreboundListener {
    /// The address this listener is actually bound to, for a TCP endpoint
    /// bound with port `0` (OS-assigned) — `None` for a UNIX endpoint,
    /// which has no numeric port. Lets a caller that bound with port `0`
    /// via [`bind_endpoints`] learn the real port with zero gap between
    /// bind and use, unlike bind-query-drop-then-reuse-the-number, which
    /// races anyone else binding an ephemeral port in that gap.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        match &self.listener {
            BoundEndpoint::Tcp(_, addr) => Some(*addr),
            BoundEndpoint::Unix(..) => None,
        }
    }

    /// The bound address for publishing (info file / `PROCSERV_INFO`).
    /// For TCP this is the kernel-reported address captured at bind time
    /// (C refreshes its `addr` member via `getsockname` right after
    /// `bind`+`listen`, `acceptFactory.cc:222`, and `writeInfoFile` prints
    /// that member), so a config that said port `0` publishes the real
    /// assigned port, never the `:0` placeholder.
    pub fn bound_addr(&self) -> BoundAddr<'_> {
        match &self.listener {
            BoundEndpoint::Tcp(_, addr) => BoundAddr::Tcp(*addr),
            BoundEndpoint::Unix(_, ep) => BoundAddr::Unix(ep),
        }
    }

    /// `true` ⇒ read-only log/viewer endpoint (bound from `listen.log`);
    /// `false` ⇒ control endpoint.
    pub fn readonly(&self) -> bool {
        self.readonly
    }

    /// Run this listener's accept loop until the supervisor's `out` channel
    /// closes. A loop that exits with an error is logged; the supervisor
    /// keeps running its other endpoints (steady-state accept failures are
    /// per-listener in C too).
    pub async fn accept(self, out: mpsc::Sender<IncomingClient>) {
        let res = match self.listener {
            BoundEndpoint::Tcp(l, _) => run_tcp_accept(l, self.readonly, out).await,
            BoundEndpoint::Unix(l, ep) => run_unix_accept(l, ep, self.readonly, out).await,
        };
        if let Err(e) = res {
            tracing::error!(error = %e, role = self.role, "procserv-rs: listener exited");
        }
    }
}

/// Bind every configured control and log endpoint, returning the bound
/// (std) listeners or the first bind error. Binds control specs first,
/// then the log spec (C order, `procServ.cc:513-543`).
///
/// Call this in the foreground process — directly in foreground/library
/// mode, or *before* `daemon::fork_and_go` in daemon mode — so a bind
/// failure (`EADDRINUSE`, abstract-socket-on-non-Linux, bad UNIX path)
/// aborts startup with a real exit status instead of being swallowed in a
/// detached task (PS-49). The bound fds are inherited by the daemon child
/// across the fork.
///
/// Requires an active tokio runtime: the TCP `SO_REUSEADDR` bind and the
/// UNIX/abstract binds go through tokio listeners (reused verbatim for
/// their tested option/permission handling) which are then detached to
/// their `std` form via `into_std`.
pub fn bind_endpoints(listen: &ListenConfig) -> ProcServResult<Vec<PreboundListener>> {
    let mut bound = Vec::new();
    for ep in &listen.control {
        bound.push(bind_one(ep.clone(), false, "control")?);
    }
    if let Some(ep) = &listen.log {
        bound.push(bind_one(ep.clone(), true, "log")?);
    }
    Ok(bound)
}

/// Bind a single [`Endpoint`] to its `std` listener form, applying UNIX
/// permissions at bind time (as C does, pre-fork).
fn bind_one(ep: Endpoint, readonly: bool, role: &'static str) -> ProcServResult<PreboundListener> {
    let listener = match ep {
        Endpoint::Tcp(addr) => {
            let std_listener = tcp_listen(addr)?.into_std().map_err(ProcServError::Io)?;
            // Read the real address back from the kernel while still in the
            // fail-fast bind path (C `getsockname` right after `bind`+`listen`,
            // `acceptFactory.cc:222`) — with port `0` this is the assigned
            // port, and it is what gets published, not the config placeholder.
            let bound = std_listener.local_addr().map_err(ProcServError::Io)?;
            BoundEndpoint::Tcp(std_listener, bound)
        }
        Endpoint::Unix(u) => {
            let std_listener = bind_unix(&u)?.into_std().map_err(ProcServError::Io)?;
            BoundEndpoint::Unix(std_listener, u)
        }
    };
    Ok(PreboundListener {
        listener,
        readonly,
        role,
    })
}

/// Run the TCP accept loop on a pre-bound listener until the supervisor's
/// `out` channel closes. Each accepted socket is wrapped in
/// [`IncomingClient`] and forwarded.
///
/// The listener was bound earlier by [`bind_endpoints`] (so a bind failure
/// already fail-fasted in the foreground process, PS-49); here it is just
/// re-adopted into the current runtime via `from_std`.
///
/// `readonly`: when true, every accepted client is read-only —
/// matches C procServ's `--readonly` deployment for sites that want
/// only log-style viewers (separate listening port for observers
/// vs operators).
async fn run_tcp_accept(
    std_listener: StdTcpListener,
    readonly: bool,
    out: mpsc::Sender<IncomingClient>,
) -> ProcServResult<()> {
    std_listener
        .set_nonblocking(true)
        .map_err(ProcServError::Io)?;
    let listener = TcpListener::from_std(std_listener).map_err(ProcServError::Io)?;
    let bind = listener.local_addr().map_err(ProcServError::Io)?;
    tracing::info!(addr = %bind, readonly, "procserv-rs: TCP listener accepted");

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let inc = IncomingClient {
                    stream: ClientStream::Tcp(stream),
                    peer: ClientPeer::Tcp(peer),
                    readonly,
                };
                if out.send(inc).await.is_err() {
                    // Supervisor went away; we're shutting down.
                    return Ok(());
                }
            }
            Err(e) => {
                // Per-accept errors (e.g. EMFILE) are recoverable;
                // log and keep listening so a transient
                // file-descriptor exhaustion doesn't kill the gateway.
                // Pause first so a *persistent* error doesn't busy-spin.
                tracing::warn!(error = %e, "procserv-rs: TCP accept error");
                tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
            }
        }
    }
}

/// Bind a TCP listen socket with `SO_REUSEADDR` set before `bind`, exactly
/// as C `acceptItemTCP::remakeConnection` (`acceptFactory.cc:187-191,207`).
/// procServ is itself frequently restarted (systemd); without
/// `SO_REUSEADDR`, a restart while prior client connections linger in
/// `TIME_WAIT` can fail `bind` with `EADDRINUSE` and abort startup. tokio's
/// `TcpListener::bind` does not set the option, so go through `TcpSocket`.
fn tcp_listen(bind: SocketAddr) -> ProcServResult<TcpListener> {
    let socket = match bind {
        SocketAddr::V4(_) => TcpSocket::new_v4(),
        SocketAddr::V6(_) => TcpSocket::new_v6(),
    }
    .map_err(|e| ProcServError::ListenerBind(format!("TCP {bind}: {e}")))?;
    socket
        .set_reuseaddr(true)
        .map_err(|e| ProcServError::ListenerBind(format!("TCP {bind} SO_REUSEADDR: {e}")))?;
    socket
        .bind(bind)
        .map_err(|e| ProcServError::ListenerBind(format!("TCP {bind}: {e}")))?;
    socket
        .listen(LISTEN_BACKLOG)
        .map_err(|e| ProcServError::ListenerBind(format!("TCP {bind} listen: {e}")))
}

/// UNIX-socket accept loop on a pre-bound listener (C `acceptItemUNIX`,
/// `acceptFactory.cc:229-381`). The socket was bound — and, for a
/// filesystem socket, had its permissions applied — earlier by
/// [`bind_endpoints`]; here it is re-adopted into the current runtime via
/// `from_std` and accepted in a loop.
///
/// The filesystem socket node is `unlink`ed when this task ends (the
/// [`UnlinkOnDrop`] guard below) for the same reason C does it in
/// `~acceptItemUNIX` — a clean shutdown leaves no stale node. An abstract
/// socket (Linux-only) has no filesystem presence, so it gets no guard.
async fn run_unix_accept(
    std_listener: StdUnixListener,
    ep: UnixEndpoint,
    readonly: bool,
    out: mpsc::Sender<IncomingClient>,
) -> ProcServResult<()> {
    std_listener
        .set_nonblocking(true)
        .map_err(ProcServError::Io)?;
    let listener = UnixListener::from_std(std_listener).map_err(ProcServError::Io)?;
    // Unlink the filesystem socket node when this task ends, so a clean
    // shutdown leaves no stale node behind (C `~acceptItemUNIX`,
    // acceptFactory.cc:331-335). Abstract sockets have no filesystem
    // presence, so they get no guard (PS-36).
    let _unlink_guard = (!ep.abstract_socket).then(|| UnlinkOnDrop(ep.name.clone()));
    // `ClientPeer::Unix` is display-only; an abstract socket has no
    // filesystem path, so report `None` for it.
    let peer_path = (!ep.abstract_socket).then(|| ep.name.clone());
    tracing::info!(
        name = %ep.name.display(),
        abstract_socket = ep.abstract_socket,
        readonly,
        "procserv-rs: UNIX listener accepted"
    );

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let inc = IncomingClient {
                    stream: ClientStream::Unix(stream),
                    peer: ClientPeer::Unix(peer_path.clone()),
                    readonly,
                };
                if out.send(inc).await.is_err() {
                    return Ok(());
                }
            }
            Err(e) => {
                // Same recoverable-but-don't-busy-spin handling as the TCP
                // loop above (PS-35).
                tracing::warn!(error = %e, "procserv-rs: UNIX accept error");
                tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
            }
        }
    }
}

/// Bind a [`UnixEndpoint`] to a tokio [`UnixListener`]. Filesystem and
/// abstract sockets take different paths; the abstract form is Linux-only
/// (C errors out elsewhere, `acceptFactory.cc:294-299`).
fn bind_unix(ep: &UnixEndpoint) -> ProcServResult<UnixListener> {
    if ep.abstract_socket {
        bind_abstract(&ep.name)
    } else {
        // Best-effort unlink of a stale socket — ignore "not found"
        // (C `acceptFactory.cc:341-346`).
        let _ = std::fs::remove_file(&ep.name);
        let listener = UnixListener::bind(&ep.name)
            .map_err(|e| ProcServError::ListenerBind(format!("UNIX {}: {e}", ep.name.display())))?;
        apply_unix_perms(ep);
        Ok(listener)
    }
}

/// Apply C's post-bind access control to a filesystem UNIX socket
/// (`acceptFactory.cc:368-377`): `chmod 0` → `chown` → `chmod perms`. The
/// 0-then-perms order closes the window in which the socket already
/// carries its final mode but still has the pre-`chown` owner. A
/// `chmod`/`chown` failure is logged and tolerated, exactly as C's
/// PRINTF-and-continue (a non-root server cannot `chown`, but the default
/// `0o666` still makes the socket usable). Abstract sockets have no
/// filesystem node, so this is never called for them.
fn apply_unix_perms(ep: &UnixEndpoint) {
    use std::os::unix::fs::PermissionsExt;

    let path = ep.name.as_path();
    // Lock the socket down (mode 0) before the ownership change.
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o0));

    // C always `chown`s (default uid/gid = `getuid`/`getgid` = a no-op);
    // we skip the syscall when no owner override was requested, which is
    // behaviourally identical to chowning to self.
    if ep.uid.is_some() || ep.gid.is_some() {
        let uid = ep.uid.map(nix::unistd::Uid::from_raw);
        let gid = ep.gid.map(nix::unistd::Gid::from_raw);
        if let Err(e) = nix::unistd::chown(path, uid, gid) {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "procserv-rs: chown unix socket failed"
            );
        }
    }

    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(ep.perms)) {
        tracing::warn!(
            path = %path.display(),
            perms = format!("{:o}", ep.perms),
            error = %e,
            "procserv-rs: chmod unix socket failed"
        );
    }
}

/// Bind a Linux abstract-namespace UNIX socket (leading-NUL address).
/// tokio has no direct API, so build a std listener via `bind_addr` and
/// adopt it (`acceptFactory.cc:316-325`).
#[cfg(target_os = "linux")]
fn bind_abstract(name: &std::path::Path) -> ProcServResult<UnixListener> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::{SocketAddr as StdSocketAddr, UnixListener as StdUnixListener};

    let label = || format!("UNIX abstract @{}", name.display());
    let addr = StdSocketAddr::from_abstract_name(name.as_os_str().as_bytes())
        .map_err(|e| ProcServError::ListenerBind(format!("{}: {e}", label())))?;
    let std_listener = StdUnixListener::bind_addr(&addr)
        .map_err(|e| ProcServError::ListenerBind(format!("{}: {e}", label())))?;
    std_listener
        .set_nonblocking(true)
        .map_err(ProcServError::Io)?;
    UnixListener::from_std(std_listener).map_err(ProcServError::Io)
}

/// Non-Linux hosts have no abstract namespace; C exits with the same
/// diagnostic (`acceptFactory.cc:294-298`).
#[cfg(not(target_os = "linux"))]
fn bind_abstract(name: &std::path::Path) -> ProcServResult<UnixListener> {
    Err(ProcServError::ListenerBind(format!(
        "Abstract unix sockets not supported by this host (@{})",
        name.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    #[tokio::test]
    async fn tcp_listener_forwards_accepted_socket() {
        // Bind once, to an OS-assigned port, and read the real address
        // back from that same listener before handing it to
        // `run_tcp_accept` — no separate bind-query-drop-then-reuse-the-
        // number step, so nothing else on the box can steal the port in
        // between (that was this test's own flake: another test's ephemeral
        // bind could land on the number in the gap between `drop(listener)`
        // and `tcp_listen(actual)` re-binding it).
        let bind = "127.0.0.1:0".parse::<SocketAddr>().unwrap();
        let listener = tcp_listen(bind).unwrap();
        let actual = listener.local_addr().unwrap();
        let std_l = listener.into_std().unwrap();

        let (tx, mut rx) = mpsc::channel(4);
        let server = tokio::spawn(async move { run_tcp_accept(std_l, false, tx).await });

        // Give the listener a moment to bind.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut conn = TcpStream::connect(actual).await.unwrap();
        let inc = rx.recv().await.expect("got incoming");
        assert!(matches!(inc.stream, ClientStream::Tcp(_)));
        assert!(matches!(inc.peer, ClientPeer::Tcp(_)));
        assert!(!inc.readonly);

        // Round-trip a byte to confirm the socket is live.
        let mut server_stream = match inc.stream {
            ClientStream::Tcp(s) => s,
            _ => unreachable!(),
        };
        conn.write_all(b"x").await.unwrap();
        let mut buf = [0u8; 1];
        server_stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf[0], b'x');

        server.abort();
    }

    /// The rewritten bind path (`TcpSocket` + `SO_REUSEADDR`, PS-23) must
    /// still bind and report a concrete local address. The rebind-over-
    /// `TIME_WAIT` effect of `SO_REUSEADDR` itself is OS/timing-dependent
    /// and not deterministically unit-testable; the accept test below
    /// exercises the full `run_tcp` path end-to-end.
    #[tokio::test]
    async fn tcp_listen_binds_via_reuseaddr_path() {
        let bind = "127.0.0.1:0".parse::<SocketAddr>().unwrap();
        let listener = tcp_listen(bind).unwrap();
        let actual = listener.local_addr().unwrap();
        assert_eq!(actual.ip(), bind.ip());
        assert_ne!(actual.port(), 0);
    }

    #[tokio::test]
    async fn unix_filesystem_listener_forwards_accepted_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ioc.sock");
        let ep = UnixEndpoint::filesystem(path.clone());

        let (tx, mut rx) = mpsc::channel(4);
        let std_l = bind_unix(&ep).unwrap().into_std().unwrap();
        let server = tokio::spawn(async move { run_unix_accept(std_l, ep, false, tx).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut conn = tokio::net::UnixStream::connect(&path).await.unwrap();
        let inc = rx.recv().await.expect("got incoming");
        assert!(matches!(inc.stream, ClientStream::Unix(_)));
        assert!(matches!(inc.peer, ClientPeer::Unix(Some(_))));
        assert!(!inc.readonly);

        let mut server_stream = match inc.stream {
            ClientStream::Unix(s) => s,
            _ => unreachable!(),
        };
        conn.write_all(b"y").await.unwrap();
        let mut buf = [0u8; 1];
        server_stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf[0], b'y');

        server.abort();
    }

    /// PS-36: a filesystem socket node must be unlinked when the listener
    /// task ends (clean shutdown leaves nothing behind), mirroring C's
    /// `~acceptItemUNIX` destructor. Aborting the task drops its future and
    /// the in-scope `UnlinkOnDrop` guard with it.
    #[tokio::test]
    async fn unix_socket_file_unlinked_when_listener_task_ends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ioc.sock");
        let ep = UnixEndpoint::filesystem(path.clone());

        let (tx, _rx) = mpsc::channel(4);
        let std_l = bind_unix(&ep).unwrap().into_std().unwrap();
        let server = tokio::spawn(async move { run_unix_accept(std_l, ep, false, tx).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(path.exists(), "socket file should exist while listening");

        // Abort the listener task; awaiting the handle lets the runtime drop
        // the future (and the unlink guard) before we re-check the path.
        server.abort();
        let _ = server.await;
        assert!(
            !path.exists(),
            "socket file should be unlinked after the listener task ends"
        );
    }

    /// PS-24: C `chmod`s a filesystem socket to its `perms` after bind
    /// (default `0o666`, "equivalent to tcp bind to localhost",
    /// acceptFactory.cc:233,368-377). The default and an explicit octal
    /// must both land on the socket node.
    #[tokio::test]
    async fn unix_socket_mode_is_applied_after_bind() {
        use std::os::unix::fs::PermissionsExt;

        async fn bound_mode(perms: u32) -> u32 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("ioc.sock");
            let mut ep = UnixEndpoint::filesystem(path.clone());
            ep.perms = perms;
            let (tx, _rx) = mpsc::channel(4);
            let std_l = bind_unix(&ep).unwrap().into_std().unwrap();
            let server = tokio::spawn(async move { run_unix_accept(std_l, ep, false, tx).await });
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            server.abort();
            mode
        }

        // Default 0o666 (UnixEndpoint::filesystem) and an explicit 0o660 —
        // both must equal the requested mode, independent of the umask.
        assert_eq!(bound_mode(0o666).await, 0o666);
        assert_eq!(bound_mode(0o660).await, 0o660);
    }

    /// PS-49: a bind failure must surface as an error from the (pre-fork)
    /// bind step, not be swallowed. Occupy a port, then a second bind to it
    /// (with `SO_REUSEADDR`, which does NOT permit stealing an active
    /// listener) must return `Err` so the caller can fail-fast.
    #[tokio::test]
    async fn bind_one_fails_fast_on_address_in_use() {
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = occupied.local_addr().unwrap();
        let res = bind_one(Endpoint::Tcp(addr), false, "control");
        assert!(
            res.is_err(),
            "a second bind to an occupied port must fail-fast, not be swallowed"
        );
    }

    /// The happy path of the same bind step: a free port binds, and the
    /// resulting `PreboundListener` accepts a real connection (proving the
    /// bound `std` listener re-adopts cleanly via `from_std`).
    #[tokio::test]
    async fn bind_one_binds_a_free_port_and_accepts() {
        let bind = "127.0.0.1:0".parse::<SocketAddr>().unwrap();
        let pl = bind_one(Endpoint::Tcp(bind), false, "control").expect("free port binds");
        let actual = pl.local_addr().expect("tcp listener reports a local addr");
        let (tx, mut rx) = mpsc::channel(4);
        let server = tokio::spawn(pl.accept(tx));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let _conn = TcpStream::connect(actual).await.unwrap();
        let inc = rx.recv().await.expect("got incoming");
        assert!(matches!(inc.stream, ClientStream::Tcp(_)));
        assert!(!inc.readonly);

        server.abort();
    }

    /// `local_addr` is how a caller that bound with port `0` learns the
    /// real port with no gap between bind and use (unlike bind-query-drop-
    /// then-reuse-the-number). A TCP listener bound to `:0` must report a
    /// concrete, non-zero port; a UNIX listener has no numeric port at all.
    #[tokio::test]
    async fn local_addr_reports_bound_tcp_port_and_none_for_unix() {
        let bind = "127.0.0.1:0".parse::<SocketAddr>().unwrap();
        let tcp = bind_one(Endpoint::Tcp(bind), false, "control").expect("free port binds");
        let addr = tcp.local_addr().expect("tcp listener reports a local addr");
        assert_eq!(addr.ip(), bind.ip());
        assert_ne!(addr.port(), 0);

        let dir = tempfile::tempdir().unwrap();
        let ep = UnixEndpoint::filesystem(dir.path().join("ioc.sock"));
        let unix = bind_one(Endpoint::Unix(ep), false, "control").expect("unix path binds");
        assert_eq!(unix.local_addr(), None);
    }
}
