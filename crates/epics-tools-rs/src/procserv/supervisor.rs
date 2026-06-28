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
//! receives, scans for menu keys, then forwards the bytes to the PTY
//! child ONLY — never to the other clients. Every client (A included)
//! then sees the keystroke once when the PTY echoes it back. This
//! matches C `SendToAll(buf, len, this)`, where a client sender's bytes
//! reach only the process recipient (`procServ.cc:754-756`).
//!
//! When the PTY emits output: child task → `child_rx` → supervisor
//! → all clients' `outbound_tx`s + log file (never back to the child).

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Local};
use tokio::sync::mpsc;

use crate::procserv::child::{ChildEvent, ChildExit, ChildHandle, ChildSpec};
use crate::procserv::client::{
    ClientId, ClientMeta, InboundEvent, IncomingClient, OutboundFrame, spawn_client,
};
use crate::procserv::config::{KeyBindings, ProcServConfig};
use crate::procserv::error::{ProcServError, ProcServResult};
use crate::procserv::menu::{Action, scan as menu_scan};
use crate::procserv::messages;
use crate::procserv::restart::{RestartMode, RestartTracker};
use crate::procserv::sidecar::{
    InfoSnapshot, LogFile, remove_pid_file, render_procserv_info_env, stamp_lines, write_info_file,
    write_pid_file,
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

    /// Run until shutdown. Returns the last child exit code (procserv's
    /// own process exit status, C `childExitCode` — `procServ.cc:701`).
    /// Returns when:
    /// - the configured restart policy refuses a respawn (limit hit)
    /// - the user issues the `quit` keystroke
    /// - one-shot / no-restart mode ends the supervisor
    /// - SIGTERM/SIGINT arrives (only when running with the daemon
    ///   wrapper that wires those into a shutdown signal)
    pub async fn run(self) -> ProcServResult<i32> {
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
    /// SIGHUP stream. A hangup means "reopen the log file" (logrotate),
    /// NOT shutdown — C `OnSigHup` → `openLogFile()`
    /// (`procServ.cc:641-645`). The supervisor owns this rather than the
    /// daemon's shutdown-signal layer so a `kill -HUP` rotates the log
    /// instead of killing the IOC.
    sighup: tokio::signal::unix::Signal,
    /// Last child exit code, returned as procserv's own process exit
    /// status on shutdown. Mirrors C `childExitCode`: updated only on a
    /// normal exit (`WIFEXITED`), so a signal death leaves the prior
    /// value, and `main` returns it (`procServ.cc:798,701`). Defaults 0.
    child_exit_code: i32,
    /// A respawn that is waiting out the crash-loop holdoff. Mirrors C
    /// procServ's `_restartTime` deadline (`processFactory.cc:188`): the
    /// child has exited and an auto/one-shot relaunch is due once
    /// `Instant::now() >= at`. Kept as state (not an inline `sleep`) so
    /// the event loop keeps polling keystrokes during the wait — C's
    /// main loop `while(!shutdownServer)` re-checks
    /// `processFactoryNeedsRestart()` every poll, so a manual restart
    /// (`restartOnce()` zeros `_restartTime`, `processFactory.cc:289-291`)
    /// or a kill is honored live rather than queued behind the sleep.
    /// Invariant: `pending_restart.is_some()` ⟹ `child.is_none()` — set
    /// only on child exit, cleared by [`Self::respawn_child`] (the single
    /// owner of the "child now running" transition).
    pending_restart: Option<PendingRestart>,
    /// Wall-clock time the supervisor started, for the welcome banner's
    /// "@@@ procServ server started at:" line (C `procServStart`,
    /// `clientFactory.cc:131-132`). Distinct from any monotonic
    /// `Instant`: this is for human display, not elapsed-time math.
    proc_started: DateTime<Local>,
    /// Directory the server was launched from, for infoMessage1's
    /// "@@@ Server startup directory:" line (C `myDir = getcwd(...)`,
    /// `procServ.cc:220`). Captured once at bootstrap.
    startup_dir: String,
}

/// A scheduled-but-not-yet-fired child relaunch: just the holdoff
/// deadline. The relaunch announcement is C's `@@@ Restarting child`,
/// emitted by [`SupervisorState::respawn_child`] when it fires (C
/// processFactory), so nothing else needs carrying here.
struct PendingRestart {
    at: tokio::time::Instant,
}

struct ClientEntry {
    out_tx: mpsc::Sender<OutboundFrame>,
    /// Per-client metadata. `meta.readonly` is read when building the
    /// welcome banner's connected-peer counts (user vs logger), so this
    /// field is live rather than purely future-facing.
    meta: ClientMeta,
    /// Mid-line state for `--logstamp` on this client's network stream:
    /// `true` ⟹ the last byte sent was not a newline, so the next chunk
    /// continues the line without a fresh timestamp. Per-client, mirroring
    /// C's `_log_stamp_sent` member (`clientFactory.cc:138`). Only logger
    /// (readonly) clients are stamped, so it stays `false` for control
    /// clients.
    stamp_in_line: bool,
}

struct ChildSlot {
    handle: ChildHandle,
    rx: mpsc::Receiver<ChildEvent>,
    /// When this child was spawned. The restart holdoff is measured
    /// from here, mirroring C procServ's `_restartTime = holdoffTime +
    /// time(0)` set at fork (`processFactory.cc:188`).
    started_at: tokio::time::Instant,
    /// Wall-clock spawn time for the welcome banner's "@@@ Child started
    /// at:" line (C `IOCStart`, `clientFactory.cc:135-136`). Separate from
    /// the monotonic `started_at`, which can't render as a calendar time.
    started_wall: DateTime<Local>,
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
            // The LOG uses `stamp_log` + `stamp_format` (raw line prefix),
            // not the banner-facing `time_format`. With `stamp_log` off
            // (C default) the log is written verbatim.
            Some(
                LogFile::open(
                    p,
                    config.logging.stamp_log,
                    config.logging.stamp_format.clone(),
                )
                .await?,
            )
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
        // Read-only viewer/log port: a second TCP listener whose clients
        // receive output but whose input is discarded. C creates this as
        // `acceptFactory(logPort, logPortLocal, /*readonly=*/true)`
        // (procServ.cc:533); the `readonly` flag flows to each accepted
        // client and gates its input (client.rs read task).
        if let Some(addr) = config.listen.log_bind {
            let tx = incoming_tx.clone();
            tokio::spawn(async move {
                if let Err(e) = super::listener::run_tcp(addr, true, tx).await {
                    tracing::error!(error = %e, "procserv-rs: log listener exited");
                }
            });
        }
        // Drop our copy so listeners' txs are the only owners.
        drop(incoming_tx);

        // SIGHUP → reopen the log (logrotate). Registered here, not in
        // the daemon shutdown layer, so a hangup never tears down the IOC.
        let sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            .map_err(ProcServError::Io)?;

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
            sighup,
            child_exit_code: 0,
            pending_restart: None,
            proc_started: Local::now(),
            // C `myDir = getcwd(NULL, 512)` (procServ.cc:220). A failure
            // to read the cwd is non-fatal display data, so fall back to
            // "." rather than aborting startup.
            startup_dir: std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| ".".to_string()),
        };

        // Initial child spawn unless `--wait` (manual start).
        if !state.config.wait_for_manual_start {
            state.respawn_child().await?;
        }

        Ok(state)
    }

    async fn event_loop(&mut self) -> ProcServResult<i32> {
        loop {
            // Snapshot the pending-restart deadline (Copy) before
            // borrowing `self.child` mutably below, so the timer arm
            // borrows only this local — not `self`.
            let restart_at = self.pending_restart.as_ref().map(|p| p.at);

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

            // Resolves when the crash-loop holdoff elapses; `pending`
            // (never resolves) when no restart is scheduled. This is the
            // non-blocking equivalent of C's poll-loop re-checking
            // `now >= _restartTime` every iteration — input arms above
            // are still serviced while we wait.
            let restart_due = async {
                match restart_at {
                    Some(at) => tokio::time::sleep_until(at).await,
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
                        return Ok(self.child_exit_code); // quit key
                    }
                }

                // 2. PTY child output.
                ev = child_event => {
                    if let Some(ev) = ev {
                        match self.handle_child_event(ev).await? {
                            ChildLoopOutcome::Continue => {}
                            ChildLoopOutcome::Shutdown => return Ok(self.child_exit_code),
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

                // 4. SIGHUP → reopen the log file (logrotate). Never a
                //    shutdown — the loop continues. C OnSigHup →
                //    openLogFile() (procServ.cc:641-645).
                _ = self.sighup.recv() => {
                    self.reopen_log().await;
                }

                // 5. Crash-loop holdoff elapsed → fire the scheduled
                //    relaunch. Lowest priority so a manual restart/kill
                //    keystroke (arm 1) that arrives during the wait
                //    preempts it: that path respawns and clears
                //    `pending_restart`, after which `restart_at` is
                //    `None` and this arm parks again. C: the poll loop
                //    restarts once `now >= _restartTime`, unless
                //    `restartOnce()` already zeroed it.
                _ = restart_due => {
                    if self.pending_restart.take().is_some() {
                        // `respawn_child` emits C's `@@@ Restarting child`
                        // announcement itself — no separate banner here.
                        self.respawn_child().await?;
                    }
                }
            }
        }
    }

    /// Reopen the log file in response to SIGHUP. No-op when no log is
    /// configured (matches C `openLogFile()` short-circuiting on a NULL
    /// `logFile`). A reopen failure is logged but never shuts the
    /// supervisor down.
    async fn reopen_log(&self) {
        if let Some(log) = &self.log {
            match log.reopen().await {
                Ok(()) => tracing::info!("procserv-rs: reopened log file on SIGHUP"),
                Err(e) => {
                    tracing::warn!(error = %e, "procserv-rs: log reopen on SIGHUP failed")
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
                            // C broadcasts the kill notice to all clients
                            // (and the log) before signalling — SendToAll
                            // with a NULL sender (clientFactory.cc:236-239).
                            // Only the live-kill path reaches here; a kill
                            // key on a dead child is a RestartChild action.
                            if self.child.is_some() {
                                self.send_to_all(b"\r\n@@@ Got a kill command\r\n", Origin::Server)
                                    .await;
                                if let Some(slot) = self.child.as_ref() {
                                    let _ = slot.handle.signal(self.config.child.kill_signal);
                                }
                            }
                        }
                        Action::RestartChild => {
                            // Force a respawn (clears any holdoff).
                            // `respawn_child` emits C's `@@@ Restarting
                            // child` announcement itself — matching C, where
                            // a manual `restartOnce()` just zeros
                            // `_restartTime` and the next poll routes through
                            // processFactory, which prints it.
                            if self.child.is_none()
                                && let Err(e) = self.respawn_child().await
                            {
                                tracing::error!(error = %e, "procserv-rs: manual respawn failed");
                            }
                        }
                        Action::ToggleRestartMode => {
                            self.restart_mode = self.restart_mode.next();
                            let msg = format!(
                                "\r\n@@@ Toggled auto restart mode to {}\r\n",
                                self.restart_mode.label()
                            );
                            self.send_to_all(msg.as_bytes(), Origin::Server).await;
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

                // C `SendToAll(buf, len, this)` (clientFactory.cc:243):
                // a client sender's bytes reach ONLY the child, not the
                // other clients — every client (including this one) sees
                // the keystroke once when the PTY echoes it back. C keys
                // recipients on sender class, not identity, so there is
                // no per-client exclusion.
                self.send_to_all(&bytes, Origin::Client).await;
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
                // PTY output is a process sender: it reaches every
                // client and the log, never the child itself (C SendToAll
                // with the process as sender, procServ.cc:730 → :759-763).
                // The log write is folded into `send_to_all`, the single
                // owner of the process/server fan-out.
                self.send_to_all(&bytes, Origin::Child).await;
                Ok(ChildLoopOutcome::Continue)
            }
            ChildEvent::Exited { exit } => {
                let (started_at, pid) = match self.child.take() {
                    Some(slot) => {
                        let pid = slot.handle.pid();
                        // C ~processClass SIGKILLs the child's whole process
                        // group on every death to reap grandchildren the
                        // child spawned, before the next launch
                        // (processFactory.cc:117). Hardcoded SIGKILL,
                        // independent of killSig. The pgid equals the
                        // (now-reaped) leader pid and stays valid while any
                        // group member survives.
                        let _ = slot.handle.signal(libc::SIGKILL);
                        (Some(slot.started_at), pid)
                    }
                    // An exit event implies a child existed; default the pid
                    // only to keep the arm total.
                    None => (None, 0),
                };
                // C updates childExitCode only under WIFEXITED
                // (procServ.cc:798); a signal death leaves the prior value.
                if let ChildExit::Exited(code) = exit {
                    self.child_exit_code = code;
                }

                // C SIGCHLD reaper block (procServ.cc:788-807): blank line +
                // ruler + the `Received a sigChild` line. Distinguishes a
                // normal exit from a signal death; never 128+sig.
                self.send_to_all(messages::child_reaped(pid, exit).as_bytes(), Origin::Server)
                    .await;
                // C ~processClass shutdown block (processFactory.cc:111-114):
                // current time + mode-dependent reason + the command menu
                // redisplayed unless oneshot. This is the single owner of the
                // per-mode shutdown line — the restart_mode arms below no
                // longer emit their own ad-hoc shutdown banners.
                let info3 = messages::info_message3(&self.config.keys);
                let now = Local::now()
                    .format(&self.config.logging.time_format)
                    .to_string();
                self.send_to_all(
                    messages::child_shutting_down(&now, self.restart_mode, &info3).as_bytes(),
                    Origin::Server,
                )
                .await;

                match self.restart_mode {
                    RestartMode::OnExit => {
                        match self.restart_tracker.try_record(&self.config.restart) {
                            Ok(()) => {
                                // Schedule the relaunch behind the holdoff
                                // deadline rather than sleeping inline, so
                                // the event loop keeps servicing keystrokes
                                // during the wait (C polls continuously).
                                self.schedule_restart(started_at);
                                Ok(ChildLoopOutcome::Continue)
                            }
                            Err((max, win)) => Err(ProcServError::RestartLimitExceeded {
                                attempts: max,
                                window_secs: win,
                            }),
                        }
                    }
                    RestartMode::OneShot => {
                        // C `oneshot`: the child launches once, and the
                        // first exit shuts the server down — `needsRestart`
                        // sees `oneshot && !firstRun` (firstRun is cleared
                        // right after the first launch) and sets
                        // `shutdownServer` (procServ.cc:656-658,
                        // processFactory.cc:51). The supervisor only reaches
                        // this arm after a real spawn, so the child has
                        // already run once; exit ⟹ shut down.
                        Ok(ChildLoopOutcome::Shutdown)
                    }
                    RestartMode::Disabled => {
                        // C `norestart`: the child stays dead but the
                        // SERVER stays up. processFactoryNeedsRestart()
                        // returns false forever after the first launch
                        // (`norestart && _restartTime`,
                        // processFactory.cc:51), so shutdownServer is
                        // never set (procServ.cc:654-669) — operators
                        // reconnect and ^R to relaunch. Only `oneshot`
                        // exits the server, never `norestart`.
                        Ok(ChildLoopOutcome::Continue)
                    }
                }
            }
        }
    }

    /// Spawn the configured child and store the handle. Updates
    /// info-file + `PROCSERV_INFO` env var.
    ///
    /// Both carry supervisor identity (pid) + listening addresses, which are
    /// fixed for the supervisor's lifetime — C writes them once at startup
    /// (`procServ.cc:560-563`) and neither holds the child pid. We set the env
    /// BEFORE `ChildHandle::spawn` so the child inherits it via `execvp`; the
    /// info-file rewrite per respawn is idempotent (same bytes).
    async fn respawn_child(&mut self) -> ProcServResult<()> {
        let info = InfoSnapshot {
            // C writeInfoFile/setEnvVar emit getpid() — the supervisor pid,
            // which manage-procs probes for liveness (procServ.cc:938,946).
            procserv_pid: std::process::id() as i32,
            addresses: super::sidecar::listen_addresses(&self.config.listen),
        };
        // SAFETY: PROCSERV_INFO is process-wide. Setting env in a
        // running multi-threaded program is racy on POSIX; we accept
        // that risk because (a) only this supervisor task touches it,
        // (b) the child gets a fresh copy via execvp at fork time, so
        // a torn read in another supervisor thread is harmless.
        unsafe { std::env::set_var("PROCSERV_INFO", render_procserv_info_env(&info)) };

        // C `processFactory` announces the (re)launch BEFORE forking
        // (processFactory.cc:66-72), naming the executable when it differs
        // from the display name. C prints this on the first launch too,
        // where it reaches only the log (no clients yet) — a server-origin
        // `send_to_all` does the same here. This is the single
        // restart-announce site, matching C where every relaunch routes
        // through processFactory.
        let command = self.config.child.program.display().to_string();
        self.send_to_all(
            messages::restarting_child(&self.config.child.name, &command).as_bytes(),
            Origin::Server,
        )
        .await;

        // Spawn — child inherits the env var. The child's stdin ignore set
        // is the operator's explicit `--ignore` list PLUS the always-active
        // command keys, so a kill/toggle/logout keystroke triggers its
        // action but never reaches the child (C auto-appends these to
        // `ignChars`, procServ.cc:431-438). Computed here — the single
        // ChildSpec construction site — so it holds for every config source.
        let spec = ChildSpec {
            program: self.config.child.program.clone(),
            args: self.config.child.args.clone(),
            cwd: self.config.child.cwd.clone(),
            ignore_chars: effective_ignore_chars(
                &self.config.child.ignore_chars,
                &self.config.keys,
            ),
            core_size: self.config.child.core_size,
        };
        let (handle, rx) = ChildHandle::spawn(&spec)?;

        if let Some(p) = &self.config.logging.info_path {
            let _ = write_info_file(p, &info);
        }

        // A child is now running, so any holdoff that was waiting to
        // relaunch one is satisfied — clear it here (the single owner of
        // this transition) so a manual restart that fires mid-holdoff
        // cancels the pending auto relaunch instead of double-spawning.
        self.pending_restart = None;
        // C `processClass` ctor, parent branch (processFactory.cc:193-196):
        // the new child PID line followed by the ruler.
        self.send_to_all(
            messages::new_child_pid(&self.config.child.name, handle.pid()).as_bytes(),
            Origin::Server,
        )
        .await;
        self.child = Some(ChildSlot {
            handle,
            rx,
            started_at: tokio::time::Instant::now(),
            started_wall: Local::now(),
        });
        Ok(())
    }

    /// Roster: register a freshly-accepted client + send the welcome
    /// banner.
    async fn handle_new_client(&mut self, incoming: IncomingClient) {
        let (meta, out_tx) = spawn_client(incoming, self.inbound_tx.clone());
        let banner = self.welcome_banner(meta.readonly);
        let _ = out_tx.send(OutboundFrame::Bytes(banner.into_bytes())).await;
        self.clients.insert(
            meta.id,
            ClientEntry {
                out_tx,
                meta: meta.clone(),
                stamp_in_line: false,
            },
        );
        tracing::debug!(client = meta.id.raw(), peer = ?meta.peer, readonly = meta.readonly, "procserv-rs: client connected");
    }

    /// Build the welcome banner per C `clientItem::clientItem`
    /// (`clientFactory.cc:95-165`). The C-exact text lives in
    /// [`messages::welcome`]; this method only assembles the live state
    /// (key bindings, child PID/start time, peer counts) the banner
    /// reports. A read-only (log/viewer) client gets a trimmed banner — C
    /// gates the greeting + key hints and the connected-peer count on
    /// `!_readonly` (`clientFactory.cc:151-163`), handled inside
    /// `messages::welcome`.
    fn welcome_banner(&self, readonly: bool) -> String {
        let tf = &self.config.logging.time_format;
        // C `command` is argv[optind] (procServ.cc:455) — the program path.
        let command = self.config.child.program.display().to_string();
        // C `chDir` defaults to `myDir` when `--chdir` is absent
        // (procServ.cc:221).
        let child_dir = self
            .config
            .child
            .cwd
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| self.startup_dir.clone());
        let log_path = self
            .config
            .logging
            .log_path
            .as_ref()
            .map(|p| p.display().to_string());
        let proc_started = self.proc_started.format(tf).to_string();
        // `Some` ⟹ child alive (its PID + start time); `None` ⟹ shut down.
        let child_started = self
            .child
            .as_ref()
            .map(|slot| slot.started_wall.format(tf).to_string());
        let child = self.child.as_ref().and_then(|slot| {
            child_started.as_deref().map(|started| messages::ChildLine {
                pid: slot.handle.pid(),
                started,
            })
        });

        let ctx = messages::WelcomeCtx {
            readonly,
            version: env!("CARGO_PKG_VERSION"),
            keys: &self.config.keys,
            restart_mode: self.restart_mode,
            pid: std::process::id() as i32,
            startup_dir: &self.startup_dir,
            child_dir: &child_dir,
            name: &self.config.child.name,
            command: &command,
            log_path: log_path.as_deref(),
            proc_started: &proc_started,
            child,
            // Counted before the new client is registered, so they exclude
            // it → C's "(plus you)" (clientFactory.cc:143-144).
            users: self.clients.values().filter(|e| !e.meta.readonly).count(),
            loggers: self.clients.values().filter(|e| e.meta.readonly).count(),
        };
        messages::welcome(&ctx)
    }

    /// Fan `bytes` out across the party-line per C `SendToAll`
    /// (`procServ.cc:707-768`). The recipient set is a function of the
    /// message [`Origin`] alone — C keys it on `sender==NULL` /
    /// `sender->isProcess()`, never the sender's identity:
    ///
    /// - [`Origin::Client`] — a network client's keystrokes reach ONLY
    ///   the PTY child (`procServ.cc:754-756`); the other clients (and
    ///   the sender) see the input once when the PTY echoes it back.
    ///   Nothing is written to the log/debug stream — C's log guard
    ///   `sender==NULL || isProcess()` is false for a client sender
    ///   (`procServ.cc:725`).
    /// - [`Origin::Server`] / [`Origin::Child`] — a NULL (server `@@@`
    ///   annotation) or process (child output) sender reaches every
    ///   connected client (`procServ.cc:759-763`) and the log/debug
    ///   stream (`procServ.cc:725-749`), never the child itself. Under
    ///   `--logstamp` each logger (readonly) client's stream is prefixed
    ///   with the timestamp at every newline, exactly as C stamps only
    ///   `isLogger()` connections (`procServ.cc:760-761` →
    ///   `clientItem::Send`, `clientFactory.cc:261-279`); control clients
    ///   get the bytes verbatim.
    ///
    /// This method is the single owner of the recipient matrix, so the
    /// rule that a client's keystrokes never reach other clients holds
    /// by construction rather than per call site. Takes `&mut self`
    /// because logger stamping mutates each client's mid-line state.
    async fn send_to_all(&mut self, bytes: &[u8], origin: Origin) {
        if let Origin::Client = origin {
            // Client → child only (C's loop forwards a non-process
            // sender's bytes solely to the process recipient,
            // procServ.cc:754-756). No fan-out to clients, no log write.
            if let Some(slot) = self.child.as_ref()
                && let Err(e) = slot.handle.write_stdin(bytes).await
            {
                tracing::debug!(error = %e, "procserv-rs: child stdin write failed");
            }
            return;
        }

        // Server or child sender → every network client + the log,
        // never the child (C procServ.cc:759-763 for clients, :725 for
        // the log; the child branch excludes a process/NULL sender).
        //
        // C computes one timestamp per `SendToAll` and shares it between
        // the log file and the logger clients (procServ.cc:715-722). We
        // compute it once here too; the log file re-stamps with its own
        // `Local::now()` read inside `write_chunk`, so the file and a
        // logger socket can differ by the sub-microsecond gap between the
        // two reads (only observable across a 1-second boundary — a
        // documented residual, PS-9).
        let stamp = self.config.logging.stamp_log.then(|| {
            Local::now()
                .format(&self.config.logging.stamp_format)
                .to_string()
        });
        for entry in self.clients.values_mut() {
            let frame = match &stamp {
                // Logger client under --logstamp: prefix each new line.
                Some(s) if entry.meta.readonly => {
                    OutboundFrame::Bytes(stamp_lines(bytes, s.as_bytes(), &mut entry.stamp_in_line))
                }
                // Control client, or stamping off: verbatim bytes.
                _ => OutboundFrame::Bytes(bytes.to_vec()),
            };
            let _ = entry.out_tx.send(frame).await;
        }
        if let Some(log) = &self.log
            && let Err(e) = log.write_chunk(bytes).await
        {
            tracing::warn!(error = %e, "procserv-rs: log write failed");
        }
    }

    /// Schedule the child relaunch behind the restart holdoff deadline,
    /// measured from the child's start instant. C procServ sets
    /// `_restartTime = holdoffTime + time(0)` when the child is forked
    /// (`processFactory.cc:188`) and `processFactoryNeedsRestart`
    /// relaunches as soon as `now >= _restartTime` — so a child that
    /// ran longer than the holdoff restarts immediately, and only a
    /// fast crash-loop waits out `holdoff - uptime`.
    ///
    /// Unlike an inline `sleep`, recording the deadline as state lets the
    /// event loop keep polling input during the wait (C's poll loop
    /// re-checks the deadline every iteration), so a manual restart or
    /// kill keystroke is honored live instead of queued behind the sleep.
    fn schedule_restart(&mut self, started_at: Option<tokio::time::Instant>) {
        let remaining = match started_at {
            Some(t) => remaining_holdoff(self.config.holdoff, t.elapsed()),
            None => self.config.holdoff,
        };
        self.pending_restart = Some(PendingRestart {
            at: tokio::time::Instant::now() + remaining,
        });
    }
}

#[derive(Debug)]
enum ChildLoopOutcome {
    Continue,
    Shutdown,
}

/// Originator of a party-line message — the Rust model of C
/// `SendToAll`'s `sender` argument (`procServ.cc:707`). C picks the
/// recipient set purely from the sender's class (`sender==NULL`,
/// `sender->isProcess()`) and never compares identities, so this
/// carries no `ClientId`.
#[derive(Clone, Copy)]
enum Origin {
    /// `sender == NULL`: a server-originated `@@@` annotation.
    Server,
    /// `sender->isProcess()`: the PTY child's output.
    Child,
    /// A network client's keystrokes.
    Client,
}

/// The child's effective stdin ignore set: the operator's explicit
/// `--ignore` bytes plus the always-active command keys C auto-appends
/// to `ignChars` (`procServ.cc:431-438`) — kill, toggle-restart, and
/// logout. Those three are scanned and consumed on every keystroke, so
/// they must be stripped before the byte reaches the child. The
/// child-dead-only keys (restart, quit) are deliberately excluded:
/// C omits them (`procServ.cc:433-438`), and correctly so, since they
/// fire only while `processClass::exists()` is false
/// (`clientFactory.cc:207-217`) — when there is no child stdin to leak
/// into. Disabled keys (`None`) contribute nothing; duplicates are
/// folded (the child-side filter is a membership test, so order and
/// repetition are irrelevant).
fn effective_ignore_chars(explicit: &[u8], keys: &KeyBindings) -> Vec<u8> {
    let mut set = explicit.to_vec();
    for key in [keys.kill, keys.toggle_restart, keys.logout]
        .into_iter()
        .flatten()
    {
        if !set.contains(&key) {
            set.push(key);
        }
    }
    set
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

impl Drop for SupervisorState {
    fn drop(&mut self) {
        if let Some(p) = &self.config.logging.pid_path {
            remove_pid_file(p);
        }
        if let Some(slot) = self.child.as_ref() {
            // C teardown is two-step: send the configurable `killSig`
            // (procServ.cc:637), then an unconditional `SIGKILL` to the
            // group in the processClass destructor (processFactory.cc:117).
            // The follow-up SIGKILL guarantees the group dies even when
            // `killSig` is catchable/ignorable (e.g. `--killsig 2` SIGINT
            // a child traps) — without it such a child would survive
            // supervisor shutdown. When `kill_signal` is already SIGKILL
            // the second send is a harmless ESRCH no-op (group already
            // gone), exactly as in C.
            let _ = slot.handle.signal(self.config.child.kill_signal);
            let _ = slot.handle.signal(libc::SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{effective_ignore_chars, remaining_holdoff};
    use crate::procserv::config::KeyBindings;
    use std::time::Duration;

    fn full_keys() -> KeyBindings {
        KeyBindings {
            kill: Some(0x18),           // ^X
            toggle_restart: Some(0x14), // ^T
            restart: Some(0x12),        // ^R
            quit: Some(0x11),           // ^Q
            logout: Some(0x1d),         // ^]
        }
    }

    #[test]
    fn ignore_set_auto_appends_kill_toggle_logout_only() {
        // C appends killChar/toggleRestartChar/logoutChar — the keys
        // scanned on every keystroke — but NOT restartChar/quitChar, which
        // only fire when the child is already gone (procServ.cc:431-438).
        let set = effective_ignore_chars(&[], &full_keys());
        assert!(set.contains(&0x18), "kill key must be stripped");
        assert!(set.contains(&0x14), "toggle key must be stripped");
        assert!(set.contains(&0x1d), "logout key must be stripped");
        assert!(!set.contains(&0x12), "restart key must NOT be stripped");
        assert!(!set.contains(&0x11), "quit key must NOT be stripped");
    }

    #[test]
    fn ignore_set_unions_explicit_then_command_keys_without_dups() {
        // Explicit `--ignore` bytes come first; a command key already in
        // the explicit list is not duplicated (the child filter is a
        // membership test, but we keep the set minimal).
        let set = effective_ignore_chars(&[b'Z', 0x18], &full_keys());
        assert_eq!(set.iter().filter(|&&b| b == 0x18).count(), 1);
        assert!(set.contains(&b'Z'));
        assert!(set.contains(&0x14) && set.contains(&0x1d));
    }

    #[test]
    fn ignore_set_skips_disabled_keys() {
        // A disabled (None) command key contributes nothing.
        let keys = KeyBindings {
            kill: Some(0x18),
            toggle_restart: None,
            restart: None,
            quit: None,
            logout: None,
        };
        assert_eq!(effective_ignore_chars(&[], &keys), vec![0x18]);
    }

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
