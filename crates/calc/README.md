# epics-calc-rs

Pure Rust implementation of the [EPICS calc engine](https://github.com/epics-modules/calc) — expression compiler and evaluator supporting numeric, string, and array calculations.

No C dependencies. Just `cargo build`.

## Features

### Expression Engine

- Infix-to-postfix compilation with operator precedence
- Numeric evaluation: arithmetic, comparison, logic, ternary, bitwise, math functions
- String evaluation: string manipulation, concatenation, comparison
- Array evaluation: element-wise operations, statistics
- 16 input variables (A–P) + 12 string/array variables (AA–LL)
- Expression checksum validation

### Math Functions (optional)

- Statistics: mean, sigma, min, max, median
- Numerical derivatives
- Curve fitting
- Interpolation

### Record Types (optional, with `epics` feature)

- Transform record
- String calc output (scalcout) record
- String sequencer (sseq) record

## Cargo Features

| Feature | Default | Description |
|---------|---------|-------------|
| `numeric` | yes | Numeric expression engine |
| `string` | no | String expression support |
| `array` | no | Array expression support |
| `math` | no | Statistics, fitting, interpolation, derivatives |
| `epics` | no | EPICS record implementations (includes string + array + epics-base-rs) |

## Quick Start

```rust
use calc_rs::engine::{compile, eval_numeric, NumericInputs};

let expr = compile("A+B*C")?;
let mut inputs = NumericInputs::default();
inputs[0] = 1.0;  // A
inputs[1] = 2.0;  // B
inputs[2] = 3.0;  // C
let result = eval_numeric(&expr, &mut inputs)?;
assert_eq!(result, 7.0);
```

## Architecture

```
calc-rs/
  src/
    lib.rs                # Public API, feature gates
    engine/
      token.rs            # Lexical tokenization
      postfix.rs          # Infix-to-postfix compiler
      opcodes.rs          # VM instruction definitions
      numeric.rs          # Numeric evaluator
      string.rs           # String evaluator
      array.rs            # Array evaluator
      value.rs            # Stack value types (strings)
      array_value.rs      # Stack value types (arrays)
      checksum.rs         # Expression checksum
      error.rs            # Error types
    math/
      stats.rs            # Statistical functions
      derivative.rs       # Numerical derivatives
      fitting.rs          # Curve fitting
      interp.rs           # Interpolation
    record/
      transform.rs        # Transform record
      scalcout.rs         # String calc output record
      sseq.rs             # String sequencer record
  opi/
    medm/                 # MEDM .adl screens (from C++ calc)
    pydm/                 # PyDM .ui screens (converted via adl2pydm)
```

## Testing

```bash
cargo test --all-features
```

110 tests covering expression compilation, numeric/string/array evaluation, math functions, and record processing.

## Dependencies

- epics-base-rs (optional) — Record trait for EPICS record support

## Requirements

- Rust 1.70+

## License

The Rust code authored in this crate is licensed under MIT.

This crate also bundles third-party OPI/UI assets related to EPICS calc and
synApps modules. See [`THIRD_PARTY_LICENSES`](THIRD_PARTY_LICENSES) for
attribution and upstream license text.
