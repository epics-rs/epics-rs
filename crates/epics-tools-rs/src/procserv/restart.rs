//! Restart policy for the supervised child.
//!
//! The procserv-specific [`RestartMode`] (3-state cycle controlled by
//! the `toggleRestart` keystroke) lives here. The underlying
//! sliding-window NRESTARTS rate limiter is shared with the rest of
//! the workspace via [`epics_base_rs::runtime::supervise`], where it
//! also serves `ca-gateway-rs::master`.

pub use epics_base_rs::runtime::supervise::{RestartPolicy, RestartTracker};

/// Three-state restart mode that cycles via `toggleRestart` key.
/// Mirrors C procServ `enum RestartMode { restart, norestart, oneshot }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RestartMode {
    /// Default: relaunch the child whenever it exits. C name: `restart`.
    #[default]
    OnExit,
    /// No auto-restart: the child is not relaunched on exit, but the
    /// supervisor stays up (only `OneShot` sets `shutdownServer`; C
    /// `norestart`, procServ.cc:369-371). Toggle back with `^T`.
    Disabled,
    /// One-shot: relaunch once more, then the supervisor exits when the
    /// child next exits (C `oneshot` gated on `firstRun`,
    /// procServ.cc:373-375,656).
    OneShot,
}

impl RestartMode {
    /// Cycle to the next mode (called by `toggleRestart` keybinding).
    /// `OnExit → Disabled → OneShot → OnExit`.
    pub fn next(self) -> Self {
        match self {
            Self::OnExit => Self::Disabled,
            Self::Disabled => Self::OneShot,
            Self::OneShot => Self::OnExit,
        }
    }

    /// Rendered string for status banners (matches C
    /// `restartModeString()` exactly).
    pub fn label(self) -> &'static str {
        match self {
            Self::OnExit => "ON",
            Self::Disabled => "OFF",
            Self::OneShot => "ONESHOT",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_cycles() {
        assert_eq!(RestartMode::OnExit.next(), RestartMode::Disabled);
        assert_eq!(RestartMode::Disabled.next(), RestartMode::OneShot);
        assert_eq!(RestartMode::OneShot.next(), RestartMode::OnExit);
    }

    #[test]
    fn mode_labels_match_c_procserv() {
        assert_eq!(RestartMode::OnExit.label(), "ON");
        assert_eq!(RestartMode::Disabled.label(), "OFF");
        assert_eq!(RestartMode::OneShot.label(), "ONESHOT");
    }
}
