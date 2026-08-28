# pvxs golden-byte capture

Captures pvxs's actual `to_wire` byte output for every fixture the
Rust wire-golden suite asserts against, linking the harness against
the same `libpvxs.a` a real pvxs build uses. The point of this
indirection is **provenance**: a golden hex value derived by reading
`dataencode.cpp` can share a blind spot with the encoder under test
(both can be wrong the same way). A byte that came out of pvxs's
own encoder at run time can't.

## The golden rule

A PVA wire fixture earns a place in this suite **only** when both
halves hold:

1. **Provenance — captured, never derived.** The expected bytes MUST
   come out of pvxs's own encoder at run time (`capture.cpp` →
   `fixtures.txt`, read via `golden(key)`). Never hand-derive a hex
   value by reading `dataencode.cpp` / `pvaproto.cpp` and never read
   bytes off a Rust round-trip — both can share the encoder's blind
   spot. If pvxs has no clean single call that emits the bytes
   (`to_wire`, `to_wire_valid`, or an `nt::*` builder), the fixture
   fails this half: hand-writing builder code on both sides
   reintroduces exactly the shared-mistake risk the suite exists to
   kill. Such a case stays an inline Rust assertion in its own test
   module (see *What's not captured*), not a `golden(...)` fixture.

2. **Distinct coverage.** The fixture must lock a wire shape no
   existing fixture or parity test already pins. A variant that only
   re-exercises a shape another test owns is not added — e.g. extra
   Cached TypeID permutations are already covered by
   `tests/parity/testxcode_port.rs::pvxs_typestore_*`, so they are
   not duplicated here.

A corollary on builders likely to diverge (NTTable, NTURI, and other
multi-substructure `nt::*` builders): the pvxs-side setup is involved
and the two ports drift easily, so the first capture usually surfaces
a real encoder finding rather than a clean lock. Treat that finding as
its own fix — separate PR, out of a golden-*expansion* PR's scope —
and add the fixture only once the encoder agrees with pvxs.

## Files

- `capture.cpp` — single-binary harness; one `emit(...)` call per
  fixture, each running pvxs's `to_wire` (or `to_wire_valid` with a
  leading-`BitSet` strip for compound fixtures).
- `build.sh` — compiles it. The trees come from `PVXS_HOME` and
  `EPICS_BASE`; host arch and compiler default off `uname`
  (`linux-<machine>` + `g++`, `darwin-<machine>` + `clang++`) and are
  overridable with `EPICS_HOST_ARCH` and `CXX`. A missing tree ends the
  build and removes `./capture`, so a stale binary cannot republish an
  older tree's bytes as the goldens.
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
