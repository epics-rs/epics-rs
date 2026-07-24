# RTEMS deviation — we build the target with a custom spec carrying `has-thread-local: true`

**Status:** adopted deviation from the stock toolchain, measured on the bring-up box
**Date:** 2026-07-24
**Deviation site:** the RTEMS target spec — `scripts/rtems-tls-spec.sh` generates it
**Upstream reference:** `rust-lang/rust`
`compiler/rustc_target/src/spec/targets/armv7_rtems_eabihf.rs` (never sets
`has_thread_local`, so it inherits the `TargetOptions` default `false`)
**Evidence:** `doc/upstream-rtems-bugs/rust-std-rtems-tls-thread-leak.md`
(mechanism + the 2026-07-23 spec-flip experiment + the 2026-07-24 adoption
track); booted logs in `doc/upstream-rtems-bugs/evidence/tlsflip/`; rig source
in `doc/upstream-rtems-bugs/repro/tlsflip/`

This file exists so nobody re-derives from scratch why an RTEMS image is built
through a JSON target spec instead of the builtin `armv7-rtems-eabihf` triple,
and — the part that matters most for a *temporary* deviation — the exact
condition under which this whole apparatus is deleted.

---

## 1. The deviation — and what shape it actually is

The stock `armv7-rtems-eabihf` target does not set `has_thread_local`, so
`cfg(target_thread_local)` is false and std takes its **key-based TLS
fallback**. On this target that fallback permanently leaks the thread handle:
RTEMS frees each pthread key/value pair *before* invoking its destructor, so
std's deferred `CLEANUP(RUN)` round reads a `CURRENT` slot that is already gone
and never drops the `Arc<Thread::Inner>`. **Measured: 136 B per `std::thread`,
0 B in raw C** — the mechanism and numbers are in the evidence doc.

The deviation is **one key**. `scripts/rtems-tls-spec.sh` dumps the builtin
spec from the active nightly and adds exactly `"has-thread-local": true`,
proving before it emits that nothing else changed. Native TLS then takes the
`#[cfg(target_thread_local)]` arm, whose destructor list does not depend on
reading another key's value in a later round, and the leak goes to **0**.

**It is not "we invented a target."** It is the builtin target plus the single
key upstream will itself set once the one-line change lands. The deviation is
that we set it *ahead* of upstream, from outside the compiler.

## 2. Why generated, not a frozen JSON in the tree

The builtin spec is whatever the *active* nightly emits — its `data-layout`,
`features` and `metadata` change across nightlies. A frozen JSON committed to
the tree would silently drift from the toolchain a developer actually runs, and
the first symptom would be a mismatched `data-layout` at codegen. So the spec
is **derived** every build: dump the builtin, add the one key. The output
tracks the nightly by construction. Verified 2026-07-24 on two different
nightlies (`87e5904f5` on the box, `59800466c` locally) — both produce a valid
spec whose only difference from that nightly's builtin is the one key.

## 3. What is wired to build through it

| path | how it takes the spec | in this repo? |
|---|---|---|
| `scripts/rtems-check.sh` (the portability gate) | default `TARGET="$(./scripts/rtems-tls-spec.sh)"` + `-Zjson-target-spec`; `RTEMS_USE_STOCK_SPEC=1` reverts to the builtin | yes |
| `scripts/rtems-tls-spec.sh` | the generator itself | yes |
| box `~/rtems-bringup/build-{ca,measure,qsrv,stage5,pva}.sh` (real image + measurement builds) | `--target "$(…/rtems-tls-spec.sh)" -Zjson-target-spec` + `CARGO_TARGET_ARMV7_RTEMS_EABIHF_TLS_LINKER=arm-rtems6-gcc`, all behind `RTEMS_USE_STOCK_SPEC=1` | no — box tooling, not tracked here; updated on the box the same day. `build-ca.sh`/`build-measure.sh` 2026-07-24 morning, the other three the same evening (they were left alone while a measurement panel was mid-run). See §3.1 for `build-pva.sh`. |
| CI (`.github/workflows/rust.yml`) | **nothing to wire.** CI does not build the RTEMS target: its RTEMS coverage is the `rtems-exec-model` feature compiled for the *host* (linux), which is spec-independent, and `rtems-check.sh` is deliberately excluded from CI (it is red on stock `libc`, unrelated to this spec). The spec gate runs on the box. | n/a |

### 3.1 `build-pva.sh` is dead, and was reporting success while dead

Wiring the last three box scripts turned up a pre-existing false green.
`build-pva.sh` builds `-p epics-pva-rs --bin rtems-pva-ioc`, but that binary
moved to `epics-bridge-rs` (`doc/qsrv-rtems-design.md` §9.7) and now declares
`required-features = ["qsrv-core", "pvalink"]`; `epics-pva-rs` produces no
RTEMS binary at all, which its own `Cargo.toml` says in two comments. So
`cargo build` has been failing with `no bin target named rtems-pva-ioc in
epics-pva-rs`.

The script had no `set -e` and piped cargo into `tail`, so the failure was
swallowed twice over: it went on to `cp` and re-stage the *previous*
`pvaioc.exe` and exited **0**. Measured 2026-07-24: the pre-change script
exits 0 while staging a binary dated the day before; with the spec wiring and
`set -e -o pipefail` (as `build-ca.sh` has always had) it exits 101 and stages
nothing.

The script is not repaired, because it has no role left to repair *to*: its
distinguishing feature was a PVA server image without `qsrv-core`, and that
target no longer exists. `build-qsrv.sh` (`qsrv-core,pvalink,bringup-probes`)
and `build-stage5.sh` (`qsrv-core,pvalink`) cover what remains.

**Retired 2026-07-24**: renamed to `build-pva.sh.dead` on the box, so no rig
can invoke it by its old name and get a stale image. Its `build-pva.sh.prespec`
backup is kept beside it as the record of what it was before the wiring.

`cargo check` does not link, so the gate needs no BSP/linker; a real image
build does, hence the `CARGO_TARGET_..._LINKER` env on the box (the checkout's
`.cargo/config.toml` only tables the builtin triple, and this keeps the
machine-specific linker path out of the repo, matching that file's own rule).

## 4. The condition that retires this deviation

**Upstream setting `has_thread_local: true` in `armv7_rtems_eabihf.rs`.** When
that lands and reaches the pinned nightly, the builtin spec already carries the
key, and `scripts/rtems-tls-spec.sh` **refuses** (it checks the stock print for
the key and exits non-zero rather than "add" a key that is already there). That
failure is the trip-wire: it fails `rtems-check.sh` loudly, which is the signal
to

1. set `RTEMS_USE_STOCK_SPEC=1` permanently (or delete the `JSON_SPEC_FLAGS`
   branch so the gate uses the builtin triple),
2. delete `scripts/rtems-tls-spec.sh` and the box build-script spec plumbing,
3. delete this file.

Until then the deviation stands, and every RTEMS image this workspace ships is
built with the flip, so the gate compiles the same native-TLS codegen the
image runs.

## 5. What adoption does and does not buy

Buys (measured, `evidence/tlsflip/`): the 136 B/thread std-thread-handle leak
goes to 0 on **every** thread lifecycle exercised — bare, named, panic-
unwinding, `const`-init, and C-created (raw `pthread_create`) — and the real
`rtems-ca-ioc` builds, boots, serves and runs a 10-minute dial workload
fault-free under the flip, with real Rust TLS segments and zero unresolved TLS
relocations.

Does **not** buy: the server-side thread pool. This paragraph used to say the
per-*connection-attempt* residue had a ~40–51 B component above the thread
handle that the flip could not reach. **That was wrong** — the 2026-07-24
`-Wl,--wrap=malloc` attribution
(`doc/upstream-rtems-bugs/measurement-dial-attempt-residue-on-rtems-6.md`)
measured the per-attempt and pooled dial shapes growing the heap
byte-identically under the flip, so the per-attempt residue is 0 B/attempt and
the flip closes it entirely. The EAGAIN admission bound and the per-connection
fd/memory ceiling still need the pool regardless of this flip — this deviation
does not replace that work, it just does not owe it any residue.
