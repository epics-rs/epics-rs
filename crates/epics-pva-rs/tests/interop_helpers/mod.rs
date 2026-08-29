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

/// The TLS-enabled pvxs build, when this host has one.
///
/// pvxs master still links without OpenSSL — the feature lives on the
/// upstream `tls` branch, so it is a *separate* checkout from
/// [`pvxs_home`] and a host without it is normal, which is why this
/// returns `None` instead of resolving through
/// `epics_base_rs::reference` (that resolver is deliberately fatal).
///
/// `PVXS_TLS_HOME` names it; otherwise we walk the ancestors of this
/// crate for a `pvxs-tls` sibling, the convention `reference` uses,
/// rather than the `$HOME/codes/pvxs-tls` that `tls_interop` and
/// `tls_mtls` each hard-coded — on a host that keeps it anywhere else
/// both tests skipped, and nextest prints a skip as a pass.
pub fn pvxs_tls_home() -> Option<PathBuf> {
    const CANDIDATES: &[&str] = &[
        "pvxs-tls",
        "codes/pvxs-tls",
        "work/pvxs-tls",
        "epics-modules/pvxs-tls",
        "work/epics-modules/pvxs-tls",
    ];
    // A built tree is the sentinel: the point of this checkout is the
    // binaries, and an unbuilt one is as useless as an absent one.
    let built = |root: PathBuf| root.join("bin").join(pvxs_arch()).is_dir().then_some(root);
    if let Ok(explicit) = std::env::var("PVXS_TLS_HOME") {
        return built(PathBuf::from(explicit));
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .flat_map(|a| CANDIDATES.iter().map(move |c| a.join(c)))
        .find_map(built)
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
/// `None` otherwise. Searches the resolved tree's `bin/<arch>/<name>`
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

/// Skip the test with a SKIP-prefixed stderr line when the pvxs build
/// this suite resolves has no TLS support.
///
/// pvxs master still links without OpenSSL, so a host can have every
/// binary an interop test needs and still be unable to speak TLS. Until
/// this was asked up front the absence only surfaced later — as a TLS
/// port that never opened, or a handshake that timed out — and those
/// look exactly like our own client or config being wrong, so the tests
/// returned early and reported green over both. Answering it here is
/// what lets every path after the gate fail instead of skip.
///
/// `pvxinfo -D` on a TLS-enabled build lists `EPICS_PVA_TLS_KEYCHAIN`
/// (the variable is dumped even when it is unset); a build without
/// OpenSSL has no TLS entry at all. The probe deliberately runs the same
/// tree [`locate_pvxs`] resolves, not [`pvxs_tls_home`], so it describes
/// the binaries the caller will actually spawn.
pub fn require_pvxs_tls() -> bool {
    let Some(pvxinfo) = locate_pvxs("pvxinfo") else {
        eprintln!(
            "SKIP: pvxs binary `pvxinfo` not found under {:?} or on PATH, so \
             this host cannot be probed for TLS support.",
            pvxs_bin_dir(),
        );
        return false;
    };
    let out = match pvxs_command(&pvxinfo)
        .arg("-D")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(out) => out,
        Err(e) => panic!("`pvxinfo` at {} would not run: {e}", pvxinfo.display()),
    };
    let dump =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    if dump.contains("EPICS_PVA_TLS_KEYCHAIN") {
        return true;
    }
    eprintln!(
        "SKIP: the pvxs build at {:?} has no TLS support (`pvxinfo -D` does \
         not mention EPICS_PVA_TLS_KEYCHAIN); rebuild pvxs against OpenSSL, \
         or point PVXS_HOME at a build that has it, to run this test.",
        pvxs_home(),
    );
    false
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

/// A helper's "I am listening" handshake file, on a path no other process can
/// pick.
///
/// The five call sites each spelled this as
/// `std::env::temp_dir().join(format!("<tag>.{port}"))`, which is machine-global
/// twice over: `/tmp` is shared by every checkout and every concurrent panel on
/// the host, and the only thing distinguishing one test's file from another's
/// was an ephemeral port that the OS is free to hand to a different process a
/// moment later. A stale file left by a killed run — or a live one from a
/// sibling worktree — reads as "the helper is up" before it has bound anything,
/// and the test then talks to a port nobody is listening on and times out.
///
/// The path is minted from the pid and a counter instead, under this checkout's
/// own `target/tmp` ([`helper_out_dir`]), so it is unique by construction rather
/// than unique by luck, and `Drop` removes it on every exit path including a
/// panicking one.
pub struct ReadyFile {
    path: PathBuf,
}

impl ReadyFile {
    pub fn new(tag: &str) -> Self {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = helper_out_dir();
        std::fs::create_dir_all(&dir).ok();
        Self {
            path: dir.join(format!("{tag}.{}.{n}.ready", std::process::id())),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The helper writes the file once its listener is bound.
    pub fn is_up(&self) -> bool {
        self.path.exists()
    }
}

impl Drop for ReadyFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A test's private view of what the Rust server logged while it ran.
///
/// The two tests that assert on server debug output each used to install
/// *the* process-global `tracing` subscriber themselves — `try_init()` with
/// the `Err` discarded, writing into a buffer private to that module. Only
/// the first install in a process takes effect, so when both tests ran in
/// one process the loser's buffer stayed empty and its assertion read that
/// emptiness as evidence: `pipeline_r20` reported
/// `monitor_pipeline_options` as regressed while the parser was fine and
/// the protocol had worked. It failed 8 of 8 runs at `--test-threads` 2 and
/// 4, and passed serially only because libtest orders tests alphabetically
/// and `pipeline_r20` sorts before `put_cross_impl`.
///
/// The subscriber is now installed once, by [`LogCapture::start`], and
/// writes to every live capture. Whoever starts first no longer decides who
/// gets output, and there is no `Err` to discard: a second installer would
/// mean some other code claimed the global, which panics here rather than
/// silently blinding a test.
///
/// Each capture owns a fresh buffer, so `text()` is what was logged since
/// `start()` — no snapshot-the-length-first arithmetic, which was the other
/// half of the old shape.
///
/// What this does NOT give you is isolation from a concurrently running
/// test: events carry no field naming the server that emitted them (see
/// `MONITOR INIT pipeline negotiated`, `server_native/tcp.rs:6938`, which
/// carries `ioid`/`queue_size`/`initial_nack` and nothing about the
/// instance), so a capture can see another test's server too. That direction
/// can only produce a false PASS, never the false FAILURE this type exists
/// to remove.
pub struct LogCapture {
    id: u64,
    buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}

type CaptureList = std::sync::Mutex<Vec<(u64, std::sync::Arc<std::sync::Mutex<Vec<u8>>>)>>;

fn live_captures() -> &'static CaptureList {
    static LIVE: std::sync::OnceLock<CaptureList> = std::sync::OnceLock::new();
    LIVE.get_or_init(Default::default)
}

/// Writes each formatted event into every live capture.
#[derive(Clone, Copy)]
struct FanOut;

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for FanOut {
    type Writer = FanOut;
    fn make_writer(&'a self) -> Self::Writer {
        *self
    }
}

impl std::io::Write for FanOut {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        // One `write` per formatted event, and the registry lock is held
        // across the fan-out, so an event lands whole in every buffer
        // rather than interleaved with a concurrent one.
        let live = live_captures().lock().unwrap_or_else(|e| e.into_inner());
        for (_, buf) in live.iter() {
            buf.lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend_from_slice(bytes);
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LogCapture {
    /// Begin capturing `epics_pva_rs` debug output for this test.
    pub fn start() -> Self {
        static INSTALLED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Register before installing, so the installing test cannot miss an
        // event emitted between the two.
        live_captures()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((id, buf.clone()));

        INSTALLED.get_or_init(|| {
            use tracing_subscriber::{EnvFilter, fmt};
            fmt()
                .with_env_filter(
                    EnvFilter::try_new("epics_pva_rs=debug")
                        .unwrap_or_else(|_| EnvFilter::new("debug")),
                )
                .with_writer(FanOut)
                .with_ansi(false)
                .with_target(true)
                .try_init()
                .expect(
                    "LogCapture must own the process-global tracing subscriber; \
                     something else in this test binary installed one first, which \
                     leaves every capture empty and turns an assertion on server \
                     output into a false regression report",
                );
        });

        Self { id, buf }
    }

    /// Everything `epics_pva_rs` has logged since [`Self::start`].
    pub fn text(&self) -> String {
        let g = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(&g).to_string()
    }
}

impl Drop for LogCapture {
    fn drop(&mut self) {
        let mut live = live_captures().lock().unwrap_or_else(|e| e.into_inner());
        live.retain(|(id, _)| *id != self.id);
    }
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

/// Built helpers, keyed by name, so one process compiles each helper once.
///
/// It is NOT the concurrency guard, though it was written as one: nextest runs
/// every test in its own PROCESS, so a `static` mutex serialises nothing
/// between the four test modules that want `reverse_server`. What makes the
/// build safe against a concurrent one is [`publish_helper`]'s rename, not this
/// lock.
static CPP_HELPER_CACHE: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<&'static str, PathBuf>>,
> = std::sync::OnceLock::new();

/// Compile `cpp_helpers/<name>.cpp` against the resolved pvxs and base
/// trees and return the binary, panicking if it cannot be produced.
///
/// Nothing here skips. `pvxs_home` / `epics_base_home` already panic on an
/// unresolvable tree; once they resolve, an absent library or a failed
/// compile is a broken checkout, and nextest reads an early return as a
/// pass — which is how this suite reported 25/25 green with six helper
/// builds failing. `name` must be in [`CPP_HELPERS`].
pub fn cpp_helper(name: &'static str) -> PathBuf {
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

/// The compiler driver the helpers build with: `$CXX` when set, else `c++`.
fn cpp_compiler() -> String {
    std::env::var("CXX").unwrap_or_else(|_| "c++".to_string())
}

/// The one gate for "this host has no C++ compiler".
///
/// An absent `c++` is a third-party PREREQUISITE, not an artefact of ours, so
/// it is the single case in this file that stays a visible skip; everything
/// past this gate — a helper source that is missing, a compile that fails, a
/// library that is not built — is our own tree being broken and panics. That
/// split is why [`cpp_helper`] returns a `PathBuf` and not an `Option`: a
/// return type that could mean "no compiler" would also be readable as "the
/// build failed", and a skip reports as a pass.
///
/// Shaped like [`require_pvxs`]: `None` prints its own reason, so a caller's
/// bare `return` is still a visible skip.
pub fn require_cxx() -> Option<String> {
    let driver = cpp_compiler();
    match Command::new(&driver).arg("--version").output() {
        Ok(_) => Some(driver),
        Err(e) => {
            eprintln!(
                "SKIP: no C++ compiler: `{driver}` would not run ({e}); set $CXX \
                 or install one to enable the pvxs interop helpers."
            );
            None
        }
    }
}

/// base ships `include/compiler/clang` and `include/compiler/gcc`, and the
/// two are not interchangeable — naming the wrong one is a compile error,
/// which is exactly what used to turn into a SKIP line. Ask the driver
/// which family it is rather than assuming.
fn base_compiler_include(base: &Path) -> PathBuf {
    let banner = Command::new(cpp_compiler())
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_ascii_lowercase())
        .unwrap_or_default();
    let family = if banner.contains("clang") {
        "clang"
    } else {
        "gcc"
    };
    base.join("include/compiler").join(family)
}

/// Where built helpers are published: this checkout's own `target/tmp`,
/// resolved at COMPILE time.
///
/// This used to be `std::env::var("CARGO_TARGET_TMPDIR")` with a
/// `std::env::temp_dir()` fallback, and the fallback was not a fallback — it
/// was the only arm ever taken. Cargo sets `CARGO_TARGET_TMPDIR` for the
/// COMPILATION of an integration test, not in the test process's environment,
/// so the runtime read is `Err(NotPresent)` (measured, under both nextest and
/// `cargo test`) and every helper was published to `/tmp/<name>`: one path
/// shared by every checkout, every worktree, and every concurrent test process
/// on the host. `env!` reads the same variable where it actually exists.
pub fn helper_out_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
}

/// Move a freshly linked helper onto its published path.
///
/// `rename` within one directory is atomic, which is the whole point: a reader
/// sees the previous complete binary or the new complete one, never the
/// half-written file a linker leaves behind. Linking straight onto the
/// published path — what this did before — let one `c++` truncate a binary
/// another test process was about to `exec`, which surfaces as ETXTBSY, as a
/// helper that dies on startup, or as the test simply timing out waiting for a
/// server that never came up.
fn publish_helper(staged: &Path, out: &Path) {
    if let Err(e) = std::fs::rename(staged, out) {
        let _ = std::fs::remove_file(staged);
        panic!("could not publish {staged:?} as {out:?}: {e}");
    }
}

fn build_cpp_helper(name: &str, defines: &[&str]) -> PathBuf {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/interop_pvxs_mods/cpp_helpers")
        .join(format!("{name}.cpp"));
    assert!(src.is_file(), "C++ interop helper source missing: {src:?}");
    let out_dir = helper_out_dir();
    std::fs::create_dir_all(&out_dir).ok();
    let out = out_dir.join(name);
    let up_to_date = out.is_file()
        && std::fs::metadata(&src).and_then(|m| m.modified()).ok()
            <= std::fs::metadata(&out).and_then(|m| m.modified()).ok();
    if up_to_date {
        return out;
    }
    // Private to this process; `publish_helper` puts it in place. Two processes
    // may still both compile, which wastes a compile and is all it wastes.
    let staged = out_dir.join(format!("{name}.{}.staged", std::process::id()));

    let pvxs = pvxs_home();
    let base = epics_base_home();
    let arch = pvxs_arch();
    let pvxs_lib = pvxs.join("lib").join(arch);
    let base_lib = base.join("lib").join(arch);
    assert!(
        pvxs_lib.is_dir() && base_lib.is_dir(),
        "{name} needs built libraries; {pvxs_lib:?} or {base_lib:?} is absent \
         (the checkouts resolved, so build pvxs and base for {arch})."
    );
    let base_os_include = if cfg!(target_os = "macos") {
        base.join("include/os/Darwin")
    } else {
        base.join("include/os/Linux")
    };

    let status = Command::new(cpp_compiler())
        .args(["-std=c++17", "-O0", "-g"])
        .args(defines)
        .arg(format!("-I{}", pvxs.join("include").display()))
        .arg(format!("-I{}", base.join("include").display()))
        .arg(format!("-I{}", base_compiler_include(&base).display()))
        .arg(format!("-I{}", base_os_include.display()))
        .arg(&src)
        .arg(format!("-L{}", pvxs_lib.display()))
        .arg("-lpvxs")
        .arg(format!("-L{}", base_lib.display()))
        .arg("-lCom")
        .arg(format!("-Wl,-rpath,{}", pvxs_lib.display()))
        .arg(format!("-Wl,-rpath,{}", base_lib.display()))
        .arg("-o")
        .arg(&staged)
        .status();
    match status {
        Ok(s) if s.success() => {
            publish_helper(&staged, &out);
            out
        }
        Ok(s) => {
            let _ = std::fs::remove_file(&staged);
            panic!("{} build of {name} failed (exit {s})", cpp_compiler())
        }
        // `require_cxx` ran the driver moments ago, so a spawn failure here is
        // the toolchain going away mid-run, not an absent prerequisite.
        Err(e) => {
            let _ = std::fs::remove_file(&staged);
            panic!(
                "{} ran for `require_cxx` and now will not: {e}",
                cpp_compiler()
            )
        }
    }
}
