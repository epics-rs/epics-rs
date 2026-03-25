# snc

Command-line SNL (Sequencer Neutral Language) compiler for EPICS. Compiles `.st` state machine files to Rust source code.

Uses [snc-core](../snc-core/) for the compiler pipeline and targets the [seq](../seq/) async runtime.

**Repository:** <https://github.com/epics-rs/epics-rs>

## Usage

```bash
# Compile to file
snc demo.st -o demo.rs

# Output to stdout
snc demo.st

# Debug: dump AST or IR
snc demo.st --dump-ast
snc demo.st --dump-ir -o demo.rs
```

### Options

| Flag | Description |
|------|-------------|
| `<input>` | Input `.st` file (required) |
| `-o, --output <PATH>` | Output `.rs` file (default: stdout) |
| `--dump-ast` | Print AST to stderr |
| `--dump-ir` | Print IR to stderr |

## Build

```bash
cargo build -p snc
cargo install --path crates/snc
```

## Example

```bash
# Compile and build a sequencer program
snc examples/seq-demo/demo.st -o demo_gen.rs
# The generated code uses `seq::prelude::*` and `#[tokio::main]`
```

See [snc-core](../snc-core/) for the compiler internals and SNL language reference.

## License

MIT
