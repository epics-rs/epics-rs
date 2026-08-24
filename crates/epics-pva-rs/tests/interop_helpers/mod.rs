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

/// The EPICS base checkout, via the same resolver (`EPICS_BASE` overrides it).
///
/// Only the C++ helper builds below need it, and they used to resolve it
/// themselves with a `$HOME/epics/epics-base` fallback — six copies of it.
/// On any host where base lives elsewhere that fallback failed a path
/// check and skipped, so `reverse_server`-backed interop ran nowhere but
/// the author's laptop while reporting green everywhere else.
fn epics_base_home() -> PathBuf {
    epics_base_rs::reference::reference_root(epics_base_rs::reference::ReferenceTree::Base)
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

// ─── C++ interop helpers ─────────────────────────────────────────────

/// The C++ helpers under `cpp_helpers/`, with the extra flags each needs.
///
/// Table-driven so the owner below has no per-helper branch: a helper is
/// its source stem plus its flags, and everything else about the build is
/// the same for all of them.
const CPP_HELPERS: &[(&str, &[&str])] = &[
    ("reverse_server", &[]),
    ("be_reverse_server", &["-DPVXS_ENABLE_EXPERT_API"]),
    ("r20_typed_monitor", &[]),
];

/// Built helpers, keyed by name. Also the build lock: four test modules
/// want `reverse_server`, and before this they each compiled it to the
/// same path, concurrently, from their own copy of this function — one
/// `c++` truncating the binary another was about to exec.
static CPP_HELPER_CACHE: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<&'static str, Option<PathBuf>>>,
> = std::sync::OnceLock::new();

/// Compile `cpp_helpers/<name>.cpp` against the resolved pvxs and base
/// trees and return the binary; `None` with a SKIP line when a build
/// artifact the compile needs is absent (no `c++`, pvxs not built).
///
/// A tree that cannot be *resolved* is not a skip — `pvxs_home` /
/// `epics_base_home` panic — because that is the case that used to report
/// green while running nothing. `name` must be in [`CPP_HELPERS`]; an
/// unknown one is a typo in a test, so it panics rather than skipping.
pub fn cpp_helper(name: &'static str) -> Option<PathBuf> {
    let (_, defines) = CPP_HELPERS
        .iter()
        .find(|(n, _)| *n == name)
        .unwrap_or_else(|| panic!("unknown C++ interop helper {name:?}"));

    let cache = CPP_HELPER_CACHE.get_or_init(Default::default);
    let mut built = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cached) = built.get(name) {
        return cached.clone();
    }
    let out = build_cpp_helper(name, defines);
    built.insert(name, out.clone());
    out
}

fn build_cpp_helper(name: &str, defines: &[&str]) -> Option<PathBuf> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/interop_pvxs_mods/cpp_helpers")
        .join(format!("{name}.cpp"));
    if !src.is_file() {
        eprintln!("SKIP: C++ interop helper source missing: {src:?}");
        return None;
    }
    let out_dir = std::env::var("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    std::fs::create_dir_all(&out_dir).ok();
    let out = out_dir.join(name);
    let up_to_date = out.is_file()
        && std::fs::metadata(&src).and_then(|m| m.modified()).ok()
            <= std::fs::metadata(&out).and_then(|m| m.modified()).ok();
    if up_to_date {
        return Some(out);
    }

    let pvxs = pvxs_home();
    let base = epics_base_home();
    let arch = pvxs_arch();
    let pvxs_lib = pvxs.join("lib").join(arch);
    let base_lib = base.join("lib").join(arch);
    if !pvxs_lib.is_dir() || !base_lib.is_dir() {
        eprintln!(
            "SKIP: {name} needs built libraries; {pvxs_lib:?} or {base_lib:?} is absent \
             (the checkouts resolved, so build pvxs and base for {arch})."
        );
        return None;
    }
    let base_os_include = if cfg!(target_os = "macos") {
        base.join("include/os/Darwin")
    } else {
        base.join("include/os/Linux")
    };

    let status = Command::new("c++")
        .args(["-std=c++17", "-O0", "-g"])
        .args(defines)
        .arg(format!("-I{}", pvxs.join("include").display()))
        .arg(format!("-I{}", base.join("include").display()))
        .arg(format!(
            "-I{}",
            base.join("include/compiler/clang").display()
        ))
        .arg(format!("-I{}", base_os_include.display()))
        .arg(&src)
        .arg(format!("-L{}", pvxs_lib.display()))
        .arg("-lpvxs")
        .arg(format!("-L{}", base_lib.display()))
        .arg("-lCom")
        .arg(format!("-Wl,-rpath,{}", pvxs_lib.display()))
        .arg(format!("-Wl,-rpath,{}", base_lib.display()))
        .arg("-o")
        .arg(&out)
        .status();
    match status {
        Ok(s) if s.success() => Some(out),
        Ok(s) => {
            eprintln!("SKIP: c++ build of {name} failed (exit {s})");
            None
        }
        Err(e) => {
            eprintln!("SKIP: c++ compiler unavailable: {e}");
            None
        }
    }
}
