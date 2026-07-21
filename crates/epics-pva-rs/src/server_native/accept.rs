//! TCP accept loop — the host socket driver in front of the connection
//! handler.
//!
//! This module is the **entire** socket surface of the native PVA server's
//! TCP side: `tokio::net::TcpListener`, the `socket2` keepalive options, the
//! TLS-vs-plaintext first-byte dispatch and its two deadlines, and the
//! accept-error backoff all live here and nowhere else. Everything past the
//! accept — `handle_connection_io` in [`super::tcp`] and the ~19,000 lines of
//! protocol behind it — speaks only `AsyncRead`/`AsyncWrite` trait objects
//! and never names a socket type.
//!
//! The split is drawn here because a second, blocking driver is coming (RTEMS
//! phase 6 item 7, `doc/pva-rtems-item7-design.md` §6 stage A): a
//! thread-per-client accept loop over `std::net::TcpListener` that hands
//! `handle_connection_io` the same two boxes this one does. Keeping the
//! sockets in one small module means that driver is an addition beside this
//! file rather than a set of `cfg`s threaded through the protocol code.
//!
//! Moved verbatim from `tcp.rs:2428-2727`; no behaviour changed.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use epics_base_rs::runtime::accept::AcceptBackoff;
use tokio::net::TcpListener;
use tracing::{debug, error, warn};

use crate::error::{PvaError, PvaResult};

use super::runtime::PvaServerConfig;
use super::source::{ChannelInvalidator, DynSource};
use super::tcp::{ConnInit, handle_connection_io};

/// Run the TCP listener forever. Backwards-compat wrapper that
/// drops per-peer stats — equivalent to calling
/// [`run_tcp_server_with_peers`] with an empty registry the caller
/// can never read.
pub async fn run_tcp_server(
    source: DynSource,
    bind_addr: SocketAddr,
    config: PvaServerConfig,
) -> PvaResult<()> {
    run_tcp_server_with_peers(
        source,
        bind_addr,
        config,
        crate::server_native::peers::PeerRegistry::new(),
    )
    .await
}

/// Run the TCP listener with an externally-shared
/// [`PeerRegistry`](crate::server_native::PeerRegistry). lets [`crate::server_native::PvaServer::report`]
/// observe per-connection stats.
pub async fn run_tcp_server_with_peers(
    source: DynSource,
    bind_addr: SocketAddr,
    config: PvaServerConfig,
    peers: Arc<crate::server_native::peers::PeerRegistry>,
) -> PvaResult<()> {
    let listener = TcpListener::bind(bind_addr).await.map_err(PvaError::Io)?;
    // Standalone single-listener path (not driven by `PvaServer`): create
    // and wire the channel invalidator here so a source served this way —
    // e.g. a PVA gateway — still force-disconnects downstream channels on an
    // operator `:drop`/`:flush`. `PvaServer`'s multi-listener path creates it
    // once in `run_pva_server` and shares the one handle across every TCP/UDP
    // listener; here there is a single listener, so a local handle suffices.
    let channel_invalidator = ChannelInvalidator::new();
    source.set_channel_invalidator(channel_invalidator.clone());
    run_tcp_server_on_listener(source, listener, config, peers, channel_invalidator).await
}

/// Variant that takes a pre-bound [`TcpListener`]. Lets
/// [`crate::server_native::PvaServer::start`] perform the bind
/// synchronously (so the bound port is observable to callers) and
/// then hand the listener to the spawned accept task. Eliminates
/// the bind-race window that existed when the spawn-and-bind happened
/// inside the spawned task — concurrent isolated tests can no longer
/// have their picked-then-dropped ephemeral ports stolen by a peer.
pub async fn run_tcp_server_on_listener(
    source: DynSource,
    listener: TcpListener,
    config: PvaServerConfig,
    peers: Arc<crate::server_native::peers::PeerRegistry>,
    // Server-wide channel invalidator. Each accepted connection holds a
    // receiver; a source publishes the PV name(s) of any channel that must
    // be force-disconnected out of band (PVA gateway operator `:drop`/`:flush`).
    // See [`ChannelSource::set_channel_invalidator`].
    channel_invalidator: ChannelInvalidator,
) -> PvaResult<()> {
    let bind_addr = listener.local_addr().map_err(PvaError::Io)?;
    debug!(?bind_addr, "TCP listener up");
    let active = Arc::new(AtomicUsize::new(0));

    #[cfg(feature = "tls")]
    let tls_acceptor = config
        .tls
        .as_ref()
        .map(|cfg| tokio_rustls::TlsAcceptor::from(cfg.config.clone()));
    // Without the `tls` feature there is no rustls type to build an acceptor
    // from — and none is needed: `TlsServerConfig` is uninhabited, so
    // `config.tls` is provably `None` and the config handle itself stands in
    // as the acceptor slot. Same `Option` shape, so everything downstream
    // (`acceptor.as_ref()`, `.is_some()`, the peek, the match) is untouched.
    #[cfg(not(feature = "tls"))]
    let tls_acceptor = config.tls.clone();

    // track per-connection tasks in a JoinSet so they're
    // aborted as a unit when this accept-loop future is dropped (e.g.
    // PvaServer::stop() → tcp_handle.abort()). Without this, every
    // per-conn task ran detached and lingered until its internal
    // idle_timeout (~45s). The select! arm on `conn_tasks.join_next()`
    // also reaps completed tasks so the set doesn't accumulate
    // finished JoinHandles.
    let mut conn_tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

    let mut backoff = AcceptBackoff::new();
    loop {
        let accept_result = tokio::select! {
            biased;
            res = listener.accept() => res,
            // Drain finished connection tasks. Returns None when the
            // set is empty — that branch resolves immediately, but
            // `biased` makes the listener arm preferred so we never
            // starve incoming accepts.
            Some(_) = conn_tasks.join_next() => continue,
        };
        match accept_result {
            Ok((stream, peer)) => {
                backoff.accepted();
                // pvxs scopes `ignoreAddrs` to the UDP SEARCH admission
                // path (`Server::Pvt::onSearch`, server.cpp:654-670); the
                // TCP accept callback registers a `ServerConn` with no
                // ignore-list check (serverconn.cpp:461-467). Applying it
                // to TCP accepts here turned a discovery filter into a
                // transport ACL, blocking direct clients that reach the
                // endpoint via a name server / cached beacon / static
                // address. The UDP path keeps the filter (`filter_inbound`).
                let cur = active.fetch_add(1, Ordering::SeqCst);
                if cur >= config.max_connections {
                    active.fetch_sub(1, Ordering::SeqCst);
                    warn!(
                        ?peer,
                        "rejecting connection: max_connections={}", config.max_connections
                    );
                    drop(stream);
                    continue;
                }
                let src = source.clone();
                let cfg = config.clone();
                let active_dec = active.clone();
                let acceptor = tls_acceptor.clone();
                let peers_for_task = peers.clone();
                let conn_invalidator = channel_invalidator.clone();
                conn_tasks.spawn(async move {
                    stream.set_nodelay(true).ok();
                    // Enable OS-level TCP keepalive so half-open connections
                    // (NAT timeout, dead client) are detected within ~30s
                    // even when the protocol-level Echo path can't fire
                    // (e.g. peer hasn't initialized control plane yet).
                    // Defence-in-depth on top of the heartbeat ECHO timer:
                    // pvxs itself does NOT set SO_KEEPALIVE — it relies on
                    // libevent's `bufferevent_set_timeouts` for inactivity
                    // detection. We add OS keepalive (CA-libca style) so a
                    // pre-handshake half-open peer still gets reaped even
                    // before the application timer arms.
                    {
                        let sock = socket2::SockRef::from(&stream);
                        let keepalive = socket2::TcpKeepalive::new()
                            .with_time(std::time::Duration::from_secs(15))
                            .with_interval(std::time::Duration::from_secs(5));
                        let _ = sock.set_keepalive(true);
                        let _ = sock.set_tcp_keepalive(&keepalive);
                    }

                    // TLS-NAMESERVER: peek the first byte to dispatch
                    // TLS vs plain PVA on a single port.
                    //
                    // TLS ClientHello record type = 0x16 — the TLS
                    // client sends this IMMEDIATELY after TCP connect
                    // (client-initiates). Plain PVA clients NEVER send
                    // a first byte; the server sends SET_BYTE_ORDER first.
                    //
                    // Dispatch rule (pvxs uses separate listeners per
                    // protocol via serverconn.h:193 `isTLS`; we unify):
                    //   peek Ok(1) && byte == 0x16 → TLS path
                    //   peek timeout (≤ 100 ms)    → plain PVA path
                    //   peek Ok(1) && byte != 0x16  → plain PVA path
                    //   peek Ok(0) / IO error       → drop (peer gone)
                    //
                    // 100 ms is enough for ClientHello to arrive (sent
                    // immediately by TLS stack) while adding negligible
                    // latency to plain PVA connections.
                    const PEEK_WINDOW: Duration = Duration::from_millis(100);
                    let is_tls_client = match acceptor.as_ref() {
                        None => false,
                        Some(_) => {
                            let mut b = [0u8; 1];
                            match tokio::time::timeout(PEEK_WINDOW, stream.peek(&mut b)).await {
                                Ok(Ok(1)) => b[0] == 0x16,
                                Ok(Ok(_)) => {
                                    debug!(?peer, "peer closed before first byte");
                                    active_dec.fetch_sub(1, Ordering::SeqCst);
                                    return;
                                }
                                Ok(Err(e)) => {
                                    debug!(?peer, "first-byte peek error: {e}");
                                    active_dec.fetch_sub(1, Ordering::SeqCst);
                                    return;
                                }
                                // Timeout → plain PVA client (server initiates).
                                Err(_) => false,
                            }
                        }
                    };

                    // Anti-downgrade: when the operator requires TLS
                    // (`disable_plaintext`), refuse a non-TLS peer before it
                    // can reach the plain code path. pvxs enforces the
                    // refusal on the CLIENT (it drops a plaintext SEARCH
                    // reply, client.cpp:944); the Rust server unifies TLS +
                    // plaintext on each listener via the peek above, so the
                    // equivalent server-side guarantee lives here. Gated on
                    // `acceptor.is_some()` so a misconfigured server with no
                    // TLS identity does not refuse every connection (a server
                    // cannot be TLS-only without TLS). A TLS ClientHello
                    // (`is_tls_client`) is served normally below.
                    if cfg.disable_plaintext && acceptor.is_some() && !is_tls_client {
                        debug!(
                            ?peer,
                            "refusing plaintext connection: disable_plaintext set (TLS required)"
                        );
                        active_dec.fetch_sub(1, Ordering::SeqCst);
                        return;
                    }

                    // register this connection in the peer registry
                    // so PvaServer::report() can surface it. Deferred to
                    // here (post-peek) so the `tls` flag reflects the
                    // actual protocol, not the server config.
                    let peer_entry = crate::server_native::peers::PeerEntry::new(is_tls_client);
                    peers_for_task.insert(peer, peer_entry.clone());

                    let result = match (acceptor, is_tls_client) {
                        // cap the TLS handshake — a peer
                        // that completes TCP but stalls during ClientHello
                        // would otherwise hold a `max_connections` slot
                        // until OS keepalive reaps it (~30s).
                        //
                        // The only arm that names a rustls type. Without the
                        // `tls` feature the acceptor is `Option<Infallible>`
                        // and the `_` arm below is already exhaustive.
                        #[cfg(feature = "tls")]
                        (Some(a), true) => {
                            match tokio::time::timeout(cfg.tls_handshake_timeout, a.accept(stream))
                                .await
                            {
                                Ok(Ok(tls_stream)) => {
                                    // derive the peer's x509 identity from
                                    // the *verified* certificate chain before
                                    // splitting the stream. rustls only
                                    // exposes `peer_certificates()` on the
                                    // whole `TlsStream`, and the chain has
                                    // already passed `WebPkiClientVerifier`,
                                    // so this is the cryptographically-checked
                                    // identity (pvxs `fill_credentials`).
                                    //
                                    // use trust_roots so that `authority`
                                    // is populated even when the peer sends a
                                    // partial chain (leaf-only or leaf+CA),
                                    // matching pvxs SSL_get0_verified_chain.
                                    let x509_id = {
                                        let (_, conn) = tls_stream.get_ref();
                                        let roots =
                                            cfg.tls.as_ref().map(|t| t.trust_roots.as_ref());
                                        conn.peer_certificates().and_then(|chain| match roots {
                                            Some(r) => {
                                                crate::auth::x509_credentials_from_chain_with_roots(
                                                    chain, r,
                                                )
                                            }
                                            None => crate::auth::x509_credentials_from_chain(chain),
                                        })
                                    };
                                    let (r, w) = tokio::io::split(tls_stream);
                                    handle_connection_io(
                                        src,
                                        Box::new(r),
                                        Box::new(w),
                                        peer,
                                        cfg,
                                        ConnInit {
                                            peer_entry: peer_entry.clone(),
                                            x509_identity: x509_id,
                                            channel_invalidator: conn_invalidator,
                                        },
                                    )
                                    .await
                                }
                                Ok(Err(e)) => {
                                    debug!(?peer, "TLS handshake failed: {e}");
                                    Err(PvaError::Io(e))
                                }
                                Err(_) => {
                                    debug!(
                                        ?peer,
                                        timeout = ?cfg.tls_handshake_timeout,
                                        "TLS handshake timed out"
                                    );
                                    Err(PvaError::Protocol("TLS handshake timeout".into()))
                                }
                            }
                        }
                        _ => {
                            // Plain PVA: no TLS configured, or client sent
                            // non-TLS bytes (name-server, plain pvxs peer).
                            let (r, w) = stream.into_split();
                            handle_connection_io(
                                src,
                                Box::new(r),
                                Box::new(w),
                                peer,
                                cfg,
                                ConnInit {
                                    peer_entry: peer_entry.clone(),
                                    x509_identity: None,
                                    channel_invalidator: conn_invalidator,
                                },
                            )
                            .await
                        }
                    };
                    if let Err(e) = result {
                        debug!(?peer, "connection ended: {e}");
                    }
                    active_dec.fetch_sub(1, Ordering::SeqCst);
                    // drop the per-peer entry whether the
                    // connection ended cleanly or via I/O error.
                    peers_for_task.remove(peer);
                });
            }
            Err(e) => {
                error!("accept error: {e}");
                // Was a flat 50 ms with no way out: a listener that could never
                // accept again spun at 20 Hz for the life of the process. Same
                // primitive as the two blocking loops now — see
                // `runtime::accept` for why the decision cannot come from `e`.
                tokio::time::sleep(backoff.failed()).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// Production scope of a source file: everything before the first
    /// column-0 `#[cfg(test)]`.
    fn production_scope(src: &str) -> &str {
        match src.find("\n#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        }
    }

    /// Stage A's invariant, stated as source inspection: the TCP protocol
    /// module names no socket type. That is what makes a second, blocking
    /// driver an addition beside this file rather than a `cfg` threaded
    /// through 21,000 lines of protocol — and it is worth pinning because
    /// nothing else stops a `tokio::net::TcpStream` from drifting back in
    /// one convenience at a time.
    #[test]
    fn the_protocol_scope_owns_no_socket() {
        let prod = production_scope(include_str!("tcp.rs"));
        // Fail closed: if the connection handler is no longer in the slice,
        // the slice is wrong and the assertion below would pass vacuously.
        assert!(
            prod.contains("async fn handle_connection_io"),
            "production slice no longer covers the connection handler"
        );
        for token in ["tokio::net", "socket2", "tokio_rustls", "TcpListener"] {
            assert_eq!(
                prod.matches(token).count(),
                0,
                "`tcp.rs` production scope must name no socket type; found `{token}`. \
                 Socket-bearing code belongs in `accept.rs`."
            );
        }
    }

    /// The companion to `tcp::tests::connection_scope_spawns_go_through_the_
    /// runtime_seam`: moving the accept loop out of `tcp.rs` must not create a
    /// place where a bare `tokio::spawn` is unexamined. The rule is the same
    /// here — the one task this module starts per connection is a `JoinSet`
    /// method (`conn_tasks.spawn`), not this literal, and item 7 replaces it
    /// with a thread rather than a seam spawn.
    ///
    /// What this module IS allowed, and `tcp.rs` is not, is the non-spawn
    /// socket surface a host driver needs; that is asserted positively above
    /// by its absence from `tcp.rs`, not by a ceiling here.
    #[test]
    fn the_accept_driver_spawns_no_bare_task_either() {
        let prod = production_scope(include_str!("accept.rs"));
        assert!(
            prod.contains("pub async fn run_tcp_server_on_listener"),
            "production slice no longer covers the accept loop"
        );
        // Written split so this assertion cannot match its own source text.
        let literal = concat!("tokio", "::spawn(");
        let hits = prod.matches(literal).count();
        assert_eq!(
            hits, 0,
            "the accept driver must not spawn bare tasks; found {hits} `{literal}`"
        );
    }
}
