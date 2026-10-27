//! Derives this crate's own copy of the `exec_backend` / `tokio_backend` cfg,
//! the way `epics-libcom-rs` (the original), `epics-base-rs`, `epics-ca-rs`
//! and `epics-pva-rs` derive theirs.
//!
//! Both integration tests drive the table record through a real CA server's
//! put gate, which needs the reactor.
//!
//! A cfg set by a dependency's build script is not visible here, so every
//! crate that gates on the pair derives it again from the same two inputs:
//! `EPICS_RS_BUILD_EXEC_BACKEND` and the target OS. Reading the variable alone
//! would be wrong on exactly the case the workspace predicate exists for: on
//! RTEMS it is unset while `exec_backend` is ON.
//!
//! It also compiles every `src/snl/*.st` with snc-rs to `$OUT_DIR/<stem>.rs`,
//! which the companion calling `include_program!("<stem>")` includes, and
//! writes `$OUT_DIR/seq_programs.rs`: the table `seq_runner` looks a
//! program's name up in, as `seq_cmd.c` looks in its registered programs.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo::rustc-check-cfg=cfg(exec_backend)");
    println!("cargo::rustc-check-cfg=cfg(tokio_backend)");
    println!("cargo::rustc-check-cfg=cfg(epics_embedded_target)");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let embedded_target = target_os == "rtems";
    if embedded_target {
        println!("cargo::rustc-cfg=epics_embedded_target");
    }

    // Build-time backend selection, from the environment rather than from a
    // cargo feature: a feature that flips a backend is not additive, so
    // `--all-features` turned the reactor off and no single invocation meant
    // "everything on". `epics-libcom-rs`'s module docs carry the reasoning;
    // `tools/rtems-exec-gate` holds every copy of this block against that
    // crate's, so 23 derivations of one rule cannot drift apart.
    println!("cargo::rerun-if-env-changed=EPICS_RS_BUILD_EXEC_BACKEND");
    let requested = std::env::var_os("EPICS_RS_BUILD_EXEC_BACKEND").unwrap_or_default();
    let host_exec_backend = match requested.to_string_lossy().as_ref() {
        "thread" => true,
        "" | "tokio" => false,
        bad => panic!(
            "EPICS_RS_BUILD_EXEC_BACKEND={bad}: the exec backend is `thread` \
             (reactor-free std threads) or `tokio` (the host default, which an \
             unset or empty variable also selects)"
        ),
    };
    if embedded_target || host_exec_backend {
        println!("cargo::rustc-cfg=exec_backend");
    } else {
        println!("cargo::rustc-cfg=tokio_backend");
    }

    compile_snl();
}

fn compile_snl() {
    let snl = Path::new("src/snl");
    // compile_dir also reruns this on any change in src/snl, companions included.
    let programs = epics_seq_rs::snc::build::compile_dir(snl);
    let mut companions = HashMap::new();
    for entry in fs::read_dir(snl).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let module = path.file_stem().unwrap().to_str().unwrap().to_owned();
        for line in fs::read_to_string(&path).unwrap().lines() {
            if let Some(rest) = line.strip_prefix("epics_seq_rs::include_program!(\"") {
                let stem = rest.split_once('"').unwrap().0;
                companions.insert(stem.to_owned(), module.clone());
            }
        }
    }
    let mut table = String::new();
    for rs in &programs {
        let stem = rs.file_stem().unwrap().to_str().unwrap();
        let module = companions.get(stem).unwrap_or_else(|| {
            panic!("src/snl/{stem}.st: no src/snl/*.rs calls include_program!(\"{stem}\")")
        });
        // The generated file ends `pub use seqg::{<prog>, <prog>Registrar};`.
        let generated = fs::read_to_string(rs).unwrap();
        let program = generated
            .rsplit_once("pub use seqg::{")
            .and_then(|(_, rest)| rest.split_once(','))
            .map(|(name, _)| name.trim())
            .unwrap_or_else(|| panic!("{}: no `pub use seqg::{{..}}`", rs.display()));
        writeln!(table, "    &crate::snl::{module}::{program},").unwrap();
    }
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    fs::write(
        out.join("seq_programs.rs"),
        format!(
            "pub(crate) static SEQ_PROGRAMS: &[&dyn epics_seq_rs::RegisteredProgram] = &[\n{table}];\n"
        ),
    )
    .unwrap();
}
