# epics-tools-rs (procServ) — C-parity review, 2026-06-28

Port crate: `crates/epics-tools-rs` (`src/procserv/*`, `src/bin/procserv_rs.rs`)
Reference: `~/codes/epics-modules/procServ/` (`procServ.cc`, `acceptFactory.cc`,
`clientFactory.cc`, `processFactory.cc`, `connectionItem.cc`, headers).
Out of scope: `procServUtils/` (Python systemd/attach tooling).

Methodology: Codex-style C-parity audit — four read-only opus reviewers, one
per category (process/child lifecycle; connection/listener/accept; telnet +
control menu; config/CLI/daemon/logging). Each reviewer numbered in its own
range; this doc consolidates and **renumbers sequentially PS-1..PS-42**,
deduplicating the heavy cross-panel overlap. The four categories independently
surfaced the same control-key, `@@@`-banner, exit-code, and endpoint-syntax
divergences from different angles — those merges are noted per finding.

Steering: find divergences from C, but do **not** copy C's own bugs (record a
Rust-correctly-declines case as an intentional-divergence aside, not a defect).

---

## Verified-MATCHING (no divergence — do not re-flag)

- Holdoff measured from child start, `remaining = holdoff − uptime` clamped 0
  (`supervisor.rs:699` ↔ `processFactory.cc:188,51-54`); default holdoff 15s.
- RestartMode 3-state cycle `restart→norestart→oneshot` + `ON/OFF/ONESHOT`
  labels (`restart.rs` ↔ `clientFactory.cc:32-40,223-229`).
- Kill-key gating (signals live child, restarts dead child; restart/quit only
  while child dead) (`menu.rs:44-93` ↔ `clientFactory.cc:207-241`).
- Live-kill broadcast `"\r\n@@@ Got a kill command\r\n"` byte-exact
  (`supervisor.rs:371` ↔ `clientFactory.cc:237`).
- Toggle message + labels byte-exact (`supervisor.rs:388-391` ↔
  `clientFactory.cc:230-234`).
- `kill(-pid, sig)` to the process group via forkpty/setsid
  (`child.rs:184-192` ↔ `processFactory.cc:117,284`).
- Default control bytes killChar=^X(0x18), toggleChar=^T(0x14),
  restartChar=^R(0x12) (`procserv_rs.rs:135-140` ↔ `procServ.cc:66-68`).
- RFC1143 telnet Q-method WILL/WONT/DO/DONT for `{No,Yes,WantYes}`
  (`telnet.rs:262-311` ↔ `libtelnet.c:393-510`); option policy `WILL ECHO` +
  `DO LINEMODE`, all else refused, SGA not offered.
- IAC-IAC unescape + `iac_escape` doubling; `IAC SB…IAC SE` subneg skip.
- Party-line exclude-sender for child PTY output (`fanout_excluding(None)` ≡
  `SendToAll(buf,len,process)`); welcome-banner readonly-trim + peer count
  "(plus you)" (`supervisor.rs:552-605` ≡ `clientFactory.cc:143-163`).
- Bind defaults: control port localhost + `--allow`→ANY; log port ANY +
  `--restrict`→localhost (`procserv_rs.rs:164-184` ↔ `acceptFactory.cc:116`).
- Disconnect silently removes from registry, no goodbye broadcast
  (`supervisor.rs:354` ≡ `procServ.cc:815`); no enforced connection cap in
  either (C's `MAX_CONNECTIONS 64` is dead code).
- `--timefmt` default `"%c"`; default `stampFormat = "[" + timeFormat + "] "`;
  `stampLog` default off → verbatim log; per-line `_log_stamp_sent` partial
  tracking (`sidecar.rs:132-152` ↔ `procServ.cc:728-744`).
- `PROCSERV_INFO` env `PID=…;CTL=…;LOG=…` trailing-`;` stripped; info-file
  `pid:\n`+addr lines; pidfile content `"%d\n"`; SIGHUP→reopen-log; SIGPIPE
  ignored; log open create+append.
- `CTL=`/`LOG=` env + `tcp:`/`unix:` info-file address formatting
  (`sidecar.rs` ≡ `acceptFactory.cc:45-99`).

---

## Open Findings

### DEFECTS

#### PS-1 `--noautorestart` / OFF mode shuts the whole server down on child exit
- Severity: DEFECT
- Rust: `src/procserv/supervisor.rs:470-473` (`RestartMode::Disabled` → `ChildLoopOutcome::Shutdown`)
- C: `processFactory.cc:51-54` + `procServ.cc:599,654-669`
- Impact: In C, `norestart` makes `processFactoryNeedsRestart()` return false forever after the first launch, so on child exit the server **stays alive** with the child dead — operators reconnect, inspect, manually `^R` to relaunch; `shutdownServer` is never set. Only `oneshot` exits the server in C, never `norestart`. Rust tears the entire supervisor down on the first child exit in OFF mode (e.g. operator pressed `^T` once ON→OFF intending "stop auto-restarting", child later exits → Rust kills procserv-rs and drops every client). Headline lifecycle divergence.

#### PS-2 Child-lifecycle broadcast `@@@` messages diverge wholesale (exit / restart / shutdown)
- Severity: DEFECT — CLEARED (24202067)
- Resolution: four C-exact builders in `procserv::messages` (`child_reaped`, `child_shutting_down`, `restarting_child`, `new_child_pid`); `handle_child_event` emits reaper + shutdown blocks (single owner of the shutdown line), `respawn_child` is the single restart-announce site (`@@@ Restarting child` + `@@@ The PID of new child` + ruler). Non-C banners removed; `PendingRestart.banner`/`banner()` deleted. Byte-exact unit tests + 3 e2e assertions updated.
- Merged from: process PS-2/PS-8/PS-9, conn PS-37, telnet PS-62, config PS-93
- Rust: `src/procserv/supervisor.rs:431-439` (exit `"\r\n@@@ Child exited (status: {:?})\r\n"`), `:449/463/466/471` (restart banners), `:520` (`"@@@ Child started (pid N)"`)
- C: `procServ.cc:788-807` (exit: `"\r\n"`, `"@@@ @@@ @@@ @@@ @@@\r\n"`, `"@@@ Received a sigChild for process N. Normal exit status = K\r\n"` / `" killed by signal N"`); `processFactory.cc:66-72,82-114,191-196` (restart `"@@@ Restarting child \"name\"\r\n"` + optional `"@@@    (as argv0)\r\n"`; post-fork `"@@@ The PID of new child \"name\" is: N\r\n"` + `@@@ @@@…` ruler; shutdown `"@@@ Current time: …\r\n"`, `"@@@ Child process is shutting down, <reason>\r\n"`, infoMessage3 redisplay unless oneshot)
- Impact: Every byte differs — missing the `@@@ @@@ @@@ @@@ @@@` separator, missing PID, missing the mode-dependent shutdown-reason line and command-help refresh, wrong wording, and a Rust-`{:?}`-debug-formatted status (`@@@ Child exited (status: "exit status: 0")`) no C consumer/log scraper keyed on `Received a sigChild` / `The PID of new child` will recognize.

#### PS-3 Signaled death encoded as `128+sig`, losing "killed by signal N"
- Severity: DEFECT
- Rust: `src/procserv/child.rs:333-334` + `make_exit_status` `:343-349`
- C: `procServ.cc:801-805` (`WIFSIGNALED`→`WTERMSIG` → `" The process was killed by signal N"`)
- Impact: The reaper maps `WaitStatus::Signaled(_,sig,_)` to `make_exit_status(128+sig)`, collapsing a signal death into a fake normal exit. A child SIGKILLed (the default kill key) is reported by Rust as `exit status: 137` rather than "killed by signal 9"; the `WIFSIGNALED` branch never reaches any message. Data-loss root behind part of PS-2.

#### PS-4 Child exit code not propagated as procserv's own exit status (always exits 0)
- Severity: DEFECT
- Merged from: process PS-4, config PS-95
- Rust: `src/procserv/supervisor.rs:431-475` (status discarded) + `src/bin/procserv_rs.rs:309-319` (`Ok(())`→`ExitCode::SUCCESS`)
- C: `procServ.cc:76-77,701,794-798` (`childExitCode = WEXITSTATUS(...)`; `main` returns it)
- Impact: `procServ -o /bin/false` (oneshot) exits **1** in C (wrapper/systemd see the failure); procserv-rs exits **0** regardless of how the child exited, masking IOC failure for any script keyed on procServ's exit code.

#### PS-5 No SIGKILL of the child's process group on child exit → orphaned grandchildren
- Severity: DEFECT
- Rust: `src/procserv/supervisor.rs:431-432` (`ChildEvent::Exited` just `self.child.take()`)
- C: `processFactory.cc:117` (`~processClass`: `if (_pid>0) kill(-_pid, SIGKILL)`)
- Impact: C's destructor runs on every child death and SIGKILLs the whole process group to reap stragglers before the next launch. Rust signals the group only on supervisor `Drop` (shutdown) and explicit kill keystroke — a normal child exit leaves grandchildren running, accumulating across a crash-loop.

#### PS-6 Connect-time welcome banner + infoMessage1/2/3 diverge / missing
- Severity: DEFECT — CLEARED (87e96887)
- Resolution: new `procserv::messages` single-owner module ports greeting1/greeting2/infoMessage1/2/3 byte-for-byte; `welcome_banner` delegates the text and supplies live state (PID, dirs, child PID/start, peer counts). Greeting now `@@@ Welcome to procServ (<version>)`. Byte-exact unit tests + e2e assertion updated.
- Merged from: telnet PS-55/PS-56, config PS-92
- Rust: `src/procserv/supervisor.rs:552-608` (`welcome_banner`)
- C: `clientFactory.cc:100-163` (greeting1/greeting2) + `procServ.cc:442-450,572-595` (infoMessage1/2/3), `processFactory.cc:191` (infoMessage2)
- Impact: Not byte-parity. C greeting1 `@@@ Welcome to procServ (<VERSION>)`; Rust `@@@ Welcome to procserv-rs` (no version, different product name). C greeting2 packs kill/restart-mode/toggle hints on combined lines; Rust emits a non-C `@@@ Wrapping: <name> (mode: …)` shape. Rust omits infoMessage1 (`@@@ procServ server PID`, server/child startup dirs, `Child started as`, `Child log file`), infoMessage2 (`@@@ Child "<name>" PID: N` / `is SHUT DOWN`), and infoMessage3 (the command menu `@@@ ^R or ^X restarts the child, ^Q quits the server`). A connecting operator never sees the command summary or the child PID/state line.

#### PS-7 Quit keystroke `^Q` disabled by default and unreachable
- Severity: DEFECT — CLEARED (679d2b6b)
- Resolution: `build_config` now defaults `quit: Some(0x11)` (C `quitChar`, not CLI-settable); the two lying `config.rs` comments corrected; menu dispatch was already gated on child-dead. Bin unit test `quit_key_defaults_to_ctrl_q` pins it.
- Merged from: process PS-11, telnet PS-51, config PS-85
- Rust: `src/bin/procserv_rs.rs:230` (`quit: None`, no CLI flag) + wrong comment `config.rs:57-58` ("default disabled in C")
- C: `procServ.cc:69` (`char quitChar = 0x11;` — ^Q, not even CLI-settable), dispatch `clientFactory.cc:214-217`, advertised in infoMessage3 `procServ.cc:443-444`
- Impact: C enables `^Q` by default (fires only while the child is dead, gating verified) and advertises it. Rust hardwires `quit: None`, exposes no flag, so the in-band "shut the server down" keystroke does not exist; `menu.rs:70-74` quit logic is dead at runtime. The config comment is factually wrong.

#### PS-8 Client keystrokes broadcast to other clients (C sends them only to the child)
- Severity: DEFECT — CLEARED (see commit marking PS-8)
- Resolution: replaced the three ad-hoc fanout helpers (`fanout_to_all` / `fanout_excluding`) with a single `send_to_all(bytes, Origin)` that encodes C `SendToAll`'s recipient matrix by construction (`Origin::Client` → child stdin only, no client fan-out, no log; `Origin::Server`/`Origin::Child` → every client + log, never the child). A client's keystrokes can no longer reach another client directly — the rule now holds in one owner instead of per call site. Deterministic regression `client_keystrokes_are_not_forwarded_to_other_clients` (dead child ⟹ no PTY echo ⟹ second client sees nothing). Logger network-stream stamping (PS-9) is the remaining `Origin::Server`/`Child` follow-up.
- Merged from: conn PS-26, telnet PS-54
- Rust: `src/procserv/supervisor.rs:405-407` (`fanout_excluding(&bytes, Some(client_id))`) → loop `:634-643`
- C: `clientFactory.cc:243` (`SendToAll(buf,len,this)`) → `procServ.cc:753-767`
- Impact: In C `SendToAll(sender=client)`, the client branch guard `if (!sender || sender->isProcess())` is false, so typed bytes go **only to the child**; other clients see the keystroke once via PTY echo re-broadcast. Rust writes the raw bytes directly to every other client *then* the child echoes → with ≥2 clients each other client sees the input **twice** (direct + echo) and out of order. Worse, the loop has no readonly filter, so a logger/viewer client receives control clients' raw keystrokes, which C never shows loggers. The `supervisor.rs:406` "Matches C SendToAll(this)" comment misreads C's recipient rule.

#### PS-9 Log-port (logger) client streams not timestamped under `--logstamp`
- Severity: DEFECT — CLEARED (see commit marking PS-9)
- Resolution: `send_to_all` now stamps each logger (readonly) client's network stream at every newline under `--logstamp`, while control clients still get verbatim bytes — exactly C's `if isLogger()` gate (`clientFactory.cc:261-279`). Per-client mid-line state (`ClientEntry.stamp_in_line`, C's `_log_stamp_sent`) handles partial-line chunks. The stamp-at-newline loop is now one shared `sidecar::stamp_lines` helper reused by both the log file and the logger clients, removing the duplicated loop. Tests: e2e `logstamp_prefixes_logger_client_stream_not_control` (logger stamped, control raw) + unit `stamp_lines_prefixes_each_new_line_and_tracks_continuation`. RESIDUAL (documented, not fixed): the log file re-reads `Local::now()` inside `write_chunk`, so the file line and a logger socket line for the same fanout can show timestamps differing by the sub-microsecond gap between the two reads — observable only across a 1-second boundary. C shares one stamp per `SendToAll`; unifying would require gutting `LogFile`'s stamp ownership for a sub-second cosmetic skew, declined as over-engineering.
- Rust: `src/procserv/supervisor.rs:617-643` (`fanout_*` raw bytes to every client; only the log *file* is stamped, `sidecar.rs`)
- C: `procServ.cc:760-761` (`if stampLog → p->Send(stamp,…)`) → `clientFactory.cc:258-279` (stamps only `if isLogger()`, per-client `_log_stamp_sent`)
- Impact: With `--logstamp`, C prepends `stampFormat` (default `"[%c] "`) at every newline on the **logger client's network stream**; control clients get unstamped output. Rust stamps only the on-disk file; logger-port clients always receive raw bytes. A telnet log-viewer on the log port sees stamped lines under C, bare lines under Rust.

#### PS-10 Command chars not stripped from child stdin; `-i`/`--ignore` not wired
- Severity: DEFECT — CLEARED (see commit marking PS-10)
- Resolution: two halves. (1) The always-active command keys are auto-added to the child's stdin ignore set via `effective_ignore_chars(explicit, keys)`, computed at the single `ChildSpec` construction site in `respawn_child` — so a kill/toggle/logout keystroke triggers its action but the byte never reaches the child, and the rule holds for any config source (bin, library, tests), not just the CLI. Only kill/toggle-restart/logout are appended (exactly C's `procServ.cc:431-438`); restart/quit are excluded because they fire only when the child is already gone (`clientFactory.cc:207-217`) — no stdin to leak into — so excluding them is C-faithful, not a gap. (2) Added the `-i`/`--ignore` CLI flag with C's `^`-escaping (`parse_ignore_chars`: `^A`..`^Z`→control byte, `^^`→`^`, else verbatim; lowercase/`^]` are NOT escapes, matching C's `>= 'A' && <= 'Z'` gate). Tests: unit `ignore_set_*` (auto-append/dedup/disabled-key/restart-quit-excluded), unit `ignore_chars_apply_caret_escaping` + `ignore_flag_populates_explicit_child_ignore_chars`, e2e `ignored_chars_are_stripped_from_child_stdin` (supervisor→child filter composition). The existing `child.rs` filter already strips `ignore_chars`; this finding wired both producers into it.
- Merged from: telnet PS-53, config PS-80
- Rust: `src/bin/procserv_rs.rs:192` (`ignore_chars: Vec::new()`, always empty; no `-i`/`--ignore` flag) → forwarded verbatim `supervisor.rs:644-654`, filter `child.rs:140-148` is a no-op
- C: `procServ.cc:240,322-336` (`--ignore` with `^`-escaping) + `:431-438` (auto-append killChar/toggleRestartChar/logoutChar to `ignChars`), strip in `processClass::Send` `processFactory.cc:256-265`
- Impact: C strips the command bytes from PTY input — `^X`/`^T`/`^]` trigger the action but the byte never reaches the child. Rust leaves `ignore_chars` empty and exposes no flag, so `0x18`/`0x14`/`0x1d` fire the action **and** leak into the IOC shell's stdin.

#### PS-11 logoutChar `^]` default inverted — enabled in Rust, disabled in C
- Severity: DEFECT — CLEARED (842d2f39)
- Resolution: `logout_char` default flipped `29 → 0` (C `logoutChar = 0x00`, opt-in via `--logoutcmd`); `config.rs` comment corrected. Bin unit test `logout_key_disabled_by_default`. The `--logout-char` flag rename + caret parsing is the separate PS-27.
- Merged from: telnet PS-52, config PS-84
- Rust: `src/bin/procserv_rs.rs:143` (`logout_char` `default_value_t = 29`) + wrong comment `config.rs:59`
- C: `procServ.cc:70` (`char logoutChar = 0x00;`), set only by `--logoutcmd` `:403`
- Impact: C disables logout by default; `^]` does nothing unless `--logoutcmd` is given. Rust defaults `logout_char=0x1d` **enabled**, so a stock Rust server disconnects a client on `0x1d` (a byte many telnet clients send as their own escape) that C passes through to the child. The config comment falsely attributes Rust's behavior to C.

#### PS-12 Kill signal hardcoded SIGKILL; `--killsig` / `-K` absent
- Severity: DEFECT — CLEARED (this round)
- Resolution: added `--killsig <n>` (long-only in C; `-K` convenience short like `-F`/`-S`, as 'K' is absent from C's optstring). `build_config` applies C's exact rule (`procServ.cc:346-355`): `i = abs(n)`, accept only `i < 32`, else `tracing::warn!` and keep the SIGKILL default. The vetted value feeds the already-live `ChildConfig.kill_signal` (used at the kill keystroke `supervisor.rs:398` and teardown `:866`). Tests: `killsig_defaults_to_sigkill`, `killsig_sets_signal_number`, `killsig_short_flag_is_accepted`, `killsig_above_31_falls_back_to_default`, `killsig_negative_is_abs`. (Teardown's final hardcoded SIGKILL is the separate PS-22.)
- Merged from: telnet PS-60, config PS-79
- Rust: `src/bin/procserv_rs.rs:191` (`kill_signal: 9`, hardcoded; no flag) — `ChildConfig.kill_signal` exists but is wired to a constant
- C: `procServ.cc:71,243,346-355` (`--killsig <n>` / `-K`, default SIGKILL, validated `<32`)
- Impact: C lets the `^X` kill signal be set (`--killsig 2` for graceful SIGINT IOC shutdown is a common deployment). Rust always sends SIGKILL with no way to change it; `config.rs:74-77` even documents the SIGINT use case the code can't honor.

#### PS-13 Control-endpoint surface collapsed: single port, no `<iface>:<port>`, no repeated `-P`, no `user:grp:perm:` UNIX spec, no abstract socket
- Severity: DEFECT
- Merged from: conn PS-32/PS-33/PS-39, config PS-89
- Rust: `src/procserv/config.rs:19-25` (`tcp_port: Option<u16>` + single `tcp_bind`); `procserv_rs.rs:39-40,164-184`; `listener.rs:63-71`; `sidecar.rs:244-251` (no abstract form)
- C: `procServ.cc:211,386-388,515-518` (`ctlSpecs` vector — repeatable `-P`); `acceptFactory.cc:125-137` (`A.B.C.D:port` interface bind), `:241-279` (`user:grp:perm:` prefix → `getpwnam`/`getgrnam`/`strtoul(…,8)`), `:293,316-325` (leading `@` → abstract socket)
- Impact: C accepts multiple `-P` endpoints + richer syntax: bind a specific NIC (`192.168.1.5:4051`), several ports at once, a group-restricted UNIX socket (`unix:ioc:operators:0660:/run/ioc.sock`), or an abstract socket (`unix:@name`). Rust takes one numeric `--port` (localhost or 0.0.0.0 only) + a bare-path `--unixpath`. Interface-specific binds, multi-endpoint configs, UNIX access-control, and abstract sockets are all impossible (and a `user:grp:perm:` path either fails to bind or silently loses access-control intent).

#### PS-14 `-p` / `-P` short-option letters swapped (pidfile vs port)
- Severity: DEFECT
- Rust: `src/bin/procserv_rs.rs:39` (`-p`/`--port`), `:96-97` (`--pidfile`, long-only)
- C: `procServ.cc:250-251,264,381-388` (`-p`/`--pidfile`, `-P`/`--port`)
- Impact: C binds `-p <file>` to the PID file and `-P <endpoint>` to the control port. Rust reassigns `-p` to the TCP port and provides no `-P`. A wrapper `procServ -p /run/x.pid -P 4051 …` treats `/run/x.pid` as a (rejected) port and fails on `-P`. Silent semantic inversion of a core flag.

#### PS-15 Daemon `chdir("/")` leaks into the child's working directory
- Severity: DEFECT — CLEARED (fecfd0f9)
- Resolution: dropped the `chdir("/")` from `fork_and_go` so the daemon stays in the launch dir (C `forkAndGo` never chdir's). Child `cwd=None` inheritance + the supervisor's startup-dir capture now both land on the launch directory. Re-found as PS-R2-2 by the round-2 caucus review.
- Rust: `src/procserv/daemon.rs:63` (`chdir("/")`) + `child.rs:217-229` (child chdir only when `cwd.is_some()`) + `procserv_rs.rs:190` (`cwd: args.chdir`, default `None`)
- C: `procServ.cc:220-221` (`chDir = myDir` = startup cwd), `processFactory.cc:211` (child always `chdir(chDir)`); `forkAndGo` never chdir's
- Impact: In daemon mode C's child chdir's to the procServ startup directory by default. Rust's `fork_and_go` chdir's the supervisor to `/`, and a child started without `--chdir` inherits `/` instead of the launch directory — relative paths in `st.cmd`/`dbLoadRecords` break. Foreground mode unaffected.

#### PS-16 `--coresize` / `-C` option and RLIMIT_CORE absent
- Severity: DEFECT — CLEARED (this round)
- Resolution: added `ChildConfig.core_size: Option<u64>` → `ChildSpec.core_size`, applied in `in_child_setup_and_exec` before chdir/exec via `getrlimit`/`setrlimit(RLIMIT_CORE)` (nix `resource` feature), keeping the hard limit and setting only the soft (`rlim_cur`) — exact mirror of `processFactory.cc:206-210`. Added the `--coresize <n>` flag (long-only in C; `-C` convenience short like `-F`/`-S`); `build_config` applies C's `l >= 0` gate (`procServ.cc:279-285`), mapping negative/absent → `None` (inherit). Tests: bin `coresize_absent_leaves_core_limit_untouched`/`coresize_nonnegative_sets_limit`/`coresize_negative_is_inert`; child integration `core_size_applies_rlimit_core_to_child` (1 MiB cap read back via `ulimit -c`).
- Rust: `src/procserv/config.rs` (no coresize field); `procserv_rs.rs` (no flag)
- C: `procServ.cc:233,279-285` (`--coresize <n>`), `processFactory.cc:206-210` (`setrlimit(RLIMIT_CORE)` in child)
- Impact: C applies `--coresize` to the child's core-dump rlimit. Rust has no option or field, so operators relying on `--coresize` get no effect (and clap rejects the unknown flag).

#### PS-17 `-N`/`--noautorestart` and `-o`/`--oneshot` startup modes absent
- Severity: DEFECT
- Rust: `src/bin/procserv_rs.rs:244` (`restart_mode: RestartMode::OnExit`, hardcoded; no flags)
- C: `procServ.cc:248-249,369-375` (`-N`→norestart, `-o`→oneshot)
- Impact: C can start directly in no-restart or one-shot mode. Rust hardcodes `OnExit`; these modes are reachable only via the runtime toggle key. `procServ --oneshot …` / `procServ -N …` wrappers lose their startup mode.

#### PS-18 `-e` / `--exec` (separate child executable) absent
- Severity: DEFECT
- Rust: `src/bin/procserv_rs.rs:156` (`program = cmd[0]` always)
- C: `procServ.cc:236,295-297,457-462` (`-e <str>` sets `childExec` distinct from `command`/`childName`)
- Impact: C execs `childExec` while passing the original command as argv. Rust always execs `cmd[0]`, so `-e` deployments (launch a wrapper while presenting a different argv[0]) are unsupported.

#### PS-19 `--logfile -` (log to stdout) creates a file literally named "-"
- Severity: DEFECT — CLEARED (this round)
- Resolution: `LogFile::open` now special-cases `path == "-"` to a new `LogSink::Stdout(tokio::io::stdout())` arm (fd 1), mirroring C `openLogFile` (`procServ.cc:920-922`). The sink is an enum (`File { handle, path }` | `Stdout`) so the write/flush path is identical once chosen; `reopen` (logrotate) is a no-op for stdout, matching C's `1 != logFileFD` guard before `close()`. fd 1 in daemon mode is `/dev/null` (`daemon::fork_and_go`), in foreground the terminal — same as C writing to whatever fd 1 points at. Bin `--logfile` help documents the `-` form. Test: `logfile_dash_logs_to_stdout_not_a_file_named_dash` (sink is Stdout, write succeeds, no file `-` created, reopen is a no-op).
- Rust: `src/procserv/sidecar.rs:85-92` (opens the path verbatim, no `"-"` special-case); `supervisor.rs:162-176`
- C: `procServ.cc:920-922` (`if logFile=="-" → logFileFD = 1`), help `:184` ("'-' logs to stdout")
- Impact: C documents+implements `-L -` as "log to stdout". Rust passes `-` to `OpenOptions::open`, creating/appending a regular file `-` in the cwd (a stray `-` file in daemon mode). The documented stdout-logging form is broken.

### CONCERNS

#### PS-20 OneShot toggle mid-run drops C's "one more run"; relaunch branch is dead code
- Severity: CONCERN
- Merged from: process PS-6, telnet PS-59, config PS-96
- Rust: `src/procserv/supervisor.rs:458-468` (`OneShot` exit: relaunch only if `!has_run_once`) + `respawn_child` unconditional `has_run_once = true` `:514`; toggle `:386-393` never resets it
- C: `clientFactory.cc:223-229` (`firstRun = true` on toggle into oneshot) + gate `procServ.cc:656-667` (`if (restartMode==oneshot && !firstRun) shutdown`)
- Impact: C's toggle into oneshot grants exactly one more launch after the current child exits; only the *next* exit shuts the server. Rust never resets `has_run_once` (already true from the initial spawn), so the `if !has_run_once` relaunch arm and its `"@@@ One-shot relaunch"` banner are unreachable — `^T`-into-oneshot shuts the supervisor down on the first exit, one run short of C.

#### PS-21 Auto / initial spawn failure aborts the supervisor; C retries after holdoff
- Severity: CONCERN
- Rust: `src/procserv/supervisor.rs:234` (bootstrap `respawn_child().await?`), `:315-319` (restart arm `respawn_child().await?`) — the *manual*-restart path `:381` correctly logs-and-continues
- C: `processFactory.cc:158,188` + `processFactoryNeedsRestart` retry loop (on forkpty failure: build a `markedForDeletion` processClass, still set `_restartTime = holdoff+now`, retry on next poll — never give up)
- Impact: A transient `forkpty` failure (pty exhaustion, brief ENOENT) at startup or on an auto-restart propagates via `?` out of the event loop and terminates procserv-rs, where C retries after holdoff. Inconsistency is auto/initial-only.

#### PS-22 Shutdown teardown uses the configurable kill signal, not C's final hardcoded SIGKILL
- Severity: CONCERN — CLEARED (this round)
- Resolution: `SupervisorState::Drop` now sends the configurable `kill_signal` *then* an unconditional `libc::SIGKILL` to the group, mirroring C's two-step teardown (`processFactorySendSignal(killSig)` procServ.cc:637 + the destructor's unconditional `kill(-_pid, SIGKILL)` processFactory.cc:117). Without the follow-up, a child that traps a catchable `--killsig` (e.g. SIGINT) would survive supervisor shutdown. When `kill_signal` is already SIGKILL the second send is a harmless ESRCH no-op, exactly as in C. Drop is the single teardown finalizer for the child resource, so the guarantee holds on every exit path. Test: e2e `teardown_sigkills_a_child_that_traps_the_configurable_kill_signal` (child traps SIGTERM, teardown's SIGKILL still reaps it). Enabled by PS-12 making `kill_signal` configurable.
- Rust: `src/procserv/supervisor.rs:720-724` (`Drop` → `signal(self.config.child.kill_signal)`)
- C: `procServ.cc:637` (`processFactorySendSignal(killSig)`) **then** `processFactory.cc:117` (unconditional `kill(-_pid, SIGKILL)` in the destructor during teardown)
- Impact: On SIGTERM/quit, C sends `killSig` then an unconditional hardcoded SIGKILL during teardown — a guaranteed kill even if `killSig` is catchable. Rust signals the group once with the configurable `kill_signal`. With default SIGKILL this is fine, but `--killsig SIGINT` (if implemented per PS-12) that the child ignores would survive procserv-rs shutdown in Rust.

#### PS-23 TCP listen socket omits SO_REUSEADDR
- Severity: CONCERN
- Rust: `src/procserv/listener.rs:30-32` (`TcpListener::bind` — std/tokio do not set SO_REUSEADDR)
- C: `acceptFactory.cc:187-191` (`setsockopt(SO_REUSEADDR)` before `bind`)
- Impact: procServ is itself frequently restarted (systemd). After a restart while prior client connections linger in TIME_WAIT, C rebinds immediately; the Rust port can fail `bind` with `EADDRINUSE` and exit at startup (`ProcServError::ListenerBind`) until TIME_WAIT drains.

#### PS-24 UNIX socket permissions/ownership not applied (C forces 0666 + chown)
- Severity: CONCERN
- Rust: `src/procserv/listener.rs:69-71` (`UnixListener::bind` — mode is umask-dependent, no chmod/chown)
- C: `acceptFactory.cc:368-377` (`chmod(path,0)`, `chown(path,uid,gid)`, `chmod(path,perms)` with `perms=0666` default)
- Impact: C sets the socket 0666 ("equivalent to tcp bind to localhost") so any local user can connect, and can chown to a configured user/group. The Rust socket inherits `0777 & ~umask`, so a peer in a different group may be unable to connect where C permitted it, with no way to set owner/group.

#### PS-25 Client sockets omit SO_KEEPALIVE / SO_SNDTIMEO → unbounded head-of-line block on the party-line
- Severity: CONCERN
- Rust: `src/procserv/client.rs:120-152` (no socket options) — fanout awaits `out_tx.send(...).await` over a bounded(64) channel (`supervisor.rs:619/641`)
- C: `clientFactory.cc:146-147` (`SO_KEEPALIVE`; `SO_SNDTIMEO` 10s)
- Impact: C bounds a stuck/dead client's blocking write at 10s, then marks it `_markedForDeletion` and continues; KEEPALIVE surfaces silently-dropped peers. Rust has neither: a silently dead client (cable pull) is never detected, its 64-deep channel fills, and the supervisor's `fanout_*().await` blocks the **entire party-line** (no other client gets output, child PTY processing stalls, new connections can't be accepted) for as long as the OS keeps the TCP write pending — potentially minutes.

#### PS-26 Telnet IAC negotiation written before the banner (order reversed vs C)
- Severity: CONCERN
- Merged from: conn PS-36, telnet PS-57
- Rust: `src/procserv/client.rs:198-210` (write task sends `initial_negotiation()` first, then drains the banner frame)
- C: `clientFactory.cc:153-174` (constructor `write()`s greeting/info first, then `telnet_init`+`telnet_negotiate`)
- Impact: First bytes differ. C: `[banner text][IAC WILL ECHO][IAC DO LINEMODE]`. Rust: `FF FB 01 FF FD 22` then banner. Functionally tolerated by most telnet clients, but a byte-exact connect-sequence capture sees the IAC bytes interleaved before the greeting.

#### PS-27 Control-char CLI uses decimal `u8` + renamed flags (no `^X` caret notation)
- Severity: CONCERN
- Merged from: telnet PS-61, config PS-87
- Rust: `src/bin/procserv_rs.rs:134-144` (`--kill-char`/`--toggle-restart-char`/`--logout-char` as decimal `u8`)
- C: `procServ.cc:142-152` (`getOptionChar`: `^X`→0x18, `^^`→`^`), options `--killcmd`/`-k`, `--autorestartcmd`/`-T`, `--logoutcmd`/`-x`
- Impact: C accepts caret notation (`--killcmd '^X'`). Rust requires a decimal byte (`--kill-char 24`) under different flag names. A wrapper passing `--killcmd ^X` both fails to find the option and can't use caret notation; the control-char *values* match but the *configuration surface* does not.

#### PS-28 `-d`/`--debug`, `-q`/`--quiet`, and `PROCSERV_DEBUG` env unsupported
- Severity: CONCERN
- Rust: `src/bin/procserv_rs.rs` (no `-d`/`-q`; no `PROCSERV_DEBUG` read)
- C: `procServ.cc:225` (`getenv("PROCSERV_DEBUG")`→inDebugMode), `:291-293` (`-d`), `:390-392,889-895` (`-q` suppresses the spawn banner & log warnings)
- Impact: `-d` (debug/foreground+printf), `-q` (suppress the `spawning daemon process: <pid>` / no-logfile warnings), and `PROCSERV_DEBUG` are accepted by C and silently unavailable in Rust (clap rejects `-d`/`-q`).

#### PS-29 `--allow` not compile-gated; Rust always binds all interfaces
- Severity: CONCERN
- Rust: `src/bin/procserv_rs.rs:166-172` (`--allow` → bind `0.0.0.0` unconditionally)
- C: `procServ.cc:43-47` (`enableAllow=false` unless `ALLOW_FROM_ANYWHERE` build), `:272-277` (`--allow` prints "not supported" / no-op in the default build)
- Impact: In the default C build `--allow` is refused and the control port stays localhost-only (a deliberate security default). Rust honors `--allow` always. A wrapper carrying `--allow` that was inert on stock procServ now exposes the control console to the network.

#### PS-30 `PROCSERV_PID` env not consulted for the pidfile default
- Severity: CONCERN
- Rust: `src/procserv/*` (no `PROCSERV_PID` read; pidfile only from `--pidfile`)
- C: `procServ.cc:224` (`pidFile = getenv("PROCSERV_PID")` when `-p`/`--pidfile` absent)
- Impact: C lets the PID-file path come from `PROCSERV_PID` (used by some service wrappers). Rust ignores the env entirely, so an environment-driven pidfile is never written.

#### PS-31 Log file requested mode 0666 (vs C 0644) and no `fsync`
- Severity: CONCERN
- Rust: `src/procserv/sidecar.rs:86-89` (`create(true).append(true)` → mode 0666 & ~umask), `:121-122/:157-158` (`flush()` only)
- C: `procServ.cc:924` (`open(..., S_IRUSR|S_IWUSR|S_IRGRP|S_IROTH)` = 0644), `:748` (`fsync` after every write)
- Impact: (1) C requests 0644 (no group/other write); Rust 0666, so under a permissive umask the log is group/other-writable where C's is not. (2) C `fsync`s after each write; Rust only `flush`es (tokio File flush is not an fsync), so on power loss buffered lines C would have synced can be lost.

#### PS-32 PID file written by the grandchild after both parents exit (type=forking race)
- Severity: CONCERN
- Rust: `src/procserv/daemon.rs:41-78` (`fork_and_go` writes no pidfile) + `supervisor.rs:159-161` (grandchild writes it during bootstrap)
- C: `procServ.cc:896-898` (parent writes the pidfile *before* `exit(0)`, with the explicit comment "removes a race condition using a type=forking systemd service")
- Impact: C writes the pidfile from the foreground parent before it exits. Rust's foreground process exits in `fork_and_go` and the grandchild writes the pidfile later; a `Type=forking` systemd unit can observe the original process gone before the pidfile appears — the exact race C's code comments out.

### NOTES

#### PS-33 PTY EOF on a still-running child doesn't terminate it
- Severity: NOTE
- Rust: `src/procserv/child.rs:281-323` (reader breaks on EOF/EIO, no death signal) + `supervisor.rs:251-256`
- C: `processFactory.cc:227-242` (`readFromFd` sets `_markedForDeletion` on EOF/err) → `~processClass` SIGKILLs the group
- Impact: When the child closes its controlling tty but keeps running, C marks it dead → destructor SIGKILLs the group, ending the child. Rust's reader task just stops while the reaper blocks on `waitpid`; the child is neither killed nor reported and output forwarding silently stops. Edge case (child detaching its tty while alive).

#### PS-34 Listen backlog differs (C=5, Rust=tokio default ~1024)
- Severity: NOTE
- Rust: `src/procserv/listener.rs:30/:70` (mio default backlog 1024)
- C: `acceptFactory.cc:216,363` (`listen(_fd, 5)`)
- Impact: Under a connection burst, C refuses past the 6th queued connection; Rust queues ~1024. Observable only as different refusal thresholds; no data divergence.

#### PS-35 Accept-error handling: C rebuilds the listen socket, Rust retries with no backoff
- Severity: NOTE (Rust correctly declines a C misbehavior, but adds a busy-loop risk)
- Rust: `src/procserv/listener.rs:48-53/:86-88` (log `warn!` and continue on the same listener)
- C: `acceptFactory.cc:396-399` (`remakeConnection()` — close + re-socket/bind/listen)
- Impact: On a transient accept failure (EMFILE) C tears down and re-creates the whole listen socket (briefly unbinding, dropping queued connections — questionable); Rust correctly keeps the bound listener. Aside: Rust's retry has no backoff, so a *persistent* error tight-loops `accept→warn→accept` burning CPU until an fd frees; C re-enters `pselect` (0.5s) between attempts. A small backoff would close the busy-loop without copying C's unbind.

#### PS-36 UNIX socket file not unlinked on shutdown
- Severity: NOTE
- Rust: `src/procserv/listener.rs:63-91` (removes a stale file only at startup `:69`; no unlink on drop, despite the `:58-62` comment claiming "best-effort unlink here too")
- C: `acceptFactory.cc:331-335` (`~acceptItemUNIX` → `unlink(addr.sun_path)` for non-abstract)
- Impact: After a clean Rust shutdown the socket file is left on disk; C removes it. Self-corrects on next start, but between runs a liveness check testing the socket file's existence sees a stale node. (Stale comment to fix or implement.)

#### PS-37 SIGPIPE notice is never broadcast (C announces it to all clients)
- Severity: NOTE
- Rust: `src/procserv/daemon.rs:96-99` (SIGPIPE → `SIG_IGN`, no broadcast); supervisor has no SIGPIPE arm
- C: `procServ.cc:628-631` (`SendToAll("@@@ Got a sigPipe signal: Did the child close its tty?\r\n", NULL)`)
- Impact: When the child closes its tty and a write raises SIGPIPE, C emits a diagnostic line to every console; Rust silently ignores it. Loss of an operator-visible diagnostic, no functional impact.

#### PS-38 Read-only client telnet *replies* are still processed (C discards all logger input)
- Severity: NOTE
- Rust: `src/procserv/client.rs:163-185` — `TelnetEvent::Data` is gated on `!readonly` `:166` but `TelnetEvent::Reply` is **not** `:176-184`
- C: `clientFactory.cc:192` (`else if (!_readonly) telnet_recv(...)` — a logger's input never reaches the telnet state machine)
- Impact: A logger that sends IAC negotiation gets telnet responses from Rust but silence from C. A log-capture tool on the readonly port could receive unexpected IAC sequences mid-stream under Rust.

#### PS-39 Dead-child killChar drops C's "@@@ Got a kill command" broadcast (intentional-divergence candidate)
- Severity: NOTE
- Rust: `src/procserv/menu.rs:52-75` (dead-child `^X` returns only `RestartChild`) → `supervisor.rs:377-385` emits `"@@@ Manual restart"`
- C: `clientFactory.cc:236-241` (the killChar block is **ungated** by `processClass::exists()`, so dead-child `^X` runs `restartOnce()` *and* broadcasts `"\r\n@@@ Got a kill command\r\n"` even though the signal is a no-op `processFactory.cc:281`)
- Impact: Rust collapses dead-child kill into a pure restart and emits `"@@@ Manual restart"` instead — the misleading "kill" notice C sends is absent. Arguably Rust correctly declines C's misleading message; flag as an intentional-divergence candidate, confirm before "fixing".

#### PS-40 `-c`/`-I`/`-n` short aliases dropped; `-F` added as a non-C short
- Severity: NOTE
- Rust: `src/bin/procserv_rs.rs:92` (`-F` for `--timefmt`), `:99-101` (`--info-file` long-only), `:113-118` (`--chdir`, `--name` long-only)
- C: `procServ.cc:234,241,247` (`-c`/`--chdir`, `-I`/`--info-file`, `-n`/`--name`); `--timefmt` is long-only, `F` is not in the C optstring
- Impact: Minor surface drift. Wrapper scripts using `-c <dir>`/`-I <file>`/`-n <name>` are rejected by clap; `-F` is a Rust-only addition.

#### PS-41 Rust-only restart-count knobs (`--max-restarts`/`--restart-window`/1s delay) (intentional)
- Severity: NOTE (intentional divergence — Rust opt-in cap over C's never-give-up)
- Rust: `src/bin/procserv_rs.rs:120-132,235-243` (`RestartPolicy{ max_restarts, window, delay: 1s }`)
- C: `procServ.cc:599` (`while(!shutdownServer)` never caps), `processFactory.cc:48-56` (gate only on mode + holdoff)
- Impact: No C analog. Defaults inert (`max_restarts = u32::MAX`), and procserv times relaunches off `holdoff` (`supervisor.rs:669-678`) not `RestartPolicy.delay`, so `delay: 1s` appears unused on the procserv path. Correctly declines C's never-give-up as an opt-in cap; noted so the extra surface isn't mistaken for parity (and so the unused `delay` isn't read as a behavior).

#### PS-42 Intentional-divergence asides — redundant `setsid` skipped; `RestartTracker` opt-in (intentional)
- Severity: NOTE (not Rust defects)
- Rust: `src/procserv/child.rs:213-216`; `src/bin/procserv_rs.rs:235-243`
- C: `processFactory.cc:204`; `processFactory.cc:48-56`
- Impact: (a) C redundantly calls `setsid()` in the child after `forkpty` (returns EPERM, harmless); Rust correctly skips it since `forkpty` already creates the session — same resulting pgid. (b) `RestartTracker` has no C analog; default `max_restarts = u32::MAX` keeps parity with C's never-give-up. Neither is a parity defect; flagged so they aren't re-reported. (`sidecar.rs` is pure pid/info/log/env file management — C `writePidFile`/`writeInfoFile`/`setEnvVar`/`openLogFile`, not a child-lifecycle counterpart.)

#### PS-43 Default child name was the program basename; C uses the whole command string
- Severity: DEFECT — CLEARED (1a0a9587)
- Found: round-2 caucus review (PS-R2-1) of the PS-6 banner landing
- Rust: `src/bin/procserv_rs.rs` `build_config` (`display_name = program.file_name()…`)
- C: `procServ.cc:455-457` (`childName = command` = `argv[optind]`) → `:579-586`, `clientFactory.cc:138`, `processFactory.cc:191`
- Resolution: default the display name to the whole program string so `name == command` and the banner shows C's bare `@@@ Child started as: <cmd>` / `@@@ Child "<cmd>" PID: N` form; `-n` still overrides. The `messages` builders were already correct — only the wiring fed a basename. Bin unit tests pin the default + override.

#### PS-44 Dead OneShot relaunch arm / `has_run_once` (no-op cleanup)
- Severity: NOTE — CLEARED (d0b3b6cc)
- Found: round-2 caucus review carryover (rev-ps-process)
- Rust: `src/procserv/supervisor.rs` OneShot arm + `has_run_once`
- C: `procServ.cc:656-658`, `processFactory.cc:51` (`oneshot && !firstRun` → shutdown on first exit)
- Resolution: `has_run_once` was set on every spawn, so the `!has_run_once` "relaunch once" branch was unreachable and its intent did not match C (which shuts down on the first oneshot exit). Behaviour already matched C; collapsed the arm to unconditional shutdown and removed the field. RESIDUAL (documented, not fixed): C gates the oneshot shutdown behind the holdoff, so a sub-holdoff crash makes C wait `holdoff − uptime`; the port exits immediately. Reachable only with oneshot + a crash inside the holdoff.

#### PS-45 Welcome version token is the crate semver, not C's PACKAGE_STRING (intentional)
- Severity: CONCERN (intentional divergence — keep the Rust version)
- Found: round-2 caucus review (PS-R2-3)
- Rust: `src/procserv/supervisor.rs` (`version: env!("CARGO_PKG_VERSION")`) → `messages::welcome`
- C: `clientFactory.cc:100` (`PROCSERV_VERSION_STRING` = `PACKAGE_STRING` = `"procServ Process Server 2.9.0-dev"`)
- Disposition: the greeting keeps the C-exact shape `@@@ Welcome to procServ (<version>)` but reports the Rust crate version, so operators see the actual running build. The `procServ (` prefix still matches liveness scrapers; only a front-end pinned to C's exact version string would differ. Kept deliberately — do not hardcode a C-compat version.

#### PS-46 `ctl_sc` emits 2-byte UTF-8 for key bytes ≥ 128 (residual)
- Severity: NOTE (unreachable with default keys — all command keys are control chars < 32)
- Found: round-2 caucus review (PS-R2-5)
- Rust: `src/procserv/messages.rs` `ctl_sc` (`(c as char).to_string()` UTF-8-encodes `c ≥ 0x80` as two bytes)
- C: `procServ.h:67` (`CTL_SC` `%c` writes the raw byte)
- Disposition: a key bound to a byte ≥ 0x80 renders as a 2-byte UTF-8 sequence vs C's one raw byte. Not reachable with default keys; left as a documented residual rather than reworking the `String`-based builders for an exotic binding.

#### PS-47 C strips a raw `NUL` from child stdin via the `index()` terminator quirk; Rust does not
- Severity: NOTE (telnet-layer concern, distinct from PS-10; do NOT copy the C quirk)
- Found: round-3 caucus review (rev-ps-process) of the PS-10 landing
- Rust: `src/procserv/child.rs` `write_stdin` filter (`!ignore_chars.contains(b)`); `src/procserv/telnet.rs` (IAC-only strip, no CR-NUL translation)
- C: `processFactory.cc:256-261` (`index(ignChars, buf[i]) == NULL` copy gate — `index`/`strchr` also matches the string's `'\0'` terminator)
- Impact: C's filter uses `index(ignChars, byte)`, which returns non-NULL for `byte == 0x00` because it matches `ignChars`'s terminating NUL — so whenever `ignChars` is non-empty (the default, since kill/toggle are set) C silently drops every raw `NUL` byte from child stdin. Rust's `contains` only drops bytes explicitly in the set, so a bare `NUL` reaches the child. Reachable: a telnet client sending a bare CR encodes it as `CR NUL` (RFC 854); libtelnet/`telnet.rs` both deliver `\r\0` as data (neither translates CR-NUL→CR), so the child sees `\r\0` in Rust vs `\r` in C-default.
- Disposition: C's NUL-stripping is an UNINTENTIONAL artifact of `index()` matching the terminator — it is conditional on `ignChars` being non-empty (disable all command keys and C stops stripping NUL too), so it is plainly not a deliberate telnet feature. Copying it would be bug-copying (steering: find divergences, don't copy C's bugs). The legitimate underlying question — should procServ translate telnet `CR NUL`→`CR` before feeding the child — belongs to the telnet input layer (`client.rs`/`telnet.rs`), not the ignore filter, and is its own future finding if pursued. Impact is negligible (shells ignore NUL; most clients send CR-LF). Recorded so it is not mistaken for a strip-path gap in PS-10.

---

## Review Log

### Round 1 — 2026-06-28 — initial 4-category audit (caucus round 01KW73Y4NE77NNESZ2YY29KBA8)

Four read-only opus reviewers, one per category, against procServ upstream at
`~/codes/epics-modules/procServ`:

- **Process / child lifecycle** (`supervisor.rs`/`child.rs`/`restart.rs`/`menu.rs`
  ↔ `processFactory.cc`/`procServ.cc` reap path) — raw PS-1..PS-13.
- **Connection / listener / accept** (`listener.rs`/`client.rs`/`supervisor.rs`
  fanout ↔ `acceptFactory.cc`/`clientFactory.cc`/`procServ.cc` SendToAll) — raw
  PS-26..PS-40.
- **Telnet protocol + control menu** (`telnet.rs`/`menu.rs`/`client.rs` ↔
  `clientFactory.cc`/libtelnet) — raw PS-51..PS-62.
- **Config / CLI / daemon / logging** (`procserv_rs.rs`/`config.rs`/`daemon.rs`/
  `sidecar.rs` ↔ `procServ.cc` main/getopt/forkAndGo/log) — raw PS-76..PS-98.

Consolidated to **42 findings (PS-1..PS-42)** after deduplicating heavy
cross-panel overlap (the four categories independently surfaced the same
control-key, `@@@`-banner, exit-code, and endpoint-syntax divergences). Tally:
**19 DEFECT, 13 CONCERN, 10 NOTE** (2 NOTEs — PS-41/PS-42 — are intentional
Rust-declines-C, and PS-39 is an intentional-divergence candidate to confirm).

Thematic clusters (each points at a structural gap, not isolated bugs):

1. **CLI surface is a partial reimplementation, not a port** — the largest
   cluster (PS-7, PS-11..PS-18, PS-27..PS-30, PS-40). Short-option letters are
   reassigned (`-p`/`-P` swapped, PS-14), whole flags are missing
   (`--coresize`/`-C`, `--killsig`/`-K`, `-e`/`--exec`, `-N`/`-o`, `-d`/`-q`,
   `-i`/`--ignore`), key-command flags are renamed and re-typed (caret→decimal,
   PS-27), and two defaults are silently inverted (`^]` logout enabled, `^Q`
   quit disabled) with **doc comments that misattribute Rust's choice to C**
   (PS-7/PS-11). Structural cause: the arg parser was written to a new clap
   spec rather than mirroring C's `long_options`/`getOptionChar` table. Fix
   direction: drive the CLI from C's option table as the single source of truth.

2. **Console `@@@` protocol is not byte-faithful** — every server-originated
   line a client/operator/log-scraper sees on connect, child exit, restart, and
   shutdown diverges (PS-2, PS-6, PS-9, plus the `^Q`/menu gap PS-7). C emits a
   specific multi-line vocabulary (`@@@ @@@ @@@ @@@ @@@` ruler, `Received a
   sigChild … exit status =` / `killed by signal N`, `Restarting child`, `The
   PID of new child`, infoMessage1/2/3); Rust emits ad-hoc single lines, one
   with a `{:?}`-debug status. Structural cause: no shared "C console message"
   module — banners are inlined ad-hoc. Fix direction: one message-builder
   module owning the exact C wire text.

3. **Party-line routing semantics differ** — PS-8 (client input mis-fanned to
   other clients/loggers instead of only the child), PS-9 (logger stream not
   stamped), PS-38/PS-40 (logger input handling). Structural cause: the fanout
   helper doesn't encode C's `SendToAll(sender)` recipient rule (process-only
   for a client sender; isLogger stamping). Fix direction: make
   `fanout_excluding` enforce the C recipient/stamp matrix by construction.

4. **Lifecycle / signal correctness** — PS-1 (norestart kills the server),
   PS-3 (signal death mis-encoded), PS-4 (exit code dropped), PS-5 (no
   process-group reap on exit → orphans), PS-20 (oneshot toggle off-by-one +
   dead code), PS-21 (spawn-failure aborts), PS-22 (final SIGKILL missing).
   These are the headline behavioral defects independent of wire text.

5. **Socket hygiene** — PS-23 (no SO_REUSEADDR), PS-24/PS-13 (UNIX perms +
   endpoint syntax), PS-25 (no KEEPALIVE/SNDTIMEO → party-line stall), PS-31
   (log mode/fsync). A dead client can wedge the whole party-line (PS-25) — the
   highest-impact CONCERN.

Doc committed doc-only before any fix work. Fix phase next: per-finding
commits, severity order (DEFECT → CONCERN → NOTE), marking each `cleared` here
as it lands, driving caucus opus convergence rounds.

### Round 2 — 2026-06-28 — console + control-key cluster review (caucus round 01KW775WKGGMZEE8F1Z42E07GP)

Two read-only opus reviewers re-checked the landed console/control-key cluster
(PS-2/PS-6/PS-7/PS-11/PS-15 plus the new `messages.rs` module) against
`procServ.cc`/`clientFactory.cc`/`processFactory.cc`. Both confirmed the `@@@`
console vocabulary and the `^Q`/logout-default fixes land byte-faithfully; all
structural checks PASS.

Two new DEFECTs found and fixed in this round:

- **PS-43** (conn panel, PS-R2-1) — the default child name was the program
  *basename*; C names the child after the whole command string. Fixed in
  `1a0a9587` (wiring only; the `messages` builders were already correct).
- **PS-15** (conn panel, PS-R2-2) — re-found the daemon `chdir("/")` leak.
  Fixed in `fecfd0f9` by dropping the `chdir` so the daemon stays in the
  launch dir (C `forkAndGo` never chdir's).

One dead-code cleanup:

- **PS-44** (process panel) — the `has_run_once`/OneShot relaunch arm was
  unreachable; collapsed to unconditional shutdown matching C. `d0b3b6cc`.

Intentional / residual, no code change:

- **PS-45** (PS-R2-3) — the welcome version token is the Rust crate semver, not
  C's `PACKAGE_STRING`; kept deliberately so operators see the running build.
- **PS-46** (PS-R2-5) — `ctl_sc` renders a key byte ≥ 0x80 as 2-byte UTF-8 vs
  C's one raw byte; unreachable with default control-char keys.
- **PS-39 reconfirmed** (PS-R2-4) — the dead-child kill broadcast is the same
  intentional divergence already recorded as PS-39; no new action.

`epics-tools-rs` after this round: 74 nextest green, clippy `-D warnings`
clean. Nothing pushed.

### Round 3 — 2026-06-28 — party-line cluster review (caucus round 01KW79D3F7NPK9XR7QGX7V8S1W)

Two read-only opus reviewers byte-traced the party-line cluster (PS-8/PS-9/PS-10)
against `procServ.cc`/`clientFactory.cc`/`processFactory.cc`. **Verdict: all three
fixes byte-faithful to upstream C — no DEFECT, no CONCERN.**

- **rev-ps-conn** (PS-8 routing + PS-9 stamping): confirmed `send_to_all(Origin)`
  reproduces C `SendToAll`'s recipient matrix exactly (client→child-only with no
  log write; server/child→clients+log, never the child; sender NOT excluded from
  the PTY-echo broadcast). Audited every `out_tx.send` site — no client→client
  path remains. `stamp_lines` is a byte-exact port of `clientItem::Send`; only
  loggers stamped, per-client mid-line state correct across partial chunks,
  one stamp shared per call. PS-9 sub-second file-vs-socket stamp residual
  re-confirmed acceptable.
- **rev-ps-process** (PS-10 stripping): confirmed the auto-append set is exactly
  `{kill, toggle, logout}` (restart/quit correctly excluded — they fire only when
  the child is dead); the `^`-escaping matches C including `^]`/lowercase as
  literals; and `write_stdin` is the single chokepoint with no bypass now that
  `send_to_all(Origin::Client)` is the only child writer.

One new finding surfaced (out of cluster scope, recorded not fixed):

- **PS-47** (rev-ps-process NOTE) — C's `index()`/`strchr` ignore filter also
  matches the string's NUL terminator, so C silently strips a raw `NUL` from
  child stdin whenever `ignChars` is non-empty (the default); Rust's `contains`
  does not, so a telnet `CR NUL` reaches the child as `\r\0` vs C's `\r`. C's
  behaviour is an accidental `index()` artifact — copying it would be
  bug-copying; the legitimate concern (telnet `CR NUL`→`CR` translation) is a
  telnet-layer matter for a future round. Negligible impact (shells ignore NUL).

rev-ps-conn also re-flagged the command-keys-not-in-`ignChars` gap (it had only
PS-8/PS-9 in its brief) — that IS PS-10, fixed concurrently in this same batch.

`epics-tools-rs` after this round: 83 nextest green, clippy `-D warnings` clean.
Party-line cluster converged. Nothing pushed.
