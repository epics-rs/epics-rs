//! SNL program launcher for the std module — the Rust stand-in for C's
//! `seq &program, "macros"`.
//!
//! Each `snl` module here is a program snc-rs compiled from the upstream
//! `.st`; this is the one place that turns a program name and a macro string
//! into `epics_seq_rs::seq`. An IOC binary reaches it with a single
//! `seqStart` startup command, the same shape
//! `optics_rs::seq_runner::seq_start` is registered with.

// RTEMS-EXEC-MODEL-ALLOW(2): checked, not waived — both ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p std-rs
// --all-features`, 223/223). The reactor each test obtains comes from the
// test's own `Runtime::new`, which the backend does not remove.

use epics_base_rs::server::database::PvDatabase;

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
    _bridge: &epics_base_rs::runtime::task::BlockingBridge,
    db: &PvDatabase,
) -> Result<(), String> {
    match program {
        "delayDo" => {
            epics_seq_rs::seq(&crate::snl::delay_do::delayDo, macro_str, 0, db)
                .ok_or_else(|| "seq: delayDo refused to start".to_string())?;
        }
        "femto" => {
            epics_seq_rs::seq(&crate::snl::femto::femto, macro_str, 0, db)
                .ok_or_else(|| "seq: femto refused to start".to_string())?;
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
