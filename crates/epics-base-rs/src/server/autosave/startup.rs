use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::server::iocsh::registry::{
    ArgDesc, ArgType, ArgValue, CommandContext, CommandDef, CommandOutcome,
};

use super::backup::BackupConfig;
use super::macros::MacroContext;
use super::manager::AutosaveBuilder;
use super::save_set::{SaveSetConfig, SaveStrategy};

/// Definition of a monitor/triggered save set from st.cmd.
#[derive(Debug, Clone)]
pub struct MonitorSetDef {
    pub filename: String,
    pub period_seconds: u32,
    pub macros: String,
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
}

impl AutosaveStartupConfig {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve a request file by searching request_file_paths.
    pub fn resolve_request_file(&self, filename: &str) -> Option<PathBuf> {
        let path = PathBuf::from(filename);
        if path.is_absolute() && path.exists() {
            return Some(path);
        }
        for dir in &self.request_file_paths {
            let candidate = dir.join(filename);
            if candidate.exists() {
                return Some(candidate);
            }
        }
        // Try current directory
        if path.exists() {
            return Some(path);
        }
        None
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
        let mut builder = AutosaveBuilder::new();

        if let Some(ref prefix) = self.status_prefix {
            builder = builder.status_prefix(prefix);
        }

        // Add monitor sets (periodic strategy)
        for def in &self.monitor_sets {
            let request_file = self.resolve_request_file(&def.filename);
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
                    interval: Duration::from_secs(def.period_seconds as u64),
                },
                request_file,
                request_pvs: Vec::new(),
                backup: BackupConfig::default(),
                macros,
                search_paths: self.request_file_paths.clone(),
            });
        }

        // Add triggered sets (OnChange strategy with polling)
        for def in &self.triggered_sets {
            let request_file = self.resolve_request_file(&def.filename);
            let save_path = self.resolve_save_file(&def.filename);
            let macros = if def.macros.is_empty() {
                HashMap::new()
            } else {
                MacroContext::parse_inline(&def.macros)
            };
            builder = builder.add_set(SaveSetConfig {
                name: format!("{}_triggered", def.filename),
                save_path,
                strategy: SaveStrategy::OnChange {
                    min_interval: Duration::from_secs(def.period_seconds as u64),
                    float_epsilon: 0.0,
                },
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
                        ArgValue::Int(n) => *n as u32,
                        _ => return Err("period argument required".into()),
                    };
                    let macros = match args.get(2) {
                        Some(ArgValue::String(s)) => s.clone(),
                        _ => String::new(),
                    };
                    eprintln!("create_monitor_set: {filename}, period={period}s");
                    h.lock().unwrap().monitor_sets.push(MonitorSetDef {
                        filename,
                        period_seconds: period,
                        macros,
                    });
                    Ok(CommandOutcome::Continue)
                },
            ));
        }

        // create_triggered_set(filename, period, macrostring)
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
                "create_triggered_set(filename, period, macrostring) - Create triggered save set",
                move |args: &[ArgValue], _ctx: &CommandContext| {
                    let filename = match &args[0] {
                        ArgValue::String(s) => s.clone(),
                        _ => return Err("filename argument required".into()),
                    };
                    let period = match &args[1] {
                        ArgValue::Int(n) => *n as u32,
                        _ => return Err("period argument required".into()),
                    };
                    let macros = match args.get(2) {
                        Some(ArgValue::String(s)) => s.clone(),
                        _ => String::new(),
                    };
                    eprintln!("create_triggered_set: {filename}, period={period}s");
                    h.lock().unwrap().triggered_sets.push(MonitorSetDef {
                        filename,
                        period_seconds: period,
                        macros,
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
