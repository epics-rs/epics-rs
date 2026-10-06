# record-seam-gate

Dev-only. The record-processing runtime-seam census, shared by every crate that
hosts record or device support. `publish = false`; it reads source text and is
never linked into a shipped artefact.

## The seam, and why naming it is a bug

`runtime::task::spawn` follows the **ambient** execution model, so on a hosted
build it needs a tokio runtime entered on the calling thread. Record processing
does not get one. Three callers reach it without a runtime:

- every blocking CA/PVA connection thread, which drives processing from a plain
  `std::thread` through `block_on_sync` → `park_on`;
- the periodic-scan threads, which drive it on their own banded threads;
- any tail already deferred to the process-global background executor, which
  runs on a callback-pool worker.

Naming the ambient seam there panics with "there is no reactor running" and
kills that thread. The `_background` counterparts — `spawn_background`,
`sleep_background`, `interval_background`, `spawn_blocking_background` — land on
the process-global background executor on every backend, the one executor whose
existence does not depend on the caller's thread.

## The rule

> In the production scope of every source under a crate's census roots, none of
> `BANNED` may appear.

The seam is always named by path at the call site, so a textual census is exact
for it: `runtime::task::spawn(` matches whether the path is spelled `crate::`,
`epics_base_rs::` or `epics_libcom_rs::`, and does not match
`spawn_background(`, which is the whole point. `tokio::task::yield_now` is
deliberately *not* banned — it wakes the waker immediately when polled off a
runtime, so it is correct on either executor.

## Roots, not lists

Each crate names a **directory**, not a set of files:

```rust
// crates/std-rs/tests/record_seam_gate.rs
record_seam_gate::assert_no_ambient_seam_in_tree(
    env!("CARGO_MANIFEST_DIR"),
    &["src/records", "src/device_support"],
    EXEMPT, // &[(file, "why this one may use the ambient executor")]
);
```

The list form came first and could only be as complete as its author's memory:
`epics-base-rs` named 9 of the 115 sources under `src/server`, `std-rs` 6 of
10. A tail deferred in an unnamed file was invisible to a census built to make
the rule unbreakable by eye. Reading the directory covers a new file the moment
it exists.

A file that genuinely wants the ambient executor — a long-lived loop task
started from the caller's own runtime rather than from a framework callback — is
named in that crate's exemption list **with its reason**, because the reason is
the part a reader has to be able to check. An exemption naming a file that no
longer exists fails too: a waiver covering nothing is the staleness the list
form already had.

## Why a crate and not a test module

It began as one `#[cfg(test)]` module in `epics-base-rs` reading its own nine
files through `include_str!`, which cannot see another crate — `include_str!`
may not escape a published crate's package directory. So the rule stopped at the
crate boundary and the defect walked across it: `std-rs`'s
`ThrottleRecord::spawn_value_sync` and `asyn-rs`'s `AsynRecord::special` /
`process_cycle` were all naming `tokio::spawn` from record-support callbacks,
and the throttle one began panicking the moment its caller moved onto the
callback pool.

The answer to "the same rule in three crates" is not three copies of it. Each
crate keeps a short `tests/record_seam_gate.rs` (for `epics-base-rs`, a module
in `server/mod.rs`) naming *its own* roots and exemptions; the rule and its
message live here once. The production slice it runs on is one level further
down, in `source-guard`, shared with every other source guard in the workspace.

## Testing

```bash
cargo nextest run -p record-seam-gate
```

11 tests over the rule's boundaries, including that a stale exemption is
rejected and that `_background` spellings pass.

## License

EPICS Open License (workspace `LICENSE`).
