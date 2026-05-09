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
