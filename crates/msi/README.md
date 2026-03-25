# msi-rs

Rust port of EPICS [msi](https://epics-controls.org/) (Macro Substitution and Include) — converts `.template` files to `.db` files by expanding macros and processing include/substitute directives.

No EPICS dependencies. Pure text processing.

Output is identical to C msi for ADCore/ADSimDetector templates (verified by golden tests).

**Repository:** <https://github.com/epics-rs/epics-rs>

## CLI

```bash
# Build the CLI
cargo build -p msi-rs --features cli

# Expand template with macros
msi-rs -M "P=IOC:,R=ai1" template.template

# Use substitution file
msi-rs -S subst.substitutions

# With include paths and output file
msi-rs -I ./includes -M "P=IOC:" template.template -o output.db

# Strict mode: fail on undefined macros
msi-rs -V -M "P=IOC:" template.template
```

### Options

| Flag | Description |
|------|-------------|
| `-M A=val,B=val` | Macro definitions (comma-separated) |
| `-S FILE` | Substitution file |
| `-I DIR` | Include search directory (repeatable) |
| `-o FILE` | Output file (default: stdout) |
| `-V` | Strict mode: report undefined macros as errors |

## Library API

```rust
use std::path::Path;
use msi_rs::{expand_template, MacHandle, TemplateProcessor};

// Simple: expand template with macros
let output = expand_template(
    Path::new("template.template"),
    &[("P", "IOC:"), ("R", "ai1")],
    &[],  // include paths
)?;

// Advanced: use MacHandle directly
let mut mac = MacHandle::new();
mac.install_macros("P=IOC:,R=ai1");
let expanded = mac.expand_string("$(P)$(R)");
```

## Macro Syntax

| Syntax | Description |
|--------|-------------|
| `$(NAME)` | Basic macro reference |
| `${NAME}` | Brace variant (equivalent) |
| `$(NAME=default)` | Default value if undefined |
| `$(NAME,SUB=val)` | Scoped macro definition |
| `$(A$(B))` | Nested macro reference |
| `'$(NAME)'` | Single-quote suppression |
| `\$(NAME)` | Backslash escape |

## Template Directives

```
include "path/to/file.template"
substitute "A=val,B=val2"
```

- Include paths are resolved using `-I` directories
- Maximum include depth: 20 (prevents infinite loops)

## Substitution File Format

```
file "template.template" {
    # Pattern block: column names + value rows
    pattern { P, R }
    { "IOC:", "ai1" }
    { "IOC:", "ai2" }
}

# Global macros apply to all subsequent sets
global { SCAN = "1 second" }

# Regular block: key=value pairs
{ P = "IOC:", R = "ao1" }
```

## Build

```bash
cargo build -p msi-rs                  # library only
cargo build -p msi-rs --features cli   # library + CLI binary
cargo test -p msi-rs                   # 57 tests
```

## License

MIT
