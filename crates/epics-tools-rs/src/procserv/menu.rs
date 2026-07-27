//! Command-key dispatch.
//!
//! C procServ's `clientItem::processInput` is a stateless byte-by-byte
//! scanner that compares each input byte against the configured
//! command keys (`killChar`, `restartChar`, `toggleRestartChar`,
//! `quitChar`, `logoutChar`) and triggers an action immediately on
//! match. There is **no** menu mode / no escape sequences / no FSM
//! state per client — every byte is independent.
//!
//! Crucially, C's scanner is a cascade of *independent* `if` blocks, not
//! a switch: one byte can fire several actions. The kill key on a dead
//! child is the case that matters — C's `!processClass::exists()` block
//! restarts the child (`clientFactory.cc:207-213`) and then the separate,
//! un-`else`d kill block still broadcasts "@@@ Got a kill command" and
//! signals (`:236-240`). [`Action::evaluate`] therefore returns a
//! *sequence* of actions per byte, in C's scan order; a byte that matches
//! no key yields the empty sequence.
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
/// turns each into the appropriate side effect. A keystroke maps to
/// zero or more of these — see [`Action::evaluate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Broadcast the kill notice and send the configured kill signal
    /// to the child. C's kill block (`clientFactory.cc:236-240`) runs
    /// on every kill keystroke, alive child or not; the signal itself
    /// is a no-op without a running child
    /// (`processFactorySendSignal`, `processFactory.cc:279-287`).
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
    /// Every action C's `processInput` would trigger for `byte`, in C's
    /// scan order. `child_alive` is the current child-process state —
    /// the restart/quit bindings only fire when the child is dead
    /// (C's `processClass::exists()` gate, `clientFactory.cc:207`).
    ///
    /// An unbound or non-command byte yields the empty sequence (which
    /// does not allocate).
    pub fn evaluate(byte: u8, keys: &KeyBindings, child_alive: bool) -> Vec<Self> {
        let mut out = Vec::new();
        let matches = |k: Option<u8>| k == Some(byte);

        // C `clientFactory.cc:206-215` — child-shut-down block. BOTH the
        // restart key and the kill key bring the child back; the kill key
        // additionally falls through to the kill block below.
        if !child_alive {
            if matches(keys.restart) || matches(keys.kill) {
                out.push(Self::RestartChild);
            }
            if matches(keys.quit) {
                out.push(Self::QuitServer);
            }
        }

        // `clientFactory.cc:216-219`
        if matches(keys.logout) {
            out.push(Self::LogoutClient);
        }
        // `clientFactory.cc:220-235`
        if matches(keys.toggle_restart) {
            out.push(Self::ToggleRestartMode);
        }
        // `clientFactory.cc:236-241` — not an `else` of the block above:
        // a kill keystroke on a dead child restarts it AND broadcasts.
        if matches(keys.kill) {
            out.push(Self::KillChild);
        }

        out
    }
}

/// Scan a buffer of bytes; return the actions every byte triggers,
/// concatenated in input order. Used by
/// [`super::client::spawn_client`]'s task pair when input arrives from the
/// telnet parser. Callers pass the resulting actions to the
/// supervisor while still echoing the original buffer to other
/// clients (matches C procServ's "act AND echo" behaviour).
pub fn scan(buf: &[u8], keys: &KeyBindings, child_alive: bool) -> Vec<Action> {
    buf.iter()
        .flat_map(|&b| Action::evaluate(b, keys, child_alive))
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
        assert_eq!(Action::evaluate(0x12, &k, true), vec![]);
        // Child dead → restart key fires, and only that.
        assert_eq!(
            Action::evaluate(0x12, &k, false),
            vec![Action::RestartChild]
        );
    }

    /// R7-17: the kill key on a dead child fires BOTH blocks C runs —
    /// the restart (`clientFactory.cc:207-213`) and the un-`else`d kill
    /// block (`:236-240`) whose broadcast monitoring clients script
    /// against. Pre-fix the dispatcher returned one action per byte and
    /// stopped at `RestartChild`, so `@@@ Got a kill command` never
    /// reached the console.
    #[test]
    fn kill_on_dead_child_restarts_and_still_broadcasts() {
        let k = keys();
        assert_eq!(
            Action::evaluate(0x18, &k, false),
            vec![Action::RestartChild, Action::KillChild],
            "C runs the restart block and the kill block for the same byte"
        );
    }

    #[test]
    fn kill_signals_a_live_child() {
        let k = keys();
        // Child alive → the child-shut-down block is skipped entirely;
        // only the kill block runs.
        assert_eq!(Action::evaluate(0x18, &k, true), vec![Action::KillChild]);
    }

    #[test]
    fn kill_restarts_dead_child_even_when_restart_key_disabled() {
        // C ORs the two keys independently, so killChar still restarts
        // a dead child even if restartChar is unbound.
        let mut k = keys();
        k.restart = None;
        assert_eq!(
            Action::evaluate(0x18, &k, false),
            vec![Action::RestartChild, Action::KillChild]
        );
    }

    #[test]
    fn unbound_key_yields_no_actions() {
        let mut k = keys();
        k.kill = None;
        assert_eq!(Action::evaluate(0x18, &k, true), vec![]);
        // With the kill key unbound, a dead child does not restart on it
        // either — C's OR has no second operand to match.
        assert_eq!(Action::evaluate(0x18, &k, false), vec![]);
    }

    /// C compares each byte against every key independently, so an
    /// operator who binds one char to two functions gets both. Pinned
    /// because the old switch-shaped dispatcher silently dropped the
    /// second.
    #[test]
    fn one_byte_bound_to_two_keys_fires_both() {
        let mut k = keys();
        k.logout = Some(0x14); // same char as toggle_restart
        assert_eq!(
            Action::evaluate(0x14, &k, true),
            vec![Action::LogoutClient, Action::ToggleRestartMode]
        );
    }

    #[test]
    fn scan_buffer_concatenates_per_byte_actions() {
        let k = keys();
        // Live child: plain bytes contribute nothing, the kill key one action.
        assert_eq!(scan(&[b'a', 0x18, b'b'], &k, true), vec![Action::KillChild]);
        // Dead child: the same kill byte contributes two, in C's order.
        assert_eq!(
            scan(&[b'a', 0x18, b'b'], &k, false),
            vec![Action::RestartChild, Action::KillChild]
        );
    }
}
