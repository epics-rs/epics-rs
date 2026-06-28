//! Connection acceptors.
//!
//! Spawns one task per configured listener (TCP + UNIX) that accepts
//! incoming connections and hands each fresh socket off to the
//! supervisor as a new [`super::client::IncomingClient`]. Mirrors
//! C `acceptFactory.cc` (`acceptItemTCP` / `acceptItemUNIX`).

use std::net::SocketAddr;

use tokio::net::{TcpListener, TcpSocket, UnixListener};
use tokio::sync::mpsc;

use crate::procserv::client::{ClientPeer, ClientStream, IncomingClient};
use crate::procserv::endpoint::UnixEndpoint;
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

/// Run the TCP listener loop until the supervisor's `out` channel
/// closes. Each accepted socket is wrapped in [`IncomingClient`] and
/// forwarded.
///
/// `readonly`: when true, every accepted client is read-only —
/// matches C procServ's `--readonly` deployment for sites that want
/// only log-style viewers (separate listening port for observers
/// vs operators).
pub async fn run_tcp(
    bind: SocketAddr,
    readonly: bool,
    out: mpsc::Sender<IncomingClient>,
) -> ProcServResult<()> {
    let listener = tcp_listen(bind)?;
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

/// UNIX-socket listener. Binds a filesystem or abstract socket per the
/// parsed [`UnixEndpoint`], then accepts in a loop (C `acceptItemUNIX`,
/// `acceptFactory.cc:229-381`).
///
/// For a filesystem socket, any existing socket file is `unlink`ed up
/// front for the same reason C does it — a stale node blocks `bind`. An
/// abstract socket (Linux-only) has no filesystem presence, so neither
/// the unlink nor the permission step applies.
pub async fn run_unix(
    ep: UnixEndpoint,
    readonly: bool,
    out: mpsc::Sender<IncomingClient>,
) -> ProcServResult<()> {
    let listener = bind_unix(&ep)?;
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
        // Pick an OS-assigned port.
        let bind = "127.0.0.1:0".parse::<SocketAddr>().unwrap();
        // Bind first to learn the port, then re-bind via run_tcp on
        // that exact port. Simpler: just bind once and use the port.
        let listener = tokio::net::TcpListener::bind(bind).await.unwrap();
        let actual = listener.local_addr().unwrap();
        drop(listener);

        let (tx, mut rx) = mpsc::channel(4);
        let server = tokio::spawn(async move { run_tcp(actual, false, tx).await });

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
        let server = tokio::spawn(async move { run_unix(ep, false, tx).await });
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
            let server = tokio::spawn(async move { run_unix(ep, false, tx).await });
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
}
