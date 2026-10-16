# 12 — Service discovery (mDNS / DNS-SD)

`epics-ca-rs` adds optional **service discovery** so clients no
longer need to maintain `EPICS_CA_ADDR_LIST` by hand. IOCs announce
themselves; clients pick up the addresses automatically.

Two transport mechanisms, same `_epics-ca._tcp` service type:

- **mDNS** (RFC 6762, link-local multicast) — single subnet only.
  Zero infrastructure: pure UDP multicast 224.0.0.251:5353.
- **DNS-SD over unicast DNS** (RFC 6763) — works across subnets,
  WAN, anywhere standard DNS reaches. Requires SRV/PTR/TXT records
  on a site DNS server (e.g. BIND, dnsmasq, AD DNS).

Both are gated behind the `discovery` cargo feature.

## When to use which

| Topology | Recommended |
|----------|-------------|
| Single subnet / VLAN | mDNS — auto-detect, no config |
| 2–3 subnets behind one router | mDNS reflector (Avahi) or DNS-SD |
| Site-wide (5+ subnets, WAN) | DNS-SD against a real DNS server |
| Cloud-native (Kubernetes) | DNS-SD against in-cluster DNS or `ExternalDNS` |
| Legacy (NAT traversal, fixed peers) | Existing `EPICS_CA_NAME_SERVERS` (unchanged) |

The `Backend` trait makes all of this pluggable — sites with
exotic discovery needs (Consul, etcd, internal CMDB) can implement
their own backend without modifying the crate.

## Quick start

### Server: announce yourself

```bash
# CLI
softioc-rs --pv MOTOR:X:VAL:double:0.0 \
           --mdns motor-ioc \
           --mdns-txt version=4.13 \
           --mdns-txt asg=BEAM
```

```rust
// Programmatic
CaServer::builder()
    .pv("MOTOR:X:VAL", value)
    .announce_mdns("motor-ioc")
    .announce_txt("version", "4.13")
    .build().await?;
```

That's it — same-LAN clients with discovery enabled will now find
this IOC at `<hostname>.local:5064`.

### Client: discover IOCs

```bash
# Env-var driven (no code change)
export EPICS_CA_DISCOVERY="mdns"
caget-rs MOTOR:X:VAL

# Or against unicast DNS-SD
export EPICS_CA_DISCOVERY="dnssd:facility.local"
caget-rs MOTOR:X:VAL

# Combined: try mDNS first, fall back to DNS-SD
export EPICS_CA_DISCOVERY="mdns dnssd:facility.local"
```

```rust
// Programmatic
let client = CaClient::new_with_config(CaClientConfig {
    discovery: Some(DiscoveryConfig::Mdns),
    ..Default::default()
}).await?;
```

## DNS-SD setup (BIND example)

For multi-subnet deployments, drop a snippet like this into your
`facility.local` zone file:

```bind
$TTL 60

; Service-type PTR — one per IOC instance
_epics-ca._tcp                  PTR    motor-ioc._epics-ca._tcp
                                PTR    bpm-ioc._epics-ca._tcp
                                PTR    vacuum-ioc._epics-ca._tcp

; Per-instance SRV (host:UDP search port) + TXT (metadata)
motor-ioc._epics-ca._tcp        SRV    0 0 5064 motor-host
                                TXT    "version=4.13" "asg=BEAM"
bpm-ioc._epics-ca._tcp          SRV    0 0 5064 bpm-host
                                TXT    "version=4.13" "asg=DIAG"

; Hosts → IPs (usually already in your zone)
motor-host                      A      10.0.5.42
bpm-host                        A      10.0.6.17
```

Reload (`rndc reload`), and any client with
`EPICS_CA_DISCOVERY=dnssd:facility.local` immediately sees both
IOCs. Adding a new IOC = appending one PTR + one SRV (+ optional
TXT) and reloading.

If you'd rather not edit zone files by hand, the
`ZoneSnippet::render()` helper produces the snippet
programmatically.

## Self-registering IOCs (RFC 2136 Dynamic DNS UPDATE)

Editing the zone file every time an IOC is added or moved is
operationally painful. The `discovery-dns-update` cargo feature
lets the IOC do it itself:

- on startup, the server sends an authenticated `UPDATE` message
  adding `SRV` / `PTR` / `TXT` records;
- a background keepalive task refreshes them every `keepalive`
  interval so a dead IOC's records age out naturally;
- on graceful shutdown the `Drop` impl sends a `DELETE` UPDATE so
  the zone snaps back immediately.

```bash
softioc-rs --pv MOTOR:X:VAL:double:0.0 \
    --dns-update-server 10.0.0.1:53 \
    --dns-update-zone facility.local. \
    --dns-update-instance motor-ioc \
    --dns-update-host motor-host \
    --dns-update-tsig-key /etc/epics/tsig.key
```

```rust
let key = TsigKey::from_bind_file("/etc/epics/tsig.key")?;
let reg = DnsRegistration {
    server: "10.0.0.1:53".parse()?,
    zone: "facility.local.".into(),
    instance: "motor-ioc".into(),
    host: "motor-host".into(),
    port: 5064,
    txt: vec![("version".into(), "4.13".into())],
    ttl: Duration::from_secs(60),
    keepalive: Duration::from_secs(30),
    tsig: Some(key),
};
CaServer::builder().register_dns_update(reg).build().await?;
```

### TSIG key (BIND)

Generate a shared key on the DNS server:

```bash
tsig-keygen -a hmac-sha256 epics-key > /etc/bind/epics.key
```

The same file ships to every IOC. Reference it from `named.conf.local`:

```bind
include "/etc/bind/epics.key";

zone "facility.local" {
    type master;
    file "/var/lib/bind/db.facility.local";
    update-policy {
        grant epics-key zonesub ANY;
    };
};
```

### Choosing TTL and keepalive

Pick a `ttl` long enough that the keepalive can refresh well
before expiry — `keepalive < ttl/2` is the rule. The defaults
(60 s TTL, 30 s keepalive) are conservative; for stable facilities
bumping to `ttl=600 keepalive=200` reduces UPDATE traffic.

### When dynamic UPDATE is NOT a fit

- DNS server doesn't allow `UPDATE` (Active Directory DNS
  sometimes locks this down site-wide). Work with IT to either
  enable secure dynamic updates or fall back to the static zone
  snippet.
- Stateless / single-shot IOCs that come and go faster than the
  TTL — better served by mDNS (no zone churn at all).
- Air-gapped clusters with no DNS server reachable. Static
  `EPICS_CA_ADDR_LIST` is fine; keep things simple.

## EPICS_CA_DISCOVERY syntax

Whitespace-separated tokens, evaluated in order:

| Token | Effect |
|-------|--------|
| `mdns` | Enable mDNS browse |
| `dnssd:<zone>` | Enable DNS-SD against the given zone |
| `static:<addr>,<addr>,...` | Static address list (alternative to `EPICS_CA_ADDR_LIST`) |

Examples:

```bash
EPICS_CA_DISCOVERY="mdns"                                    # LAN only
EPICS_CA_DISCOVERY="dnssd:facility.local"                    # site DNS
EPICS_CA_DISCOVERY="mdns dnssd:facility.local"               # both
EPICS_CA_DISCOVERY="dnssd:opera.example dnssd:diag.example"  # multi-zone
EPICS_CA_DISCOVERY="static:10.0.0.5:5064,10.0.0.6:5064"      # explicit
```

## Defaults

- The `discovery` cargo feature is **off by default**. Default builds
  carry no mDNS / DNS-SD code.
- Even with the feature, `EPICS_CA_DISCOVERY` is not set by default,
  so clients fall back to the existing `EPICS_CA_ADDR_LIST` /
  `EPICS_CA_AUTO_ADDR_LIST` flow.
- Server: `announce_mdns(...)` must be called explicitly. By
  default the server announces nothing.

Discovery is purely additive — discovered addresses are merged with
whatever `EPICS_CA_ADDR_LIST` / `EPICS_CA_AUTO_ADDR_LIST` already
provide. There's no way for discovery to *prevent* search from
reaching a previously-known address.

## Subnet boundaries (the honest answer)

mDNS uses TTL=1 multicast → packets are dropped at the first router.
For multi-subnet deployments:

1. **mDNS reflector** — Avahi-daemon's `enable-reflector=yes` mode
   forwards mDNS across local subnets. Works for 2–3 segments;
   beyond that, multicast becomes unwieldy.
2. **DNS-SD over unicast DNS** — the proper solution for site-wide
   discovery. Uses standard DNS routing.
3. **Custom Backend** — implement the trait to query your existing
   service registry (Consul, etcd, k8s, internal CMDB).

For most facility deployments option 2 is the answer: leverage the
DNS infrastructure your site IT team already runs.

## Security notes

mDNS is unauthenticated by design — anyone on the link-local
segment can announce a service. Don't trust mDNS results for
authentication; use ACF (or mTLS, see
[`11-tls-design.md`](11-tls-design.md)) on the resulting
connection.

DNS-SD over unicast DNS inherits whatever guarantees your DNS
infrastructure offers. For tamper-resistance, sign the zone with
DNSSEC; for confidentiality of queries, run DNS-over-TLS or
DNS-over-HTTPS.

## Custom backends

Sites with their own service registry (Consul, etcd, an HTTP CMDB,
a Kafka topic, …) plug in by implementing
`discovery::Backend` and adding it to `CaClientConfig::extra_backends`:

```rust
struct MyRegistry { /* … */ }

#[async_trait]
impl Backend for MyRegistry {
    async fn discover(&self) -> Vec<SocketAddr> { /* fetch */ }
    fn subscribe(&self) -> Option<UnboundedReceiver<DiscoveryEvent>> {
        /* push live updates */
    }
}

CaClient::new_with_config(CaClientConfig {
    extra_backends: vec![Box::new(MyRegistry::new())],
    ..Default::default()
}).await?;
```

A worked example with three backend templates (file-based,
HTTP-API, push-style) is at
`examples/custom_discovery_backend.rs`.

## Status

- mDNS server announce: ✅
- mDNS client discover (with subscribe stream): ✅
- DNS-SD unicast client (PTR / SRV / TXT): ✅
- Zone snippet generator (`ZoneSnippet`): ✅
- `EPICS_CA_DISCOVERY` env var: ✅
- softioc-rs `--mdns` / `--mdns-txt` CLI: ✅
- RFC 2136 dynamic DNS UPDATE with TSIG, keepalive, and Drop-based
  cleanup (`discovery-dns-update` feature): ✅
- softioc-rs `--dns-update-*` CLI: ✅
- Custom backends (`extra_backends` field + `Backend` trait): ✅
  template at `examples/custom_discovery_backend.rs`. No bundled
  Consul / etcd integrations — implement to taste.
