# busy-rs

Pure Rust implementation of the [EPICS busy record](https://github.com/epics-modules/busy) — a binary output record that tracks asynchronous operation state.

No C dependencies. Just `cargo build`.

## Features

- VAL=1 (busy), VAL=0 (done) semantics
- Forward links (FLNK) fire only on transitions involving 0 (suppressed during 1→1)
- Output mode select (OMSL): Supervisory / Closed Loop
- Invalid output action (IVOA): Continue / Don't Drive / Set to IVOV
- State alarm severity (ZSV, OSV, COSV)
- HIGH timer support for timed busy periods
- RVAL/RBV with mask support
- Full monitoring with MLST tracking

## Architecture

```
busy-rs/
  src/
    lib.rs        # Public API
    record.rs     # BusyRecord — implements Record trait
    types.rs      # Omsl, Ivoa, AlarmSevr enums
  tests/
    busy_test.rs  # Integration tests
  opi/
    medm/         # MEDM .adl screens (from C++ busy)
    pydm/         # PyDM .ui screens (converted via adl2pydm)
```

## Quick Start

```rust
use busy_rs::BusyRecord;

let record = BusyRecord::default();
// VAL=1 → busy, VAL=0 → done (fires FLNK)
```

## Testing

```bash
cargo test
```

28 tests covering process behavior, alarm handling, FLNK semantics, IVOA handling, and state transitions.

## Dependencies

- epics-base-rs — Record trait, EpicsValue

## Requirements

- Rust 1.70+

## License

MIT
