# stream-pcre-sys

PCRE 8.45 (PCRE1), vendored in `pcre/` and compiled by `build.rs` with the
options of PSI's EPICS Makefile for PCRE, which StreamDevice's
`docs/formats.html` points to: 8-bit, no UTF, UCP or JIT, `NEWLINE` 10,
`LINK_SIZE` 2, `PCRE_STATIC`. `src/lib.rs` declares the calls StreamDevice's
`RegexpConverter.cc` makes: `pcre_compile`, `pcre_fullinfo`, `pcre_exec`,
plus `pcre_version`.

It is the C oracle for the PCRE port in
[`epics-stream-rs`](../stream-rs). The tests in `stream-rs/tests/pcre/` and
`stream-rs/tests/pcre_internal.rs` hold `stream_rs::pcre` to this library:
the same compiled bytecode or error, the same `pcre_exec` result and
ovector, the same `pcre_fullinfo` answers and tables, over PCRE's own
testinput corpus.

`epics-stream-rs` does not need it at run time. It is a dev-dependency
only; `%/regexp/` runs on `stream_rs::pcre`, so an IOC that depends on
`epics-stream-rs` links no C PCRE and builds none.

## Use

```rust
use std::ffi::CStr;

let version = unsafe { CStr::from_ptr(stream_pcre_sys::pcre_version()) };
assert_eq!(version.to_bytes(), b"8.45 2021-06-15");
```

Building needs a C compiler (the `cc` crate). The package sets
`links = "pcre"` and exports the header directory, so a dependent's build
script reads it as `DEP_PCRE_INCLUDE`.

No feature flags. The library is BSD-3-Clause, PCRE's own licence
(`pcre/LICENCE`).
