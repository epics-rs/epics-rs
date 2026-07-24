# rust std on RTEMS: every `std::thread` leaks its `Arc<Thread::Inner>` — the TLS key/value pair is freed before its deferred destructor runs

Evidence package for an upstream report — the sixth in this directory, and
the only one whose venue is outside the EPICS ecosystem: **rust-lang/rust**.
(An RTEMS-side conformance report was considered as a secondary and killed
by measurement on 2026-07-24 — glibc behaves the same; see the C-primitive
control section.) Report
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

POSIX does not clearly specify whether destructor-less keys' values remain
readable during other keys' destructor iterations — and a controlled
C-primitive experiment (next section, 2026-07-24) shows that **neither
RTEMS nor glibc keeps them readable**: glibc's exit sweep clears every
non-NULL slot regardless of destructor (`nptl_deallocate_tsd.c`: "Always
clear the data"), so by round 2 the value is gone on *both* measured
implementations. The earlier working assumption that glibc preserves them
was wrong. This pins the defect on the std protocol itself: it relies on
behavior that POSIX does not guarantee and that no measured mainstream
implementation provides — which is why the venue is rust-lang, alone.

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
accounts for 128 B of that. The remaining ~48–51 B was long carried here as
"unattributed"; the 2026-07-24 `--wrap=malloc` attribution
(`measurement-dial-attempt-residue-on-rtems-6.md`) showed it is **not a
separate allocation at all** — the whole 176/179 B is thread-creation cost,
and under the flip the two dial shapes grow the heap byte-identically
(0 B/attempt difference).

**Reach in a Rust IOC:** any thread churn pays it forever — our port's
client dial path did (one thread per attempt; closed by `DialPool`,
`doc/calink-rtems-design.md` §13), and the server side still does (one
thread per accepted connection, `epics-pva-rs/src/server_native/blocking.rs`
and `epics-base-rs/src/runtime/blocking_io.rs` pumps) — 176–179 B per
connection cycle until the planned thread pool lands.

## The C-primitive control — RTEMS *and* glibc drain destructor-less values (2026-07-24)

Run to decide whether an RTEMS-side POSIX-conformance report (a would-be
seventh package) was sustainable. **It is not** — the measurement killed it —
but it sharpened this package instead. Full write-up with raw logs and
hashes: [`evidence/keydtor/RESULT.md`](evidence/keydtor/RESULT.md); sources
in [`repro/keydtor/`](repro/keydtor/).

Minimal C program (`keydtor.c` / `keydtor-setorder.c`, zero Rust): key A
with **no** destructor, key B **with** one; a thread sets both and exits;
B's destructor reads `pthread_getspecific(A)` and re-arms itself once to
force a second round. Both key-creation orders × both `setspecific` orders,
each image run twice (byte-identical logs):

| `getspecific(A)` from B's destructor | round 1 | round 2 |
|---|---|---|
| RTEMS 6.0.0 armv7 (kernel `2faafecb`) | NULL when A was **set** first (RBTree insertion order); creation order irrelevant | **NULL always** |
| Linux glibc 2.39 | NULL when A's **key id** is lower (slot order); creation order decides | **NULL always** |

- glibc's behavior is deliberate — `nptl/nptl_deallocate_tsd.c` (master,
  fetched 2026-07-24): `if (data != NULL) { /* Always clear the data. */
  level2[inner].data = NULL; if (seq match && destr != NULL) destr (data); }`
  — every non-NULL slot is cleared in the sweep, destructor or not; only the
  ordering rule differs from RTEMS.
- The destructor **argument** channel worked correctly on both platforms in
  both rounds (`B_arg=0xb0b0b0b0` round 1, re-armed `0xb1b1b1b1` round 2) —
  the value handed to a destructor by the implementation itself is the one
  POSIX-guaranteed way to receive data across the teardown, which is exactly
  the std fix candidate below.
- Conformance reading that motivated the experiment: POSIX specifies the
  NULL return from `pthread_getspecific` inside a destructor *only for the
  key being destroyed*, and specifies value-clearing only for keys with
  destructors. Strictly read, both implementations deviate identically; with
  glibc as established practice, an RTEMS-only defect report is not
  sustainable, and RTEMS's extract-before-call loop is a deliberate fix for
  its 2010 issue #1615 (destructor re-invoked with a stale value).
- Consequence for this package: the std key-based fallback is broken on
  *any* key-based-TLS platform that behaves like the two measured ones — a
  hypothetical glibc target using the fallback would leak identically. No
  tier-1 target does (they all have native TLS), which is presumably why
  this never surfaced before a tier-3 target exercised the fallback.

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
  re-reading `CURRENT` via `pthread_getspecific` in round 2. The keydtor
  control (above) measured the argument channel delivering the correct
  value in both rounds on both RTEMS and glibc — it is the only channel
  either implementation (or POSIX) actually guarantees.
- ~~**RTEMS (secondary)**~~ **closed by measurement 2026-07-24**: glibc
  clears destructor-less values in the same sweep (deliberately, "Always
  clear the data"), so RTEMS's behavior matches mainstream practice and an
  RTEMS-side conformance report is not sustainable (see the C-primitive
  control section).

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
change itself was not built). Four cases the 2026-07-23 rig did **not**
exercise — panic-unwind through TLS, `const`-init `thread_local!`, TLS from
C-created threads, and a full IOC workload — were the five owed adoption
checks, and all are now measured: see the next section.

## The adoption track — the five owed checks, measured (2026-07-24)

A second rig, `repro/tlsflip/` (booted logs in `evidence/tlsflip/`), closes
the four unexercised cases above plus the build-wiring question. Same
toolchain (box nightly `87e5904f5`), same one-key spec
(`repro/tlsflip/armv7-rtems-tls.json`, diff vs the current stock print: the
single key `has-thread-local: true`). The rig runs **one phase per image**,
selected at build time (`TLSDTOR_PHASE`), because the stock leak was measured
to be **contingent on TLS key-creation order** — on one unchanged stock image,
touching the rig's `SLOT` `thread_local!` before `std::thread::current()`
measures 136.16 B/thread, and the reverse order measures 0.00 (RTEMS's exit
sweep pops the root of a per-thread RBTree, so which pair is freed before
std's deferred `CLEANUP(RUN)` reads `CURRENT` depends on the tree shape). A
four-phase boot would measure later phases in a key layout the earlier ones
created; one phase per image removes that variable, leaving the spec flag as
the only difference in each stock/native pair. Every image was booted twice;
the measurement lines are byte-identical across the two boots except in the
two socket-touching phases, noted below. Native leak is **0.00 in every
phase**.

**Check ONE — TLS destructors on an unwinding panic.** The rig images are
`panic=unwind` (the spec sets no `panic-strategy`, so it inherits the default;
`-Zbuild-std=std,panic_abort` names crates to build, it does not select the
strategy — confirmed by `panic_unwind` linked, `.ARM.exidx` present, and
`rust_eh_personality` in the image). A thread sets its `thread_local!` slot,
`catch_unwind`s an inner panic, then panics out of the closure with a live
frame guard. `stock-unwind`/`native-unwind`: **frame_drops=110/110** (proves
a real unwind, not an abort — an abort never runs the guard's `Drop`),
**catch=55/55**, **join_err=55/55**, **slot destructor 55/55**. The TLS
destructor runs on the unwinding exit on both images; native leaks 0.00.

**Check TWO — Rust TLS from a C-created thread.** `cthread-*`: a thread
created by raw `pthread_create` (never `std::thread`) runs a Rust
`extern "C"` body that touches two `thread_local!`s and calls
`std::thread::current()`. **started=55/55, tls_ok=55/55, tls_bad=0,
current_distinct=55/55** (std minted a distinct per-thread handle each time),
destructors 55/55. Stock leaks **136.00 B per C-created thread**; native
0.00. *Codebase audit:* production Rust creates **no** threads via
`pthread_create` (`rg 'pthread_create'` over `crates/**/*.rs` non-test: one
comment, zero calls) — every worker is `std::thread` /
`spawn_dedicated_thread`. The **only** structural site where Rust TLS is
touched on a C-created thread is the process entry: RTEMS's initial POSIX task
`POSIX_Init` (`crates/epics-rtems-boot/csrc/rtems_init.c`) calls Rust `main`
directly, and `main`'s call graph touches `thread_local!`s (e.g.
`runtime::task::SCHED_CALLS`). That thread never exits until board reset, so
its handle cost is one-time, not churn. No Rust callback is registered to run
on a libbsd/RTEMS-created thread (the I/O model is blocking-thread-per-
connection on Rust-spawned threads, no reactor, no C callback into Rust TLS).

**Check THREE — `const`-init `thread_local!`.** `constinit-*`: a
`const { Cell::new(0) }` slot (no destructor) and a `const { ConstMarker … }`
slot (with `Drop`). **iso_ok=55/55, iso_bad=0**, const-init destructor
**55/55**; native 0.00.

**Check FOUR — full IOC workload on the flipped image.** The real
`rtems-ca-ioc` (full dependency graph, not the microrig) was built with the
flipped spec via
`--target …/armv7-rtems-tls.json -Zbuild-std=std,panic_abort -Zjson-target-spec`
(linker supplied by `CARGO_TARGET_ARMV7_RTEMS_TLS_LINKER` so the checkout's
`.cargo/config.toml` is untouched), alongside a stock-spec control from the
same source commit.

- **Codegen/link:** the native IOC gains a real Rust TLS segment —
  `.tdata 0x98` / `.tbss 0x7f8` / `PT_TLS memsz 0x890` vs stock
  `0x18`/`0x608`/`0x620` — with **zero** unresolved TLS relocations
  (`evidence/tlsflip/rtems-ca-ioc-tls-segments.txt`).
- **Boot + serve:** both images boot, bring up libbsd + DHCP (lease
  `10.0.2.15` BOUND), build the database, install the calink resolver, and
  register `RTEMS:AO`/`RTEMS:LO`/`RTEMS:MSG`. A 10-minute dial workload
  (compiled-in refused name server driving the DialPool) ran with **no fault**
  on the native image (`evidence/tlsflip/measure-native.log`), the stock
  control likewise (`measure-stock.log`). Host-to-guest `caget` over SLIRP
  hits the CA server-address-advertisement obstacle (the server advertises its
  own `10.0.2.15`, unreachable from the host) — an environmental limit
  independent of the spec, which is why the residue reading below uses the
  guest-internal console gauge, not a wire read.
- **Residue.** The prior 176.0 ± 5 / 179.1 ± 3 B *per connection attempt* is
  128 B thread handle + a ~40–51 B remainder this section treated as separate
  dial-machinery allocation. **It is not** — see the correction at the end of
  this bullet. The flip's effect on the
  handle is now measured **four ways** (bare unnamed 136→0, bare named
  160.64→0, C-created 136→0, and a **noise-free dial phase** — `dialattempt-*`,
  a named thread + `TcpStream::connect` to a refused `127.0.0.1:9` per
  attempt, the pre-DialPool `CAC-connect` shape — **161.76→0.00 / 1.98→0.00
  blocks**, run 2 `161.12→1.28`, i.e. native 0 within a ~1 B socket-path
  noise floor). So the flip removes the **entire** thread-creation-and-dial
  cost, and a bare thread+socket dial leaves **no** per-attempt residue. The
  ~40–51 B gap is therefore neither the thread handle (zeroed with no residue)
  nor a bare socket; it is other per-attempt allocation in the real dial
  machinery (the `oneshot`/`DialRequest`/address formatting the microrig does
  not reproduce), which is orthogonal to `has_thread_local` and unaffected by
  the flip. **Not** independently re-confirmed by a full pooled-vs-perattempt
  IOC differential under the flipped spec this session: a single-image full-IOC
  `MEM_USED` slope moves ±~1 KB per C6 tick (record processing) and reached
  only 10 dial attempts in 10 min, far too coarse to resolve ~40 B — the
  documented single-image false-confirmation the differential exists to avoid.
  This does not affect the adoption verdict (the flip's job is the 128 B
  handle, which is fully closed).

  **Correction, 2026-07-24.** The full pooled-vs-perattempt IOC differential
  this bullet says was not run *has* now been run under the flipped spec, with
  absolute `-Wl,--wrap=malloc` per-call-site accounting instead of a `MEM_FREE`
  slope — which removes the drift that made it impossible before. The two dial
  shapes grow the heap **byte-identically** (+1936 B each over the same 209
  attempts), so the per-attempt residue is **0 B/attempt** and the ~40 B
  remainder does not exist. The server-side thread pool is still needed for
  the EAGAIN admission bound and the per-connection fd/memory ceiling, but
  **not** for this residue. See
  `measurement-dial-attempt-residue-on-rtems-6.md`.

**Check FIVE — build wiring.** See `doc/rtems-tls-spec-deviation.md`: the
committed spec, the paths taught to build through it, and the exact condition
that retires the deviation (upstream setting `has_thread_local: true` in
`armv7_rtems_eabihf.rs`).

## Not measured / open

- ~~The ~48–51 B gap … is unattributed.~~ **CLOSED 2026-07-24 — the gap does
  not exist.** The pooled-vs-perattempt IOC differential this bullet said was
  not run *was* run, under the flipped spec, with `-Wl,--wrap=malloc`
  per-call-site accounting instead of a `MEM_FREE` slope: the two dial shapes
  grow the heap **byte-identically** (+1936 B each over the same 209 dial
  attempts, same five size classes, same per-class counts), so the per-attempt
  residue the `DialPool` removes is **0 B/attempt**, not ~40–51. The 176/179 B
  was all thread-creation cost; the "remainder" was an arithmetic residue
  between a bare *unnamed* thread (136 B) and a *named* thread inside the real
  dial (176 B), not a separate allocation. Under an unreachable name server the
  only growth left is one 144 B block per ~120 s in
  `epics_ca_rs::client::search::fire_searches`, bounded by
  `EPICS_CA_NAMESERVER_QUEUE_DEPTH` (proven by pinning it to 8 and watching the
  count stop at exactly +8). See
  `measurement-dial-attempt-residue-on-rtems-6.md`.
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
| keydtor control: RTEMS+glibc both NULL by round 2, order rules, argument channel intact | raw `KEYDTOR-*` lines re-read from the four logs in `evidence/keydtor/` (sha256 verified against `RESULT.md`'s table after copy off the box, 2026-07-24); run twice per image, logs byte-identical |
| glibc "Always clear the data" sweep | WebFetch of glibc master `nptl/nptl_deallocate_tsd.c` raw (github mirror), 2026-07-24 |
| POSIX destructor/getspecific text | pubs.opengroup.org POSIX.1-2017 `pthread_key_create` + `pthread_getspecific` DESCRIPTION, fetched 2026-07-24 |
| RTEMS #1615 history (extract-before-call is a deliberate fix) | gitlab.rtems.org issue #1615 fetched 2026-07-24; no existing report of the drain behavior (gitlab search `_POSIX_Keys_Run_destructors`: only #1615/#1266, both distinct) |
| spec-flip: 136.00→0.00 B/thread, TLS segments, dtor 110/110, iso clean | experiment run 2026-07-23 in `~/rtems-bringup/tlsdtor/` (source, both specs, three images, `stock*.log`/`native*.log`/`control.log`, `RESULT.md`); slope and counter lines re-read directly from `stock.log`/`native.log` console output and recomputed by hand — not quoted from the experiment panel's summary alone |
| adoption checks 1–3 (unwind dtor 55/55 + frame_drops 110/110; C-thread tls_ok 55/55; const-init iso 55/55), each stock 136/native 0.00 | rig 2 `repro/tlsflip/` on box nightly `87e5904f5` 2026-07-24, 8 images (4 phases × 2 specs), each booted twice byte-identical; `SLOPE`/`TLSDTOR2-*` lines in `evidence/tlsflip/{stock,native}-{plain,unwind,cthread,constinit}.run1.log` |
| key-creation-order contingency of the stock leak (SLOT-first 136.16 vs CURRENT-first 0.00 on one unchanged image) | measured on the box 2026-07-24 by reordering the main-thread key touches in one stock image and re-booting; the reason one phase per image is required |
| check 4: real `rtems-ca-ioc` builds+boots+DHCP+serves+10-min dial workload fault-free under the flip; native `.tdata 0x98`/`PT_TLS 0x890` vs stock `0x18`/`0x620`, 0 unresolved TLS relocs | box epics-rs `419b59d5d7c`, both specs from one source; `evidence/tlsflip/measure-{native,stock}.log`, `rtems-ca-ioc-tls-segments.txt`, `caflip-native2.log` (DHCP `10.0.2.15` BOUND); linker via `CARGO_TARGET_ARMV7_RTEMS_TLS_LINKER` |
| check 4 residue: flip zeroes the dial thread+socket cost (`dialattempt` 161.76→0.00 / 1.98→0.00 blocks, refused=55/55) | rig 2 `dialattempt` phase, `evidence/tlsflip/{stock,native}-dialattempt.run1.log`; run-2 slopes (stock 161.12, native 1.28) noted inline in the doc as the ~1 B socket noise floor |
| ~~~40–51 B gap is non-handle dial-machinery alloc~~ **RETRACTED**: per-attempt minus pooled is 0 B/attempt (+1936 B each over the same 209 attempts, identical size classes) | 2026-07-24 `-Wl,--wrap=malloc` + `arm-rtems6-addr2line` rig, four boots, `evidence/dialresidue/*.log.gz`, `repro/dialresidue/`; written up in `measurement-dial-attempt-residue-on-rtems-6.md` |
