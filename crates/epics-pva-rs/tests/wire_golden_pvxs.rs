//! PVA wire-golden suite — pvxs byte-exact reproduction.
//!
//! Opt-in via `cargo nextest run --profile interop`.
//!
//! Each test captures the hex bytes pvxs (`f8d6192`,
//! `$PVXS_HOME`) emits for a known fixture and asserts the Rust
//! encoder produces the same bytes. Catches divergences of the
//! shape caught manually — per-element presence
//! byte, unspecified-address encoding, etc.
//!
//! Captures are documented inline (function comment cites the pvxs
//! source line and the byte breakdown). When pvxs ships a new wire-
//! shape change, re-capture by running the matching unit test in
//! `pvxs/test/testdataencode.cpp` and copying the hex.

#![allow(clippy::needless_range_loop)]

mod pva_cases;
