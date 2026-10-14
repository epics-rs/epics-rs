//! Where this suite's own build artefacts live.
//!
//! Not an interop test — a test of the harness, kept beside the suite it
//! guards. Everything the interop tests write has to be private to this
//! checkout, because the alternative is not theoretical: the C++ helpers were
//! published to `/tmp/<name>` and linked in place, so one `c++` truncated a
//! binary another test process was about to `exec` and the run died with
//! `Text file busy`.

// No exec-model census marker: the file's tests are plain `#[test]`s, so there is no reactor-dependent site here for a marker to vouch for.

use super::interop_helpers::{ReadyFile, helper_out_dir, require_cxx};

/// The published helper must be inside this checkout, not the host's temp
/// directory.
///
/// `std::env::var("CARGO_TARGET_TMPDIR")` reads `Err(NotPresent)` in a test
/// process — cargo sets that variable for the COMPILATION of an integration
/// test, not for the run — so the old `unwrap_or_else(|_| temp_dir())` was
/// taken every time and every helper landed on a path shared by every
/// checkout, every worktree and every concurrent test process on the host.
#[test]
fn a_built_helper_is_published_inside_this_checkout() {
    require_cxx();
    let out = super::interop_helpers::cpp_helper("reverse_server");
    assert!(
        out.starts_with(helper_out_dir()),
        "helper published outside this checkout: {out:?} is not under {:?}",
        helper_out_dir()
    );
    assert!(
        !out.starts_with(std::env::temp_dir()),
        "helper published to the host temp dir: {out:?}"
    );
}

/// Two ready-files minted in one process must not collide, and neither must
/// two minted in different processes — the pid is in the name for the second
/// half, and this test covers the first.
///
/// The five call sites used to spell the path
/// `std::env::temp_dir().join(format!("<tag>.{port}"))`, unique only by the
/// ephemeral port the OS happened to hand out, on a directory shared with every
/// other checkout on the host.
#[test]
fn ready_files_are_unique_and_private_to_this_checkout() {
    let a = ReadyFile::new("probe");
    let b = ReadyFile::new("probe");
    assert_ne!(a.path(), b.path(), "two ready files share one path");
    for f in [&a, &b] {
        assert!(
            f.path().starts_with(helper_out_dir()),
            "ready file outside this checkout: {:?}",
            f.path()
        );
        assert!(!f.is_up(), "a freshly minted ready file already exists");
    }
    // `Drop` is the only remover: the call sites no longer clean up by hand,
    // so a panicking test cannot leave the file behind for the next run to
    // read as "the helper is already up".
    let path = a.path().to_path_buf();
    std::fs::write(&path, b"up").expect("write ready file");
    assert!(a.is_up());
    drop(a);
    assert!(!path.exists(), "Drop did not remove {path:?}");
}

/// A binary from another pvxs tree (the TLS one) loads that tree's libpvxs,
/// not [`super::interop_helpers::pvxs_lib_dir`]'s.
#[test]
fn a_pvxs_command_loads_the_lib_dir_of_its_own_tree() {
    let arch = super::interop_helpers::pvxs_arch();
    let root = tempfile::tempdir().unwrap();
    let lib = root.path().join("lib").join(arch);
    std::fs::create_dir_all(&lib).unwrap();
    let bin = root.path().join("bin").join(arch).join("pvxget");
    let var = if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else {
        "LD_LIBRARY_PATH"
    };
    let cmd = super::interop_helpers::pvxs_command(&bin);
    let set = cmd
        .get_envs()
        .find(|(k, _)| *k == var)
        .and_then(|(_, v)| v)
        .map(|v| v.to_string_lossy().into_owned())
        .unwrap_or_default();
    assert!(
        set.split(':').next() == Some(&*lib.to_string_lossy()),
        "{var}={set:?}, not led by {lib:?}"
    );
}
