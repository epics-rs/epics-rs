//! nfs_utils.c: `save_restoreSet_NFSHost` and the mount it manages, as the
//! Linux build has it (`os/Linux/osdNfs.c`, `MANAGE_MOUNT 0`), where
//! autosave never mounts anything itself.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use crate::server::iocsh::registry::{
    ArgDesc, ArgType, ArgValue, CommandContext, CommandDef, CommandOutcome,
};

use super::AutosaveStartupConfig;
use super::request::concatenate_paths;
use super::settings::SrSettings;
use super::startup::c_path;

#[derive(Debug, Default)]
pub struct NfsHost {
    host_name: String,
    host_addr: String,
    mnt_point: String,
}

/// C `mountFileSystem` / `dismountFileSystem` when autosave does not
/// manage the mount (os/Linux/osdNfs.c:118-121, :140-143).
fn not_managed() -> i32 {
    println!("Autosave is not configured to manage the file-system mount point.");
    -1
}

impl NfsHost {
    /// C `nfs_managed()` (nfs_utils.c:98-101).
    pub fn managed(&self) -> bool {
        !self.host_name.is_empty() && !self.host_addr.is_empty() && !self.mnt_point.is_empty()
    }

    /// C `save_restore_nfs_show()` (nfs_utils.c:103-108).
    pub fn show(&self, ctx: &CommandContext, settings: &SrSettings) {
        ctx.print_fmt(format_args!(
            "  NFS host: '{}'; address:'{}'",
            self.host_name, self.host_addr
        ));
        ctx.print_fmt(format_args!("  NFS mount point:\n    '{}'", self.mnt_point));
        let ok = settings.nfs_ok.load(Ordering::Relaxed) != 0;
        ctx.print_fmt(format_args!(
            "  NFS mount status: {}",
            if ok { "Ok" } else { "Failed" }
        ));
    }

    /// C `do_mount()` (nfs_utils.c:16-29), whose mount always fails here.
    fn do_mount(&self) -> i32 {
        not_managed();
        println!("save_restore: Can't mount '{}'", self.mnt_point);
        -1
    }
}

/// C `save_restoreSet_NFSHost(hostname, address, mntpoint)`
/// (nfs_utils.c:69-96). `None` is C's NULL.
pub fn save_restore_set_nfs_host(
    cfg: &mut AutosaveStartupConfig,
    hostname: &str,
    address: &str,
    mntpoint: Option<&str>,
) -> i32 {
    // Dismount what we mounted, presuming the caller wants a remount.
    if cfg.settings.nfs_ok.load(Ordering::Relaxed) != 0 && !cfg.nfs.mnt_point.is_empty() {
        not_managed();
    }
    cfg.nfs.host_name = c_path(hostname);
    cfg.nfs.host_addr = c_path(address);
    let save_path = cfg
        .save_file_path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    match mntpoint.filter(|m| !m.is_empty()) {
        Some(mntpoint) => {
            cfg.nfs.mnt_point = c_path(mntpoint);
            // A file path already set must begin with the mount point.
            if !save_path.is_empty() && !save_path.starts_with(&cfg.nfs.mnt_point) {
                cfg.save_file_path = Some(PathBuf::from(c_path(&concatenate_paths(
                    &cfg.nfs.mnt_point,
                    &save_path,
                    &cfg.settings,
                ))));
            }
        }
        None if !save_path.is_empty() => cfg.nfs.mnt_point = c_path(&save_path),
        None => {
            println!("save_restore: Mount point not specified, cannot mount NFS.");
            return -1;
        }
    }
    cfg.nfs.do_mount()
}

impl AutosaveStartupConfig {
    /// C `save_restoreNFSRegister` (nfs_utils.c:121-124), the registrar
    /// only `asSupportNfs.dbd` names: an IOC registers these itself, so one
    /// without them has no `save_restoreSet_NFSHost`, as in C.
    pub fn nfs_commands(holder: Arc<Mutex<Self>>) -> Vec<CommandDef> {
        vec![CommandDef::new(
            "save_restoreSet_NFSHost",
            vec![
                ArgDesc {
                    name: "hostname",
                    arg_type: ArgType::String,
                },
                ArgDesc {
                    name: "address",
                    arg_type: ArgType::String,
                },
                ArgDesc {
                    name: "mntpoint",
                    arg_type: ArgType::String,
                },
            ],
            "",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let arg = |i: usize| match args.get(i) {
                    Some(ArgValue::String(s)) => s.to_str().map(|s| Some(s.to_owned())),
                    _ => Ok(None),
                };
                let (hostname, address, mntpoint) = (arg(0)?, arg(1)?, arg(2)?);
                let status = save_restore_set_nfs_host(
                    &mut holder.lock().unwrap(),
                    hostname.as_deref().unwrap_or_default(),
                    address.as_deref().unwrap_or_default(),
                    mntpoint.as_deref(),
                );
                // C `iocshSetError(status)` (nfs_utils.c:116-119).
                Ok(if status == 0 {
                    CommandOutcome::Continue
                } else {
                    CommandOutcome::Failed
                })
            },
        )]
    }
}
