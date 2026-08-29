//! `.dbd` -> Rust field-table generator.
//!
//! The EPICS `.dbd` record declarations are the machine-readable spec for every
//! record field: its name, `DBF_*` type, `special()` dispatch code, `pp`, `asl`,
//! `size`, `menu()` and `initial()`. The port used to hand-copy that spec into
//! `FieldDesc` literals; this generator derives them instead, so a transcription
//! error is no longer representable.
//!
//! ```text
//! cargo run -p dbd-codegen -- --write    # regenerate the checked-in tables
//! cargo run -p dbd-codegen -- --check    # fail on drift (CI)
//! ```
//!
//! The generator is offline: it reads the `.dbd` files **vendored into the
//! repository**, and its output is checked in, so neither the build nor CI
//! depends on an EPICS installation.
//!
//! It serves several crates, not just `epics-base-rs` — see [`targets`]. Each
//! target owns a vendored `.dbd` directory and gets a table generated into
//! itself, under the same drift gate.

mod breaktable;
mod emit;
mod parse;
mod targets;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use targets::{BASE, TARGETS, Target};

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

    let outputs = match generate_all(&root) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("dbd-codegen: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut stale = false;
    for (path, want) in &outputs {
        if write {
            if let Err(e) = std::fs::write(path, want) {
                eprintln!("dbd-codegen: {}: {e}", path.display());
                return ExitCode::FAILURE;
            }
            eprintln!("dbd-codegen: wrote {}", path.display());
            continue;
        }
        let have = std::fs::read_to_string(path).unwrap_or_default();
        if &have == want {
            eprintln!("dbd-codegen: {} is up to date", path.display());
            continue;
        }
        stale = true;
        eprintln!(
            "dbd-codegen: {} is STALE — it does not match the vendored .dbd files.\n\
             Re-run `cargo run -p dbd-codegen -- --write` and commit the result.",
            path.display()
        );
        eprintln!("  {}", first_difference(&have, want));
    }

    if stale {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Every generated file the repository checks in, as (path, wanted content).
///
/// One walk over [`TARGETS`], and every consumer — `--write`, `--check`, and the
/// in-tree drift gate — goes through it, so a new target cannot be added to the
/// generator without also being gated.
fn generate_all(root: &Path) -> Result<Vec<(PathBuf, String)>, String> {
    // There is ONE `dbCommon` declaration in the port, and it is base's: a
    // downstream `.dbd` that says `include "dbCommon.dbd"` means base's.
    let common = parse::parse_db_common(&root.join(BASE.dbd_dir).join("dbCommon.dbd"))?;
    let base_menus = base_menu_paths(root, &common)?;

    let mut outputs = Vec::new();
    for target in TARGETS {
        let external = if target.is_base() {
            BTreeMap::new()
        } else {
            base_menus.clone()
        };
        outputs.push((
            root.join(target.out_file),
            generate(root, target, &common, &external)?,
        ));
        if let Some(bpt) = target.bpt_out_file {
            outputs.push((
                root.join(bpt),
                generate_breaktables(&root.join(target.dbd_dir))?,
            ));
        }
    }
    Ok(outputs)
}

/// The shared menus, as the paths a *downstream* generated module names them by.
///
/// `motorRecord.dbd` declares `menu(motorDIR)` itself but references
/// `menu(menuYesNo)`, `menu(menuOmsl)` and `menu(menuAlarmSevr)` — EPICS Base
/// menus it expects the loaded `.dbd` set to already carry. Re-declaring them in
/// motor's generated module would be a second declaration of exactly the kind
/// this generator exists to remove (and the wire-visible choice indices would be
/// free to drift apart), so a downstream table points at base's const instead.
fn base_menu_paths(
    root: &Path,
    common: &[parse::Field],
) -> Result<BTreeMap<String, String>, String> {
    let base = parse_dir(&root.join(BASE.dbd_dir), common)?;
    Ok(base
        .menus
        .keys()
        .map(|name| {
            (
                name.clone(),
                format!(
                    "{}::server::record::dbd_generated::{}",
                    // Every downstream target names base the same way; the
                    // const path is the same for all of them.
                    "epics_base_rs",
                    emit::menu_const(name)
                ),
            )
        })
        .collect())
}

fn first_difference(have: &str, want: &str) -> String {
    have.lines()
        .zip(want.lines())
        .enumerate()
        .find(|(_, (a, b))| a != b)
        .map(|(n, (a, b))| {
            format!(
                "first difference at line {}:\n    have: {a}\n    want: {b}",
                n + 1
            )
        })
        .unwrap_or_else(|| {
            format!(
                "length differs: {} vs {} lines",
                have.lines().count(),
                want.lines().count()
            )
        })
}

/// The `breaktable(...)` half of the vendored `.dbd` set. Separate walk,
/// separate grammar, separate output file — see [`Target::bpt_out_file`].
fn generate_breaktables(dbd_dir: &Path) -> Result<String, String> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dbd_dir)
        .map_err(|e| format!("{}: {e}", dbd_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "dbd"))
        .filter(|p| breaktable::is_breaktable_file(p))
        .collect();
    paths.sort();

    let mut tables = Vec::new();
    for path in &paths {
        tables.extend(breaktable::parse_file(path)?);
    }
    eprintln!(
        "dbd-codegen: {} breakpoint tables ({} points)",
        tables.len(),
        tables.iter().map(|t| t.points.len()).sum::<usize>()
    );
    rustfmt(&breaktable::emit(&tables))
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

/// The `recordtype(...)`/`menu(...)`/`device(...)` declarations of one target's
/// `.dbd` directory.
struct Parsed {
    menus: BTreeMap<String, parse::Menu>,
    records: Vec<parse::RecordType>,
    devices: Vec<parse::Device>,
    /// Record type names in DBD **load** order — the order C's
    /// `pdbbase->recordTypeList` holds, which `buildScanLists` walks
    /// record-type-major (`dbScan.c:1054-1076`). Distinct from `records`,
    /// which is sorted by name so the generated table has a stable shape.
    order: Vec<String>,
    /// Every `variable()` declaration, from every `.dbd` in the directory.
    variables: Vec<parse::Variable>,
}

fn parse_dir(dbd_dir: &Path, common: &[parse::Field]) -> Result<Parsed, String> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dbd_dir)
        .map_err(|e| format!("{}: {e}", dbd_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "dbd"))
        .filter(|p| p.file_name().is_some_and(|n| n != "dbCommon.dbd"))
        // `bpt*.dbd` declare `breaktable(...)`, not `recordtype(...)` — a
        // different grammar, walked by `generate_breaktables`.
        .filter(|p| !breaktable::is_breaktable_file(p))
        .collect();
    paths.sort();

    let mut out = Parsed {
        menus: BTreeMap::new(),
        records: Vec::new(),
        devices: Vec::new(),
        order: Vec::new(),
        variables: Vec::new(),
    };
    let mut by_file: Vec<(String, Vec<String>)> = Vec::new();
    for path in &paths {
        let dbd = parse::parse_file(path, common)?;
        by_file.push((
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string(),
            dbd.records.iter().map(|r| r.name.clone()).collect(),
        ));
        out.devices.extend(dbd.devices);
        out.variables.extend(dbd.variables);
        for (name, menu) in dbd.menus {
            if let Some(prev) = out.menus.insert(name.clone(), menu) {
                // Two `.dbd` files declaring the same menu name with different
                // choices would silently mislabel one record's enum, so the
                // generator refuses rather than picking a winner.
                let now = &out.menus[&name];
                if prev.choices != now.choices {
                    return Err(format!(
                        "menu({name}) is declared twice with different choices: \
                         {:?} vs {:?}",
                        prev.choices, now.choices
                    ));
                }
            }
        }
        out.records.extend(dbd.records);
    }
    out.records.sort_by(|a, b| a.name.cmp(&b.name));
    out.order = load_order(dbd_dir, &by_file);
    Ok(out)
}

/// Record types in the order a C IOC would have loaded their declarations.
///
/// The order comes from `stdRecords.dbd`, which is base's own generated include
/// list and is vendored beside the `.dbd` files it names — so it is base's
/// answer, not a list maintained here. A record type declared by a `.dbd` that
/// list does not name (the synApps and asyn types this port also vendors, and
/// every downstream target, which has no such list at all) sorts after all of
/// them, in the walk's own filename order. That mirrors where those types sit
/// in C — a module's `.dbd` is included after `base.dbd`, so its record types
/// join `recordTypeList` behind base's — but their order *among themselves* is
/// the application's include order in C, which the port has no source for.
fn load_order(dbd_dir: &Path, by_file: &[(String, Vec<String>)]) -> Vec<String> {
    let mut order: Vec<String> = Vec::new();
    let push = |names: &[String], order: &mut Vec<String>| {
        for n in names {
            if !order.contains(n) {
                order.push(n.clone());
            }
        }
    };
    if let Ok(src) = std::fs::read_to_string(dbd_dir.join("stdRecords.dbd")) {
        for include in dbd_includes(&src) {
            if let Some((_, names)) = by_file.iter().find(|(f, _)| *f == include) {
                push(names, &mut order);
            }
        }
    }
    for (_, names) in by_file {
        push(names, &mut order);
    }
    order
}

/// The quoted arguments of a `.dbd`'s top-level `include` lines, in order.
fn dbd_includes(src: &str) -> Vec<String> {
    src.lines()
        .filter_map(|line| line.trim().strip_prefix("include"))
        .filter_map(|rest| {
            let inner = rest.trim().strip_prefix('"')?;
            let end = inner.find('"')?;
            Some(inner[..end].to_string())
        })
        .collect()
}

/// The `cvt_dbaddr.types` of one target, or an empty map when it has none.
///
/// The file is only needed by a target whose `.dbd`s declare a
/// `special(SPC_DBADDR)` field; a target with none does not carry an empty file
/// to say so. A target that *does* declare one and has no row for it is a hard
/// generator error (`emit::field_dbf`), so the exception stays closed either way.
fn parse_cvt(dbd_dir: &Path) -> Result<BTreeMap<(String, String), emit::CvtDbAddr>, String> {
    let path = dbd_dir.join("cvt_dbaddr.types");
    match std::fs::read_to_string(&path) {
        Ok(src) => emit::parse_cvt_dbaddr(&src),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn generate(
    root: &Path,
    target: &Target,
    common: &[parse::Field],
    external_menus: &BTreeMap<String, String>,
) -> Result<String, String> {
    let dbd_dir = root.join(target.dbd_dir);
    let cvt = parse_cvt(&dbd_dir)?;
    let parsed = parse_dir(&dbd_dir, common)?;

    let own = || {
        parsed
            .records
            .iter()
            .flat_map(|r| r.fields.iter())
            .filter(|f| !f.from_common)
    };
    let internal = own().filter(|f| emit::is_dropped_internal(f)).count();
    let emitted = own().filter(|f| !emit::is_dropped_internal(f)).count();
    let dbaddr = own()
        .filter(|f| f.special.as_deref() == Some("SPC_DBADDR"))
        .count();
    eprintln!(
        "dbd-codegen: {}: {} record types, {emitted} own fields emitted \
         ({dbaddr} typed from cvt_dbaddr.types), {internal} DBF_NOACCESS internals \
         dropped, {} menus, {} device() entries",
        target.dbd_dir,
        parsed.records.len(),
        parsed.menus.len(),
        parsed.devices.len()
    );

    let src = emit::emit(&emit::Input {
        menus: &parsed.menus,
        devices: &parsed.devices,
        records: &parsed.records,
        // The scan lists are base's, and so is the one table that orders them.
        // A downstream target's record types are unknown to it by construction
        // — the same way C's `recordTypeList` holds them behind base's.
        order: if target.is_base() { &parsed.order } else { &[] },
        variables: &parsed.variables,
        // `dbCommon` is modelled once, by base, and emitted once, into base's
        // table. A downstream module neither re-emits it nor re-declares it.
        common: if target.is_base() { common } else { &[] },
        cvt: &cvt,
        dbd_dir: target.dbd_dir,
        base_path: target.base_path,
        external_menus,
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

#[cfg(test)]
mod tests {
    /// The drift gate.
    ///
    /// The generated tables are checked in, so nothing at build time forces them
    /// to agree with the vendored `.dbd` files they claim to be derived from.
    /// The file used to say `--check` "is what CI runs" — CI ran no such step,
    /// and this crate had no test for nextest to pick up, so the single
    /// generator the port has could go stale in silence and the header would
    /// keep asserting it hadn't.
    ///
    /// It is a `#[test]` rather than a CI-yaml step so `cargo nextest run`
    /// catches drift on the developer's machine, before the push, and so the
    /// gate cannot be skipped by editing a workflow file.
    ///
    /// It walks [`super::generate_all`], which walks every target — so a
    /// downstream crate's table (`motor`, `optics`, `scaler`, `std`) is as
    /// ungateable as base's, and adding a target cannot forget to add a gate.
    #[test]
    fn generated_files_are_not_stale() {
        let root = super::repo_root().expect("workspace root");
        let outputs = super::generate_all(&root).expect("generator run");
        assert!(
            outputs.len() >= super::TARGETS.len(),
            "the target list emptied"
        );
        for (path, want) in &outputs {
            let have = std::fs::read_to_string(path).unwrap_or_default();
            assert!(
                have == *want,
                "{} is STALE — it no longer matches the vendored .dbd files.\n\
                 Re-run `cargo run -p dbd-codegen -- --write` and commit the result.\n\
                 {}",
                path.display(),
                super::first_difference(&have, want)
            );
        }
    }
}
