//! Connection acceptors.
//!
//! Spawns one task per configured listener (TCP + UNIX) that accepts
//! incoming connections and hands each fresh socket off to the
//! supervisor as a new [`super::client::IncomingClient`]. Mirrors
//! C `acceptFactory.cc` (`acceptItemTCP` / `acceptItemUNIX`).

use std::net::SocketAddr;

use tokio::net::{TcpListener, UnixListener};
use tokio::sync::mpsc;

use crate::procserv::client::{ClientPeer, ClientStream, IncomingClient};
use crate::procserv::endpoint::UnixEndpoint;
use crate::procserv::error::{ProcServError, ProcServResult};

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
    let listener = TcpListener::bind(bind)
        .await
        .map_err(|e| ProcServError::ListenerBind(format!("TCP {bind}: {e}")))?;
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
                tracing::warn!(error = %e, "procserv-rs: TCP accept error");
            }
        }
    }
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
                tracing::warn!(error = %e, "procserv-rs: UNIX accept error");
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
        UnixListener::bind(&ep.name)
            .map_err(|e| ProcServError::ListenerBind(format!("UNIX {}: {e}", ep.name.display())))
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
}
