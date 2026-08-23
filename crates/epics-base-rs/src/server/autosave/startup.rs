// RTEMS-EXEC-MODEL-ALLOW(2): sync tests that hand-build their own tokio runtime; run and pass in the feature-ON suite.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::server::iocsh::registry::{
    ArgDesc, ArgType, ArgValue, CommandContext, CommandDef, CommandOutcome,
};

use super::backup::BackupConfig;
use super::format::CompatMode;
use super::macros::MacroContext;
use super::manager::AutosaveBuilder;
use super::save_set::{SaveSetConfig, SaveStrategy, TriggerMode};

/// Definition of a monitor/triggered save set from st.cmd.
#[derive(Debug, Clone)]
pub struct MonitorSetDef {
    pub filename: String,
    /// The save period for a monitor set, or the trigger-watcher
    /// debounce for a triggered one. Always a legal interval: build it
    /// with [`MonitorSetDef::poll_period`].
    pub period: Duration,
    pub macros: String,
    /// Trigger PV name for a triggered save set. `None` for periodic
    /// monitor sets (`create_monitor_set`). `Some(pv)` for
    /// `create_triggered_set` — the set is saved whenever this PV
    /// changes, mapped to [`SaveStrategy::Triggered`].
    pub trigger_pv: Option<String>,
}

impl MonitorSetDef {
    /// Turn the `period` argument of `create_monitor_set` /
    /// `create_triggered_set` into an interval that is legal on both task
    /// backends.
    ///
    /// The iocsh argument is an `i64` and both ends of it were unsafe.
    /// Zero panics tokio's `interval` on the async backend and
    /// busy-rewrites the `.sav` forever on the blocking one, which does
    /// not assert; a negative value cast to `u32` became 4294967295
    /// seconds, so the set never saved again while `fdblist` reported it
    /// idle. One second is the floor already used for the trigger
    /// debounce.
    ///
    /// The clamp belongs here and not in `into_builder`, because that is
    /// two loops: the monitor loop used the number raw and the triggered
    /// loop clamped it, which is how one `u32` came to mean both "what
    /// the operator typed" and "the interval to run at".
    pub fn poll_period(seconds: i64) -> Duration {
        Duration::from_secs(seconds.max(1) as u64)
    }
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
    pub monitor_sets: Vec<MonitorSetDef>,
    pub triggered_sets: Vec<MonitorSetDef>,
    pub pass0_restores: Vec<RestoreDef>,
    pub pass1_restores: Vec<RestoreDef>,
    /// On-disk save-file format. Default [`CompatMode::Native`]; set
    /// to [`CompatMode::CRead`] (via the `save_restoreSet_CompatMode`
    /// iocsh command) so a C IOC can read the produced `.sav` files.
    pub compat: CompatMode,
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

    /// Build an AutosaveBuilder from the collected configuration.
    pub fn into_builder(&self) -> AutosaveBuilder {
        let mut builder = AutosaveBuilder::new().compat(self.compat);

        if let Some(ref prefix) = self.status_prefix {
            builder = builder.status_prefix(prefix);
        }

        // Add monitor sets (periodic strategy)
        for def in &self.monitor_sets {
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
            let request_file = Some(PathBuf::from(&def.filename));
            let save_path = self.resolve_save_file(&def.filename);
            let macros = if def.macros.is_empty() {
                HashMap::new()
            } else {
                MacroContext::parse_inline(&def.macros)
            };
            builder = builder.add_set(SaveSetConfig {
                name: def.filename.clone(),
                save_path,
                strategy: SaveStrategy::Periodic {
                    interval: def.period,
                },
                request_file,
                request_pvs: Vec::new(),
                backup: BackupConfig::default(),
                macros,
                search_paths: self.request_file_paths.clone(),
            });
        }

        // Add triggered sets. C-autosave `create_triggered_set`
        // saves the set whenever its trigger PV changes — mapped to
        // the real `SaveStrategy::Triggered` (trigger-PV watcher),
        // NOT `OnChange` polling of every member PV. The `period`
        // argument is the trigger-watcher poll interval (debounce),
        // matching the `poll_interval` of `SaveStrategy::Triggered`.
        for def in &self.triggered_sets {
            // Same as the monitor loop above: the name travels, so an
            // unresolvable request file fails the build instead of
            // impersonating an inline-PV set.
            let request_file = Some(PathBuf::from(&def.filename));
            let save_path = self.resolve_save_file(&def.filename);
            let macros = if def.macros.is_empty() {
                HashMap::new()
            } else {
                MacroContext::parse_inline(&def.macros)
            };
            let strategy = match &def.trigger_pv {
                Some(pv) => SaveStrategy::Triggered {
                    trigger_pv: pv.clone(),
                    mode: TriggerMode::AnyChange,
                    poll_interval: def.period,
                },
                None => {
                    // No trigger PV supplied — fall back to OnChange
                    // polling of member PVs so the set still saves.
                    eprintln!(
                        "create_triggered_set({}): no trigger PV — using OnChange polling",
                        def.filename
                    );
                    SaveStrategy::OnChange {
                        min_interval: def.period,
                        float_epsilon: 0.0,
                    }
                }
            };
            builder = builder.add_set(SaveSetConfig {
                name: format!("{}_triggered", def.filename),
                save_path,
                strategy,
                request_file,
                request_pvs: Vec::new(),
                backup: BackupConfig::default(),
                macros,
                search_paths: self.request_file_paths.clone(),
            });
        }

        builder
    }

    /// Register the 7 autosave iocsh commands that populate this config.
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
                        optional: false,
                    },
                    ArgDesc {
                        name: "pathsub",
                        arg_type: ArgType::String,
                        optional: true,
                    },
                ],
                "set_requestfile_path(path, pathsub) - Add request file search path",
                move |args: &[ArgValue], _ctx: &CommandContext| {
                    let path = match &args[0] {
                        ArgValue::String(s) => s.clone(),
                        _ => return Err("path argument required".into()),
                    };
                    let full_path = match args.get(1) {
                        Some(ArgValue::String(sub)) if !sub.is_empty() => {
                            PathBuf::from(&path).join(sub)
                        }
                        _ => PathBuf::from(&path),
                    };
                    eprintln!("set_requestfile_path: {}", full_path.display());
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
                        optional: false,
                    },
                    ArgDesc {
                        name: "pathsub",
                        arg_type: ArgType::String,
                        optional: true,
                    },
                ],
                "set_savefile_path(path, pathsub) - Set save file directory",
                move |args: &[ArgValue], _ctx: &CommandContext| {
                    let path = match &args[0] {
                        ArgValue::String(s) => s.clone(),
                        _ => return Err("path argument required".into()),
                    };
                    let full_path = match args.get(1) {
                        Some(ArgValue::String(sub)) if !sub.is_empty() => {
                            PathBuf::from(&path).join(sub)
                        }
                        _ => PathBuf::from(&path),
                    };
                    eprintln!("set_savefile_path: {}", full_path.display());
                    if let Err(e) = std::fs::create_dir_all(&full_path) {
                        eprintln!("  warning: could not create directory: {e}");
                    }
                    h.lock().unwrap().save_file_path = Some(full_path);
                    Ok(CommandOutcome::Continue)
                },
            ));
        }

        // create_monitor_set(filename, period, macrostring)
        {
            let h = holder.clone();
            commands.push(CommandDef::new(
                "create_monitor_set",
                vec![
                    ArgDesc {
                        name: "filename",
                        arg_type: ArgType::String,
                        optional: false,
                    },
                    ArgDesc {
                        name: "period",
                        arg_type: ArgType::Int,
                        optional: false,
                    },
                    ArgDesc {
                        name: "macrostring",
                        arg_type: ArgType::String,
                        optional: true,
                    },
                ],
                "create_monitor_set(filename, period, macrostring) - Create periodic save set",
                move |args: &[ArgValue], _ctx: &CommandContext| {
                    let filename = match &args[0] {
                        ArgValue::String(s) => s.clone(),
                        _ => return Err("filename argument required".into()),
                    };
                    let period = match &args[1] {
                        ArgValue::Int(n) => MonitorSetDef::poll_period(*n),
                        _ => return Err("period argument required".into()),
                    };
                    let macros = match args.get(2) {
                        Some(ArgValue::String(s)) => s.clone(),
                        _ => String::new(),
                    };
                    eprintln!(
                        "create_monitor_set: {filename}, period={}s",
                        period.as_secs()
                    );
                    h.lock().unwrap().monitor_sets.push(MonitorSetDef {
                        filename,
                        period,
                        macros,
                        trigger_pv: None,
                    });
                    Ok(CommandOutcome::Continue)
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
                        optional: false,
                    },
                    ArgDesc {
                        name: "trigger_channel",
                        arg_type: ArgType::String,
                        optional: false,
                    },
                    ArgDesc {
                        name: "macrostring",
                        arg_type: ArgType::String,
                        optional: true,
                    },
                ],
                "create_triggered_set(filename, trigger_channel, macrostring) - \
                 Create triggered save set (saves when trigger_channel changes)",
                move |args: &[ArgValue], _ctx: &CommandContext| {
                    let filename = match &args[0] {
                        ArgValue::String(s) => s.clone(),
                        _ => return Err("filename argument required".into()),
                    };
                    let trigger_channel = match &args[1] {
                        ArgValue::String(s) if !s.is_empty() => s.clone(),
                        _ => return Err("trigger_channel argument required".into()),
                    };
                    let macros = match args.get(2) {
                        Some(ArgValue::String(s)) => s.clone(),
                        _ => String::new(),
                    };
                    eprintln!("create_triggered_set: {filename}, trigger={trigger_channel}");
                    h.lock().unwrap().triggered_sets.push(MonitorSetDef {
                        filename,
                        // The command takes no period: the debounce is
                        // whatever the shared owner makes of "unset".
                        period: MonitorSetDef::poll_period(0),
                        macros,
                        trigger_pv: Some(trigger_channel),
                    });
                    Ok(CommandOutcome::Continue)
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
                        optional: false,
                    },
                    ArgDesc {
                        name: "macrostring",
                        arg_type: ArgType::String,
                        optional: true,
                    },
                ],
                "set_pass0_restoreFile(filename, macrostring) - Restore before device support init",
                move |args: &[ArgValue], _ctx: &CommandContext| {
                    let filename = match &args[0] {
                        ArgValue::String(s) => s.clone(),
                        _ => return Err("filename argument required".into()),
                    };
                    let macros = match args.get(1) {
                        Some(ArgValue::String(s)) => s.clone(),
                        _ => String::new(),
                    };
                    eprintln!("set_pass0_restoreFile: {filename}");
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
                        optional: false,
                    },
                    ArgDesc {
                        name: "macrostring",
                        arg_type: ArgType::String,
                        optional: true,
                    },
                ],
                "set_pass1_restoreFile(filename, macrostring) - Restore after device support init",
                move |args: &[ArgValue], _ctx: &CommandContext| {
                    let filename = match &args[0] {
                        ArgValue::String(s) => s.clone(),
                        _ => return Err("filename argument required".into()),
                    };
                    let macros = match args.get(1) {
                        Some(ArgValue::String(s)) => s.clone(),
                        _ => String::new(),
                    };
                    eprintln!("set_pass1_restoreFile: {filename}");
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
                    optional: false,
                }],
                "save_restoreSet_CompatMode(mode) - 'C'/'CRead' for C-readable .sav, \
                 else native",
                move |args: &[ArgValue], _ctx: &CommandContext| {
                    let mode = match &args[0] {
                        ArgValue::String(s) => s.clone(),
                        _ => return Err("mode argument required".into()),
                    };
                    let compat =
                        if mode.eq_ignore_ascii_case("c") || mode.eq_ignore_ascii_case("cread") {
                            CompatMode::CRead
                        } else {
                            CompatMode::Native
                        };
                    eprintln!("save_restoreSet_CompatMode: {compat:?}");
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
                    optional: false,
                }],
                "save_restoreSet_status_prefix(prefix) - Set status PV prefix",
                move |args: &[ArgValue], _ctx: &CommandContext| {
                    let prefix = match &args[0] {
                        ArgValue::String(s) => s.clone(),
                        _ => return Err("prefix argument required".into()),
                    };
                    eprintln!("save_restoreSet_status_prefix: {prefix}");
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
    use crate::server::autosave::save_set::{SaveStrategy, TriggerMode};
    use std::time::Duration;

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
        cfg.triggered_sets.push(MonitorSetDef {
            filename: "settings.req".to_string(),
            period: MonitorSetDef::poll_period(0),
            macros: String::new(),
            trigger_pv: Some("IOC:saveTrigger".to_string()),
        });

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
        match &sets[0].0.config().strategy {
            SaveStrategy::Triggered {
                trigger_pv,
                mode,
                poll_interval,
            } => {
                assert_eq!(trigger_pv.as_str(), "IOC:saveTrigger");
                assert_eq!(*mode, TriggerMode::AnyChange);
                // period 0 clamped to a 1s debounce.
                assert_eq!(*poll_interval, Duration::from_secs(1));
            }
            other => panic!("expected SaveStrategy::Triggered, got {other:?}"),
        }
    }

    /// M3: a triggered set with no trigger PV falls back to
    /// `OnChange` so the set still saves (defensive — the iocsh
    /// command requires the trigger arg, but a programmatic
    /// `MonitorSetDef` could omit it).
    #[test]
    fn triggered_set_without_trigger_pv_falls_back_to_onchange() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = AutosaveStartupConfig::new();
        with_request_file(&mut cfg, &dir);
        cfg.triggered_sets.push(MonitorSetDef {
            filename: "settings.req".to_string(),
            period: MonitorSetDef::poll_period(5),
            macros: String::new(),
            trigger_pv: None,
        });

        let builder = cfg.into_builder();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mgr = rt.block_on(builder.build());
        match &mgr.sets()[0].0.config().strategy {
            SaveStrategy::OnChange { min_interval, .. } => {
                assert_eq!(*min_interval, Duration::from_secs(5));
            }
            other => panic!("expected OnChange fallback, got {other:?}"),
        }
    }
}
