# 12 — Auth + TLS

PVA has a two-stage authentication model:

1. **CONNECTION_VALIDATION** — over the (possibly TLS-protected) TCP
   handshake, the client advertises the auth method it will use and
   any authentication payload (e.g. `user`, `host`, `groups` for
   the `ca` method).
2. **TLS** (optional) — when the connection itself is wrapped in
   TLS, the server may *additionally* require a verified client
   certificate (mTLS) and use the certificate's identity for
   authorization.

`pva-rs` supports both stages.

## Modules

| Module | Role |
|--------|------|
| `auth/mod.rs` | `ClientCredentials` types; `posix_groups()` for `ca` auth |
| `auth/tls.rs` | `TlsClientConfig` / `TlsServerConfig`, env-driven loaders |
| `client_native/conn.rs` (legacy, deleted) — was the dev-time TLS test harness | removed in v0.10.5 |
| `client_native/server_conn.rs::connect_tls` | client-side `pvas://` TCP wrapper |
| `server_native/runtime.rs::PvaServerConfig::tls` | server-side TLS config slot |

## CONNECTION_VALIDATION

See [`02-wire-protocol.md`](02-wire-protocol.md) for the byte layout.
The server offers a list of methods (`["ca", "anonymous"]` by
default); the client picks one and emits a method-specific payload.

For `"ca"`, the payload is a Variant of:

```text
struct {
    string user
    string host
    string[] groups   (optional; this client advertises its own POSIX
                       groups via getgrouplist(3), but the SERVER ignores
                       the field — see below. pvxs sends no groups field.)
}
```

`server_native/tcp.rs::parse_client_credentials` extracts `user` and
`host` only. It deliberately does **not** read the wire `groups`/`roles`
field: trusting client-advertised roles would let any client self-assign
group membership to satisfy an ACF `member group:X` rule (ACL bypass).
Instead the server re-derives the account's roles itself from the local
passwd/group database (`auth::osd_get_roles`, mirroring pvxs
`ClientCredentials::roles()` → `osdGetRoles`). The credential is passed to
the `PvaServerConfig::auth_complete` hook, where ACF / ACL belongs.
The protocol layer always `Status::ok()`s.

For `"anonymous"`, no payload follows the method string. The server
still gets the connection's peer-addr for logging.

For `"x509"` (when mTLS is in effect), the payload is empty: the
server-side identity comes from the verified client cert, surfaced
on the TLS layer (`auth/tls.rs::identity_from_cert`).

## TLS (transport layer)

| Direction | Knob |
|-----------|------|
| Client opt-in | `PvaClientBuilder::with_tls(Arc<TlsClientConfig>)` or `EPICS_PVA_TLS_CERT/KEY/CA` env vars |
| Server opt-in | `PvaServerConfig::tls = Some(Arc<TlsServerConfig>)` or `EPICS_PVAS_TLS_CERT/KEY` |
| mTLS policy | `EPICS_PVAS_TLS_CLIENT_CERT = none / optional / require` |
| Disable | `EPICS_PVA_TLS_DISABLE=YES` (client-side override) |

When the server is configured with TLS, beacons advertise `proto =
"tls"` and search responses set the same; clients that built without
TLS skip these on search.

### Loading

`auth/tls.rs::client_from_env()` and `server_from_env()` read the
env vars, load the cert + key files (PEM), build a `rustls::*Config`,
and return an `Arc`-wrapped handle. Failures are typed errors (not
panics); CLIs surface them with `--verbose` traces.

### Capability tokens (server-side, behind feature flag)

When the `cap-tokens` feature is enabled on the downstream side, the
client's `CLIENT_NAME` payload may begin with `cap:<token>`. The
server's installed `TokenVerifier` validates the token and the
resolved subject becomes the ACF-matched username. Unverifiable
tokens are logged and replaced with an `unverified:` sentinel that
ACF rules can deliberately deny. Plain (non-`cap:`) usernames pass
through unchanged.

## Threading

TLS handshake is synchronous-async (`tokio_rustls::TlsConnector` /
`TlsAcceptor`). Each accept spawns a task that runs the handshake
before delegating to `handle_client`. Existing clients continue to
serve traffic while new TLS handshakes are in flight on other
threads — the TLS handshake is per-accept, not per-server.

## Mixed environments

`pva-rs` server can serve plaintext and TLS clients on different
ports — bind two `PvaServer`s with the same `Arc<source>` and
different `PvaServerConfig`s. The shared source means both hand out
the same data; the config carries the bind / TLS independently.

## Reload

`PvaServer::reload_tls_from_env()` (when wired up via
`PvaServerConfig::tls_paths`) re-reads cert + key from disk and
swaps the active `Arc<rustls::ServerConfig>` in place. Existing
connections continue with their pinned config; only new accepts see
the swap. Mirrors pvxs `Server::reloadTLS` at the swap-while-running
granularity.
