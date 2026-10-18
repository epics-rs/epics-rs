// RTEMS-EXEC-MODEL-ALLOW(3): checked, not waived — sync tests that hand-build their own tokio runtime; all three passed under `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p epics-base-rs --lib server::autosave::startup::`.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::server::database::PvDatabase;
use crate::server::iocsh::registry::{
    ArgDesc, ArgType, ArgValue, CommandContext, CommandDef, CommandOutcome,
};

use super::backup::BackupConfig;
use super::error::AutosaveError;
use super::macros::MacroContext;
use super::manager::{AutosaveBuilder, AutosaveManager};
use super::request::concatenate_paths;
use super::save_set::{
    ListStatus, ManualSource, SaveMethods, SaveSetConfig, SaveStrategy, SrStatus, do_manual_restore,
};
use super::settings::SrSettings;
use super::verify;

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

/// C `MAX_PATH_LEN` (save_restore_common.h:15, `NFS_PATH_LEN`).
const MAX_PATH_LEN: usize = 255;

/// `set_requestfile_path` / `set_savefile_path`'s `path` and `pathsub`,
/// iocsh's NULL as empty: C's `concatenate_paths` skips either.
fn path_args(args: &[ArgValue]) -> Result<(String, String), String> {
    let arg = |i: usize| match args.get(i) {
        Some(ArgValue::String(s)) => s.to_str().map(str::to_owned),
        _ => Ok(String::new()),
    };
    Ok((arg(0)?, arg(1)?))
}

/// A path as C's `MAX_PATH_LEN` buffer holds it.
pub(super) fn c_path(path: &str) -> String {
    let mut end = path.len().min(MAX_PATH_LEN - 1);
    while !path.is_char_boundary(end) {
        end -= 1;
    }
    path[..end].to_string()
}

/// Definition of a restore file from st.cmd: C's `restoreFileListItem`.
#[derive(Debug, Clone)]
pub struct RestoreDef {
    pub filename: String,
    pub macros: String,
    /// C `restoreStatus` and `restoreStatusStr`, which only the boot restore
    /// of the first item naming this file sets (dbrestore.c:801-809).
    pub status: Arc<Mutex<ListStatus>>,
}

impl RestoreDef {
    /// C `set_restoreFile` (dbrestore.c:1111-1118).
    pub fn new(filename: String, macros: String) -> Self {
        Self {
            filename,
            macros,
            status: Arc::new(Mutex::new(ListStatus::new(
                SrStatus::Init,
                "Unknown, probably failed",
            ))),
        }
    }
}

/// One boot-restore item as C `reboot_restore` opens it.
#[derive(Debug, Clone)]
pub struct BootRestore {
    /// The name `set_pass*_restoreFile` was given.
    pub filename: String,
    pub path: PathBuf,
    pub status: Arc<Mutex<ListStatus>>,
    /// The IOC's settings, C `save_restoreNumSeqFiles` and
    /// `save_restoreDatedBackupFiles` among them.
    pub settings: Arc<SrSettings>,
    /// The item's `macrostring`, as its lines are expanded with.
    pub macrostring: String,
    /// C `write_backup` (dbrestore.c:1014-1028): not for a pass-1 file
    /// pass 0 also restores, nor for an absolute name.
    pub write_backup: bool,
}

/// C `manual_save`'s refusals (save_restore.c:702-726), printed: the
/// manager to queue the save to, when there is one. `save_file`,
/// `callback` and `puser` are its other arguments as its trace prints them.
pub(super) fn manual_save_manager(
    holder: &Mutex<AutosaveStartupConfig>,
    request_file: &str,
    save_file: &str,
    callback: &str,
    puser: &str,
) -> Option<Arc<AutosaveManager>> {
    // OP_MSG_FILENAME_SIZE 100
    if request_file.is_empty() || request_file.len() >= 99 {
        println!("manual_save: bad filename");
        return None;
    }
    let (manager, debug) = {
        let cfg = holder.lock().unwrap();
        (cfg.manager.clone(), cfg.settings.debug())
    };
    if debug != 0 {
        println!(
            "manual_save: request_file='{request_file}', save_file='{save_file}', callbackFunction={callback}, puserPvt={puser}"
        );
    }
    if manager.is_none() {
        println!("manual_save: message queue not initialized (no restore set has been created?)");
    }
    manager
}

/// The `op_msg` checks `remove_data_set` and the `reload_*_set`s make
/// before queueing (save_restore.c:2652-2655, :2734-2742).
fn op_msg_ok(func: &str, filename: &str, macrostring: &str) -> bool {
    if filename.is_empty() || filename.len() >= 99 {
        crate::runtime::log::errlog_vprintf(format_args!(
            "../save_restore.c:{func} bad filename\n"
        ));
        return false;
    }
    if macrostring.len() > 99 {
        crate::runtime::log::errlog_vprintf(format_args!(
            "macro string '{macrostring}' is too long for message queue\n"
        ));
        return false;
    }
    true
}

/// C `create_data_set`'s entry trace (save_restore.c:2329-2332).
fn trace_create(settings: &SrSettings, filename: &str, method: &SaveStrategy) {
    if settings.debug() <= 1 {
        return;
    }
    let (period, trigger, mon_period) = match method {
        SaveStrategy::Periodic { interval } => (interval.as_secs(), "NONE", 0),
        SaveStrategy::Triggered { trigger_pv } => (0, trigger_pv.as_str(), 0),
        SaveStrategy::OnChange { min_interval } => (0, "NONE", min_interval.as_secs()),
        SaveStrategy::Manual => (0, "NONE", 0),
    };
    println!(
        "save_restore:create_data_set: file '{filename}', method {:x}, period {period}, trig_chan '{trigger}', mon_period {mon_period}",
        method.method_bit()
    );
}

/// The save task's `op_Reload*` (save_restore.c:1170-1199): the list of
/// `filename` removed and made again with `method`, `None` being a
/// `create_triggered_set` refusal.
fn reload_set(
    holder: &Mutex<AutosaveStartupConfig>,
    ctx: &CommandContext,
    filename: &str,
    macros: &str,
    method: Option<SaveStrategy>,
) {
    let Some(manager) = holder.lock().unwrap().manager.clone() else {
        return;
    };
    let mut status = manager.take_list(filename);
    if status == 0 {
        status = match method {
            Some(method) => {
                trace_create(&holder.lock().unwrap().settings, filename, &method);
                let cfg = holder.lock().unwrap().set_config(filename, macros);
                match ctx.block_on(manager.create_data_set(cfg, method)) {
                    Ok(()) => 0,
                    Err(_) => -1,
                }
            }
            None => {
                crate::runtime::log::errlog_vprintf(format_args!(
                    "save_restore:create_triggered_set: Error: trigger-channel name is required.\n"
                ));
                -1
            }
        };
    }
    let outcome = if status == 0 { "Succeeded" } else { "Failed" };
    manager.note_recent(format!("Reload '{filename}' {outcome}"));
}

/// C `request_manual_restore`'s refusals (save_restore.c:2829-2837),
/// ERRLOGged.
pub(super) fn manual_restore_request_ok(
    filename: &str,
    macrostring: &str,
    settings: &SrSettings,
) -> bool {
    if settings.debug() >= 5 {
        println!("save_restore:request_manual_restore: entry");
    }
    if filename.is_empty() || filename.len() >= 99 {
        crate::runtime::log::errlog_vprintf(format_args!(
            "../save_restore.c:request_manual_restore bad filename\n"
        ));
        return false;
    }
    if macrostring.len() > 99 {
        crate::runtime::log::errlog_vprintf(format_args!(
            "../save_restore.c:request_manual_restore macro string '{macrostring}' is too long for message queue\n"
        ));
        return false;
    }
    true
}

/// The save task's `op_RestoreFromSaveFile` and `op_RestoreFromAsciiFile`
/// (save_restore.c:1137-1159): an ASCII file that restored is then
/// `asVerify`ed, and that is the status.
pub(super) async fn run_manual_restore(
    manager: Option<Arc<AutosaveManager>>,
    settings: &SrSettings,
    db: &PvDatabase,
    path: &Path,
    filename: &str,
    from: ManualSource,
    macrostring: &str,
) -> i32 {
    let debug = settings.debug();
    if debug != 0 {
        println!("save_restore task: calling do_manual_restore('{filename}')");
    }
    let status = match manager {
        Some(mgr) => {
            mgr.manual_restore(db, path, filename, from, macrostring)
                .await
        }
        None => do_manual_restore(db, filename, path, from, macrostring, settings).await,
    };
    if debug > 1 {
        println!("save_restore: manual restore status={status} (0==success)");
    }
    if status == 0 && from == ManualSource::AsciiFile {
        if !Path::new(filename).is_absolute() {
            super::request::concatenated(&path.to_string_lossy(), settings);
        }
        return verify::do_as_verify(db, path, -1, None).await;
    }
    status
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
    /// The manager `iocInit` built from this config. Set by `iocInit`;
    /// from then on the `create_*_set` commands add their set to it
    /// instead of to `sets`, which C's `create_data_set` does at
    /// any time (`save_restore.c`).
    pub manager: Option<Arc<AutosaveManager>>,
    /// C `save_restore.c`'s globals: the `save_restoreSet_*` commands write
    /// them, the save task and the restores read them.
    pub settings: Arc<SrSettings>,
    /// C `buildInfoList`, `autosaveBuild`'s request files.
    pub build_items: Vec<super::req_build::BuildItem>,
    /// C `autosaveBuildInitialized`: the `dbLoadRecords` hook is installed.
    pub build_hooked: bool,
    /// C `save_restoreNFSHostName`, `save_restoreNFSHostAddr` and
    /// `save_restoreNFSMntPoint` (nfs_utils.c:9-11).
    pub nfs: super::nfs::NfsHost,
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

    /// C `concatenate_paths(saveRestoreFilePath, filename)` unless
    /// `filename` is absolute.
    pub fn save_dir_file(&self, filename: &str) -> PathBuf {
        match &self.save_file_path {
            Some(dir) => dir.join(filename),
            None => PathBuf::from(filename),
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
        let macro_string = macros.to_string();
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
            macro_string,
            search_paths: self.request_file_paths.clone(),
        }
    }

    /// The files pass 1 (`pass1`) or pass 0 restores at boot, one per item,
    /// each with the status of the list's first item naming that file
    /// (C `reboot_restore`, dbrestore.c:801-809).
    pub fn boot_restores(&self, pass1: bool) -> Vec<BootRestore> {
        let list = if pass1 {
            &self.pass1_restores
        } else {
            &self.pass0_restores
        };
        list.iter()
            .map(|item| {
                let first = list.iter().find(|i| i.filename == item.filename);
                let status = first.map_or(&item.status, |i| &i.status);
                BootRestore {
                    filename: item.filename.clone(),
                    // `fname` (dbrestore.c:819-823): the name as given, under
                    // the save path unless it is absolute.
                    path: self.save_dir_file(&item.filename),
                    status: status.clone(),
                    settings: self.settings.clone(),
                    macrostring: item.macros.clone(),
                    write_backup: !Path::new(&item.filename).is_absolute()
                        && !(pass1
                            && self
                                .pass0_restores
                                .iter()
                                .any(|i| i.filename == item.filename)),
                }
            })
            .collect()
    }

    /// C `save_restoreShow(verbose)` (save_restore.c:2492-2583).
    pub fn save_restore_show(&self, ctx: &CommandContext, verbose: i32) {
        let settings = &self.settings;
        let get = |v: &std::sync::atomic::AtomicI32| v.load(Ordering::Relaxed);
        let yes = |b: bool| if b { "YES" } else { "NO" };
        let status = self.manager.as_ref().map_or_else(
            || ListStatus::new(SrStatus::Init, ""),
            |manager| manager.sr_status(),
        );
        ctx.print_fmt(format_args!("BEGIN save_restoreShow"));
        ctx.print_fmt(format_args!(
            "  Current date-time (yymmdd-hhmmss): [{}] ",
            super::backup::date_str()
        ));
        ctx.print_fmt(format_args!(
            "  Status: '{}' - '{}'",
            status.code.label(),
            status.text
        ));
        ctx.print_fmt(format_args!("  Debug level: {}", get(&settings.debug)));
        #[cfg(unix)]
        ctx.print_fmt(format_args!(
            "  File permissions: 0{:o}",
            settings.file_permissions.load(Ordering::Relaxed)
        ));
        ctx.print_fmt(format_args!(
            "  Save/restore incomplete save sets? {}",
            yes(settings.incomplete_sets_ok())
        ));
        ctx.print_fmt(format_args!(
            "  Write dated backup files? {}",
            yes(settings.dated_backup_files())
        ));
        ctx.print_fmt(format_args!(
            "  Number of sequence files to maintain: {}",
            get(&settings.num_seq_files)
        ));
        ctx.print_fmt(format_args!(
            "  Time interval between sequence files: {} seconds",
            get(&settings.seq_period_in_seconds)
        ));
        ctx.print_fmt(format_args!(
            "  Time interval between .sav-file write failure and retry: {} seconds",
            get(&settings.retry_seconds)
        ));
        if self.nfs.managed() {
            self.nfs.show(ctx, settings);
        }
        ctx.print_fmt(format_args!("  I/O errors: {}", get(&settings.io_errors)));
        ctx.print_fmt(format_args!("  request file path list:"));
        for path in &self.request_file_paths {
            // `set_requestfile_path` ends each with '/' (:2608).
            let mut path = path.display().to_string();
            if !path.ends_with('/') {
                path.push('/');
            }
            ctx.print_fmt(format_args!("    '{path}'"));
        }
        ctx.print_fmt(format_args!("  save file path:"));
        ctx.print_fmt(format_args!(
            "    '{}'",
            self.save_file_path
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        ));
        match &self.manager {
            Some(manager) => manager.show_lists(ctx.db(), ctx, verbose),
            None => ctx.print_fmt(format_args!(
                "  The save_restore task apparently is not running."
            )),
        }
        ctx.print_fmt(format_args!("reboot-restore status:"));
        self.dbrestore_show(ctx);
        ctx.print_fmt(format_args!("END save_restoreShow"));
    }

    /// C `dbrestoreShow` (dbrestore.c:147-168).
    pub fn dbrestore_show(&self, ctx: &CommandContext) {
        ctx.print_fmt(format_args!(
            "  '     filename     ' -  status  - 'message' - 'macro string'"
        ));
        for (pass, list) in [(0, &self.pass0_restores), (1, &self.pass1_restores)] {
            ctx.print_fmt(format_args!("  pass {pass}:"));
            for item in list {
                let status = item.status.lock().unwrap();
                let (code, text) = (status.code.label(), &status.text);
                if pass == 0 {
                    let macros = if item.macros.is_empty() {
                        "None"
                    } else {
                        &item.macros
                    };
                    ctx.print_fmt(format_args!(
                        "  '{}' - {code} - '{text}' - '{macros}'",
                        item.filename
                    ));
                } else {
                    ctx.print_fmt(format_args!("  '{}' - {code} - '{text}'", item.filename));
                }
            }
        }
    }

    /// Build an AutosaveBuilder from the collected configuration.
    pub fn into_builder(&self) -> AutosaveBuilder {
        let mut builder = AutosaveBuilder::new()
            .settings(self.settings.clone())
            .reboot_statuses(
                self.pass0_restores
                    .iter()
                    .chain(&self.pass1_restores)
                    .map(|item| item.status.clone())
                    .collect(),
            );

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
        trace_create(&holder.lock().unwrap().settings, &filename, &method);
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

        // set_requestfile_path(path, subpath)
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
                        name: "subpath",
                        arg_type: ArgType::String,
                    },
                ],
                "",
                move |args: &[ArgValue], _ctx: &CommandContext| {
                    let (path, pathsub) = path_args(args)?;
                    if path.len() + pathsub.len() > MAX_PATH_LEN - 1 {
                        crate::runtime::log::errlog_vprintf(format_args!(
                            "../save_restore.c:set_requestfile_path 'path'+'pathsub' is too long\n"
                        ));
                        return Ok(CommandOutcome::Continue);
                    }
                    let settings = h.lock().unwrap().settings.clone();
                    let mut full_path = c_path(&concatenate_paths(&path, &pathsub, &settings));
                    if !full_path.is_empty() {
                        if !full_path.ends_with('/') {
                            full_path.push('/');
                        }
                        h.lock()
                            .unwrap()
                            .request_file_paths
                            .push(PathBuf::from(full_path));
                    }
                    Ok(CommandOutcome::Continue)
                },
            ));
        }

        // set_savefile_path(path, subpath)
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
                        name: "subpath",
                        arg_type: ArgType::String,
                    },
                ],
                "",
                move |args: &[ArgValue], _ctx: &CommandContext| {
                    let (path, pathsub) = path_args(args)?;
                    let settings = h.lock().unwrap().settings.clone();
                    let full_path = c_path(&concatenate_paths(&path, &pathsub, &settings));
                    if full_path.is_empty() {
                        return Ok(CommandOutcome::Continue);
                    }
                    let full_path = PathBuf::from(full_path);
                    // Creating the directory is an rs extension; C prints nothing here.
                    let _ = std::fs::create_dir_all(&full_path);
                    h.lock().unwrap().save_file_path = Some(full_path);
                    Ok(CommandOutcome::Continue)
                },
            ));
        }

        // create_periodic_set / create_monitor_set(filename, period, "macro string")
        let period_sets: [(&str, fn(Duration) -> SaveStrategy); 2] = [
            ("create_periodic_set", |interval| SaveStrategy::Periodic {
                interval,
            }),
            ("create_monitor_set", monitor_method),
        ];
        for (name, method) in period_sets {
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
                        name: "macro string",
                        arg_type: ArgType::String,
                    },
                ],
                "",
                move |args: &[ArgValue], ctx: &CommandContext| {
                    // A missing argument is iocsh's NULL or 0, which C
                    // takes as no set and a zero period.
                    let filename = match &args[0] {
                        ArgValue::String(s) => s.to_str()?.to_owned(),
                        _ => String::new(),
                    };
                    let period = match &args[1] {
                        ArgValue::Int(n) => period_arg(*n),
                        _ => period_arg(0),
                    };
                    let macros = match args.get(2) {
                        Some(ArgValue::String(s)) => s.to_str()?.to_owned(),
                        _ => String::new(),
                    };
                    Self::create_data_set(&h, ctx, filename, macros, method(period))
                },
            ));
        }

        // create_triggered_set(filename, trigger_channel, "macro string")
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
                        name: "macro string",
                        arg_type: ArgType::String,
                    },
                ],
                "",
                move |args: &[ArgValue], ctx: &CommandContext| {
                    let trigger_channel = match &args[1] {
                        ArgValue::String(s) if s.to_str().is_ok_and(is_valid_first_pv_char) => {
                            s.to_str()?.to_owned()
                        }
                        // C's refusal, before the filename is looked at: no
                        // set is created at all rather than one that saves on
                        // something else, and the line does not fail
                        // (`save_restore.c:2296-2303`, `:3772-3775`).
                        _ => {
                            crate::runtime::log::errlog_vprintf(format_args!(
                                "save_restore:create_triggered_set: Error: trigger-channel name is required.\n"
                            ));
                            return Ok(CommandOutcome::Continue);
                        }
                    };
                    let filename = match &args[0] {
                        ArgValue::String(s) => s.to_str()?.to_owned(),
                        _ => String::new(),
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
                "",
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

        // set_pass0_restoreFile(file, macrostring)
        {
            let h = holder.clone();
            commands.push(CommandDef::new(
                "set_pass0_restoreFile",
                vec![
                    ArgDesc {
                        name: "file",
                        arg_type: ArgType::String,
                    },
                    ArgDesc {
                        name: "macrostring",
                        arg_type: ArgType::String,
                    },
                ],
                "",
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
                        .push(RestoreDef::new(filename, macros));
                    Ok(CommandOutcome::Continue)
                },
            ));
        }

        // set_pass1_restoreFile(file, macrostring)
        {
            let h = holder.clone();
            commands.push(CommandDef::new(
                "set_pass1_restoreFile",
                vec![
                    ArgDesc {
                        name: "file",
                        arg_type: ArgType::String,
                    },
                    ArgDesc {
                        name: "macrostring",
                        arg_type: ArgType::String,
                    },
                ],
                "",
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
                        .push(RestoreDef::new(filename, macros));
                    Ok(CommandOutcome::Continue)
                },
            ));
        }

        // fdbrestore(filename)
        {
            let h = holder.clone();
            commands.push(CommandDef::new(
                "fdbrestore",
                vec![ArgDesc {
                    name: "filename",
                    arg_type: ArgType::String,
                }],
                "",
                move |args: &[ArgValue], ctx: &CommandContext| {
                    ctx.print_fmt(format_args!("save_restore:fdbrestore:entry"));
                    let filename = match args.first() {
                        Some(ArgValue::String(s)) => s.to_str()?.to_owned(),
                        _ => String::new(),
                    };
                    let settings = h.lock().unwrap().settings.clone();
                    if !manual_restore_request_ok(&filename, "", &settings) {
                        return Ok(CommandOutcome::Continue);
                    }
                    let (path, mgr, settings) = {
                        let cfg = h.lock().unwrap();
                        (
                            cfg.save_dir_file(&filename),
                            cfg.manager.clone(),
                            cfg.settings.clone(),
                        )
                    };
                    let db = ctx.db().clone();
                    ctx.block_on(async move {
                        run_manual_restore(
                            mgr,
                            &settings,
                            &db,
                            &path,
                            &filename,
                            ManualSource::SaveFile,
                            "",
                        )
                        .await
                    });
                    Ok(CommandOutcome::Continue)
                },
            ));
        }

        // fdbrestoreX(filename, macrostring)
        {
            let h = holder.clone();
            commands.push(CommandDef::new(
                "fdbrestoreX",
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
                "",
                move |args: &[ArgValue], ctx: &CommandContext| {
                    let text = |i: usize| match args.get(i) {
                        Some(ArgValue::String(s)) => s.to_str().map(str::to_owned),
                        _ => Ok(String::new()),
                    };
                    let (filename, macrostring) = (text(0)?, text(1)?);
                    let settings = h.lock().unwrap().settings.clone();
                    if !manual_restore_request_ok(&filename, &macrostring, &settings) {
                        return Ok(CommandOutcome::Continue);
                    }
                    let (path, mgr, settings) = {
                        let cfg = h.lock().unwrap();
                        (
                            cfg.save_dir_file(&filename),
                            cfg.manager.clone(),
                            cfg.settings.clone(),
                        )
                    };
                    let db = ctx.db().clone();
                    let status = ctx.block_on(async move {
                        run_manual_restore(
                            mgr,
                            &settings,
                            &db,
                            &path,
                            &filename,
                            ManualSource::AsciiFile,
                            &macrostring,
                        )
                        .await
                    });
                    // No callback from the shell: C's `defaultCallback`.
                    println!("save_restore:defaultCallback:status={status}");
                    Ok(CommandOutcome::Continue)
                },
            ));
        }

        // asVerify(filename, verbose, restoreFileName)
        {
            let h = holder.clone();
            commands.push(CommandDef::new(
                "asVerify",
                vec![
                    ArgDesc {
                        name: "filename",
                        arg_type: ArgType::String,
                    },
                    ArgDesc {
                        name: "verbose",
                        arg_type: ArgType::Int,
                    },
                    ArgDesc {
                        name: "restoreFileName",
                        arg_type: ArgType::String,
                    },
                ],
                "",
                move |args: &[ArgValue], ctx: &CommandContext| {
                    let text = |i: usize| match args.get(i) {
                        Some(ArgValue::String(s)) => s.to_str().map(str::to_owned),
                        _ => Ok(String::new()),
                    };
                    let filename = text(0)?;
                    let verbose = match args.get(1) {
                        Some(ArgValue::Int(n)) => *n as i32,
                        _ => 0,
                    };
                    let restore = text(2)?;
                    let settings = h.lock().unwrap().settings.clone();
                    let debug = settings.debug();
                    if debug >= 5 {
                        println!("save_restore:request_asVerify: entry");
                    }
                    // `request_asVerify` (save_restore.c:2859-2862).
                    if filename.is_empty() || filename.len() >= 99 {
                        crate::runtime::log::errlog_vprintf(format_args!(
                            "../save_restore.c:request_asVerify bad filename\n"
                        ));
                        return Ok(CommandOutcome::Continue);
                    }
                    // op_asVerify (:1239-1247): the restore file is not under
                    // the save path.
                    if debug != 0 {
                        println!("save_restore task: calling do_asVerify('{filename}')");
                    }
                    let path = h.lock().unwrap().save_dir_file(&filename);
                    if !Path::new(&filename).is_absolute() {
                        super::request::concatenated(&path.to_string_lossy(), &settings);
                    }
                    let db = ctx.db().clone();
                    ctx.block_on(async move {
                        let restore = (!restore.is_empty()).then(|| PathBuf::from(restore));
                        verify::do_as_verify(&db, &path, verbose, restore.as_deref()).await
                    });
                    Ok(CommandOutcome::Continue)
                },
            ));
        }

        // manual_save(request file) (save_restore.c:3739-3743): no callback
        // from the shell, so C's `defaultCallback` prints the status.
        {
            let h = holder.clone();
            commands.push(CommandDef::new(
                "manual_save",
                vec![ArgDesc {
                    name: "request file",
                    arg_type: ArgType::String,
                }],
                "",
                move |args: &[ArgValue], ctx: &CommandContext| {
                    let request_file = match args.first() {
                        Some(ArgValue::String(s)) => s.to_str()?.to_owned(),
                        _ => String::new(),
                    };
                    let Some(manager) =
                        manual_save_manager(&h, &request_file, "(null)", "(nil)", "(nil)")
                    else {
                        return Ok(CommandOutcome::Continue);
                    };
                    let path = h.lock().unwrap().save_dir_file("");
                    let db = ctx.db().clone();
                    let status = ctx.block_on(async move {
                        manager.save_file_op(&db, &request_file, "", &path).await
                    });
                    println!("save_restore:defaultCallback:status={status}");
                    Ok(CommandOutcome::Continue)
                },
            ));
        }

        // remove_data_set(filename) (save_restore.c:3821-3825)
        {
            let h = holder.clone();
            commands.push(CommandDef::new(
                "remove_data_set",
                vec![ArgDesc {
                    name: "filename",
                    arg_type: ArgType::String,
                }],
                "",
                move |args: &[ArgValue], _ctx: &CommandContext| {
                    let filename = match args.first() {
                        Some(ArgValue::String(s)) => s.to_str()?.to_owned(),
                        _ => String::new(),
                    };
                    if op_msg_ok("remove_data_set", &filename, "")
                        && let Some(manager) = h.lock().unwrap().manager.clone()
                    {
                        manager.remove_data_set(&filename);
                    }
                    Ok(CommandOutcome::Continue)
                },
            ));
        }

        // reload_periodic_set / reload_monitor_set(filename, period, macro
        // string) (save_restore.c:3827-3856)
        let reload_period_sets: [(&'static str, fn(Duration) -> SaveStrategy); 2] = [
            ("reload_periodic_set", |interval| SaveStrategy::Periodic {
                interval,
            }),
            ("reload_monitor_set", monitor_method),
        ];
        for (name, method) in reload_period_sets {
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
                        name: "macro string",
                        arg_type: ArgType::String,
                    },
                ],
                "",
                move |args: &[ArgValue], ctx: &CommandContext| {
                    let text = |i: usize| match args.get(i) {
                        Some(ArgValue::String(s)) => s.to_str().map(str::to_owned),
                        _ => Ok(String::new()),
                    };
                    let (filename, macros) = (text(0)?, text(2)?);
                    let period = match args.get(1) {
                        Some(ArgValue::Int(n)) => *n,
                        _ => 0,
                    };
                    if op_msg_ok(name, &filename, &macros) {
                        reload_set(
                            &h,
                            ctx,
                            &filename,
                            &macros,
                            Some(method(period_arg(period))),
                        );
                    }
                    Ok(CommandOutcome::Continue)
                },
            ));
        }

        // reload_triggered_set(filename, trigger_channel, macro string)
        // (save_restore.c:3839-3849)
        {
            let h = holder.clone();
            commands.push(CommandDef::new(
                "reload_triggered_set",
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
                        name: "macro string",
                        arg_type: ArgType::String,
                    },
                ],
                "",
                move |args: &[ArgValue], ctx: &CommandContext| {
                    let text = |i: usize| match args.get(i) {
                        Some(ArgValue::String(s)) => s.to_str().map(str::to_owned),
                        _ => Ok(String::new()),
                    };
                    let (filename, trigger, macros) = (text(0)?, text(1)?, text(2)?);
                    if op_msg_ok("reload_triggered_set", &filename, &macros) {
                        // strNcpy into OP_MSG_TRIGGER_SIZE (80)
                        let trigger: String = trigger.chars().take(79).collect();
                        let method =
                            is_valid_first_pv_char(&trigger).then(|| triggered_method(trigger));
                        reload_set(&h, ctx, &filename, &macros, method);
                    }
                    Ok(CommandOutcome::Continue)
                },
            ));
        }

        // reload_manual_set(filename, macrostring) (save_restore.c:3863-3868)
        {
            let h = holder.clone();
            commands.push(CommandDef::new(
                "reload_manual_set",
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
                "",
                move |args: &[ArgValue], ctx: &CommandContext| {
                    let text = |i: usize| match args.get(i) {
                        Some(ArgValue::String(s)) => s.to_str().map(str::to_owned),
                        _ => Ok(String::new()),
                    };
                    let (filename, macros) = (text(0)?, text(1)?);
                    if op_msg_ok("reload_manual_set", &filename, &macros) {
                        reload_set(&h, ctx, &filename, &macros, Some(SaveStrategy::Manual));
                    }
                    Ok(CommandOutcome::Continue)
                },
            ));
        }

        // save_restoreShow(verbose) (save_restore.c:3795-3799)
        {
            let h = holder.clone();
            commands.push(CommandDef::new(
                "save_restoreShow",
                vec![ArgDesc {
                    name: "verbose",
                    arg_type: ArgType::Int,
                }],
                "",
                move |args: &[ArgValue], ctx: &CommandContext| {
                    let verbose = match args.first() {
                        Some(ArgValue::Int(v)) => *v as i32,
                        _ => 0,
                    };
                    h.lock().unwrap().save_restore_show(ctx, verbose);
                    Ok(CommandOutcome::Continue)
                },
            ));
        }

        // dbrestoreShow() (dbrestore.c:1756-1758)
        {
            let h = holder.clone();
            commands.push(CommandDef::new(
                "dbrestoreShow",
                vec![],
                "",
                move |_args: &[ArgValue], ctx: &CommandContext| {
                    h.lock().unwrap().dbrestore_show(ctx);
                    Ok(CommandOutcome::Continue)
                },
            ));
        }

        // makeAutosaveFileFromDbInfo(filename, info_name), makeAutosaveFiles
        // (dbrestore.c:1760-1774)
        commands.push(CommandDef::new(
            "makeAutosaveFileFromDbInfo",
            vec![
                ArgDesc {
                    name: "filename",
                    arg_type: ArgType::String,
                },
                ArgDesc {
                    name: "info_name",
                    arg_type: ArgType::String,
                },
            ],
            "",
            |args: &[ArgValue], ctx: &CommandContext| {
                let arg = |i: usize| match args.get(i) {
                    Some(ArgValue::String(s)) => s.to_str().map(str::to_owned),
                    _ => Ok(String::new()),
                };
                super::req_build::make_autosave_file_from_db_info(ctx, &arg(0)?, &arg(1)?);
                Ok(CommandOutcome::Continue)
            },
        ));
        commands.push(CommandDef::new(
            "makeAutosaveFiles",
            vec![],
            "",
            |_args: &[ArgValue], ctx: &CommandContext| {
                super::req_build::make_autosave_files(ctx);
                Ok(CommandOutcome::Continue)
            },
        ));

        // eraseFile(filename), appendToFile(filename, line)
        // (dbrestore.c:1776-1789)
        commands.push(CommandDef::new(
            "eraseFile",
            vec![ArgDesc {
                name: "filename",
                arg_type: ArgType::String,
            }],
            "",
            |args: &[ArgValue], _ctx: &CommandContext| {
                let filename = match args.first() {
                    Some(ArgValue::String(s)) => s.to_str()?.to_owned(),
                    _ => String::new(),
                };
                super::req_build::erase_file(&filename);
                Ok(CommandOutcome::Continue)
            },
        ));
        commands.push(CommandDef::new(
            "appendToFile",
            vec![
                ArgDesc {
                    name: "filename",
                    arg_type: ArgType::String,
                },
                ArgDesc {
                    name: "line",
                    arg_type: ArgType::String,
                },
            ],
            "",
            |args: &[ArgValue], _ctx: &CommandContext| {
                let arg = |i: usize| match args.get(i) {
                    Some(ArgValue::String(s)) => s.to_str().map(str::to_owned),
                    _ => Ok(String::new()),
                };
                super::req_build::append_to_file(&arg(0)?, &arg(1)?);
                Ok(CommandOutcome::Continue)
            },
        ));

        // autosaveBuild(filename, reqFileSuffix, on) (dbrestore.c:1789-1795)
        {
            let h = holder.clone();
            commands.push(CommandDef::new(
                "autosaveBuild",
                vec![
                    ArgDesc {
                        name: "filename",
                        arg_type: ArgType::String,
                    },
                    ArgDesc {
                        name: "reqFileSuffix",
                        arg_type: ArgType::String,
                    },
                    ArgDesc {
                        name: "on",
                        arg_type: ArgType::Int,
                    },
                ],
                "",
                move |args: &[ArgValue], _ctx: &CommandContext| {
                    let arg = |i: usize| match args.get(i) {
                        Some(ArgValue::String(s)) => s.to_str().map(|s| Some(s.to_owned())),
                        _ => Ok(None),
                    };
                    let on = matches!(args.get(2), Some(ArgValue::Int(n)) if *n as i32 != 0);
                    super::req_build::autosave_build(
                        &h,
                        &arg(0)?.unwrap_or_default(),
                        arg(1)?.as_deref(),
                        on,
                    );
                    Ok(CommandOutcome::Continue)
                },
            ));
        }

        // set_savefile_name(filename, save_filename) (save_restore.c:3745-3750)
        {
            let h = holder.clone();
            commands.push(CommandDef::new(
                "set_savefile_name",
                vec![
                    ArgDesc {
                        name: "filename",
                        arg_type: ArgType::String,
                    },
                    ArgDesc {
                        name: "save_filename",
                        arg_type: ArgType::String,
                    },
                ],
                "",
                move |args: &[ArgValue], _ctx: &CommandContext| {
                    let text = |i: usize| match args.get(i) {
                        Some(ArgValue::String(s)) => s.to_str().map(str::to_owned),
                        _ => Ok(String::new()),
                    };
                    let (filename, save_filename) = (text(0)?, text(1)?);
                    let manager = h.lock().unwrap().manager.clone();
                    match manager {
                        Some(manager) => {
                            manager.set_savefile_name(&filename, &save_filename);
                        }
                        None => crate::runtime::log::errlog_vprintf(format_args!(
                            "  The save_restore task apparently is not running.\n"
                        )),
                    }
                    Ok(CommandOutcome::Continue)
                },
            ));
        }

        // The save_restoreSet_* setters of C's globals (save_restore.c:453-491,
        // 3870-3987), an absent argument being iocsh's 0.
        let setters: [(&'static str, &'static str, fn(&SrSettings, i32)); 11] = [
            ("save_restoreSet_Debug", "level", SrSettings::set_debug),
            (
                "save_restoreSet_NumSeqFiles",
                "numSeqFiles",
                SrSettings::set_num_seq_files,
            ),
            (
                "save_restoreSet_SeqPeriodInSeconds",
                "period",
                SrSettings::set_seq_period_in_seconds,
            ),
            (
                "save_restoreSet_IncompleteSetsOk",
                "ok",
                SrSettings::set_incomplete_sets_ok,
            ),
            (
                "save_restoreSet_DatedBackupFiles",
                "ok",
                SrSettings::set_dated_backup_files,
            ),
            (
                "save_restoreSet_FilePermissions",
                "permissions",
                SrSettings::set_file_permissions,
            ),
            (
                "save_restoreSet_RetrySeconds",
                "seconds",
                SrSettings::set_retry_seconds,
            ),
            (
                "save_restoreSet_UseStatusPVs",
                "ok",
                SrSettings::set_use_status_pvs,
            ),
            (
                "save_restoreSet_CAReconnect",
                "ok",
                SrSettings::set_ca_reconnect,
            ),
            (
                "save_restoreSet_CallbackTimeout",
                "t",
                SrSettings::set_callback_timeout,
            ),
            (
                "save_restoreSet_periodicDatedBackups",
                "periodMinutes",
                SrSettings::set_periodic_dated_backups,
            ),
        ];
        for (name, arg, set) in setters {
            let h = holder.clone();
            commands.push(CommandDef::new(
                name,
                vec![ArgDesc {
                    name: arg,
                    arg_type: ArgType::Int,
                }],
                "",
                move |args: &[ArgValue], _ctx: &CommandContext| {
                    let n = match args.first() {
                        Some(ArgValue::Int(n)) => *n as i32,
                        _ => 0,
                    };
                    let settings = h.lock().unwrap().settings.clone();
                    set(&settings, n);
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
                "",
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
        for bad in [
            ArgValue::String("".into()),
            ArgValue::String(" IOC:trig".into()),
            ArgValue::String("*".into()),
            ArgValue::Missing,
        ] {
            // C's wrapper drops the status, so the line does not fail.
            let outcome = cmd
                .handler
                .call(&[ArgValue::String("settings.req".into()), bad], &ctx);
            assert!(
                matches!(outcome, Ok(CommandOutcome::Continue)),
                "{:?}",
                outcome.err()
            );
        }
        assert!(
            holder.lock().unwrap().sets.is_empty(),
            "a refused call must leave no set behind"
        );
    }

    /// iocsh's NULL filename and 0 period: no set, and the line does not
    /// fail (`save_restore.c:2357-2362`).
    #[test]
    fn create_period_sets_take_missing_arguments_as_c() {
        let holder = Arc::new(Mutex::new(AutosaveStartupConfig::new()));
        let cmds = AutosaveStartupConfig::register_startup_commands(holder.clone());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let bridge = {
            let _guard = rt.enter();
            crate::runtime::task::BlockingBridge::capture()
        };
        let ctx = CommandContext::new(Arc::new(crate::server::database::PvDatabase::new()), bridge);
        for name in ["create_periodic_set", "create_monitor_set"] {
            let cmd = cmds.iter().find(|c| c.name == name).expect("registered");
            for args in [
                [ArgValue::Missing, ArgValue::Int(30), ArgValue::Missing],
                [
                    ArgValue::String("s.req".into()),
                    ArgValue::Missing,
                    ArgValue::Missing,
                ],
            ] {
                let outcome = cmd.handler.call(&args, &ctx);
                assert!(
                    matches!(outcome, Ok(CommandOutcome::Continue)),
                    "{name}: {:?}",
                    outcome.err()
                );
            }
        }
        let h = holder.lock().unwrap();
        assert_eq!(h.sets.len(), 1, "{:?}", h.sets);
        assert_eq!(h.sets[0].filename, "s.req");
    }
}
