# Parity Review 10 — iocInit / device support / autosave

Scope: `server/ioc_app.rs`, `ioc_builder.rs`, `pv.rs`, `device_support.rs`,
`snapshot.rs`, `builtin_devices/`, `autosave/`.

C references: `modules/database/src/ioc/misc/iocInit.c`,
`modules/libcom/src/iocsh/initHooks.{c,h}`.

**Autosave reference note:** No synApps `autosave` C source tree
(`save_restore.c`, `dbrestore.c`, `configMenuSubr.c`) is present under
`~/codes`. `epics-modules/` contains only `asyn`, `motor`, `procServ`.
`pyepics/epics/autosave/` is a pyepics Python module, not synApps C.
Autosave findings below are therefore reviewed for **internal correctness
and self-consistency** against documented C-autosave behavior from memory;
they are not line-verified against the C source. Findings that depend on
exact C-autosave semantics are flagged `[unverified-ref]`.

---

## Severity counts

- Critical: 0
- High: 4
- Medium: 6
- Low: 5

---

## HIGH

### H1. iocInit announces no `initHooks` states — entire hook subsystem missing
**Rust:** `server/ioc_app.rs:275-553` (`IocApplication::run`), `ioc_builder.rs:175-361` (`IocBuilder::build`)
**C:** `iocInit.c:123-205` (`iocBuild`), `:255-276` (`iocRun`); `initHooks.h:78-155`

The C IOC fires 13 `initHookAnnounce()` calls during `iocBuild`/`iocRun`
(`initHookAtIocBuild`, `initHookAfterCallbackInit`, `initHookAfterInitDevSup`,
`initHookAfterInitDatabase`, `initHookAfterScanInit`,
`initHookAfterInitialProcess`, `initHookAfterCaServerInit`,
`initHookAfterIocBuilt`, `initHookAtIocRun`, `initHookAfterDatabaseRunning`,
`initHookAfterCaServerRunning`, `initHookAfterIocRunning`, ...). Both Rust
build paths fire **none**. There is no `initHookRegister`-equivalent public
API anywhere in the crate (`rg 'initHook|init_hook'` returns only a comment
in `ioc_app.rs`).

**Impact:** Any port of C code that registers an init hook (autosave itself
registers `initHookAfterInitDevSup`/`initHookAfterInitDatabase` for pass-0/
pass-1 restore; areaDetector plugins, sequencer programs, caPutLog, devIocStats
all use hooks) cannot be wired. The Rust autosave works around this by hard-
coding pass0/pass1 calls into `ioc_app::run` (lines 367-431), but third-party
hook consumers have no entry point. This is a structural feature gap, not just
a missing notification.

### H2. PINI records process *inside* the scan task (post-handoff), not before CA server start
**Rust:** `server/ioc_app.rs:380-489` then handoff at `:521`; PINI actually runs in `server/scan.rs:48-59` / `scan_event.rs:77-113`
**C:** `iocInit.c:195-196` `initialProcess()` runs inside `iocBuild`, *before* `iocRun` starts the CA server (`:264-266 rsrv_run`).

In C the ordering is strict: `initialProcess()` (process all `PINI=YES`
records) → `initHookAfterInitialProcess` → `rsrv_init` →
`initHookAfterIocBuilt` → then `iocRun` flips `interruptAccept` and starts the
CA server. PINI is fully done before any client can connect.

In the Rust port, `IocApplication::run` does device wiring, link-wait, autosave
restore, then hands the database to the caller-supplied `protocol_runner`
(`:521`). PINI records are only processed when a `ScanScheduler`/`ScanSchedulerV2`
`run()` is later invoked *by that runner* (`scan.rs:48-57`, `scan_event.rs:79`).
The CA/PVA server's TCP listener is also started by the same runner. There is
no guaranteed ordering between "CA listener accepts connections" and "PINI
burst complete".

**Impact:** A CA client connecting in the first moments after IOC start can
`caget` a record whose `PINI=YES` initial processing has not yet run — it sees
the UDF/default value instead of the processed value. C guarantees this cannot
happen. Severity High because it is a real init-order divergence affecting
observable values; not Critical only because the window is short and most
clients retry/monitor.

### H3. `after_init_hooks` are collected but `IocApplication::run` never executes them
**Rust:** `server/ioc_app.rs:91` (field), `:193-196` (`register_after_init`), `:510` (moved into `IocRunConfig`)

`register_after_init` lets the caller queue "run after iocInit completes (e.g.
start pollers)" closures. `run()` moves them into `IocRunConfig.after_init_hooks`
(`:510`) and hands the config to `protocol_runner`. `run()` itself never calls
them. Whether they ever fire depends entirely on the external protocol-runner
crate remembering to drain `config.after_init_hooks` — `IocRunConfig` is only
consumed in this file; nothing in this crate executes the vector.

**Impact:** Doc comment on `register_after_init` promises "run after iocInit
completes". If the runner does not explicitly drain the vector (a custom user
runner, per the doc example at `:15-23`, easily would not), the hooks are
silently dropped — pollers never start, hardware never gets polled. Silent
no-op of an advertised API. The `Box<dyn FnOnce>` are also just dropped at the
end of `run` if the runner future does not consume `config`.

### H4. `verify` treats a corrupt save file as empty → false "all match"
**Rust:** `server/autosave/verify.rs:28`
**C autosave:** `asVerify` reports an error on a save file lacking `<END>`. `[unverified-ref]`

```rust
let entries = read_save_file(save_file_path).await?.unwrap_or_default();
```

`read_save_file` returns `Ok(None)` for a file with no `<END>` marker (i.e. a
truncated/corrupt save — `save_file.rs:85-90`). `verify` collapses that `None`
into an empty `Vec` via `unwrap_or_default()`, then iterates zero entries and
`format_verify_report` prints `Summary: 0 match, 0 mismatch, 0 not found, 0
parse errors`.

**Impact:** An operator running `asVerify` against a corrupt `.sav` file is
told everything is fine. The corruption — exactly the condition `asVerify`
exists to surface — is hidden. Should return an error or a distinct
"corrupt/incomplete save file" result. Compare `restore_from_entries`
(`save_set.rs:251-256`) which correctly converts `None` into
`CorruptSaveFile`.

---

## MEDIUM

### M1. `wire_device_support` calls `dev.init()` AFTER `set_record_info`/`apply_record_info`; `IocBuilder` calls them in the opposite order
**Rust:** `ioc_app.rs:582-590` vs `ioc_builder.rs:286-293`

`ioc_app::wire_device_support`:
```
set_record_info(); apply_record_info(); dev.init(&mut *instance.record);
```
`ioc_builder::build`:
```
dev.init(&mut *instance.record); set_record_info(); apply_record_info();
```

The two build paths run device-support setup callbacks in **different orders**.
A driver that reads info-tags inside `init()` works in the IocApplication path
(info applied first) but sees an empty info map in the IocBuilder path (init
runs first). A driver that depends on `set_record_info` having run before
`init` works in IocBuilder but not IocApplication. There is no single owner of
the device-support init contract.

**Impact:** Same `.db` + same driver behaves differently depending on whether
the IOC was built via `IocApplication` (st.cmd) or `IocBuilder` (pure-Rust).
A driver author cannot write one correct `init()`. Pick one canonical order
(C runs device `init_record` after `recGblInitConstantLink`-style field setup;
`set_record_info`+`apply_record_info` are Rust extensions and should precede
`init`).

### M2. `IocBuilder` build path ignores `init()` failures; `IocApplication` path partially handles them
**Rust:** `ioc_builder.rs:286` `let _ = dev.init(...)` vs `ioc_app.rs:590-594`

`ioc_builder.rs:286` discards the `Result` of `dev.init()` with `let _ =`.
`ioc_app.rs:590` captures `init_ok` and at least uses it to decide whether to
clear UDF. Neither path sets a record alarm on device-support init failure;
C `initDevSup`/`init_record` failure marks the record and logs. The IocBuilder
path additionally loses the error entirely (no log line).

**Impact:** A device support whose `init()` returns `Err` (e.g.
`GetenvDeviceSupport::init` rejecting a non-`stringin`/`lsi` record,
`getenv.rs:78-82`) is silently attached anyway in the IocBuilder path with no
diagnostic. The record looks healthy. Should at minimum log, ideally flag the
record INVALID/`badField`-style.

### M3. Triggered save sets are built as `OnChange`, not as a real trigger-PV watcher
**Rust:** `autosave/startup.rs:115-136` (`create_triggered_set` → `SaveStrategy::OnChange`)
**C autosave:** `create_triggered_set(file, trigger_PV)` saves when `trigger_PV` changes/processes. `[unverified-ref]`

C-autosave `create_triggered_set` takes a *trigger PV name* and saves the set
whenever that PV is posted. The Rust `MonitorSetDef` has no trigger-PV field
(`startup.rs:16-21` — only `filename`, `period_seconds`, `macros`), and
`into_builder` maps a triggered set to `SaveStrategy::OnChange { min_interval:
period_seconds }` — i.e. it polls every member PV for any change. The
`SaveStrategy::Triggered` variant (`save_set.rs:21-25`) that *does* take a
`trigger_pv` exists but is unreachable from the iocsh `create_triggered_set`
command.

**Impact:** `create_triggered_set` semantics diverge: instead of "save when
the trigger fires" it becomes "poll all PVs, save on any change". Saves happen
at different times than C; the `period` argument is reinterpreted from
"trigger debounce" to "poll interval". Site st.cmd files copied from a C IOC
will not behave as written.

### M4. `restore_from_entries` uses `put_pv_no_process` — restored values never propagate through links / never re-process
**Rust:** `autosave/save_set.rs:290` (`db.put_pv_no_process`)
**C autosave (dbrestore.c):** `dbPutField`/`dbPutLink` restore — fields with PP, FLNK chains, and `SCAN=PINI` records are processed. `[unverified-ref]`

Restore writes each PV with `put_pv_no_process`. C-autosave's `reboot_restore`
uses `dbPutField` (and for arrays `dbPut`), which honors `PP`, processes the
record if appropriate, and lets PINI/FLNK chains run afterward.

**Impact:** Restoring a `.VAL` that should ripple to OUT links, or restoring
`.SCAN`/`.DOL` and expecting downstream re-evaluation, will not propagate. For
the common "restore a setpoint AO" case `put_pv_no_process` is actually closer
to desired (avoids spurious hardware writes at boot), so this is partly
intentional — but it is an unflagged behavioral divergence from C and breaks
restore of records whose correctness depends on processing. Document the
deliberate choice or offer a process-on-restore mode.

### M5. `.savB` backup copied from the *old* `.sav` only when the old file is valid — first save after corruption loses the backup
**Rust:** `autosave/backup.rs:62-74`

`rotate_backups` returns early (`Ok(())`, no rotation) if the current `.sav`
is missing *or* invalid (`:62-69`). The `.savB`/seq copy only happens for a
valid existing `.sav`. That is correct for not propagating corruption. But
note the consequence with `find_best_save_file` (`:112-135`): if `.sav` is
corrupt at boot, restore falls back to `.savB`. If a save cycle then runs,
`rotate_backups` sees the corrupt `.sav`, skips rotation, and `write_save_file`
overwrites `.sav` with fresh good data — fine. However if the *process crashes
mid-write* the partial `.sav` + unchanged stale `.savB` is the recovery state;
acceptable. The genuine gap: there is **no `.savB` written for the very first
save** (no prior `.sav` exists, `:62` early-returns), so a crash during the
2nd-ever write leaves only one usable file. Minor robustness gap vs C which
keeps `.savB` one cycle behind from the first save onward.

**Impact:** Narrow crash window with reduced backup depth on a fresh IOC.

### M6. Save-file value encoding is autosave-rs–native, not C-autosave wire-compatible
**Rust:** `autosave/save_file.rs:230-276` (`value_to_save_str`), `format.rs:1` (`VERSION = "autosave-rs V1.0"`)
**C autosave:** scalar saved as plain printf form; arrays as `PV @array@ { "v" "v" ... }`. `[unverified-ref]`

Rust writes the *header* with `VERSION = "autosave-rs V1.0"` (not the C
`save/restore V...` banner) and writes arrays as `[v,v,v]` (`save_file.rs:240-275`),
while it *reads* C's `@array@ { ... }` form (`parse_c_array_line`, `:162-181`).
So Rust can restore a C-written `.sav` but a C IOC (or `asVerify` in a C IOC)
cannot read a Rust-written one — the format is read-compatible but not
write-compatible. Strings are double-quoted with `\\`/`\"` escaping which C
does for strings but C does not quote scalar numbers; close enough for numbers.

**Impact:** A site running mixed Rust + C IOCs sharing a save directory, or
migrating, cannot have a C IOC consume Rust-produced `.sav` files. Feature/
compat gap; the `CompatMode` enum in `format.rs:8-14` is defined but unused
(`rg CompatMode` shows no consumer) — the intended C-write mode was never
implemented.

---

## LOW

### L1. `getenv` device support: unset env var leaves stale `VAL` instead of clearing it
**Rust:** `builtin_devices/getenv.rs:108-128`

On `read()`, if the env var is unset, the code returns `Err` ("variable unset")
and explicitly does **not** touch `VAL` (comment `:119-121`). So after an env
var that was set at init is later unset, the record keeps showing the old
value with a READ_ALARM. C `getenv` devsup re-reads each process and would
reflect the current (empty) value. Minor — env vars rarely change at runtime.

### L2. `getenv` `init()` initial read swallows the unsupported-record error path's value write
**Rust:** `builtin_devices/getenv.rs:104`

`init` does `record.put_field("VAL", ...)` for the resolved env value, but if
the env var is unset it writes an empty string and returns `Ok` — so a missing
env var at init produces an empty `VAL` with **no** alarm, whereas the same
missing var on a later `read()` produces a READ_ALARM (L1). Init vs read are
inconsistent. C flags the alarm at init too.

### L3. `Snapshot` carries no `user_tag`/alarm-ack from `ProcessVariable::snapshot`
**Rust:** `pv.rs:216-219`, `snapshot.rs:74-92`

`ProcessVariable::snapshot()` and `notify_subscribers` always build
`Snapshot::new(...)` with `alarm.status=0, severity=0` and `user_tag=0`,
`display=None`. For a bare `ProcessVariable` (non-record PV) there is no alarm
source, so this is acceptable, but `post_alarm` is the only path that injects
severity. A monitor consumer expecting GR/CTRL metadata on a plain PV gets
`None`. Feature gap, not a bug, since record-backed channels use a different
snapshot path.

### L4. `dropped_monitor_events` counter not bumped in `post_alarm` overflow
**Rust:** `pv.rs:268-272` vs `:301-315`

`notify_subscribers` calls `record_dropped_monitor()` when it overwrites an
unconsumed coalesced slot (`:305-312`). `post_alarm` does the identical
overwrite (`:268-272`) but does **not** bump the counter. Alarm events lost to
a slow consumer are invisible to the `dropped_events` metric. Minor diagnostic
inconsistency.

### L5. `AnyChange` triggered mode never fires on the first observed value; `OnChange` skips first cycle
**Rust:** `manager.rs:141-146` (`AnyChange` requires `last_value.is_some()`), `:209` (`OnChange` requires `!last_snapshot.is_empty()`)

Both change-detecting strategies deliberately skip the first poll cycle to
establish a baseline. Defensible (avoids a spurious save at startup), and C-
autosave on-change also needs a baseline. Noted only because combined with M3
(triggered → OnChange) it means a `create_triggered_set` will not save until
*after* its first `period` has elapsed AND a change is seen — two cycles of
latency vs a C trigger that fires immediately on the trigger PV.

---

## Notes / non-findings verified

- `save_file.rs::write_save_file` (`:48-66`) does correct atomic write:
  open RDWR → `write_all` → `sync_all` on the **same fd** → `rename` →
  parent-dir `sync_all`. The inline comment about a prior RDONLY-reopen bug
  indicates this was already hardened. Good — no finding.
- `request.rs` include handling has depth limit (`MAX_INCLUDE_DEPTH=10`) and
  canonical-path cycle detection (`:213-220`). Correct.
- `macros.rs::expand` handles `$()`/`${}`, `$$`, nested depth, `$(K=default)`,
  and env-var fallback (`:105`). Matches `macEnvExpand` semantics. Good.
- `dedup_entries` (`request.rs:330-341`) keeps last occurrence — matches
  C-autosave "later definition wins". Good.
- `pv.rs` subscriber cap + dead-sender reaping (`:333-372`) is sound.
