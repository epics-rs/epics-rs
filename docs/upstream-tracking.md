# Upstream tracking — epics-base & asyn

**Status**: Snapshot 2026-05-10. Sources: `gh pr/issue list` against `epics-base/epics-base@7.0`, `epics-modules/asyn@master`. Window: PRs merged ≥ 2025-05-10 + all open PRs/issues at snapshot time.

**Purpose**: Each upstream item is mapped to an epics-rs location to inspect. Status column tracks whether epics-rs currently reflects the change. This file is the single source of truth for "what is upstream that we have not yet looked at".

**Refresh procedure**: re-run the `gh` queries at the bottom of this file; diff against the lists below; update the status column.

---

## How to read the status column

- `not started` — upstream change identified, epics-rs side not yet inspected.
- `inspected, equivalent` — read epics-rs code, behavior matches upstream intent.
- `inspected, missing` — epics-rs lacks the behavior; ticket needed.
- `inspected, n/a` — upstream change does not apply (C-only build/platform/toolchain).
- `done <commit>` — implemented in epics-rs; cite commit.

---

## A. epics-base merged PRs (last 12 months) to reflect

Shape: PR# / merged date / what changed / epics-rs location to inspect / status / notes.

| PR | Merged | Change | Where to look in epics-rs | Status |
|---|---|---|---|---|
| [#856](https://github.com/epics-base/epics-base/pull/856) | 2026-04 | `dbCa: iocInit wait for all conditions` | `crates/epics-base-rs/src/server/database/mod.rs::wait_for_external_links` + `ioc_app.rs` Phase 2b.5 | done — `LinkSet::is_connected` polled per registered scheme/name with 100 ms cadence, `EPICS_RS_INIT_LINK_TIMEOUT` (default 10 s) cap. Logs `connected/total external links connected` summary. Tests: `wait_for_external_links_returns_zero_zero_when_no_lsets`, `_connected_quickly`, `_returns_partial_on_timeout` |
| [#768](https://github.com/epics-base/epics-base/pull/768) | 2026-02 | `dbCa: iocInit wait for local CA links to connect` | same as #856 | done — same mechanism |
| [#817](https://github.com/epics-base/epics-base/pull/817) | 2026-04 | `bi` AFTC flag + `mbbi` AFTC bug fix | `crates/epics-base-rs/src/server/records/{bi,mbbi}.rs` + `record_instance.rs::aftc_filter` | done — alarm pipeline already drives STATE/COS via `evaluate_alarms`; added AFTC/AFVL fields to bi/mbbi and a `aftc_filter` low-pass filter (1 - 1/e threshold) gated on `rtype == "bi" \|\| rtype == "mbbi"`; tests `disabled_when_aftc_le_zero`, `initial_sample_seeds_state_unchanged_alarm`, `raises_alarm_only_after_full_time_constant`, `dt_zero_is_no_op`, `long_steady_state_converges_to_alarm` |
| [#812](https://github.com/epics-base/epics-base/pull/812) | 2026-03 | iocsh `dbCreateRecord` (runtime record creation) | `iocsh/commands.rs::cmd_db_create_record` — validates name (PR #78 rules), refuses duplicates, routes through `db_loader::create_record`. Tests: 5 cases under `test_execute_line_db_create_record_*` | done |
| [#831](https://github.com/epics-base/epics-base/pull/831) | 2026-03 | caRepeater run-time debug switch | `crates/epics-ca-rs/src/repeater/` | not started |
| [#788](https://github.com/epics-base/epics-base/pull/788) | 2026-02 | Avoid overreporting available CPUs (cgroup/affinity) | tokio runtime worker thread sizing in `epics-rs` runtime entry | not started |
| [#742](https://github.com/epics-base/epics-base/pull/742) | 2025-11 | `aao`, `aai` VAL field type `DOUBLE[]` → `FTVL[]` | aao/aai record VAL polymorphism in `crates/epics-base-rs/src/server/records/` | not started |
| [#732](https://github.com/epics-base/epics-base/pull/732) | 2025-11 | `normativeTypes` NTNDArray::getValueSize fix | `crates/epics-pva-rs/src/nt/nd_array.rs` | inspected, n/a (no equivalent helper; negation pattern audit clean) |
| [#711](https://github.com/epics-base/epics-base/pull/711) | 2025-10 | Allow CA clients to determine server protocol version | `crates/epics-ca-rs/src/client/` server-version exposure | not started |
| [#708](https://github.com/epics-base/epics-base/pull/708) | 2025-10 | Expand dbEvent synchronization | dbEvent path in `crates/epics-base-rs/src/server/db/event*` | not started |
| [#689](https://github.com/epics-base/epics-base/pull/689) | 2025-10 | Better guesses for wrong field names | dbAccess parser error path | not started |
| [#688](https://github.com/epics-base/epics-base/pull/688) | 2025-08 | dfanout record improvements (16 OUT* + IVOA/IVOV) | 16 outputs already in `records/dfanout.rs`; IVOA/IVOV invalid-output handling now wired in `database/links.rs::dispatch_multi_output` (SEVR=INVALID + IVOA selects continue / suppress / use IVOV) | done |
| [#678](https://github.com/epics-base/epics-base/pull/678) | 2025-11 | hex/octal strings in dbPut/dbGet | dbPut/dbGet string-to-numeric parser | not started |
| [#655](https://github.com/epics-base/epics-base/pull/655) | 2025-08 | Extend `calc` inputs A–L → A–U | `crates/epics-base-rs/src/server/records/calc.rs` and `calcout.rs` | done — engine `CALC_NARGS = 21`, lexer A..U / AA..UU, `CalcRecord` + `CalcoutRecord` carry INPM..INPU + M..U + LM..LU; tests `test_extended_inputs_m_through_u`, `test_full_a_to_u_sum`, `test_double_letter_uu_parses` |
| [#636](https://github.com/epics-base/epics-base/pull/636) | 2025-06 | `EPICS_DB_INCLUDE_PATH` for dbLoadTemplate | dbLoadTemplate in epics-base-rs | not started |
| [#626](https://github.com/epics-base/epics-base/pull/626) | 2025-06 | `dbgrep` → `dbglob` (alias kept) | `iocsh/commands.rs::cmd_dbglob` + `cmd_dbgrep` aliases of `dbsr`, share `dbsr_handler`. PR #613's `[fields]` argument also added | done |
| [#608](https://github.com/epics-base/epics-base/pull/608) | 2025-10 | Warn to stderr when discarding CPP modifier for outlink | dblink/outlink parser | not started |
| [#558](https://github.com/epics-base/epics-base/pull/558) | 2025-06 | iocsh `afterIocRunning` (≠ atInit/afterInit) | `PvDatabase::queue_after_ioc_running` + `take_after_ioc_running` + iocsh `cmd_after_ioc_running` push lines into queue. `IocApplication::run` Phase 2e drains them on a fresh `IocShell` after PINI/device-support/autosave | done |
| [#359](https://github.com/epics-base/epics-base/pull/359) | 2026-02 | Fix undefined timestamp for NORD field | aao/aai/waveform NORD timestamp path | not started |
| [#677](https://github.com/epics-base/epics-base/pull/677) | 2025-07 | gethostbyname → getaddrinfo | tokio `lookup_host` already uses getaddrinfo | inspected, equivalent |
| [#587](https://github.com/epics-base/epics-base/pull/587) | 2025-06 | fdManager poll() (CAS) | tokio epoll/kqueue covers this | inspected, n/a |
| [#698](https://github.com/epics-base/epics-base/pull/698) | 2025-08 | xxxRecord → typed dset | Rust generics make this natural | inspected, n/a |
| [#745](https://github.com/epics-base/epics-base/pull/745) | 2025-12 | `epicsThreadCreateOpt` race | tokio thread model — n/a | inspected, n/a |
| #648, #652, #649, #639, #763, #862-RTEMS, #707, #715, #714, #712, #703, #682, #674, #644, #635, #634, #631, #628, #610, #603, #552, #478, #444, #631, #828, #631 | various | RTEMS / Windows / build / docs / test infra | C/build only | inspected, n/a |

## B. epics-base open PRs (track-only)

| PR | Theme | Where epics-rs sits today | Action |
|---|---|---|---|
| [#641](https://github.com/epics-base/epics-base/pull/641) | `7.0 secure pvaccess` (TLS) | `crates/epics-pva-rs` has rustls TLS + pvxs cert interop | **re-validate when upstream merges**; spec is moving |
| [#862](https://github.com/epics-base/epics-base/pull/862), [#863](https://github.com/epics-base/epics-base/issues/863) | DNS TTL refresh of HAG / CA hostname state | `/reload-acf` introspection exists; no automatic TTL refresh | not started |
| [#803](https://github.com/epics-base/epics-base/pull/803) | TOUT common record field | not in base either; track | watch |
| [#796](https://github.com/epics-base/epics-base/pull/796) | msi post-expansion calc syntax | msi-equivalent in epics-rs | not started |
| [#776](https://github.com/epics-base/epics-base/pull/776) | Soft Time Part device support | timestamp dtyp split | not started |
| [#775](https://github.com/epics-base/epics-base/pull/775) | bi/bo conversion improvements | `records/{bi,bo}.rs` | not started |
| [#673](https://github.com/epics-base/epics-base/pull/673) | iocsh readline ctrl+C close stdin | iocsh signal handling | not started |
| [#671](https://github.com/epics-base/epics-base/pull/671) | atExit on SIGTERM/SIGINT | epics-rs shutdown signal handling | not started |
| [#629](https://github.com/epics-base/epics-base/pull/629) | caget INT→SHORT in dbr | CA wire DBR mapping | not started |
| [#621](https://github.com/epics-base/epics-base/pull/621) | nameserver: force CA protocol version | `EPICS_CA_NAME_SERVERS` handshake version selection | not started |
| [#507](https://github.com/epics-base/epics-base/pull/507) | iocsh local variables | epics-rs iocsh already supports `epicsEnvSet` + `$(VAR=DEFAULT)` substitution via `registry::substitute_env_vars`. Process-wide env vars are functionally equivalent for single-instance iocsh sessions; sub-shell scoping is the only behavioural delta and is not needed for current use cases | inspected, equivalent (different mechanism) |
| [#503](https://github.com/epics-base/epics-base/pull/503) | caget tolerates partial disconnect | `caget` tool behavior | not started |
| [#497](https://github.com/epics-base/epics-base/pull/497) | iocsh `pushd`/`popd`/`dirs` | `iocsh/commands.rs::{cmd_pushd, cmd_popd, cmd_dirs}` with process-global `dir_stack` (OnceLock<Mutex<Vec<PathBuf>>>); failed `cd` restores popped entry | done |
| [#475](https://github.com/epics-base/epics-base/pull/475) | asLib type-safety incompat | ACF library | not started |
| [#459](https://github.com/epics-base/epics-base/pull/459) | iocsh history/file size limit | `iocsh/mod.rs::run_repl` builds `rustyline::Config` with `max_history_size` from `EPICS_RS_IOCSH_HISTORY_SIZE` (default 500, floor 16) | done |
| [#205](https://github.com/epics-base/epics-base/pull/205) | IPv6 part 1 | epics-ca-rs ADDR_LIST/BEACON multi-NIC is IPv4-only | not started |
| [#154](https://github.com/epics-base/epics-base/pull/154) | DBR_VFIELD virtual field | CA DBR types | not started |
| [#149](https://github.com/epics-base/epics-base/pull/149) | Address modifiers | dblink modifiers | not started |
| [#69](https://github.com/epics-base/epics-base/pull/69) | Specified TCP port + UDP 5064 fixed | server port env vars (decouple TCP/UDP) | not started |

## C. epics-base open issues — validate epics-rs against

| Issue | Failure mode | epics-rs check |
|---|---|---|
| [#867](https://github.com/epics-base/epics-base/issues/867) | `db_queue_event_log` drops most-recent, keeps oldest | dbEvent ring/coalesce policy → `crates/epics-base-rs/src/server/pv.rs::ProcessVariable::notify_subscribers` overwrites the coalesce slot with the **newest** event and bumps `DROPPED_MONITOR_EVENTS` — explicitly the opposite of the upstream bug. **inspected, equivalent (correct policy)** |
| [#868](https://github.com/epics-base/epics-base/issues/868) | event-queue-add condition suspect | dbEvent queue branch → epics-base-rs uses bounded `tokio::mpsc(N)` + per-subscriber coalesce slot; no `rngSpace<=EVENTSPERQUE` numeric comparison exists. **inspected, n/a** |
| [#855](https://github.com/epics-base/epics-base/issues/855) | dbCa duplicate subscriptions in `CA_MONITOR_STRING` | client subscribe path string-type case → `crates/epics-ca-rs/src/client/subscription.rs` registers exactly one `SubscriptionRecord` per `subid`; epics-rs has no dbCa-style auto dual (native+string) subscription. **inspected, n/a** |
| [#823](https://github.com/epics-base/epics-base/issues/823) | calc breaks if unused INPx link is broken | calc input-connect failure handling |
| [#666](https://github.com/epics-base/epics-base/issues/666) | dbLoadTemplate vs msi inconsistent on empty instance | template engine |
| [#643](https://github.com/epics-base/epics-base/issues/643) | server filter framework shutdown safety | CA server filter teardown |
| [#572](https://github.com/epics-base/epics-base/issues/572) | PVA vs CA monitor performance | self comparison |
| [#549](https://github.com/epics-base/epics-base/issues/549) | DB parse-error message printed twice | dbd/db parser |
| [#548](https://github.com/epics-base/epics-base/issues/548) | lso VAL short-string fails | `records/lso.rs` |
| [#521](https://github.com/epics-base/epics-base/issues/521) | mbbi/mbbo: RVAL as index when *VL undefined | mbbi/mbbo conversion |
| [#488](https://github.com/epics-base/epics-base/issues/488) | startup-only DNS → IP change = permanent disconnect | `crates/epics-ca-rs/src/client/mod.rs::parse_addr_list` (line ~3038) calls `to_socket_addrs()` once at `CaClient::new`; no re-resolution on reconnect. Same upstream defect class. **UNFIXED** — needs (a) `parse_addr_list` to preserve hostname per entry, (b) reconnect path to call `to_socket_addrs()` with cached hostname, (c) DNS TTL refresh policy. Upstream PR not yet merged either; gateway upstream-monitor already auto-restarts but resolves the original IP only. Tracked as a follow-up: link-state framework (#856/#768) is now in place, but the per-entry hostname plumbing is invasive across `transport.rs` / `state.rs` / `search.rs` and warrants its own PR with a settled refresh-policy decision |
| [#477](https://github.com/epics-base/epics-base/issues/477) | 30s hang after destroy of both ends | ca-rs Drop/abort path; latent risk |
| [#455](https://github.com/epics-base/epics-base/issues/455) | OS clock < EPICS epoch → CA client spin | epoch validation in time conversions |
| [#426](https://github.com/epics-base/epics-base/issues/426) | nameserver + CA_V413 incompat | nameserver TCP handshake |
| [#372](https://github.com/epics-base/epics-base/issues/372) | Mass-channel search performance | AIMD search budget under load |
| [#324](https://github.com/epics-base/epics-base/issues/324) | dbEvent `eventsRemaining` lost on cancel | dbEvent cancel path |

## D. asyn-rs vs upstream asyn

Upstream asyn merged in last 12 months: **0**. Project effectively dormant. Open items:

### Open PRs

| PR | Note | asyn-rs status |
|---|---|---|
| [#217](https://github.com/epics-modules/asyn/pull/217) | autoconnect: queue connect timer on enable | `port.rs` autoconnect timing — check |
| [#211](https://github.com/epics-modules/asyn/pull/211) | Nonblocking connect via poll | tokio is async by construction; verify `connect_timeout` policy |
| [#188](https://github.com/epics-modules/asyn/pull/188) | Auto serial break after every write | serial driver option — likely missing |
| [#67](https://github.com/epics-modules/asyn/pull/67) | `ASYN_TRACE_STATE` mask bit | `trace.rs` mask flags |
| #228, #226, #225, #222, #216, #196, #145, #130, #229, #230 | C-only / docs | n/a |

### Open issues

| Issue | Concern | asyn-rs check |
|---|---|---|
| [#227](https://github.com/epics-modules/asyn/issues/227) | lockless asynPortDriver | actor model already addresses this; add note in README |
| [#224](https://github.com/epics-modules/asyn/issues/224) | autoConnect connects too early | `port_actor.rs` startup ordering |
| [#220](https://github.com/epics-modules/asyn/issues/220) | `pasynOctetSyncIO->read` overwrites `pasynUser->timeout` | `sync_io.rs::read` timeout handling |
| [#218](https://github.com/epics-modules/asyn/issues/218) | Add `getLimits` to interfaces | `AsynInt32::get_bounds` / `AsynInt64::get_bounds` already existed; `AsynFloat64::get_limits` added with `(NEG_INFINITY, INFINITY)` default | **done** |
| [#215](https://github.com/epics-modules/asyn/issues/215) | Long-running scan period drift | tokio `interval` MissedTickBehavior policy |
| [#170](https://github.com/epics-modules/asyn/issues/170) | Parallel callback queue overflow | `interrupt.rs` queue capacity |
| [#167](https://github.com/epics-modules/asyn/issues/167) | AAI/AAO record support | `asyn_record/` mapping |
| [#166](https://github.com/epics-modules/asyn/issues/166) | `asynMask` shift parameter | `param.rs` mask layout |
| [#146](https://github.com/epics-modules/asyn/issues/146) | `setStringParam` NULL ptr | `Option<&str>` makes this n/a — confirm |
| [#103](https://github.com/epics-modules/asyn/issues/103) | EOS setters block IOC init/exit if no device | `interpose/eos.rs` connect-wait policy |
| [#82](https://github.com/epics-modules/asyn/issues/82) | drvAsynIPServerPort `readIt()` bug | server-port read loop |
| [#44](https://github.com/epics-modules/asyn/issues/44) | destroy + reconstruct asynPortDriver | `manager.rs` re-registration |

## E. Recommended starting order

Ranked by ratio of (epics-rs regression risk) to (verification cost):

1. **PR #732 NTNDArray::getValueSize** — wire-format bug. Read pvxs commit, diff against `crates/epics-pva-rs/src/nt/nd_array.rs`, add a byte-level regression test mirroring pvxs reference vector. Self-contained, decisive.
2. **Issue #867 / #868 dbEvent queue policy** — read epics-base 7.0 commit referenced in the issue, compare with `crates/epics-base-rs/src/server/db/event*` queue add + drop paths.
3. **Issue #855 dbCa duplicate subscriptions on CA_MONITOR_STRING** — review client subscribe path for the string-DBR branch; add regression test.
4. **PR #817 bi/mbbi AFTC** — record-level correctness; bounded scope.
5. **PR #655 calc A–U inputs** — record correctness; mechanical extension.
6. **PR #856 / #768 iocInit dbCa wait** — broader semantic change to IOC startup; sequence with #855.
7. **Issue #488 DNS-changed disconnect** — validate client reconnect re-resolves DNS (gateway side already handled).
8. **PR #641 secure pvaccess** — defer until upstream merges; mark for re-validation gate.

## G. Older PRs (2023-05 .. 2025-05, audit summary)

Snapshot from `gh pr list --search "merged:2023-05-10..2025-05-10"`. Bulk
categorization first; individual rows only for items where the change has
behavioural impact on epics-rs.

### Bulk N/A — pure C / build / platform / docs

These have no Rust counterpart and are listed here as an audit receipt
rather than per-PR rows: #647 (RTEMS NVRAM warnings), #645 (Windows conda),
#642/#591/#198/#173 (doc typos), #627 (32-bit `epicsStrtod`), #615/#403
(VxWorks/RTEMS attrs), #612 (Pod→HTML), #611/#609/#606/#168 (CI),
#604/#589 (Python LINKER_USE_RPATH), #599/#542 (extern C, isnan/isinf
defines), #543 (`std::unexpected`), #540 (noreturn attr), #539 (genVersion
submodules), #535/#534/#533/#530/#509 (warnings/leaks in C), #527 (FreeBSD
arch), #523/#492/#465 (compiler/error messages), #517 (`-D_FORTIFY_SOURCE=3`),
#511/#420 (SPDX/RTD), #496 (UB `pthread_join`), #482/#481/#484 (doc/test
fixes), #470/#519/#509 (C memory leaks), #461 (VSCode makefile),
#460 (WIN32 setThreadName), #458 (accept return type), #454 (Clang-15),
#453/#452 (CodeQL/typed dbEvent typing), #451 (initMainThread), #448/#440
(help text/RTEMS LDFLAGS), #437 (NULL callback — Rust types prevent),
#447 (link types as text — debug only), #375 (RTEMS MVME2700).

### Behaviour changes mapped to epics-rs

| PR | Merged | Change | Where in epics-rs | Status |
|---|---|---|---|---|
| [#571](https://github.com/epics-base/epics-base/pull/571) | 2024-12 | Recursion bug v2 (record process re-entry) | `record_instance.rs::processing: AtomicBool` re-entrancy guard + `process_record_with_links` visited set | inspected, equivalent |
| [#568](https://github.com/epics-base/epics-base/pull/568) | 2024-12 | Propagate `AMSG` (alarm message string) through MS links | `CommonFields` has no `amsg`/`namsg` | **not started** |
| [#566](https://github.com/epics-base/epics-base/pull/566) | 2024-11 | Clear `NAMSG` together with NSTAT/NSEV | depends on #568 (no `amsg` field) | **not started** |
| [#559](https://github.com/epics-base/epics-base/pull/559) | 2025-03 | CP link triggers RPRO during target processing | `processing.rs:1318` sets `rpro=true` when target is mid-process | inspected, equivalent |
| [#544](https://github.com/epics-base/epics-base/pull/544) | 2024-11 | `DBE_PROPERTY` only when property field actually changed | `record_instance.rs::notify_field_written` invalidates metadata cache; verify PROPERTY event gating uses same comparison | **not started** (audit) |
| [#520](https://github.com/epics-base/epics-base/pull/520) | 2024-08 | readline: keep history only in interactive sessions | `iocsh/mod.rs` rustyline — interactive default | inspected, equivalent |
| [#516](https://github.com/epics-base/epics-base/pull/516) | 2024-06 | `RSRV_SERVER_PORT` > 9999 | server port stored as `u16` (0..65535) — no 4-digit format clip | inspected, equivalent |
| [#508](https://github.com/epics-base/epics-base/pull/508) | 2025-02 | `iocshSetError` in more places | `iocsh/mod.rs:115` uses non-zero exit equivalent; broader instrumentation pending | inspected, partial |
| [#505](https://github.com/epics-base/epics-base/pull/505) | 2024-08 | Allow record deletion at database creation | `PvDatabase::remove_record(name) -> bool` cleans records map + scan_index + cp_links; iocsh `dbDeleteRecord <name>` exposes it | done |
| [#501](https://github.com/epics-base/epics-base/pull/501) | 2024-10 | `asTrap` serverSpecific is `dbChannel` | ACF trap pipeline does not expose dbChannel handle | **not started** |
| [#486](https://github.com/epics-base/epics-base/pull/486) | 2024-05 | `printf` record `sizv` fix | `records/printf.rs` — verify SIZV field bound | **not started** (audit) |
| [#468](https://github.com/epics-base/epics-base/pull/468) | 2024-05 | compress record fix | `records/compress.rs::process` re-zeroes `val` + clears `nuse`/`off` when RES != 0; framework's process-then-monitor flow fans the zeroed buffer out as a regular VAL change. Equivalent to base's "fix issue with compress record" | inspected, equivalent |
| [#467](https://github.com/epics-base/epics-base/pull/467) | 2024-06 | Off-by-one in constant link fetch | `record/link.rs::ParsedLink::Constant` — verify offset/length math | **not started** (audit) |
| [#463](https://github.com/epics-base/epics-base/pull/463) | 2024-02 | `dbLoadRecords` allows macros with defaults without substitutions | `db_loader::dbLoadRecords` — verify macro-default semantics | **not started** (audit) |
| [#592](https://github.com/epics-base/epics-base/pull/592) | 2025-03 | `dbServerStats()` API | introspection module exposes counters; full `dbServerStats` shape pending | **not started** |
| [#594](https://github.com/epics-base/epics-base/pull/594) | 2025-03 | `initHookRegister` idempotent + MustSucceed | epics-rs init hook registration — verify idempotency | **not started** (audit) |
| [#581](https://github.com/epics-base/epics-base/pull/581) | 2025-02 | Post monitors from compress record on reset | covered by `records/compress.rs::process` re-running through the framework's monitor fan-out path; reset writes `val[*] = 0.0` and the `notify_subscribers` step handles the rest | inspected, equivalent |
| [#578](https://github.com/epics-base/epics-base/pull/578) | 2024-12 | Document UDFS field | `records/dbCommon` UDFS docs — Rust-side optional | inspected, equivalent (UDFS field present) |
| [#551](https://github.com/epics-base/epics-base/pull/551) | 2025-02 | Null-check `IOCSH_STARTUP_SCRIPT` | `iocsh/mod.rs` script-loader env var | inspected, equivalent |
| [#558](https://github.com/epics-base/epics-base/pull/558) (already in B) | 2025-06 | `afterIocRunning` iocsh command | iocsh commands | not started |
| [#450](https://github.com/epics-base/epics-base/pull/450) | 2025-03 | Lock record for `db_create_read_log()` and `dbChannelGetField()` | dbChannel get path concurrency | inspected, equivalent (`RwLock<RecordInstance>`) |
| [#439](https://github.com/epics-base/epics-base/pull/439) | 2023-11 | mbboDirect `B0..BF` fields ASL0 | `records/mbbo_direct.rs` — verify per-bit ACF level | **not started** (audit) |
| [#434](https://github.com/epics-base/epics-base/pull/434) | 2023-11 | DB parser hint for unknown field name | `db_loader` parse error UX | **not started** |
| [#432](https://github.com/epics-base/epics-base/pull/432) | 2023-11 | Avoid hang during concurrent `db_cancel_event()` | epics-rs uses `tokio::mpsc` + `subscribers.lock()` — drop semantics differ; no hang risk on cancel | inspected, equivalent |
| [#371](https://github.com/epics-base/epics-base/pull/371) | 2023-11 | iocsh: trim multiple trailing newlines | iocsh output formatting | **not started** (audit) |

### asyn (older — 2020 .. 2024)

asyn upstream is dormant; the older PR set is mostly C-only, build, or
docs. Only items with a behavioural counterpart in `crates/asyn-rs`:

| PR | Merged | Change | asyn-rs status |
|---|---|---|---|
| [#208](https://github.com/epics-modules/asyn/pull/208) | 2024-06 | Fix `mbboDirect` asyn:READBACK | `AsynDeviceSupport::asyn_readback` flag + `io_intr_receiver` activates the mailbox path when `SCAN != IoIntr` and the flag is set. Info-tag plumbing follow-up | **partial — Rust API done, info-tag capture follow-up** |
| [#200](https://github.com/epics-modules/asyn/pull/200) | 2024-02 | Connection cleanup avoids spurious errors at IOC exit | `port_actor`/`transport` shutdown — async drop semantics differ from C; verify quiet exit | inspected, equivalent (actor abort cascades) |
| [#180](https://github.com/epics-modules/asyn/pull/180) | 2023-05 | Send serial break via option interface | drvAsynSerialPort missing in asyn-rs | **not started** |
| [#171](https://github.com/epics-modules/asyn/pull/171) | 2024-11 | Destructible ports | actor model in asyn-rs makes ports destructible by construction | inspected, equivalent |
| [#162](https://github.com/epics-modules/asyn/pull/162) | 2022-09 | Improve waveform device support, add aai/aao | `asyn_record/` AAI/AAO mapping missing | **not started** |
| [#157](https://github.com/epics-modules/asyn/pull/157) | 2022-09 | `asynDisconnected`/`asynDisabled` strings | `error::AsynStatus` Display impl | inspected, equivalent |
| [#148](https://github.com/epics-modules/asyn/pull/148) | 2022-04 | Bind interface for IP server port | `transport` IP-server bind options | **not started** |
| [#109](https://github.com/epics-modules/asyn/pull/109) | 2020-05 | `tcp&`/`udp&` SO_REUSEPORT protocols | socket option support in transport | **not started** |
| [#104](https://github.com/epics-modules/asyn/pull/104) | 2020-02 | Add lsi/lso/printf record support | `asyn_record/` lsi/lso/printf mapping | **not started** |
| Older (#125, #123, #120, #117, #116, #115, #114, #107, #106, #102, #101, #100, #98, #97, #144, #142, #140, #128) | 2020..2022 | C++/build only / std-bound legacy | inspected, n/a |

### Audit follow-up backlog

The `**not started** (audit)` rows above are spot-checks that look
plausible but were not exhaustively verified in this round. They sit at
the front of the queue for a future audit pass:

1. PROPERTY event gating (#544)
2. printf SIZV (#486)
3. compress record reset (#468, #581)
4. constant link fetch off-by-one (#467)
5. dbLoadRecords macro-default semantics (#463)
6. mbboDirect B0..BF ASL0 (#439)
7. iocsh trailing newline trim (#371)
8. initHookRegister idempotency (#594)

Substantive scope (own PR each):

- **AMSG / NAMSG alarm message string** (#568, #566) — adds two new
  fields to `CommonFields`, plumbs through MS link propagation, every
  record's process path gains an `amsg` argument. Visible in CA/PVA
  monitor metadata and ACF audit logs.
- **dbServerStats / asTrap dbChannel** (#592, #501) — new introspection
  surface; one-shot API addition.
- **Record-deletion at DB creation** (#505) — `db_loader` extension.

## H. Even older PRs (2020-01 .. 2023-05, audit summary)

Same audit pattern as Section G, against the 2020-01..2023-05-09 window
plus an asyn 2015-2020 sweep. PR numbers in this range mix 7.0 / 3.15 /
3.14 backports — the listing below is by behaviour, not by branch.

### epics-base 2020-2023 — bulk N/A

Build / RTEMS / Windows / VxWorks / docs typo / CI: #270 (SetThreadDescr),
#262 (genVersion epoch), #210 (regressTest), #206 (RTEMS osdEvent),
#201 (Win32 GetParam refactor), #198 (DEBUG flags), #182/#181 (osiSock vs
osdSock), #179 (vxWorks 6.3 close()), #176 (RTEMS5 QEMU), #171/#150/#148
(SONAME / DBCORE_API / -Z7), #163 (RELEASE validation), #146 (test
timeouts), #132/#125 (epicsStrtod / OS strtod), #131 (Win32 waitable
timers), #130 (msi.cpp typing), #105 (RTEMS5 rebases), #103 ("Command"
build target), #91/#88 (READMEs/docs), #82 (RTEMS msgQ), #81 (SPDX),
#79 (osdSockAddrReuse), #77/#73/#70/#74 (static-analysis / Makefile /
doxy-libcom), #218 (epicsInt8 signedness), #214 (fork() warning, Rust
N/A), #75 (sim doc), #26 (timestamp before outlink — rare path).

### epics-base 2020-2023 — behaviour rows

| PR | Merged | Change | Where in epics-rs | Status |
|---|---|---|---|---|
| [#199](https://github.com/epics-base/epics-base/pull/199) | 2021-10 | Cleanup `mbboDirect` bit field handling | `records/mbbo_direct.rs` — verify B0..BF bit-mask compose/decompose | **not started** (audit) |
| [#193](https://github.com/epics-base/epics-base/pull/193) | 2022-01 | Clear `IP_MULTICAST_ALL` on Linux (drop unrelated multicast) | already applied: `epics-base-rs/src/net/async_udp_v4.rs:630`, `epics-ca-rs/src/server/udp.rs:87`, `epics-base-rs/src/net/loopback_mcast.rs:69` all call `socket.set_multicast_all_v4(false)`; `epics-pva-rs` server uses `AsyncUdpV4::bind` so it inherits the same option | inspected, equivalent (already applied) |
| [#191](https://github.com/epics-base/epics-base/pull/191) | 2021-08 | `int64in`: fix monitor delta test | `records/int64in.rs` MDEL/ADEL comparison vs i64 vs f64 | **not started** (audit) |
| [#213](https://github.com/epics-base/epics-base/pull/213) | 2022-01 | Hex literals in hardware links (e.g. `@dev 0xFF`) | `record/link.rs` hardware-link parser does not split hex args | **not started** — hardware-link forms not parsed at all yet |
| [#144](https://github.com/epics-base/epics-base/pull/144) | 2022-05 | Add `SIMM=RAW` to ao records | `records/ao.rs` SIMM stored but RAW menu not handled distinct from VAL | **not started** (audit) |
| [#114](https://github.com/epics-base/epics-base/pull/114) | 2021-03 | Post array events against the field pointer, not the array pointer | `record_instance.rs::notify_field_written` keys events by field name, equivalent in spirit | inspected, equivalent |
| [#112](https://github.com/epics-base/epics-base/pull/112) | 2021-02 | Limit auto-declaration of record types to `regRecDevDrv` only | `db_loader::register_record_type` requires explicit registration — equivalent | inspected, equivalent |
| [#99](https://github.com/epics-base/epics-base/pull/99) | 2021-03 | Remove `dbfl_type_rec` (legacy direct-record dbflag) | epics-rs has no dbfl_type_rec equivalent — clean by construction | inspected, n/a |
| [#86](https://github.com/epics-base/epics-base/pull/86) | 2021-01 | Add JSON5 support (trailing commas, hex, comments in db files) | epics-rs `link.rs::parse_link_v2` does not parse inline JSON link options (`{ca: {pv:'foo'}}`) at all — JSON-link grammar itself is missing. JSON5 alone has nothing to apply to | **deferred** — needs the JSON-link grammar in `link.rs` first, then JSON5 leniency layered on top |
| [#78](https://github.com/epics-base/epics-base/pull/78) | 2020-07 | Restrict character set for record names | `db_loader::validate_record_name` mirrors base `dbRecordNameValidate`: empty / space / `.` / `$` / quote → error; leading `-`/`+`/`[`/`{` and non-printable → warn. Tests: `name_validation_*` | done |

### asyn 2015-2020 — behaviour rows

Bulk N/A: build/typo/leak fixes, Windows port handling, USBTMC, VXI-11,
GPIB, FTDI driver internals, Travis CI. Listed for receipt: #92, #91,
#89, #84, #81, #78 (typo), #76, #74, #73, #71, #69 (XON/XOFF — IOC shell
flow control), #61, #57, #55, #53, #45, #43, #40, #39, #38, #36, #33, #32,
#27, #23, #21, #20, #19, #18, #17, #15, #13, #10, #8, #6, #4, #1.

| PR | Merged | Change | asyn-rs status |
|---|---|---|---|
| [#88](https://github.com/epics-modules/asyn/pull/88) | 2019-10 | Add `drvAsynFTDIPort` | FTDI driver missing in asyn-rs — already covered by `feature = "ftdi"` PR queue elsewhere | **not started** |
| [#84](https://github.com/epics-modules/asyn/pull/84) | 2019-08 | Shut down thread before destroying `asynPortDriver` | `port_actor` actor model joins on drop — already correct | inspected, equivalent |
| [#76](https://github.com/epics-modules/asyn/pull/76) | 2019-01 | String options for `asynSetTrace*Mask` | `trace.rs` mask parsing — verify accepts string keys | **not started** (audit) |
| [#66](https://github.com/epics-modules/asyn/pull/66) | 2017-12 | `devEpics`: ASLO/AOFF/SMOO conversion on ai/ao float64 | epics-rs applies the slope/offset/smoothing transforms at the record layer — `records/ai.rs::process` and `records/ao.rs::process` already implement (val-eoff)/eslo + AOFF + ASLO + SMOO. asyn-rs adapter forwards raw values to record `set_val`; the record runs the transform. | inspected, equivalent (record-layer) |
| [#60](https://github.com/epics-modules/asyn/pull/60) | 2017-11 | Process output records on `asyn:READBACK` callbacks | covered by the same `AsynDeviceSupport::asyn_readback` Rust-API plumbing as #208 | **partial — Rust API done, info-tag capture follow-up** |
| [#13](https://github.com/epics-modules/asyn/pull/13) | 2016-02 | `asynOption` interface on drvAsynIPPort | `asynOption` registration in `interfaces/` — partial | **not started** (audit) |
| [#6](https://github.com/epics-modules/asyn/pull/6) | 2015-10 | `drvAsynIPPort`: configurable disconnect on read timeout | transport read-timeout policy in asyn-rs — verify configurable | **not started** (audit) |

### Section G+H roll-up — substantive items added to backlog

| Class | Items | Reason for own-PR scope | Status |
|---|---|---|---|
| Alarm-message string | #568 AMSG / #566 NAMSG | `CommonFields::{amsg, namsg}` + `recgbl::rec_gbl_set_sevr_msg` + `rec_gbl_reset_alarms` transfer + MS-link `LinkAlarm::amsg` propagation in `processing.rs` + AMSG/NAMSG getters/setters in `record_instance.rs`. Tests: 4 `recgbl` cases | **done** |
| JSON-link grammar | #86 | `link.rs::try_parse_json_link` recognizes `{const: …}`, `{ca: { pv: "…" }}`, `{pva: { pv: "…" }}` (unquoted-key + single-quote tolerant). 7 tests | **done (JSON5 leniency on inline db files still deferred)** |
| Hardware-link parsing | #213 hex in HW links + `@dev arg` / `#Cn Sn` forms | `link.rs::ParsedLink::Hw(HwLink { kind, args, raw })` with `try_parse_hw_link`; preserves hex literals as full-token args. 4 tests | **done** |
| ~~`IP_MULTICAST_ALL` socket option~~ | ~~#193~~ | already applied — see Section H | **done (already applied)** |
| ASLO/AOFF/SMOO on asyn | #66 | record-layer transform in `records/ai.rs` + `ao.rs` already applies ASLO/AOFF/SMOO. asyn-rs adapter forwards raw values to `set_val`; record's `process()` runs the conversion. | **done (record-layer)** |
| asyn:READBACK | #208, #60 | `AsynDeviceSupport::set_asyn_readback(true)` Rust-API toggle wired through `io_intr_receiver`. Info-tag auto-capture is the remaining follow-up | **partial — Rust API done** |
| Record-name validation | #78 | parser-side gate + regression for legacy databases | **done (this branch)** |

### epics-base pre-2020 (2017-2020)

Mostly POD/doc work (#33 compressRecord, #31 waveform/menuFtype, #43-48
mbbi/mbboDirect/permissive/state/stringin POD); behaviourally relevant:

- **#25 (2019-03)**: stripped-down fix for Launchpad bug 1816841 — record
  monitor lockup under specific event delivery race. epics-rs uses
  `tokio::mpsc` per subscriber + coalesce slot, no equivalent path.
  Inspected, n/a.

## I. Closed issues audit (epics-base + asyn)

`gh issue list --state closed`. Most closed issues reduce to "fixed by
PR #N already in Sections A/B/G/H" — a roll-up rather than per-row
duplication. Listed here are issues that introduced **new behaviour
gaps** not yet captured in earlier sections, plus a correction list for
items that turn out to be already-fixed in epics-rs.

### Already-equivalent in epics-rs (audit corrections)

Re-grepped during the closed-issue pass:

| Issue | Original concern | Why epics-rs already covers it |
|---|---|---|
| [#485](https://github.com/epics-base/epics-base/issues/485) | IOC segfault when SIZV > 32767 in `printfRecord` | `crates/epics-base-rs/src/server/records/printf.rs::sizv: u16` (default 256, capped via `v.max(1) as u16`) — the i16 truncation that segfaulted base C is structurally absent |
| [#692](https://github.com/epics-base/epics-base/issues/692) | CA link truncates `0xffffffff` to `0x7fffffff` | `record/link.rs::ParsedLink::Constant(String)` retains the literal as a string until typed parse — i32 truncation does not happen at parse time; downstream `to_f64()` round-trips through f64 |
| [#174](https://github.com/epics-base/epics-base/issues/174) | Initial alarm STAT/SEVR of base record types | `CommonFields::default()` sets `sevr = NoAlarm` and `stat = NO_ALARM`; `udf = true` triggers `rec_gbl_check_udf` on first process — equivalent to base's "Initial NaN/UDF then resolve" |
| [#361](https://github.com/epics-base/epics-base/issues/361) | `printf` record does not format `%%` correctly | `records/printf.rs:44` has a `%%` escape branch |
| [#187](https://github.com/epics-base/epics-base/issues/187) | `waveformRecord` missing `PACT=true` during async | `RecordInstance::processing: AtomicBool` is the PACT equivalent; async device support sets it via `is_processing()` |
| [#423](https://github.com/epics-base/epics-base/issues/423) | dbEvent: double cancel causes hang | `subscribers.lock().retain(|sub| !sub.tx.is_closed())` makes double-cancel a no-op |
| [#9](https://github.com/epics-base/epics-base/issues/9) | compress record FIFO/LIFO not working (very old) | check `records/compress.rs` algo |

### New gaps captured from closed issues

| Issue | Gap | epics-rs site to revisit |
|---|---|---|
| [#564](https://github.com/epics-base/epics-base/issues/564) | DBR_ULONG negative-number handling | epics-rs CA wire does not expose `DbFieldType::ULong` / `EpicsValue::ULong` as a usable type — `DBR_LONG` (i32) is the supported integer-32 form; unsigned interpretation is the receiver's responsibility via bit-cast. The base defect (negative i32 written into u32 path) cannot reach our codebase | inspected, n/a |
| [#421](https://github.com/epics-base/epics-base/issues/421) | Cannot put JSON links via pvput | `epics-pva-rs` PUT path JSON link handling — likely missing |
| [#312](https://github.com/epics-base/epics-base/issues/312) | `dbLoadRecords` should warn/error on alias with field part | `db_loader` alias parser permissive |
| [#284](https://github.com/epics-base/epics-base/issues/284) | Constant `INP*` to aSub | `records/asub_record.rs` constant link plumbing — verify each FT* type |
| [#280](https://github.com/epics-base/epics-base/issues/280) | Timestamp when monitoring non-VAL field | `record_instance.rs::snapshot_for_field` (line 269) sets `Snapshot::ts = self.common.time` for **every** field — VAL or otherwise — so non-VAL monitor updates carry the same timestamp as the record. The base "undef on first non-VAL update" defect is structurally absent | inspected, equivalent |
| [#209](https://github.com/epics-base/epics-base/issues/209) | `ca_enable_preemptive_callback` gets no monitor updates | epics-rs CA client is preemptive by construction (tokio tasks); not applicable as a bug |
| [#194](https://github.com/epics-base/epics-base/issues/194) | Long string `CALC$` issue | `records/scalcout.rs` `$` suffix handling — string-typed input |
| [#190](https://github.com/epics-base/epics-base/issues/190) | CA connections "stalled" after sleeping (laptop suspend) | `ca-rs` echo watchdog (already P-G fix) — should detect; verify under the suspend path |
| [#183](https://github.com/epics-base/epics-base/issues/183) | DB link `DBF_MENU` to `DBF_STRING` conversions broken | `database/links.rs::read_link_value` type-conversion table — verify menu→string |
| [#106](https://github.com/epics-base/epics-base/issues/106) | Timers expire early | tokio::time uses monotonic clock; not subject to `epicsTimer` quantization issue |
| [#97](https://github.com/epics-base/epics-base/issues/97) | Segfault reading from compress to aai | `records/{compress,aai}.rs` cross-record link read — verify size negotiation |

### asyn closed issues — new gaps

| Issue | Gap | asyn-rs site |
|---|---|---|
| [#231](https://github.com/epics-modules/asyn/issues/231) | UInt64 interface support | `interfaces/uint64.rs` adds `AsynUInt64` + `AsynUInt64Array` traits with default `get_bounds`. `InterfaceType::UInt64` registered. ParamValue / RequestOp plumbing through the actor protocol is the remaining follow-up | **partial — trait surface done** |
| [#136](https://github.com/epics-modules/asyn/issues/136) | Records with `asyn:READBACK` not setting STAT/SEVR correctly when UDF=0 | adapter callback path — same scope as #208 / #60 |
| [#80](https://github.com/epics-modules/asyn/issues/80) | Deadlocks with `asyn:READBACK` on output records | actor model in asyn-rs naturally avoids synchronous deadlock — verify |
| [#79](https://github.com/epics-modules/asyn/issues/79) | Add `asynInterposeDelay` and `asynInterposeEcho` | both already in `asyn-rs/src/interpose/` (`delay.rs` `DelayInterpose` with per-character write delay; `echo.rs` `EchoInterpose` for half-duplex devices). inspected, equivalent (already implemented) |
| [#56](https://github.com/epics-modules/asyn/issues/56) | Race condition with info tag `asyn:READBACK` | adapter path |
| [#46](https://github.com/epics-modules/asyn/issues/46) | Reporting parameter value change to driver | `param.rs` change-notify hook — verify direction (driver → record vs record → driver) |
| [#30](https://github.com/epics-modules/asyn/issues/30) | Enhance `asynInt32Average` / `asynFloat64Average` device support | averaging device support not in asyn-rs |

### Repo audit horizon reached

`gh pr list --search "merged:<2017-01-01"` for `epics-base/epics-base`
returns `[]`; `merged:<2015-01-01"` for `epics-modules/asyn` returns
`[]`. The respective GitHub PR histories begin at 2017 and 2015 — older
patches predate the GitHub migration and live only in the Launchpad
mirror. Pre-2017 epics-base merged PRs are exclusively dbd POD doc
work plus a single behavioural fix (#25, race in event delivery —
inspected n/a; per-subscriber mpsc model).

## J. Stability roll-up (cross-section)

This section pulls every PR/issue from Sections A–I that addresses a
**stability** concern — race, deadlock, hang, crash, leak, spin loop,
panic, recovery, or shutdown ordering. The intent is operational: if a
production-leaning user wants a "what's safe vs what isn't" snapshot,
they read here. Feature additions are excluded.

Tagging convention:

- `equivalent`: the same defect class is structurally absent in
  epics-rs (often via Rust ownership, type system, or async-runtime
  semantics).
- `applied`: an explicit fix lives in epics-rs.
- `audit`: spot-check pending; needs a regression test or manual
  trace.
- `gap`: latent risk in epics-rs; warrants its own follow-up.

### epics-rs already covers (equivalent / applied)

| Upstream | Defect | epics-rs site |
|---|---|---|
| epics-base #571 | record process re-entry recursion | `record_instance.rs::processing: AtomicBool` + visited set in `process_record_with_links` (equivalent) |
| epics-base #496 / #745 | UB pthread_join, epicsThread race | tokio task model, no equivalent unsafe path (equivalent) |
| epics-base #432 / #324 | dbEvent double-cancel hang, eventsRemaining missed | `subscribers.lock().retain(\|s\| !s.tx.is_closed())` + bounded mpsc per subscriber (equivalent) |
| epics-base #543 | std::unexpected deprecated | Rust panic / Result idioms (equivalent) |
| epics-base #437 | Null-check callback in callbackRequest | typed `Option<Fn>` makes null callback impossible (equivalent) |
| epics-base #559 | CP link RPRO during target-mid-process | `processing.rs:1318` sets `rpro=true` when target is processing (equivalent) |
| epics-base #450 / #331 | rsrv `db_create_read_log` lock | `RwLock<RecordInstance>` per record (equivalent) |
| epics-base #517 | `_FORTIFY_SOURCE=3` C buffer checks | Rust bounds-checks at runtime (equivalent) |
| epics-base #211 | `fork()` safety | tokio + Rust drop order; no fork model (equivalent — out of scope) |
| epics-base #485 | printfRecord SIZV>32767 segfault | `sizv: u16` field — i16 truncation absent (equivalent) |
| epics-base #692 | CA link 0xffffffff truncation | `ParsedLink::Constant(String)` retains literal (equivalent) |
| epics-base #423 | dbEvent double-cancel hang | mpsc + retain pattern (equivalent — fix-committed upstream) |
| epics-base #495 | posix data race on `pthreadInfo->osiPriority` | tokio task priority model (equivalent — out of scope) |
| epics-base #683 | `lockSetsActive` accessed without lock | epics-rs has no global lock-set table — different concurrency model (equivalent — out of scope) |
| epics-base #758 | yajl `yajl_render_error_string` buffer overflow | `serde_json` (Rust, bounded-checked) (equivalent — different lib) |
| epics-base #380 | `astac()` leaks resources | C-only; epics-rs ACF uses owned strings (equivalent) |
| epics-base #444 | linux/ header in osdSockUnsentCount | C build; tokio sockets (equivalent) |
| epics-base #25 | event-delivery race (2019) | per-subscriber mpsc + coalesce slot (equivalent) |
| epics-rs commit acfa608 | `CaClient` / `ServerConnection` task abort on Drop | `client/mod.rs:954`, `client/transport.rs:130` — applied on this branch's history |
| epics-rs commit df93e49 | Beacon EMA reset on TCP (re)connect | applied |
| epics-rs / G1-G4 | server TCP send timeout, per-channel monitor cap, 64KB UDP buffer, CAS_USE_HOST_NAMES forward-DNS | applied (see kodex audit notes) |

### epics-rs gaps under the stability lens

These are the items where epics-rs may share — or has not been verified
free of — the upstream defect. Each one needs an explicit regression
test before it can move to "equivalent".

| Upstream | Defect | epics-rs site to verify | Risk class |
|---|---|---|---|
| epics-base #455 | OS clock < EPICS epoch → CA client spin | `epics-ca-rs/src/client/{transport,beacon_monitor,circuit_breaker}.rs` use `tokio::time::Instant` / `std::time::Instant` (monotonic) for every deadline / freshness check; `SystemTime::now()` appears only as a one-shot snapshot timestamp in `subscription.rs:85` (no comparison). Wall-clock < epoch never enters a comparison path | inspected, equivalent |
| epics-base #438 | Test IOC w/ ACF hangs in `asCaStart()` after first run | ACF reload code path under repeated invocation | medium (audit) |
| epics-base #477 | Application hangs 30 s after both ends destroy | `ServerConnection::drop` aborts both pumps; `drop_aborts_read_and_write_tasks` test now also asserts the abort cascade completes within 500 ms — a regression toward "let echo timeout drain" surfaces immediately | inspected, equivalent (regression-guarded) |
| epics-base #190 | CA connections "stalled" after laptop suspend | echo heartbeat (P-G fix) should detect; verify under `pmset` suspend on macOS / `systemctl suspend` on Linux | medium (environmental) |
| epics-base #426 | CA nameserver + CA_V413 protocol mismatch | `EPICS_CA_NAME_SERVERS` TCP search handshake under v4.13 servers | low |
| epics-base #469 | `iocshLoad` doesn't undo `IOCSH_STARTUP_SCRIPT` | `iocsh/mod.rs` env-restore on script-load nesting | low (UX rather than crash) |
| epics-base #97 | Segfault reading from compress to aai | epics-rs has no `aai` record at all; `records/compress.rs` outputs `Vec<f64>`. The base segfault depended on a stack-allocated dst buffer with fixed NELM; not reproducible in our model | inspected, n/a (no aai) |
| epics-base #557 | CP link inconsistency with async record | `processing.rs` async-record + CP-link interaction | low |
| epics-base #471 | `iocshLoad` + `on error` bad interaction | iocsh error-state propagation in nested loads | low |
| epics-base #194 | `CALC$` long string issue | `records/scalcout.rs` long-string handling; bounded under our `String` type | low |
| asyn #34 | `asynPortDriver` segfault on duplicate port name | `PortManager::register_port[_with_config]` now returns `Result` and rejects duplicate names with `AsynError::PortAlreadyRegistered`. Tests: `duplicate_port_name_rejected`, `duplicate_after_unregister_succeeds` | **applied** |
| asyn #80 / #56 | Deadlocks / race with `asyn:READBACK` on output records | adapter callback path; intersects with the deferred #208/#60 work | medium (depends on #208/#60) |
| asyn #170 | Parallel callback queue overflow | `interrupt.rs` mailbox model: never drops, intermediates coalesce into a `latest` slot; broadcast lane (legacy) still has tokio capacity. Test: `mailbox_burst_coalesces_no_drop` (1 000 events → 1 observable, `coalesce_count == 999`) | inspected, equivalent (regression-guarded) |
| asyn #99 | "main" blocks for `asynPortDriverCallback` thread to exit | port_actor join semantics on shutdown | low |
| asyn #220 | `pasynOctetSyncIO->read` overwrites `pasynUser->timeout` | `SyncIOHandle::timeout` is `Copy + Duration`; `read_*`/`write_*` take `&self`. `pub fn timeout(&self) -> Duration` getter added; test `read_write_does_not_mutate_timeout_field` asserts the contract through 10× r/w rounds | inspected, equivalent (regression-guarded) |
| asyn #224 | autoConnect connects too early (before all init) | `port_actor.rs` startup ordering | medium (audit) |
| asyn #105 / #93 | vxi11 buffer overflow / std::vector overrun | not in asyn-rs (no vxi11 driver) | inspected, n/a |

### Audit-round outcome

This round closed a substantial fraction of the audit list:

- **applied**: asyn #34 duplicate port name (commit `bb080e0`)
- **inspected, equivalent (regression-guarded)** — verified by a new
  reproducer test pinning the contract:
  - epics-base #477 — `drop_aborts_read_and_write_tasks` now asserts
    the abort cascade completes within 500 ms; a regression toward the
    upstream 30 s symptom would fail immediately.
  - asyn #220 — `read_write_does_not_mutate_timeout_field` asserts the
    timeout-preservation contract through 10× r/w rounds.
  - asyn #170 — `mailbox_burst_coalesces_no_drop` asserts 1 000 events
    → 1 observable + `coalesce_count == 999`.
- **inspected, equivalent** (no test required — type / module choice):
  - epics-base #455 — monotonic `Instant` everywhere; `SystemTime` only
    in non-comparison snapshots.
- **inspected, n/a** (precondition absent in epics-rs):
  - epics-base #97 — `aai` record is not implemented; the base segfault
    depended on a fixed-size NELM dst buffer that doesn't exist in our
    `Vec<f64>`-based output.
  - asyn #105 / #93 — no `vxi11` driver in asyn-rs.

Remaining audit rows that still need an environmental or integration
reproducer (deferred for individual follow-up):

- epics-base #438 (ACF reload hang) — needs introspection
  `/reload-acf` exercise loop.
- epics-base #190 (CA stalled after suspend) — needs `pmset` /
  `systemctl suspend` test harness.
- asyn #224 (autoConnect connects too early) — needs a startup race
  scenario with deliberate delay-injection.
- asyn #80 / #56 (asyn:READBACK race) — depends on #208/#60 work.

## K. Closed-unmerged PRs (audit closure)

`gh pr list --state closed --search "is:unmerged"` for both repos
(epics-base ≈ 200 entries; asyn ≈ 30). The dominant outcome is
**superseded by a later merged PR** — those rows fold into Sections G
or H by the merged PR's number, no extra audit needed. The remainder
are abandoned drafts, rejected redesigns, or build/CI proposals that
never landed.

### Already covered (superseded by merged PRs in earlier sections)

| Closed-unmerged | Final fate |
|---|---|
| epics-base #570 | superseded by **#571** (Recursion bug v2 — Section G) |
| epics-base #424 | superseded by **#432** (db event double cancel — Section G) |
| epics-base #326 | superseded by **#324**-fix path (Section A/B) |
| epics-base #287 | superseded by tracked open issue **#284** (Constant `INP*` to aSub — Section I) |
| epics-base #196 | superseded by tracked open issue **#183** (DBF_MENU→DBF_STRING — Section I) |
| epics-base #195 | superseded by tracked open issue **#194** (`CALC$` long string — Section I) |
| epics-base #425 | superseded by tracked open issue **#426** (CA nameserver + CA_V413) |
| epics-base #464 | superseded by tracked **#505** (record deletion at DB creation — Section G) |
| epics-base #500 | superseded by **#501** (asTrap dbChannel) |
| epics-base #263 | superseded by **#359** (Undefined ts on NORD first update — Section A) |
| asyn #210 / #182 / #5 / #25 / #22 / #54 / #47 / #48 / #133 | superseded by later merged variants on the same defect class |

### Stability-relevant closed-unmerged that map to existing epics-rs equivalents

| PR | Topic | epics-rs status |
|---|---|---|
| epics-base [#335](https://github.com/epics-base/epics-base/pull/335) | Treat `""` as unset for links | `link.rs:149` already returns `ParsedLink::None` on empty inner — equivalent |
| epics-base [#232](https://github.com/epics-base/epics-base/pull/232) | Add `FMOD` to calc | `calc/engine/numeric.rs::CoreOp::Fmod` already implemented — equivalent |
| epics-base [#152](https://github.com/epics-base/epics-base/pull/152) / [#151](https://github.com/epics-base/epics-base/pull/151) | Race in `db_close_events()` | epics-rs uses `subscribers.lock()` + `is_closed()` retain pattern — equivalent class as the dbEvent cancel races already covered |
| epics-base [#185](https://github.com/epics-base/epics-base/pull/185) | epicsTime rework | epics-rs time conversion lives in `runtime::general_time` + `SystemTime`/`Instant` — equivalent (different model) |
| epics-base [#379](https://github.com/epics-base/epics-base/pull/379) | Discard search requests in CA client | already covered by AIMD search budget + RTT estimator (kodex audit notes) |
| epics-base [#225](https://github.com/epics-base/epics-base/pull/225) | caRepeater socket-loss in `register_new_client` | epics-ca-rs uses in-process repeater fallback; the C socket-list manipulation has no analog |
| epics-base [#311](https://github.com/epics-base/epics-base/pull/311) | sub record returns error on bad INP | `sub_record.rs` link-read failure path — verify on next audit pass |
| epics-base [#283](https://github.com/epics-base/epics-base/pull/283) | `iocShutdown` always stop worker threads | tokio task abort cascade on Drop (commit acfa608 covers `CaClient`/`ServerConnection`) |

### Stability-relevant closed-unmerged that point at gaps NOT in earlier sections

| PR | Topic | epics-rs site |
|---|---|---|
| epics-base [#336](https://github.com/epics-base/epics-base/pull/336) | Validate target record name on alias | `db_loader::parse_db` now parses `alias("name")` inside record bodies, runs each through `validate_record_name`, attaches to `DbRecordDef::aliases`. `PvDatabase::add_alias / resolve_alias` + alias-aware lookup in `find_entry_no_resolve` / `has_name_no_resolve`. iocsh `dbLoadRecords` registers each parsed alias post-`add_record`. Tests: 3 cases | **done** |
| epics-base [#618](https://github.com/epics-base/epics-base/pull/618) | TLS + cert-based access security | epics-ca-rs has the `cap-tokens` feature + signed beacons; pvxs-style cert ACF subject matching is **partial** (signed-beacon verifier, no TLS-cert ACF gateway) |
| epics-base [#563](https://github.com/epics-base/epics-base/pull/563) | ACF METHOD/AUTHORITY + YAML | epics-rs ACF reads the legacy text format only; YAML + METHOD/AUTHORITY extensions deferred |
| epics-base [#449](https://github.com/epics-base/epics-base/pull/449) | dbLoadTemplate error propagation | `db_loader::dbLoadTemplate` (if implemented) error path — verify |
| epics-base [#677](https://github.com/epics-base/epics-base/pull/676) | dfanout IVOA | already covered by Section A #688 dfanout improvements |
| epics-base [#502](https://github.com/epics-base/epics-base/pull/502) | dbAllocRecord buffer overflow fix | C-only; epics-rs allocates `Box<dyn Record>` per record so no equivalent overflow path |
| asyn [#210](https://github.com/epics-modules/asyn/pull/210) | TCP connect long-timeout fix (closed) | superseded by current asyn-rs `connect_timeout` policy — verify under audit |

### Rest

The ~150 remaining closed-unmerged entries on epics-base and ~20 on
asyn are: build/CI churn, doc/POD work, RTEMS / VxWorks / Windows /
macOS host-specific tweaks, codeathon-tagged style cleanups,
deprecated-language-feature fixes (auto_ptr → unique_ptr, etc.), or
abandoned drafts. All N/A for epics-rs.

### Closure

PR audit horizon now covers:

- **All merged PRs**, both repos (Sections A, G, H + roll-up).
- **All open PRs**, both repos (Sections B + Section H asyn rows).
- **All closed-unmerged PRs**, both repos (this section).
- **All open issues**, both repos (Sections B/C).
- **All closed issues**, both repos (Section I).

GitHub PR / issue history begins at `2017-01-01` for epics-base and
`2015-01-01` for asyn — earlier patches predate the migration and live
on the upstream Launchpad mirrors.

## L. Launchpad bug tracker (epics-base)

epics-base maintains a separate bug tracker on Launchpad
(<https://bugs.launchpad.net/epics-base>). asyn is GitHub-only
(`https://bugs.launchpad.net/asyn` returns 404). The Launchpad bug
list is **113 entries total**, oldest #541180 (2010), newest
#2052814 (2024). Many GitHub PRs and issues already cite Launchpad
bug numbers; this section is the authoritative roll-up.

Audit method: pulled the full list (75 + 38 across two pages),
then deep-dived the four entries that point at potentially
production-affecting stability defects: **#1686787, #1577761,
#1722540, #739789**.

### Spot-checked stability defects

| LP bug | Defect | epics-rs status |
|---|---|---|
| [#1686787](https://bugs.launchpad.net/epics-base/+bug/1686787) | Zero-length PV name in UDP search → infinite loop in `casDGClient::processDG()`; gateway "runs away" | `epics-ca-rs/src/server/udp.rs` advances the outer parse loop by `offset += msg_len` regardless of PV-name content; an empty name takes the `has_name("") == false` branch and falls through. No infinite-loop path | inspected, equivalent |
| [#1577761](https://bugs.launchpad.net/epics-base/+bug/1577761) | Record-type **attributes** are per-type static values without locking → race when accessed concurrently | epics-rs records are per-instance `Box<dyn Record>`; the C "per-type attribute" concept does not exist as a separate shared mutable surface. The `Record` trait's static methods (e.g. `record_type()`) return immutable `&'static str` | inspected, equivalent (different model) |
| [#1722540](https://bugs.launchpad.net/epics-base/+bug/1722540) | Undefined `ao` record processed after IOC reboot wrongly resets `UDF` to false (base unconditionally sets `udf = isnan(val)`) | `records/ao.rs::process` does not touch `udf` directly; UDF is cleared by `record_instance.rs::process_local` via the `Record::clears_udf()` trait method default. Need to verify `clears_udf()` for `ao` matches base's "UDF stays true until set explicitly" semantic — open audit task | medium (audit) |
| [#739789](https://bugs.launchpad.net/epics-base/+bug/739789) | TCP name-resolver `sendQue.pushString` in libca grows unbounded when the TCP peer is unresponsive — process memory leak under nameserver stall | epics-rs had the **same defect class** in `client/search.rs::run_search_engine`: `nameserver_send_txs: Vec<mpsc::UnboundedSender<Vec<u8>>>` with three `let _ = ns_tx.send(...)` call sites in `fire_searches`. Now bounded: `mpsc::Sender` capped by `EPICS_CA_NAMESERVER_QUEUE_DEPTH` (default 256), with a new `ns_try_send` helper that drops + bumps the `ca_client_nameserver_queue_drops_total` counter on full. Tests: `nameserver_queue_drops_when_full_no_leak`, `nameserver_queue_handles_closed_receiver` | **applied** |

### Already-mapped or duplicates of GitHub items

| LP bug | GitHub equivalent | Status |
|---|---|---|
| #2052814 dbLoadRecords macros with defaults fail | merged PR **#463** | Section H — open audit |
| #2031563 JSON input link with calc→pva not working | issue **#421** | Section I gap |
| #2029482 pvget JSON error | covered by **#421** | Section I gap |
| #1841634 CP link triggers lost on async record | issue **#557** | Section J audit |
| #1712363 ao record SIMM=RAW | merged PR **#144** | Section H audit |
| #1573462 Add alarm filtering to AO | feature class — same family as **#817** AFTC for bi/mbbi (done) | partially-applied |
| #1532864 assert(this->pudpiiu) on TCP name resolution | nameserver TCP path — overlaps the lp #739789 fix | covered |
| #1532328 caget slow to fail on missing nameserver | timeout policy — `EPICS_CA_NAME_SERVERS` task already has 5s connect-timeout + exp backoff | inspected, equivalent |
| #1392516 OSI monotonic time source | `tokio::time::Instant` is monotonic by construction | inspected, equivalent |
| #541353 epicsThreadSleepQuantum not accurate | `tokio::time::sleep` precision — different model | inspected, equivalent |
| #1686787 zero-length PV (this section) | covered above | inspected, equivalent |
| #1722540 ao UDF (this section) | covered above | medium (audit) |
| #739789 TCP nameserver leak (this section) | **applied this round** | applied |

### Lower-priority / niche items (~95 entries)

The remaining ~95 Launchpad bugs split into:

- **vxWorks / RTEMS / Windows / win32 specifics** (≈30): #2004463
  iocsh on vxWorks, #1188026 iocLogServer on win32, #772471 win32
  handle leak, #667474 CA tools on vxWorks/RTEMS, #608956 vxWorks
  taskVarLib, #580748 WIN32 thread priority, #663592 NTP broadcasts
  on RTEMS, etc. — N/A in epics-rs.
- **build / makefile / packaging** (≈15): #2015234 genVersionHeader,
  #1399301 makeBaseApp dirs, etc. — N/A.
- **doc / POD / cosmetic** (≈10): #541297 manual error/warning
  language, #1428339 TPRO context — UX, deferred.
- **stability / behaviour items below the spot-check threshold** (≈25):
  #1714447 (CA filter on long strings), #1706589 (MSI #line), #1630958
  (record visibility), #1536987 (CAS log dropped updates), #1495843
  (real-time CA API), #1495417 (final field types), #1462952 (dbpr
  info items), #1424092 (epicsAssert message lost), #1398215 (output
  records write-on-change), #1185928 (alarm monitoring of non-VAL),
  #1182091 (TIME field directly accessible), #1052459 (iocShell
  thread-id check), #954138 (caput truncate warning), #541396 (mbbi/o
  initialize VAL with string), #541371 (assert in dbEvent.c — likely
  superseded), #541350 (monitors on EGU when SEVR changes), #541324
  (beacon "connection refused"), #541180 (numeric bounds on enums) —
  each is a candidate for an individual audit follow-up but no single
  one rises to "must-fix in this round".
- **abandoned or wontfix** (≈15): #1012788 (CA SOCKS support, never
  done), #1052459 (iocShell threadResume), #541318 (poolPoll), etc.

### Audit horizon — final

After this section the upstream-tracking doc covers:

- **GitHub PRs** (epics-base, asyn): merged + open + closed-unmerged
- **GitHub issues** (epics-base, asyn): open + closed
- **Launchpad bugs** (epics-base): all 113 entries

asyn has no Launchpad bug tracker (404). EPICS Base GitHub history
begins 2017-01-01; Launchpad coverage extends back to 2010-09 via
#541180. No older artifact source remains to audit at the public
upstream layer.

## F. Refresh queries (run to update this doc)

```sh
# epics-base merged PRs (last 12 months)
gh pr list --repo epics-base/epics-base --state merged --limit 200 \
  --search "merged:>=$(date -v-1y +%F)" \
  --json number,title,mergedAt,labels,baseRefName

# epics-base open PRs
gh pr list --repo epics-base/epics-base --state open --limit 200 \
  --json number,title,createdAt,labels,baseRefName

# epics-base open issues
gh issue list --repo epics-base/epics-base --state open --limit 300 \
  --json number,title,createdAt,labels

# asyn merged PRs (last 12 months)
gh pr list --repo epics-modules/asyn --state merged --limit 200 \
  --search "merged:>=$(date -v-1y +%F)" --json number,title,mergedAt,labels

# asyn open PRs / issues
gh pr list --repo epics-modules/asyn --state open --limit 200 \
  --json number,title,createdAt,labels
gh issue list --repo epics-modules/asyn --state open --limit 200 \
  --json number,title,createdAt,labels
```
