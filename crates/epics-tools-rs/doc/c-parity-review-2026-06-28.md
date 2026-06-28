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
- Severity: DEFECT — CLEARED (629c57c9)
- Resolution: `handle_child_event`'s `RestartMode::Disabled` arm now returns `ChildLoopOutcome::Continue` (`supervisor.rs:533-543`) — the child stays dead but the server stays up, mirroring C `processFactoryNeedsRestart()` returning false forever after the first launch (`processFactory.cc:51`) so `shutdownServer` is never set; only `oneshot` exits the server. e2e `norestart_keeps_server_alive_after_child_exit` pins it. (Doc CLEARED-marker added retroactively; fix landed in an earlier round.)
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
- Severity: DEFECT — CLEARED (1b8922bd)
- Resolution: the reaper now maps `WaitStatus::Signaled(_,sig,_)` to `ChildExit::Signaled(sig)` (`child.rs:389`), a distinct variant from `ChildExit::Exited(code)`; `messages::child_reaped` renders the signal arm as `" The process was killed by signal N"` (`messages.rs:109`, C `procServ.cc:801-804`), never `128+sig`. `child_exit_code` is updated only under the `Exited` arm (`supervisor.rs:481`, C `WIFEXITED`-only). (Doc CLEARED-marker added retroactively; fix landed in an earlier round.)
- Rust: `src/procserv/child.rs:333-334` + `make_exit_status` `:343-349`
- C: `procServ.cc:801-805` (`WIFSIGNALED`→`WTERMSIG` → `" The process was killed by signal N"`)
- Impact: The reaper maps `WaitStatus::Signaled(_,sig,_)` to `make_exit_status(128+sig)`, collapsing a signal death into a fake normal exit. A child SIGKILLed (the default kill key) is reported by Rust as `exit status: 137` rather than "killed by signal 9"; the `WIFSIGNALED` branch never reaches any message. Data-loss root behind part of PS-2.

#### PS-4 Child exit code not propagated as procserv's own exit status (always exits 0)
- Severity: DEFECT — CLEARED (ad7dd347)
- Resolution: `SupervisorState::run` returns `ProcServResult<i32>` carrying `child_exit_code` (updated on each `WIFEXITED` reap, `supervisor.rs:481`; returned at both shutdown exits `:301/:310`), and `procserv_rs.rs:493` converts it to `ExitCode::from(code as u8)` so `procserv-rs -o /bin/false` exits 1 like C (`childExitCode`, `procServ.cc:701`). (Doc CLEARED-marker added retroactively; fix landed in an earlier round.)
- Merged from: process PS-4, config PS-95
- Rust: `src/procserv/supervisor.rs:431-475` (status discarded) + `src/bin/procserv_rs.rs:309-319` (`Ok(())`→`ExitCode::SUCCESS`)
- C: `procServ.cc:76-77,701,794-798` (`childExitCode = WEXITSTATUS(...)`; `main` returns it)
- Impact: `procServ -o /bin/false` (oneshot) exits **1** in C (wrapper/systemd see the failure); procserv-rs exits **0** regardless of how the child exited, masking IOC failure for any script keyed on procServ's exit code.

#### PS-5 No SIGKILL of the child's process group on child exit → orphaned grandchildren
- Severity: DEFECT — CLEARED (1e4e9bfe)
- Resolution: the `ChildEvent::Exited` arm now SIGKILLs the child's whole process group (`slot.handle.signal(libc::SIGKILL)`, `supervisor.rs:472`) on every reap before the next launch, reaping grandchildren — mirroring C `~processClass`'s unconditional `kill(-_pid, SIGKILL)` (`processFactory.cc:117`). Independent of `killSig` (hardcoded SIGKILL, as in C). (Doc CLEARED-marker added retroactively; fix landed in an earlier round.)
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
- Severity: DEFECT — CLEARED (this round)
- Resolution: new `procserv::endpoint` module ports C `acceptFactory`'s spec grammar (`acceptFactory.cc:104-150,229-325`) into `parse_endpoint(spec, local)` → an `Endpoint` (TCP `SocketAddr`, or `UnixEndpoint`). It recognizes a bare TCP port, an `A.B.C.D:port` interface bind (honored only when `local` is clear, i.e. `--allow`; loopback-forced otherwise, exactly as C's `local ? INADDR_LOOPBACK : A.B.C.D`), and `unix:` sockets — filesystem `/path`, access-controlled `user:grp:perm:/path` (resolved via `getpwnam`/`getgrnam`, 3-colon requirement enforced, octal perms parsed), and abstract `@name` (leading-`@` stripped). `ListenConfig` is restructured from the single-port `{tcp_port, tcp_bind, log_port, log_bind, unix_path}` to `{control: Vec<Endpoint>, log: Option<Endpoint>}` (C `ctlSpecs` vector + single log spec). `-P`/`--port` is now repeatable (each occurrence adds a control endpoint); `-l`/`--logport` takes the same spec grammar; `--unixpath` stays a filesystem-socket convenience (`= -P unix:<path>`). The supervisor spawns one listener task per control endpoint plus the log; `listener::run_unix` binds filesystem or abstract sockets (abstract is Linux-only via std `bind_addr`, errors elsewhere as C does); the sidecar info-file/env formatter emits `unix:@<name>` for abstract sockets. **Intentional divergence:** the legacy bare-positional endpoint form (C `singleEndpointStyle`, `procServ.cc:452-453`) is not reproduced — the Rust port keeps its `--`-delimited command and requires endpoints via `-P`/`--unixpath`, an unambiguous parser that supersedes the getopt positional. UNIX `chmod`/`chown` **application** is the immediately-following PS-24 (the perms are parsed here but applied there). Tests: 12 `endpoint::` parse cases, a UNIX filesystem listener accept test, bin endpoint-wiring tests (repeatable `-P`, allow/restrict interface binds, unix-via-spec/`--unixpath`, bad-spec error), sidecar abstract-address formatting.
- Merged from: conn PS-32/PS-33/PS-39, config PS-89
- Rust: `src/procserv/config.rs:19-25` (`tcp_port: Option<u16>` + single `tcp_bind`); `procserv_rs.rs:39-40,164-184`; `listener.rs:63-71`; `sidecar.rs:244-251` (no abstract form)
- C: `procServ.cc:211,386-388,515-518` (`ctlSpecs` vector — repeatable `-P`); `acceptFactory.cc:125-137` (`A.B.C.D:port` interface bind), `:241-279` (`user:grp:perm:` prefix → `getpwnam`/`getgrnam`/`strtoul(…,8)`), `:293,316-325` (leading `@` → abstract socket)
- Impact: C accepts multiple `-P` endpoints + richer syntax: bind a specific NIC (`192.168.1.5:4051`), several ports at once, a group-restricted UNIX socket (`unix:ioc:operators:0660:/run/ioc.sock`), or an abstract socket (`unix:@name`). Rust takes one numeric `--port` (localhost or 0.0.0.0 only) + a bare-path `--unixpath`. Interface-specific binds, multi-endpoint configs, UNIX access-control, and abstract sockets are all impossible (and a `user:grp:perm:` path either fails to bind or silently loses access-control intent).

#### PS-14 `-p` / `-P` short-option letters swapped (pidfile vs port)
- Severity: DEFECT — CLEARED (this round)
- Resolution: swapped the short letters to match C (procServ.cc:250-251,264): `--port` now takes `-P` (was `-p`), `--pidfile` now takes `-p` (was long-only). A wrapper `procServ -p /run/x.pid -P 4051 …` now routes the pidfile and port to the correct fields. **Breaking CLI change**, per the locked C-faithful CLI decision. Tests: `short_p_is_pidfile_and_short_uppercase_p_is_port`, `short_p_does_not_set_the_port`. (`-P`'s value stays a `u16` here; PS-13 expands it to the full endpoint-spec surface.)
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
- Severity: DEFECT — CLEARED (this round)
- Resolution: added `--noautorestart` (long-only in C; `-N` convenience short) → `RestartMode::Disabled` and `--oneshot`/`-o` → `RestartMode::OneShot`; `build_config` resolves the startup `restart_mode` from them (default `OnExit` = C `restart`, procServ.cc:58,369-375). The two flags `conflicts_with` at the clap layer (both set the same C `restartMode`). Also corrected the stale `RestartMode::Disabled` doc comment in restart.rs (it wrongly said "child exit shuts the supervisor down"; C `norestart` keeps the supervisor up — only `oneshot` shuts it down, as the `norestart_keeps_server_alive_after_child_exit` e2e test already asserts). Tests: `restart_mode_defaults_to_on_exit`, `noautorestart_starts_in_disabled_mode`, `oneshot_starts_in_oneshot_mode`, `noautorestart_and_oneshot_conflict`. (The runtime OneShot relaunch bug is the separate open PS-20.)
- Rust: `src/bin/procserv_rs.rs:244` (`restart_mode: RestartMode::OnExit`, hardcoded; no flags)
- C: `procServ.cc:248-249,369-375` (`-N`→norestart, `-o`→oneshot)
- Impact: C can start directly in no-restart or one-shot mode. Rust hardcodes `OnExit`; these modes are reachable only via the runtime toggle key. `procServ --oneshot …` / `procServ -N …` wrappers lose their startup mode.

#### PS-18 `-e` / `--exec` (separate child executable) absent
- Severity: DEFECT — CLEARED (this round)
- Resolution: added `ChildConfig.child_exec` / `ChildSpec.child_exec: Option<PathBuf>` and the `-e`/`--exec <path>` flag (real C short). `in_child_setup_and_exec` now keeps argv[0] = the positional command and execs `child_exec` when set, else the command itself — so a wrapper runs under the original command line (C `childExec`, procServ.cc:295-296,459-462). **Intentional divergence (C bug not copied):** C's `childArgv = argv + optind - 1` makes argv[0] the token *before* the command (the port / previous option value — garbage as argv[0]); the help text's documented intent is "specify child executable (default arg0 of command)", so the Rust port presents argv[0] = the command's arg0 instead. Tests: bin `exec_absent_runs_the_command_itself`, `exec_overrides_exec_target_only`; child integration `child_exec_runs_override_binary_with_command_as_argv0` (program is a bare name, `/bin/sh` runs and `echo $0` prints the name).
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
- Severity: CONCERN — CLEARED (this round)
- Resolution: ported C's `firstRun` flag as `SupervisorState::first_run`. C semantics (procServ.cc:59,597,656-667 + clientFactory.cc:226-227): `firstRun` is set at startup and whenever the operator toggles *into* oneshot, and cleared after each launch in the needsRestart block; the oneshot exit gate shuts down only when `!firstRun`, so a mid-run toggle into oneshot grants exactly one more launch. The Rust port had no such flag — the `OneShot` exit arm shut down unconditionally, one run short of C. Now: `first_run` is `true` at bootstrap and cleared by `respawn_child` (the single launch owner, alongside `pending_restart = None`), so a child *started* in oneshot exits→shutdown (one run); the `ToggleRestartMode` handler sets `first_run = true` on the `Disabled→OneShot` transition (C's `norestart→oneshot` branch), granting the extra run. The exit decision is factored into a pure `exit_disposition(mode, first_run) -> ExitDisposition` (`AutoRestart` w/ crash-loop cap | `OneShotRerun` no-cap | `StayDead` | `Shutdown`) — the testable core of C's needsRestart gate; the `OneShotRerun` arm schedules the relaunch behind the same holdoff C applies via `_restartTime`. Tests: unit `exit_disposition_*` (4 cases over the mode×first_run matrix) + e2e `toggle_into_oneshot_grants_one_more_run` (^T^T into oneshot, first ^X kill relaunches once, second ^X shuts down); existing e2e `child_exit_code_becomes_server_exit_code` now also guards the startup-oneshot-runs-once path (broken `first_run` clearing would loop-relaunch and time out).
- Merged from: process PS-6, telnet PS-59, config PS-96
- Rust: `src/procserv/supervisor.rs:458-468` (`OneShot` exit: relaunch only if `!has_run_once`) + `respawn_child` unconditional `has_run_once = true` `:514`; toggle `:386-393` never resets it
- C: `clientFactory.cc:223-229` (`firstRun = true` on toggle into oneshot) + gate `procServ.cc:656-667` (`if (restartMode==oneshot && !firstRun) shutdown`)
- Impact: C's toggle into oneshot grants exactly one more launch after the current child exits; only the *next* exit shuts the server. Rust never resets `has_run_once` (already true from the initial spawn), so the `if !has_run_once` relaunch arm and its `"@@@ One-shot relaunch"` banner are unreachable — `^T`-into-oneshot shuts the supervisor down on the first exit, one run short of C.

#### PS-21 Auto / initial spawn failure aborts the supervisor; C retries after holdoff
- Severity: CONCERN — CLEARED (this round)
- Resolution: the bootstrap and `restart_due` spawn paths no longer propagate a `respawn_child` failure via `?` (which terminated the supervisor). Both now route a spawn error through the new `schedule_spawn_retry`, which logs it and schedules a relaunch behind the full holdoff — C's handling of a `forkpty` failure: build a `markedForDeletion` child that still sets `_restartTime = holdoff + now` and retry on the next poll, never giving up (processFactory.cc:158,188). The retry is recorded against the Rust crash-loop cap (`RestartTracker`), so a *persistent* spawn failure (binary deleted, pty exhausted) with an explicit `--max-restarts` eventually terminates with `RestartLimitExceeded`; left at the CLI default (unlimited, PS-41) it retries indefinitely = C parity. The manual `^R` path stays operator-driven (log-and-continue, no auto-reschedule), matching its prior correct behavior. `pending_restart` is now set on child-exit *or* failed-respawn; its invariant comment updated. Tests: `spawn_retry_schedules_holdoff_when_under_cap` (failure under cap → `pending_restart` set, no abort) + `spawn_retry_gives_up_when_cap_exceeded` (cap=1 → second failure → `RestartLimitExceeded`), via a `#[cfg(test)]` `SupervisorState::for_test` minimal-state constructor (forkpty failure can't be injected deterministically — a missing binary execs-then-exits, a child *exit* not a spawn failure).
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
- Severity: CONCERN — CLEARED (this round)
- Resolution: `run_tcp` now binds through a new `tcp_listen` helper that creates a `TcpSocket`, calls `set_reuseaddr(true)` before `bind`, then `listen(LISTEN_BACKLOG)` — the exact order of C `acceptItemTCP::remakeConnection` (`acceptFactory.cc:187-191,207`). A systemd restart while prior client sockets linger in `TIME_WAIT` now rebinds immediately instead of failing `bind` with `EADDRINUSE` and aborting startup. The backlog stays tokio's 1024 (PS-34 intentional divergence, not C's 5). Test: `tcp_listen_binds_via_reuseaddr_path` exercises the new bind path (the rebind-over-`TIME_WAIT` effect itself is OS/timing-dependent and not deterministically unit-testable; the accept test covers the full path).
- Rust: `src/procserv/listener.rs:30-32` (`TcpListener::bind` — std/tokio do not set SO_REUSEADDR)
- C: `acceptFactory.cc:187-191` (`setsockopt(SO_REUSEADDR)` before `bind`)
- Impact: procServ is itself frequently restarted (systemd). After a restart while prior client connections linger in TIME_WAIT, C rebinds immediately; the Rust port can fail `bind` with `EADDRINUSE` and exit at startup (`ProcServError::ListenerBind`) until TIME_WAIT drains.

#### PS-24 UNIX socket permissions/ownership not applied (C forces 0666 + chown)
- Severity: CONCERN — CLEARED (this round)
- Resolution: `listener::apply_unix_perms` runs C's exact post-bind sequence on every filesystem control/log socket (`acceptFactory.cc:368-377`): `chmod 0` → `chown(uid,gid)` → `chmod perms`, with `perms` defaulting to `0o666` ("equivalent to tcp bind to localhost"). The 0-then-perms order closes the window where the socket carries its final mode but the pre-`chown` owner. `chown` runs only when a `user:grp:` override was parsed (no override ⇒ C's default self-`chown`, a no-op, is skipped); `chmod`/`chown` failures are logged and tolerated, matching C's PRINTF-and-continue (a non-root server still gets the usable `0o666` default). Abstract sockets have no filesystem node, so the perms step is gated out, exactly as C's `if(!abstract)`. Built on PS-13's `UnixEndpoint` carrying the parsed `uid`/`gid`/`perms`. Test: `unix_socket_mode_is_applied_after_bind` binds real sockets and reads back `0o666` (default) and `0o660` (explicit) independent of umask.
- Rust: `src/procserv/listener.rs:69-71` (`UnixListener::bind` — mode is umask-dependent, no chmod/chown)
- C: `acceptFactory.cc:368-377` (`chmod(path,0)`, `chown(path,uid,gid)`, `chmod(path,perms)` with `perms=0666` default)
- Impact: C sets the socket 0666 ("equivalent to tcp bind to localhost") so any local user can connect, and can chown to a configured user/group. The Rust socket inherits `0777 & ~umask`, so a peer in a different group may be unable to connect where C permitted it, with no way to set owner/group.

#### PS-25 Client sockets omit SO_KEEPALIVE / SO_SNDTIMEO → unbounded head-of-line block on the party-line
- Severity: CONCERN — CLEARED (this round)
- Resolution: closed both halves. **(A) SO_KEEPALIVE** — `client.rs::set_keepalive` sets `SO_KEEPALIVE` on every accepted TCP socket via `setsockopt` (tokio exposes no setter), called in `spawn_client`'s TCP arm; UNIX-domain clients are exempt (keepalive is a TCP option, no analogue). Test `set_keepalive_enables_so_keepalive` reads the flag back with `getsockopt`. **(B) SO_SNDTIMEO 10s → drop** — the fanout no longer does an unbounded `out_tx.send(...).await`. `supervisor.rs::send_to_all` now routes each per-client send through `send_bounded(tx, frame, CLIENT_SEND_TIMEOUT)` (`CLIENT_SEND_TIMEOUT = 10s`, C's `SO_SNDTIMEO`); a client whose bounded(64) channel stays full for the whole timeout (write task wedged on a dead socket) or whose channel is closed returns `false` and is collected into a `dead` list, then removed after the loop — the async analogue of C marking the peer `_markedForDeletion` and moving on. A healthy peer drains within the buffer and never trips the timeout, so the party-line can no longer be wedged by one dead client. `send_bounded` is factored out so the boundary is unit-testable without a real 10s wait (tests: `send_bounded_queues_when_buffer_has_room`, `send_bounded_drops_on_closed_channel`, `send_bounded_drops_on_full_buffer_timeout`).
- Rust: `src/procserv/client.rs:120-152` (no socket options) — fanout awaits `out_tx.send(...).await` over a bounded(64) channel (`supervisor.rs:619/641`)
- C: `clientFactory.cc:146-147` (`SO_KEEPALIVE`; `SO_SNDTIMEO` 10s)
- Impact: C bounds a stuck/dead client's blocking write at 10s, then marks it `_markedForDeletion` and continues; KEEPALIVE surfaces silently-dropped peers. Rust has neither: a silently dead client (cable pull) is never detected, its 64-deep channel fills, and the supervisor's `fanout_*().await` blocks the **entire party-line** (no other client gets output, child PTY processing stalls, new connections can't be accepted) for as long as the OS keeps the TCP write pending — potentially minutes.

#### PS-26 Telnet IAC negotiation written before the banner (order reversed vs C)
- Severity: CONCERN — CLEARED (this round)
- Resolution: removed the negotiation prelude from the client write task and moved it into the supervisor's `handle_new_client`, which now enqueues the banner `Bytes` frame *then* the `RawIac(initial_negotiation())` frame — the FIFO outbound channel writes them in that order, so the peer sees `[banner][IAC WILL ECHO][IAC DO LINEMODE]`, matching C's ctor order (write greeting/info, then `telnet_negotiate`, clientFactory.cc:153-174). The write task is now a pure drain loop. C negotiates with every client (the telopt loop is unconditional), so the supervisor enqueues the IAC frame for readonly clients too. Tests: e2e `banner_precedes_telnet_negotiation` (raw connect bytes: stream does not start with `0xFF`, and "Welcome to procServ" appears entirely before the first IAC byte); the three `client.rs` unit tests that skipped the old 6-byte prelude were updated (a bare `spawn_client` now sends nothing until a frame is queued).
- Merged from: conn PS-36, telnet PS-57
- Rust: `src/procserv/client.rs:198-210` (write task sends `initial_negotiation()` first, then drains the banner frame)
- C: `clientFactory.cc:153-174` (constructor `write()`s greeting/info first, then `telnet_init`+`telnet_negotiate`)
- Impact: First bytes differ. C: `[banner text][IAC WILL ECHO][IAC DO LINEMODE]`. Rust: `FF FB 01 FF FD 22` then banner. Functionally tolerated by most telnet clients, but a byte-exact connect-sequence capture sees the IAC bytes interleaved before the greeting.

#### PS-27 Control-char CLI uses decimal `u8` + renamed flags (no `^X` caret notation)
- Severity: CONCERN — CLEARED (this round)
- Resolution: replaced the decimal-`u8` `--kill-char`/`--toggle-restart-char`/`--logout-char` flags with C's names + caret notation — `-k`/`--killcmd` (killChar), `--autorestartcmd` (toggleRestartChar; long-only in C, `-T` convenience short), `-x`/`--logoutcmd` (logoutChar). Added `get_option_char`, a byte-exact port of C `getOptionChar` (procServ.cc:142-152): empty → 0 (disabled), `^^` → `^`, `^A`..`^Z` → byte−64, else first byte. Defaults preserved (kill `^X`, toggle `^T`, logout disabled). **Faithful C quirk documented (not a bug):** Ctrl-] (0x1d) is NOT producible from `^]` — C gates `^`-notation to A–Z, so `^]` parses to `^`; the logoutcmd help directs operators to pass the raw 0x1d byte. README flag table updated. Tests: `get_option_char_matches_c_semantics`, `command_keys_default_to_c_values`, `killcmd_parses_caret_and_disable`, `autorestartcmd_sets_toggle_key`, `logoutcmd_enables_logout_key`.
- Merged from: telnet PS-61, config PS-87
- Rust: `src/bin/procserv_rs.rs:134-144` (`--kill-char`/`--toggle-restart-char`/`--logout-char` as decimal `u8`)
- C: `procServ.cc:142-152` (`getOptionChar`: `^X`→0x18, `^^`→`^`), options `--killcmd`/`-k`, `--autorestartcmd`/`-T`, `--logoutcmd`/`-x`
- Impact: C accepts caret notation (`--killcmd '^X'`). Rust requires a decimal byte (`--kill-char 24`) under different flag names. A wrapper passing `--killcmd ^X` both fails to find the option and can't use caret notation; the control-char *values* match but the *configuration surface* does not.

#### PS-28 `-d`/`--debug`, `-q`/`--quiet`, and `PROCSERV_DEBUG` env unsupported
- Severity: CONCERN — CLEARED (this round)
- Resolution: added `-d`/`--debug` and `-q`/`--quiet` flags and the `PROCSERV_DEBUG` env read, closing the clap-rejects-the-flag gap. **Debug** (`-d` OR a present `PROCSERV_DEBUG`, C `inDebugMode`, procServ.cc:225,291-292): keeps the child in the foreground — C's daemonize gate is `!inFgMode && !inDebugMode` (procServ.cc:549), so `build_config` folds `args.debug` into `foreground` and `entry` ORs in the env half — and raises diagnostics, mapping C's `PRINTF` (`if (inDebugMode) printf`, procServ.h:30) to a `debug`-level tracing default that RUST_LOG still overrides. **Quiet** (`-q`, C `quiet`, procServ.cc:390-391): gates the "Warning: No log file[ and no port for log connections] specified." stderr notice C prints when daemonizing without a log file (procServ.cc:889-893); the message now fires (unless `--quiet`) only on the daemonize path, naming the log-port variant exactly as C. Tests: `debug_and_quiet_flags_parse` (both short+long accepted), `debug_forces_foreground` (`-d` ⇒ `foreground`). README flag table updated. NOTE — the daemon-spawn pid notice C prints alongside the no-logfile warning (`spawning daemon process: <pid>`) needs the foreground parent to know the daemon pid, which the current double-fork loses; it is folded into the PS-32 forking rework rather than faked here.
- Rust: `src/bin/procserv_rs.rs` (no `-d`/`-q`; no `PROCSERV_DEBUG` read)
- C: `procServ.cc:225` (`getenv("PROCSERV_DEBUG")`→inDebugMode), `:291-293` (`-d`), `:390-392,889-895` (`-q` suppresses the spawn banner & log warnings)
- Impact: `-d` (debug/foreground+printf), `-q` (suppress the `spawning daemon process: <pid>` / no-logfile warnings), and `PROCSERV_DEBUG` are accepted by C and silently unavailable in Rust (clap rejects `-d`/`-q`).

#### PS-29 `--allow` not compile-gated; Rust always binds all interfaces — CLEARED (intentional divergence, doc-only)
- Severity: CONCERN
- Rust: `src/bin/procserv_rs.rs:166-172` (`--allow` → bind `0.0.0.0` unconditionally)
- C: `procServ.cc:43-47` (`enableAllow=false` unless `ALLOW_FROM_ANYWHERE` build), `:272-277` (`--allow` prints "not supported" / no-op in the default build)
- Impact: In the default C build `--allow` is refused and the control port stays localhost-only (a deliberate security default). Rust honors `--allow` always. A wrapper carrying `--allow` that was inert on stock procServ now exposes the control console to the network.
- Disposition: kept — `--allow` is honored as a genuine runtime opt-in. C's behavior is a *build-time* gate (`ALLOW_FROM_ANYWHERE`): in the common stock build `--allow` is inert and prints "not supported", so the flag's effect depends on how the binary was compiled rather than on the operator's intent. The Rust port has no compile-time variants, so it treats `--allow` as what it plainly reads as — an explicit request to bind all interfaces — and the secure default (localhost-only) still holds whenever `--allow` is absent. The default stays safe; only an operator who explicitly passes `--allow` gets the broad bind, which is the documented intent of the flag. The README flag table and the source (`procserv_rs.rs:50-53`, C-citation) document the localhost default + the `--allow` opt-in. Recorded as a deliberate divergence; no code change. (If a site needs C's "refuse `--allow`" posture, that is a future opt-in build/runtime flag, not a silent default.)

#### PS-30 `PROCSERV_PID` env not consulted for the pidfile default
- Severity: CONCERN — CLEARED (this round)
- Resolution: `build_config` now resolves the pidfile through a pure `resolve_pidfile(flag, env)` helper applying C's precedence — `-p`/`--pidfile` overrides `PROCSERV_PID`, which provides the default when the flag is absent (procServ.cc:224,382) — and an empty value (flag or env) means "no pidfile", matching C's `!pidFile || strlen(pidFile)==0` guard (procServ.cc:125). The env read happens at the build_config call site (`std::env::var_os("PROCSERV_PID")`); the precedence logic is the pure helper, so it is tested without env pollution (`pidfile_resolves_flag_over_env_over_none`: flag>env>none + both empty-cases). `--pidfile` doc comment updated.
- Rust: `src/procserv/*` (no `PROCSERV_PID` read; pidfile only from `--pidfile`)
- C: `procServ.cc:224` (`pidFile = getenv("PROCSERV_PID")` when `-p`/`--pidfile` absent)
- Impact: C lets the PID-file path come from `PROCSERV_PID` (used by some service wrappers). Rust ignores the env entirely, so an environment-driven pidfile is never written.

#### PS-31 Log file requested mode 0666 (vs C 0644) and no `fsync`
- Severity: CONCERN — CLEARED (this round)
- Resolution: (1) `open_handle` now requests mode `0o644` via `OpenOptions::mode` (`S_IRUSR|S_IWUSR|S_IRGRP|S_IROTH`, C procServ.cc:924) instead of the platform default 0666; `mode()` applies only on creation, like the mode arg to C's `open()`, so a permissive umask no longer yields a group/other-writable log. (2) `LogSink::write_all`'s File arm now `sync_all()`s (fsync) after the write/flush, matching C's `fsync(logFileFD)` after every log write (procServ.cc:748) — a power loss can no longer lose lines the server already accepted; tokio's `flush` is not an fsync. The Stdout arm (`--logfile -`) stays flush-only: C's `fsync(1)` on the tty/pipe case is an error-ignored no-op, and tokio's `Stdout` exposes no sync (a `--logfile -` redirected to a regular file is the lone residual — minor, fd-1 case only). Test: `log_file_is_created_mode_0644` (umask cleared for determinism under nextest's process-per-test, asserts the created file is exactly `0o644`).
- Rust: `src/procserv/sidecar.rs:86-89` (`create(true).append(true)` → mode 0666 & ~umask), `:121-122/:157-158` (`flush()` only)
- C: `procServ.cc:924` (`open(..., S_IRUSR|S_IWUSR|S_IRGRP|S_IROTH)` = 0644), `:748` (`fsync` after every write)
- Impact: (1) C requests 0644 (no group/other write); Rust 0666, so under a permissive umask the log is group/other-writable where C's is not. (2) C `fsync`s after each write; Rust only `flush`es (tokio File flush is not an fsync), so on power loss buffered lines C would have synced can be lost.

#### PS-32 PID file written by the grandchild after both parents exit (type=forking race) — CLEARED
- Severity: CONCERN
- Rust: `src/procserv/daemon.rs:41-78` (`fork_and_go` writes no pidfile) + `supervisor.rs:159-161` (grandchild writes it during bootstrap)
- C: `procServ.cc:896-898` (parent writes the pidfile *before* `exit(0)`, with the explicit comment "removes a race condition using a type=forking systemd service")
- Impact: C writes the pidfile from the foreground parent before it exits. Rust's foreground process exits in `fork_and_go` and the grandchild writes the pidfile later; a `Type=forking` systemd unit can observe the original process gone before the pidfile appears — the exact race C's code comments out.
- Resolution: structural — replaced the double `fork` (a divergence mislabeled as "mirrors C `forkAndGo`": C uses a **single** fork, procServ.cc:882-911) with C's single fork. The foreground parent now owns the parent-side side-effects via a new `DaemonParent` params struct: it prints the spawn notice (`spawning daemon process: <pid>`, the message PS-28 deferred here) and the no-log-file warning (both gated on `!quiet`, C procServ.cc:889-894), then writes the pid file **with the daemon child's pid** before `exit(0)` — closing the `Type=forking` race exactly as C's comment describes. The child redirects stdio→/dev/null then `setsid`s (C order, procServ.cc:904-910); the `chdir`-omission (PS-15) and pre-fork `/dev/null` open are preserved. The PS-28 no-log-file warning moved out of `entry()` into the fork parent so it fires from the same site and order as C. The supervisor bootstrap still writes the pid file with the daemon's *own* pid — in daemon mode that re-publishes the identical value the parent already wrote (idempotent, atomic rename), and it remains the **sole** writer in foreground / library use where there is no parent; a clarifying comment records that relationship so the two writers aren't misread as a defect. Not separately unit-testable (forking from the test harness is hostile, as the existing `mod tests` note records); the foreground/daemon split is covered by the e2e suite. `crates/epics-tools-rs` fmt + clippy(`-D warnings`,`--all-targets`) clean; 145/145 nextest pass.

### NOTES

#### PS-33 PTY EOF on a still-running child doesn't terminate it — CLEARED (accepted edge-case divergence, doc-only)
- Severity: NOTE
- Rust: `src/procserv/child.rs:281-323` (reader breaks on EOF/EIO, no death signal) + `supervisor.rs:251-256`
- C: `processFactory.cc:227-242` (`readFromFd` sets `_markedForDeletion` on EOF/err) → `~processClass` SIGKILLs the group
- Impact: When the child closes its controlling tty but keeps running, C marks it dead → destructor SIGKILLs the group, ending the child. Rust's reader task just stops while the reaper blocks on `waitpid`; the child is neither killed nor reported and output forwarding silently stops. Edge case (child detaching its tty while alive).
- Disposition: accepted as a benign edge divergence. The triggering condition — the PTY **master read** returning EOF/EIO while the child is **still alive** — requires the child to *release its controlling terminal while running* (close all slave fds AND detach the ctty via `setsid`/`TIOCNOTTY`). As established in the PS-37 investigation, a child merely closing its fds keeps the ctty reference, so the master neither errors nor EOFs; the master read EOFs in practice only when the child **exits**, and then the reaper's `waitpid` reaps it and marks it dead through the normal path. So the "reader stops but child lives" state is effectively unreachable for any normal child. C's `_markedForDeletion → SIGKILL` is defensive cover for the exotic ctty-detach case; the Rust port relies on the reaper for the common exit case and accepts the exotic detached-child case (an operator can still `^X`/`^R`). Re-creating C's auto-SIGKILL-on-EOF would risk killing a child that legitimately re-opened its tty, for a path that does not arise in practice. No code change; revisit only if a real workload is found that detaches its tty while expecting to keep running.

#### PS-34 Listen backlog differs (C=5, Rust=tokio default ~1024) — CLEARED (intentional divergence, doc-only)
- Severity: NOTE
- Rust: `src/procserv/listener.rs:30/:70` (mio default backlog 1024)
- C: `acceptFactory.cc:216,363` (`listen(_fd, 5)`)
- Impact: Under a connection burst, C refuses past the 6th queued connection; Rust queues ~1024. Observable only as different refusal thresholds; no data divergence.
- Disposition: kept at 1024 (a named `LISTEN_BACKLOG` const, listener.rs:17-21, with the C-citation + rationale already in source). C's `listen(fd, 5)` is a historically tiny limit; queuing a burst rather than refusing past the 6th pending connection is strictly more tolerant and never loses data — declining C's small backlog, not bug-copying it down to 5. No code change. (Steering: find divergences, don't copy C's limitations.)

#### PS-35 Accept-error handling: C rebuilds the listen socket, Rust retries with no backoff — CLEARED
- Severity: NOTE (Rust correctly declines a C misbehavior, but adds a busy-loop risk)
- Rust: `src/procserv/listener.rs:48-53/:86-88` (log `warn!` and continue on the same listener)
- C: `acceptFactory.cc:396-399` (`remakeConnection()` — close + re-socket/bind/listen)
- Impact: On a transient accept failure (EMFILE) C tears down and re-creates the whole listen socket (briefly unbinding, dropping queued connections — questionable); Rust correctly keeps the bound listener. Aside: Rust's retry has no backoff, so a *persistent* error tight-loops `accept→warn→accept` burning CPU until an fd frees; C re-enters `pselect` (0.5s) between attempts. A small backoff would close the busy-loop without copying C's unbind.
- Resolution: added a 100ms `ACCEPT_ERROR_BACKOFF` sleep after a logged accept error in **both** accept loops (TCP + UNIX — same defect, fixed at both sites), so a persistent `EMFILE`/`ENFILE` no longer busy-spins a core; recovery is within 100ms of an fd freeing. We keep declining C's `remakeConnection` unbind (the bound listener and its queued connections survive); the backoff only closes the busy-loop the audit flagged. `cargo clippy`/`nextest` on `epics-tools-rs` clean.

#### PS-36 UNIX socket file not unlinked on shutdown — CLEARED
- Severity: NOTE
- Rust: `src/procserv/listener.rs:63-91` (removes a stale file only at startup `:69`; no unlink on drop, despite the `:58-62` comment claiming "best-effort unlink here too")
- C: `acceptFactory.cc:331-335` (`~acceptItemUNIX` → `unlink(addr.sun_path)` for non-abstract)
- Impact: After a clean Rust shutdown the socket file is left on disk; C removes it. Self-corrects on next start, but between runs a liveness check testing the socket file's existence sees a stale node. (Stale comment to fix or implement.)
- Resolution: added an `UnlinkOnDrop(PathBuf)` RAII guard, created in `run_unix` for filesystem (non-abstract) sockets and held across the accept loop. It mirrors C's `~acceptItemUNIX` destructor and fires on both exit paths — the supervisor dropping `incoming_rx` (`out.send` fails → normal return) and runtime/abort teardown (the future, and the guard with it, is dropped at the suspended `.await`) — so the socket node never outlives the listener. Abstract sockets have no filesystem presence and get no guard. The "stale comment" the finding cites no longer exists (line numbers had shifted; the surviving comments at `:89-92`/`:135-137` already correctly describe the *startup* unlink). New test `unix_socket_file_unlinked_when_listener_task_ends`. 146/146 nextest pass.

#### PS-37 SIGPIPE notice is never broadcast (C announces it to all clients) — CLEARED (intentional decline, doc-only)
- Severity: NOTE
- Rust: `src/procserv/daemon.rs:96-99` (SIGPIPE → `SIG_IGN`, no broadcast); supervisor has no SIGPIPE arm
- C: `procServ.cc:628-631` (`SendToAll("@@@ Got a sigPipe signal: Did the child close its tty?\r\n", NULL)`)
- Impact: When the child closes its tty and a write raises SIGPIPE, C emits a diagnostic line to every console; Rust silently ignores it. Loss of an operator-visible diagnostic, no functional impact.
- Disposition: declined as bug-copying (steering: find divergences, don't copy C's bugs). Investigation: C catches SIGPIPE *globally* (`OnSigPipe` sets a flag, the main loop broadcasts) and SIGPIPE is raised by C's **blocking writes to a broken fd** — in practice a disconnected **client socket** or the **log pipe** inside `SendToAll`, *not* the child PTY (a PTY-master write to a closed/released slave returns `EIO`, which raises no SIGPIPE). So C's "Did the child close its tty?" text is a guess that is usually wrong, and the notice fires mainly on **client disconnects**. The Rust port already handles a disconnected client cleanly: SIGPIPE is `SIG_IGN`, the async write returns `EPIPE`, and `send_to_all` drops the dead client (PS-25) — no process death, no broadcast needed. The literal scenario in C's message (a child that *detaches its controlling tty while still running*) is exotic: a child merely closing its fds keeps the ctty reference, so the master write still succeeds (verified — `forkpty` child closing 0/1/2 echoes input rather than erroring on both Linux and Darwin); the write breaks only when the child **exits** (→ `alive=false` → `write_stdin` returns `ChildExited`, a different case) or calls `TIOCNOTTY`/`setsid` while alive (no normal child does this, and it is not portably constructible in a test — macOS ships no `setsid`). Replicating C's broadcast on every broken write would reproduce C's misleading client-disconnect noise; emitting it only in the exotic ctty-detach case adds an effectively-unreachable, unverifiable path. Rust correctly declines the misbehavior. No code change.

#### PS-38 Read-only client telnet *replies* are still processed (C discards all logger input) — CLEARED
- Severity: NOTE
- Rust: `src/procserv/client.rs:163-185` — `TelnetEvent::Data` is gated on `!readonly` `:166` but `TelnetEvent::Reply` is **not** `:176-184`
- C: `clientFactory.cc:192` (`else if (!_readonly) telnet_recv(...)` — a logger's input never reaches the telnet state machine)
- Impact: A logger that sends IAC negotiation gets telnet responses from Rust but silence from C. A log-capture tool on the readonly port could receive unexpected IAC sequences mid-stream under Rust.
- Resolution: structural — the per-event `!readonly` gate (which only suppressed `Data`, letting `Reply` through) is replaced by an early `if readonly { continue; }` that skips `parser.feed` entirely. The read loop still runs (so EOF/disconnect is detected) but a logger's bytes never reach the telnet state machine, exactly as C's `else if (!_readonly) telnet_recv`. No data is forwarded and no IAC reply is emitted for readonly clients. New test `readonly_client_telnet_negotiation_gets_no_reply` proves the same `IAC DO <opt>` bytes yield a `TelnetReply` on a control client but nothing on a readonly one (so the gate, not inert input, is what suppresses it). `epics-tools-rs` clippy/nextest clean.

#### PS-39 Dead-child killChar drops C's "@@@ Got a kill command" broadcast (intentional-divergence candidate) — CLEARED (intentional decline, doc-only)
- Severity: NOTE
- Rust: `src/procserv/menu.rs:52-75` (dead-child `^X` returns only `RestartChild`) → `supervisor.rs:377-385` emits `"@@@ Manual restart"`
- C: `clientFactory.cc:236-241` (the killChar block is **ungated** by `processClass::exists()`, so dead-child `^X` runs `restartOnce()` *and* broadcasts `"\r\n@@@ Got a kill command\r\n"` even though the signal is a no-op `processFactory.cc:281`)
- Impact: Rust collapses dead-child kill into a pure restart and emits `"@@@ Manual restart"` instead — the misleading "kill" notice C sends is absent. Arguably Rust correctly declines C's misleading message; flag as an intentional-divergence candidate, confirm before "fixing".
- Disposition: declined (don't copy C's bug). On a **dead** child, C's killChar path is ungated by `processClass::exists()`, so it announces `"@@@ Got a kill command"` and then sends the kill signal to a non-existent process — a guaranteed no-op (`processFactory.cc:281` early-returns when the child is gone). The notice tells the operator a kill happened when nothing was signalled; the only real effect of dead-child `^X` is the `restartOnce()` that runs alongside it. The Rust port models that real effect directly: dead-child `^X` returns `RestartChild` and emits the accurate `"@@@ Manual restart"`. Reproducing C's "Got a kill command" on a dead child would re-introduce a misleading message for an action that signals nothing. Live-child `^X` still broadcasts the kill notice in both. Kept as a deliberate divergence; no code change.

#### PS-40 `-c`/`-I`/`-n` short aliases dropped; `-F` added as a non-C short — CLEARED
- Severity: NOTE
- Rust: `src/bin/procserv_rs.rs:92` (`-F` for `--timefmt`), `:99-101` (`--info-file` long-only), `:113-118` (`--chdir`, `--name` long-only)
- C: `procServ.cc:234,241,247` (`-c`/`--chdir`, `-I`/`--info-file`, `-n`/`--name`); `--timefmt` is long-only, `F` is not in the C optstring
- Impact: Minor surface drift. Wrapper scripts using `-c <dir>`/`-I <file>`/`-n <name>` are rejected by clap; `-F` is a Rust-only addition.
- Resolution: added the three missing C-faithful shorts — `short = 'c'` on `--chdir`, `short = 'I'` on `--info-file`, `short = 'n'` on `--name` (each carries a `procServ.cc:264` optstring citation). These three letters are present in C's getopt optstring `"+c:de:fhi:I:k:l:L:n:op:P:qVwx:"` (procServ.cc:264), so a drop-in wrapper invoking them now parses identically to the long forms. Verified the C optstring/long_options split: `-F`/`-S`/`-N`/`-T`/`-C`/`-K` are *long-option return codes only* in C (their letters are absent from the optstring), so C rejects them as `-X` too — the Rust crate keeps them as documented additive convenience shorts (their doc comments already cite this), which does not break drop-in compat because no C wrapper could be using them as shorts. Regression: `c_faithful_short_aliases_parse` (procserv_rs.rs) asserts `-c/-I/-n` and the long forms build the identical `cfg.child.cwd` / `cfg.logging.info_path` / `cfg.child.name`. README flag table updated (`-c`/`-I`/`-n` shown on the chdir/info-file/name rows).

#### PS-41 Rust-only restart-count knobs (`--max-restarts`/`--restart-window`/1s delay) (intentional)
- Severity: NOTE (intentional divergence — Rust opt-in cap over C's never-give-up)
- Rust: `src/bin/procserv_rs.rs:120-132,235-243` (`RestartPolicy{ max_restarts, window, delay: 1s }`)
- C: `procServ.cc:599` (`while(!shutdownServer)` never caps), `processFactory.cc:48-56` (gate only on mode + holdoff)
- Impact: No C analog. Defaults inert (`max_restarts = u32::MAX`), and procserv times relaunches off `holdoff` (`supervisor.rs:669-678`) not `RestartPolicy.delay`, so `delay: 1s` appears unused on the procserv path. Correctly declines C's never-give-up as an opt-in cap; noted so the extra surface isn't mistaken for parity (and so the unused `delay` isn't read as a behavior).
- Disposition: KEEP (intentional divergence, no code change). The default leaves C's never-give-up behaviour intact; the cap is operator opt-in. Recorded so the extra CLI surface is not mistaken for a parity gap on later review rounds.

#### PS-42 Intentional-divergence asides — redundant `setsid` skipped; `RestartTracker` opt-in (intentional)
- Severity: NOTE (not Rust defects)
- Rust: `src/procserv/child.rs:213-216`; `src/bin/procserv_rs.rs:235-243`
- C: `processFactory.cc:204`; `processFactory.cc:48-56`
- Impact: (a) C redundantly calls `setsid()` in the child after `forkpty` (returns EPERM, harmless); Rust correctly skips it since `forkpty` already creates the session — same resulting pgid. (b) `RestartTracker` has no C analog; default `max_restarts = u32::MAX` keeps parity with C's never-give-up. Neither is a parity defect; flagged so they aren't re-reported. (`sidecar.rs` is pure pid/info/log/env file management — C `writePidFile`/`writeInfoFile`/`setEnvVar`/`openLogFile`, not a child-lifecycle counterpart.)
- Disposition: KEEP (intentional divergence, no code change). (a) Skipping the EPERM-returning `setsid()` yields the same session/pgid C ends up with — copying the redundant call would be copying a no-op. (b) Tracked under PS-41. Recorded so neither is re-reported as a parity defect on later review rounds.

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

#### PS-48 Fallible startup side-files (`pid`/`log`) abort `bootstrap` where C warns-and-continues — CLEARED
- Severity: CONCERN
- Found: round-4 caucus review (rev-ps-process), reported there as "PS-43 (NEW)"; renumbered (PS-43 was already taken)
- Rust: `src/procserv/supervisor.rs:192-194` (`write_pid_file(...)?`), `:195-209` (`LogFile::open(...).await?`)
- C: `procServ.cc:131-136` (`writePidFile`: "Don't stop here - just go without"), `procServ.cc:925` (`openLogFile`: same)
- Impact: C opens both side-files in the foreground process and, on failure, prints a warning and **runs anyway** (explicit "just go without"). Rust `?`-propagated the error out of `bootstrap`, aborting the supervisor. In daemon mode this is worse than a plain divergence: the foreground parent has already printed the spawn notice, written the pid file, and `exit(0)`'d **before** the daemon child reaches `bootstrap`, so a misconfigured `--logfile /badpath/...` (or unwritable `--pidfile`) returns success to the shell/systemd while the IOC never starts — a silent false-start a `Type=forking` unit treats as "up".
- Resolution: both sites now warn (`tracing::error!`) and continue — the pid write tolerates an unwritable path (`continuing without it`), and the log open falls back to `None` (`continuing without a log`), matching C's identical "Don't stop here - just go without" policy at each. This is a **defect-family** fix: the reviewer cited the log only, but C warns-and-continues for the pid file at `procServ.cc:131-136` with the same comment, so both `?`-abort sites are fixed. Distinct and intentionally left: the listener binds (PS-49 — fail-fast policy, not continue). **Correction (PS-50):** this note originally also classified the SIGHUP-register `?` as distinct ("no C 'go without' analog"); that was wrong — C installs every signal handler pre-fork with *unchecked* `sigaction` (`procServ.cc:496-509`), so a register failure can neither abort nor reach the daemon, making the post-fork `?`-abort a member of this same warn-continue family. See PS-50. Regression `unwritable_pid_and_log_paths_do_not_abort_startup` (procserv_e2e) points both side-files at a missing directory and asserts the child still starts (round-trips a line through `cat`) and neither file was created. The residual warning-visibility gap (in daemon mode the warning goes to the redirected stderr, where C — opening pre-fork — reaches the terminal) is closed by PS-49's pre-fork relocation.

#### PS-49 Listener bind failure is swallowed in a detached task; C exits startup — CLEARED
- Severity: DEFECT
- Found: round-4 caucus review (rev-ps-conn), reported there as "PS-51 (new)"; renumbered (collision with the raw round-1 telnet range)
- Rust: `src/procserv/supervisor.rs:214-230` (each endpoint `tokio::spawn`ed detached) → `spawn_endpoint` logged `tracing::error!` and the task ended; bootstrap never observed the result and the event loop ran regardless
- C: `procServ.cc:513-526` (control) / `:531-542` (log) — `acceptFactory` `throw errno` on `socket`/`bind`/`listen` failure (`acceptFactory.cc:184,207-219`), caught by `perror` + `exit(error)`, **before** the fork at `procServ.cc:551`
- Impact: When a control or log port is already in use (`EADDRINUSE`), or an abstract socket is requested on a non-Linux host, C aborts startup with a non-zero exit + diagnostic. Rust instead daemonized, spawned the child IOC, logged one error line, and kept running with that listener absent. With the only control port taken, the operator got a **silently headless** IOC that reported success but was unreachable for console/kill/restart. Shared structural cause with PS-48: Rust performed the fallible startup ops post-fork, after the parent reported success; C does them pre-fork.
- Resolution: the bind is split from the accept loop and **relocated into the foreground process, before `fork_and_go`** (`listener::bind_endpoints` → `PreboundListener`; `supervisor.rs:214-225`; `procserv_rs.rs` entry binds via a short-lived current-thread runtime, fully dropped before the fork, and the bound `std` listener fds are inherited by the daemon child, which re-adopts them with `from_std`). A bind failure now returns `Err` from `bind_endpoints` → the daemon binary prints the diagnostic and `exit(FAILURE)` **before** daemonizing (C-exact fail-fast), and the foreground/library path `?`s the same error out of `bootstrap` to the caller. The `ProcServ::with_prebound` API addition is additive (existing `ProcServ::new(cfg).run()` still binds in `bootstrap`), so no break. The structural cause is closed for the whole family: bind (PS-49, fail-fast) and the side-file opens (PS-48, warn-continue) both now execute in the foreground process pre-fork. Tests: `bind_one_fails_fast_on_address_in_use` + `bind_one_binds_a_free_port_and_accepts` (listener unit) and `occupied_control_port_fails_fast_not_headless` (procserv_e2e: a second supervisor on an occupied port returns `Err` from `run()` within 2s rather than coming up headless). The daemon-mode fork-inherit path itself is review-verified, not auto-tested — the e2e harness runs in foreground, as it does for all daemonize code (PS-32).

#### PS-50 Post-fork signal-handler registration `?`-aborts the daemon child where C never aborts — CLEARED
- Severity: CONCERN
- Found: round-5 caucus review (rev-ps-process), during the PS-48 defect-family re-scan; corrects PS-48's "SIGHUP-register is distinct" classification
- Rust: `src/procserv/daemon.rs:156` (SIGPIPE `SIG_IGN`), `:160` (SIGTERM), `:162` (SIGINT) in `install_signal_handlers`, and `src/procserv/supervisor.rs:282` (SIGHUP) in `bootstrap` — each `?`-aborted on a failed disposition/registration
- C: `procServ.cc:477` installs all handlers **before** `forkAndGo` at `procServ.cc:551`; the six `sigaction()` calls (`:496-509`) are **unchecked** (NULL old-action, no return test)
- Impact: C installs every handler in the foreground parent and never checks the result, so a signal-install failure can neither abort nor reach the daemon. Rust must register **post-fork in the daemon child** — the tokio reactor can't survive `fork()` — and `?`-aborted four of those sites. In daemon mode the parent has already printed the spawn notice, written the pidfile with the child's pid, and `exit(0)`'d (systemd `Type=forking` marks the unit *started*) before the child reaches the registration; a failure then returns `FAILURE`/propagates `Err` and the child dies — the same **headless-daemon false-success** PS-32/PS-48/PS-49 exist to prevent. This is a structural sibling of PS-48 (post-fork fallible startup op → false-start), **not** distinct as PS-48 first recorded. Reachability is near-nil — `tokio::signal::unix::signal()` registers into the global signal driver and effectively never fails on a fresh process (unlike bind's common `EADDRINUSE`) — hence CONCERN, not DEFECT.
- Resolution: Rust's architecture *forces* post-fork registration, so the C-faithful close is not "move it pre-fork" but **warn-continue instead of `?`-abort**, matching C's unchecked `sigaction` and the PS-48 pidfile/log policy. `install_signal_handlers` is now infallible (returns `ShutdownSignal`, not `Result`): SIGPIPE-ignore failure logs and continues; SIGTERM/SIGINT become `Option<Signal>` (a failed one logs → `None`). The SIGHUP field on the supervisor likewise becomes `Option<Signal>` (failed register → log → `None`, dropping log-reopen-on-HUP). A single owner `daemon::recv_optional_signal(&mut Option<Signal>)` awaits each stream and **parks forever on `None`** via `std::future::pending()`, so the disabled handler's `select!` arm never fires and the daemon keeps running — it does not abort, hot-loop, or fire a spurious shutdown. This is a **defect-family** fix across all four sites; the reviewer named three (SIGHUP/SIGTERM/SIGINT) and SIGPIPE at `daemon.rs:156` is the fourth, same family. Distinct/skipped: the `#[cfg(test)] for_test` SIGHUP `.expect` (test-only) and the runtime `handle.signal(...)` *kill-forwarding* calls (not handler install; already `let _ =`-ignored). *Intentional-divergence note:* C's unchecked `sigaction` silently swallows a genuinely-failed install; Rust does not copy that — log-and-continue is observable and strictly better than both C's silence and the prior abort (steering: don't copy C's bugs). Test: `recv_optional_signal_none_never_fires` (daemon unit) pins the `None`-parks-forever contract. The post-fork registration-failure path itself is not auto-tested — a `tokio::signal` register failure can't be forced deterministically, the same untestability as PS-32's fork path — and is closed by inspection.

#### PS-51 Fail-fast config `validate()` is stranded post-fork → headless-daemon false-success on no-control-endpoint — CLEARED
- Severity: DEFECT
- Found: round-6 caucus review (rev-ps-process), missed-sibling sweep of the post-fork startup path while verifying the PS-50 fix
- Rust: `src/procserv/supervisor.rs:76` (`config.validate()` inside `ProcServ::new`, called post-fork at `bin/procserv_rs.rs:580`); the gap was `bin/procserv_rs.rs` `entry()` never validating pre-fork, and `bind_endpoints` (`listener.rs:102-111`) returning `Ok(vec![])` on empty control rather than fail-fasting
- C: `procServ.cc:551` `forkAndGo()` runs *after* getopt; the control port is a required positional, so a missing port aborts in the foreground parent during arg parsing (pre-fork), never daemonizing
- Impact: `-P`/`--port` is `Vec<String>` and not `required`, and `build_config` only rejects an empty *command*. Run `procserv-rs <cmd>` in daemon mode with no `-P`/`--unixpath`: `build_config` → `control=[]`, pre-fork `bind_endpoints` → `Ok(vec![])` (binds nothing, no fail-fast), then `fork_and_go` has the foreground parent print the spawn notice, write the pidfile with the *child's* pid, and `exit(0)`. The daemon child then hits `ProcServ::new` → `validate()` → `Err("at least one control endpoint is required")` → `ExitCode::FAILURE` and dies. Because the child's stderr is already `dup2`'d to `/dev/null` (`daemon.rs:129-132`), the error is swallowed: a `Type=forking` systemd unit reads the parent-written pidfile + sees exit 0 → marks the service *active (running)* with no process and no visible error — the same headless-daemon false-success PS-32/PS-48/PS-49/PS-50 exist to prevent. The empty-`program` config (`procserv-rs -P 2000 ""`) is a second, more degenerate trigger of the same stranded gate.
- Resolution: structural close mirrors PS-49 — the validation owner (`ProcServConfig::validate`) is hoisted into the foreground process, **before `fork_and_go`** (`bin/procserv_rs.rs` `entry()`, right after `foreground` is determined and before the pre-fork bind block). A "cannot run" config now `eprintln!`s the diagnostic and `exit(FAILURE)` in the foreground command (reaching the terminal/systemd) and never writes a pidfile or daemonizes, exactly as the PS-49 bind fail-fast does. The gate is uniform for both modes (foreground gets the earlier, cleaner error too). `ProcServ::new`'s own `validate()` stays as the second gate for library/foreground users who construct it directly — same single owner (`validate()`), two call sites, not duplicated logic. `bind_endpoints`' permissive `Ok(vec![])` is left untouched: with `validate()` now guaranteeing non-empty control pre-fork in both paths, an empty control set is unreachable before bind, so a redundant guard there would be defensive code against an impossible input. Test: `no_control_endpoint_is_rejected_by_validate` (procserv-rs app unit) asserts `build_config(["procserv-rs", "/bin/echo"])` yields empty control yet `validate()` errors — proving the no-control config is reachable from CLI args and is rejected by the owner. The `entry()` pre-fork placement itself is closed by inspection (the daemon fork path is not auto-tested, as with PS-32/PS-49).

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
