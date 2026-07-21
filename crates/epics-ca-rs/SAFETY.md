# Unsafe Code Audit

A complete inventory of every `unsafe` block / impl in the EPICS Rust
crates this repository ships, with the invariants each one relies on
and how those invariants are upheld. Audited 2026-04-28.

## Scope

`epics-ca-rs` and `epics-base-rs`. Other crates in the workspace
(`epics-pva-rs`, `epics-bridge-rs`, `optics-rs`, etc.) inherit safety
from these and from third-party `unsafe` in `tokio` / `rustls` /
`hickory` / `socket2`, which we trust transitively.

`grep -rn 'unsafe ' crates/{epics-ca-rs,epics-base-rs}/src/` returns
~23 hits at audit time. They cluster into three categories described
below.

## Category 1 — `std::env::set_var` / `remove_var` (Rust 2024 edition)

Rust 2024 marked these `unsafe` because POSIX `setenv` is not safe to
call from one thread while another reads `getenv`. The race is real
but we side-step it by only calling these helpers from one of two
contexts:

- **IOC startup**, before any worker thread exists. Setting defaults
  via `runtime::env::set_default` happens on the main thread before
  `tokio::main` starts the runtime.
- **The IOC shell** (`iocsh::cmd_epicsenvset`), which is a single
  reader-writer thread interacting with the user.

Every call site:

| File | Line(s) | Context |
|------|---------|---------|
| `epics-base-rs/src/runtime/env.rs:31` | `set_default` helper | Single-thread startup |
| `epics-base-rs/src/runtime/net.rs:44…68` | port test helpers | `#[cfg(test)]` only |
| `epics-base-rs/src/runtime/env.rs:60…113` | env unit tests | `#[cfg(test)]` only |
| `epics-base-rs/src/server/iocsh/commands.rs:663` | `epicsEnvSet` | Single shell thread |
| `epics-base-rs/src/server/iocsh/registry.rs:530…535` | registry tests | `#[cfg(test)]` only |

Test code is run serially (cargo test default) so the unsafety doesn't
materialize. Production uses are documented and fenced to startup /
shell.

The risk we accept: a future contributor adding a `set_var` call
inside an async task. Mitigation: all production set sites go through
`runtime::env::set_default`, which has a SAFETY comment explaining the
"call before threads spawn" requirement. CI reviewers must reject any
new direct `std::env::set_var` calls in non-test code.

## Category 2 — Raw FD borrow for socket-option tweaks

`epics-ca-rs/src/client/search.rs:262` (and analogous sites): we
construct a `BorrowedFd` from a `tokio::net::UdpSocket`'s raw fd so we
can pass it to `socket2::SockRef`. The unsafety is that the borrowed
fd must not outlive the owning socket.

Invariant: in every site, the borrowed fd is consumed inside a `{ … }`
block whose scope ends well before the owning socket is dropped. The
sites are simple enough that visual review suffices; we don't store or
return the borrow.

## Category 3 — `unsafe impl Send` + raw pointer Drop guard in record processing

`epics-base-rs/src/server/record/record_instance.rs:1153–1162` defines
a `ProcessGuard` carrying `*const AtomicBool` so a `?` early return
still resets the per-record `processing` flag. Two distinct unsafety
points:

1. **`unsafe impl Send for ProcessGuard`**: raw pointers aren't
   automatically `Send`, but `AtomicBool` is, and the pointer never
   escapes the function. Documented at the impl.
2. **`unsafe { &*self.0 }`**: dereferences the pointer in `Drop`.
   Justified by the lifetime of the parent `RecordInstance` strictly
   outliving the guard (the record sits inside an
   `Arc<parking_lot::RwLock<RecordInstance>>` for the duration of the call).

Both blocks have SAFETY comments since this audit. There's no easy
way to express the lifetime relationship purely in safe Rust without
either splitting the struct or reorganising the borrow flow; the raw
pointer is the contained workaround.

## Out-of-scope third-party `unsafe`

These crates contain `unsafe` we trust without re-auditing:

- `tokio`, `tokio-rustls`, `rustls`, `socket2` — heavily used,
  audited by their maintainers, in lockstep with the Rust async
  ecosystem
- `hickory-resolver` / `hickory-client` — DNS plumbing
- `mdns-sd`, `if-addrs` — discovery primitives
- `ed25519-dalek` — Ed25519 crypto for cap_tokens / signed beacons
- `ring` (transitive via `rustls`) — cryptographic primitives

If a CVE lands against any of these we treat it like any other dep
upgrade. Cargo `audit` (run in CI) flags advisories.

## Maintenance

When adding new `unsafe`:

1. Add a `// SAFETY: …` comment at the call site explaining what
   invariants make the operation sound.
2. Update the "Category" table above so this file remains a complete
   inventory.
3. Ask whether the unsafety can be avoided. If a safe equivalent
   exists (e.g. `std::os::fd::AsFd` instead of a raw fd dance), use
   it.
