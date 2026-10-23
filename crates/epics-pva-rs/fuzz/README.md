# epics-pva-rs fuzz targets

Adversarial-input harness for the PVA wire decoders. Built on top of
`cargo-fuzz` + `libfuzzer-sys`.

## Targets

| Target | Surface |
|---|---|
| `fuzz_pva_header` | `PvaHeader::decode` — 8-byte frame header parser |
| `fuzz_search_response` | UDP search response — `try_parse_frame → decode_search_response` |
| `fuzz_type_desc` | Wire-format type-descriptor parse (recursive Structure/Union) |
| `fuzz_pv_field` | `decode_pv_field` against a synthetic NTScalar introspection |
| `fuzz_op_response` | Full GET/PUT/MONITOR/RPC response decode pipeline |

## Running

```sh
# Install the runner (one-time):
cargo install cargo-fuzz

# Run a target. Add `--release` for proper coverage.
cd crates/epics-pva-rs
cargo +nightly fuzz run fuzz_pva_header --release
```

CI tip: use a short timeout and a small corpus seed so each target
runs ~30s per merge — long enough to catch shallow regressions, short
enough to fit in a per-PR check.

## Coverage rationale

Each target hits a distinct decode pivot:
- header is the very first byte the network can drive
- search-response caps its cid pre-allocation at what the payload can hold
- type-desc has the deepest recursion (Structure inside StructureArray
  inside Union); compound-cycle bugs would surface here
- pv-field is the value path, whose array allocations `safe_capacity` bounds
- op-response stitches header + descriptor + value, so it's the
  end-to-end equivalent of "send a malformed frame to the client"

Panics found here would all be remote-DoS regressions. New targets
should follow the same `Cursor::new` + `let _ = decode(...)` shape so
the fuzzer is allowed to drive every error path without needing setup.
