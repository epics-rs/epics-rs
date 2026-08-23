//! Helpers shared across `interop_pvxs.rs` sub-modules.
//!
//! pvxs binaries are not on PATH by default. We look them up
//! under `<pvxs>/bin/<host-arch>/` (the layout a local build
//! produces), then fall back to PATH. When spawning
//! the binary, prepend `<pvxs>/lib/<host-arch>/` to
//! `DYLD_LIBRARY_PATH` (mac) / `LD_LIBRARY_PATH` (linux) so the
//! bundled libpvxs is found.

#![allow(dead_code)]

pub mod pv_builders;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// The pvxs checkout, via the shared resolver (`PVXS_HOME` overrides it).
///
/// This used to fall back to `$HOME/codes/pvxs`, and before that to a
/// hard-coded `/Users/<name>/codes/pvxs`; on a machine where the checkout
/// lives anywhere else every interop test silently reported SKIP — which
/// nextest prints as a pass. Resolution failure is now fatal, matching the
/// parity guards; a resolved tree whose binaries are simply not built still
/// skips, because that is an absent build artifact rather than a
/// misconfigured path (see `require_pvxs`).
fn pvxs_home() -> PathBuf {
    epics_base_rs::reference::reference_root(epics_base_rs::reference::ReferenceTree::Pvxs)
}

/// Host-arch name pvxs uses for its `bin/` and `lib/`
/// subdirectories. Matches `EPICS_HOST_ARCH`. We only support
/// the platforms the maintainer actually builds on.
pub fn pvxs_arch() -> &'static str {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        "darwin-aarch64"
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        "darwin-x86_64"
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "linux-x86_64"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "linux-aarch64"
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        ""
    }
}

pub fn pvxs_bin_dir() -> PathBuf {
    pvxs_home().join("bin").join(pvxs_arch())
}

pub fn pvxs_lib_dir() -> PathBuf {
    pvxs_home().join("lib").join(pvxs_arch())
}

pub fn pvxs_dbd_dir() -> PathBuf {
    pvxs_home().join("dbd")
}

/// Locate a pvxs binary by name. Returns the full path if found,
/// `None` otherwise. Searches `~/codes/pvxs/bin/<arch>/<name>`
/// first; falls back to PATH via `which`.
pub fn locate_pvxs(name: &str) -> Option<PathBuf> {
    let direct = pvxs_bin_dir().join(name);
    if direct.is_file() {
        return Some(direct);
    }
    Command::new("which")
        .arg(name)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .and_then(|out| {
            if !out.status.success() {
                return None;
            }
            let s = String::from_utf8(out.stdout).ok()?;
            let p = PathBuf::from(s.trim());
            p.is_file().then_some(p)
        })
}

/// Skip the test with a SKIP-prefixed stderr line if the named
/// binary cannot be found.
pub fn require_pvxs(name: &str) -> Option<PathBuf> {
    match locate_pvxs(name) {
        Some(p) => Some(p),
        None => {
            eprintln!(
                "SKIP: pvxs binary `{name}` not found under {:?} or on PATH; \
                 build pvxs in `{:?}` to enable this interop test.",
                pvxs_bin_dir(),
                pvxs_home(),
            );
            None
        }
    }
}

/// Build a `Command` for a pvxs binary, with `DYLD_LIBRARY_PATH`
/// (macOS) / `LD_LIBRARY_PATH` (linux) extended with the bundled
/// libpvxs directory so the binary loads cleanly.
pub fn pvxs_command(bin: &Path) -> Command {
    let mut cmd = Command::new(bin);
    let lib = pvxs_lib_dir();
    if lib.is_dir() {
        let var = if cfg!(target_os = "macos") {
            "DYLD_LIBRARY_PATH"
        } else {
            "LD_LIBRARY_PATH"
        };
        let prev = std::env::var(var).unwrap_or_default();
        let joined = if prev.is_empty() {
            lib.display().to_string()
        } else {
            format!("{}:{}", lib.display(), prev)
        };
        cmd.env(var, joined);
    }
    cmd
}

/// Bind a random localhost port via `TcpListener`, then close it.
/// Caller passes the port to a subprocess that re-binds it.
pub fn pick_localhost_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 0");
    l.local_addr().unwrap().port()
}

/// Spawn a child process the test will tear down when dropped.
pub struct DropChild {
    pub child: Child,
}

impl Drop for DropChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// Convenience aliases — pvxs binary names actually used.
pub const PVXGET: &str = "pvxget";
pub const PVXPUT: &str = "pvxput";
pub const PVXMONITOR: &str = "pvxmonitor";
pub const PVXLIST: &str = "pvxlist";
pub const SOFT_IOC_PVX: &str = "softIocPVX";
