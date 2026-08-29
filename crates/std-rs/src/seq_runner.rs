//! SNL program launcher for the std module — the Rust stand-in for C's
//! `seq &program, "macros"`.
//!
//! Each `snl` module here is a pure state machine plus a `run(config, db)`
//! that gives it PVs; this is the one place that turns a program name and a
//! macro string into the right pair. An IOC binary reaches it with a single
//! `seqStart` startup command, the same shape
//! `optics_rs::seq_runner::seq_start` is registered with.

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::macro_defn_pairs;

/// Split a `seq` macro string into definitions.
///
/// Through `macro_defn_pairs`, the port's owner of C's `macParseDefns`
/// grammar (`macUtil.c`), so a quoted value with an embedded comma survives —
/// a raw `split(',')` tears it. A name given with no `=` is a deletion in
/// that grammar and is dropped here, since a program can only be started with
/// definitions.
pub fn parse_macros(input: &str) -> HashMap<String, String> {
    macro_defn_pairs(input)
        .into_iter()
        .filter_map(|(name, value)| value.map(|v| (name, v)))
        .collect()
}

fn require_macro(
    macros: &HashMap<String, String>,
    key: &str,
    program: &str,
) -> Result<String, String> {
    macros
        .get(key)
        .cloned()
        .ok_or_else(|| format!("{program}: required macro '{key}' not specified"))
}

/// Start a std-module SNL program by name.
///
/// | Name | Macros | Program |
/// |------|--------|---------|
/// | `delayDo` | P, R | `delayDo.st` — wait out an active condition, then process `doSeq` |
/// | `femto` | P, H, F, G1, G2, G3, NO | `femto.st` — Femto amplifier gain control |
///
/// Must be called with the runtime reachable — from st.cmd that means
/// `CommandContext::bridge()`, because the shell runs on a blocking thread.
pub fn seq_start(
    program: &str,
    macro_str: &str,
    bridge: &epics_base_rs::runtime::task::BlockingBridge,
    db: &PvDatabase,
) -> Result<(), String> {
    let macros = parse_macros(macro_str);

    match program {
        "delayDo" => {
            let config = crate::snl::delay_do::DelayDoConfig::new(
                &require_macro(&macros, "P", program)?,
                &require_macro(&macros, "R", program)?,
            );
            let db = db.clone();
            bridge.spawn(async move {
                if let Err(e) = crate::snl::delay_do::run(config, db).await {
                    eprintln!("delayDo error: {e}");
                }
            });
        }
        "femto" => {
            let config = crate::snl::femto::FemtoConfig::new(
                &require_macro(&macros, "P", program)?,
                &require_macro(&macros, "H", program)?,
                &require_macro(&macros, "F", program)?,
                &require_macro(&macros, "G1", program)?,
                &require_macro(&macros, "G2", program)?,
                &require_macro(&macros, "G3", program)?,
                &require_macro(&macros, "NO", program)?,
            );
            let db = db.clone();
            bridge.spawn(async move {
                if let Err(e) = crate::snl::femto::run(config, db).await {
                    eprintln!("femto error: {e}");
                }
            });
        }
        other => return Err(format!("seq_start: unknown std program '{other}'")),
    }

    Ok(())
}
