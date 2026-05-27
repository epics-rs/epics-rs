//! Extended CA wire-golden suite — opt-in via `--profile interop`.
//!
//! Pure-Rust hex-byte comparisons of `encode_*` output against
//! captured C reference (`epics-base` 7.0.x). Mirrors the pattern in
//! `tests/wire_golden.rs` but covers byte shapes whose divergence
//! historically surfaced as parity findings — admission-failure
//! frames, ACCESS_RIGHTS coalescing semantics, extended-form
//! `CLIENT_NAME` / `HOST_NAME`, etc.
//!
//! Gated to the `interop` nextest profile because the surface is
//! easy to over-add (every finding has a candidate test). Keep the
//! default-suite runtime tight; opt-in here when verifying a
//! protocol change.

#![allow(clippy::needless_range_loop)]

mod cases;
