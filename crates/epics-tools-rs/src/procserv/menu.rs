//! Command-key dispatch.
//!
//! C procServ's `clientItem::processInput` is a stateless byte-by-byte
//! scanner that compares each input byte against the configured
//! command keys (`killChar`, `restartChar`, `toggleRestartChar`,
//! `quitChar`, `logoutChar`) and triggers an action immediately on
//! match. There is **no** menu mode / no escape sequences / no FSM
//! state per client — every byte is independent.
//!
//! Some bindings only fire when the child is currently shut down
//! (e.g., `restartChar` and `quitChar` are gated on
//! `!processClass::exists()`). The supervisor passes the current
//! child-alive state in via [`Action::evaluate`].
//!
//! All input bytes — including bytes that triggered an action — are
//! still echoed to other connections via SendToAll, so other
//! viewers can see exactly what was typed.

use crate::procserv::config::KeyBindings;

/// Action requested by a single keystroke. The supervisor task
/// turns each into the appropriate side effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// No command — pass byte through to PTY/echo as normal.
    None,
    /// Send the configured kill signal to the child.
    KillChild,
    /// Restart the child once (manual override of policy/holdoff).
    RestartChild,
    /// Cycle the global RestartMode (OnExit → Disabled → OneShot).
    ToggleRestartMode,
    /// Disconnect this client (others stay).
    LogoutClient,
    /// Shut the entire procserv down.
    QuitServer,
}

impl Action {
    /// Evaluate a single byte against the bindings. `child_alive` is
    /// the current child-process state — some commands only fire
    /// when the child is dead (matches C `processClass::exists()`
    /// gate at `clientFactory.cc:207`).
    pub fn evaluate(byte: u8, keys: &KeyBindings, child_alive: bool) -> Self {
        // Order matches C procServ scan order in
        // clientFactory.cc::processInput.

        // Restart / quit only fire when child is shut down. C
        // semantics: if the child is alive, the byte goes through
        // unmodified. If dead, restart/quit are how the user comes
        // back from a manual kill.
        if !child_alive {
            // While the child is shut down, BOTH the restart key and
            // the kill key bring it back: C clientFactory.cc:207-213
            // gates restartOnce() on `(restartChar && byte==restartChar)
            // || (killChar && byte==killChar)` inside the
            // `!processClass::exists()` branch. So a kill keystroke on
            // an already-dead child is a restart, not a signal to a
            // process that no longer exists.
            if let Some(c) = keys.restart
                && byte == c
            {
                return Self::RestartChild;
            }
            if let Some(c) = keys.kill
                && byte == c
            {
                return Self::RestartChild;
            }
            if let Some(c) = keys.quit
                && byte == c
            {
                return Self::QuitServer;
            }
        }

        if let Some(c) = keys.logout
            && byte == c
        {
            return Self::LogoutClient;
        }
        if let Some(c) = keys.toggle_restart
            && byte == c
        {
            return Self::ToggleRestartMode;
        }
        if let Some(c) = keys.kill
            && byte == c
        {
            return Self::KillChild;
        }
        Self::None
    }
}

/// Scan a buffer of bytes; return per-byte actions. Used by
/// [`super::client::ClientConnection`] when input arrives from the
/// telnet parser. Callers pass the resulting actions to the
/// supervisor while still echoing the original buffer to other
/// clients (matches C procServ's "act AND echo" behaviour).
pub fn scan(buf: &[u8], keys: &KeyBindings, child_alive: bool) -> Vec<Action> {
    buf.iter()
        .map(|&b| Action::evaluate(b, keys, child_alive))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> KeyBindings {
        KeyBindings {
            kill: Some(0x18),           // Ctrl-X
            toggle_restart: Some(0x14), // Ctrl-T
            restart: Some(0x12),        // Ctrl-R
            quit: Some(0x11),           // Ctrl-Q
            logout: Some(0x1d),         // Ctrl-]
        }
    }

    #[test]
    fn restart_only_when_child_dead() {
        let k = keys();
        // Child alive → restart key passes through.
        assert_eq!(Action::evaluate(0x12, &k, true), Action::None);
        // Child dead → restart key fires.
        assert_eq!(Action::evaluate(0x12, &k, false), Action::RestartChild);
    }

    #[test]
    fn kill_signals_a_live_child_but_restarts_a_dead_one() {
        let k = keys();
        // Child alive → kill key sends the signal.
        assert_eq!(Action::evaluate(0x18, &k, true), Action::KillChild);
        // Child dead → kill key restarts it (C clientFactory.cc:208-213
        // restarts on restartChar OR killChar while the child is down).
        assert_eq!(Action::evaluate(0x18, &k, false), Action::RestartChild);
    }

    #[test]
    fn kill_restarts_dead_child_even_when_restart_key_disabled() {
        // C ORs the two keys independently, so killChar still restarts
        // a dead child even if restartChar is unbound.
        let mut k = keys();
        k.restart = None;
        assert_eq!(Action::evaluate(0x18, &k, false), Action::RestartChild);
    }

    #[test]
    fn unbound_key_returns_none() {
        let mut k = keys();
        k.kill = None;
        assert_eq!(Action::evaluate(0x18, &k, true), Action::None);
    }

    #[test]
    fn scan_buffer() {
        let k = keys();
        let actions = scan(&[b'a', 0x18, b'b'], &k, true);
        assert_eq!(actions, vec![Action::None, Action::KillChild, Action::None]);
    }
}
