# epics-bridge-rs Gateway C-Parity Review — 2026-07-01

Codex-style C-parity audit of the **gateway** surface of `epics-bridge-rs`
(`ca_gateway`, `pva_gateway`, `pvalink`) against the upstream C/C++.
This is a **fresh inventory** — the `qsrv` Q-series
(`c-parity-review-qsrv-2026-07-01.md`) is a *different* scope and covers
nothing here. Findings are numbered `GW-N`.

## References

| Rust port | C/C++ upstream |
|-----------|----------------|
| `crates/epics-bridge-rs/src/ca_gateway/*` | `~/codes/epics-modules/ca-gateway/src/` (`gateServer.cc`, `gateVc.cc`, `gatePv.cc`, `gateAs.cc`, `gateAsCa.cc`, `gateResources.cc`, `gateStat.cc`, `gateway.cc`) |
| `crates/epics-bridge-rs/src/pva_gateway/*` | `~/codes/epics-base/modules/pva2pva/p2pApp/` (`server.cpp`, `channel.cpp`, `chancache.cpp`, `moncache.cpp`, `gwmain.cpp`) |
| `crates/epics-bridge-rs/src/pvalink/*` | see caveat below |

### ⚠ pvalink reference caveat — RESOLVED (re-validated vs pvxs 2026-07-01)

The pvalink category was first audited against **pva2pva** `pdbApp/pvalink*.cpp`
(the reference path handed to the auditor), but the Rust port's own comments
cite **pvxs** `ioc/pvalink*.cpp` — its true upstream. pva2pva and pvxs
pvalink diverge in exactly the change-mask, PP-scan, and `atomic`-option
areas that generated GW-80/81/82. Re-validation round
`01KWEB9B0MF4D257X3C8V9RWZP` opened pvxs directly and **overturned most of
the grading**: GW-80/81/82 are **false positives** (the port faithfully
tracks pvxs), GW-83 collapses to a narrow NOTE, and only GW-84 is a
confirmed (NOTE-severity) divergence vs pvxs. Verdicts are recorded inline
in Category 5 below. **Net: zero pvalink defects; nothing to fix except two
low-priority NOTEs.** (Lesson: audit against the reference the port actually
tracks — pva2pva was the wrong upstream here.)

## Methodology

5 read-only opus auditor panels (caucus round `01KWE96SQY6TJGFRDACXAZ130S`),
one per category, each applying the five Codex principles: (1) C call graph
not isolated bodies; (2) negative space / silent failures / missing gates;
(3) wire+behavior parity not "looks similar"; (4) test skepticism (open
every cited line); (5) fresh inventory (qsrv Q-series out of scope). No
source was modified during the audit.

Severity: **DEFECT** (wrong observable behavior), **CONCERN** (real
divergence, often a documented-intentional redesign needing sign-off),
**NOTE** (additive / niche / informational).

---

## Open Findings

### Category 1 — CA-gateway SERVER leg (client-facing)

`ca_gateway/{server,downstream,stats,beacon,master}.rs` vs
`gateServer.cc`, `gateVc.cc` (downstream), `gateStat.cc`, `gateway.cc`.

**GW-1** Upstream Active→Disconnect keeps downstream channels alive — **CONCERN** (documented-intentional; see GW-22)
- Rust: `ca_gateway/upstream.rs:1002-1015`
- C ref: `gatePv.cc:597-610` (`gatePvData::death` deletes the `gateVcData`)
- Impact: C destroys the VC → tears down the CAS channel → connected clients get ECA_DISCONN and re-search. Rust keeps the shadow channel and only posts an INVALID/`LINK_ALARM` last value. A client keying off connection-state (re-search, access-rights reset, provider failover) sees a live channel with a stale-but-alarmed value instead of a disconnect.

**GW-2** `existTestRate` stat measures a materially different quantity than C — **CONCERN**
- Rust: `ca_gateway/server.rs:683` (`record_exist_test()` only after pvlist match **and** successful `ensure_subscribed`)
- C ref: `gateServer.cc:1497` (`++exist_count;` at the top of `pvExistTest`, before ACL/deny/resolution)
- Impact: C counts **every** exist test reaching the gateway (denied, nonexistent, repeat searches of already-served PVs); Rust counts only the first successful resolution of each new name (resolver isn't even invoked for cached PVs). `existTestRate` reads far lower on Rust for identical search traffic.

**GW-3** `.DESC` stat companion PVs are not served — **NOTE**
- Rust: `ca_gateway/stats.rs:304-386` (base names only; no `.DESC` anywhere)
- C ref: `gateServer.cc:2100-2102` builds `<prefix>:<name>.DESC`; `pvExistTest:1604`; `gateStat.cc:428-464` creates `gateStatDesc`
- Impact: C serves a string description PV per stat (e.g. `gateway:vctotal.DESC`); Rust answers does-not-exist, so C-compat screens / `caget <stat>.DESC` lose the description.

**GW-4** Extra beacon anomaly fired on first PV resolution — **NOTE**
- Rust: `ca_gateway/server.rs:669-675` (`beacon_anomaly.request()` after first `ensure_subscribed`)
- C ref: `gatePv.cc:543-546` fires anomaly only on `gatePvDisconnect→gatePvInactive` (reconnect); first-connect path has none
- Impact: Rust emits one (inhibit-throttled) extra beacon per newly-resolved name; C never announces an anomaly on first connect. Benign extra broadcast traffic. Reconnect beacon itself matches C.

**GW-5** Rust advertises stat PVs a default C build does not — **NOTE**
- Rust: `ca_gateway/stats.rs:372-375` (`fd`, `clientEventCount`, `postEventCount`, `loopCount`)
- C ref: `gateServer.cc:1965-1973` (`statFd` gated on non-default `USE_FDS`; other three absent)
- Impact: Additive only — a C-compat client never searches these. Documented Rust-native; noted for namespace completeness.

*Clean (verified):* VC state machine (`cache.rs:41-60` ↔ `pvExistTest`), cleanup timeouts + reconnect inhibit (1s/2h/2min/2h/5min, exact), write accept/deny order, control-flag PVs (all 7 C names + fixed drain order), stat native type/count mapping, per-request pvlist re-check.

### Category 2 — CA-gateway CLIENT leg (upstream) + PV/connection cache

`ca_gateway/{upstream,cache,routing}.rs` vs `gatePv.cc`, `gateVc.cc`
(upstream), `gateway.cc` (connect path).

**GW-20** Cached-mode upstream monitor never torn down when the last downstream client leaves — **CONCERN**
- Rust: `cache.rs:162-167` (`remove_subscriber` does Active→Inactive only); `upstream.rs:1197-1200` (`release_monitor` no-op in cached mode); value/prop tasks run until eviction
- C ref: `gateVc.cc:512-534` (`vcRemove`) → `gatePv.cc:424-461` `deactivate()` unmonitors value + property immediately on last-client-leaves
- Impact: Rust keeps both upstream monitors running until `inactive_timeout` (2h default). Sustains upstream monitor traffic + cache churn for every PV any client ever touched, up to 2h after all clients leave — an unbounded upstream/CPU load C does not impose.

**GW-21** Disconnected-PV eviction keyed on `disconnect_timeout`; C uses `inactiveTimeout()` — **CONCERN**
- Rust: `cache.rs:421-423` (Disconnect state evicts at `timeouts.disconnect_timeout`)
- C ref: `gateServer.cc:1442-1443` uses `inactiveTimeout()`; `disconnectTimeout()` is set/printed but never consulted in `inactiveDeadCleanup`
- Impact: In C the `-disconnect_timeout` option has no eviction effect (dead code). Defaults match (both 2h), but an operator setting `-disconnect_timeout` ≠ `-inactive_timeout` gets non-C eviction timing.

**GW-22** Upstream disconnect keeps downstream channels alive (LINK_ALARM) instead of C's VC-delete → ECA_DISCONN — **CONCERN** (documented-intentional; twin of GW-1)
- Rust: `upstream.rs:1002-1015` (set `Disconnect` + `post_alarm(3, LINK_ALARM)`, channels retained; reconnect Disconnect→Active)
- C ref: `gatePv.cc:597-610` `death()` deletes the VC (clients see ECA_DISCONN, must re-search); reconnect `life()` → Inactive not Active
- Impact: Client-visible connection-state differs. Marked intentional (`upstream.rs:996-1001`); flag for sign-off.

**GW-23** CTRL-metadata seed bounded to 500 ms; on timeout, zeroed metadata is served indefinitely (repairing prop event unconditionally skipped) — **FIXED `70ebcfae`**
- Rust: `upstream.rs:713-744` (500 ms timeout on `get_with_metadata(Ctrl)`; empty on timeout) + `upstream.rs:1081,1112-1115` (prop task skips its first event unconditionally)
- C ref: `gatePv.cc:915-1011` `get(ctrlType)` → `getCB:1689-1696` async + unbounded (always seeds); `propEventCB:1564-1568` ignores first event *because* the seed is reliable (getCB enables propMonitor() only AFTER setPvData, gatePv.cc:1702-1705)
- Impact: For a slow upstream where the initial control get exceeds 500 ms, the shadow PV serves zeroed units/precision/limits/enum-labels to DBR_CTRL/DBR_GR reads and does not self-heal — the repairing first DBE_PROPERTY event is skipped, so metadata stays zeroed until a genuine future property change. C has no such failure mode.
- Fix: capture a real `seed_succeeded` bool from the seed outcome, thread it into `spawn_prop_forward_task` (both the cached and no-cache lazy spawn paths, the latter via a stored `UpstreamSubscription.seed_succeeded`), and start `first_event_seen = !seed_succeeded` so a missed seed consumes the first property event to seed (via `post_pv_property`, which persists metadata) instead of skipping it. Also closes C's own get-hard-failure gap. Regression test `br_gw23_seed_miss_first_prop_event_seeds_metadata`.

**GW-24** `put`/`get` look up `subs` by `upstream_name` but `subs` is keyed by `served_name` (latent; API uncalled) — **NOTE**
- Rust: `upstream.rs:1300`, `:1321` (`get(upstream_name)`) vs insert key `served_name` at `:835`
- Impact: For an aliased PV these fall through to a fresh transient channel per call. No caller exists today (write hook uses its captured channel), so latent; would silently regress alias performance if wired.

**GW-25** Connect-timeout drops the channel entirely; no persistent Dead-PV background reconnect — **NOTE**
- Rust: `upstream.rs:540-556` (on `wait_connected` timeout: remove entry, drop channel, Err); `cache.rs:406-436` demotes Connecting→Dead with a misleading "reuse on reappear" comment
- C ref: `gateServer.cc:1335-1356` `connectCleanup` → `gatePv.cc:620-638`: PV kept as Dead, channel NOT cleared, keeps searching; IOC return revives Dead→Inactive
- Impact: C revives a connect-timed-out name the instant the IOC appears; Rust discards channel+entry, so every later search re-initiates and re-waits `connect_timeout`, and the FSM's documented reuse cannot happen.

**GW-26** No separate archive-mode upstream DBE_LOG subscription (`logMonitor`) — **NOTE**
- Rust: `server.rs:304` folds `-mask l` into the single value monitor; no `logMonitor` equivalent
- C ref: `gatePv.cc:796-837` `logMonitor()` (dedicated `DBE_LOG` subscription) in archive mode for DBE_LOG-only clients
- Impact: Rust has no archive mode, so this client-leg subscription path is absent. Niche (archive off by default).

*Clean (verified):* `routing.rs` env mapping (exact to `gateway.cc:359-402`), native type/count discovery, beacon-anomaly reconnect throttle, default cache timeouts + event mask, first-subscriber Active transition + existence gate.

### Category 3 — CA-gateway ACCESS-SECURITY + pvlist + putlog + admin

`ca_gateway/{access,pvlist,putlog,command,control,report}.rs` vs
`gateAs.cc`, `gateAsCa.cc`, `gateResources.cc`. (All divergences fail in the
**safe / over-deny / over-report** direction — no over-grant found.)

**GW-40** Upstream READ-access not bridged into the downstream read decision — **FIXED `1ed22d29`**
- Rust: `upstream.rs:1447` (`read = cfg.can_read(...)`, no AND with upstream) and `:798-808` (access-rights watcher swaps only `rights.write`; `rights.read` ignored)
- C ref: `gateVc.cc:326` `readAccess() = asclient->readAccess() && vc->readAccess()`; `gatePv.cc:395,1851` set `vc` read-access from `ca_read_access` on connect + every AR callback
- Impact: Write path correctly ANDs local+upstream (`:1453`); read path uses local ACF only. When the upstream revokes read to the gateway's client, C reports read=false downstream; Rust keeps read=true and (cached) serves the last value. Asymmetric with write bridging — over-reports read rights (no fresh data flows, so CONCERN not DEFECT).
- Fix: seed `upstream_read` from the same connect-time `channel.info()` snapshot, keep it live in the AR watcher (now tracks both bits, wakes downstream clients if either flips), and AND it into the read decision — symmetric with the write path. Test `br_gw40_upstream_read_denied_overrides_local_acf_allow`. Single read-decision owner, no sibling sites.

**GW-41** Conditional ASG rules (CALC/INP) entirely non-functional — the whole `gateAsCa` subsystem is unported — **CONCERN**
- Rust: `access.rs:103-109` (`Mode::Rules` with no INP resolver); `access.rs:117,147` call sync `check_access_*`, which in `epics-base-rs/src/server/access_security.rs:741-743` pass `calc_ok = |_| false` (fail-closed)
- C ref: `gateAsCa.cc:139-233` opens CA channels + subscriptions on every ASG `INP` PV, feeds `asComputeAsg`; invoked from `gateAs.cc:716` on init + every `reInitialize`
- Impact: A `RULE(...){CALC("A=1")}` with `INP(A)` that grants dynamically in C evaluates to **deny forever** in Rust. Fail-**closed** (can only deny, never over-grant), so CONCERN — but conditional-access ACFs are silently broken and can deny legitimate operators.

**GW-42** ACF open/read failure aborts startup instead of C's read-only-default fallback — **NOTE**
- Rust: `server.rs:426` `AccessConfig::from_file(path)?` propagates I/O error; reload same at `command.rs:225`
- C ref: `gateAs.cc:646-660` — `fopen` failure sets `use_default_rules` + installs `ASG(DEFAULT){RULE(1,READ)}`, prints, continues
- Impact: Rust refuses to start (safer); C silently degrades to read-only-default. On reload where the ACF became unreadable, Rust keeps prior rules while C swaps to read-only default.

**GW-43** Empty-user (no `CLIENT_NAME`) writes hard-denied even for unconditional WRITE rules — **NOTE**
- Rust: `upstream.rs:1555-1561`, `:1448-1452` deny write when `user.is_empty() && has_rules()`
- C ref: `gateAs.h:160-161` `writeAccess() = asCheckPut(...)` — no empty-user special case; unconditional `RULE(1,WRITE)` grants any client
- Impact: Stricter-than-C hardening against pre-identity WRITE_NOTIFY. Fail-closed, deliberate.

*Clean (verified):* pvlist ALLOW/DENY/ALIAS/ORDER precedence (`match_name` ↔ `findEntry`), DENY FROM host scoping, GNU-BRE dialect + backreference alias expansion, putlog content + trap-mask gating, admin command dispatch (R1/R2/R3/AS, case-sensitive, fixed order), AS-reload eviction + beacon anomaly, R1/R2/R3 report substance.

### Category 4 — PVA gateway (pva2pva p2pApp)

`pva_gateway/{gateway,channel_cache,source,middleware,multi_gateway,control,error}.rs`
vs `server.cpp`, `channel.cpp`, `chancache.cpp`, `moncache.cpp`,
`gwmain.cpp`. The Rust port is an architectural re-implementation
(broadcast fan-out + per-credential identity forwarding), so several
CONCERNs are deliberate redesign departures.

**GW-60** Monitor overflow drops the oldest of a fixed-16 window and does not coalesce, where C squashes overflow into the tail (latest) — **CONFIRMED REAL vs pvxs** (re-validation round `01KWEDDFX0PW559DGSFRPBQVVH`)
- Rust: `pva_gateway/channel_cache.rs:48` (`BROADCAST_CAPACITY=16` FIFO ring) + `source.rs:965-968,1068-1071` (on `broadcast::Lagged` resume at oldest retained frame, mark `pending_overrun`)
- C ref (PRIMARY, pvxs — the reference the port tracks): `pvxs/src/servermon.cpp:273-297` `ServerMonitorControl::doPost` — on a full queue the default `post()` (`maybe=false`, source.h:98-99) takes the SQUASH branch `queue.back().assign(val)` (`:285`), overwriting the tail with the newest value and bumping `nSquash`; the newest is NEVER dropped. Only `tryPost()` (`maybe=true`) drops-newest; `forcePost` grows past `limit`. Queue depth honors the client-negotiated `record._options.queueSize` (`:533-544`, min 2). Secondary ref (pva2pva): `moncache.cpp:157-197` `overflowElement` — same coalesce-to-latest discipline.
- Impact: A slow downstream client on the Rust gateway gets the oldest-first trailing window of a fixed 16-frame ring and, under sustained overflow, may never converge to the true latest value; pvxs always lands the latest in the queue tail. REAL wrong-data divergence for latest-value control monitors.
- ⚠ RE-VALIDATED (was flagged as a possible pva2pva-reference false positive like GW-80/81/82). Verdict is REAL: pvxs `doPost` squash confirms the SAME coalesce-to-latest behavior as the cited pva2pva, so the reference did not mislead. Two wording refinements a fix must absorb: (1) pvxs emits NO wire overrun mask — `servermon.cpp:174-176` is a TODO that always serializes an empty BitSet, `nSquash` is server-side stats only; so the Rust gateway's overrun-marking is an *addition* over pvxs (safe over-signaling), a mirror only of pva2pva — do not call it pvxs parity. (2) pvxs keeps a FIFO front backlog and coalesces ONLY the overflow tail, so it too delivers older values first; the guarantee is "newest never dropped, staleness bounded by `queueSize`," not "pure latest."
- Related: GW-64 (fixed-16 ring vs client-negotiated `queueSize`) is now also confirmed vs pvxs (`servermon.cpp:533-544`), not just pva2pva.

**GW-61** Per-credential cache split breaks chancache's single-shared-upstream-monitor model — **CONCERN**
- Rust: `source.rs:584-622` (`upstream_cache_for` — every distinct `(account,method,host,authority)` gets its own `ChannelCache` → own upstream channel+monitor per PV; only anonymous share `self.cache`)
- C ref: `chancache.h:151-173`, `server.cpp:62-92` — one `ChannelCache` per client provider; all downstream `GWChannel`s for a PV share one entry/one upstream monitor
- Impact: The N-downstream ⇒ 1-upstream invariant no longer holds for credentialed deployments — upstream connection/monitor count scales with distinct downstream identities. Deliberate p4p.gw-style identity forwarding.

**GW-62** Upstream disconnect fans out only to active monitors, not to downstream channel state; entry retained + auto-reconnected — **CONCERN**
- Rust: `channel_cache.rs:470-485` (`signal_disconnect_boundary` → MONITOR FINISH on fan-out streams only) + `:1347-1363` (entry kept, loop reconnects)
- C ref: `chancache.cpp:63-99` — `channelStateChange(DISCONNECTED)` erases the entry (`:78-83`) and fans DISCONNECTED to every interested `GWChannel` (`:90-98`) so the downstream channel goes DISCONNECTED
- Impact: A downstream client observing connection-state sees no disconnect during an upstream outage; only in-flight monitors get FINISH; non-monitor channels never signalled. Transparent-reconnect redesign.

**GW-63** Clean upstream `unlisten` (MONITOR FINISH) treated as transient + re-subscribed; C treats it as terminal — **CONCERN**
- Rust: `channel_cache.rs:1346-1393` (`handle.wait()` Ok = clean FINISH runs the same boundary-emit + backoff-resubscribe as an error; never stops)
- C ref: `moncache.cpp:212-236` — `unlisten()` sets `startresult = ERROR("upstream unlisten()")` so all future `start()` fail; forwards unlisten, no reconnect
- Impact: A genuinely-final upstream monitor is re-opened indefinitely (INIT→FINISH→resubscribe, backoff-bounded) instead of staying finished; downstream re-INITs repeatedly.

**GW-64** Downstream-requested `record._options.queueSize` ignored for buffer depth — **NOTE**
- Rust: fixed `channel_cache.rs:48` (16) + `source.rs:300` (`subscriber_queue` default 64); pvRequest used only as dedup key
- C ref: `moncache.cpp:36` (`getS(pvr,"record._options.queueSize",2)`), `:292-300`
- Impact: A client asking for queueSize=1 (latest-only) or a large queue gets neither; C default depth (2) differs from Rust (16/64), changing overflow timing.

**GW-65** SEARCH/`has_pv` blocks up to `connect_timeout` on upstream connect; C answers `channelFind` immediately — **NOTE**
- Rust: `source.rs:1150-1172` + `channel_cache.rs:1072-1113` (awaits `pvconnect` up to `connect_timeout`, default 5 s)
- C ref: `server.cpp:36-56` + `chancache.cpp:166-208` — returns an entry only if already `isConnected()`; a cold PV returns not-found immediately, search thread never waits
- Impact: Rust defers a cold-PV search reply by up to `connect_timeout` instead of replying not-found + relying on client re-search. Different search dynamics; one open task per in-flight cold search.

**GW-66** `is_writable`/access-rights proxied as upstream *connection* state, not upstream access rights — **NOTE**
- Rust: `source.rs:1417-1442` (`is_writable` = "can we connect upstream")
- C ref: `channel.cpp:92-96` — `GWChannel::getAccessRights` returns the upstream's actual rights
- Impact: A connectable-but-read-only upstream is reported writable. Partly protocol-forced (PVA transmits no per-field AR message this client exposes).

**GW-67** Idle eviction keyed on MONITOR interest only, not all open downstream channels — **NOTE**
- Rust: `channel_cache.rs:562-564` (`is_retained = drop_poke || subscriber_count>0`, subscribers = monitor receivers)
- C ref: `chancache.cpp:121` — `cacheClean` evicts on `!dropPoke && interested.empty()`, `interested` = all open `GWChannel`s
- Impact: A connection-only channel ages out after ~2 cleaner ticks even while a downstream channel stays open, so a later monitor re-searches. Documented deliberate narrowing; low cost.

*Clean (verified):* pvRequest passthrough + monitor dedup, `p2pReadOnly` gating (stricter than C), op forwarding (GET/PUT/PUT_GET/RPC/PROCESS/ChannelArray/GET_FIELD), type-change handling (superset), cleaner two-tick reaping + counters, `list_pvs` empty (matches C no `channelList`), operator `:drop`/`:flush`/status (superset).

### Category 5 — pvalink (⚠ first graded vs pva2pva; RE-VALIDATED vs pvxs 2026-07-01)

`pvalink/{link,config,integration,registry,iocsh}.rs`. The port's true
upstream is **pvxs** `ioc/pvalink*.cpp`, not pva2pva. Round
`01KWEB9B0MF4D257X3C8V9RWZP` re-checked GW-80..GW-84 against pvxs directly
(citations verified by hand): **GW-80/81/82 are FALSE POSITIVES** (the port
matches pvxs, which — unlike pva2pva — has no change-mask suppression, no
PP-scan target, and does parse `atomic`); **GW-83 downgrades** (its alarm-on-
disconnect claim is wrong vs pvxs; only a `retry`-forward-link residual
remains, NOTE); **GW-84 is CONFIRMED vs pvxs** (NOTE). Net: **zero pvalink
defects.** Each verdict is inlined below.

**GW-80** ~~CP/CPP scan has no per-field change suppression~~ — **FALSE POSITIVE (RE-VALIDATED vs pvxs)**
- Rust: `integration.rs:973-985` (`run_notify_forwarder` scans on every `ScanEvent`) + `link.rs:354-367` (monitor callback caches whole value, no changed-bitset)
- pvxs evidence: `pvalink_channel.cpp:312-325` (`ScanTrack::scan()` has only the passive + PACT gates, then `dbProcess(prec)` — NO changed-bitset test), `:422-432` (scan loop runs `trac.scan()` for every record on every `run()`/monitor pop), `pvalink_link.cpp:83-120` (`onTypeChange` builds no `proc_changed` mask). The entire pva2pva change-mask machinery does not exist in pvxs.
- Verdict: The change-mask suppression is a **pva2pva-only** behavior. pvxs processes on every monitor pop, exactly like the Rust forwarder. **Rust matches pvxs — drop.** (Hand-verified: pvalink_channel.cpp:312-325.)

**GW-81** ~~`PP` input link does not register a passive-gated monitor scan target~~ — **FALSE POSITIVE (RE-VALIDATED vs pvxs)**
- Rust: `config.rs:129-135` (`ProcMode::inp_scan()` returns `(false,false)` for `Pp`); `:283-305` derives scan flags from it, so `PP` never sets `scan_on_update`
- pvxs evidence: `pvalink_link.cpp:122-133` (`scanOnUpdate()`: `CP → Yes`, `CPP → Passive`, **everything else incl. `PP` → No`). PP INP links register no scan target in pvxs.
- Verdict: Rust `Pp → (false,false)` matches pvxs `PP → scanOnUpdateNo`. The `config.rs:286-292` comment citing pva2pva is stale/misleading, but the **code is correct vs pvxs — drop.** (Hand-verified: pvalink_link.cpp:122-133.)

**GW-82** ~~`atomic` accepted as a per-link option pva2pva never performs~~ — **FALSE POSITIVE (RE-VALIDATED vs pvxs)**
- Rust: `config.rs:400,532` (parse `atomic`) + `integration.rs:1075-1092` (atomic targets scanned under one `lock_records` epoch)
- pvxs evidence: `pvalink_jlif.cpp:112-113` (`else if(pvt->jkey=="atomic") pvt->atomic = !!val;`) parses it, `:256` reports it, `pvalink_channel.cpp:398-404` builds `atomicrecs` from `pvaLink::atomic` and `:422-427` holds `DBManyLocker` across the atomic group.
- Verdict: pvxs **does** parse `atomic` and drive atomic-group scanning; the "pva2pva drops it" grading was against the wrong reference. Rust matches pvxs — **drop.**

**GW-83** `scanForward` on a disconnected `retry` forward link errors instead of queuing — **NOTE (RE-VALIDATED vs pvxs, downgraded)**
- Rust: `link.rs:990-998` (`scan_forward` returns `Err(Disconnected)` when `!is_connected()`, gates on connection alone regardless of `retry`) via `integration.rs:1285-1314`
- pvxs evidence: `pvalink_lset.cpp:677-680` (`pvaScanForward` raises `recGblSetSevrMsg(LINK_ALARM, INVALID_ALARM, "Disconn")` on a disconnected non-retry forward link — so Rust's `Err(Disconnected)→alarm` **matches pvxs** for the common case); pvxs gates on `!retry && !valid()`, so a `retry` forward link proceeds to `put(true)` (queues) even while disconnected.
- Verdict: The graded "C is silent on disconnect" claim was against pva2pva and is **wrong vs pvxs** (pvxs alarms too). Residual real divergence: a `retry=true` pva `FLNK` proceeds/queues in pvxs but Rust rejects it. Obscure config → **NOTE.**

**GW-84** Connected read of a value lacking a selected child fails (→INVALID) instead of keeping prior VAL — **NOTE (CONFIRMED vs pvxs)**
- Rust: `link.rs:1788-1801` (`select_link_value` → `PvField::Null` when the child is absent) → resolver returns `None` → base raises LINK/INVALID
- pvxs evidence: `pvalink_lset.cpp:281-286`,`:439` (connected but `!value`: copies nothing, memsets 0 only when `pnRequest==NULL`, falls through to `return 0` — success, record keeps prior value, no LINK_ALARM)
- Verdict: Real divergence vs pvxs, reachable with a metadata-substruct selector (`field=timeStamp`, `field=alarm`). Confidence hinges on `pvfield_to_epics_value(Null)→None`. Rare NT shape → **NOTE, keep.**

*Clean (verified vs pva2pva):* jlif option parsing (proc/sevr enums, bool shorthands, clamps, KIND dispatch), `makeRequest` monitor pvRequest, OUT `put()` combine-process resolution, disconnect value-serving gate, `dbpvar` levels + glob match, sub-field selection.

---

## Review Log — round `01KWE96SQY6TJGFRDACXAZ130S` (2026-07-01)

Findings as first graded: 1 DEFECT / 12 CONCERN / 11 NOTE across 5
categories (GW-1..5, GW-20..26, GW-40..43, GW-60..67, GW-80..84).

**After the pvxs re-validation round `01KWEB9B0MF4D257X3C8V9RWZP`: 0 DEFECT.**
GW-80 (the only DEFECT) plus GW-81/GW-82 are false positives — the pvalink
category was first graded against pva2pva but the port tracks pvxs, and pvxs
lacks the change-mask suppression / PP-scan / `atomic`-drop behaviors that
generated them. GW-40 (`1ed22d29`) and GW-23 (`70ebcfae`) are now **FIXED**.
Remaining actionable: GW-60 (wrong-data, CONFIRMED REAL vs pvxs
`servermon.cpp:273-297` — structural PVA-gateway fan-out change, subsumes
GW-64), GW-41 (subsystem scope). The rest are redesign sign-off decisions
or NOTEs. GW-40 review round `01KWEDDFX...` PASSED all four questions (no
defect); GW-23 review re-run pending.

Thematic clusters:

1. **"Keep-warm" upstream retention (CA-gw).** GW-1/GW-22 (keep downstream
   channels alive on upstream disconnect) and GW-20 (keep upstream monitors
   after last client leaves) are the same design decision: the Rust CA
   gateway favors transparent-reconnect over C's tear-down-and-re-search.
   Client-visible connection-state and upstream load both differ. Needs a
   single sign-off on whether C parity or the keep-warm model is wanted.

2. **Transparent-reconnect redesign (PVA-gw).** GW-62/GW-63 mirror the same
   philosophy on the PVA side (retain + auto-reconnect vs C erase +
   propagate DISCONNECTED / terminal unlisten). GW-61 (per-credential cache
   split) is a deliberate identity-forwarding departure from chancache's
   fan-in. These are architectural, not bugs — but they break C invariants
   a strict-parity consumer may rely on.

3. **Access-security fidelity gaps (CA-gw), all fail-closed.** GW-40
   (upstream read-access not bridged) and GW-41 (`gateAsCa` conditional
   CALC/INP rules unported → deny-forever) are the substantive security
   divergences; both over-deny / over-report, never over-grant. GW-41 is
   the larger gap (a whole subsystem) and can silently deny legitimate
   operators using conditional ACFs.

4. **Metadata/latest-value freshness.** GW-23 (CTRL metadata zeroed forever
   after a 500 ms seed timeout) and GW-60 (monitor overflow delivers stale
   trailing values, not the coalesced latest) are the two findings where a
   client can observe *wrong data*, not just different connection dynamics.
   GW-23 in particular has no C analogue (self-heals in C).

5. **pvalink reference mismatch — RESOLVED.** The port tracks pvxs; the
   audit first used pva2pva. Re-validation vs pvxs cleared GW-80/81/82 as
   false positives (the three areas where pva2pva and pvxs diverge),
   downgraded GW-83, and confirmed only GW-84 (NOTE). Zero pvalink defects.
   Lesson: grade against the reference the port actually tracks.

### Next steps (fix phase — separate commits, after this doc lands)

- **GW-40** (bridge upstream read-access) — **DONE `1ed22d29`.**
- **GW-80..GW-84 re-validated vs pvxs** — no fix owed (GW-80/81/82 false
  positive; GW-83/GW-84 are low-priority NOTEs, left as documented).
- **GW-23** (metadata zeroed forever) — **DONE `70ebcfae`.**
- **GW-60** (stale overflow value) — **CONFIRMED REAL vs pvxs** (`servermon.cpp:273-297`
  squash-to-tail). Fix = replace the fixed-16 drop-oldest broadcast ring with a
  coalesce-to-latest per-subscriber queue sized to the client-negotiated
  `queueSize` (subsumes GW-64). This is a structural change to the PVA gateway
  monitor fan-out — scope/sign-off before starting.
- **GW-41** (`gateAsCa`) is a subsystem-sized port — confirm scope with the
  user before starting.
- **GW-1/GW-20/GW-22/GW-61/GW-62/GW-63** are redesign sign-off decisions,
  not defects — surface as a group.
