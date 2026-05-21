# epics-pva-rs Documentation

Internal architecture and protocol reference for the Rust pvAccess
(PVA) implementation.

This directory documents how `epics-pva-rs` is built and how it maps
to the EPICS pvAccess wire protocol. The intended audience is anyone
modifying the crate or trying to understand a runtime issue. End-user
usage (running a server, building a client) belongs in the top-level
`README.md`.

## Document map

| File | Topic |
|------|-------|
| [`01-overview.md`](01-overview.md) | High-level architecture and module map |
| [`02-wire-protocol.md`](02-wire-protocol.md) | PVA wire format: header, control vs application, opcodes |
| [`03-pvdata-types.md`](03-pvdata-types.md) | pvData type system: FieldDesc, BitSet, type cache (0xFD/0xFE) |
| [`04-client.md`](04-client.md) | Client internals: ServerConn, search engine, channel pool, ops_v2 |
| [`05-server.md`](05-server.md) | Server internals: TCP, UDP responder, SharedPV, monitor pipeline |
| [`06-state-machines.md`](06-state-machines.md) | Channel lifecycle, reconnect, monitor INIT/START/FINISH |
| [`07-introspection-cache.md`](07-introspection-cache.md) | Per-connection FieldDesc cache, 0xFD/0xFE markers, pvAccessCPP compat |
| [`08-environment.md`](08-environment.md) | All `EPICS_PVA*` / `EPICS_PVAS*` variables |
| [`09-pvxs-parity.md`](09-pvxs-parity.md) | API parity matrix vs upstream pvxs |
| [`functional-review-2026-05-20.md`](functional-review-2026-05-20.md) | Open functional gaps and bugs vs upstream pvxs |
| [`10-observability.md`](10-observability.md) | tracing events + metrics schema |
| [`11-gateway.md`](11-gateway.md) | PVA-to-PVA gateway architecture (mirrors `pva2pva/p2pApp`) |
| [`12-auth-tls.md`](12-auth-tls.md) | Connection validation, ca-auth, x509 mTLS |

## Reading order

If you're new to the codebase:

1. `01-overview.md` — module map and the upstream-parity model.
2. `02-wire-protocol.md` — how PVA frames are laid out on the wire.
3. `03-pvdata-types.md` — the type system every op carries.
4. Pick `04-client.md` or `05-server.md` depending on which side
   you're touching.
5. `06-state-machines.md` for reconnect / monitor lifecycle.
6. The remaining docs are reference material — read on demand.

## Conventions

- File / line references use `crate/file.rs:line` so you can navigate
  in any editor.
- "pvxs" refers to the C++ reference implementation upstream
  ([github.com/mdavidsaver/pvxs](https://github.com/mdavidsaver/pvxs));
  "p2pApp" refers to the legacy C++ PVA gateway in `pva2pva/p2pApp`.
- "channel id" (cid) is client-allocated; "server id" (sid) is server-
  allocated on `CMD_CREATE_CHANNEL`.
- `ioid` (operation id) is per-op (per get / put / monitor), client-
  allocated, scoped to its parent sid.

## Contributing

When changing PVA behaviour:

1. Update the relevant doc page under `doc/` if a protocol- or
   architecture-level invariant changes.
2. Add or extend a test under `crates/epics-pva-rs/tests/` (or the
   parity test fixtures) if behaviour is observable.
3. Run `cargo test -p epics-pva-rs --all-features` before committing.
