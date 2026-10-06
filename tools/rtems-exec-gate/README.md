# rtems-exec-gate

Dev-only. The exec-backend test-gating census, shared by every crate in the
RTEMS closure. `publish = false`; it reads source text and is never linked into
a shipped artefact.

## What it is for

On the exec backend the `runtime::task` seam routes `spawn` to the std-thread
background executor, which has no tokio reactor. A test that needs an ambient
reactor therefore cannot run in that configuration — not because it is wrong,
but because the backend deliberately does not start one. Those tests are gated
out; the rest stay in and are expected to pass.

That classification used to live only in reviewers' heads, so a newly added
async test silently reddened the exec-backend suite and the next person had to
rediscover why. This crate makes it a checked property of the source tree.

The backend is one axis: `EPICS_RS_BUILD_EXEC_BACKEND` plus the target triple.
It was also a cargo feature once, which is why the census reads only
`exec_backend` / `tokio_backend` now — one axis, one name, and no way for a gate
to name the target where it meant the backend.

## The rule

Every *reactor-dependent test site* in the scanned crate — a `#[tokio::test]`
attribute, or a line in test code that builds a tokio runtime by hand, since
both obtain the reactor the exec backend does not provide — must be accounted
for by exactly one of four things:

1. a file-level `#![cfg(..)]` gate;
2. an enclosing column-0 `mod` carrying a `#[cfg(..)]` gate;
3. a per-test `#[cfg(..)]` gate directly above it, which is what keeps a file's
   pure wire-format tests running on the exec backend while its async ones are
   gated;
4. a census marker declaring how many ungated sites follow it and why they may
   stay — `// RTEMS-EXEC-MODEL-ALLOW(N): why`, as the first thing in a comment
   body.

A *gate* at any of the first three scopes is any `cfg` predicate that is false
on the exec backend, however spelled and however rustfmt wrapped it. The
predicate is **evaluated**, not matched against a literal, so a new spelling
needs no edit here — and what is not a gate falls out of the same evaluation:
`not(tokio_backend)` selects the exec backend, and
`any(not(feature = ".."), feature = "client")` still holds there, so tests
carrying either stay in the census. A gate the guard cannot read is a site it
counts as ungated, and the tempting fix for that false red is to spell the gate
the guard likes — which is how a census ends up describing its instrument
instead of the tree.

## Why it fails closed

Option 4 is a census, not a blanket waiver: each marker's `N` must equal the
number of ungated sites between it and the next marker. Adding an async test to
an already-declared file changes some marker's count and fails the guard — the
case a plain allowlist lets through. Adding a new file with async tests and no
declaration fails for want of any accounting. Either way the author is told the
rule and has to state which of the four applies.

The count is regional rather than per-file for a reason. A file-wide total is
invalidated by an edit anywhere in the file — `epics-base-rs`'s
`h6_gate_released_across_async.rs` carried a stale `4` from the commit that
added its fifth test — and the number that went wrong is then nowhere near the
line that broke it. A marker that counts only what follows it sits beside the
site it vouches for, where the next author is already editing.

Bumping `N` is deliberate and not self-certifying: the caller runs *inside* the
exec-backend suite, so a site vouched for by a bumped count still has to pass
there.

## Using it

```rust
// crates/<crate>/tests/rtems_exec_model_gate.rs
#[test]
fn the_crate_is_accounted_for() {
    rtems_exec_gate::assert_crate_is_accounted_for(env!("CARGO_MANIFEST_DIR"));
}
```

It began as one test file in `epics-ca-rs`. Every crate that derives the backend
cfg has the same hole — `epics-pva-rs` had it, and its exec-backend suite
reddened exactly as predicted — and the answer to the same rule in three crates
is not three copies of a 300-line audit. Five crates now call the line above;
the rule, its message and its boundary tests live here.

```bash
cargo run -q -p rtems-exec-gate --bin scope-gated -- <workspace-root>
```

prints what the `SCOPE_GATED` pin should say about a tree and why each entry
moved. The pin is a name-set rather than a count precisely so it merges: two
branches that each gate a different file compose into the union instead of into
a wrong total. What does not compose is the pin itself, so filling it in is a
merge-time step on the merged tree — hence a tool that can measure a tree
nobody has built yet.

## Testing

```bash
cargo nextest run -p rtems-exec-gate
```

51 tests. `every_backend_deriving_crate_is_accounted_for` is the reverse check
(every crate whose `build.rs` derives the backend must carry the gate test);
`mod_gate_resolution` pins how a gated `mod` declaration charges the file it
names, through `mod.rs` and through a `#[path]` override; `pin_maintenance`
pins that the printed constant is what rustfmt would write. The fixtures
assemble attribute spellings with `concat!` so this crate's own body does not
match itself when scanned.

## License

EPICS Open License (workspace `LICENSE`).
