//! Small utility types — Rust-idiom analogues of the pvxs `util.h`
//! helpers. Most are thin convenience wrappers rather than
//! load-bearing infrastructure.
//!
//! `pvxs::ServerGUID`, `pvxs::Escaper`, `pvxs::Indented`,
//! `pvxs::Detailed`, `pvxs::SigInt`, `pvxs::Timer`, `pvxs::MPMCFIFO`
//! map onto the items in this module. The existing internal
//! [`tokio::sync::mpsc`] / [`tokio::time`] facilities cover the
//! "MPMCFIFO" and timer roles for production code paths; this
//! module re-exposes thin newtype wrappers so end-users writing
//! against the public surface have a 1:1 named target.
//!
//! These types are intentionally minimal — they exist for code
//! authored against pvxs whose `using namespace pvxs;` translation
//! to Rust expects them to be present. None of them are used on the
//! hot path of the client or server; if you want raw concurrency
//! primitives, prefer `tokio::sync::mpsc` / `parking_lot::Mutex`
//! directly.

use std::fmt;

/// Server identity emitted in BEACON / SEARCH_RESPONSE frames.
/// 12-byte opaque token chosen per server-instance.
pub type ServerGuid = [u8; 12];

/// `<<` adapter that escapes a C-style string for safe logging.
/// Mirrors `pvxs::Escaper` — replaces non-printable bytes
/// with `\xNN` escapes.
pub struct Escaper<'a>(pub &'a [u8]);

impl fmt::Display for Escaper<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &b in self.0 {
            match b {
                b'\\' => f.write_str("\\\\")?,
                b'"' => f.write_str("\\\"")?,
                b'\n' => f.write_str("\\n")?,
                b'\r' => f.write_str("\\r")?,
                b'\t' => f.write_str("\\t")?,
                0x20..=0x7e => fmt::Write::write_char(f, b as char)?,
                _ => write!(f, "\\x{b:02x}")?,
            }
        }
        Ok(())
    }
}

/// RAII indenter for nested `format()` / `report()` output.
/// Each `Indented` increases the indent level for the
/// duration of its scope; `Display` impls wrap newline boundaries
/// with the configured number of leading spaces.
///
/// Use is "stateful inside a single closure" — track the current
/// indent yourself and pass to nested formatters. The pvxs original
/// uses thread-local state which doesn't translate cleanly to
/// Rust's borrow model; this is the value-passing variant.
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
/// Mirrors `pvxs::Detailed`. Implementations of `Display`
/// that change behaviour based on this flag should accept it as an
/// explicit parameter rather than relying on thread-local state
/// (Rust idiom).
#[derive(Debug, Default, Clone, Copy)]
pub struct Detailed(pub bool);

/// RAII Ctrl-C handler matching pvxs `SigInt`.
/// Wraps the existing `tokio::signal::ctrl_c` so callers
/// can `await` it like a one-shot. Drop handlers in pvxs unregister
/// the SIGINT trap; here we rely on `tokio::signal` lifecycle.
pub struct SigInt {
    pub triggered: tokio::sync::Notify,
}

impl SigInt {
    pub fn new() -> std::sync::Arc<Self> {
        let s = std::sync::Arc::new(Self {
            triggered: tokio::sync::Notify::new(),
        });
        let s_clone = s.clone();
        tokio::spawn(async move {
            // tokio::signal::ctrl_c only succeeds on platforms with
            // signal support (Unix/Windows). Errors are non-fatal:
            // SigInt simply never fires.
            if (tokio::signal::ctrl_c().await).is_ok() {
                s_clone.triggered.notify_waiters();
            }
        });
        s
    }

    /// Block until SIGINT (or Ctrl-C on Windows) is received.
    pub async fn wait(&self) {
        self.triggered.notified().await;
    }
}

/// Single-shot or periodic timer. Thin wrapper over
/// [`tokio::time`]; provided so the public surface has a named
/// type for the role pvxs `Timer` plays.
///
/// Prefer `tokio::time::interval` / `tokio::time::sleep` directly
/// for new code. This wrapper exists for symbol-parity only.
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
}
