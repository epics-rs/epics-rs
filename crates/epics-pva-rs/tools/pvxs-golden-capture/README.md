# pvxs golden-byte capture

Captures pvxs's actual `to_wire` byte output for every fixture the
Rust wire-golden suite asserts against, linking the harness against
the same `libpvxs.a` a real pvxs build uses. The point of this
indirection is **provenance**: a golden hex value derived by reading
`dataencode.cpp` can share a blind spot with the encoder under test
(both can be wrong the same way). A byte that came out of pvxs's
own encoder at run time can't.

## Files

- `capture.cpp` — single-binary harness; one `emit(...)` call per
  fixture, each running pvxs's `to_wire` (or `to_wire_valid` with a
  leading-`BitSet` strip for compound fixtures).
- `build.sh` — `clang++` invocation with the right `-I`/`-L` for a
  macOS box where pvxs lives at `~/codes/pvxs` and EPICS base at
  `~/epics/epics-base`. Override with `PVXS_TOP`, `EPICS_BASE`,
  `EPICS_HOST_ARCH` env vars.
- `fixtures.txt` — captured output, **the** source of truth for the
  Rust tests. `key=hex` per line, one fixture per line.

## Workflow

Capture or re-capture after a pvxs change:

```sh
./build.sh
./capture > fixtures.txt
cd ../../../..
cargo nextest run -p epics-pva-rs --profile interop
```

A diff in `fixtures.txt` after re-capture is the wire-shape change
report — review it line by line. If a Rust test fails after a re-
capture, the encoder has diverged from pvxs (or pvxs's wire shape
changed and the Rust encoder must follow).

## Pinned pvxs version

The current `fixtures.txt` snapshot is from pvxs `f8d6192` ("server:
monitor TX check buffer level on each iteration"). The pin lives in
the comment block at the top of `capture.cpp`; update both when re-
capturing against a new pvxs commit.

## What's not captured

- `unspecified_address` fixtures — use a separate Rust encoder
  (`encode_unspec_addr`-family) not `to_wire`, so they live as
  inline assertions in `unspecified_address.rs`.
- `monitor_data` fixtures — exercise `encode_pv_field_with_bitset`,
  which has no clean pvxs single-call equivalent (the leading
  BitSet header is what pvxs emits; the value-only portion is what
  Rust asserts).
- `type_code` fixtures — byte-stable round-trip checks, not pvxs-
  bytes assertions.

These three categories are documented in their respective test
modules and stay inline.
