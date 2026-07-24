//! Small utility types — Rust-idiom analogues of the pvxs `util.h`
//! helpers. Most are thin convenience wrappers rather than
//! load-bearing infrastructure.
//!
//! `pvxs::ServerGUID`, `pvxs::Escaper`, `pvxs::Indented`,
//! `pvxs::Detailed`, `pvxs::SigInt`, `pvxs::Timer`, `pvxs::MPMCFIFO`
//! have named analogues here. Several are deliberately simplified
//! rather than 1:1 — `Detailed` is a bool not a level, `Timer` is
//! periodic-only, `Indented` is value-passing not an RAII stream
//! guard, `ServerGuid` is a plain alias not a distinct type; see each
//! type's docs. The existing internal [`tokio::sync::mpsc`] /
//! [`tokio::time`] facilities cover the "MPMCFIFO" and timer roles for
//! production code paths.
//!
//! These types are intentionally minimal — they exist for code
//! authored against pvxs whose `using namespace pvxs;` translation
//! to Rust expects them to be present. None of them are used on the
//! hot path of the client or server; if you want raw concurrency
//! primitives, prefer `tokio::sync::mpsc` / `parking_lot::Mutex`
//! directly.

// RTEMS-EXEC-MODEL-ALLOW(1): checked - these run and pass in the feature-ON suite.

use std::fmt;

/// Server identity emitted in BEACON / SEARCH_RESPONSE frames.
/// 12-byte opaque token chosen per server-instance. A plain alias, not
/// a distinct type — pvxs `ServerGUID` is a `struct` with its own
/// formatting `operator<<`; the rest of this crate uses bare `[u8; 12]`.
pub type ServerGuid = [u8; 12];

/// `<<` adapter that escapes a C-style string for safe logging.
/// Mirrors `pvxs::Escaper` (`util.cpp` `operator<<`): C short escapes
/// for `\a \b \f \n \r \t \v \\ \' \"`, printable bytes (`0x20..=0x7e`)
/// verbatim, everything else as `\xNN` (lowercase hex).
pub struct Escaper<'a>(pub &'a [u8]);

impl fmt::Display for Escaper<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &b in self.0 {
            match b {
                0x07 => f.write_str("\\a")?, // bell
                0x08 => f.write_str("\\b")?, // backspace
                0x0c => f.write_str("\\f")?, // form feed
                b'\n' => f.write_str("\\n")?,
                b'\r' => f.write_str("\\r")?,
                b'\t' => f.write_str("\\t")?,
                0x0b => f.write_str("\\v")?, // vertical tab
                b'\\' => f.write_str("\\\\")?,
                b'\'' => f.write_str("\\'")?,
                b'"' => f.write_str("\\\"")?,
                0x20..=0x7e => fmt::Write::write_char(f, b as char)?,
                _ => write!(f, "\\x{b:02x}")?,
            }
        }
        Ok(())
    }
}

/// Indent-prefix helper for nested `format()` / `report()` output.
/// Its `Display` writes the leading indent (`level * spaces_per_level`
/// spaces) once — it does **not** see the content, so it cannot
/// reindent interior newlines; emit it at the start of each line
/// yourself (or via [`Indented::prefix`]).
///
/// Use is "stateful inside a single closure" — track the current
/// indent yourself and pass to nested formatters. The pvxs original
/// is an RAII guard that pushes/pops thread-local stream state, which
/// doesn't translate cleanly to Rust's borrow model; this is the
/// value-passing variant.
pub struct Indented {
    pub level: usize,
    pub spaces_per_level: usize,
}

impl Indented {
    pub fn new(level: usize) -> Self {
        Self {
            level,
            spaces_per_level: 2,
        }
    }

    pub fn deeper(&self) -> Self {
        Self {
            level: self.level + 1,
            spaces_per_level: self.spaces_per_level,
        }
    }

    pub fn prefix(&self) -> String {
        " ".repeat(self.level * self.spaces_per_level)
    }
}

impl fmt::Display for Indented {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for _ in 0..(self.level * self.spaces_per_level) {
            f.write_str(" ")?;
        }
        Ok(())
    }
}

/// Toggle "show every sub-field" mode for stream-insertion display.
/// A simplified analogue of `pvxs::Detailed`, which carries an integer
/// detail *level* (`Detailed::level(strm)`); this collapses that to an
/// on/off bool and cannot express graded verbosity. `Display` impls
/// that vary on it should take it as an explicit parameter rather than
/// relying on thread-local state (Rust idiom).
#[derive(Debug, Default, Clone, Copy)]
pub struct Detailed(pub bool);

/// RAII Ctrl-C handler matching pvxs `SigInt`. Wraps
/// `tokio::signal::ctrl_c` so callers can `await` it like a one-shot.
///
/// Like pvxs's `SigInt`, the trap is undone on drop: dropping the
/// `SigInt` aborts the background signal task. The task holds only the
/// shared `Notify` (not the `SigInt` itself), so there is no
/// strong-reference cycle keeping it alive.
///
/// Must be constructed inside a Tokio runtime — [`SigInt::new`] spawns a
/// task, so calling it with no reactor panics (standard `tokio::spawn`
/// precondition).
pub struct SigInt {
    triggered: std::sync::Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl SigInt {
    pub fn new() -> std::sync::Arc<Self> {
        let triggered = std::sync::Arc::new(tokio::sync::Notify::new());
        let notify = triggered.clone();
        let task = tokio::spawn(async move {
            // tokio::signal::ctrl_c only succeeds on platforms with
            // signal support (Unix/Windows). Errors are non-fatal:
            // SigInt simply never fires. RTEMS is the same case reached
            // one level earlier: tokio is built there without the
            // `signal` feature at all (it pulls signal-hook-registry,
            // which newlib cannot compile — design doc §8.1), and an
            // embedded IOC has no console interrupt to trap.
            #[cfg(not(target_os = "rtems"))]
            if (tokio::signal::ctrl_c().await).is_ok() {
                notify.notify_waiters();
            }
            #[cfg(target_os = "rtems")]
            drop(notify);
        });
        std::sync::Arc::new(Self { triggered, task })
    }

    /// Block until SIGINT (or Ctrl-C on Windows) is received.
    pub async fn wait(&self) {
        self.triggered.notified().await;
    }
}

impl Drop for SigInt {
    fn drop(&mut self) {
        // Undo the trap: stop the background ctrl_c task so it doesn't
        // outlive the SigInt (pvxs restores the handler in its dtor).
        self.task.abort();
    }
}

/// Periodic timer — a thin wrapper over [`tokio::time::Interval`].
/// Unlike pvxs `Timer` it offers only the periodic role: there is no
/// one-shot constructor and no `cancel()` (drop the `Timer` to stop
/// ticking). Prefer `tokio::time::interval` / `sleep` directly in new
/// code; this exists to give the public surface a named type.
pub struct Timer {
    interval: tokio::time::Interval,
}

impl Timer {
    pub fn periodic(period: std::time::Duration) -> Self {
        Self {
            interval: tokio::time::interval(period),
        }
    }

    pub async fn tick(&mut self) {
        self.interval.tick().await;
    }
}

/// Multi-producer / multi-consumer FIFO. pvxs's
/// `MPMCFIFO` is essentially what `tokio::sync::mpsc` already
/// provides; this newtype is a clarifying re-export rather than
/// a re-implementation.
pub use tokio::sync::mpsc::{
    UnboundedReceiver as MpmcReceiver, UnboundedSender as MpmcSender,
    unbounded_channel as mpmc_fifo,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaper_roundtrips_printable() {
        let s = Escaper(b"hello world").to_string();
        assert_eq!(s, "hello world");
    }

    #[test]
    fn escaper_escapes_nonprintable() {
        let s = Escaper(b"a\xfb\nb").to_string();
        assert_eq!(s, "a\\xfb\\nb");
    }

    #[test]
    fn escaper_escapes_quote_and_backslash() {
        let s = Escaper(b"\"\\").to_string();
        assert_eq!(s, "\\\"\\\\");
    }

    #[test]
    fn escaper_uses_c_short_escapes_like_pvxs() {
        // pvxs util.cpp emits short escapes for these control bytes
        // (not \xNN) and escapes the single quote.
        assert_eq!(Escaper(b"\x07\x08\x0c\x0b").to_string(), "\\a\\b\\f\\v");
        assert_eq!(Escaper(b"'").to_string(), "\\'");
    }

    #[test]
    fn indented_prefix_uses_two_spaces_default() {
        let i = Indented::new(2);
        assert_eq!(i.prefix(), "    ");
    }

    #[test]
    fn indented_deeper_increments_level() {
        let i = Indented::new(1);
        let d = i.deeper();
        assert_eq!(d.level, 2);
        assert_eq!(d.spaces_per_level, i.spaces_per_level);
    }

    #[test]
    fn detailed_default_is_false() {
        let d = Detailed::default();
        assert!(!d.0);
    }

    #[tokio::test]
    async fn sigint_waits_without_spurious_fire_and_drops_clean() {
        let sig = SigInt::new();
        // No Ctrl-C arrives, so wait() must not complete in the window.
        let r = tokio::time::timeout(std::time::Duration::from_millis(50), sig.wait()).await;
        assert!(r.is_err(), "wait() must not fire without a signal");
        // RAII: dropping aborts the background task, must not panic.
        drop(sig);
    }
}
