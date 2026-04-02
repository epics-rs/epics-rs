use std::sync::Arc;

use crate::server::iocsh::registry::{
    ArgDesc, ArgType, ArgValue, CommandContext, CommandDef, CommandOutcome,
};

use super::manager::AutosaveManager;
use super::verify;

/// Create iocsh command definitions for autosave.
pub fn autosave_commands(manager: Arc<AutosaveManager>) -> Vec<CommandDef> {
    let mut commands = Vec::new();

    // fdbrestore <set_name>
    {
        let mgr = manager.clone();
        commands.push(CommandDef::new(
            "fdbrestore",
            vec![ArgDesc {
                name: "set_name",
                arg_type: ArgType::String,
                optional: false,
            }],
            "Restore PVs from a save set",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let set_name = match &args[0] {
                    ArgValue::String(s) => s.clone(),
                    _ => return Err("expected string argument".to_string()),
                };
                let mgr = mgr.clone();
                let db = ctx.db().clone();
                ctx.block_on(async move {
                    match mgr.manual_restore(&set_name, &db).await {
                        Ok(result) => {
                            eprintln!(
                                "fdbrestore: restored {} PVs from {}",
                                result.restored,
                                result.source_file.display()
                            );
                            if !result.not_found.is_empty() {
                                eprintln!("  {} PVs not found", result.not_found.len());
                            }
                            if !result.failed_puts.is_empty() {
                                eprintln!("  {} put failures", result.failed_puts.len());
                            }
                        }
                        Err(e) => eprintln!("fdbrestore error: {e}"),
                    }
                });
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // fdbsave <set_name>
    {
        let mgr = manager.clone();
        commands.push(CommandDef::new(
            "fdbsave",
            vec![ArgDesc {
                name: "set_name",
                arg_type: ArgType::String,
                optional: false,
            }],
            "Save PVs for a save set",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let set_name = match &args[0] {
                    ArgValue::String(s) => s.clone(),
                    _ => return Err("expected string argument".to_string()),
                };
                let mgr = mgr.clone();
                let db = ctx.db().clone();
                ctx.block_on(async move {
                    match mgr.manual_save(&set_name, &db).await {
                        Ok(count) => eprintln!("fdbsave: saved {count} PVs"),
                        Err(e) => eprintln!("fdbsave error: {e}"),
                    }
                });
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // fdblist
    {
        let mgr = manager.clone();
        commands.push(CommandDef::new(
            "fdblist",
            vec![],
            "List all save sets and their status",
            move |_args: &[ArgValue], ctx: &CommandContext| {
                let mgr = mgr.clone();
                ctx.block_on(async move {
                    let statuses = mgr.status_all().await;
                    for (name, status) in statuses {
                        let status_str = match status {
                            super::save_set::SaveSetStatus::Idle => "idle".to_string(),
                            super::save_set::SaveSetStatus::Saving => "saving".to_string(),
                            super::save_set::SaveSetStatus::Error(e) => format!("error: {e}"),
                        };
                        eprintln!("  {name}: {status_str}");
                    }
                });
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // asVerify <set_name>
    {
        let mgr = manager.clone();
        commands.push(CommandDef::new(
            "asVerify",
            vec![ArgDesc {
                name: "set_name",
                arg_type: ArgType::String,
                optional: false,
            }],
            "Verify saved vs live values",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let set_name = match &args[0] {
                    ArgValue::String(s) => s.clone(),
                    _ => return Err("expected string argument".to_string()),
                };
                let mgr = mgr.clone();
                let db = ctx.db().clone();
                ctx.block_on(async move {
                    let save_path = mgr
                        .sets()
                        .iter()
                        .find(|(s, _)| s.config().name == set_name)
                        .map(|(s, _)| s.config().save_path.clone());

                    match save_path {
                        Some(path) => match verify::verify(&db, &path).await {
                            Ok(entries) => {
                                eprint!("{}", verify::format_verify_report(&entries));
                            }
                            Err(e) => eprintln!("asVerify error: {e}"),
                        },
                        None => eprintln!("save set '{set_name}' not found"),
                    }
                });
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // asShow <set_name>
    {
        let mgr = manager.clone();
        commands.push(CommandDef::new(
            "asShow",
            vec![ArgDesc {
                name: "set_name",
                arg_type: ArgType::String,
                optional: false,
            }],
            "Show save set configuration",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let set_name = match &args[0] {
                    ArgValue::String(s) => s.clone(),
                    _ => return Err("expected string argument".to_string()),
                };
                if let Some((set, _)) = mgr.sets().iter().find(|(s, _)| s.config().name == set_name)
                {
                    let cfg = set.config();
                    eprintln!("Save set: {}", cfg.name);
                    eprintln!("  Save path: {}", cfg.save_path.display());
                    eprintln!("  Strategy: {:?}", cfg.strategy);
                    if let Some(ref req) = cfg.request_file {
                        eprintln!("  Request file: {}", req.display());
                    }
                    eprintln!("  Inline PVs: {}", cfg.request_pvs.len());
                    eprintln!("  Total PVs: {}", set.pv_names().len());
                    eprintln!(
                        "  Backup: savB={}, seq_files={}, dated={}",
                        cfg.backup.enable_savb, cfg.backup.num_seq_files, cfg.backup.enable_dated
                    );
                } else {
                    eprintln!("save set '{set_name}' not found");
                }
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // asStatus
    {
        let mgr = manager.clone();
        commands.push(CommandDef::new(
            "asStatus",
            vec![],
            "Show overall autosave status",
            move |_args: &[ArgValue], ctx: &CommandContext| {
                let mgr = mgr.clone();
                ctx.block_on(async move {
                    let statuses = mgr.status_all().await;
                    eprintln!("Autosave status:");
                    for (name, status) in statuses {
                        let status_str = match status {
                            super::save_set::SaveSetStatus::Idle => "OK",
                            super::save_set::SaveSetStatus::Saving => "SAVING",
                            super::save_set::SaveSetStatus::Error(_) => "ERROR",
                        };
                        eprintln!("  {name}: {status_str}");
                    }
                });
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    commands
}
