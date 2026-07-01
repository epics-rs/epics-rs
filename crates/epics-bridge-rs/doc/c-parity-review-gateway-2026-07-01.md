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

### ⚠ pvalink reference caveat (blocks GW-80..GW-84 disposition)

The pvalink category was audited against **pva2pva** `pdbApp/pvalink*.cpp`
(the reference path handed to the auditor), but the Rust port's own
comments cite **pvxs** `ioc/pvalink*.cpp`. pva2pva and pvxs pvalink differ
in several semantics, and the port appears to track pvxs where they
diverge. **GW-80..GW-84 must be re-validated against pvxs
(`~/codes/epics-modules/pvxs`, or the canonical pvxs checkout) before any
are treated as real divergences or fixed.** Treating them as pva2pva
divergences risks "fixing" the port toward the wrong reference.

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

**GW-23** CTRL-metadata seed bounded to 500 ms; on timeout, zeroed metadata is served indefinitely (repairing prop event unconditionally skipped) — **CONCERN**
- Rust: `upstream.rs:713-744` (500 ms timeout on `get_with_metadata(Ctrl)`; empty on timeout) + `upstream.rs:1081,1112-1115` (prop task skips its first event unconditionally)
- C ref: `gatePv.cc:915-1011` `get(ctrlType)` → `getCB:1689-1696` async + unbounded (always seeds); `propEventCB:1564-1568` ignores first event *because* the seed is reliable
- Impact: For a slow upstream where the initial control get exceeds 500 ms, the shadow PV serves zeroed units/precision/limits/enum-labels to DBR_CTRL/DBR_GR reads and does not self-heal — the repairing first DBE_PROPERTY event is skipped, so metadata stays zeroed until a genuine future property change. C has no such failure mode.

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

**GW-40** Upstream READ-access not bridged into the downstream read decision — **CONCERN**
- Rust: `upstream.rs:1447` (`read = cfg.can_read(...)`, no AND with upstream) and `:798-808` (access-rights watcher swaps only `rights.write`; `rights.read` ignored)
- C ref: `gateVc.cc:326` `readAccess() = asclient->readAccess() && vc->readAccess()`; `gatePv.cc:395,1851` set `vc` read-access from `ca_read_access` on connect + every AR callback
- Impact: Write path correctly ANDs local+upstream (`:1453`); read path uses local ACF only. When the upstream revokes read to the gateway's client, C reports read=false downstream; Rust keeps read=true and (cached) serves the last value. Asymmetric with write bridging — over-reports read rights (no fresh data flows, so CONCERN not DEFECT).

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

**GW-60** Monitor overflow delivers a lagging trailing window, not C's latest-value-coalesced element — **CONCERN**
- Rust: `channel_cache.rs:48` (`BROADCAST_CAPACITY=16` FIFO ring) + `source.rs:1068,965` (on `broadcast::Lagged` resume at oldest retained frame, mark `overrun`)
- C ref: `moncache.cpp:157-197,354-368` — on overflow each `MonitorUser` coalesces into a single `overflowElement` (latest value), `release()` returns that with the overrun bitset OR'd
- Impact: A slow downstream client receives delayed/stale values (oldest-first) where pva2pva coalesces to newest. Observable semantic difference for latest-value control monitors; overrun flag set but delivered value not current until upstream rate falls.

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

### Category 5 — pvalink (⚠ audited vs pva2pva; port tracks pvxs — RE-VALIDATE)

`pvalink/{link,config,integration,registry,iocsh}.rs` vs pva2pva
`pdbApp/pvalink*.cpp`. **See the pvalink reference caveat above — none of
GW-80..GW-84 is dispositioned until re-checked against pvxs.**

**GW-80** CP/CPP scan has no per-field change suppression (processes on every event) — **DEFECT (pending pvxs re-validation)**
- Rust: `integration.rs:973-985` (`run_notify_forwarder` scans on every `ScanEvent`, payload unused) + `link.rs:354-367` (monitor callback caches whole value, no changed-bitset)
- C ref (pva2pva): `pvalink_channel.cpp:304` (`if(connected_latched && !op_mon.changed.logical_and(scan_changed[idx])) return;`), mask built `pvalink_link.cpp:132-153`
- Impact: pva2pva reprocesses a CP/CPP record only when the monitor's changed-bitset intersects the link's selected-field mask; the Rust forwarder has no changed-bitset and processes on every update. A `CP` link on `field=alarm.severity` fires the record (and FLNK/output cascade) on every value change. **Must confirm pvxs pvalink does field-mask suppression before treating as a defect.**

**GW-81** `PP` input link does not register a passive-gated monitor scan target — **CONCERN (pending pvxs re-validation)**
- Rust: `config.rs:129-135` (`ProcMode::inp_scan()` returns `(false,false)` for `Pp`); `:283-305` derives scan flags from it, so `PP` never sets `scan_on_update`
- C ref (pva2pva): `pvalink_channel.cpp:394-399` (scan-list build includes `pp==PP`, `scan_check_passive = (pp != CP)`)
- Impact: In pva2pva `INP=@pva://SRC PP` on a Passive record reprocesses on SRC updates; in Rust `PP` registers no scan target (PUT-only), so the record never processes on updates. The `config.rs:286-292` comment asserts PP "must register a passive-only scan target" but the code does not.

**GW-82** `atomic` accepted as a per-link option, driving scan-group locking pva2pva never performs — **NOTE (pending pvxs re-validation)**
- Rust: `config.rs:400,532` (parse `atomic`) + `integration.rs:1075-1092` (atomic targets scanned under one `lock_records` epoch)
- C ref (pva2pva): `pvalink.h:98` has no `atomic` member (`pva_parse_bool` ignores unknown keys); `isatomic` init false and never assigned true (dead `DBManyLocker` branch)
- Impact: pva2pva silently ignores `{pva:{atomic:...}}` and always scans per-record; Rust honors `atomic:true` (a pvxs behavior). Default matches pva2pva; the parser accepting an option pva2pva drops is a surface divergence.

**GW-83** `scanForward` errors on disconnect + ignores `retry`; pva2pva silent + honors `retry` — **NOTE (pending pvxs re-validation)**
- Rust: `link.rs:990-998` (`scan_forward` returns `Err(Disconnected)` when `!is_connected()`, no `retry`) via `integration.rs:1285-1314`
- C ref (pva2pva): `pvalink_lset.cpp:479-493` (gate `if(!self->retry && !self->valid()) return;` — void, no alarm; a `retry` FWD link proceeds to `put(true)` while disconnected)
- Impact: A disconnected non-retry pva FWD link is a silent no-op in pva2pva; Rust returns an error. A `retry` FWD link is queued in pva2pva but rejected in Rust. Obscure config.

**GW-84** Connected read of an NT value lacking a `value` child fails (→INVALID) instead of leaving buffer untouched — **NOTE (pending pvxs re-validation)**
- Rust: `link.rs:1788-1801` (`select_link_value` → `PvField::Null` when no `value` child) → base raises LINK/INVALID
- C ref (pva2pva): `pvalink_lset.cpp:218-224` (`if(self->fld_value){...}` — null fld_value copies nothing, still `return 0`, keeps prior VAL, no alarm)
- Impact: For a connected structure with no `value` field, pva2pva reads succeed (value untouched); Rust drives the record to INVALID. Rare NT shape.

*Clean (verified vs pva2pva):* jlif option parsing (proc/sevr enums, bool shorthands, clamps, KIND dispatch), `makeRequest` monitor pvRequest, OUT `put()` combine-process resolution, disconnect value-serving gate, `dbpvar` levels + glob match, sub-field selection.

---

## Review Log — round `01KWE96SQY6TJGFRDACXAZ130S` (2026-07-01)

Findings: **1 DEFECT** (GW-80, pending pvxs re-validation), **12 CONCERN**,
**11 NOTE** across 5 categories (GW-1..5, GW-20..26, GW-40..43, GW-60..67,
GW-80..84).

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

5. **pvalink reference mismatch (BLOCKER for GW-80..84).** The port tracks
   pvxs; the audit used pva2pva. GW-80 (CP/CPP over-processing) would be a
   real DEFECT *if* pvxs also does field-mask suppression — must be
   re-validated against pvxs before any fix.

### Next steps (fix phase — separate commits, after this doc lands)

- **Re-validate GW-80..GW-84 against pvxs** before touching pvalink.
- **GW-23** (metadata zeroed forever) and **GW-60** (stale overflow value)
  are the strongest wrong-data candidates for fixes.
- **GW-40** (bridge upstream read-access) is a small, safe correctness fix.
- **GW-41** (`gateAsCa`) is a subsystem-sized port — confirm scope with the
  user before starting.
- **GW-1/GW-20/GW-22/GW-61/GW-62/GW-63** are redesign sign-off decisions,
  not defects — surface as a group.
