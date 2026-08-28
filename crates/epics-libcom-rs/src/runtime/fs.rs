//! Filesystem operations through the runtime seam.
//!
//! The counterpart of [`crate::runtime::task`]'s time seam, for the same
//! reason (decision A2): no site in this crate names `tokio::fs` directly.
//!
//! # Why `tokio::fs` is not portable here
//!
//! `tokio::fs` is not async file IO — there is no such thing on a POSIX
//! filesystem. Every call is a blocking `std::fs` call that tokio hands to its
//! `spawn_blocking` pool, so each one requires an entered tokio runtime and
//! panics without one:
//!
//! ```text
//! thread 'cbMedium' panicked at tokio-1.51.1/src/fs/mod.rs:317:11:
//! there is no reactor running, must be called from the context of a
//! Tokio 1.x runtime
//! ```
//!
//! Under `exec_backend` the background executor runs callbacks on its own std
//! threads, which are not tokio runtime threads, so every `tokio::fs` call
//! reached from a callback took the IOC's autosave writer down with it — and
//! because the RTEMS build unwinds rather than aborts, it took *only* that
//! thread down, leaving an IOC that looked healthy and had silently stopped
//! saving.
//!
//! These wrappers do the same thing tokio does — `std::fs` on a blocking
//! worker — but through [`crate::runtime::task::spawn_blocking`], which both
//! backends implement. On the RTEMS backend that is a callback-pool worker.
//!
//! # Composite operations belong in one closure
//!
//! Prefer doing a whole sequence inside a single [`blocking`] call over
//! chaining these wrappers. An open-write-sync-rename sequence built from four
//! awaits makes four hops through the worker pool and lets unrelated work
//! interleave between the steps; the same sequence in one closure makes one
//! hop and keeps the durability ordering where a reader can see it.

use std::io;
use std::path::{Path, PathBuf};

/// Run `f` on a blocking worker, flattening the join error into `io::Error`.
///
/// A join error means the worker panicked or was cancelled. There is no
/// meaningful filesystem answer in that case, so it surfaces as
/// [`io::ErrorKind::Other`] rather than being unwrapped into a second panic on
/// the caller's thread.
pub async fn blocking<F, T>(f: F) -> io::Result<T>
where
    F: FnOnce() -> io::Result<T> + Send + 'static,
    T: Send + 'static,
{
    match crate::runtime::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(e) => Err(io::Error::other(format!(
            "filesystem worker did not complete: {e}"
        ))),
    }
}

/// [`std::fs::read_to_string`] on a blocking worker.
pub async fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    let path = path.as_ref().to_path_buf();
    blocking(move || std::fs::read_to_string(path)).await
}

/// [`std::fs::write`] on a blocking worker.
pub async fn write(path: impl AsRef<Path>, contents: impl Into<Vec<u8>>) -> io::Result<()> {
    let path = path.as_ref().to_path_buf();
    let contents = contents.into();
    blocking(move || std::fs::write(path, contents)).await
}

/// [`std::fs::copy`] on a blocking worker.
pub async fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<u64> {
    let from = from.as_ref().to_path_buf();
    let to = to.as_ref().to_path_buf();
    blocking(move || std::fs::copy(from, to)).await
}

/// [`std::fs::rename`] on a blocking worker.
pub async fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    let from = from.as_ref().to_path_buf();
    let to = to.as_ref().to_path_buf();
    blocking(move || std::fs::rename(from, to)).await
}

/// [`std::fs::canonicalize`] on a blocking worker.
pub async fn canonicalize(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    let path = path.as_ref().to_path_buf();
    blocking(move || std::fs::canonicalize(path)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[epics_macros_rs::epics_test]
    async fn a_round_trip_goes_through_the_seam() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seam.txt");
        write(&path, "hello").await.unwrap();
        assert_eq!(read_to_string(&path).await.unwrap(), "hello");
    }

    /// The error must be the filesystem's, not a wrapper's: callers switch on
    /// `ErrorKind::NotFound` (autosave treats a missing `.sav` as "no saved
    /// state" and anything else as corruption), so flattening must not lose it.
    #[epics_macros_rs::epics_test]
    async fn a_missing_file_keeps_its_error_kind() {
        let dir = tempfile::tempdir().unwrap();
        let e = read_to_string(dir.path().join("absent")).await.unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
    }

    #[epics_macros_rs::epics_test]
    async fn rename_and_copy_move_the_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        let c = dir.path().join("c");
        write(&a, "payload").await.unwrap();
        rename(&a, &b).await.unwrap();
        assert!(!a.exists());
        copy(&b, &c).await.unwrap();
        assert_eq!(read_to_string(&c).await.unwrap(), "payload");
    }

    #[epics_macros_rs::epics_test]
    async fn canonicalize_resolves_to_an_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("real");
        write(&path, "x").await.unwrap();
        assert!(canonicalize(&path).await.unwrap().is_absolute());
    }

    /// A composite sequence is one hop, and its `io::Error` reaches the caller
    /// unchanged — this is the shape `write_save_file` uses.
    #[epics_macros_rs::epics_test]
    async fn a_composite_closure_returns_its_own_error() {
        let e = blocking(|| std::fs::read_to_string("/definitely/not/here"))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
    }

    /// No wrapper in this module may name the crate it replaces: that is the
    /// whole point of it existing, and one that reached for the convenient
    /// spelling would reintroduce the panic it was written to remove.
    ///
    /// Bounded to the production half — from the imports up to this test
    /// module — for the reason this file exists to document: the header prose
    /// above and this test's own doc comment both name the forbidden spelling
    /// in order to explain it, so a whole-file search would match the
    /// explanation and fail. Measured; it failed exactly that way first time.
    #[test]
    fn the_seam_does_not_name_the_crate_it_replaces() {
        let src = include_str!("fs.rs");
        let code = src
            .split_once("use std::io;")
            .expect("the module still starts its code with the imports")
            .1;
        let production = code
            .split_once(concat!("#[cfg", "(test)]"))
            .expect("the test module is still the tail of this file")
            .0;
        assert!(!production.contains(concat!("tokio", "::fs")));
    }
}
