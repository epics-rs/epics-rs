//! SNL program launcher for the std module — the Rust stand-in for C's
//! `seq &program, "macros"`.
//!
//! Each `snl` module here is a pure state machine plus a `run(config, db)`
//! that gives it PVs; this is the one place that turns a program name and a
//! macro string into the right pair. An IOC binary reaches it with a single
//! `seqStart` startup command, the same shape
//! `optics_rs::seq_runner::seq_start` is registered with.

// RTEMS-EXEC-MODEL-ALLOW(2): checked, not waived — both ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p std-rs
// --all-features`, 223/223). The reactor each test obtains comes from the
// test's own `Runtime::new`, which the backend does not remove.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::snl::{SeqMacros, spawn_program};

/// Start a std-module SNL program by name, its macros parsed over the
/// program's `program` parameters as `seq` does, so one left out takes the
/// `.st`'s default. A macro with no value reads as "", what `seqMacEval`
/// puts for its `{name}`.
///
/// | Name | Program |
/// |------|---------|
/// | `delayDo` | `delayDo.st` — wait out an active condition, then process `doSeq` |
/// | `femto` | `femto.st` — Femto amplifier gain control |
///
/// Must be called with the runtime reachable — from st.cmd that means
/// `CommandContext::bridge()`, because the shell runs on a blocking thread.
pub fn seq_start(
    program: &str,
    macro_str: &str,
    bridge: &epics_base_rs::runtime::task::BlockingBridge,
    db: &PvDatabase,
) -> Result<(), String> {
    match program {
        "delayDo" => {
            let m = SeqMacros::new("name=delayDo,P=xxx:,R=delayDo1", macro_str);
            let v = |name| m.value(name).unwrap_or_default();
            let config = crate::snl::delay_do::DelayDoConfig::new(v("P"), v("R"));
            spawn_program(bridge, db, "delayDo", &m, move |db| {
                crate::snl::delay_do::run(config, db)
            });
        }
        "femto" => {
            let m = SeqMacros::new("name=femto,P=,H=,F=,G1=,G2=,G3=,NO=", macro_str);
            let v = |name| m.value(name).unwrap_or_default();
            let config = crate::snl::femto::FemtoConfig::new(
                v("P"),
                v("H"),
                v("F"),
                v("G1"),
                v("G2"),
                v("G3"),
                v("NO"),
            );
            spawn_program(bridge, db, "femto", &m, move |db| {
                crate::snl::femto::run(config, db)
            });
        }
        other => return Err(format!("Can't find sequencer `{other}'.")),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `delayDo.st:1` and `femto.st:15` define every macro the programs
    /// read, so `seq` starts them with none given.
    #[test]
    fn every_program_starts_on_its_program_defaults() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let bridge = {
            let _guard = rt.enter();
            epics_base_rs::runtime::task::BlockingBridge::capture()
        };
        let db = PvDatabase::new();
        assert_eq!(seq_start("delayDo", "", &bridge, &db), Ok(()));
        assert_eq!(seq_start("femto", "", &bridge, &db), Ok(()));
    }

    /// seq_cmd.c:162.
    #[test]
    fn an_unknown_program_is_refused_in_seq_cmd_c_s_words() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let bridge = {
            let _guard = rt.enter();
            epics_base_rs::runtime::task::BlockingBridge::capture()
        };
        assert_eq!(
            seq_start("nonexistent", "", &bridge, &PvDatabase::new()),
            Err("Can't find sequencer `nonexistent'.".to_string())
        );
    }
}
