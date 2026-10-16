// RTEMS-EXEC-MODEL-ALLOW(2): sync tests that hand-build their own tokio runtime; run and pass in the exec-backend suite.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::server::iocsh::registry::{
    ArgDesc, ArgType, ArgValue, CommandContext, CommandDef, CommandOutcome,
};

use super::backup::BackupConfig;
use super::error::AutosaveError;
use super::format::CompatMode;
use super::macros::MacroContext;
use super::manager::{AutosaveBuilder, AutosaveManager};
use super::save_set::{SaveMethods, SaveSetConfig, SaveStrategy};

/// One request file's set from st.cmd, before `iocInit`: C's `chlist`,
/// which each `create_*_set` naming the file gives one more method
/// (`create_data_set`, save_restore.c:2367-2392). The first call's macros
/// are the set's.
#[derive(Debug, Clone)]
pub struct SetDef {
    pub filename: String,
    pub macros: String,
    pub methods: SaveMethods,
}

impl SetDef {
    /// The set the first `create_*_set` naming `filename` makes.
    pub fn new(filename: String, macros: String, method: SaveStrategy) -> Self {
        let mut methods = SaveMethods::default();
        let _ = methods.add(method.creating_list());
        Self {
            filename,
            macros,
            methods,
        }
    }
}

/// A `create_*_set` period argument. C keeps the `int` raw on a method
/// added to a list, and a timer delay below zero expires at once, as
/// zero does. Only the method that makes the list is floored.
pub fn period_arg(seconds: i64) -> Duration {
    Duration::from_secs(seconds.max(0) as u64)
}

/// The method `create_monitor_set` adds: C's MONITORED, a save when a
/// member changed during a `period` window, not every period.
pub fn monitor_method(period: Duration) -> SaveStrategy {
    SaveStrategy::OnChange {
        min_interval: period,
    }
}

/// The method `create_triggered_set` adds: a save on each monitor event
/// of the trigger PV.
pub fn triggered_method(trigger_pv: String) -> SaveStrategy {
    SaveStrategy::Triggered { trigger_pv }
}

/// C's `isValid1stPVChar` (`save_restore.c:495-499`), which is what
/// `create_triggered_set` tests its trigger-channel argument with.
pub(super) fn is_valid_first_pv_char(name: &str) -> bool {
    name.chars().next().is_some_and(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '_' | '-' | '+' | ':' | '[' | ']' | '<' | '>' | ';')
    })
}

/// Definition of a restore file from st.cmd.
#[derive(Debug, Clone)]
pub struct RestoreDef {
    pub filename: String,
    pub macros: String,
}

/// Startup configuration collected from st.cmd autosave commands.
///
/// Populated during Phase 1 (st.cmd execution) via iocsh commands,
/// then consumed during Phase 2 (iocInit) to build the AutosaveManager.
#[derive(Debug, Default)]
pub struct AutosaveStartupConfig {
    pub request_file_paths: Vec<PathBuf>,
    pub save_file_path: Option<PathBuf>,
    pub status_prefix: Option<String>,
    /// The sets the `create_*_set` commands made before `iocInit`, one per
    /// request file, in creation order.
    pub sets: Vec<SetDef>,
    pub pass0_restores: Vec<RestoreDef>,
    pub pass1_restores: Vec<RestoreDef>,
    /// On-disk save-file format. Default [`CompatMode::Native`]; set
    /// to [`CompatMode::CRead`] (via the `save_restoreSet_CompatMode`
    /// iocsh command) so a C IOC can read the produced `.sav` files.
    pub compat: CompatMode,
    /// The manager `iocInit` built from this config. Set by `iocInit`;
    /// from then on the `create_*_set` commands add their set to it
    /// instead of to `sets`, which C's `create_data_set` does at
    /// any time (`save_restore.c`).
    pub manager: Option<Arc<AutosaveManager>>,
}

impl AutosaveStartupConfig {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve a save file path from a request filename.
    pub fn resolve_save_file(&self, filename: &str) -> PathBuf {
        let base = filename.trim_end_matches(".req");
        let sav_name = if base.ends_with(".sav") {
            base.to_string()
        } else {
            format!("{base}.sav")
        };
        match &self.save_file_path {
            Some(dir) => dir.join(&sav_name),
            None => PathBuf::from(&sav_name),
        }
    }

    /// The set a request file describes, named by the file as C's lists are.
    pub fn set_config(&self, filename: &str, macros: &str) -> SaveSetConfig {
        // The NAME, not a pre-resolved path.
        // `SaveSet::load_entries` hands it to
        // `load_request_file_with_search_paths`, which searches
        // `search_paths` (the same `request_file_paths`) and returns
        // `AutosaveError::RequestFile` when nothing matches, so
        // `build()` refuses the set. Pre-resolving here turned that
        // failure into `request_file: None`, which is this field's
        // OTHER meaning — "this set has no request file, its members
        // are `request_pvs`" — and the set then built with zero
        // entries and overwrote a good `.sav` with a two-line file on
        // its first tick.
        let request_file = Some(PathBuf::from(filename));
        let save_path = self.resolve_save_file(filename);
        let macros = if macros.is_empty() {
            HashMap::new()
        } else {
            MacroContext::parse_inline(macros)
        };
        SaveSetConfig {
            name: filename.to_string(),
            save_path,
            request_file,
            request_pvs: Vec::new(),
            backup: BackupConfig::default(),
            macros,
            search_paths: self.request_file_paths.clone(),
        }
    }

    /// Build an AutosaveBuilder from the collected configuration.
    pub fn into_builder(&self) -> AutosaveBuilder {
        let mut builder = AutosaveBuilder::new().compat(self.compat);

        if let Some(ref prefix) = self.status_prefix {
            builder = builder.status_prefix(prefix);
        }

        for def in &self.sets {
            for method in def.methods.iter() {
                builder =
                    builder.add_set(self.set_config(&def.filename, &def.macros), method.clone());
            }
        }

        builder
    }

    /// C `create_data_set` for a `create_*_set` command: `method` joins the
    /// set built from `filename`, or makes it. After `iocInit` the set is
    /// the running manager's, as C's live list is; before, it is kept for
    /// `into_builder`. A method the set already has is refused on the error
    /// log only, as C's iocsh wrappers ignore the status.
    fn create_data_set(
        holder: &Mutex<Self>,
        ctx: &CommandContext,
        filename: String,
        macros: String,
        method: SaveStrategy,
    ) -> Result<CommandOutcome, String> {
        // save_restore.c:2357-2362
        if filename.is_empty() {
            return Ok(CommandOutcome::Continue);
        }
        let (mgr, cfg) = {
            let mut cfg = holder.lock().unwrap();
            match cfg.manager.clone() {
                Some(mgr) => {
                    let set = cfg.set_config(&filename, &macros);
                    (mgr, set)
                }
                None => {
                    match cfg.sets.iter_mut().find(|d| d.filename == filename) {
                        Some(def) => {
                            if let Err(bit) = def.methods.add(method) {
                                let refusal = AutosaveError::AlreadyInMode {
                                    request_file: filename,
                                    bit,
                                };
                                crate::runtime::log::errlog_vprintf(format_args!("{refusal}\n"));
                            }
                        }
                        None => cfg.sets.push(SetDef::new(filename, macros, method)),
                    }
                    return Ok(CommandOutcome::Continue);
                }
            }
        };
        // Already reported; C's iocsh wrappers ignore the status.
        let _ = ctx.block_on(mgr.create_data_set(cfg, method));
        Ok(CommandOutcome::Continue)
    }

    /// Register the autosave iocsh commands that populate this config.
    pub fn register_startup_commands(holder: Arc<Mutex<Self>>) -> Vec<CommandDef> {
        let mut commands = Vec::new();

        // set_requestfile_path(path, pathsub)
        {
            let h = holder.clone();
            commands.push(CommandDef::new(
                "set_requestfile_path",
                vec![
                    ArgDesc {
                        name: "path",
                        arg_type: ArgType::String,
                    },
                    ArgDesc {
                        name: "pathsub",
                        arg_type: ArgType::String,
                    },
                ],
                "set_requestfile_path(path, pathsub) - Add request file search path",
                move |args: &[ArgValue], _ctx: &CommandContext| {
                    let path = match &args[0] {
                        ArgValue::String(s) => s.to_path()?.to_path_buf(),
                        _ => return Err("path argument required".into()),
                    };
                    let full_path = match args.get(1) {
                        Some(ArgValue::String(sub)) if !sub.is_empty() => path.join(sub.to_path()?),
                        _ => path,
                    };
                    h.lock().unwrap().request_file_paths.push(full_path);
                    Ok(CommandOutcome::Continue)
                },
            ));
        }

        // set_savefile_path(path, pathsub)
        {
            let h = holder.clone();
            commands.push(CommandDef::new(
                "set_savefile_path",
                vec![
                    ArgDesc {
                        name: "path",
                        arg_type: ArgType::String,
                    },
                    ArgDesc {
                        name: "pathsub",
                        arg_type: ArgType::String,
                    },
                ],
                "set_savefile_path(path, pathsub) - Set save file directory",
                move |args: &[ArgValue], _ctx: &CommandContext| {
                    let path = match &args[0] {
                        ArgValue::String(s) => s.to_path()?.to_path_buf(),
                        _ => return Err("path argument required".into()),
                    };
                    let full_path = match args.get(1) {
                        Some(ArgValue::String(sub)) if !sub.is_empty() => path.join(sub.to_path()?),
                        _ => path,
                    };
                    // Creating the directory is an rs extension; C prints nothing here.
                    let _ = std::fs::create_dir_all(&full_path);
                    h.lock().unwrap().save_file_path = Some(full_path);
                    Ok(CommandOutcome::Continue)
                },
            ));
        }

        // create_periodic_set / create_monitor_set(filename, period, macrostring)
        let period_sets: [(&str, &str, fn(Duration) -> SaveStrategy); 2] = [
            (
                "create_periodic_set",
                "create_periodic_set(filename, period, macrostring) - Create save set saved every period",
                |interval| SaveStrategy::Periodic { interval },
            ),
            (
                "create_monitor_set",
                "create_monitor_set(filename, period, macrostring) - Create save set saved on change",
                monitor_method,
            ),
        ];
        for (name, usage, method) in period_sets {
            let h = holder.clone();
            commands.push(CommandDef::new(
                name,
                vec![
                    ArgDesc {
                        name: "filename",
                        arg_type: ArgType::String,
                    },
                    ArgDesc {
                        name: "period",
                        arg_type: ArgType::Int,
                    },
                    ArgDesc {
                        name: "macrostring",
                        arg_type: ArgType::String,
                    },
                ],
                usage,
                move |args: &[ArgValue], ctx: &CommandContext| {
                    let filename = match &args[0] {
                        ArgValue::String(s) => s.to_str()?.to_owned(),
                        _ => return Err("filename argument required".into()),
                    };
                    let period = match &args[1] {
                        ArgValue::Int(n) => period_arg(*n),
                        _ => return Err("period argument required".into()),
                    };
                    let macros = match args.get(2) {
                        Some(ArgValue::String(s)) => s.to_str()?.to_owned(),
                        _ => String::new(),
                    };
                    Self::create_data_set(&h, ctx, filename, macros, method(period))
                },
            ));
        }

        // create_triggered_set(filename, trigger_channel, macrostring)
        //
        // C-autosave parity: the second argument is the *trigger PV
        // name*, not a poll period — the save set is written whenever
        // that PV changes/processes. Maps to `SaveStrategy::Triggered`
        // in `into_builder`.
        {
            let h = holder.clone();
            commands.push(CommandDef::new(
                "create_triggered_set",
                vec![
                    ArgDesc {
                        name: "filename",
                        arg_type: ArgType::String,
                    },
                    ArgDesc {
                        name: "trigger_channel",
                        arg_type: ArgType::String,
                    },
                    ArgDesc {
                        name: "macrostring",
                        arg_type: ArgType::String,
                    },
                ],
                "create_triggered_set(filename, trigger_channel, macrostring) - \
                 Create triggered save set (saves when trigger_channel changes)",
                move |args: &[ArgValue], ctx: &CommandContext| {
                    let filename = match &args[0] {
                        ArgValue::String(s) => s.to_str()?.to_owned(),
                        _ => return Err("filename argument required".into()),
                    };
                    let trigger_channel = match &args[1] {
                        ArgValue::String(s) if s.to_str().is_ok_and(is_valid_first_pv_char) => {
                            s.to_str()?.to_owned()
                        }
                        // C's wording, and C's refusal: no set is created
                        // at all rather than one that saves on something
                        // else (`save_restore.c:2296-2303`).
                        _ => {
                            return Err(
                                "save_restore:create_triggered_set: Error: trigger-channel \
                                 name is required."
                                    .into(),
                            );
                        }
                    };
                    let macros = match args.get(2) {
                        Some(ArgValue::String(s)) => s.to_str()?.to_owned(),
                        _ => String::new(),
                    };
                    Self::create_data_set(
                        &h,
                        ctx,
                        filename,
                        macros,
                        triggered_method(trigger_channel),
                    )
                },
            ));
        }

        // create_manual_set(filename, macrostring)
        {
            let h = holder.clone();
            commands.push(CommandDef::new(
                "create_manual_set",
                vec![
                    ArgDesc {
                        name: "filename",
                        arg_type: ArgType::String,
                    },
                    ArgDesc {
                        name: "macrostring",
                        arg_type: ArgType::String,
                    },
                ],
                "create_manual_set(filename, macrostring) - Create save set saved only on request",
                move |args: &[ArgValue], ctx: &CommandContext| {
                    let filename = match args.first() {
                        Some(ArgValue::String(s)) => s.to_str()?.to_owned(),
                        _ => String::new(),
                    };
                    let macros = match args.get(1) {
                        Some(ArgValue::String(s)) => s.to_str()?.to_owned(),
                        _ => String::new(),
                    };
                    Self::create_data_set(&h, ctx, filename, macros, SaveStrategy::Manual)
                },
            ));
        }

        // set_pass0_restoreFile(filename, macrostring)
        {
            let h = holder.clone();
            commands.push(CommandDef::new(
                "set_pass0_restoreFile",
                vec![
                    ArgDesc {
                        name: "filename",
                        arg_type: ArgType::String,
                    },
                    ArgDesc {
                        name: "macrostring",
                        arg_type: ArgType::String,
                    },
                ],
                "set_pass0_restoreFile(filename, macrostring) - Restore before device support init",
                move |args: &[ArgValue], _ctx: &CommandContext| {
                    let filename = match &args[0] {
                        ArgValue::String(s) => s.to_str()?.to_owned(),
                        _ => return Err("filename argument required".into()),
                    };
                    let macros = match args.get(1) {
                        Some(ArgValue::String(s)) => s.to_str()?.to_owned(),
                        _ => String::new(),
                    };
                    h.lock()
                        .unwrap()
                        .pass0_restores
                        .push(RestoreDef { filename, macros });
                    Ok(CommandOutcome::Continue)
                },
            ));
        }

        // set_pass1_restoreFile(filename, macrostring)
        {
            let h = holder.clone();
            commands.push(CommandDef::new(
                "set_pass1_restoreFile",
                vec![
                    ArgDesc {
                        name: "filename",
                        arg_type: ArgType::String,
                    },
                    ArgDesc {
                        name: "macrostring",
                        arg_type: ArgType::String,
                    },
                ],
                "set_pass1_restoreFile(filename, macrostring) - Restore after device support init",
                move |args: &[ArgValue], _ctx: &CommandContext| {
                    let filename = match &args[0] {
                        ArgValue::String(s) => s.to_str()?.to_owned(),
                        _ => return Err("filename argument required".into()),
                    };
                    let macros = match args.get(1) {
                        Some(ArgValue::String(s)) => s.to_str()?.to_owned(),
                        _ => String::new(),
                    };
                    h.lock()
                        .unwrap()
                        .pass1_restores
                        .push(RestoreDef { filename, macros });
                    Ok(CommandOutcome::Continue)
                },
            ));
        }

        // save_restoreSet_CompatMode(mode)
        //
        // Rust extension: select the on-disk save-file format.
        // `mode="C"` / `"CRead"` writes `.sav` files a C IOC can
        // read (mixed Rust + C IOC site); anything else (default)
        // keeps the autosave-rs native format.
        {
            let h = holder.clone();
            commands.push(CommandDef::new(
                "save_restoreSet_CompatMode",
                vec![ArgDesc {
                    name: "mode",
                    arg_type: ArgType::String,
                }],
                "save_restoreSet_CompatMode(mode) - 'C'/'CRead' for C-readable .sav, \
                 else native",
                move |args: &[ArgValue], _ctx: &CommandContext| {
                    let mode = match &args[0] {
                        ArgValue::String(s) => s.to_str()?.to_owned(),
                        _ => return Err("mode argument required".into()),
                    };
                    let compat =
                        if mode.eq_ignore_ascii_case("c") || mode.eq_ignore_ascii_case("cread") {
                            CompatMode::CRead
                        } else {
                            CompatMode::Native
                        };
                    h.lock().unwrap().compat = compat;
                    Ok(CommandOutcome::Continue)
                },
            ));
        }

        // save_restoreSet_status_prefix(prefix)
        {
            let h = holder.clone();
            commands.push(CommandDef::new(
                "save_restoreSet_status_prefix",
                vec![ArgDesc {
                    name: "prefix",
                    arg_type: ArgType::String,
                }],
                "save_restoreSet_status_prefix(prefix) - Set status PV prefix",
                move |args: &[ArgValue], _ctx: &CommandContext| {
                    let prefix = match &args[0] {
                        ArgValue::String(s) => s.to_str()?.to_owned(),
                        _ => return Err("prefix argument required".into()),
                    };
                    h.lock().unwrap().status_prefix = Some(prefix);
                    Ok(CommandOutcome::Continue)
                },
            ));
        }

        commands
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::autosave::save_set::SaveStrategy;

    /// M3 regression: a triggered save set must build as a real
    /// `SaveStrategy::Triggered` (trigger-PV watcher), NOT
    /// `SaveStrategy::OnChange` polling. Pre-fix `into_builder`
    /// mapped every triggered set to `OnChange`, so the
    /// `SaveStrategy::Triggered` variant was unreachable from the
    /// iocsh `create_triggered_set` command.
    /// A real, loadable request file, plus the search path that finds it.
    /// A set that names a request file it cannot load no longer builds at
    /// all (see `autosave_unresolvable_request_file.rs`), so a strategy
    /// test has to give it one.
    fn with_request_file(cfg: &mut AutosaveStartupConfig, dir: &tempfile::TempDir) {
        std::fs::write(dir.path().join("settings.req"), "IOC:setpoint\n").unwrap();
        cfg.request_file_paths.push(dir.path().to_path_buf());
        cfg.save_file_path = Some(dir.path().to_path_buf());
    }

    #[test]
    fn triggered_set_maps_to_triggered_strategy() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = AutosaveStartupConfig::new();
        with_request_file(&mut cfg, &dir);
        cfg.sets.push(SetDef::new(
            "settings.req".to_string(),
            String::new(),
            triggered_method("IOC:saveTrigger".to_string()),
        ));

        let builder = cfg.into_builder();
        // The builder owns the configs; rebuild to inspect the set.
        // `AutosaveBuilder` does not expose its set list, so verify
        // through a constructed `SaveSetConfig` shape instead by
        // re-running the mapping logic the public path uses.
        // Direct check: `into_builder` is the public mapping; assert
        // the strategy via a SaveSet built from it.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mgr = rt.block_on(builder.build());
        let sets = mgr.sets();
        assert_eq!(sets.len(), 1, "one triggered set expected");
        match &sets[0].0.methods()[..] {
            [SaveStrategy::Triggered { trigger_pv }] => {
                assert_eq!(trigger_pv.as_str(), "IOC:saveTrigger");
            }
            other => panic!("expected SaveStrategy::Triggered, got {other:?}"),
        }
    }

    /// M3: the trigger PV is not optional. C refuses the call rather
    /// than building a set that saves on something else, so the command
    /// must too — and there is no longer a `None` for `into_builder` to
    /// interpret.
    #[test]
    fn create_triggered_set_refuses_a_missing_trigger_channel() {
        let holder = Arc::new(Mutex::new(AutosaveStartupConfig::new()));
        let cmds = AutosaveStartupConfig::register_startup_commands(holder.clone());
        let cmd = cmds
            .iter()
            .find(|c| c.name == "create_triggered_set")
            .expect("registered");

        let rt = tokio::runtime::Runtime::new().unwrap();
        let bridge = {
            let _guard = rt.enter();
            crate::runtime::task::BlockingBridge::capture()
        };
        let ctx = CommandContext::new(Arc::new(crate::server::database::PvDatabase::new()), bridge);
        for bad in ["", " IOC:trig", "*"] {
            let err = cmd
                .handler
                .call(
                    &[
                        ArgValue::String("settings.req".into()),
                        ArgValue::String(bad.into()),
                    ],
                    &ctx,
                )
                .err()
                .expect("a trigger channel that is not a PV name must be refused");
            assert!(err.contains("trigger-channel name is required"), "{err}");
        }
        assert!(
            holder.lock().unwrap().sets.is_empty(),
            "a refused call must leave no set behind"
        );
    }
}
