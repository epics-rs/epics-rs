# sequencer-rs

Rust-native [EPICS sequencer](https://github.com/epics-modules/sequencer) — SNL compiler and runtime for state machine execution with Channel Access I/O.

No C dependencies. Just `cargo build`.

**Repository:** <https://github.com/epics-rs/epics-rs>

## Workspace

| Crate | Description |
|-------|-------------|
| **seq** | Runtime library — executes sequencer programs with async CA I/O |
| **snc-core** | SNL compiler library — lexer, parser, AST, IR, Rust codegen |
| **snc** | CLI compiler tool — `snc` command |

## Features

### Runtime (seq)

- State set execution with async pvGet/pvPut/pvMonitor
- Channel management with CA lifecycle (connect/disconnect)
- Event flag coordination across state sets
- Completion types: Default, Sync, Async
- Program builder API for constructing sequencer programs
- ProgramVars / ProgramMeta traits for generated code interface

### Compiler (snc-core)

- Full SNL syntax lexer and parser
- Abstract Syntax Tree (AST) generation
- Intermediate Representation (IR) lowering
- Semantic analysis and validation
- Rust code generation from IR

## Architecture

```
sequencer-rs/
  seq/
    src/
      lib.rs             # Public API
      channel.rs         # Active CA channel management
      channel_store.rs   # Central channel value storage
      state_set.rs       # State set execution (pvGet/pvPut/pvMonitor)
      event_flag.rs      # Event flag management with wakeup
      program.rs         # ProgramBuilder and ProgramShared
      variables.rs       # ProgramVars, ProgramMeta traits
      macros.rs          # Utility macros
      error.rs           # Error types
  snc-core/
    src/
      lib.rs             # Public API
      lexer.rs           # SNL tokenization
      parser.rs          # Token → AST parsing
      ast.rs             # Abstract syntax tree
      ir.rs              # Intermediate representation
      codegen.rs         # IR → Rust code generation
      analysis.rs        # Semantic analysis
      preprocess.rs      # Preprocessing
      error.rs           # Compiler errors
  snc/
    src/main.rs          # CLI compiler
  examples/
    demo/                # Example sequencer program
  opi/
    medm/                # MEDM .adl screens (from C++ sequencer)
    pydm/                # PyDM .ui screens (converted via adl2pydm)
```

## Quick Start

### Compile SNL to Rust

```bash
snc my_program.st -o my_program.rs
```

### Runtime Usage

```rust
use seq::{ProgramBuilder, StateSetContext};

let program = ProgramBuilder::new("my_seq")
    .channel("temperature", "TEMP:VALUE")
    .event_flag("tempChanged")
    .state_set("monitor", |ctx| async move {
        // State machine logic
    })
    .build();

program.run().await?;
```

## Testing

```bash
cargo test --workspace
```

75 tests covering runtime (channel store, event flags, state sets) and compiler (lexer, parser, analysis, codegen, preprocessing).

## Dependencies

- epics-base-rs — CA client library
- tokio — async runtime
- clap — CLI argument parsing (snc)

## Requirements

- Rust 1.70+
- tokio runtime

## License

MIT
