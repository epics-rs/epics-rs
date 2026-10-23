# cfloat-gate

Dev-only. Checks that every precision or exponent float placeholder in
production source formats a `CFloat`. `publish = false`; it reads source text
and is never linked into a shipped artefact.

## The rule

> In the production slice of each named crate's `src`, a format placeholder
> with a precision (`{:.3}`, `{:>8.*}`, `{:.p$}`) or the `e` / `E` type must
> format an argument written `CFloat(..)`.

The caller names the crates and the exemptions: `tests/cfloat_gate.rs` holds
epics-rs's (`crates/*`, `examples/*`), and epics-rs-iocs runs the same check
over its members through a git dev-dependency.

Those placeholders are the workspace's spelling of C's `%.Nf` / `%e`. On a
raw `f64`, Rust prints `NaN` with its sign dropped where glibc prints `nan` /
`-nan`, and `{:e}` writes `1e5` where C writes `1e+05`. 131 such sites were
found printing console or PV text that differed from the C IOC's;
`epics_libcom_rs::cfmt::CFloat` is the one owner of C's spelling.

## Why textual

clippy cannot see a format argument's type, and the type is the only thing
that tells a float from a string being truncated (`{:.12}` of a `&str`). So the
argument must say what it is. A captured identifier (`{x:.3}`) fails, because
nothing at the call site says what `x` holds. Non-floats are named in the
caller's exemption list, with a reason each. An entry that matches nothing
fails the gate.

## What it does not see

A float through a placeholder with no precision (`{}`, `{:?}`) prints Rust's
`NaN` / shortest digits as well. Without a precision, nothing marks the
placeholder as a float, so that family needs its own owner. A format string
not written as one literal (`concat!`) is also not read.

## Running

```bash
cargo run -p cfloat-gate --bin cfloat-census   # every site, exempted or not
cargo nextest run -p cfloat-gate               # the gate itself
```

## License

EPICS Open License (workspace `LICENSE`).
