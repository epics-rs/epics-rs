# epics-ca-rs Documentation

Internal architecture and protocol reference for the Rust Channel
Access (CA) implementation.

This directory documents how `epics-ca-rs` is built and how it maps to
the EPICS CA wire protocol. The intended audience is anyone modifying
the crate or trying to understand a runtime issue. End-user usage
(building a client, running an IOC) belongs in the top-level
`README.md`.

## Document map

| File | Topic |
|------|-------|
| [`01-overview.md`](01-overview.md) | High-level architecture and module map |
| [`02-wire-protocol.md`](02-wire-protocol.md) | CA wire format: header, opcodes, DBR layout |
| [`03-client.md`](03-client.md) | Client internals: search, transport, coordinator, subscription, beacon monitor |
| [`04-server.md`](04-server.md) | Server internals: TCP listener, UDP responder, beacon emitter, monitor flow, address list |
| [`05-state-machines.md`](05-state-machines.md) | Channel lifecycle, search backoff, reconnection sequence |
| [`06-dbr-types.md`](06-dbr-types.md) | DBR type families and encoding |
| [`07-flow-control.md`](07-flow-control.md) | Backpressure: client queue, server flow control gate, coalescing |
| [`08-environment.md`](08-environment.md) | All `EPICS_CA_*` / `EPICS_CAS_*` variables and their effect |
| [`functional-review-2026-05-20.md`](functional-review-2026-05-20.md) | Functional-review findings (with resolution status) vs `epics-base` C implementation |
| [`10-observability.md`](10-observability.md) | tracing events, metrics schema, exporter integrations |
| [`11-tls-design.md`](11-tls-design.md) | CA over TLS (encryption + mTLS auth), design and migration plan |
| [`12-discovery.md`](12-discovery.md) | Service discovery via mDNS + DNS-SD, BIND zone setup, custom backends |
| [`../TESTING.md`](../TESTING.md) | How to run unit, interop, stress, and soak tests |
| [`../STABILITY_PLAN.md`](../../../Documents/STABILITY_PLAN.md) | (Out-of-tree) the plan that tracked the P1–P12 stability work |

## Reading order

If you're new to the codebase:

1. `01-overview.md` — get the big picture and the module map.
2. `02-wire-protocol.md` — understand the bytes on the wire so the
   handler code reads naturally.
3. Pick `03-client.md` or `04-server.md` depending on which side you're
   touching.
4. `05-state-machines.md` for connection-lifecycle questions.
5. The remaining docs are reference material — read on demand.

## Conventions

- File / line references inside this crate use `crate/file.rs:line`,
  read relative to `crates/epics-ca-rs/src/`. Anything outside this
  crate is spelled repo-relative in full —
  `crates/epics-base-rs/src/types/codec.rs:538` — because a bare
  basename there resolves in several crates at once and reads as a
  valid reference long after it has stopped being one.
- Wire-protocol byte layouts are shown as `byte offset: field name`
  tables.
- "libca" refers to the C reference client in
  `epics-base/modules/ca/src/client/`. "rsrv" refers to the C server in
  `epics-base/modules/database/src/ioc/rsrv/`.
- "channel id" (cid) is allocated by the client; "server id" (sid) is
  assigned by the server on `CA_PROTO_CREATE_CHAN`.

## Contributing

When changing CA behaviour:

1. Update the relevant doc page under `doc/` if the protocol- or
   architecture-level invariants change.
2. Add or extend a test under `crates/epics-ca-rs/tests/` if behaviour
   is observable.
3. Verify against a real `softIoc` via the interop suite
   (`cargo test -p epics-ca-rs --tests -- --test-threads=1`).
