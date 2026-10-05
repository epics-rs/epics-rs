# env-codegen

Offline generator. Turns the vendored EPICS `configure/` spec into the two Rust
files `epics-libcom-rs` serves EPICS environment defaults and the Base version
number from. Dev-only, `publish = false`, not a workspace dependency of
anything.

## Why generated and not written

C does not hand-write either file, and both come from the same `configure/`
directory:

- `envDefs.h` (which parameters exist, and in what order) + `CONFIG_ENV` +
  `CONFIG_SITE_ENV` —`bldEnvData.pl`→ `envData.c`: a table of
  `ENV_PARAM {name, pdflt}` plus the `env_param_list[]` that every
  `envGet*ConfigParam` and `epicsPrtEnvParams` walks.
- `CONFIG_BASE_VERSION` —`makeEpicsVersion.pl`→ `epicsVersion.h`: the
  `EPICS_VERSION_*` macros behind every tool banner and the
  `iocshRegisterCommon` environment variables.

This generator is the same pair of transforms, emitting Rust. Its output is the
**only** place an EPICS environment default or a Base version number is written
down in this workspace — `EnvParam` has no public constructor and the accessors
that resolve a value take no `default` argument, so a caller cannot introduce a
second one.

```
crates/epics-base-rs/envconfig/      the vendored configure/ spec (input)
  → crates/epics-libcom-rs/src/runtime/env_table.rs
  → crates/epics-libcom-rs/src/runtime/version.rs
```

Three parameters are not read from the config files at all:
`EPICS_BUILD_COMPILER_CLASS`, `EPICS_BUILD_OS_CLASS` and
`EPICS_BUILD_TARGET_ARCH`. C's makefile passes them on `bldEnvData.pl`'s command
line (`-c`, `-s`, `-t`), so they describe the toolchain that built libCom; the
generated table points them at the hand-written `build_info` consts, which
describe the toolchain that built *this* crate.

## Running

```bash
cargo run -p env-codegen -- --write    # regenerate the checked-in tables
cargo run -p env-codegen -- --check    # fail on drift
```

It is offline in both directions: it reads the `configure/` files vendored into
the repository, and its output is checked in, so neither the build nor CI needs
an EPICS installation.

## Testing

```bash
cargo nextest run -p env-codegen
```

9 tests. `generated_files_are_not_stale` is the drift gate — it runs the
generator and compares against what is checked in, so an edited `env_table.rs`
or a bumped spec that was never regenerated fails here rather than at the next
reader. The rest pin the transforms against their Perl originals: the version
strings against `makeEpicsVersion.pl`, value expansion against the C
preprocessor, and a trailing `#` comment being delimiter rather than value.

## License

EPICS Open License (workspace `LICENSE`).
