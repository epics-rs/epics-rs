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
| [#812](https://github.com/epics-base/epics-base/pull/812) | 2026-03 | iocsh `dbCreateRecord` (runtime record creation) | `crates/epics-base-rs/src/server/iocsh/commands.rs` | not started |
| [#831](https://github.com/epics-base/epics-base/pull/831) | 2026-03 | caRepeater run-time debug switch | `crates/epics-ca-rs/src/repeater/` | not started |
| [#788](https://github.com/epics-base/epics-base/pull/788) | 2026-02 | Avoid overreporting available CPUs (cgroup/affinity) | tokio runtime worker thread sizing in `epics-rs` runtime entry | not started |
| [#742](https://github.com/epics-base/epics-base/pull/742) | 2025-11 | `aao`, `aai` VAL field type `DOUBLE[]` → `FTVL[]` | aao/aai record VAL polymorphism in `crates/epics-base-rs/src/server/records/` | not started |
| [#732](https://github.com/epics-base/epics-base/pull/732) | 2025-11 | `normativeTypes` NTNDArray::getValueSize fix | `crates/epics-pva-rs/src/nt/nd_array.rs` | inspected, n/a (no equivalent helper; negation pattern audit clean) |
| [#711](https://github.com/epics-base/epics-base/pull/711) | 2025-10 | Allow CA clients to determine server protocol version | `crates/epics-ca-rs/src/client/` server-version exposure | not started |
| [#708](https://github.com/epics-base/epics-base/pull/708) | 2025-10 | Expand dbEvent synchronization | dbEvent path in `crates/epics-base-rs/src/server/db/event*` | not started |
| [#689](https://github.com/epics-base/epics-base/pull/689) | 2025-10 | Better guesses for wrong field names | dbAccess parser error path | not started |
| [#688](https://github.com/epics-base/epics-base/pull/688) | 2025-08 | dfanout record improvements | `crates/epics-base-rs/src/server/records/dfanout.rs` | not started |
| [#678](https://github.com/epics-base/epics-base/pull/678) | 2025-11 | hex/octal strings in dbPut/dbGet | dbPut/dbGet string-to-numeric parser | not started |
| [#655](https://github.com/epics-base/epics-base/pull/655) | 2025-08 | Extend `calc` inputs A–L → A–U | `crates/epics-base-rs/src/server/records/calc.rs` and `calcout.rs` | done — engine `CALC_NARGS = 21`, lexer A..U / AA..UU, `CalcRecord` + `CalcoutRecord` carry INPM..INPU + M..U + LM..LU; tests `test_extended_inputs_m_through_u`, `test_full_a_to_u_sum`, `test_double_letter_uu_parses` |
| [#636](https://github.com/epics-base/epics-base/pull/636) | 2025-06 | `EPICS_DB_INCLUDE_PATH` for dbLoadTemplate | dbLoadTemplate in epics-base-rs | not started |
| [#626](https://github.com/epics-base/epics-base/pull/626) | 2025-06 | `dbgrep` → `dbglob` (alias kept) | iocsh commands | not started |
| [#608](https://github.com/epics-base/epics-base/pull/608) | 2025-10 | Warn to stderr when discarding CPP modifier for outlink | dblink/outlink parser | not started |
| [#558](https://github.com/epics-base/epics-base/pull/558) | 2025-06 | iocsh `afterIocRunning` (≠ atInit/afterInit) | iocsh commands | not started |
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
| [#507](https://github.com/epics-base/epics-base/pull/507) | iocsh local variables | iocsh interpreter | not started |
| [#503](https://github.com/epics-base/epics-base/pull/503) | caget tolerates partial disconnect | `caget` tool behavior | not started |
| [#497](https://github.com/epics-base/epics-base/pull/497) | iocsh `pushd`/`popd`/`dirs` | iocsh commands | not started |
| [#475](https://github.com/epics-base/epics-base/pull/475) | asLib type-safety incompat | ACF library | not started |
| [#459](https://github.com/epics-base/epics-base/pull/459) | iocsh history/file size limit | iocsh history | not started |
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
| [#218](https://github.com/epics-modules/asyn/issues/218) | Add `getLimits` to interfaces | `interfaces/` trait surface |
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
| [#505](https://github.com/epics-base/epics-base/pull/505) | 2024-08 | Allow record deletion at database creation | `db_loader` does not expose record-delete primitive | **not started** |
| [#501](https://github.com/epics-base/epics-base/pull/501) | 2024-10 | `asTrap` serverSpecific is `dbChannel` | ACF trap pipeline does not expose dbChannel handle | **not started** |
| [#486](https://github.com/epics-base/epics-base/pull/486) | 2024-05 | `printf` record `sizv` fix | `records/printf.rs` — verify SIZV field bound | **not started** (audit) |
| [#468](https://github.com/epics-base/epics-base/pull/468) | 2024-05 | compress record fix | `records/compress.rs` — verify reset/N=0 handling | **not started** (audit) |
| [#467](https://github.com/epics-base/epics-base/pull/467) | 2024-06 | Off-by-one in constant link fetch | `record/link.rs::ParsedLink::Constant` — verify offset/length math | **not started** (audit) |
| [#463](https://github.com/epics-base/epics-base/pull/463) | 2024-02 | `dbLoadRecords` allows macros with defaults without substitutions | `db_loader::dbLoadRecords` — verify macro-default semantics | **not started** (audit) |
| [#592](https://github.com/epics-base/epics-base/pull/592) | 2025-03 | `dbServerStats()` API | introspection module exposes counters; full `dbServerStats` shape pending | **not started** |
| [#594](https://github.com/epics-base/epics-base/pull/594) | 2025-03 | `initHookRegister` idempotent + MustSucceed | epics-rs init hook registration — verify idempotency | **not started** (audit) |
| [#581](https://github.com/epics-base/epics-base/pull/581) | 2025-02 | Post monitors from compress record on reset | `records/compress.rs` reset path — verify monitor fan-out | **not started** (audit) |
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
| [#208](https://github.com/epics-modules/asyn/pull/208) | 2024-06 | Fix `mbboDirect` asyn:READBACK | `asyn_record/` — verify mbboDirect readback path | **not started** |
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
| [#193](https://github.com/epics-base/epics-base/pull/193) | 2022-01 | Clear `IP_MULTICAST_ALL` on Linux (drop unrelated multicast) | UDP responder socket options in `epics-ca-rs/src/server/udp.rs` and `epics-pva-rs/src/server_native/udp.rs` | **not started** — kernel default subscribes to all groups, can leak unrelated traffic on multi-group hosts |
| [#191](https://github.com/epics-base/epics-base/pull/191) | 2021-08 | `int64in`: fix monitor delta test | `records/int64in.rs` MDEL/ADEL comparison vs i64 vs f64 | **not started** (audit) |
| [#213](https://github.com/epics-base/epics-base/pull/213) | 2022-01 | Hex literals in hardware links (e.g. `@dev 0xFF`) | `record/link.rs` hardware-link parser does not split hex args | **not started** — hardware-link forms not parsed at all yet |
| [#144](https://github.com/epics-base/epics-base/pull/144) | 2022-05 | Add `SIMM=RAW` to ao records | `records/ao.rs` SIMM stored but RAW menu not handled distinct from VAL | **not started** (audit) |
| [#114](https://github.com/epics-base/epics-base/pull/114) | 2021-03 | Post array events against the field pointer, not the array pointer | `record_instance.rs::notify_field_written` keys events by field name, equivalent in spirit | inspected, equivalent |
| [#112](https://github.com/epics-base/epics-base/pull/112) | 2021-02 | Limit auto-declaration of record types to `regRecDevDrv` only | `db_loader::register_record_type` requires explicit registration — equivalent | inspected, equivalent |
| [#99](https://github.com/epics-base/epics-base/pull/99) | 2021-03 | Remove `dbfl_type_rec` (legacy direct-record dbflag) | epics-rs has no dbfl_type_rec equivalent — clean by construction | inspected, n/a |
| [#86](https://github.com/epics-base/epics-base/pull/86) | 2021-01 | Add JSON5 support (trailing commas, hex, comments in db files) | `db_loader` JSON parsing — current parser is strict JSON; JSON5 features (trailing commas, hex literals, comments inside `{}`) likely missing | **not started** — verify under JSON-rich db (asub `info`, link options) |
| [#78](https://github.com/epics-base/epics-base/pull/78) | 2020-07 | Restrict character set for record names (`[A-Za-z0-9_-:.[]<>;]`) | `db_loader` no explicit name validation — may accept names that base would reject | **not started** |

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
| [#66](https://github.com/epics-modules/asyn/pull/66) | 2017-12 | `devEpics`: ASLO/AOFF/SMOO conversion on ai/ao float64 | adapter `asyn_record/` — these slope/offset/smoothing transforms are not applied | **not started** |
| [#60](https://github.com/epics-modules/asyn/pull/60) | 2017-11 | Process output records on `asyn:READBACK` callbacks | adapter does not implement `asyn:READBACK` info-tag handling | **not started** (related to #208 from later) |
| [#13](https://github.com/epics-modules/asyn/pull/13) | 2016-02 | `asynOption` interface on drvAsynIPPort | `asynOption` registration in `interfaces/` — partial | **not started** (audit) |
| [#6](https://github.com/epics-modules/asyn/pull/6) | 2015-10 | `drvAsynIPPort`: configurable disconnect on read timeout | transport read-timeout policy in asyn-rs — verify configurable | **not started** (audit) |

### Section G+H roll-up — substantive items added to backlog

| Class | Items | Reason for own-PR scope |
|---|---|---|
| Alarm-message string | #568 AMSG / #566 NAMSG | new field on `CommonFields`, MS-link plumbing, monitor metadata extension |
| JSON5 db parsing | #86 | parser switch + extensive db-file regression suite |
| Hardware-link parsing | #213 hex in HW links + the entire `@dev arg` form | epics-rs has no HW-link grammar yet |
| `IP_MULTICAST_ALL` socket option | #193 | per-socket option set during UDP bind in CA + PVA |
| ASLO/AOFF/SMOO on asyn | #66 | new transform layer in `asyn_record/` adapter |
| asyn:READBACK | #208, #60 | output-record process-on-callback wiring in `asyn_record/` |
| Record-name validation | #78 | parser-side gate + regression for legacy databases |

### epics-base pre-2020 (2017-2020)

Mostly POD/doc work (#33 compressRecord, #31 waveform/menuFtype, #43-48
mbbi/mbboDirect/permissive/state/stringin POD); behaviourally relevant:

- **#25 (2019-03)**: stripped-down fix for Launchpad bug 1816841 — record
  monitor lockup under specific event delivery race. epics-rs uses
  `tokio::mpsc` per subscriber + coalesce slot, no equivalent path.
  Inspected, n/a.

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
