//! `.dbd` -> Rust field-table generator for `epics-base-rs`.
//!
//! The EPICS `.dbd` record declarations are the machine-readable spec for every
//! record field: its name, `DBF_*` type, `special()` dispatch code, `pp`, `asl`,
//! `size`, `menu()` and `initial()`. The port used to hand-copy that spec into
//! 1,174 `FieldDesc` literals; this generator derives them instead, so a
//! transcription error is no longer representable.
//!
//! ```text
//! cargo run -p dbd-codegen -- --write    # regenerate the checked-in table
//! cargo run -p dbd-codegen -- --check    # fail on drift (CI)
//! ```
//!
//! The generator is offline: it reads the `.dbd` files **vendored into the
//! repository** at `crates/epics-base-rs/dbd/`, and its output is checked in, so
//! neither the build nor CI depends on an EPICS installation.

mod emit;
mod parse;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const DBD_DIR: &str = "crates/epics-base-rs/dbd";
const OUT_FILE: &str = "crates/epics-base-rs/src/server/record/dbd_generated.rs";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let check = args.iter().any(|a| a == "--check");
    let write = args.iter().any(|a| a == "--write");
    if check == write {
        eprintln!("usage: dbd-codegen (--write | --check)");
        return ExitCode::from(2);
    }

    let root = match repo_root() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("dbd-codegen: {e}");
            return ExitCode::FAILURE;
        }
    };

    let generated = match generate(&root.join(DBD_DIR)) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("dbd-codegen: {e}");
            return ExitCode::FAILURE;
        }
    };

    let out_path = root.join(OUT_FILE);
    if write {
        if let Err(e) = std::fs::write(&out_path, &generated) {
            eprintln!("dbd-codegen: {}: {e}", out_path.display());
            return ExitCode::FAILURE;
        }
        eprintln!("dbd-codegen: wrote {}", out_path.display());
        return ExitCode::SUCCESS;
    }

    // --check
    let current = std::fs::read_to_string(&out_path).unwrap_or_default();
    if current == generated {
        eprintln!("dbd-codegen: {} is up to date", out_path.display());
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "dbd-codegen: {} is STALE — it does not match the vendored .dbd files.\n\
             Re-run `cargo run -p dbd-codegen -- --write` and commit the result.",
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

/// Walk up from the manifest dir to the workspace root (the directory whose
/// `Cargo.toml` declares `[workspace]`), so the tool works from any cwd.
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

fn generate(dbd_dir: &Path) -> Result<String, String> {
    let common = parse::parse_db_common(&dbd_dir.join("dbCommon.dbd"))?;

    let cvt_path = dbd_dir.join("cvt_dbaddr.types");
    let cvt = emit::parse_cvt_dbaddr(
        &std::fs::read_to_string(&cvt_path).map_err(|e| format!("{}: {e}", cvt_path.display()))?,
    )?;

    let mut paths: Vec<PathBuf> = std::fs::read_dir(dbd_dir)
        .map_err(|e| format!("{}: {e}", dbd_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "dbd"))
        .filter(|p| p.file_name().is_some_and(|n| n != "dbCommon.dbd"))
        .collect();
    paths.sort();

    let mut menus: BTreeMap<String, parse::Menu> = BTreeMap::new();
    let mut records: Vec<parse::RecordType> = Vec::new();
    for path in &paths {
        let dbd = parse::parse_file(path, &common)?;
        for (name, menu) in dbd.menus {
            if let Some(prev) = menus.insert(name.clone(), menu) {
                // Two `.dbd` files declaring the same menu name with different
                // choices would silently mislabel one record's enum, so the
                // generator refuses rather than picking a winner.
                let now = &menus[&name];
                if prev.choices != now.choices {
                    return Err(format!(
                        "menu({name}) is declared twice with different choices: \
                         {:?} vs {:?}",
                        prev.choices, now.choices
                    ));
                }
            }
        }
        records.extend(dbd.records);
    }
    records.sort_by(|a, b| a.name.cmp(&b.name));

    let own = || {
        records
            .iter()
            .flat_map(|r| r.fields.iter())
            .filter(|f| !f.from_common)
    };
    let internal = own().filter(|f| emit::is_internal(f)).count();
    let emitted = own().filter(|f| !emit::is_internal(f)).count();
    let dbaddr = own()
        .filter(|f| f.special.as_deref() == Some("SPC_DBADDR"))
        .count();
    eprintln!(
        "dbd-codegen: {} record types, {emitted} own fields emitted \
         ({dbaddr} typed from cvt_dbaddr.types), {internal} DBF_NOACCESS internals \
         dropped, {} menus",
        records.len(),
        menus.len()
    );

    let src = emit::emit(&emit::Input {
        menus: &menus,
        records: &records,
        common: &common,
        cvt: &cvt,
    })?;

    // `cargo fmt --all` is a mandatory gate and it does not skip generated
    // files, so raw emitter output would be reformatted the moment anyone runs
    // it — and `--check` would then report the file as stale forever, on a
    // perfectly current checkout. Formatting here makes the generator's output
    // the fixed point rustfmt already agrees with, so `--check` answers the
    // question it is meant to answer ("is this file current with the .dbd?")
    // rather than "has anyone run cargo fmt since?".
    rustfmt(&src)
}

/// Pipe the emitted source through the toolchain's `rustfmt`.
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
