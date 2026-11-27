# Fuzzing harness

This directory contains [`cargo-fuzz`](https://rust-fuzz.github.io/book/cargo-fuzz.html)
targets exercising the CA wire-protocol parsers under arbitrary
adversarial inputs. They complement the `proptest`-based tests in
`tests/integration/property_tests.rs` by running for hours/days with libFuzzer's
coverage-guided mutation engine.

The four targets:

| Target | What it fuzzes |
|--------|----------------|
| `fuzz_header_parse` | `CaHeader::from_bytes` — standard 16-byte header |
| `fuzz_header_extended` | `CaHeader::from_bytes_extended` — extended (24-byte) form, including pathological `extended_postsize` claims |
| `fuzz_dbr_decode` | `decode_dbr(type, data, count)` — codec entry point used by client subscription delivery |
| `fuzz_pad_string_input` | `pad_string` — payload-padding helper |

## Setup (one-time)

```bash
# nightly toolchain is required by cargo-fuzz
rustup toolchain install nightly

# install the cargo subcommand
cargo install cargo-fuzz
```

## Run

```bash
cd crates/epics-ca-rs

# 60-second smoke run
cargo +nightly fuzz run fuzz_header_parse -- -max_total_time=60

# overnight run (8 hours)
cargo +nightly fuzz run fuzz_dbr_decode -- -max_total_time=28800

# resume from previous corpus
cargo +nightly fuzz run fuzz_header_extended
```

Discovered crashes are saved to
`fuzz/artifacts/<target>/crash-<hash>` and a regression test should be
added to `tests/integration/property_tests.rs` reproducing them.

## What we expect to find

Rust eliminates entire classes of bug that affect C parsers (UB on
buffer over-read, signed overflow, format-string injection). Realistic
findings are limited to:

- Panic via unchecked indexing → DoS
- Quadratic blowup via attacker-controlled length fields
- Float NaN propagation in numeric decoders
- Memory-amplification (large allocation triggered by small input)

These are still worth fixing — anything that panics in a server task
is a potential DoS. The targets above are designed to make any such
issue surface quickly.

## Continuous fuzzing

For long-term coverage we recommend running these targets on a
dedicated host (or via [OSS-Fuzz](https://github.com/google/oss-fuzz))
for days at a time. The fuzz corpus accumulated under `corpus/<target>`
should be preserved across runs to retain coverage progress.

## Design notes

- All targets use `libfuzzer-sys = "0.4"` (the standard cargo-fuzz
  dependency).
- We avoid `#[no_std]` / panic=abort tweaks; default panic-on-error
  semantics are what cargo-fuzz expects.
- Each target has a single `fuzz_target!` body — the simplest form
  possible. Add corpus seeds under `corpus/<target>/` to bootstrap
  coverage.
