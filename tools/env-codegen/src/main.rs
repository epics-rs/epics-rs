//! `CONFIG_ENV` -> Rust `ENV_PARAM` table generator for `epics-base-rs`.
//!
//! C does not hand-write its environment-parameter defaults either. The spec is
//! three files — `modules/libcom/src/env/envDefs.h` (which parameters exist, and
//! in what order), `configure/CONFIG_ENV` and `configure/CONFIG_SITE_ENV` (what
//! each one defaults to) — and `bldEnvData.pl` turns them into `envData.c`, a
//! table of `ENV_PARAM {name, pdflt}` plus the `env_param_list[]` every
//! `envGet*ConfigParam` and `epicsPrtEnvParams` walks.
//!
//! This generator is the same transform, emitting Rust instead of C. Its output
//! is the ONLY place an EPICS environment default is written down in this
//! workspace: `EnvParam` has no public constructor, and the accessors that
//! resolve a value take no `default` argument, so a caller cannot introduce a
//! second one.
//!
//! ```text
//! cargo run -p env-codegen -- --write    # regenerate the checked-in table
//! cargo run -p env-codegen -- --check    # fail on drift (the in-tree gate test)
//! ```
//!
//! Offline: it reads the `configure/` files **vendored into the repository** at
//! `crates/epics-base-rs/envconfig/`, and its output is checked in, so neither
//! the build nor CI depends on an EPICS installation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const ENVCONFIG_DIR: &str = "crates/epics-base-rs/envconfig";
const OUT_FILE: &str = "crates/epics-base-rs/src/runtime/env_table.rs";

/// The three parameters `bldEnvData.pl` does NOT read from the config files:
/// C's Makefile passes them on the command line (`-c`, `-s`, `-t`) so they
/// describe the toolchain that built libCom. The generated table references the
/// hand-written `runtime::build_info` consts, which describe the toolchain that
/// built *this* crate.
const BUILD_SUPPLIED: [(&str, &str); 3] = [
    ("EPICS_BUILD_COMPILER_CLASS", "COMPILER_CLASS"),
    ("EPICS_BUILD_OS_CLASS", "OS_CLASS"),
    ("EPICS_BUILD_TARGET_ARCH", "TARGET_ARCH"),
];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let check = args.iter().any(|a| a == "--check");
    let write = args.iter().any(|a| a == "--write");
    if check == write {
        eprintln!("usage: env-codegen (--write | --check)");
        return ExitCode::from(2);
    }

    let root = match repo_root() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("env-codegen: {e}");
            return ExitCode::FAILURE;
        }
    };

    let generated = match generate(&root.join(ENVCONFIG_DIR)) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("env-codegen: {e}");
            return ExitCode::FAILURE;
        }
    };

    let out_path = root.join(OUT_FILE);
    if write {
        if let Err(e) = std::fs::write(&out_path, &generated) {
            eprintln!("env-codegen: {}: {e}", out_path.display());
            return ExitCode::FAILURE;
        }
        eprintln!("env-codegen: wrote {}", out_path.display());
        return ExitCode::SUCCESS;
    }

    let current = std::fs::read_to_string(&out_path).unwrap_or_default();
    if current == generated {
        eprintln!("env-codegen: {} is up to date", out_path.display());
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "env-codegen: {} is STALE — it does not match the vendored configure/ files.\n\
             Re-run `cargo run -p env-codegen -- --write` and commit the result.",
            out_path.display()
        );
        for (n, (a, b)) in current.lines().zip(generated.lines()).enumerate() {
            if a != b {
                eprintln!(
                    "  first difference at line {}:\n    have: {a}\n    want: {b}",
                    n + 1
                );
                break;
            }
        }
        ExitCode::FAILURE
    }
}

/// Walk up from the manifest dir to the workspace root, so the tool works from
/// any cwd.
fn repo_root() -> Result<PathBuf, String> {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        let manifest = dir.join("Cargo.toml");
        if manifest.exists()
            && std::fs::read_to_string(&manifest)
                .map(|s| s.contains("[workspace]"))
                .unwrap_or(false)
        {
            return Ok(dir);
        }
        if !dir.pop() {
            return Err("could not find the workspace root".into());
        }
    }
}

fn generate(dir: &Path) -> Result<String, String> {
    let defs = read(&dir.join("envDefs.h"))?;
    let names = parse_env_defs(&defs);
    if names.is_empty() {
        return Err("envDefs.h declared no ENV_PARAM".into());
    }

    let mut values: BTreeMap<String, String> = BTreeMap::new();
    for cfg in ["CONFIG_ENV", "CONFIG_SITE_ENV"] {
        let text = read(&dir.join(cfg))?;
        for (k, v) in parse_config(&text) {
            values.insert(k, v);
        }
    }

    let mut rows = Vec::with_capacity(names.len());
    for name in &names {
        if let Some((_, konst)) = BUILD_SUPPLIED.iter().find(|(n, _)| n == name) {
            rows.push((name.clone(), Value::BuildConst(konst)));
            continue;
        }
        let raw = values.get(name).map(String::as_str).unwrap_or("");
        let expanded =
            expand_value(raw).map_err(|e| format!("parameter {name}: {e} (value was `{raw}`)"))?;
        rows.push((name.clone(), Value::Literal(expanded)));
    }

    eprintln!("env-codegen: {} ENV_PARAM rows", rows.len());
    rustfmt(&emit(&rows))
}

fn read(p: &Path) -> Result<String, String> {
    std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))
}

enum Value {
    Literal(String),
    /// A `runtime::build_info` const name.
    BuildConst(&'static str),
}

/// C `bldEnvData.pl` reads the declaration order out of `envDefs.h`, looking for
/// `LIBCOM_API extern const ENV_PARAM <name>;`. That order is what
/// `env_param_list[]` — and therefore `epicsPrtEnvParams` — prints in.
fn parse_env_defs(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("LIBCOM_API extern const ENV_PARAM ") else {
            continue;
        };
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        // `env_param_list` is declared as a pointer array, not an ENV_PARAM.
        if !name.is_empty() && rest[name.len()..].trim_start().starts_with(';') {
            names.push(name);
        }
    }
    names
}

/// C `EPICS::Release::readRelease`: skip comment lines, take `NAME = value`,
/// later files override earlier ones.
fn parse_config(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.trim_start().starts_with('#') {
            continue;
        }
        let Some((lhs, rhs)) = line.split_once('=') else {
            continue;
        };
        let name = lhs.trim();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        out.push((name.to_string(), rhs.trim().to_string()));
    }
    out
}

/// The C preprocessor's view of the value `bldEnvData.pl` pastes into
/// `envData.c`.
///
/// `bldEnvData.pl` accepts three shapes (its `$colored_str` regex):
/// juxtaposed `"quoted"` strings, bare `ANSI_ESC_*` macros, and
/// `ANSI_COLOR("...")` macros — all concatenated, exactly as C's adjacent
/// string-literal rule does. Anything else is a bare word (`YES`, `5064`,
/// `30.0`) which the script wraps in quotes verbatim.
fn expand_value(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(String::new());
    }
    if !(raw.starts_with('"') || raw.starts_with("ANSI_")) {
        return Ok(raw.to_string());
    }

    let bytes: Vec<char> = raw.chars().collect();
    let mut i = 0;
    let mut out = String::new();
    while i < bytes.len() {
        match bytes[i] {
            c if c.is_whitespace() => i += 1,
            '"' => {
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i] != '"' {
                    i += 1;
                }
                if i == bytes.len() {
                    return Err("unterminated string literal".into());
                }
                out.extend(&bytes[start..i]);
                i += 1;
            }
            'A' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == '_') {
                    i += 1;
                }
                let ident: String = bytes[start..i].iter().collect();
                if let Some(color) = ident.strip_prefix("ANSI_ESC_") {
                    out.push_str(ansi_esc(color)?);
                } else if let Some(color) = ident.strip_prefix("ANSI_") {
                    // ANSI_COLOR(STR) == ANSI_ESC_COLOR STR ANSI_ESC_RESET
                    let esc = ansi_esc(color)?;
                    while i < bytes.len() && bytes[i].is_whitespace() {
                        i += 1;
                    }
                    if i == bytes.len() || bytes[i] != '(' {
                        return Err(format!("{ident} is a macro and needs `(...)`"));
                    }
                    i += 1;
                    let inner_start = i;
                    let mut depth = 1;
                    while i < bytes.len() && depth > 0 {
                        match bytes[i] {
                            '(' => depth += 1,
                            ')' => depth -= 1,
                            _ => {}
                        }
                        i += 1;
                    }
                    if depth != 0 {
                        return Err(format!("{ident}: unbalanced `(`"));
                    }
                    let inner: String = bytes[inner_start..i - 1].iter().collect();
                    out.push_str(esc);
                    out.push_str(&expand_value(&inner)?);
                    out.push_str(ansi_esc("RESET")?);
                } else {
                    return Err(format!("unknown macro `{ident}`"));
                }
            }
            c => return Err(format!("unexpected character `{c}`")),
        }
    }
    Ok(out)
}

/// `errlog.h:281-289`.
fn ansi_esc(color: &str) -> Result<&'static str, String> {
    Ok(match color {
        "RED" => "\x1b[31;1m",
        "GREEN" => "\x1b[32;1m",
        "YELLOW" => "\x1b[33;1m",
        "BLUE" => "\x1b[34;1m",
        "MAGENTA" => "\x1b[35;1m",
        "CYAN" => "\x1b[36;1m",
        "BOLD" => "\x1b[1m",
        "UNDERLINE" => "\x1b[4m",
        "RESET" => "\x1b[0m",
        other => return Err(format!("unknown ANSI colour `{other}`")),
    })
}

fn emit(rows: &[(String, Value)]) -> String {
    let mut s = String::new();
    s.push_str(
        "//! EPICS environment parameters — the Rust `envData.c`.\n\
         //!\n\
         //! @generated by `cargo run -p env-codegen -- --write` from the vendored\n\
         //! `crates/epics-base-rs/envconfig/{envDefs.h,CONFIG_ENV,CONFIG_SITE_ENV}`.\n\
         //! DO NOT EDIT — edit the spec and regenerate. `env-codegen --check` fails on\n\
         //! drift and runs as part of `cargo nextest run -p env-codegen`.\n\
         //!\n\
         //! This module is the workspace's ONLY declaration of an EPICS environment\n\
         //! default. [`EnvParam`](super::env::EnvParam) cannot be constructed outside\n\
         //! `epics-base-rs`, and none of its accessors take a `default` argument, so a\n\
         //! caller has no way to introduce a second one.\n\n",
    );
    s.push_str("use super::build_info;\n");
    s.push_str("use super::env::EnvParam;\n\n");

    for (name, value) in rows {
        let (doc, init) = match value {
            Value::Literal(v) if v.is_empty() => (
                "/// Compiled default: empty, so `get()` answers `None` \
                 (C `envGetConfigParamPtr`).\n"
                    .to_string(),
                format!("{v:?}"),
            ),
            Value::Literal(v) => (
                format!("/// Compiled default: `{v:?}`.\n"),
                format!("{v:?}"),
            ),
            Value::BuildConst(k) => (
                format!(
                    "/// Compiled default: the build that produced this binary \
                     (`build_info::{k}`).\n"
                ),
                format!("build_info::{k}"),
            ),
        };
        s.push_str(&doc);
        s.push_str(&format!(
            "pub const {name}: EnvParam = EnvParam::new({name:?}, {init});\n"
        ));
    }

    s.push_str("\n/// C `env_param_list[]` (`envDefs.h:87`) — every parameter EPICS Base knows,\n");
    s.push_str("/// in declaration order. `epicsPrtEnvParams` walks exactly this list.\n");
    s.push_str("pub const ENV_PARAM_LIST: &[EnvParam] = &[\n");
    for (name, _) in rows {
        s.push_str(&format!("    {name},\n"));
    }
    s.push_str("];\n");
    s
}

/// Pipe the emitted source through the toolchain's `rustfmt`, so the checked-in
/// file is already the fixed point `cargo fmt --all` agrees with and `--check`
/// answers "is this current with the spec?" rather than "has anyone run fmt?".
fn rustfmt(src: &str) -> Result<String, String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("rustfmt")
        .args(["--edition", "2024", "--emit", "stdout", "--quiet"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run rustfmt: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("rustfmt: no stdin")?
        .write_all(src.as_bytes())
        .map_err(|e| format!("rustfmt: {e}"))?;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("rustfmt: {e}"))?;
    if !out.status.success() {
        return Err(format!("rustfmt failed: {}", out.status));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("rustfmt: non-utf8 output: {e}"))
}
