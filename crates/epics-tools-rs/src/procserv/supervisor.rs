//! Central supervisor task — the heart of the procserv daemon.
//!
//! ## Hub-and-spoke architecture
//!
//! C procServ uses a single `poll(2)` loop that iterates a linked
//! list of `connectionItem*`s and dispatches `readFromFd()` /
//! `Send()` virtuals. Output fan-out goes through `SendToAll(buf,
//! count, sender)` which excludes the originator from the
//! party-line.
//!
//! The Rust port keeps the same party-line semantics but maps it
//! onto tokio with a hub-and-spoke shape:
//!
//! ```text
//!                       ┌──────────────────┐
//!                       │   Supervisor     │
//!                       │   (single task)  │
//!                       └────┬────┬────┬───┘
//!     inbound_rx (mpsc)      │    │    │      outbound_tx (mpsc, one per peer)
//!     ┌──────────────────────┘    │    └──────────────────┐
//!     │                           │                       │
//! ┌───▼──────┐               ┌────▼─────┐           ┌─────▼─────┐
//! │ Client A │               │ Client B │           │ ChildPTY  │
//! └──────────┘               └──────────┘           └───────────┘
//! ```
//!
//! When client A types: A's read task → `inbound_tx` → supervisor
//! receives, scans for menu keys, then forwards the bytes to every
//! OTHER peer's `outbound_tx`. The "exclude sender" property comes
//! for free because the supervisor knows which `ClientId` produced
//! the message.
//!
//! When the PTY emits output: child task → `child_rx` → supervisor
//! → all clients' `outbound_tx`s + log file.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::procserv::child::{ChildEvent, ChildHandle, ChildSpec};
use crate::procserv::client::{
    ClientId, ClientMeta, InboundEvent, IncomingClient, OutboundFrame, spawn_client,
};
use crate::procserv::config::ProcServConfig;
use crate::procserv::error::{ProcServError, ProcServResult};
use crate::procserv::menu::{Action, scan as menu_scan};
use crate::procserv::restart::{RestartMode, RestartTracker};
use crate::procserv::sidecar::{
    InfoSnapshot, LogFile, remove_pid_file, render_procserv_info, write_info_file, write_pid_file,
};

/// Top-level handle. Construct via [`Self::new`], drive with [`Self::run`].
pub struct ProcServ {
    config: Arc<ProcServConfig>,
}

impl ProcServ {
    /// Construct from validated config. Does not yet open listeners
    /// or spawn the child — call [`Self::run`].
    pub fn new(config: ProcServConfig) -> ProcServResult<Self> {
        config.validate().map_err(ProcServError::Config)?;
        Ok(Self {
            config: Arc::new(config),
        })
    }

    /// Run until shutdown. Returns when:
    /// - the configured restart policy refuses a respawn (limit hit)
    /// - the user issues the `quit` keystroke
    /// - SIGTERM/SIGINT arrives (only when running with the daemon
    ///   wrapper that wires those into a shutdown signal)
    pub async fn run(self) -> ProcServResult<()> {
        let mut state = SupervisorState::bootstrap(self.config).await?;
        state.event_loop().await
    }
}

/// Internal supervisor state. Owns the roster of clients, the child
/// handle (or `None` when between restarts), the restart tracker,
/// and the inbound mpsc that all peers feed.
struct SupervisorState {
    config: Arc<ProcServConfig>,
    inbound_tx: mpsc::Sender<(ClientId, InboundEvent)>,
    inbound_rx: mpsc::Receiver<(ClientId, InboundEvent)>,
    incoming_rx: mpsc::Receiver<IncomingClient>,
    clients: HashMap<ClientId, ClientEntry>,
    child: Option<ChildSlot>,
    restart_mode: RestartMode,
    restart_tracker: RestartTracker,
    log: Option<LogFile>,
    /// Set after first successful child spawn. Used by `OneShot`
    /// mode: it allows the FIRST exit to be honored by re-checking
    /// the policy (so `OneShot` selected mid-run still launches
    /// once); subsequent exits with `OneShot` exit the supervisor.
    has_run_once: bool,
}

struct ClientEntry {
    out_tx: mpsc::Sender<OutboundFrame>,
    /// Held for future per-client logic (audit, future welcome
    /// banner that includes peer info, host-based ACL). Not yet read
    /// in the hot path so we mark it explicitly to silence the
    /// unused-field lint.
    #[allow(dead_code)]
    meta: ClientMeta,
}

struct ChildSlot {
    handle: ChildHandle,
    rx: mpsc::Receiver<ChildEvent>,
    /// When this child was spawned. The restart holdoff is measured
    /// from here, mirroring C procServ's `_restartTime = holdoffTime +
    /// time(0)` set at fork (`processFactory.cc:188`).
    started_at: tokio::time::Instant,
}

impl SupervisorState {
    async fn bootstrap(config: Arc<ProcServConfig>) -> ProcServResult<Self> {
        let (inbound_tx, inbound_rx) = mpsc::channel::<(ClientId, InboundEvent)>(256);
        let (incoming_tx, incoming_rx) = mpsc::channel::<IncomingClient>(8);

        // Side-cars
        if let Some(p) = &config.logging.pid_path {
            write_pid_file(p, std::process::id() as i32)?;
        }
        let log = if let Some(p) = &config.logging.log_path {
            Some(LogFile::open(p, config.logging.time_format.clone()).await?)
        } else {
            None
        };

        // Listeners — TCP + UNIX in parallel.
        if let Some(addr) = config.listen.tcp_bind {
            let tx = incoming_tx.clone();
            tokio::spawn(async move {
                if let Err(e) = super::listener::run_tcp(addr, false, tx).await {
                    tracing::error!(error = %e, "procserv-rs: TCP listener exited");
                }
            });
        }
        if let Some(path) = config.listen.unix_path.clone() {
            let tx = incoming_tx.clone();
            tokio::spawn(async move {
                if let Err(e) = super::listener::run_unix(path, false, tx).await {
                    tracing::error!(error = %e, "procserv-rs: UNIX listener exited");
                }
            });
        }
        // Drop our copy so listeners' txs are the only owners.
        drop(incoming_tx);

        let mut state = Self {
            restart_mode: config.restart_mode,
            config,
            inbound_tx,
            inbound_rx,
            incoming_rx,
            clients: HashMap::new(),
            child: None,
            restart_tracker: RestartTracker::new(),
            log,
            has_run_once: false,
        };

        // Initial child spawn unless `--wait` (manual start).
        if !state.config.wait_for_manual_start {
            state.respawn_child().await?;
        }

        Ok(state)
    }

    async fn event_loop(&mut self) -> ProcServResult<()> {
        loop {
            // Build a future that resolves when the child sends an
            // event — only if there IS a child. When there isn't,
            // we use `pending` so the select arm is always polling
            // a valid future.
            let child_event = async {
                match self.child.as_mut() {
                    Some(slot) => slot.rx.recv().await,
                    None => std::future::pending().await,
                }
            };

            tokio::select! {
                biased;

                // 1. Inbound event from a client (highest priority —
                //    user-typed bytes shouldn't queue behind PTY
                //    output, especially the kill keystroke).
                Some((peer_id, event)) = self.inbound_rx.recv() => {
                    if self.handle_inbound(peer_id, event).await? {
                        return Ok(()); // quit key
                    }
                }

                // 2. PTY child output.
                ev = child_event => {
                    if let Some(ev) = ev {
                        match self.handle_child_event(ev).await? {
                            ChildLoopOutcome::Continue => {}
                            ChildLoopOutcome::Shutdown => return Ok(()),
                        }
                    } else {
                        // child rx closed but slot still there — drop it
                        self.child = None;
                    }
                }

                // 3. New client accepted.
                Some(incoming) = self.incoming_rx.recv() => {
                    self.handle_new_client(incoming).await;
                }
            }
        }
    }

    /// Handle one inbound event from a client. Returns `Ok(true)`
    /// if the user pressed the quit key (caller should exit the loop).
    async fn handle_inbound(
        &mut self,
        client_id: ClientId,
        event: InboundEvent,
    ) -> ProcServResult<bool> {
        match event {
            InboundEvent::TelnetReply { bytes } => {
                if let Some(entry) = self.clients.get(&client_id) {
                    let _ = entry.out_tx.send(OutboundFrame::RawIac(bytes)).await;
                }
            }
            InboundEvent::Disconnected => {
                self.clients.remove(&client_id);
            }
            InboundEvent::Data { bytes } => {
                let child_alive = self.child.is_some();
                let actions = menu_scan(&bytes, &self.config.keys, child_alive);

                let mut quit = false;
                for action in &actions {
                    match action {
                        Action::None => {}
                        Action::KillChild => {
                            if let Some(slot) = self.child.as_ref() {
                                let _ = slot.handle.signal(self.config.child.kill_signal);
                            }
                        }
                        Action::RestartChild => {
                            // Force a respawn (clears any holdoff).
                            if self.child.is_none() {
                                self.banner("@@@ Manual restart").await;
                                if let Err(e) = self.respawn_child().await {
                                    tracing::error!(error = %e, "procserv-rs: manual respawn failed");
                                }
                            }
                        }
                        Action::ToggleRestartMode => {
                            self.restart_mode = self.restart_mode.next();
                            let msg = format!(
                                "\r\n@@@ Toggled auto restart mode to {}\r\n",
                                self.restart_mode.label()
                            );
                            self.fanout_to_all(msg.as_bytes()).await;
                        }
                        Action::LogoutClient => {
                            if let Some(entry) = self.clients.remove(&client_id) {
                                let _ = entry.out_tx.send(OutboundFrame::Disconnect).await;
                            }
                        }
                        Action::QuitServer => {
                            quit = true;
                        }
                    }
                }

                // Echo / forward the bytes to all peers EXCEPT the
                // sender. Matches C `SendToAll(buf, count, this)`.
                self.fanout_excluding(&bytes, Some(client_id)).await;
                if quit {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Handle one event from the PTY child.
    async fn handle_child_event(&mut self, event: ChildEvent) -> ProcServResult<ChildLoopOutcome> {
        match event {
            ChildEvent::Output(bytes) => {
                // Fan out to all clients (PTY is the sender; clients
                // are everyone else). C semantics: SendToAll(buf,
                // len, this).
                self.fanout_excluding(&bytes, None).await;
                if let Some(log) = &self.log
                    && let Err(e) = log.write_chunk(&bytes).await
                {
                    tracing::warn!(error = %e, "procserv-rs: log write failed");
                }
                Ok(ChildLoopOutcome::Continue)
            }
            ChildEvent::Exited { status } => {
                let started_at = self.child.take().map(|slot| slot.started_at);
                let msg = format!(
                    "\r\n@@@ Child exited (status: {:?})\r\n",
                    status
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "unknown".into())
                );
                self.fanout_to_all(msg.as_bytes()).await;

                match self.restart_mode {
                    RestartMode::OnExit => {
                        match self.restart_tracker.try_record(&self.config.restart) {
                            Ok(()) => {
                                self.holdoff_remaining(started_at).await;
                                self.banner("@@@ Auto restart").await;
                                self.respawn_child().await?;
                                Ok(ChildLoopOutcome::Continue)
                            }
                            Err((max, win)) => Err(ProcServError::RestartLimitExceeded {
                                attempts: max,
                                window_secs: win,
                            }),
                        }
                    }
                    RestartMode::OneShot => {
                        if !self.has_run_once {
                            // First-ever exit under OneShot —
                            // permitted to relaunch once more.
                            self.has_run_once = true;
                            self.holdoff_remaining(started_at).await;
                            self.banner("@@@ One-shot relaunch").await;
                            self.respawn_child().await?;
                            Ok(ChildLoopOutcome::Continue)
                        } else {
                            self.banner("@@@ One-shot mode: exiting").await;
                            Ok(ChildLoopOutcome::Shutdown)
                        }
                    }
                    RestartMode::Disabled => {
                        self.banner("@@@ Auto restart disabled — exiting").await;
                        Ok(ChildLoopOutcome::Shutdown)
                    }
                }
            }
        }
    }

    /// Spawn the configured child and store the handle. Updates
    /// info-file + `PROCSERV_INFO` env var.
    ///
    /// Ordering note: `PROCSERV_INFO` is set BEFORE `ChildHandle::spawn`
    /// so the child inherits it via `execvp`. The pre-spawn render
    /// uses `child_pid: None`; the post-spawn info-file write uses the
    /// real pid. C procServ does the same — the env-var carries
    /// supervisor identity at exec time, the info file (separate
    /// channel) carries the live child pid for `manage-procs`.
    async fn respawn_child(&mut self) -> ProcServResult<()> {
        // 1. Render the env-var info BEFORE fork. child_pid is unknown
        //    at this point (we haven't forked yet); leave it None.
        let pre_spawn_info = InfoSnapshot {
            procserv_pid: std::process::id() as i32,
            child_pid: None,
            child_exe: self.config.child.program.clone(),
            child_args: self.config.child.args.clone(),
        };
        // SAFETY: PROCSERV_INFO is process-wide. Setting env in a
        // running multi-threaded program is racy on POSIX; we accept
        // that risk because (a) only this supervisor task touches it,
        // (b) the child gets a fresh copy via execvp at fork time, so
        // a torn read in another supervisor thread is harmless.
        unsafe { std::env::set_var("PROCSERV_INFO", render_procserv_info(&pre_spawn_info)) };

        // 2. Spawn — child inherits the env var.
        let spec = ChildSpec {
            program: self.config.child.program.clone(),
            args: self.config.child.args.clone(),
            cwd: self.config.child.cwd.clone(),
            ignore_chars: self.config.child.ignore_chars.clone(),
        };
        let (handle, rx) = ChildHandle::spawn(&spec)?;

        // 3. Write info file with the real child_pid for manage-procs.
        let post_spawn_info = InfoSnapshot {
            procserv_pid: pre_spawn_info.procserv_pid,
            child_pid: Some(handle.pid()),
            child_exe: pre_spawn_info.child_exe.clone(),
            child_args: pre_spawn_info.child_args.clone(),
        };
        if let Some(p) = &self.config.logging.info_path {
            let _ = write_info_file(p, &post_spawn_info);
        }

        self.has_run_once = true;
        self.banner(&format!("@@@ Child started (pid {})", handle.pid()))
            .await;
        self.child = Some(ChildSlot {
            handle,
            rx,
            started_at: tokio::time::Instant::now(),
        });
        Ok(())
    }

    /// Roster: register a freshly-accepted client + send the welcome
    /// banner.
    async fn handle_new_client(&mut self, incoming: IncomingClient) {
        let (meta, out_tx) = spawn_client(incoming, self.inbound_tx.clone());
        let banner = self.welcome_banner();
        let _ = out_tx.send(OutboundFrame::Bytes(banner.into_bytes())).await;
        self.clients.insert(
            meta.id,
            ClientEntry {
                out_tx,
                meta: meta.clone(),
            },
        );
        tracing::debug!(client = meta.id.raw(), peer = ?meta.peer, readonly = meta.readonly, "procserv-rs: client connected");
    }

    /// Build the welcome banner per C `clientItem::clientItem`.
    /// Simplified — no `_users`/`_loggers` counts, those would
    /// require additional bookkeeping. Banner content is enough to
    /// orient an operator.
    fn welcome_banner(&self) -> String {
        let mut s = String::new();
        s.push_str("@@@ Welcome to procserv-rs\r\n");
        s.push_str(&format!(
            "@@@ Wrapping: {} (mode: {})\r\n",
            self.config.child.name,
            self.restart_mode.label()
        ));
        if let Some(c) = self.config.keys.kill {
            s.push_str(&format!(
                "@@@ Use ^{} to kill the child\r\n",
                ascii_caret(c)
            ));
        }
        if let Some(c) = self.config.keys.toggle_restart {
            s.push_str(&format!(
                "@@@ Use ^{} to toggle auto restart\r\n",
                ascii_caret(c)
            ));
        }
        if let Some(c) = self.config.keys.logout {
            s.push_str(&format!("@@@ Use ^{} to logout\r\n", ascii_caret(c)));
        }
        s
    }

    /// Send `bytes` to every connected client.
    async fn fanout_to_all(&self, bytes: &[u8]) {
        for entry in self.clients.values() {
            let _ = entry
                .out_tx
                .send(OutboundFrame::Bytes(bytes.to_vec()))
                .await;
        }
    }

    /// Send `bytes` to every connected client except the originator.
    /// `exclude == None` means "send to all" (used for PTY-output
    /// fan-out where the sender is the child, not a client).
    async fn fanout_excluding(&self, bytes: &[u8], exclude: Option<ClientId>) {
        for (id, entry) in &self.clients {
            if Some(*id) == exclude {
                continue;
            }
            let _ = entry
                .out_tx
                .send(OutboundFrame::Bytes(bytes.to_vec()))
                .await;
        }
        // Forward to PTY child too — but only for client-originated
        // bytes (when the child isn't the sender). C semantics: every
        // non-readonly client's input flows through the party-line
        // and the PTY is one of the recipients, putting it on the
        // child's stdin.
        if exclude.is_some()
            && let Some(slot) = self.child.as_ref()
            && let Err(e) = slot.handle.write_stdin(bytes).await
        {
            tracing::debug!(error = %e, "procserv-rs: child stdin write failed");
        }
    }

    /// Sleep out the remainder of the restart holdoff, measured from
    /// the child's start instant. C procServ sets `_restartTime =
    /// holdoffTime + time(0)` when the child is forked
    /// (`processFactory.cc:188`) and `processFactoryNeedsRestart`
    /// relaunches as soon as `now >= _restartTime` — so a child that
    /// ran longer than the holdoff restarts immediately, and only a
    /// fast crash-loop waits out `holdoff - uptime`. Sleeping the full
    /// `holdoff` after every exit (regardless of uptime) penalised a
    /// long-lived child that exited cleanly.
    async fn holdoff_remaining(&self, started_at: Option<tokio::time::Instant>) {
        let remaining = match started_at {
            Some(t) => remaining_holdoff(self.config.holdoff, t.elapsed()),
            None => self.config.holdoff,
        };
        if !remaining.is_zero() {
            tokio::time::sleep(remaining).await;
        }
    }

    /// Convenience: emit a `@@@`-prefixed banner line to all clients.
    async fn banner(&self, text: &str) {
        let mut line = text.trim_end_matches('\n').to_string();
        line.push_str("\r\n");
        self.fanout_to_all(line.as_bytes()).await;
    }
}

#[derive(Debug)]
enum ChildLoopOutcome {
    Continue,
    Shutdown,
}

/// Remaining restart holdoff for a child that ran `uptime` before
/// exiting: `holdoff - uptime`, clamped at zero. Mirrors C
/// `processFactoryNeedsRestart`, which permits a restart once
/// `now >= _restartTime` where `_restartTime = child_start + holdoff`
/// (`processFactory.cc:188`, `processFactory.cc:51-54`).
fn remaining_holdoff(
    holdoff: std::time::Duration,
    uptime: std::time::Duration,
) -> std::time::Duration {
    holdoff.saturating_sub(uptime)
}

/// Format byte `c` for `^c` notation (C `CTL_SC` macro).
fn ascii_caret(c: u8) -> char {
    if c < 32 {
        (c + b'@') as char
    } else {
        c as char
    }
}

impl Drop for SupervisorState {
    fn drop(&mut self) {
        if let Some(p) = &self.config.logging.pid_path {
            remove_pid_file(p);
        }
        if let Some(slot) = self.child.as_ref() {
            // Best-effort: signal the child group on supervisor drop
            // so a panic in the supervisor doesn't leave a zombie.
            let _ = slot.handle.signal(self.config.child.kill_signal);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::remaining_holdoff;
    use std::time::Duration;

    // Boundaries of `_restartTime = child_start + holdoff`: the wait is
    // `holdoff - uptime`, clamped at zero (C processFactory.cc:51-54,188).
    #[test]
    fn holdoff_zero_uptime_waits_full() {
        assert_eq!(
            remaining_holdoff(Duration::from_secs(15), Duration::ZERO),
            Duration::from_secs(15)
        );
    }

    #[test]
    fn holdoff_short_uptime_waits_difference() {
        assert_eq!(
            remaining_holdoff(Duration::from_secs(15), Duration::from_secs(4)),
            Duration::from_secs(11)
        );
    }

    #[test]
    fn holdoff_uptime_equals_holdoff_no_wait() {
        assert_eq!(
            remaining_holdoff(Duration::from_secs(15), Duration::from_secs(15)),
            Duration::ZERO
        );
    }

    #[test]
    fn holdoff_long_uptime_restarts_immediately() {
        // A child that outlived the holdoff restarts with no delay,
        // matching C's `now >= _restartTime` being already true.
        assert_eq!(
            remaining_holdoff(Duration::from_secs(15), Duration::from_secs(3600)),
            Duration::ZERO
        );
    }
}
