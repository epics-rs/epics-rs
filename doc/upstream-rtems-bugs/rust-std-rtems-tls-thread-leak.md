# rust std on RTEMS: every `std::thread` leaks its `Arc<Thread::Inner>` — the TLS key/value pair is freed before its deferred destructor runs

Evidence package for an upstream report — the sixth in this directory, and
the only one whose venue is outside the EPICS ecosystem: **rust-lang/rust**
(with an RTEMS-side conformance discussion as a possible secondary). Report
prose is deliberately not written here (standing rule: upstream prose is
hand-written); this file is the evidence it will be written from.

Mechanism claims were re-verified on **2026-07-23** against the exact
toolchain that produced the measurements (box nightly) and against current
rust master. Measured numbers are quoted from their dated artefacts; the
Sourcing table maps each claim to how it was verified.

## Summary

On `armv7-rtems-eabihf`, every `std::thread` that exits permanently leaks
the heap block holding its `Arc<Thread::Inner>` (128 B, plus a second block
when the thread is named). Joined and detached threads leak identically.
The same lifecycle in raw C (`pthread_create`/exit, including
`pthread_setspecific` use and a 2-round key destructor) leaks 0.000 —
this is not an RTEMS heap property; it is std's thread-handle cleanup
protocol meeting RTEMS's TLS-destructor implementation.

## Mechanism — three verified pieces

**1. rustc emits no `target_thread_local` for the RTEMS target.**

- On the measurement box (nightly 1.99.0 `87e5904f5` 2026-07-20, the
  toolchain all RTEMS images are built with):
  `rustc --print cfg --target armv7-rtems-eabihf | grep thread_local` →
  no output; the same command with `--target armv7-unknown-linux-gnueabihf`
  prints `target_thread_local`.
- Root cause in the compiler, still current on master (fetched 2026-07-23):
  `compiler/rustc_target/src/spec/targets/armv7_rtems_eabihf.rs` never sets
  `has_thread_local`, so it inherits the `TargetOptions` default (`false`).

**2. std's `#[cfg(not(target_thread_local))]` fallback defers the thread
handle's release to a second round of TLS destruction.**

`library/std/src/sys/thread_local/guard/key.rs` (read from the box
sysroot's `rust-src`, same nightly): the fallback `enable()` registers a
`CLEANUP` pthread key whose destructor implements a two-state protocol —
first invocation (`DEFER`) re-arms the key so a second destructor round
runs; the second invocation (`RUN`) calls `crate::rt::thread_cleanup()`,
which calls `drop_current()`. Its comment states the accepted failure mode:
"If there is no further round, there will be leaks, but that's okay" —
i.e. the design assumes *if a second round runs, the deferred cleanup can
still do its job*.

`drop_current()` (`library/std/src/thread/current.rs:324-332`, same
sysroot) releases the handle only if the `CURRENT` TLS slot still holds it:

```rust
pub(crate) fn drop_current() {
    let current = CURRENT.get();
    if current > DESTROYED {
        unsafe {
            CURRENT.set(DESTROYED);
            drop(Thread::from_raw(current));
        }
    }
}
```

Without `target_thread_local`, `CURRENT` (`current.rs:14`, cfg fork at
`:25`) is itself a **destructor-less pthread key**, so the protocol's
correctness rests on `pthread_getspecific(CURRENT)` still returning the
stored pointer when round 2 runs.

**3. RTEMS frees each key/value pair before invoking its destructor — and
consumes destructor-less pairs in the same sweep.**

`cpukit/posix/src/keycreate.c` (kernel `2faafecb`, RTEMS 6.0.0, the exact
kernel the measurements booted; function at `:113`, registered as
`.thread_terminate` at `:170`):

```c
node = _RBTree_Root( &the_thread->Keys.Key_value_pairs );
...
_RBTree_Extract( &the_thread->Keys.Key_value_pairs, ... );   /* :133 */
_POSIX_Keys_Key_value_release( the_thread, &lock_context );
_POSIX_Keys_Key_value_free( key_value_pair );                /* :139 */
...
if ( destructor != NULL && value != NULL ) {
    ( *destructor )( value );
}
```

The loop pops **every** pair — extract, free, *then* maybe call the
destructor. A destructor-less pair (`CURRENT`) is extracted and freed with
its value simply discarded. The "second round" exists only because a
destructor that calls `pthread_setspecific` (as `CLEANUP`'s `DEFER` arm
does) re-inserts a pair, keeping the loop alive.

**Failure sequence** (all three pieces together): round 1 sweeps the tree —
`CURRENT`'s pair is extracted and freed (value discarded), `CLEANUP`
re-arms; round 2 runs `CLEANUP(RUN)` → `thread_cleanup()` →
`drop_current()` → `CURRENT.get()` reads NULL (pair long gone) → the
`current > DESTROYED` guard fails → the `Arc<Thread::Inner>` is never
dropped. Both rounds *did* run — measured `round1=50 round2=50` over 50
threads — so the "no further round" leak the comment accepts is not what
happens; the deferred cleanup runs and finds its data already destroyed.

Neither side is individually wrong in an obvious way: RTEMS may free pair
*storage* when it likes, and POSIX does not clearly specify whether
destructor-less keys' values remain readable during other keys' destructor
iterations (glibc keeps them until thread storage is torn down, which is
what the std protocol was written against). That assessment — unspecified
ordering relied upon by std — is why the primary venue is rust-lang.

## Measured numbers

**Attribution run, 2026-07-22** (box, `-Wl,--wrap=malloc` +
`arm-rtems6-addr2line`; a `#[global_allocator]` counter reads 0 because
`Thread::new` reaches `malloc` directly — the wrap is required):

- 1 block / **128 B** per spawned `std::thread`, +1 block if named (the
  name `CString` lives in the same `Inner`).
- Joined and detached leak identically — a reaper/join discipline does not
  help.
- Raw C `pthread_create`/exit — joined, detached, with
  `pthread_setspecific`, with a 2-round key destructor — leaks **0.000**.
- Destructor rounds measured `round1=50 round2=50` over 50 threads: the
  obvious "RTEMS runs no second round" hypothesis is **false**; the
  pair-freed-before-destructor ordering is the cause.

**Differential residue measurement, 2026-07-23** (in-repo:
`doc/calink-rtems-design.md` §13.2–13.3, `doc/pvalink-rtems-design.md`
§9.11, commits `913f96fc`/`4d366800`): total heap residue per thread
creation, measured as pooled-vs-per-attempt image *difference* (single-image
readings are a documented false confirmation), is **176.0 ± 5 B** (CA, 29
attempts) and **179.1 ± 3 B** (PVA, 87 attempts). The orphaned `Arc`
accounts for 128 B of that; the remaining ~48–51 B are unattributed (see
open items).

**Reach in a Rust IOC:** any thread churn pays it forever — our port's
client dial path did (one thread per attempt; closed by `DialPool`,
`doc/calink-rtems-design.md` §13), and the server side still does (one
thread per accepted connection, `epics-pva-rs/src/server_native/blocking.rs`
and `epics-base-rs/src/runtime/blocking_io.rs` pumps) — 176–179 B per
connection cycle until the planned thread pool lands.

## No existing upstream report

`gh search issues --repo rust-lang/rust` on 2026-07-23 for
`"rtems thread leak"`, `"rtems tls"`, `"target_thread_local rtems"` — zero
hits each. (RTEMS target support is recent, tier 3.)

## What a fix could look like (candidates, for the hand-written report)

- **Compiler:** set `has_thread_local: true` for the RTEMS targets —
  **validated on target 2026-07-23** (see "The spec-flip experiment" below):
  native TLS codegen, link, boot, destructors and isolation all work on
  `armv7-rtems-eabihf`, and the leak goes to exactly 0. Native TLS takes
  the `#[cfg(target_thread_local)]` arm, whose destructor list does not
  depend on reading another key's value in a later round.
- **std:** make the key-based fallback robust to a platform that frees
  pairs before invoking destructors — e.g. `CLEANUP`'s round-1 arm could
  capture the `CURRENT` pointer into the key's own value (the destructor
  argument is passed by value and survives the pair teardown) instead of
  re-reading `CURRENT` via `pthread_getspecific` in round 2.
- **RTEMS (secondary):** whether `_POSIX_Keys_Run_destructors` should keep
  destructor-less pairs readable until destructor iteration completes —
  a POSIX-interpretation discussion, not a clear-cut defect.

## The spec-flip experiment — `has_thread_local: true` measured VIABLE (2026-07-23)

Same toolchain (nightly `87e5904f5`), same box, standalone RTEMS image
(`~/rtems-bringup/tlsdtor/`): the stock spec was dumped to JSON and exactly
one key added (`"has-thread-local": true`; diff vs stock: that key only).
Three images from the same source: stock target, custom-JSON *control*
(spec dumped verbatim, nothing added — isolates the JSON-spec build path),
and custom-JSON *native* (the one key added). Built with
`-Zbuild-std=std,panic_abort` + `-Zjson-target-spec`.

- **Codegen/link:** clean. Native image gets real TLS segments
  (`.tdata 0x38`, `.tbss 0x630`, `PT_TLS memsz 0x668`, `__aeabi_read_tp`
  present, zero unresolved TLS relocations).
- **Boot:** all three images boot and exit 0; no faults.
- **Leak, N=50 spawn/join after 5 warm-up cycles** (RTEMS `malloc_info`,
  console-verified by hand from `stock.log`/`native.log`):

  | image | named threads | unnamed threads |
  |---|---|---|
  | stock | 160.64 B / **2.000 blocks** per thread | **136.00 B / 1.000 block** |
  | control (JSON path, flag off) | 160.64 / 2.000 | 136.00 / 1.000 |
  | native (flag on) | **0.00 / 0.000** | **0.00 / 0.000** — `used.total`/`used.number` byte-identical at n0/n25/n50 |

  136 B = the attributed 128 B `Arc<Thread::Inner>` + 8 B RTEMS heap
  header; the named phase adds exactly the predicted second (name
  `CString`) block. Control ≡ stock rules out the JSON-spec build path as
  the variable.
- **TLS semantics:** user-level `thread_local!` destructors ran 110/110
  and cross-thread isolation held on **both** stock and native — sharpening
  the defect statement: user TLS was never broken; only std's own
  `CURRENT` handle cleanup is.

Caveats: the flag was applied via a custom JSON spec, not an upstream
`armv7_rtems_eabihf.rs` patch (same codegen path, but the one-line upstream
change itself was not built); panic-unwind through TLS, `const`-init
`thread_local!`, TLS from C-created threads, and a full IOC workload were
not exercised.

## Not measured / open

- The ~48–51 B gap between the attributed `Arc`+header (136 B measured on
  bare threads) and the total per-*connection-attempt* residue (176–179 B)
  is unattributed — different measurements; the spec-flip experiment does
  not speak to it.
- Current rust master was inspected (target spec), not executed; the
  measured toolchain is nightly `87e5904f5` (2026-07-20).
- The 2026-07-22 attribution transcript was not re-run on 2026-07-23; its
  artefacts remain on the box (below) and were not copied into this
  repository.

## Sourcing

| claim | verified how |
|---|---|
| no `target_thread_local` on the RTEMS target | run on the box nightly 2026-07-23: `rustc --print cfg` for both targets (rtems: grep exit 1; linux: prints it) |
| `has_thread_local` absent from the target spec, still current | WebFetch of rust master `armv7_rtems_eabihf.rs` raw, 2026-07-23 |
| `guard/key.rs` DEFER/RUN protocol + "no further round" comment | full file read from box sysroot `rust-src` (nightly `87e5904f5`) |
| `drop_current()` guard, `current.rs:324-332`, `CURRENT` cfg fork `:14`/`:25` | sed/grep of the same sysroot file |
| `keycreate.c` extract(:133)/free(:139)-before-destructor, `.thread_terminate` (:170), kernel `2faafecb` | sed + `grep -n` + `git log -1` in `~/rtems-bringup/kernel`, 2026-07-23 |
| 128 B/thread, +named block, joined=detached, raw-pthread 0.000, `round1=50 round2=50` | 2026-07-22 attribution run; artefacts on the box: `~/rtems-bringup/rtems-ca-ioc.instrumented*.rs`, `rtems-*-ioc.heapinstr.rs`, `leak.log`, `leak2.log`; recorded in session memory the same day; **not re-run today** |
| 176.0 ± 5 / 179.1 ± 3 B per creation, differential method | in-repo `doc/calink-rtems-design.md` §13.2–13.3 and `doc/pvalink-rtems-design.md` §9.11 (commits `913f96fc`/`4d366800`), measured 2026-07-23 |
| no existing rust-lang report | `gh search issues`, three queries, 2026-07-23 |
| spec-flip: 136.00→0.00 B/thread, TLS segments, dtor 110/110, iso clean | experiment run 2026-07-23 in `~/rtems-bringup/tlsdtor/` (source, both specs, three images, `stock*.log`/`native*.log`/`control.log`, `RESULT.md`); slope and counter lines re-read directly from `stock.log`/`native.log` console output and recomputed by hand — not quoted from the experiment panel's summary alone |
