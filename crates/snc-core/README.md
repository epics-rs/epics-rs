# snc-core

Compiler library for EPICS [Sequencer](https://epics-controls.org/) Neutral Language (SNL). Compiles `.st` state machine programs into Rust source code targeting the [seq](../seq/) async runtime.

No C dependencies. Pure Rust.

**Repository:** <https://github.com/epics-rs/epics-rs>

## Pipeline

```
Source (.st) → Preprocess → Lex → Parse → Analyze → Codegen → Rust (.rs)
                                    AST        IR
```

| Stage | Module | Description |
|-------|--------|-------------|
| Preprocess | `preprocess.rs` | `#define`, `#undef`, `#ifdef`/`#ifndef` macro expansion |
| Lexer | `lexer.rs` | Tokenization with line/column tracking |
| Parser | `parser.rs` | Token stream → Abstract Syntax Tree |
| Analysis | `analysis.rs` | Semantic analysis, name resolution, type checking → IR |
| Codegen | `codegen.rs` | IR → Rust source with async state machine functions |

## SNL Input

```c
program demo
option +s;

double counter;
assign counter to "{P}counter";
monitor counter;

evflag ef_counter;
sync counter to ef_counter;

ss counter_ss {
    state init {
        when (delay(1.0)) {
            counter = 0.0;
            pvPut(counter);
        } state counting
    }
    state counting {
        when (counter >= 10.0) {} state done
        when (delay(1.0)) {
            counter += 1.0;
            pvPut(counter);
        } state counting
    }
    state done {
        when (delay(0.1)) {} exit
    }
}
```

### Language Constructs

| Construct | Syntax | Description |
|-----------|--------|-------------|
| Variables | `int`, `short`, `long`, `float`, `double`, `string`, `char` | Typed program variables |
| Assign | `assign var to "PV"` | Bind variable to EPICS PV (supports `{MACRO}`) |
| Monitor | `monitor var` | Track PV changes via Channel Access |
| Sync | `sync var to evflag` | Link variable changes to event flag |
| Event flag | `evflag name` | Cross-state-set coordination signal |
| State set | `ss name { ... }` | Concurrent finite state machine |
| State | `state name { ... }` | FSM node with entry/exit blocks |
| Transition | `when (cond) { action } state target` | Condition → action → next state |
| Options | `+s` (safe), `+r` (reentrant), `+m` (main) | Program-level options |

## Rust Output

The codegen produces a complete Rust program:

```rust
use seq::prelude::*;

const CH_COUNTER: usize = 0;
const EF_EF_COUNTER: usize = 0;

struct demoVars { counter: f64 }
impl ProgramVars for demoVars { ... }
impl ProgramMeta for demoMeta { ... }

async fn counter_ss(ctx: StateSetContext) { ... }

#[tokio::main]
async fn main() {
    ProgramBuilder::<demoVars, demoMeta>::new("demo")
        .channel(0, "counter", "{P}counter", true, Some(0))
        .state_set(0, "counter_ss", counter_ss)
        .build()
        .run().await.unwrap();
}
```

## API

```rust
use snc_core::{compile, CompileResult};

let source = std::fs::read_to_string("demo.st")?;
let result: CompileResult = compile(&source)?;
println!("{}", result.code);  // generated Rust source
```

## Build

```bash
cargo build -p snc-core
cargo test -p snc-core
```

## License

MIT
