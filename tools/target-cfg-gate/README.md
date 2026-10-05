# target-cfg-gate

Dev-only. Checks that every test, bench, example and bin target declares the
cfg its own imports require. `publish = false`; it reads source text and a
manifest, and is never linked into a shipped artefact.

## The rule

> A target whose sources name a `cfg`-gated module of its own crate, or an
> optional dependency of its own crate, must itself be declared under a cfg
> that implies that thing's requirement — a file-level `#![cfg(...)]`, or
> `required-features` on its manifest block.

Undeclared, cargo compiles the target in configurations where the thing it
names does not exist. Not hypothetical: `--no-default-features --all-targets`
was red for `epics-ca-rs` and `epics-pva-rs` at 47 targets while
`cargo nextest run --workspace` stayed green throughout, because the default
feature set supplied every module they named. The same shape took three
`asyn-rs` tests down under the empty feature selection
`scripts/rtems-check.sh` builds that crate with: 58 errors under a flag no host
row passes.

## Why a derivation and not a list

`crates/asyn-rs/tests/epics_only_tests_declare_their_feature.rs` enforced this
same rule from a hand-written list of three needles, for one crate and one
feature. The list is the part that rots: the workspace has 131 gated module
paths and 98 optional dependency declarations, and a needle list covers the ones
somebody thought of. Both halves are read out of the sources here — the module
tree under `src/lib.rs` for the requirement, the target's own attributes and
manifest block for the declaration — so a module gated tomorrow is covered
tomorrow.

## The axes it reasons over

Features, `epics_embedded_target`, and the backend cfgs `tokio_backend` /
`exec_backend` (`build.rs` derives `exec_backend = embedded ||
EPICS_RS_BUILD_EXEC_BACKEND=thread`, and `tokio_backend` as its negation).
Those are exactly the axes cargo and `scripts/rtems-check.sh` vary, so a gate on
one of them fails in a configuration no default row builds — silently, which is
why the gate exists.

Every other cfg atom — `unix`, `windows`, `target_os`, a bespoke build-script
cfg — is held true: a platform gate fails loudly on the machine you are already
on, and demanding that a target restate `any(unix, windows)` would be noise
rather than a check.

A target that has to define `main` (a bin, an example, a `harness = false`
bench) is checked on the feature axis alone. That is the mechanism, not a
carve-out: cfg-ing such a file away leaves a crate with no `main` and E0601
instead of a skipped target, so `required-features` is the only gate it can
carry — and `required-features` cannot name a build-script cfg.

## What it does not see

A path is what this gate can follow, so a `#[cfg]` on an *item* inside an
ungated module is invisible to it. `epics_pva_rs::server_native::PvaServer` is
ungated and its `client_config` method is not, and five `epics-pva-rs` tests
named it undeclared. Those were found by compiling every crate at
`--no-default-features --all-targets` until clean, which is the other half of
this rule and the exhaustive half. This gate is the half that runs on every
commit without a per-crate build matrix; neither closes the family alone.

## Running

```bash
cargo run -p target-cfg-gate --bin target-cfg-census   # one line per (target, gated thing)
cargo nextest run -p target-cfg-gate                   # the gate itself
```

18 tests. The gate over the real workspace is one of them; the rest pin the
predicate machinery it stands on — parsing, per-axis projection, implication,
and the source scrub that keeps a path inside a string or a comment from
counting as a reference.

## License

EPICS Open License (workspace `LICENSE`).
