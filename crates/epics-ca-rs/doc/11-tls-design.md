# 11 — CA over TLS (experimental, Rust-only)

> ⚠️ **EXPERIMENTAL — RUST-ONLY EXTENSION**
>
> CA-over-TLS in `epics-ca-rs` is **not part of the EPICS Channel
> Access specification**. Enabling it makes your IOC unreachable from
> all standard EPICS tooling — `caget`, `caput`, `camonitor`, `cainfo`,
> EDM, MEDM, CSS/Phoebus, pyepics, p4p (any pyepics-based program) —
> because libca and rsrv have no TLS support.
>
> **For production traffic encryption, prefer network-layer solutions:
> IPSec, WireGuard, or your site VPN.** They protect every EPICS
> message (TCP virtual circuits, UDP search, beacons) transparently
> and don't break interop. CA-over-TLS only secures TCP and only
> between two epics-ca-rs peers.
>
> This feature exists for two narrow use cases:
>
> 1. Closed Rust-only deployments where the operator controls both
>    ends of every CA conversation.
> 2. As a foundation for **future epics-base 7 `ca-secure` wire
>    compatibility** — that work has not yet been done; the wire
>    format here is currently Rust-only.

## Background

EPICS CA was designed in the late 1980s for trusted LANs. PV names
and values traverse the wire in plaintext; ACF rules trust the
client-supplied hostname/username with no cryptographic verification.
For modern environments — remote operations, cloud bridges, multi-site
collaborations, and sites under medical or nuclear compliance regimes
— that's increasingly untenable.

`epics-ca-rs` adds **CA over TLS** as an opt-in extension: TCP virtual
circuits are wrapped in TLS before the CA handshake; UDP search and
beacons remain plaintext (PV names are not secret, broadcast doesn't
play nicely with TLS, and treating them as public is a deliberate
design choice).

This document describes the design. The runtime API is in
[`crate::tls`](../src/tls/mod.rs) and is gated behind the
`experimental-rust-tls` cargo feature.

## Configuration

### Cargo feature

```toml
# Cargo.toml
[dependencies]
epics-ca-rs = { version = "0.9", features = ["experimental-rust-tls"] }
```

The feature name `experimental-rust-tls` is deliberately verbose to
prevent operators from enabling it under the impression that it's a
standard EPICS option.

### Server: environment variables (operational)

```bash
EPICS_CAS_TLS_CERT_FILE=/etc/epics/server.crt        # required
EPICS_CAS_TLS_KEY_FILE=/etc/epics/server.key         # required
EPICS_CAS_TLS_CLIENT_CA_FILE=/etc/epics/clients.pem  # optional → mTLS
softioc-rs --pv MOTOR:X:VAL:double:0.0
```

Both `CERT_FILE` and `KEY_FILE` must be set (or both unset). Setting
only one is a startup error. Adding `CLIENT_CA_FILE` enables mTLS:
the server requires every client to present a certificate signed by
that CA bundle, and the cert's identity (SAN dNSName / SAN URI / CN
/ SHA-256 fingerprint, in that order) becomes the ACF hostname.

### Server: CLI flags (override env)

```bash
softioc-rs \
    --pv MOTOR:X:VAL:double:0.0 \
    --tls-cert /etc/epics/server.crt \
    --tls-key  /etc/epics/server.key \
    --tls-client-ca /etc/epics/clients.pem    # optional, enables mTLS
```

CLI flags take precedence over env vars. Without the
`experimental-rust-tls` feature, passing any `--tls-*` flag is an
error.

### Server: programmatic API

```rust
use epics_ca_rs::server::CaServer;
use epics_ca_rs::tls::{TlsConfig, load_certs, load_private_key};

let cert = load_certs("server.crt")?;
let key  = load_private_key("server.key")?;
let tls  = TlsConfig::server_from_pem(cert, key)?;

let server = CaServer::builder()
    .pv("MOTOR:X:VAL", value)
    .with_tls(tls)        // overrides env
    .build().await?;

server.run().await?;
```

When the server starts with TLS active it logs a multi-line warning
to draw attention to the non-standard nature.

### Client: environment variables

```bash
EPICS_CA_TLS_ROOTS_FILE=/etc/epics/server-ca.pem    # required (server CA bundle)
EPICS_CA_TLS_CLIENT_CERT=/etc/epics/me.crt          # optional → mTLS
EPICS_CA_TLS_CLIENT_KEY=/etc/epics/me.key           # required when CERT is set
caget-rs MOTOR:X:VAL
```

`CaClient::new()` reads these automatically. `CaClient::new_with_config(...)`
takes precedence (the explicit code path).

### Client: programmatic API

```rust
use epics_ca_rs::client::{CaClient, CaClientConfig};
use epics_ca_rs::tls::{TlsConfig, load_root_store};

let roots = load_root_store("server-ca.pem")?;
let tls   = TlsConfig::client_from_roots(roots);

let client = CaClient::new_with_config(CaClientConfig {
    tls: Some(tls),
}).await?;
```

### Disabling

To disable TLS again:

- **Build-time**: drop the `experimental-rust-tls` feature.
- **Runtime (server)**: unset all `EPICS_CAS_TLS_*` env vars and don't
  pass `--tls-*` flags. The server falls back to plaintext.
- **Runtime (client)**: unset `EPICS_CA_TLS_ROOTS_FILE`. Without it,
  the client doesn't attempt TLS regardless of what the server offers.

## Goals

1. **Encrypt** TCP-borne CA traffic so neither values nor write
   commands are observable on the wire.
2. **Authenticate** clients via certificates (mTLS) so ACF rules can
   key on a verifiable identity instead of the spoofable
   `CA_PROTO_HOST_NAME` message.
3. **Coexist** with plaintext peers — a TLS-enabled IOC must still
   answer non-TLS clients (and vice versa) on a separate port.
4. **No protocol changes** — once the TLS handshake completes, the
   CA wire format is identical. libca-aware tools that link our TLS
   stack should work unchanged.

## Goals

1. **Encrypt** TCP-borne CA traffic so neither values nor write
   commands are observable on the wire.
2. **Authenticate** clients via certificates (mTLS) so ACF rules can
   key on a verifiable identity instead of the spoofable
   `CA_PROTO_HOST_NAME` message.
3. **Coexist** with plaintext peers — a TLS-enabled IOC must still
   answer non-TLS clients (and vice versa) on a separate port.
4. **No protocol changes** — once the TLS handshake completes, the
   CA wire format is identical. libca-aware tools that link our TLS
   stack should work unchanged.

## Non-goals

- Encrypting UDP (search, beacons). PV names are not secret; covering
  them with DTLS adds complexity without clear value. Beacons over
  multicast/broadcast plus TLS is impractical.
- Replacing `CA_PROTO_HOST_NAME`. We retain it for legacy clients.
  When mTLS is in effect, the cert-derived identity takes precedence
  for ACF matching.

## Wire-level model

```
┌────────────┐                ┌──────────────────┐
│   Client   │                │      Server      │
└─────┬──────┘                └────────┬─────────┘
      │                                │
      │ TCP connect to TLS port (e.g. 5076) │
      ├───────────────────────────────▶│
      │                                │
      │ TLS handshake (rustls)         │
      ├══════════════════════════════▶│
      │ ◀═════════════════════════════│
      │ session keys established       │
      │                                │
      │ CA_PROTO_VERSION + HOST + ... (encrypted)│
      ├───────────────────────────────▶│
      │ ◀──────────────────────────────│
      │                                │
      │ ... normal CA traffic, all     │
      │     bytes inside TLS record    │
      │ ◀────────────────────────────▶│
```

There is **no** TLS negotiation inside CA — the choice is made by
which port the client connects to.

## Port allocation

| Port | Service |
|------|---------|
| 5064 | CA TCP plaintext (default) |
| 5076 | CA TCP TLS (proposed default; configurable via `EPICS_CA_TLS_PORT`) |

Servers can listen on either or both. Search responses include the
port the listener is bound to, so a client connecting to a SEARCH
reply pointing at port 5076 must use TLS; a reply pointing at 5064
must use plaintext.

(Open question: should SEARCH replies indicate "TLS available on
port X" out-of-band? Current CA wire format has no flag for this.
Sites running mixed plaintext/TLS would either point each PV to one
or the other, or run two IOC instances.)

## Configuration API

```rust
use epics_ca_rs::tls::{TlsConfig, load_certs, load_private_key, load_root_store};

// Server side: TLS only (server-auth, no client cert required)
let cert_chain = load_certs("server.crt")?;
let key       = load_private_key("server.key")?;
let tls       = TlsConfig::server_from_pem(cert_chain, key)?;

let server = epics_ca_rs::server::CaServer::builder()
    .pv("MOTOR:X:VAL", value)
    .with_tls(tls)
    .build().await?;

// Client side: verify the server cert against a custom CA bundle
let roots = load_root_store("site-ca.pem")?;
let tls   = TlsConfig::client_from_roots(roots);
let client = epics_ca_rs::client::CaClient::new_with_config(
    epics_ca_rs::client::CaClientConfig { tls: Some(tls), ..Default::default() }
).await?;
```

## mTLS (mutual auth)

```rust
// Server: require valid client cert + use cert CN/SAN as ACF identity
let client_ca_roots = load_root_store("operator-ca.pem")?;
let tls = TlsConfig::server_mtls_from_pem(cert_chain, key, client_ca_roots)?;

// Client: present a certificate
let tls = TlsConfig::client_mtls(roots, client_cert, client_key)?;
```

When mTLS is in effect, the server's per-client `state.hostname` is
populated from the client cert's Subject Alternative Name (or CN as
fallback). The identity is *pinned*: neither `asCheckClientIP` mode
applies and `CA_PROTO_HOST_NAME` cannot replace it. ACF rules can
match on this verified identity:

```
HAG(operators) { CN=alice, CN=bob }
ASG(MOTORS) { RULE(1, WRITE) { HAG(operators) } }
```

## Architecture

### Configuration storage

Both client and server hold the TLS config behind the same enum:

```rust
pub enum TlsConfig {
    Server(Arc<rustls::ServerConfig>),
    Client(Arc<rustls::ClientConfig>),
}
```

Server-side state: `CaServer` gains an `Option<Arc<ServerConfig>>`
field set by `with_tls(...)`. Client-side: `CaClient::new_with_config`
takes a `CaClientConfig { tls: Option<TlsConfig>, ... }`.

### Stream type

The transport manager currently operates over `tokio::net::TcpStream`
directly. To carry TLS, the read/write loops switch to a generic
form:

```rust
async fn read_loop<R: AsyncRead + Unpin + Send + 'static>(reader: R, ...) {}
async fn write_loop<W: AsyncWrite + Unpin + Send + 'static>(writer: W, ...) {}
```

`connect_server` then dispatches:

```rust
let tcp = TcpStream::connect(addr).await?;
let stream = match tls_config {
    Some(c) => StreamKind::Tls(TlsConnector::from(c).connect(server_name, tcp).await?),
    None    => StreamKind::Plain(tcp),
};
```

`StreamKind` is a wrapper enum that implements AsyncRead + AsyncWrite
by dispatching to the active variant. Same pattern on the server side
in `handle_client`.

### Identity propagation (mTLS)

After a successful TLS handshake the server side calls
`stream.peer_certificates()` to extract the client cert, parses the
CN/SAN, and stores it in `ClientState::hostname` as
`HostIdentity::Pinned`. A later `CA_PROTO_HOST_NAME` is then ignored by
construction — `HostIdentity::claim` is a no-op on a pinned identity — so
the cert-derived identity always wins, whatever `asCheckClientIP` says.

## Migration path

For an existing C-based facility:

1. **Phase A** — Run `epics-ca-rs` IOCs alongside C IOCs. Both serve
   plaintext on 5064. No security change.
2. **Phase B** — Enable TLS on Rust IOCs (port 5076). Critical
   clients (operator GUIs, trusted services) connect via TLS;
   everyone else continues on 5064.
3. **Phase C** — Issue client certs to operators via the existing
   PKI. Switch to mTLS. ACF rules updated to match cert identities.
4. **Phase D** — When all important clients are TLS-capable, disable
   plaintext listener on selected IOCs.

## Status in this crate

- `crate::tls` module: ✅ complete (helpers, config types, mTLS builders, identity extraction)
- `CaServerBuilder::with_tls()`: ✅ complete — stored and threaded through `run()`
- `CaClient::new_with_config(tls=...)`: ✅ complete
- Client transport stream wrapping: ✅ complete — `connect_server`
  dispatches plaintext or `tokio_rustls::TlsStream`; `read_loop` /
  `write_loop` are generic over `AsyncRead`/`AsyncWrite`
- mTLS identity extraction (`identity_from_cert`): ✅ complete —
  prefers SAN dNSName, then SAN URI, then CN, falls back to SHA-256
  fingerprint
- Server transport stream wrapping: ✅ complete — `handle_client<S>`
  generic over the stream, all helpers + `monitor::spawn_monitor_sender<W>`
  generic over the writer
- mTLS identity → ACF: ✅ complete — `run_tcp_listener` extracts the
  client cert after handshake and passes the derived identity as
  `initial_hostname`, taking precedence over `peer.ip()`
- End-to-end TLS interop: ✅ verified by `tests/tls_end_to_end.rs`
  (Rust client + Rust IOC, self-signed cert, get round-trip)
- Port-based plaintext/TLS coexistence: 🚧 minor — currently a server
  is either plaintext OR TLS. Running both simultaneously requires
  spawning two CaServer instances on different ports (workable today;
  could be unified into a single "dual" listener in a follow-up).
- Wire-level libca interop on TLS: 🚧 — libca/rsrv don't speak our
  TLS variant, so cross-impl TLS is Rust-only for now. Plaintext
  interop is unchanged and fully tested.

The current commit lays the foundation: stable API, cert/key loading
helpers, and a documented design. Wiring the actual TLS handshake
through the read/write loops requires changing the concrete
`OwnedReadHalf` / `OwnedWriteHalf` types to be generic, which is a
contained refactor scheduled for a follow-up patch.

## Threat model

This document does not enumerate every threat — that's site policy —
but the key claims our TLS extension makes:

| Threat | Mitigation |
|--------|------------|
| Eavesdropper on the LAN reads PV values | TLS encryption on the TCP circuit |
| Eavesdropper learns which PVs exist | Not mitigated (UDP search remains plaintext) |
| Attacker writes to a PV by spoofing IP | mTLS: server requires signed client cert |
| Attacker hijacks an established TCP circuit | TLS authenticated encryption (no in-band injection) |
| Attacker MITMs the TLS handshake | rustls cert-chain validation against site root CA |
| Compromised IOC presents a cert it was issued | Out of scope (revoke and rotate via PKI) |

## Future: `ca-secure` interop with epics-base 7

The upstream `epics-base` project has been working on a secure-CA
extension that adds TLS at the wire level rather than at the socket
level. The two designs are *not* interoperable today:

| Dimension | `experimental-rust-tls` (this crate) | upstream `ca-secure` (draft) |
|-----------|---------------------------------------|------------------------------|
| Layer | TLS wraps the entire TCP stream from byte zero | TLS upgrade negotiated via a `CA_PROTO_VERSION` capability bit |
| Plaintext + TLS on same port | No | Yes — capability flag picks per-connection |
| Spec stability | Stable Rust crate | Drafting upstream |

The Rust-only mode survives — it's the right tool for all-Rust
deployments and for prototyping. When ca-secure stabilizes upstream,
[`crate::tls::ca_secure`](../src/tls/ca_secure.rs) holds the
negotiation hooks and `EPICS_CAS_TLS_MODE=ca-secure-draft`
selects the upgrade path. Today the draft mode falls back to
RustOnly with a debug log; the file documents what needs to land for
real interop.

## Caveats

- TLS adds ~50-100 µs per request (handshake amortized over the
  connection lifetime). On low-latency control loops this is
  noticeable; reserve TLS for cross-LAN segments.
- Cert lifecycle (issuance, rotation, revocation) is outside this
  crate. Use your site's PKI and rotate certs periodically.
- The client's `EPICS_CA_NAME_SERVERS` (TCP nameserver) stream
  benefits from TLS the same way virtual circuits do — the
  follow-up patch covers both paths.
