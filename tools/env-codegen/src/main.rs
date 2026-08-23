//! `configure/` -> Rust generator for `epics-libcom-rs`: the `ENV_PARAM` table and
//! the EPICS Base version.
//!
//! C does not hand-write either of them. Both are generated from the same
//! `configure/` spec:
//!
//! * `envDefs.h` (which parameters exist, and in what order) + `CONFIG_ENV` +
//!   `CONFIG_SITE_ENV` --`bldEnvData.pl`--> `envData.c`, a table of
//!   `ENV_PARAM {name, pdflt}` plus the `env_param_list[]` every
//!   `envGet*ConfigParam` and `epicsPrtEnvParams` walks.
//! * `CONFIG_BASE_VERSION` --`makeEpicsVersion.pl`--> `epicsVersion.h`, the
//!   `EPICS_VERSION_*` macros every tool banner and `iocshRegisterCommon`
//!   environment variable is built from.
//!
//! This generator is the same pair of transforms, emitting Rust instead of C.
//! Its output is the ONLY place an EPICS environment default or an EPICS Base
//! version number is written down in this workspace: `EnvParam` has no public
//! constructor, and the accessors that resolve a value take no `default`
//! argument, so a caller cannot introduce a second one.
//!
//! ```text
//! cargo run -p env-codegen -- --write    # regenerate the checked-in tables
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
const OUT_FILE: &str = "crates/epics-libcom-rs/src/runtime/env_table.rs";
const OUT_VERSION: &str = "crates/epics-libcom-rs/src/runtime/version.rs";

/// The three parameters `bldEnvData.pl` does NOT read from the config files:
/// C's Makefile passes them on the command line (`-c`, `-s`, `-t`) so they
/// describe the toolchain that built libCom. The generated table references the
/// hand-written `build_info` consts, which describe the toolchain that
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

    let mut stale = false;
    for (file, generated) in generated {
        let out_path = root.join(file);
        if write {
            if let Err(e) = std::fs::write(&out_path, &generated) {
                eprintln!("env-codegen: {}: {e}", out_path.display());
                return ExitCode::FAILURE;
            }
            eprintln!("env-codegen: wrote {}", out_path.display());
            continue;
        }

        let current = std::fs::read_to_string(&out_path).unwrap_or_default();
        if current == generated {
            eprintln!("env-codegen: {} is up to date", out_path.display());
            continue;
        }
        stale = true;
        eprintln!(
            "env-codegen: {} is STALE — it does not match the vendored configure/ files.\n\
             Re-run `cargo run -p env-codegen -- --write` and commit the result.\n\
             {}",
            out_path.display(),
            first_difference(&current, &generated)
        );
    }

    if stale {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// The line the reader has to look at, not "the files differ".
fn first_difference(have: &str, want: &str) -> String {
    have.lines()
        .zip(want.lines())
        .enumerate()
        .find(|(_, (a, b))| a != b)
        .map(|(n, (a, b))| {
            format!(
                "  first difference at line {}:\n    have: {a}\n    want: {b}",
                n + 1
            )
        })
        .unwrap_or_else(|| {
            format!(
                "  length differs: {} vs {} lines",
                have.lines().count(),
                want.lines().count()
            )
        })
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

/// Every generated file, as `(path relative to the workspace root, contents)`.
fn generate(dir: &Path) -> Result<Vec<(&'static str, String)>, String> {
    Ok(vec![
        (OUT_FILE, generate_env_table(dir)?),
        (OUT_VERSION, generate_version(dir)?),
    ])
}

fn generate_env_table(dir: &Path) -> Result<String, String> {
    let defs = read(&dir.join("envDefs.h"))?;
    let names = parse_env_defs(&defs);
    if names.is_empty() {
        return Err("envDefs.h declared no ENV_PARAM".into());
    }

    let mut values: BTreeMap<String, String> = BTreeMap::new();
    for cfg in ["CONFIG_ENV", "CONFIG_SITE_ENV"] {
        let text = read(&dir.join(cfg))?;
        read_release(&text, cfg, &mut values)?;
    }
    expand_release(&mut values)?;

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
    /// An `epics_libcom_rs::runtime::build_info` const name.
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

/// C `$MVAR` (`Release.pm:10`), `[A-Za-z_][A-Za-z_0-9-]*` — the length of the
/// variable name at the head of `s`, or 0 when there is none. Note the
/// hyphen, which C admits and a plain "alphanumeric or underscore" test does
/// not, and the leading character, which C forbids from being a digit.
fn mvar_len(s: &str) -> usize {
    let mut chars = s.char_indices();
    match chars.next() {
        Some((_, c)) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return 0,
    }
    for (i, c) in chars {
        if !(c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return i;
        }
    }
    s.len()
}

/// C's `m/ (.*) \$\( ($MVAR) \) (.*) /x` (`Release.pm:111`). Both `(.*)` are
/// greedy, so the match is the RIGHTMOST `$(VAR)` in the string; returns
/// `(before, name, after)`.
fn split_macro_ref(s: &str) -> Option<(String, String, String)> {
    let mut found = None;
    let mut from = 0;
    while let Some(i) = s[from..].find("$(") {
        let start = from + i;
        let n = mvar_len(&s[start + 2..]);
        if n > 0 && s[start + 2 + n..].starts_with(')') {
            found = Some((start, n));
        }
        from = start + 2;
    }
    let (start, n) = found?;
    Some((
        s[..start].to_string(),
        s[start + 2..start + 2 + n].to_string(),
        s[start + 3 + n..].to_string(),
    ))
}

/// C `EPICS::Release::expandMacros` (`Release.pm:105-115`): substitute
/// `$(VAR)` until one is undefined, and leave that one as written. This is
/// the expansion a `:=` assignment performs on its own value, at the moment
/// it is read.
fn expand_macros(s: &str, macros: &BTreeMap<String, String>) -> String {
    let mut out = s.to_string();
    while let Some((pre, var, post)) = split_macro_ref(&out) {
        let Some(val) = macros.get(&var) else { break };
        out = format!("{pre}{val}{post}");
    }
    out
}

/// C `EPICS::Release::expandRelease` (`Release.pm:119-137`): substitute
/// `$(VAR)` throughout every value once all the files have been read. An
/// undefined variable warns and expands to nothing — C's `$Rmacros->{$var}`
/// is `undef` there — and a definition that reaches itself is fatal. Any
/// cycle terminates on that second rule, because going round it substitutes
/// the variable being expanded.
fn expand_release(macros: &mut BTreeMap<String, String>) -> Result<(), String> {
    for name in macros.keys().cloned().collect::<Vec<_>>() {
        let mut val = macros[&name].clone();
        while let Some((pre, var, post)) = split_macro_ref(&val) {
            if var == name {
                return Err(format!("Circular definition of variable {var}"));
            }
            let sub = match macros.get(&var) {
                Some(v) => v.clone(),
                None => {
                    eprintln!("env-codegen: Undefined variable $({var}) used");
                    String::new()
                }
            };
            val = format!("{pre}{sub}{post}");
            macros.insert(name.clone(), val.clone());
        }
    }
    Ok(())
}

/// C `EPICS::Release::readRelease` (`Release.pm:44-99`), applied to `macros`
/// so that a later file overrides an earlier one exactly as C's shared
/// `%values` hash does.
///
/// The per-line pipeline is C's, in C's order: strip a trailing CR, strip
/// leading whitespace, then strip everything from the first `#` to the end of
/// the line, then strip trailing whitespace. That comment rule is why
/// `EPICS_CA_ADDR_LIST = 10.0.0.1  # site gateway` is one address and not
/// three, and it is also what empties a whole-line comment, so there is no
/// separate rule for those.
///
/// An assignment is `NAME`, then `=`, `?=` or `:=`, then the value: `?=`
/// keeps an existing definition, `:=` expands `$(VAR)` in its value there and
/// then. A line matching none of C's three forms is an error here, where C
/// ignores it: these two files exist to be site-edited, and a setting that
/// vanishes without a word is the failure this refuses to have.
fn read_release(
    text: &str,
    file: &str,
    macros: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim_end_matches('\r').trim_start();
        let line = match line.find('#') {
            Some(i) => &line[..i],
            None => line,
        }
        .trim_end();
        if line.is_empty() {
            continue;
        }

        // `undefine <var>` (`Release.pm:66-72`). C requires whitespace and a
        // name after the keyword; without both the line falls through to the
        // matchers below, so this does too.
        if let Some(rest) = line.strip_prefix("undefine")
            && rest.starts_with(char::is_whitespace)
        {
            let rest = rest.trim_start();
            let len = mvar_len(rest);
            if len > 0 {
                macros.remove(&rest[..len]);
                continue;
            }
        }

        // `include <path>` / `-include <path>` (`Release.pm:74-83`). C
        // resolves the path against the caller's working directory; this
        // generator reads a vendored `envconfig/` copy from the workspace
        // root, so there is no directory it could resolve one against without
        // inventing a rule C does not state. Refuse rather than guess — and
        // rather than drop it in silence.
        for op in ["-include", "include"] {
            if let Some(rest) = line.strip_prefix(op)
                && rest.starts_with(char::is_whitespace)
            {
                return Err(format!(
                    "{file}:{}: `{op}` is not supported by the offline generator; \
                     inline the file into the vendored envconfig/ copy",
                    n + 1
                ));
            }
        }

        // `<var> = <value>` and the `?=` / `:=` variants (`Release.pm:85-98`).
        let (name, rest) = line.split_at(mvar_len(line));
        let rest = rest.trim_start();
        let Some((op, value)) = ["?=", ":=", "="]
            .into_iter()
            .find_map(|op| rest.strip_prefix(op).map(|v| (op, v)))
        else {
            return Err(format!(
                "{file}:{}: not a comment, an `undefine`, or a `NAME =/?=/:= value` \
                 assignment: `{line}`",
                n + 1
            ));
        };

        // C folds every INSTALL_LOCATION* spelling onto TOP (`Release.pm:88`).
        let name = if name.starts_with("INSTALL_LOCATION") {
            "TOP"
        } else {
            name
        };
        if op == "?=" && macros.contains_key(name) {
            continue;
        }
        let value = value.trim_start();
        let value = if op == ":=" {
            expand_macros(value, macros)
        } else {
            value.to_string()
        };
        macros.insert(name.to_string(), value);
    }
    Ok(())
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
         //! default. [`EnvParam`] cannot be constructed outside\n\
         //! `epics-libcom-rs`, and none of its accessors take a `default` argument, so a\n\
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

/// C `makeEpicsVersion.pl`'s five variables, straight out of
/// `configure/CONFIG_BASE_VERSION`, plus the site string C's Makefile passes in
/// as `-v $(EPICS_SITE_VERSION)` (empty unless a site sets it in `CONFIG_SITE`).
struct BaseVersion {
    version: u32,
    revision: u32,
    modification: u32,
    patch_level: u32,
    snapshot: String,
    site: String,
}

impl BaseVersion {
    /// `makeEpicsVersion.pl:51-55`. The patch level is part of the number only
    /// when it is non-zero; the snapshot and site suffixes are appended only
    /// when non-empty.
    fn short(&self) -> String {
        let mut s = format!("{}.{}.{}", self.version, self.revision, self.modification);
        if self.patch_level > 0 {
            s.push_str(&format!(".{}", self.patch_level));
        }
        s
    }

    fn full(&self) -> String {
        let mut s = self.short();
        s.push_str(&self.snapshot);
        if !self.site.is_empty() {
            s.push_str(&format!("-{}", self.site));
        }
        s
    }

    /// `epicsVersion.h`'s `VERSION_INT(V,R,M,P)`.
    fn int(&self) -> u32 {
        (self.version << 24) | (self.revision << 16) | (self.modification << 8) | self.patch_level
    }
}

fn generate_version(dir: &Path) -> Result<String, String> {
    let text = read(&dir.join("CONFIG_BASE_VERSION"))?;
    let v = parse_base_version(&text)?;
    eprintln!("env-codegen: EPICS Base {}", v.full());
    rustfmt(&emit_version(&v))
}

/// `makeEpicsVersion.pl:36-49` — the same five `^NAME\s*=\s*...` scans over
/// `CONFIG_BASE_VERSION`, and the same "missing variable is a hard error".
fn parse_base_version(text: &str) -> Result<BaseVersion, String> {
    let num = |key: &str| -> Option<u32> {
        assignment(text, key).and_then(|v| v.split_whitespace().next()?.parse().ok())
    };
    let version = num("EPICS_VERSION").ok_or("CONFIG_BASE_VERSION: no EPICS_VERSION")?;
    let revision = num("EPICS_REVISION").ok_or("CONFIG_BASE_VERSION: no EPICS_REVISION")?;
    let modification =
        num("EPICS_MODIFICATION").ok_or("CONFIG_BASE_VERSION: no EPICS_MODIFICATION")?;
    let patch_level =
        num("EPICS_PATCH_LEVEL").ok_or("CONFIG_BASE_VERSION: no EPICS_PATCH_LEVEL")?;
    // C captures `([-\w]*)` (`makeEpicsVersion.pl:43`), i.e. the leading run
    // of word characters and hyphens — so `-DEV # bumped` is `-DEV`, and the
    // suffix never carries a trailing comment into `epicsVersion.h`.
    let snapshot: String = assignment(text, "EPICS_DEV_SNAPSHOT")
        .ok_or("CONFIG_BASE_VERSION: no EPICS_DEV_SNAPSHOT")?
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    // C's Makefile passes this in from CONFIG_SITE (`-v`); no site version is
    // vendored here, and the reference build likewise has it empty.
    let site = assignment(text, "EPICS_SITE_VERSION").unwrap_or_default();

    if version == 0 || version > 255 || revision > 255 || modification > 255 || patch_level > 255 {
        return Err(format!(
            "CONFIG_BASE_VERSION: {version}.{revision}.{modification}.{patch_level} does not fit \
             VERSION_INT's one-byte fields"
        ));
    }
    Ok(BaseVersion {
        version,
        revision,
        modification,
        patch_level,
        snapshot,
        site,
    })
}

/// The right-hand side of the first `^KEY = value` line, comments skipped —
/// the Perl `m/^KEY\s*=\s*.../` scan, which never sees a `#` line.
/// C `makeEpicsVersion.pl:36-43` — a different reader from
/// [`read_release`]: it skips whole-line comments only and matches
/// `^KEY\s*=` per key, and each key's own capture group decides where its
/// value stops, so the trimming that belongs to a value happens at the call
/// site rather than here.
fn assignment(text: &str, key: &str) -> Option<String> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .find_map(|l| {
            let rest = l.strip_prefix(key)?;
            let rest = rest.trim_start();
            let rest = rest.strip_prefix('=')?;
            Some(rest.trim().to_string())
        })
}

fn emit_version(v: &BaseVersion) -> String {
    let (short, full, int) = (v.short(), v.full(), v.int());
    format!(
        "//! The EPICS Base release this crate ports — the Rust `epicsVersion.h`.\n\
         //!\n\
         //! @generated by `cargo run -p env-codegen -- --write` from the vendored\n\
         //! `crates/epics-base-rs/envconfig/CONFIG_BASE_VERSION`, exactly as C's\n\
         //! `makeEpicsVersion.pl` generates `epicsVersion.h` from `configure/`.\n\
         //! DO NOT EDIT — bump the spec and regenerate. `env-codegen --check` fails on\n\
         //! drift and runs as part of `cargo nextest run -p env-codegen`.\n\
         //!\n\
         //! These are the EPICS Base version, NOT the `epics-libcom-rs` crate version\n\
         //! (`CARGO_PKG_VERSION`): the crate version tracks the Rust port's own release\n\
         //! cadence, these name the upstream release being ported.\n\
         \n\
         /// C `EPICS_VERSION` — the major number.\n\
         pub const EPICS_VERSION: u32 = {version};\n\
         /// C `EPICS_REVISION`.\n\
         pub const EPICS_REVISION: u32 = {revision};\n\
         /// C `EPICS_MODIFICATION`.\n\
         pub const EPICS_MODIFICATION: u32 = {modification};\n\
         /// C `EPICS_PATCH_LEVEL`.\n\
         pub const EPICS_PATCH_LEVEL: u32 = {patch_level};\n\
         /// C `EPICS_DEV_SNAPSHOT` — `\"-DEV\"` between releases, empty on one.\n\
         pub const EPICS_DEV_SNAPSHOT: &str = {snapshot:?};\n\
         /// C `EPICS_SITE_VERSION` — a site's local version suffix, empty upstream.\n\
         pub const EPICS_SITE_VERSION: &str = {site:?};\n\
         \n\
         /// C `EPICS_VERSION_SHORT` — the release, with no snapshot or site suffix.\n\
         pub const EPICS_VERSION_SHORT: &str = {short:?};\n\
         /// C `EPICS_VERSION_FULL` — short version plus the snapshot and site suffixes.\n\
         pub const EPICS_VERSION_FULL: &str = {full:?};\n\
         /// C `EPICS_VERSION_STRING` — what every tool's `-V` banner prints.\n\
         pub const EPICS_VERSION_STRING: &str = {string:?};\n\
         /// C `epicsReleaseVersion`.\n\
         pub const EPICS_RELEASE_VERSION: &str = {release:?};\n\
         /// C `EPICS_VERSION_INT` — `VERSION_INT(V, R, M, P)`, for numeric compares.\n\
         pub const EPICS_VERSION_INT: u32 = {int:#010x};\n",
        version = v.version,
        revision = v.revision,
        modification = v.modification,
        patch_level = v.patch_level,
        snapshot = v.snapshot,
        site = v.site,
        string = format!("EPICS {full}"),
        release = format!("EPICS R{full}"),
    )
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

#[cfg(test)]
mod tests {
    /// The drift gate — see `dbd-codegen`'s twin.
    ///
    /// A checked-in generated file with no test behind it is a file that can go
    /// stale in silence. `cargo nextest run` executes this, so a `CONFIG_ENV`
    /// edit that nobody regenerated is a failing test rather than a table that
    /// quietly disagrees with its own spec.
    #[test]
    fn generated_files_are_not_stale() {
        let root = super::repo_root().expect("workspace root");
        for (file, want) in
            super::generate(&root.join(super::ENVCONFIG_DIR)).expect("generator run")
        {
            let have = std::fs::read_to_string(root.join(file)).unwrap_or_default();
            assert!(
                have == want,
                "{file} is STALE — it no longer matches the vendored configure/ files.\n\
                 Re-run `cargo run -p env-codegen -- --write` and commit the result.\n{}",
                super::first_difference(&have, &want)
            );
        }
    }

    /// `makeEpicsVersion.pl`'s two conditionals — the patch level drops out of
    /// the number when it is zero, the snapshot suffix when it is empty — are
    /// what make `7.0.10` and `7.0.10.1-DEV` different strings. A generator that
    /// always pastes all four fields prints `7.0.10.0` on a release.
    #[test]
    fn version_strings_match_make_epics_version() {
        let v = super::parse_base_version(
            "EPICS_VERSION = 7\nEPICS_REVISION = 0\nEPICS_MODIFICATION = 10\n\
             EPICS_PATCH_LEVEL = 1\nEPICS_DEV_SNAPSHOT=-DEV\n",
        )
        .expect("parses");
        assert_eq!(v.short(), "7.0.10.1");
        assert_eq!(v.full(), "7.0.10.1-DEV");
        assert_eq!(v.int(), 0x0700_0a01);

        // A zero patch level is not in the version number (`$patch > 0`).
        let rel = super::parse_base_version(
            "EPICS_VERSION = 7\nEPICS_REVISION = 0\nEPICS_MODIFICATION = 10\n\
             EPICS_PATCH_LEVEL = 0\nEPICS_DEV_SNAPSHOT=\n",
        )
        .expect("parses");
        assert_eq!(rel.short(), "7.0.10");
        assert_eq!(rel.full(), "7.0.10");
        assert_eq!(rel.int(), 0x0700_0a00);

        // A site version appends `-SITE` (`$ver_str .= "-$opt_v"`).
        let site = super::parse_base_version(
            "EPICS_VERSION = 7\nEPICS_REVISION = 0\nEPICS_MODIFICATION = 10\n\
             EPICS_PATCH_LEVEL = 1\nEPICS_DEV_SNAPSHOT=-DEV\nEPICS_SITE_VERSION = pls\n",
        )
        .expect("parses");
        assert_eq!(site.full(), "7.0.10.1-DEV-pls");

        // A missing variable is a hard error, not a zero.
        assert!(super::parse_base_version("EPICS_VERSION = 7\n").is_err());
    }

    /// The generator's own transform, on the shapes `bldEnvData.pl` accepts.
    /// `IOCSH_PS1`'s default is a C macro, not a string literal — getting this
    /// wrong is how the prompt silently becomes the literal text
    /// `ANSI_GREEN("epics> ")`.
    #[test]
    fn expand_value_matches_the_c_preprocessor() {
        // Bare word: the script quotes it verbatim.
        assert_eq!(super::expand_value("YES").unwrap(), "YES");
        assert_eq!(super::expand_value("5064").unwrap(), "5064");
        assert_eq!(super::expand_value("").unwrap(), "");
        // Quoted: the quotes are delimiters, not content.
        assert_eq!(super::expand_value("\"\"").unwrap(), "");
        assert_eq!(
            super::expand_value("\"CST6CDT,M3.2.0/2\"").unwrap(),
            "CST6CDT,M3.2.0/2"
        );
        // Adjacent string literals concatenate, as in C.
        assert_eq!(super::expand_value("\"a\" \"b\"").unwrap(), "ab");
        // ANSI_COLOR(STR) == ANSI_ESC_COLOR STR ANSI_ESC_RESET (errlog.h:290-297).
        assert_eq!(
            super::expand_value("ANSI_GREEN(\"epics> \")").unwrap(),
            "\x1b[32;1mepics> \x1b[0m"
        );
        assert_eq!(super::expand_value("ANSI_ESC_RED").unwrap(), "\x1b[31;1m");
        // An unknown macro is a hard error, not a silently pasted literal.
        assert!(super::expand_value("ANSI_TEAL(\"x\")").is_err());
    }

    fn read(text: &str) -> std::collections::BTreeMap<String, String> {
        let mut m = std::collections::BTreeMap::new();
        super::read_release(text, "CONFIG_SITE_ENV", &mut m).expect("parses");
        m
    }

    /// C strips `#` to end-of-line from ANYWHERE in the line
    /// (`Release.pm:63`), not just from column zero. Keeping the comment puts
    /// `#`, `site` and `gateway` into the CA address list every client and
    /// server in the workspace then tries to resolve.
    #[test]
    fn a_trailing_comment_is_not_part_of_the_value() {
        let m = read("EPICS_CA_ADDR_LIST = 10.0.0.1  # site gateway\n");
        assert_eq!(m["EPICS_CA_ADDR_LIST"], "10.0.0.1");
        // The whole-line case falls out of the same rule.
        assert!(read("# EPICS_TZ = \"JST-9\"\n").is_empty());
    }

    /// C's assignment operator is `([?:]?=)` (`Release.pm:87`). A site that
    /// writes the `?=` spelling — the one that means "unless already set" —
    /// had its whole line dropped, so the parameter silently kept
    /// `CONFIG_ENV`'s value with nothing said.
    #[test]
    fn conditional_and_immediate_assignment_are_both_accepted() {
        // `?=` defines when unset...
        let m = read("EPICS_CA_ADDR_LIST ?= 10.0.0.1\n");
        assert_eq!(m["EPICS_CA_ADDR_LIST"], "10.0.0.1");
        // ...and keeps the earlier definition when set (Release.pm:91).
        let m = read("EPICS_CA_ADDR_LIST = 10.0.0.1\nEPICS_CA_ADDR_LIST ?= 10.0.0.2\n");
        assert_eq!(m["EPICS_CA_ADDR_LIST"], "10.0.0.1");
        // `:=` expands its value at the point of assignment (Release.pm:95).
        let m = read("HOST = 10.0.0.1\nEPICS_CA_ADDR_LIST := $(HOST)\nHOST = 10.0.0.9\n");
        assert_eq!(m["EPICS_CA_ADDR_LIST"], "10.0.0.1");
    }

    /// `expandRelease` (`Release.pm:119-137`) runs over the finished table, so
    /// a `=` value picks up a definition that appears after it.
    #[test]
    fn dollar_paren_references_expand_after_every_file_is_read() {
        let mut m = read("EPICS_CA_ADDR_LIST = $(HOST)\nHOST = 10.0.0.9\n");
        super::expand_release(&mut m).expect("expands");
        assert_eq!(m["EPICS_CA_ADDR_LIST"], "10.0.0.9");

        // A definition that reaches itself is fatal in C, and so a cycle
        // terminates rather than spinning.
        let mut cycle = read("A = $(B)\nB = $(A)\n");
        let err = super::expand_release(&mut cycle).expect_err("cycle must be refused");
        assert!(err.contains("Circular definition"), "{err}");
    }

    /// The silent skip is the worst half of this: a site operator editing the
    /// file that exists to be edited must not get silence.
    #[test]
    fn a_line_that_matches_none_of_cs_forms_is_an_error() {
        let mut m = std::collections::BTreeMap::new();
        let err = super::read_release("EPICS_CA_ADDR_LIST 10.0.0.1\n", "CONFIG_SITE_ENV", &mut m)
            .expect_err("an unparseable line must be reported");
        assert!(err.contains("CONFIG_SITE_ENV:1"), "{err}");
        assert!(err.contains("EPICS_CA_ADDR_LIST 10.0.0.1"), "{err}");
    }

    /// `undefine` deletes the macro (`Release.pm:66-72`); C's `$MVAR` admits a
    /// hyphen and forbids a leading digit (`Release.pm:10`).
    #[test]
    fn undefine_and_the_c_variable_charset() {
        let m = read("EPICS_TZ = \"JST-9\"\nundefine EPICS_TZ\n");
        assert!(!m.contains_key("EPICS_TZ"));
        let m = read("MY-VAR = 1\n");
        assert_eq!(m["MY-VAR"], "1");
        let mut bad = std::collections::BTreeMap::new();
        assert!(super::read_release("9VAR = 1\n", "CONFIG_ENV", &mut bad).is_err());
    }

    /// C's `EPICS_DEV_SNAPSHOT` capture is `([-\w]*)`
    /// (`makeEpicsVersion.pl:43`), which stops at the space before a comment.
    /// Carrying it through would put `# bumped` inside the version string
    /// every tool banner prints.
    #[test]
    fn dev_snapshot_stops_where_cs_capture_stops() {
        let v = super::parse_base_version(
            "EPICS_VERSION = 7\nEPICS_REVISION = 0\nEPICS_MODIFICATION = 10\n\
             EPICS_PATCH_LEVEL = 1\nEPICS_DEV_SNAPSHOT=-DEV # bumped after the release\n",
        )
        .expect("parses");
        assert_eq!(v.full(), "7.0.10.1-DEV");
    }
}
