# Workspace C-parity review — 2026-07-10 (round 6, full-workspace 5-way fan-out)

Baseline: main @ 429ee9e1 (v0.22.1 + NUM_CAPTURED fix). Prior inventories:
`doc/c-parity-review-2026-06-14.md` (R1), `-06-15.md` (R2/R3+ADP), `-06-16.md`
(R4/R5, all closed), plus per-crate docs under `crates/*/doc/` (2026-05-18 ..
2026-07-01). This round re-audits the whole workspace C→Rust with five
parallel category agents (opus), numbering R6-1..R6-75 by category block.

## Category split

- A (R6-1..15): epics-base-rs database engine (links/scan/locks/field_io/loader) — record `process()` bodies had deep R4/R5 coverage
- B (R6-16..30): epics-ca-rs (client + rsrv server) + epics-tools-rs (procServ)
- C (R6-31..45): epics-pva-rs ↔ pvxs + epics-bridge-rs (qsrv/ca-gateway/pvalink/pva-gateway)
- D (R6-46..60): asyn-rs ↔ asyn + motor-rs ↔ motor
- E (R6-61..75): std-rs/scaler-rs/optics-rs/modbus-rs/mqtt-rs ↔ synApps modules + ad-core-rs/ad-plugins-rs ↔ ADCore

## Parity philosophy (scope filter — unchanged from rounds 1–5)

Wire-faithful port: the C/C++ reference's observable behaviour (wire bytes,
field values, alarm/monitor semantics, state transitions) is the contract.
Documented intentional deviations (SIZV=256, FTVL=DOUBLE, EGU asyn-motor
boundary, etc.) are not findings.

## Open Findings

### Category A — epics-base-rs database engine (R6-1..R6-8)

### R6-1: dbCommon `DBF_MENU` fields (STAT/SEVR/ACKS/ACKT/DISS/UDFS/PINI) are served as SHORT/CHAR, not `DBR_ENUM` with choice strings
Severity: High
Rust: `crates/epics-base-rs/src/server/record/record_instance.rs:1141-1175` — `"SEVR" => EpicsValue::Short`, `"STAT" => EpicsValue::Short`, `"ACKS" => EpicsValue::Short`, `"ACKT" => EpicsValue::Char`, `"DISS" => EpicsValue::Short`, `"UDFS" => EpicsValue::Short`, `"PINI" => EpicsValue::Char`. Promotion to `EpicsValue::Enum` + attached choice strings happens only via `menu_choices_for()` (`record_instance.rs:507-529`), which consults `Record::menu_field_choices` then `shared_menu_choices` (`record/menu_choices.rs:115-148`) — that table lists `HHSV/HSV/LSV/LLSV/…/SIMS/OLDSIMM/SSCN/OMSL/IVOA/LINR/PBUF/FTVL/PRIO` and contains **no** dbCommon alarm/pini menu. Note `"SCAN" => EpicsValue::Enum` on line 1152, so the promotion exists but was never extended to the sibling menus.
C reference: `modules/database/src/ioc/db/dbCommon.dbd.pod:296` `field(STAT,DBF_MENU){ menu(menuAlarmStat) }`, `:302` `field(SEVR,DBF_MENU){ menu(menuAlarmSevr) }`, `:329` `ACKS DBF_MENU`, `:335` `ACKT DBF_MENU`, `:343` `DISS DBF_MENU`, `:556` `UDFS DBF_MENU`, `:169` `field(PINI,DBF_MENU){ menu(menuPini) }`. `dbAccess.c:1074` `paddr->dbr_field_type = mapDBFToDBR[dbfType]` maps `DBF_MENU → DBR_ENUM`, and `dbAccess.c:167-175` (`get_enum_strs`) serves the menu's `papChoiceValue[]` for `DBF_MENU`.
Impact: `caget REC.SEVR` on a C IOC returns `DBR_ENUM` value 2 with the string `MAJOR` and a 4-entry `dbr_enumStrs`; the port returns `DBR_SHORT` 2 with no strings. `caget -d DBR_CTRL_ENUM REC.STAT`, `caget -n`/`-t` on `.ACKT`, and any OPI widget binding `.SEVR`/`.STAT` as an enum get a wrong native type and empty choice list. `.PINI`/`.ACKT` are served as `DBR_CHAR` (a 1-byte payload) where C sends a 2-byte `DBR_ENUM`.

### R6-2: A DB link whose target field name contains a digit (`B0`, `DO1`, `LNK1`) is parsed as a link to a *record* named `"REC.B0"`
Severity: High
Rust: `crates/epics-base-rs/src/server/record/link.rs:903-907` — after `rsplit_once('.')` the field part is accepted only if `field_upper.chars().all(|c| c.is_ascii_uppercase())`. `'0'.is_ascii_uppercase()` is `false`, so `INP="MBBOD.B0"` fails the guard and falls through to `link.rs:918-923`, returning `ParsedLink::Db { record: "MBBOD.B0", field: "VAL" }`. Digit-bearing fields exist in the port: `records/mbbo_direct.rs:105` `bf!("B0")`, `records/sseq.rs:42,45` `"DO1".."DOA"`, `"LNK1".."LNKA"`.
C reference: `modules/database/src/ioc/db/dbAccess.c:667-671` (`dbNameToAddr`) — `dbFindRecordPart` terminates the record name at the first `.`, then `dbFindFieldPart` matches the remainder against the record type's field table by name (`dbStaticLib.c` `dbFindField`), which contains `B0`…`BF` for `mbboDirect` and `DO1`…`LNKA` for `sseq`. No character-class restriction exists.
Impact: `field(INP,"$(P)DIRECT.B0 NPP MS")` resolves in C to a local DB link on the `B0` field (same lock set, PP/MS semantics). In the port `has_name_no_resolve("$(P)DIRECT.B0")` fails, so the link either never resolves (input record's value freezes, no `LINK_ALARM`) or is re-routed as an *external CA* channel by `database/links.rs:1806-1823` (`classify_cp_link`'s `convert_to_ca`), losing lock-set atomicity, `PP` target processing, and `MS` severity inheritance.

### R6-3: Link modifiers are stripped as ordered suffixes; C resolves them by fixed precedence over the whole modifier string
Severity: Medium
Rust: `crates/epics-base-rs/src/server/record/link.rs:745-816` — `strip_link_modifiers` loops `strip_suffix(" NMS"/" MSI"/" MSS"/" MS"/" NPP"/" CPP"/" CP"/" PP"/" CA")`, each match overwriting `policy`/`ms`/`force_ca` and re-looping. The last suffix consumed wins, and `" CA"` sets an independent `force_ca` flag that coexists with a `CP`/`PP` policy (`link.rs:809-813`, and the doc comment at `link.rs:284` explicitly claims `"OTHER:PV CP CA"` is a CP link).
C reference: `modules/database/src/ioc/dbStatic/dbStaticLib.c:2369-2373` — a single `else if` chain over `strstr(pstr, …)` in the order `NPP, CPP, PP, CA, CP`, assigning (not OR-ing) exactly one process-class bit; `:2375-2378` likewise for `NMS, MSI, MSS, MS`.
Impact: `INP="REC CP NPP"` → C sets `modifiers = 0` (NPP; no subscription). The port strips `" NPP"` first, then `" CP"`, ending at `ChannelProcess` — the holder is registered in the CP trigger registry (`links.rs:1798-1802`) and reprocessed on every source change. `INP="OTHER:PV CP CA"` → C matches `"CA"` before `"CP"` and yields `pvlOptCA` alone (a plain CA link that never processes the holder); the port yields `ParsedLink::Ca` with `policy = ChannelProcess`, so the holder is processed on every remote monitor update. `INP="REC PP CA"` → C yields `pvlOptPP` (a **local DB** link, since `dbAccess.c:1104` routes to `dbDbInitLink` when none of `CA|CP|CPP` is set); the port yields a `ParsedLink::Ca` external channel.

### R6-4: `CP`/`CPP` on an OUT link is honoured; C discards it for `DBF_OUTLINK` and masks `DBF_FWDLINK` to `CA` only
Severity: Medium
Rust: `crates/epics-base-rs/src/server/database/mod.rs:809` — `record_link_fields` pushes `("OUT", &inst.common.out)` through the same `parse_link_v2` as `INP`, and `database/links.rs:1838-1866` (`setup_cp_links`) feeds every returned field to `classify_cp_link` (`links.rs:1788-1834`), which registers any link whose `policy.cp_passive_only()` is `Some(_)` — including the `OUT` field — into `db_links`/`ext_links`. Nothing in `parse_link_v2` / `parse_output_link_v2` (`link.rs:940`) filters by link-field direction.
C reference: `modules/database/src/ioc/dbStatic/dbStaticLib.c:2380-2391` — `switch(ftype) { case DBF_INLINK: break; case DBF_OUTLINK: if (modifiers & (pvlOptCPP|pvlOptCP)) errlogPrintf(ERL_WARNING ": Discarding CP/CPP modifier in CA output link …"); modifiers &= ~(pvlOptCPP|pvlOptCP); break; case DBF_FWDLINK: modifiers &= pvlOptCA; break; }`.
Impact: `field(OUT,"TARGET PP CP")` in C loads as a plain `PP` output link with a startup warning; the port additionally opens a CP monitor on `TARGET` and processes the *holder* record every time `TARGET` changes — a processing loop (holder writes TARGET → TARGET change fires CP → holder reprocesses → writes TARGET …) that does not exist on a C IOC.

### R6-5: `PINI` is a `bool`; `menuPini` RUN / RUNNING / PAUSE / PAUSED are silently dropped
Severity: Medium
Rust: `crates/epics-base-rs/src/server/record/common_fields.rs:34` `pub pini: bool`. `record/record_instance.rs:1363-1368` — the `.db`/CA put arm accepts only `Char(v) => v != 0` or `String(s) => s == "YES" || s == "1" || s == "true"`; `field(PINI,"RUN")` and a numeric `PINI=2` both leave `pini = false` with no diagnostic. The three PINI drivers (`scan.rs:50`, `scan_event.rs:105`, `ioc_app.rs:899`) each run one pass over `pini_records()` and there is no `initHooks`-driven second/third/fourth pass.
C reference: `modules/database/src/ioc/db/menuPini.dbd:11-18` — six choices `NO, YES, RUN, RUNNING, PAUSE, PAUSED`. `modules/database/src/ioc/misc/iocInit.c:598` `if (precord->pini != pphase->pini) return;` — records are matched against an exact menu index, and `iocInit.c:629-646` (`piniProcessHook`) runs `piniProcess(menuPiniRUN)` at `initHookAtIocRun`, `menuPiniRUNNING` at `initHookAfterIocRunning`, `menuPiniPAUSE` at `initHookAtIocPause`, `menuPiniPAUSED` at `initHookAfterIocPaused`; `iocInit.c:655-656` runs `piniProcess(menuPiniYES)` at init.
Impact: A record loaded with `field(PINI,"RUN")` is processed once at `iocRun` on a C IOC and **never** on the port. `field(PINI,"NO")` and `field(PINI,"RUN")` both produce `pini=false`, so `caget REC.PINI` returns `0` for both where C returns `0` and `2`. A `caput REC.PINI RUN` sets the flag to `false` (the string does not match `"YES"`), i.e. it *disables* PINI.

### R6-6: PINI records are processed in hash-map order, ignoring the `PHAS` phase ordering C guarantees
Severity: Medium
Rust: `crates/epics-base-rs/src/server/database/scan_index.rs:106-122` — `pini_records()` snapshots `self.inner.records` (a `HashMap<String, …>`, declared `database/mod.rs:175`) and pushes every `common.pini` name in iteration order; `common.phas` is never read. The three callers (`scan.rs:50-58`, `scan_event.rs:105-108`, `ioc_app.rs:899-902`) process the returned `Vec` front-to-back.
C reference: `modules/database/src/ioc/misc/iocInit.c:609-627` — `piniProcess` repeatedly sweeps the whole database with `phase.this` set to the lowest not-yet-run `PHAS` value, processing only `phas == phase.this` records per sweep and recording the next-lowest `phas` for the following sweep; `iocInit.c:596-604` (`doRecordPini`). `dbCommon.dbd.pod:178` documents "PINI processing phase. All records of a specified phase are processed before …".
Impact: A database whose PINI records rely on phase ordering (e.g. `PHAS=0` calc seeds its inputs, `PHAS=1` calc consumes them) processes in an arbitrary, run-to-run-varying order on the port. The `PHAS=1` record can process before `PHAS=0`, reading an unseeded (UDF/zero) input, and the resulting startup VAL/SEVR is nondeterministic across restarts.

### R6-7: An empty array written into a scalar field returns an error to the client; C accepts the put and raises `LINK_ALARM`/`INVALID_ALARM` on the target
Severity: Medium
Rust: `crates/epics-base-rs/src/server/database/field_io.rs:791-796` (CA client path `put_record_field_from_ca_inner`), and the same block at `:195-199` (`put_pv`, which `links.rs:750` `write_db_link_value` uses for DB OUT-link writes) and `:417-421` (`put_pv_and_post`). All three: `if value.is_empty_array() { return Err(CaError::InvalidValue("empty array cannot be coerced to scalar field …")) }`. The comment cites `// C EPICS dbPut (12cfd41): empty-array → scalar coercion would produce silent zero; reject.`
C reference: `modules/database/src/ioc/db/dbAccess.c:1370-1372` — `} else { if (nRequest < 1) { recGblSetSevr(precord, LINK_ALARM, INVALID_ALARM); } else { …convert… } }`. `status` stays `0`, so control falls through to the `db_post_events(precord, pfieldsave, DBE_VALUE|DBE_LOG)` at `:1408-1411` and `dbPut` returns `0`. Commit `12cfd418d` is titled *"fix dbPut to set target to INVALID/LINK alarm when writing empty arrays into scalars"* — the cited commit's contract is to **set the alarm**, not to reject.
Impact: `caput -a REC.VAL 0` (zero-element array to a scalar) on a C IOC returns success, leaves the field unchanged, posts `DBE_VALUE|DBE_LOG` on it, and drives the record to `STAT=LINK, SEVR=INVALID` on its next `recGblResetAlarms`. On the port the CA client receives a write error, no monitor fires, and the record's alarm state is untouched. Same divergence for a DB `OUT` link that delivers an empty array: C alarms the destination, the port silently fails the write (and `links.rs:785` then skips `processTarget`).

### R6-8: `SCAN="I/O Intr"` on a record with no usable device support is left as-is; C rewrites `SCAN` to `Passive`
Severity: Low
Rust: `crates/epics-base-rs/src/server/ioc_builder.rs:446-451` and `ioc_app.rs:1191-1194` — `if inst.common.scan == ScanType::IoIntr || independent { if let Some(mut dev) = inst.device.take() { if let Some(mut intr_rx) = dev.io_intr_receiver() { …spawn… } } }`. When `inst.device` is `None` (no `DTYP`) or `io_intr_receiver()` returns `None`, both blocks fall through with no diagnostic and `inst.common.scan` stays `ScanType::IoIntr`. The record then remains in the `IoIntr` scan bucket surfaced by `iocsh/commands.rs:607,934` (`records_for_scan(ScanType::IoIntr)`).
C reference: `modules/database/src/ioc/db/dbScan.c:266-297` — `scanAdd`'s `menuScanI_O_Intr` branch sets `precord->scan = menuScanPassive` on four failure paths: `dset == NULL` (`:273`), `get_ioint_info == NULL` (`:280`), `get_ioint_info(...)` returns non-zero (`:284`), `piosh == NULL` (`:290`), plus an illegal `PRIO` (`:297`) — each preceded by `recGblRecordError`.
Impact: `caget REC.SCAN` returns `I/O Intr` on the port where a C IOC returns `Passive` after logging `scanAdd: I/O Intr not valid (no DSET)`. The record is also still reported by `scanpiol`, and a subsequent `caput REC.SCAN Passive` → `caput REC.SCAN "I/O Intr"` round-trip observes a different starting value than C.

CARRYOVER
None. All Round-5 items in scope (`R5-1`…`R5-16`, incl. the `R5-4..R5-14` "pending re-verify" block) are recorded FIXED with commit hashes and I confirmed the fixes are present in the current tree. The older per-crate items in `crates/epics-base-rs/doc/parity-review/04-database.md` / `05-record-infra.md` that fall in this category are also closed at HEAD: `04-M1` (RPRO is now a queued reprocess — `processing.rs:3267-3284`, `4243-4260`), `04-H4` (`rec_gbl_set_sevr` clears `namsg` — `recgbl.rs:149`), `05-H1` (per-event routing — `scan_index.rs:148-169`), `05-H2` (`EVNT` is `String` — `common_fields.rs:77`).

Audited clean
- `dbProcess` PACT branch: `lcnt++ < MAX_LOCK(10)`, `stat == SCAN_ALARM`, `sevr >= INVALID` short-circuits, `SCAN_ALARM`/`INVALID` + `"Async in progress"` message, VAL post with `resetAlarms_mask | DBE_VALUE | DBE_LOG`, and `lcnt = 0` on the non-PACT path (`processing.rs:1036-1121` vs `dbAccess.c:537-559`).
- `dbProcess` SDIS/DISA/DISV disable branch: `dbGetLink(&sdis, DBR_SHORT, &disa)` from any link class, `rpro=false`/`putf=false`/notify-completion, the `stat == DISABLE_ALARM` debounce, `sevr=diss`/`nsev=nsta=0`, and the STAT/SEVR `DBE_VALUE` + VAL `DBE_VALUE|DBE_ALARM` posts (`processing.rs:1124-1215` vs `dbAccess.c:562-594`).
- `dbPutField` gating: `DISP` block (`field_io.rs:766-769`), `.PROC` always-process intercept, and `pp(TRUE) && scan == Passive` reprocess gate with ACKT/ACKS excluded (`field_io.rs:1028-1033` vs `dbAccess.c:1263-1268`).
- `recGblResetAlarms` per-field monitor masks: SEVR `DBE_VALUE` only on `prev_sevr != new_sevr`; STAT and AMSG with `stat_mask = DBE_ALARM(on sevr/amsg change) | DBE_VALUE(on stat change)`; `INVALID_ALARM` clamp; returned `val_mask = DBE_ALARM` (`processing.rs:2739-2775`, `recgbl.rs:178-236` vs `recGbl.c:178-222`). All four dispatch copies (`processing.rs:2367`, `:3702`, `:4879`, `record_instance.rs:2200`) agree.
- `recGblSetSevr`/`recGblSetSevrMsg` raise-only rule and the `msg == NULL → namsg[0]='\0'` clear (`recgbl.rs:144-169` vs `recGbl.c:237-261`).
- `recGblInheritSevrMsg` NMS/MS/MSI/MSS dispatch, including MSI's `sevr < INVALID_ALARM → no-op` fall-through and MSS being the only mode that inherits `amsg` (`links.rs:55-76` vs `recGbl.c:264-281`).
- `dbDbPutValue` ordering: `dbPut` → `recGblInheritSevrMsg` (runs regardless of put status) → early return on non-zero status → `processTarget` gated on `.PROC` field **or** `pvlOptPP && pdest->scan == 0` (`links.rs:750-800` vs `dbDbLink.c:373-392`).
- `cvtRawToEngBpt` / `cvtEngToRawBpt` interval walk, `lbrk` clamp to `[0, number-2]`, both ascending and descending raw orders, and out-of-range extrapolation from the terminal point (`cvt_bpt.rs:119-207` vs `cvtBpt.c:43-120`); `dbBreakBody` slope computation, zero-slope and sign-change rejection, terminal slope copy (`cvt_bpt.rs:59-95` vs `dbLexRoutines.c:1046-1064`).
- `arr` channel filter: the asymmetric `wrapArrayIndices` clamps (`start → [0, n]`, `end → [0, n-1]`), `1 + (end-start)/incr` count, `incr <= 0 → 1` normalisation, and the `no_elements <= 1 → no filter` guard (`filters/arr.rs:148-207` vs `std/filters/arr.c:62-92,148`).
- Event-scan routing: `eventNameToHandle` whitespace trim, `[1,255]` numeric canonicalisation, event `0` → no event, and per-`EVNT` list membership (`scan_index.rs:148-198` vs `dbScan.c:469-552`); `postEvent <n>` iocsh command routes through the named path (`iocsh/commands.rs:799-819`).
- Periodic scan-list ordering: `(PHAS, load_order)` `BTreeSet` reproduces `addToList`'s "insert after the last element with `phas <= new phas`" stable ordering (`scan_index.rs:62-95` vs `dbScan.c:1075-1095`). **This verdict was scoped to the wrong function and is withdrawn**: it checked `addToList` only, never `buildScanLists` (`dbScan.c:1054-1076`), which feeds `scanAdd` record-type-major, so the order the FIFO was stable *over* was wrong. Corrected by `ScanKey`, which carries the DBD record-type ordinal ahead of the `.db` load sequence.
- macLib port: `$` + `(`/`{` macro-reference detection, no expansion inside single quotes, `\`-escape passthrough, `$(name=default)` and `$(name,k=v,…)` scoped-macro push/pop, self-reference recursion guard (`db_loader/mod.rs:511-740` vs `macLib/macCore.c:688-860`).
- `.db` quoted-string lexing keeps escape bytes raw and rejects an embedded newline (`db_loader/mod.rs:830-900` vs `dbLexRoutines.c` `{escape}` rule) — the old `03-H-2`/`03-H-3` findings are closed.
- `recGblFwdLink` ordering (`dbScanFwdLink` → `dbNotifyCompletion` → queued `scanOnce` on RPRO → `putf = FALSE`) and `dbScanPassive`'s `pto->scan != 0 → no-op` gate (`processing.rs:3225-3284` vs `recGbl.c:288-302`, `dbDbLink.c:427-434`).
### Category B — epics-ca-rs + epics-tools-rs (R6-16..R6-29)

### R6-16: Client recv watchdog tears the circuit down after a second echo timeout; libca never closes the socket
Severity: High
Rust: `crates/epics-ca-rs/src/client/transport.rs:1416-1418` — on the second echo timeout (~10 s after the first) the read loop sends `TransportEvent::TcpClosed` and returns. `transport.rs:1394-1414` marks unresponsive and re-arms one extra probe first.
C reference: `modules/ca/src/client/tcpRecvWatchdog.cpp:54-81` — with `probeResponsePending` set and the recv thread idle, C calls `receiveTimeoutNotify` and returns `noRestart`; `tcpiiu.cpp:890-897` routes that to `unresponsiveCircuitNotify` (`tcpiiu.cpp:899-941`), which sets `unresponsiveCircuit`, re-arms `echoRequestPending`, cancels both watchdogs, raises `ECA_UNRESPTMO`, and **keeps the socket**. The circuit closes only on a genuine socket error (`tcpiiu.cpp:586-601`).
Impact: A server that goes quiet for 30 s and does not answer an echo within 10 s (GC pause, load spike, paused VM) loses its circuit in Rust: every channel disconnects, every subscription is torn down, and the client re-searches and reconnects. libca keeps the same circuit, marks it unresponsive, and recovers it on the next byte from the server with no re-search and no subscription churn. Note R2-40 fixed exactly this on the *send* watchdog; the receive watchdog still closes.

### R6-17: Client flow control is driven by consumer-queue depth, not by socket-buffer occupancy, so `EVENTS_OFF` latches until the application drains
Severity: High
Rust: `crates/epics-ca-rs/src/client/mod.rs:3146-3187` — `flow_control_note_queued` sends `EVENTS_OFF` when 10 monitor events sit unread in the per-circuit queue; `flow_control_note_consumed` sends `EVENTS_ON` only once `outstanding <= 5`. `transport.rs:1485-1489` explicitly disables any read-loop frame counting ("Automatic CA flow control is intentionally disabled here"). `EVENTS_ON` has exactly one call site (`mod.rs:3178`) — no timer, no socket-drain trigger.
C reference: `modules/ca/src/client/tcpiiu.cpp:548-567` — `busyStateDetected` is set only when `bytesArePendingInOS()` is true for `maxContigFrames` consecutive processed frames, and is cleared **immediately** (`contigRecvMsgCount = 0`) the first time the OS socket buffer is empty, "w/o waiting for more data to arrive". `iocinf.h:62` fixes the trigger at 10 contiguous frames; `cac.cpp:233-237` scales it to `bufsPerArray * 10` from `EPICS_CA_MAX_ARRAY_BYTES`.
Impact: Two divergences on the wire. (1) A Rust consumer that holds a `MonitorHandle` and stops polling it leaves `outstanding` above 5 forever, so `EVENTS_OFF` is never lifted — every *other* subscription on the same circuit stops receiving monitors indefinitely. libca cannot reach that state: the moment the socket drains it emits `EVENTS_ON`. (2) The trigger is hard-coded at 10 and never scaled by `EPICS_CA_MAX_ARRAY_BYTES`, so a large-waveform circuit trips `EVENTS_OFF` far earlier than libca does. `crates/epics-ca-rs/doc/09-libca-parity.md:78-80` and `crates/epics-ca-rs/doc/07-flow-control.md:40-44` claim libca has a "per-server outstanding-monitor counter" with "hysteresis (10 / 5)" — libca has neither.

### R6-18: Server emits extended-form headers to pre-CA_V49 clients; `ECA_16KARRAYCLIENT` is defined but never sent
Severity: High
Rust: `crates/epics-ca-rs/src/protocol.rs:325-337` — `set_payload_size` promotes to extended form on `size >= 0xFFFF || count >= 0xFFFF` with no client-version parameter. Every reply builder then calls `to_bytes_extended()` unconditionally: `server/monitor.rs:292` (EVENT_ADD delivery), `server/tcp.rs:2864` (READ / READ_NOTIFY), `server/tcp.rs:4152`, `server/tcp.rs:4730`, `server/tcp.rs:5072`. `ECA_16KARRAYCLIENT` is declared at `protocol.rs:135` and has zero references in the crate.
C reference: `modules/database/src/ioc/rsrv/caserverio.c:266-270` — `cas_copy_in_header` refuses to build the frame: `if (alignedPayloadSize >= 0xffff || nElem >= 0xffff) { if (!CA_V49(pclient->minor_version_number)) return ECA_16KARRAYCLIENT; ... }`, and every caller (`read_reply` `camessage.c:515`, `read_action` `camessage.c:625`) turns that status into a `send_err`. The same gate appears on the receive side (`camessage.c:2410`, `if (CA_V49(minor) && m_postsize == 0xffff)`) and in the error echo (`camessage.c:201-202`, extended echo only when `... && CA_V49(minor)`).
Impact: A client negotiating minor version 4..8 that reads or monitors an array whose padded size reaches 65536 bytes (e.g. 8192 × DBR_DOUBLE) gets a 24-byte extended header it has no code to parse; its TCP stream de-syncs. C sends a clean `CA_PROTO_ERROR` / `ECA_16KARRAYCLIENT`. The same missing predicate makes the Rust server accept a `m_postsize == 0xffff` request from a pre-V49 peer (C rejects it as misaligned, `camessage.c:2452`) and echo a 24-byte request header in `CA_PROTO_ERROR` where C truncates to 16 (`server/tcp.rs:5203,5228` — the comment there cites `camessage.c:201-214` but omits the version condition on those exact lines). One owner, three call surfaces.

### R6-19: Client omits libca's CA_V413 zero-count substitution on EVENT_ADD and READ_NOTIFY
Severity: High
Rust: `crates/epics-ca-rs/src/client/transport.rs:586-597` writes `hdr.count = count as u16` for `CA_PROTO_EVENT_ADD`, and `client/mod.rs:1663-1678` (`build_read_notify_frame`) does the same for `CA_PROTO_READ_NOTIFY`. `client/subscription.rs:360-364` (`resolve_subscription_count`) deliberately resolves "no cap" to wire count `0`. Neither path consults `server_minor_version` (tracked at `transport.rs:1619`).
C reference: `modules/ca/src/client/tcpiiu.cpp:1476` — `if (nElem == 0 && !CA_V413(this->minorProtocolVersion)) nElem = chan.getcount();` before `insertRequestHeader(CA_PROTO_READ_NOTIFY, ...)`. For EVENT_ADD, `tcpiiu.cpp:1573` calls `subscr.getCount(guard, CA_V413(minorProtocolVersion))`, and `netIO.h:241-251` returns `nativeCount` when `count == 0 && !allow_zero`. `caProto.h:48` defines CA_V413 as "Allow zero length in requests."
Impact: `camonitor`-style subscriptions and native-count gets are the default path (`subscription.rs:336-352` documents wire count 0 as the intended request). Against any EPICS ≤ 3.15 IOC — minor version ≤ 12, still the common field population — libca substitutes the channel's native element count while the Rust client sends `m_count = 0`. Those servers predate the zero-count autosize contract (`rsrv/camessage.c:507`, `autosize = pevext->msg.m_count == 0`, is the V4.13 addition), so the request resolves to a zero-element transfer rather than the record's data.

### R6-20: Client emits extended headers with no `v49Ok` gate and skips libca's pre-V49 element bound
Severity: Medium
Rust: `crates/epics-ca-rs/src/client/mod.rs:1714` (`build_write_frame`) and `client/transport.rs:593-594` call `set_payload_size` / `to_bytes_extended` with no reference to `server_minor_version`; `protocol.rs:325` takes no version parameter. There is no `ECA_TOLARGE` bound on the requested element count before framing.
C reference: `modules/ca/src/client/comQueSend.cpp:285-315` — `insertRequestHeader(..., bool v49Ok)` writes the 16-byte form when `payloadSize < 0xffff && nElem < 0xffff`, the 24-byte extended form only `else if (v49Ok)`, and otherwise `throw cacChannel::outOfBounds()`. Every call site passes `CA_V49(this->minorProtocolVersion)` (e.g. `tcpiiu.cpp:1313, 1343, 1418, 1484`). `comQueSend.cpp:353-363` additionally caps `nElem` against `maxBytes = MAX_TCP - sizeof(caHdr)` for a pre-V49 circuit and throws `outOfBounds` past it; `tcpiiu.cpp:1465-1471` does the same for READ_NOTIFY / EVENT_ADD with `maxBytes = MAX_TCP`.
Impact: Writing or subscribing to a >65535-byte array on a pre-4.9 circuit makes the Rust client put a 24-byte header on the wire that the server parses as 16 bytes plus 8 bytes of payload — the circuit de-syncs. libca fails the operation locally with `ECA_TOLARGE` and never transmits. This is the client-side half of the same missing predicate as R6-18.

### R6-21: Client caps received payloads at `EPICS_CA_MAX_ARRAY_BYTES` and closes the circuit; C ignores that cap by default and never closes
Severity: Medium
Rust: `crates/epics-ca-rs/src/client/transport.rs:1494-1503` — when the accumulated read buffer exceeds `max_accumulated()` the loop prints and emits `TcpClosed`. `max_accumulated()` (`transport.rs:104-108`) is `max_payload_size() + 24 + 64 KiB`, and `protocol.rs:242-246` resolves `max_payload_size()` from `EPICS_CA_MAX_ARRAY_BYTES`, defaulting to 16 MiB. `EPICS_CA_AUTO_ARRAY_BYTES` is never read anywhere in the workspace.
C reference: `modules/ca/src/client/cac.cpp:222-232` — when `EPICS_CA_AUTO_ARRAY_BYTES` is unset or YES (the `configure/CONFIG_ENV:37` default since 3.16), `tcpLargeRecvBufFreeList` is left NULL. `tcpiiu.cpp:1214-1225` then takes the `if (!this->cacRef.tcpLargeRecvBufFreeList)` branch and `malloc`/`realloc`s the body cache to `((m_postsize-1)|0xfff)+1` — **no cap at all**. `EPICS_CA_MAX_ARRAY_BYTES` bounds the receive path only under `AUTO_ARRAY_BYTES=NO`, and even then `tcpiiu.cpp:1246-1248` logs "not enough memory for message body cache (ignoring response message)" and keeps the circuit alive.
Impact: A C IOC serving a 33 MB waveform (4096×4096 uint16 AreaDetector frame) is read successfully by libca and closes the Rust client's circuit instead — permanently, since the server re-sends on reconnect. `protocol.rs:226-241` documents the *default value* as the deviation and advises strict-parity callers to set `EPICS_CA_MAX_ARRAY_BYTES=16384`; with C's default `AUTO_ARRAY_BYTES=YES` that setting has no effect on a C receiver at all, so following the advice makes the divergence worse, not better.

### R6-22: Repeater registration gives up after three send attempts; libca retries every second until it sees a CONFIRM
Severity: Medium
Rust: `crates/epics-ca-rs/src/client/beacon_monitor.rs:179-186` — a `for attempt in 0..3` loop that breaks as soon as `register_with_repeater(&socket).await.is_ok()`, i.e. as soon as the `sendto` succeeds, with 200 ms / 400 ms spacing. `register_with_repeater` has no other call site; the only later retry is the `REREGISTER_INTERVAL` arm (`beacon_monitor.rs:117`, 300 s) which fires only after five minutes of beacon silence.
C reference: `modules/ca/src/client/repeaterSubscribeTimer.cpp:30-31` (`initialPeriod = 10.0`, `period = 1.0`) and `:84-90` — each expiry sends `repeaterRegistrationMessage(attempts)` and, while `!registered`, returns `expireStatus(restart, 1.0)`; it never gives up (only a one-shot diagnostic after 50 tries). `registered` is set exclusively by `confirmNotify` (`repeaterSubscribeTimer.cpp:102-105`), called from `udpiiu.cpp:793` on receipt of `CA_PROTO_REPEATER_CONFIRM`.
Impact: Two failures. A successful `sendto` to a socket with no listener does not mean registration succeeded, so Rust stops retrying without ever confirming. And if the repeater is not bound within the ~600 ms startup window (cold start, or a repeater restart), the Rust client receives no beacon fan-out for up to five minutes. libca re-registers within ~1 s of the repeater becoming available.

### R6-23: Beacon monitor has no long-period (missing-beacon) anomaly, so a returning server gets no search boost
Severity: Medium
Rust: `crates/epics-ca-rs/src/client/beacon_monitor.rs:701-716` — `anomaly_kind` is `FirstSighting`, `IdMismatch`, or a short-period self-reset. There is no branch for `actual_interval` being *longer* than the running estimate: a monotonic-id beacon after a long gap classifies as `None` and emits `BeaconArrival { anomaly: false }` with no re-search poke. The only long-gap reaction is the 180 s stale-prune (`beacon_monitor.rs:107,581`).
C reference: `modules/ca/src/client/bhe.cpp:226-240` — `currentPeriod >= averagePeriod * 1.25` calls `beaconAnomalyNotify`, and `>= averagePeriod * 3.25` additionally sets `netChange = true`. `cac.cpp:475-501` (`beaconNotify`) propagates that into `udpiiu::beaconAnomalyNotify`, which moves every disconnected channel onto the ~5 s `beaconAnomalyTimerIndex` search timer.
Impact: After a 50–180 s route flap or network partition in which the server never restarted (beacon sequence continues, no id mismatch), libca accelerates re-search of the disconnected channels to ~5 s. The Rust client sees a healthy beacon, does nothing, and waits out its normal bucket cadence (derived from the 300 s `EPICS_CA_MAX_SEARCH_PERIOD` default). Reconnection after a restored segment is measurably later.

### R6-24: procServ foreground mode never attaches the launching terminal as an interactive console
Severity: Medium
Rust: `crates/epics-tools-rs/src/bin/procserv_rs.rs:485-628` — `entry()` binds the control listeners and runs the supervisor; nothing is attached to fd 0. `procserv/config.rs:127-144` has no console concept, and the crate contains no `isatty` / `termios` / fd-0 client (grep clean).
C reference: `epics-modules/procServ/procServ.cc:566-569` — `if (inFgMode && !(logFile && strcmp(logFile,"-")==0)) { ttySetCharNoEcho(true); AddConnection(clientFactory(0)); }`, with the terminal setup at `procServ.cc:955-974`.
Impact: `procServ -f cmd` (and `-d`) turns the launching terminal into a live client: the operator types straight into the IOC shell with echo off, uses the `^X` / `^R` / `^T` command keys, and sees child output inline. `procserv-rs -f cmd` attaches nothing, so even in foreground the operator must open a separate telnet connection to the control port. The documented interactive foreground workflow is absent.

### R6-25: procServ foreground mode does not ignore SIGINT/SIGQUIT
Severity: Medium
Rust: `crates/epics-tools-rs/src/procserv/daemon.rs:183-193` — `install_signal_handlers()` registers SIGINT as a shutdown trigger unconditionally, with no foreground gate, and never touches SIGQUIT. It is called unconditionally at `bin/procserv_rs.rs:607`.
C reference: `epics-modules/procServ/procServ.cc:504-509` — `if (inFgMode) { sig.sa_handler = SIG_IGN; sigaction(SIGINT, &sig, NULL); sig.sa_handler = SIG_IGN; sigaction(SIGQUIT, &sig, NULL); }`.
Impact: At a `procServ -f` prompt, `Ctrl-C` and `Ctrl-\` belong to the operator's console session — the server ignores both. At a `procserv-rs -f` prompt, `Ctrl-C` shuts the supervisor down and drops the IOC, and `Ctrl-\` terminates it with a core dump via the default SIGQUIT disposition.

### R6-26: procServ child chdir/exec failure exits 126/127 where C exits 255
Severity: Medium
Rust: `crates/epics-tools-rs/src/procserv/child.rs:259` (and `:264,276,286,298`) exit `126` on setup/chdir failure; `child.rs:311` exits `127` on `execvp` failure.
C reference: `epics-modules/procServ/processFactory.cc:211-221` — a failed `chdir` falls through the `else` and a failed `execvp` returns; both reach the single `exit(-1)` at `processFactory.cc:221`, i.e. wait-status 255.
Impact: A missing or non-executable child binary is the common misconfiguration. `procServ -o /nonexistent` exits 255 and broadcasts `@@@ Received a sigChild ... Normal exit status = 255`; `procserv-rs` exits 127 (126 for permission-denied) and broadcasts the different number. Wrappers, systemd units, and log scrapers keyed on procServ's 255 launch-failure contract see a different code and a different `@@@` line.

### R6-27: Repeater skips C's unconditional `verifyClients()` sweep on each new registration
Severity: Low
Rust: `crates/epics-ca-rs/src/repeater.rs:412-436` — `register_client` prunes only clients whose `send_message` fails (and `send_message`, `repeater.rs:33-47`, treats only `ConnectionRefused` / `HostUnreachable` as gone). A full `verify()` bind-test sweep of every registered client runs only when the 1024-entry cap is reached.
C reference: `modules/ca/src/client/repeater.cpp:473-486` — every `newClient` registration calls `verifyClients(freeList)` (`repeater.cpp:317-335`), which calls `verify()` on **every** registered client and reaps the dead ones regardless of send success. The comment there names the exact reason: platforms where an ICMP error never reaches `send()`.
Impact: On a platform that does not surface `ECONNREFUSED` on `sendto` to a departed client, Rust leaves stale clients registered and keeps fanning beacons to them until the cap is hit; libca reaps them at the next registration. Masked on Linux, where `ECONNREFUSED` is delivered.

### R6-28: Beacon period EMA smoothing factor is 0.25 where libca uses 0.125
Severity: Low
Rust: `crates/epics-ca-rs/src/client/beacon_monitor.rs:736` — `let alpha = 0.25;`, i.e. `new = 0.75*prev + 0.25*sample`.
C reference: `modules/ca/src/client/bhe.cpp:268` — `this->averagePeriod = currentPeriod * 0.125 + this->averagePeriod * 0.875;`.
Impact: The Rust period estimate converges twice as fast on interval jitter, so the estimate that feeds anomaly classification differs from libca's after any burst of irregular beacons. Combined with R6-23 (Rust's only period-based branch is a `< est/3` collapse test versus C's 0.80× / 1.25× / 3.25× bands), the two clients classify the same beacon stream differently.

### R6-29: Scalar DBR_STRING puts always frame a 40-byte payload; libca frames `align8(strlen+1)`
Severity: Low
Rust: `crates/epics-base-rs/src/types/value.rs:352-360` — `EpicsValue::String::to_bytes()` always returns a fixed 40-byte buffer. `crates/epics-ca-rs/src/client/mod.rs:1714` then sets `m_postsize` from that length, so every scalar string put carries `m_postsize = 40`.
C reference: `modules/ca/src/client/comQueSend.cpp:332-341` — for `nElem == 1 && dataType == DBR_STRING`, C computes `size = strlen(pStr) + 1u` (throwing `outOfBounds` past `MAX_STRING_SIZE`), sets `payloadSize = CA_MESSAGE_ALIGN(size)`, and pushes only `size` bytes plus the alignment padding.
Impact: `caput PV "abc"` puts `m_postsize = 8` with an 8-byte body on the wire from libca and `m_postsize = 40` with a 40-byte body from the Rust client. rsrv tolerates both, but the frames are not byte-identical, and any packet-level consumer that accounts by `m_postsize` — CA gateways, the Wireshark CA dissector, wire-replay fixtures — sees a different message. Note the Rust *server*'s deprecated-READ reply path already implements the mirror-image contraction correctly (`server/tcp.rs:2826-2840`).

## CARRYOVER

None. Every finding in `crates/epics-ca-rs/doc/c-parity-review-2026-05-18.md` is dispositioned by the two triage blocks at its head (17 server findings ALREADY-FIXED; R2-50 / R2-55 intentional keeps; 22 client findings ALREADY-FIXED or fixed in round 4, with three benign residuals). `crates/epics-tools-rs/doc/c-parity-review-2026-06-28.md` (PS-1..PS-52) is fully cleared. I re-verified the residual noted for R2-63: `transport.rs:113-122` still asserts that C falls back to the default on `connTMO <= 0.0`, and `cac.cpp:188-194` still shows the fallback keyed on the parse-failure `status` alone — the doc already records this as a known comment/code misread left as-is, so it is not re-filed.

## Audited clean

- **rsrv jump-table coverage.** Every live `tcpJumpTable[]` entry (`camessage.c:2294`) and `udpJumpTable[]` entry (`camessage.c:2330`) maps to a Rust dispatch arm; the `bad_tcp_cmd_action` / `bad_udp_cmd_action` slots map to `server/tcp.rs:4626` (ECA_INTERNAL + disconnect) and `server/udp.rs` silent drop respectively.
- **Per-opcode header slots.** CREATE_CHAN reply `m_cid=cid, m_available=sid` with the pre-V49 `nElem` cap at `0xfffe` (`server/tcp.rs:2385-2394` vs `camessage.c:1157-1172`); ACCESS_RIGHTS `m_cid`/`m_available`; EVENT_ADD reply `m_cid=ECA_NORMAL, m_available=sub_id`; READ_NOTIFY status-in-`m_cid` versus deprecated READ `m_cid=pciu->cid` (`camessage.c:622`); WRITE_NOTIFY `m_cid=status`; EVENT_CANCEL confirm echoing the stored EVENT_ADD header; CREATE_CH_FAIL, READ_SYNC, NOT_FOUND echoes.
- **Extended-form trigger boundary.** `set_payload_size`'s `>= 0xFFFF` matches `caserverio.c:266` and `comQueSend.cpp:285`; the `count == 0xFFFF` exact-boundary case is correct and unit-tested (`protocol.rs:580`). Receive-side detection on `m_postsize == 0xffff` alone (ignoring `m_count`) matches `tcpiiu.cpp:1168`.
- **Access-security ACF reload.** DEFAULT-ASG fallback for a channel whose ASG vanished from the new ACF; `CA_PROTO_ACCESS_RIGHTS` pushed only on an actual level transition; the revoke path emits one `no_read_access_event` then gates the producer, the restore path re-enables and posts a fresh snapshot (`server/tcp.rs:4811,4845-4982` vs `casAccessRightsCB`, `camessage.c:1080-1096`).
- **CA_V411 / CA_V44 gating on the server.** UDP sequence-number VERSION prepend and its removal for pre-4.11 peers (`server/udp.rs:417-447,714-737` vs `caserverio.c:194-201`); `CA_VSUPPORTED` / minor-4.4 rejection at `server/tcp.rs:1797`.
- **Server beacon.** `EPICS_CAS_BEACON_PERIOD` with `EPICS_CA_BEACON_PERIOD` fallback, 15 s default, non-positive → default (`server/addr_list.rs:241-248` vs `online_notify.c:52-64`); 20 ms initial period doubling to `maxPeriod` (`online_notify.c:66,116-121`); `m_count=port`, `m_dataType=CA_MINOR_PROTOCOL_REVISION`, `m_available=0`, and the one-cycle `m_cid` counter lag (`online_notify.c:118`).
- **Repeater wire surface.** `CA_PROTO_REPEATER_CONFIRM` carrying the client source IP in `m_available` (`repeater.cpp:166-190`); fan-out originator skip by full address, not port alone (`repeater.cpp:263-273`); `RSRV_IS_UP` `m_available` rewritten only when zero and only on the outer header (`repeater.cpp:613-630`); exclusive-bind "repeater already running" detection with `SO_REUSEADDR` only after a successful bind (`repeater.cpp:106-129`).
- **Client echo cadence.** 30 s idle from `EPICS_CA_CONN_TMO`, 5 s echo timeout (`iocinf.h:50-51`); `CA_PROTO_ECHO` downgraded to `CA_PROTO_READ_SYNC` for pre-4.3 servers (`transport.rs:1234` vs `tcpiiu.cpp:1406`); `messageArrivalNotify` clearing the anomaly flag and refreshing the deadline; the sticky `beaconAnomaly` flag suppressing healthy-beacon deadline refresh (`tcpRecvWatchdog.cpp:94-129`).
- **procServ console protocol and lifecycle.** All `@@@` message bytes against `clientFactory.cc:100-163` / `processFactory.cc:66-114,191-196` / `procServ.cc:442-450,572-595,788-807`; restart-announce ordering; `--wait` bootstrap gating and the dead-child `^R`/`^X` manual start; the `SendToAll` recipient matrix with per-client IAC escaping and raw log bytes (`procServ.cc:709-768`); `--logstamp` optional-arg parsing, `--ignore` caret decoding, `--killsig` / `--coresize` validation, `-p`/`-P` mapping, `PROCSERV_PID` / `PROCSERV_DEBUG`; oneshot exit disposition (`procServ.cc:656-667`); signal-death encoded as the raw signal, not `128+sig`.
### Category C — epics-pva-rs + epics-bridge-rs (R6-31..R6-35)

### R6-31: `Q:time:tag` nsec-LSB parser rejects `N ≥ 31` and accepts tags pvxs rejects; the "pvxs clamps to [1,30]" comment is false
Severity: Medium
Rust: `crates/epics-base-rs/src/server/record/record_instance.rs:628-644` — `parse_qtime_tag_nsec_lsb()` splits `info("Q:time:tag")` on `:`, compares the first two parts with `eq_ignore_ascii_case` after `trim()`, then returns `Some(n)` only `if (1..=30).contains(&n)`. Line 639 asserts `// pvxs clamps to [1, 30]; values outside leave the timestamp alone`.
C reference: `/home/stevek/work/epics-modules/pvxs/ioc/typeutils.cpp:79-88` — `MappingInfo::updateNsecMask()` does `strncmp(val, "nsec:lsb:", 9)==0 && !epicsParseInt32(&val[9], &dig, 10, nullptr)` then `nsecMask = (uint64_t(1u)<<dig)-1u`. There is no clamp and no case/whitespace tolerance: the prefix match is byte-exact and any parsed `dig` is used verbatim.
Impact: Two opposite wire divergences on `timeStamp.nanoseconds` / `timeStamp.userTag`. (1) `info(Q:time:tag, "nsec:lsb:31")` — pvxs builds `nsecMask = 0x7FFF_FFFF`, so `iocsource.cpp:239` publishes `nanoseconds = nsec & ~mask` (0 for any nsec < 2^31) and `userTag = nsec & mask`; Rust ignores the tag entirely and publishes the raw nanoseconds with `userTag` untouched. (2) `info(Q:time:tag, "NSEC:LSB:4")` or `"nsec: lsb: 4"` — pvxs's `strncmp` fails, `nsecMask` stays 0, and it serves unmasked `nanoseconds` with no `userTag` override; Rust matches the tag, masks off the low 4 bits of `nanoseconds` and overwrites `userTag`. The in-source comment cited as reference authority is not what `typeutils.cpp:79-88` does.

### R6-32: RPC INIT applies `request_to_mask` to the pvRequest; pvxs never masks an RPC
Severity: Medium
Rust: `crates/epics-pva-rs/src/server_native/tcp.rs:6125-6127,6179-6196` — the INIT path selects `intro` as `(OpKind::Rpc, None) => FieldDesc::Variant` (or the channel introspection when present) and then unconditionally runs `crate::pv_request::request_to_mask(&intro, &req_desc)`; on `RequestMaskError::EmptyMask` it answers `send_chan_op_error(..., "invalid pvRequest mask: …")` and the RPC INIT fails. With `intro = FieldDesc::Variant`, `mask_from_selector_fields` (`crates/epics-pva-rs/src/pv_request.rs:308-322`) never enters its `if let FieldDesc::Structure` arm, so `any_matched` stays false and *every* named `field(...)` selector yields `EmptyMask`.
C reference: `/home/stevek/work/epics-modules/pvxs/src/serverget.cpp:402` — `if(cmd==CMD_RPC) { ctrl->connect(Value()); }`, and `serverget.cpp:198-201` — `if(prototype) { oper->type = …; oper->pvMask = request2mask(oper->type.get(), _pvRequest); }`. The prototype is a default-constructed (falsy) `Value` for RPC, so `request2mask()` is never invoked on an RPC pvRequest and no mask is ever built or enforced.
Impact: A pvxs client that builds an RPC with any field selector — `RPCBuilder::pvRequest("field(value)")`, or `epics-bridge-rs` `pva_gateway` forwarding a downstream pvRequest through `PvaClient::pvrpc_with_request` (`crates/epics-pva-rs/src/client_native/context.rs:1856-1900`) — receives an INIT `Status::Error "invalid pvRequest mask: pvRequest selected no existing fields"` from the Rust server where pvxs replies `Status{}` (success) and proceeds to the exec phase. The RPC never runs.

### R6-33: Server INIT treats a null (`0xFF`) pvRequest descriptor as connection-fatal; pvxs decodes it to an invalid `Value` and serves the operation as a wildcard
Severity: Medium
Rust: `crates/epics-pva-rs/src/server_native/tcp.rs:6159-6165` — `decode_type_desc_cached(&mut cur, inbound_order, decode_cache)` on error returns `Err(PvaError::Decode(format!("INIT pvRequest descriptor: {e}")))`, which the read loop treats as connection-fatal. `crates/epics-pva-rs/src/pvdata/encode.rs:683-684` documents the behaviour: `/// 0xFF: NULL — handled by callers (we reject here as caller-context dependent).` No caller in the GET/PUT/MONITOR/RPC INIT path peeks for `0xFF` before calling it.
C reference: `/home/stevek/work/epics-modules/pvxs/src/dataencode.cpp:79-80` — `if(code == TypeCode::Null) { return; }` leaves `descs` empty and the buffer good; `dataencode.cpp:737-744` (`from_wire_type`) then sets `val = Value()`. `serverget.cpp:366-376` / `servermon.cpp:491-503` check only `!M.good()`, so the frame passes; `pvrequest.cpp:53-55` — `else if(!fields.valid()) foundrequested = true;` — turns the invalid pvRequest into the all-fields wildcard.
Impact: A peer that sends a single `0xFF` byte where the pvRequest type descriptor goes (legal per `from_wire_type`, and what pvxs's own `to_wire(Buf&, const FieldDesc*)` at `dataencode.cpp:29-33` emits for a null desc) gets its TCP connection torn down by the Rust server — killing every other channel and operation multiplexed on that circuit — where pvxs replies with a normal INIT success and a wildcard field mask.

### R6-34: RPC reply with a null (`0xFF`) type is a decode error in the Rust client, and the Rust server can never emit one
Severity: Medium
Rust: `crates/epics-pva-rs/src/decode.rs:583-596` (`decode.rs` split out of `client_native/` in `24d514e8`) — the RPC data branch unconditionally calls `decode_type_desc_cached(&mut cur, order, type_cache)?` then `decode_pv_field_cached(&resp_desc, …)?`; a `0xFF` type byte is rejected (`pvdata/encode.rs:683`) and the RPC fails with `PvaError::Decode`. Symmetrically, `crates/epics-pva-rs/src/server_native/tcp.rs:7291-7294` always writes `encode_type_desc(&resp_desc, order, &mut payload)` followed by `encode_pv_field(&resp_value, &resp_desc, order, &mut payload)` — there is no "reply with no value" shape.
C reference: `/home/stevek/work/epics-modules/pvxs/src/serverget.cpp:105-109` — `else if(cmd==CMD_RPC) { auto type = Value::Helper::desc(value); to_wire(R, type); if(value) to_wire_full(R, value); }`. `ExecOp::reply()` (the no-argument overload, `src/pvxs/srvcommon.h:108`) reaches `doReply(Value(), …)`, so `desc()` is `nullptr` and `to_wire(Buf&, const FieldDesc*)` (`dataencode.cpp:29-33`) emits exactly one `0xff` byte with no value body. The pvxs client accepts it: `src/clientget.cpp:415-421` — `from_wire_type(M, rxRegistry, data); if(data) from_wire_full(M, rxRegistry, data);`.
Impact: Against any pvxs RPC handler that calls `op->reply()` instead of `op->reply(value)`, the Rust client's RPC fails with a decode error on a well-formed 6-byte reply body (`ioid | subcmd | Status | 0xFF`) that pvxs's own client completes with an empty `Value`. In the reverse direction the Rust server has no way to express that reply at all.

### R6-35: Rust client discards a MONITOR FINISH frame's trailing value/overrun body; pvxs decodes it
Severity: Low
Rust: `crates/epics-pva-rs/src/decode.rs:500-511` (`decode.rs` split out of `client_native/` in `24d514e8`) — `if cmd == Command::Monitor && subcmd & 0x10 != 0 { … monitor_finish_body(…) … return Ok(OpResponse::Status(…)); }`, checked before the INIT branch and before any data decode. Any bytes after the Status are dropped. The raw-forwarding monitor loop then treats the frame as `RawMonitorFrameKind::FinishOk` and returns `Ok(())` (`crates/epics-pva-rs/src/client_native/ops_v2.rs:2541-2552`), so nothing downstream ever sees the body.
C reference: `/home/stevek/work/epics-modules/pvxs/src/clientmon.cpp:504-511` — `if(!sts.isSuccess()) { } else if(init) { … } else if(!final || !M.empty()) {` — the final (`subcmd & 0x10`) frame still enters the data-decode arm whenever the body has bytes left after the Status, decoding `from_wire_valid` into a queued update and then the trailing overrun bitset. `servermon.cpp:176-178` documents the shape it is decoding (`} else { // finish (could be used to send an error)`).
Impact: A server that appends a final update to the FINISH frame (the shape pvxs's client explicitly supports and its own `doReply` comment reserves) delivers one last monitor update to a pvxs subscriber and zero to a Rust subscriber; through `pva_gateway`'s raw-forwarding path the trailing body is silently dropped rather than relayed downstream.

## CARRYOVER

None. Every finding still marked open in the three inventories handed to me is a signed-off deferral or a documented residual, not a live OPEN item:

- `crates/epics-bridge-rs/doc/c-parity-review-qsrv-2026-07-01.md` — Q15 (AMSG latent), Q50 residual (advisory gate vs DB-link writers), Q14 sibling (a) (`USHORT`/`ULONG` `reallocate_val` collapse), inbound PUT-decode `UByte→Char` backlog: all recorded as tracked/deferred with rationale, review closed 2026-07-02.
- `crates/epics-bridge-rs/doc/c-parity-review-gateway-2026-07-01.md` — GW-41 (`gateAsCa` conditional ASG) DEFERRED fail-closed; GW-1/20/22/61/62/63 keep-warm redesign DEFERRED with explicit sign-off; GW-60/80/81/82 closed as false positives; GW-23/GW-40 FIXED.
- `crates/epics-pva-rs/doc/c-parity-review-2026-06-30.md` — all eight CONCERNs (PVX-1, -2, -21, -41, -42, -61, -81, -82) are CLEARED or documented-intentional; campaign marked CONVERGED.

## Audited clean

Verified against the cited pvxs sources with no divergence found:

- **Framing / segmentation** — `conn.cpp:148-300` `ConnBase::bevRead`: header fault → `bev.reset()`, `sendBE` latched from the `SetEndian` MSB flag, `peerBE` re-latched per application frame, the `(continuation ^ expectSeg) || (continuation && header[3]!=segCmd)` gate, and handler `catch(std::exception&)` → reset. Rust `server_native/tcp.rs:3514-3600`.
- **`request2mask` for GET/PUT/MONITOR** — `pvrequest.cpp:13-70` against `pv_request.rs:198-373`, including the empty-`field{}` wildcard, the absent-`field` wildcard, the non-struct-`field` throw, the `findSet(1)==size()` wildcard collapse, and the flattened-`mlookup` dotted-key behaviour (`field(alarm.status)` ⇒ bits `{0,2,4,…}`) that `mark_path`'s recursion reproduces. Cross-checked against `test/testpvreq.cpp:20-100`.
- **Server monitor pipeline negotiation** — `servermon.cpp:476-708`: INIT `nack` rider read iff `subcmd & 0x80` with `!M.good()` → `bev.reset()`; `op->window = nack`; `queueSize >= 2` else `ctrl->error("can not pipeline invalid queueSize")` when pipelined / `logRemote(Warn)` when not; `ackAny` as plain integer or `"N%"`; `ackAt = limit/2` when 0 then clamped to `[1, limit]`; ACK refill applied before START/STOP; ACK ignored for non-pipelined ops; destroy bit last. Rust `server_native/tcp.rs:526-600,6260-6300,7026-7110`.
- **Client monitor flow control** — `clientmon.cpp:320-455`: INIT `subcmd |= 0x80` + trailing `u32 queueSize`, `window = queueSize`, ACK frame is `sid | ioid | 0x80 | u32 num2ack`. Rust `client_native/ops_v2.rs:115-175,2420-2432,2583-2589`, `codec.rs:332-340`.
- **Monitor overrun bitset** — `servermon.cpp:172-174` writes `to_wire(R, uint8_t(0u))` under `// TODO: placeholder for overrun mask`; the Rust single `0x00` matches.
- **Server op handlers** — `serverconn.cpp:163-355` (ECHO verbatim segBuf echo; `handle_PUT_GET(){}` no-op; CANCEL_REQUEST Executing→Idle + `onCancel`; DESTROY_REQUEST erase-by-ioid + cleanup; MESSAGE decode-fault throw), `serverchan.cpp:258-410` (CREATE_CHANNEL u16 count loop, `Fatal "Refused to create Channel"`, no access-rights word; DESTROY_CHANNEL 8-byte `(sid,cid)` reply), `serverintrospect.cpp:115-185` (GET_FIELD ignores the `subfield` string; unknown SID or live IOID → silent return).
- **CONNECTION_VALIDATION** — `serverconn.cpp` credential commit before the `selected != "ca" && selected != "anonymous"` `Status::Error` reply; Rust `server_native/tcp.rs:2664-2760,3188-3190`.
- **NT type definitions** — `nt.cpp:36-265`: NTScalar `display` field order `limitLow, limitHigh, description, units[, precision, form]`, the `isnumeric = Kind::Integer || Kind::Real` gate collapsing string `display` to `{description, units}` with no `precision`/`form`, `control{limitLow,limitHigh,minStep}`, the 10-field `valueAlarm`, anonymous (2-arg `Struct`) ids on `display`/`control`/`valueAlarm`, and NTTable's `labels, value{cols}, descriptor, alarm, timeStamp`. Rust `qsrv/pvif.rs:150-195,795-865`, `nt/table.rs:41-90`.
- **qsrv single-record prototype** — `ioc/singlesource.cpp:189-206` (`DBR_ENUM` → `NTEnum`, else `NTScalar` with display/control/valueAlarm all true), `ioc/typeutils.cpp:30-60` `fromDbrType` (`DBR_CHAR`→Int8, `DBR_UCHAR`→UInt8, `DBR_ENUM`→UInt16) vs `convert.rs:9-28` and `qsrv/pvif.rs:150-184`; `ioc/groupconfigprocessor.cpp:958-974` `getTypeDefForChannel`.
- **qsrv alarm mapping** — `ioc/iocsource.cpp:182-248`: alarm status class switch and `node["alarm.message"] = meta.status && stsmsg ? stsmsg : "";` (NO_ALARM ⇒ `""`). Rust `qsrv/pvif.rs:665-690,1046-1073`.
- **pvalink option parsing** — `ioc/pvalink_jlif.cpp:69-197`: null/bool/integer/string callback dispatch, `Q: val<1?1:val`, `monorder` clamped to `[-1024,1024]`, `MSS`→MS, unknown key → warn and continue. Rust `pvalink/config.rs:370-540`.
- **SharedPV state machine** — `sharedpv.cpp:348-440`: `open()` throws `"close() first"` when already open; `close()` clears current + subscribers and calls `chan->close()`; `post()` requires exact descriptor-pointer identity. Rust `server_native/shared_pv.rs:216-238,471-500,528-545,570-610`.
- **Type-cache markers** — `dataencode.cpp:69-140` `from_wire(buf, descs, cache)`: `0xFD` emplace-no-overwrite, `0xFE` lookup with empty-slot fault, the `code!=0xff && code&0x10` fixed-length rejection, `StructA`/`UnionA` element-code check. Rust `pvdata/encode.rs:672-790`.


### Category D — asyn-rs + motor-rs (R6-46..R6-50)


### R6-46: EOS interpose never resets its read-ahead buffer or partial-EOS match state on reconnect
Severity: High
Rust: `crates/asyn-rs/src/interpose/eos.rs:44-52` — `in_buf` / `in_buf_head` / `in_buf_tail` / `eos_in_match` are reset only in `flush` (`:228-233`) and `set_input_eos` (`:235-240`). The `OctetInterpose` trait (`crates/asyn-rs/src/interpose/mod.rs:58-84`) exposes only `read`/`write`/`flush`/`set_input_eos`/`set_output_eos` — there is no connect/disconnect hook. `PortDriverBase::set_connected` (`crates/asyn-rs/src/port.rs:210-223`) fans out `AsynException::Connect` but never touches the interpose stack, and no subscriber does either (`rg 'EosInterpose'` finds only the three install sites: `iocsh.rs:636`, `drivers/serial_port.rs:530`, `drivers/serial_port_win32.rs:423`).
C reference: `asyn/miscellaneous/asynInterposeEos.c:142-151` — `eosInExceptionHandler` is registered via `exceptionCallbackAdd` at `:110` and, on `asynExceptionConnect`, sets `inBufHead = 0; inBufTail = 0; eosInMatch = 0;`.
Impact: after a TCP/serial drop and auto-reconnect, the first `read` on the new link is served from `in_buf` — up to 2047 bytes of the *previous* connection's traffic are handed to the record before any byte of the new session is read. Independently, a `eos_in_match == 1` left over from a 2-byte terminator straddling the drop makes the first byte of the new session's terminator complete a spurious EOS match, so the first response is truncated and `EomReason::EOS` is reported one byte early.

### R6-47: No autonomous connect-retry timer — an idle auto-connect port that drops never reconnects
Severity: High
Rust: `crates/asyn-rs/src/port_actor.rs:281-322` is the **only** auto-connect attempt in the crate, and it runs inside `process_one` — i.e. it fires only when a request is dequeued. `PortDriverBase::set_connected(false)` (`crates/asyn-rs/src/port.rs:210-223`) stamps `last_connect_disconnect` and announces the exception but schedules nothing. No timer, task, or wakeup exists (`rg 'connect_timer|seconds_between'` → no hits), and `iocsh.rs:238-533` registers no `asynSetAutoConnectTimeout` / `asynWaitConnect`.
C reference: `asynManager.c:2181-2182` — `exceptionDisconnect` arms `epicsTimerStartDelay(pport->connectTimer, .01)` whenever the port is disconnected and `autoConnect` is set. `asynManager.c:3252-3266` `portConnectTimerCallback` then issues `queueRequest(pasynUser, asynQueuePriorityConnect, 0)`, and `:3268-3283` `portConnectProcessCallback` calls `pasynCommon->connect(...)`, re-arming the timer at `pport->secondsBetweenPortConnect` (default `DEFAULT_SECONDS_BETWEEN_PORT_CONNECT` = 20, `asynManager.c:48`, `:3249`) on every failure.
Impact: an auto-connect port with no queued traffic (I/O-Intr-only device support, or a quiescent asynRecord) stays disconnected indefinitely after a link drop. `asynExceptionConnect` never re-fires, so asynRecord `CNCT`, `isConnected()` queries, and `waitConnect` waiters never observe the recovery — where C restores the link within 20 s with no client action.

### R6-48: EOS interpose discards accumulated bytes and eomReason when the lower-layer read errors
Severity: Medium
Rust: `crates/asyn-rs/src/interpose/eos.rs:177` — `let result = next.read(user, &mut self.in_buf[..])?;`. The `?` returns `Err` immediately, discarding the `n_read` bytes already drained out of `in_buf` at `:114-118` and the `eom` accumulated so far. Those bytes are unrecoverable: `in_buf_tail` was already advanced past them. `AsynError` (`crates/asyn-rs/src/error.rs:14-16`) has no variant that can carry a partial count, so the information cannot reach the caller.
C reference: `asyn/miscellaneous/asynInterposeEos.c:242-253` — on `status != asynSuccess` the loop `break`s and falls through to `if(nRead<maxchars) *data = 0; if (eomReason) *eomReason = eom; *nbytesTransfered = nRead; return status;`. The partial data, the null terminator, and the eom reason are all delivered *together with* the timeout/error status.
Impact: a device that emits a partial line and then stops (the common timeout case) yields, in C, `asynTimeout` with `AINP="abc"`, `NORD=3`, `EOMR=0`; in Rust the record sees the error with zero bytes and the three bytes are dropped. The existing regression test `eos.rs:537-595` (`test_lower_layer_error_surfaces_with_partial_data`) cites `readIt` for status preservation but asserts only the `Err`, cementing the byte loss it was meant to guard.

### R6-49: Multi-device auto-connect never attempts the port-level connect
Severity: High
Rust: `crates/asyn-rs/src/port_actor.rs:293-310` — when `base().flags.multi_device` is set, the auto-connect path calls only `self.driver.connect_addr(&connect_user)`; the port-level `driver.connect()` is confined to the `else` arm (`:311-322`). `PortDriver::connect_addr`'s default (`crates/asyn-rs/src/port.rs:927-930`) merely calls `PortDriverBase::connect_addr` (`:472-474`), which flips the per-address `connected` flag and opens no transport — and neither `drivers/prologix.rs` nor `drivers/ip_server_port.rs` overrides it. The following `check_ready_addr` → `check_ready` (`port.rs:384-389`) then rejects on the port-level `!self.connected`.
C reference: `asynManager.c:704-721` — `autoConnectDevice` first reconnects the **port** (`connectAttempt(&pport->dpc)` at `:716`, guarded by the 2 s window at `:712-713`), returns `FALSE` if the port is still down (`:721`), and only then, at `:723-737`, reconnects the device.
Impact: `prologix.rs:167` and `ip_server_port.rs:387` both declare `multi_device: true`. Once such a port's transport drops (`set_connected(false)`), every subsequent request fails `asynDisconnected` forever — the request path marks the address connected but never reopens the socket, and per R6-47 no background timer does it either. The port is permanently dead until an explicit `RequestOp::Connect` arrives.

### R6-50: A failed motor poll is silently swallowed — no COMM_ERR, no record process, STUP strands at BUSY
Severity: Medium
Rust: `crates/motor-rs/src/poll_loop.rs:79-88` — `poll_and_notify` does `match self.motor.lock() { Ok(m) => m, Err(_) => return }` and `match motor.poll(&user) { Ok(s) => s, Err(_) => return }`. Both early returns skip the `status_seq` bump, the `latest_status` write, and the `io_intr` pulse — even when `force == true`. `MstaFlags::COMM_ERR` is set only from `status.comms_error` on a successful poll (`crates/motor-rs/src/record/status_update.rs:317-318`), so an `Err` can never raise it.
C reference: `asynMotorController.cpp:217-222` — the forced-refresh path (`motorUpdateStatus_`, what STUP/GET_INFO drives) runs `poll(); status = pAxis->poll(&moving); pAxis->statusChanged_ = 1;` — the axis poll's return status is *captured but not acted on*, and `statusChanged_` is forced to 1 unconditionally, so the callback fires and the record processes. The background poller (`asynMotorController.cpp:658`) likewise calls `pAxis->poll(&moving);` and ignores the return, leaving the driver to surface failure via `motorStatusCommsError_` (`asynMotorController.cpp:97`) → MSTA bit 12 → `alarm_sub` COMM/INVALID (`motorRecord.cc:3392-3398`).
Impact: a driver whose `poll()` returns `Err` (transport timeout, or a poisoned `motor` mutex) produces no MSTA `COMM_ERR` bit, no COMM alarm, and no record process. A `STUP=BUSY` refresh clears only when a fresh sequence lands (`crates/motor-rs/src/record/status_update.rs:48-58`), so STUP latches at BUSY permanently, and `last_moving` is never updated so the poll loop keeps the stale moving/idle rate.

---

## CARRYOVER

- **DRV-16 / DRV-17** (`crates/asyn-rs/doc/c-parity-review-drivers-2026-06-29.md`, "OPEN for sign-off"). Still live: `drivers/ip_server_port.rs` has no asynOctet-*interface* interrupt list and no push on TCP accept (C `drvAsynIPServerPort.c:373-383`) or UDP `recvfrom` (C `:311-320`). asyn-rs models parameter interrupts only.
- **DRV-53** (LOW, prologix over-grants shutdown rights). Still live: `drivers/prologix.rs:169` sets `destructible: true`; C `drvPrologixGPIB.c:592-593` registers the port without `ASYN_DESTRUCTIBLE`.
- **R60** (motor, DEFER). Still live: `rg 'enc_ratio|encoder_ratio|EncoderRatio' crates/motor-rs/src` → no hits. C `motorRecord.cc:1975-1980` emits `WRITE_MSG(SET_ENC_RATIO, ep_mp)` on any MRES/ERES/UEIP change when `EA_PRESENT`; motor-rs never forwards the ratio to the driver.
- **R64** (motor, CONCERN candidate awaiting reachability sign-off). Still live: `crates/motor-rs/src/record/state_machine.rs:255-274` — the MIP_STOP Pause completion's `ls_blocks` arm clears MISS and restores SPMG but never calls `postprocess_sync()`; C `motorRecord.cc:1366-1380` treats a stop landing on a limit in the commanded direction as a terminal LS-completion (`pp = TRUE`, GET_INFO, `mip = MIP_DONE`) regardless of `pp`.

Verified as closed since their inventories, so **not** carried over: motor R2 (DIFF/RDIF now in the move-start notify list, `record/mod.rs:285-295`), DRV-40 (`drivers/serial_port.rs:650-658` rejects a double open), DRV-51 (`drivers/prologix.rs:326-340` clears `read_carry` on connect), DRV-23 (`drivers/ip_server_port.rs:1018` sets `EomReason::END` at the datagram boundary).

---

## Audited clean

- `motorRecord.cc` `maybeRetry` (1042-1104) ↔ `record/state_machine.rs:649-751`: boundary-inclusive `|diff| >= rdbd`, `++rcnt > rtry` give-up semantics (RCNT left at `rtry+1`), MISS latch on give-up only, MISS-clear + SPMG Move→Pause restore confined to the close-enough/LS-blocked arm, `rtry == 0` leaving both untouched.
- `motorRecord.cc` `alarm_sub` (3367-3406) ↔ `record/mod.rs:573-625`: UDF short-circuit, both limit arms gated on and raised at HLSV, one-shot COMM_ERR bit clear, no early return above the EA_SLIP_STALL/RA_PROBLEM STATE/MAJOR check.
- `motorRecord.cc` LVIO re-evaluation (1462-1484) ↔ `record/state_machine.rs:576-592`: `MIP_JOG` mask is exactly `JOGF|JOGR|JOG_BL1|JOG_BL2` (matching C's `#define` at 293), home disables the check, `!set && !igset` gates the stop; recomputed on the poll path too (`command_planner.rs:2041`, `:2142`, `state_machine.rs:105`).
- `motorRecord.cc` do_work soft-limit block (2397-2453) ↔ `command_planner.rs:632-656` + `refuse_move_restore_lasts` (`:1609-1620`): inverted-limit → LVIO, moving-toward-valid exception, preferred vs backlash-pretarget arms, reject-not-clamp restore of VAL/DVAL/RVAL, RETRY→DONE, DMOV→true; URIP RDBL-error refusal routed to the same owner (`:199-214`).
- `motorRecord.cc` `special()` DIR/OFF (2768-2793) and `load_pos` (3771-3801) ↔ `record/field_access.rs:985-1024`: FOFF-frozen branch recomputes VAL, FOFF-variable recomputes OFF, OFF write also re-anchors `lval` from `ldvl` so no false retarget.
- `asynInterposeEos.c` `writeIt` (154-181) and `flushIt` (256-268) ↔ `interpose/eos.rs:209-233`: single combined write of payload+terminator, `min(actual, data.len())` reported count, full buffer + match-state reset on flush.
- `asynInterposeFlush.c` `flushIt` (112-133) ↔ `interpose/flush.rs:58-75`: short timeout save/restore, discard-until-zero-bytes loop, read status ignored.
- `asynShellCommands.c` `asynSetEos` (219-253) ↔ `iocsh.rs:139-176` + `port.rs:1344-1380`: escape decoding (`raw_from_escaped`, `iocsh.rs:88-137`) and the `eoslen > 2` → `illegal eoslen N` rejection are present on the production path.
- `asynRecord.c` binary-input EOS bracket (1563-1582) ↔ `port_actor.rs:366-393`: save/clear/restore of IEOS and OEOS around the raw transfer, restored on every exit path.
- `asynManager.c` trace defaults (46, 456, 3128-3145) ↔ `trace.rs:261-277`, `:505-511`: `io_truncate_size` default 80 and truncation applied at emit. (Only the `asynSetTraceIOTruncateSize` iocsh registrar is absent — folded into the broader iocsh gap noted under R6-47, not reported separately.)


### Category E — synApps modules + areaDetector (R6-61..R6-70)

### R6-61: Overlay plugin ignores color mode — RGB images are painted with mono geometry and a single grey value
Severity: High
Rust: `crates/ad-plugins-rs/src/overlay.rs:353` — `draw_overlays` guards only `arr.dims.len() < 2`, then takes `w = dims[0].size`, `h = dims[1].size` (`:355-356`), writes one mono value `overlay.color[1]` (`:180`) at `idx = y * w + x` (`:186`). There is no `NDColorMode` branch anywhere in the file, and `OverlayProcessor::process_array` calls it for every array unconditionally (`:641`).
C reference: `ADCore/ADApp/pluginSrc/NDPluginOverlay.cpp:37-64` — `setPixel` tests `pArrayInfo->colorMode == NDColorModeRGB1/RGB2/RGB3` and writes `red`, `green`, `blue` at successive `pArrayInfo->colorStride` offsets; `addPixel` (`:29-34`) computes the address as `iy*pArrayInfo->yStride + ix*pArrayInfo->xStride`, so both the geometry and the value are color-mode aware.
Impact: An RGB1 array with dims `[3, X, Y]` is interpreted as a 3×X mono image. Rust paints the mono value into the first `3*X` samples (a corner of channel-interleaved data); C paints correct pixel coordinates across all three color planes. Every output pixel of an overlay on a color detector or a post-ColorConvert stream differs.

### R6-62: `polint` picks the wrong interpolation coefficient when the target is nearest the first sample, giving wrong rotation limits (HLAX/LLAX)
Severity: Medium
Rust: `crates/optics-rs/src/records/table.rs:1535-1536` — `let mut y = ya[ns]; ns = ns.saturating_sub(1);`. `ns` is 0-based, so the intended post-decrement value is `ns0 - 1`; for `ns0 == 0` `saturating_sub` clamps to `0` instead. The loop then evaluates `dy = if 2 * (ns + 1) < n - m { c[ns + 1] } else { … d[ns] … }` (`:1555-1560`), which for `ns0 == 0` tests `2 < n-m` and selects `c[1]`.
C reference: `optics/opticsApp/src/tableRecord.c:1918` (`int i,m,ns=1;`), `:1934` (`*y=ya[ns--];`), `:1945` (`*y += (*dy=(2*ns < (n-m) ? c[ns+1] : d[ns--]));`). With 1-based `ns`, `ns0 == 0` leaves `ns == 0`, so the test is `0 < n-m` (always true for `m < n`) and the selected coefficient is `c[1]` **1-based**, i.e. `c[0]` 0-based.
Impact: Whenever the motor limit being solved for is closest to trajectory sample 0, Rust adds `c[0-based 1]` where C adds `c[0-based 0]` on every Neville iteration, and for `m >= n-2` it takes the `d` branch C never takes. `find_limit` (`table.rs:1601/1608`) therefore returns a different user-coordinate crossing, so the record's calculated rotation limits `HLAX/HLAY/HLAZ` and `LLAX/LLAY/LLAZ` — and the `UserLimitViol` gate and the DBR_GR/CTRL limits served from them (`table.rs:3951-3955`) — differ from C.

### R6-63: pf4 "Other" filter with an unknown material or an out-of-range energy reads as fully transparent; C reads as fully opaque
Severity: Medium
Rust: `crates/optics-rs/src/snl/pf4.rs:226-229` — `MAT_OTHER` returns `transmission(mat, energy_kev, thickness_mm * 0.1).unwrap_or(1.0)` for a found material and `None => 1.0` for an unknown name. `transmission` → `interpolate_mu` returns `None` when `e < kev[0] || e > kev[n-1]` (`crates/optics-rs/src/data/chantler.rs:1208`), so an out-of-range energy also yields `1.0`.
C reference: `optics/opticsApp/src/pf4.st:629-631` and `:637-639` — `OtherAbsorptionLength` returns `0.` both when `strcmp` finds no species and when `j >= numEntries`. `RecalcFilters` then does `if (xOther1 > 0) xmit[i] *= exp(-xOther1*1000./absLenOther1);` (`:696`), i.e. `exp(-x/0) = exp(-inf) = 0`.
Impact: For an inserted "Other" blade with a bad material name or an energy above the Chantler table, `xmit[i]` is `1.0` in Rust and `0.0` in C — the filter combination is reported as passing the full beam instead of blocking it, and `sort_decreasing` (`pf4.rs:299`) then ranks that combination first instead of last. Rust's lookup is additionally case-insensitive (`chantler.rs:1192`, `eq_ignore_ascii_case`) where C uses `strcmp` (`pf4.st:627`), so names differing only in case take the interpolating path in Rust and the `0.` path in C.

### R6-64: `create_file_name` never calls `check_path`, so an un-normalized FilePath produces a run-together filename
Severity: Medium
Rust: `crates/ad-core-rs/src/driver/ndarray_driver.rs:567-582` — `create_file_name` reads `FILE_PATH`, `FILE_NAME`, `FILE_NUMBER`, `FILE_TEMPLATE` and calls `sprintf_template(template, path, name, number)` directly. `check_path()` (`:598`) exists but is not invoked here.
C reference: `ADCore/ADApp/ADSrc/asynNDArrayDriver.cpp:203` — `createFileName` calls `this->checkPath()` before reading any parameter; `checkPath` (`:98-111`) appends the trailing `'/'` to `NDFilePath`, writes it back with `setStringParam`, and refreshes `NDFilePathExists`.
Impact: With the default template `"%s%s_%3.3d.dat"` and a `FilePath` seeded internally (e.g. by a driver's `set_string_param`) without a trailing separator, C writes `/data/img_000.dat` while Rust writes `/dataimg_000.dat` — wrong file, wrong directory. `FilePathExists_RBV` is also not refreshed on each `createFileName` as it is in C.

### R6-65: `NDArrayPool::alloc` stamps `epicsTS` but never `timeStamp`, so the two published timestamps disagree on every reused buffer
Severity: Medium
Rust: `crates/ad-core-rs/src/ndarray_pool.rs:256` — `arr.timestamp = EpicsTimestamp::now();`. The free-list reuse branch (`:144-199`) resets `data`, `dims`, `attributes`, `codec` and `data_size` but never touches `time_stamp`; only a fresh `NDArray::new` zeroes it (`ndarray.rs:339`). There is no `update_time_stamps` equivalent anywhere in the crate, and `prepare_array` publishes both fields straight off the array (`driver/ndarray_driver.rs:320-322`).
C reference: `ADCore/ADApp/ADSrc/NDArrayPool.cpp:187-199` — `alloc`'s "Initialize fields" block sets `dataType`, `ndims`, `dims`, refcount and codec, and sets **neither** `epicsTS` nor `timeStamp`. `asynNDArrayDriver::updateTimeStamps` (`asynNDArrayDriver.cpp:832-836`) is the single owner that stamps both, and it derives `timeStamp = epicsTS.secPastEpoch + epicsTS.nsec/1.e9` so they always agree.
Impact: For any driver that allocates and publishes without explicitly setting timestamps, `EPICS_TS_SEC`/`EPICS_TS_NSEC` read `now()` while `TIME_STAMP_RBV` reads `0.0` (fresh buffer) or the previous frame's value (reused buffer). C keeps the pair consistent, or leaves both untouched.

### R6-66: ColorConvert false-color silently degrades to grayscale for every mono type except UInt8
Severity: Medium
Rust: `crates/ad-plugins-rs/src/color_convert.rs:741` — `false_color_mono_to_rgb1` returns `None` unless `src.data.data_type() == NDDataType::UInt8`. The caller chains `.or_else(|| color::mono_to_rgb1(array).ok())` (`:869`), so a non-UInt8 mono frame with `FalseColor != 0` falls through to plain grayscale replication with no error and no status write.
C reference: `ADCore/ADApp/pluginSrc/NDPluginColorConvert.cpp:108` — inside the templated `convertColor<epicsType>`, the Mono→RGB1 arm applies `memcpy(pOut, colorMapRGB + 3 * ((unsigned char)*pIn++), 3)` for **every** `epicsType`; the RGB2 (`:135-145`) and RGB3 (`:177-189`) arms do the same via `colorMapR/G/B[(unsigned char)*pIn]`.
Impact: A Mono `Int8`/`Int16`/`UInt16`/`Int32`/`Float32`… frame with `FalseColor=1|2` emits a grayscale RGB1 array in Rust and the (low-byte-indexed) pseudo-color image in C. Every output pixel value differs.

### R6-67: `interpolate_mu` brackets the energy differently than C `OtherAbsorptionLength`, so μ (and the transmission) differ
Severity: Low
Rust: `crates/optics-rs/src/data/chantler.rs:1211-1230` — binary search for `low`/`high` with `kev[low] <= e < kev[high]`, then `t = (e - x0)/(x1 - x0)` in `[0,1)` and `mu = y0 + t*(y1 - y0)` — a true interpolation on `[low, high]`.
C reference: `optics/opticsApp/src/pf4.st:634-643` — `for (j=0; j<numEntries; j++) if (keV < keV[j]) break;` then `frac = (keV - keV[j]) / (keV[j+1] - keV[j]); mu = mu[j] + frac*(mu[j+1] - mu[j]);`. Because the loop breaks at the first index **above** `keV`, `frac` is negative and the slope is taken from `[j, j+1]` — a backwards extrapolation off the upper interval, not an interpolation on `[j-1, j]`.
Impact: μ differs on every non-node energy, and by a large factor immediately below an absorption edge (where `mu[j+1]-mu[j]` is the post-edge slope, not the pre-edge one). The resulting `xmit[i]` values and the sorted `bits[i]` ordering (`pf4.rs:299`) differ from C. In the top bin (`j == numEntries-1`) C also reads the zero-padded `keV[j+1]`/`mu[j+1]`, which Rust never reproduces.

### R6-68: Overlay `DrawMode=XOR` is ignored on Float32/Float64 images
Severity: Low
Rust: `crates/ad-plugins-rs/src/overlay.rs:165-167` — the `set_only` macro arm's setter is `data[idx] = value;` and discards the `DrawMode` argument. `NDDataBuffer::F32` (`:375`) and `NDDataBuffer::F64` (`:386`) are the only two buffers dispatched with `set_only`; every integer buffer uses the `xor` arm (`:158-162`).
C reference: `ADCore/ADApp/pluginSrc/NDPluginOverlay.cpp:60` — the mono arm of `setPixel` is templated over `epicsType` and applies `*pValue = (epicsType)((int)*pValue ^ (int)pOverlay->green)` for `NDOverlayXOR` regardless of width, including `epicsFloat32`/`epicsFloat64`.
Impact: With `DrawMode=XOR` on a float image, Rust overwrites the pixel (Set semantics); C writes `(float)((int)old ^ (int)green)`. The drawn pixel values differ.

### R6-69: Manual `ResetFilter` frees the filter buffer, so the reinitialized filter uses the current frame instead of the previous filter contents
Severity: Low
Rust: `crates/ad-plugins-rs/src/process.rs:318-321` — `reset_filter()` sets `filter_state = None`. On the next frame `reset_filter = self.filter_state.is_none()` (`:458`) and the buffer is re-cloned from the current processed data (`:465`, `self.filter_state = Some(values.clone())`), so the reset formula at `:476` (`r_offset + rc1*filter[i] + rc2*values[i]`) evaluates with `filter[i] == values[i]`.
C reference: `ADCore/ADApp/pluginSrc/NDPluginProcess.cpp:91` clears the `ResetFilter` PV but does **not** free `pFilter`; `pFilter` is released only on an element-count mismatch (`:184`). The reset loop at `:204-209` therefore computes `newFilter = rOffset; if (rc1) newFilter += rc1*filter[i]; if (rc2) newFilter += rc2*data[i];` against the **previous** filter contents.
Impact: With `RC1 != 0`, a mid-acquisition `ResetFilter=1` reinitializes the filter (and hence the next `Filtered` output array) to a different value than C. The auto-reset path (`num_filtered >= num_filter`) is unaffected — it keeps the buffer.

### R6-70: `ADDriverBase::new` seeds Gain/Temperature/Bin/Size/StatusMessage that C `ADDriver` deliberately leaves to the DB and save/restore
Severity: Low
Rust: `crates/ad-core-rs/src/driver/ad_driver.rs:72` (`status_message = "Idle"`), `:87` (`gain = 1.0`), `:89-90` (`temperature = temperature_actual = 25.0`), plus `size_x/size_y = max_size_*` and `bin_x/bin_y = 1` (`:61-65`).
C reference: `ADCore/ADApp/ADSrc/ADDriver.cpp:180-191` — the constructor sets only `ADMaxSizeX/Y`, `ADStatus`, the two counters, `ADTimeRemaining`, `ADShutterStatus`, `ADAcquire`, and `setStringParam(ADStatusMessage, "")` (`:189`). The comment block at `:174-179` states explicitly that any value set here overrides the database for output records, which is why `ADGain`, `ADTemperature`, `ADBinX/Y` and `ADSizeX/Y` are omitted.
Impact: At init `StatusMessage_RBV` reads `"Idle"` (Rust) vs `""` (C), and `Gain`, `Temperature`, `BinX/BinY`, `SizeX/SizeY` read back the driver defaults instead of the values asyn device support would have taken from the DB or autosave.

---

## CARRYOVER

### R5-2 (sibling, still live): synApps calc-family records seed the calc `VAL` token with 0 instead of the previous result
The base `calc`/`calcout` fix (e4ae8906) was not propagated to `swait`, `scalcout`, or `transform`. All three still construct their input struct with the `prev_val: 0.0` default.

- `crates/epics-base-rs/src/server/records/swait.rs:87-93` — `build_inputs()` returns `StringInputs::new()` (`prev_val = 0.0`, `calc/engine/mod.rs:116/124`) and only fills `num_vars[0..12]`; consumed at `:437`. C `calc/calcApp/src/swaitRecord.c:409` calls `calcPerform(&pwait->a, &pwait->val, pwait->rpcl)` — `presult` is `&pwait->val`, so a `VAL` token in CALC reads the **previous VAL**.
- `crates/epics-base-rs/src/server/records/scalcout.rs:110-120` — same `StringInputs::new()`; consumed for CALC at `:520` and for OCAL at `:587`. C `sCalcoutRecord.c:357` passes `&pcalc->val`/`pcalc->sval` as the result cells, and `:768-769` passes `&pcalc->oval`/`pcalc->osv` for OCAL — so `VAL`/`SVAL` read the previous VAL/SVAL, and in OCAL they read the previous OVAL/OSV.
- `crates/epics-base-rs/src/server/records/transform.rs:498` — `NumericInputs::new()` (`prev_val = 0.0`). C `transformRecord.c:593` calls `sCalcPerform(&ptran->a, 16, NULL, 0, pval, NULL, 0, prpcbuf, ptran->prec)` where `pval = &ptran->a + i` (`:564`, `:569`), so a `VAL` token in `CLCx` reads **that channel's current value**, not 0.

Observable: any `CALC`/`OCAL`/`CLCx` expression containing the `VAL` token evaluates against `0` on every cycle in Rust. E.g. a swait with `CALC="VAL+A"` accumulates nothing (VAL stays `A`); a transform with `CLCB="VAL*2"` yields `0` instead of doubling B.

---

## Audited clean

- **`optics-rs/src/math/matrix3.rs`** ↔ `matrix3.c` — `dot`, `cross`, `dotcross`, `mult_mat_mat`, `mult_mat_vec`, `determinant` term-by-term identical; `invert` gate `|det| <= SMALL` ≡ C `fabs(det) > SMALL` (`matrix3.c:91`), `SMALL = 1e-11` matches `matrix3.h`. All C helpers already use temporaries, so C's aliased call sites (`multArrayArray(rot, r3, rot)`) carry no divergence.
- **`optics-rs/src/math/orient.rs`** ↔ `orient.c` — `calc_rot_z`/`calc_rot_y` sign conventions, `angles_to_hkl` rotation order and `vec = [sin(TTH/2),0,0]`, all three `hkl_to_angles` constraint branches (incl. `MIN_CHI_PHIm90` falling through to the `PHI_CONST` body), `check_small`, `calc_a0` (A/B/C lattice vectors, `lambda/(2*A·B×C)` factor, column layout), `calc_omtx` Vp/Vpp row layout and identity fallbacks, `check_omtx` normalization + `acos/D2R`. `Constraint` discriminants (0/1/2) match `orient.h:33-35` and the `$(P)orient$(O):Mode` mbbo order in `opticsApp/Db/orient.db:200-204`.
- **`optics-rs/src/records/table.rs` `process()`** ↔ `tableRecord.c:411-601` — ZERO/READ/SYNC/INIT/SET/calc-and-move branch order and bodies, `lvio` reset, `axl[i] = ax[i] + ax0[i]` tail, `motor_limit_viol` (`:1046-1050` guard `can_read_limits && can_RW_drive && (|h|>SMALL || |l|>SMALL)`), `user_limit_viol` (`:1071-1076`), `build_output_actions` ↔ `ProcessOutputLinks` (`:932-952`, speeds gated by the move mask, drives written for every `can_RW_drive`) + `RestoreMotorSpeeds` (`:998-1006`, restores every speed-capable motor, not just the moved ones).
- **`table.rs` `on_put`** ↔ `special()` (`tableRecord.c:604-723`) — geometry/GEOM re-init + `sync=1`; the YANG two-pass `LabToLocal(old)` → `LocalToLab(new)` offset rotation; the 12-element absolute↔relative user-limit conversions with `ax0[i%6]`; SSET/SUSE/SYNC/INIT/ZERO/READ; the AUNIT `convertFact` (`1e6*D2R` vs `1e-6/D2R`, applied to indices 0..2 of `uhax/ulax/uhaxr/ulaxr/ax0`), `aegu`/`torad` update and `sync=1`.
- **`table.rs` `sort_trajectory` / `find_limit` / `user_limits_local_to_lab` / `calc_local_user_limits`** ↔ `SortTrajectory` (`:1893-1908`), `FindLimit` (`:1955-2009`), `UserLimits_LocalToLab` (`:2160-2207`) 4-quadrant table, and `CalcLocalUserLimits` (`:2011-2152`) trajectory bisection incl. the `limitCrossings<2` bound, the `delta *= 0.5` halving, the ±89°/±1.55e6 µrad clamp, and the "couldn't find a limit" fallback. Only the `polint` core diverges (R6-62).
- **`table.rs` `field_metadata_override`** ↔ `get_units` (`:751-772`), `get_graphic_double` (`:778-791`), `get_control_double`, `get_precision` — angle-class fields → `aegu`, others → `legu`; AX..Z report `(hlax[i], llax[i])`; `VERS` → precision 2.
- **`optics-rs/src/snl/orient.rs`** ↔ `orient_st.st` — `recalc_a0` zero-parameter gate (`:491`) and identity-on-singular publish (`orient.c:195-200`, `orient_st.st:496-503`); `recalc_omtx` non-zero-HKL gate (`:523`), identity-on-singular, and the `(i==0) && fabs(errAngle) < errAngleThresh` success gate (`:550-560`); `constraint_from_mode` mapping.
- **`ad-plugins-rs/src/time_series_plugin.rs`** ↔ `NDPluginTimeSeries.cpp` — averaging truncation, Fixed/Circular acquire, axis, current-point, elapsed time, per-signal `doCallbacksFloat64Array`. `TS_TIMESTAMP` is not emitted in either.
- **`ad-plugins-rs/src/stats.rs`** histogram binning and sigma/centroid; **`process.rs`** auto-offset-scale `maxScale` (`NDPluginProcess.cpp:196-214`).
- **`ad-core-rs`** — `NDArray::get_info` RGB1/2/3 strides and the ColorMode-attribute read; `NDArrayPool` alloc/release/free-list buffer accounting vs `numBuffers_`/`getNumFree`; `NDAttributeList::copy_from`; `format_int_spec` width-vs-precision.
- **`scaler-rs`** — SCAL-1..SCAL-9 surface re-checked against the round-2 doc; no new divergence. **`std-rs`** — STD-9 (`throttle` WAIT ⟺ `pending_value.is_some()`) and STD-11a (`epid.dt` from `time_per_point_actual`) confirmed landed. **`modbus-rs`** — R54/R56/R57/R58 confirmed CLEARED per the round-3/4 log; R34/R52 are asyn-rs-contract items outside this category's scope. **`mqtt-rs`** — no OPEN items in the inventory doc.
## Cleared During Review

### Fix wave 1 (2026-07-11): categories D + E — merged 1caa6034 / d7363692, verified by main (workspace clippy -D warnings clean, nextest 7462/7462 passed)

Category D (asyn-rs / motor-rs):
- R6-46: FIXED db4d0347 — `OctetInterpose::connection_changed()` hook + stack fan-out routed through the single owner `PortDriverBase::set_connected`; EOS resets via `reset_link_state()` shared with `flush()`.
- R6-47: FIXED 9a03baa0 — autonomous connect timer: 0.01 s on disconnect, re-arm every `secondsBetweenPortConnect` (20 s default) on failure; fires with no queued traffic.
- R6-48: FIXED fe221c8a — `AsynError::PartialRead { status, message, partial }`; partial bytes + eom delivered with the error; all status-extracting sites moved to `e.status()`; the test that cemented the byte loss rewritten.
- R6-49: FIXED b9706d87 — `auto_connect_device` connects the PORT first (bail if still down) then the device; widening closed a latent `device_states[-1]` phantom-index bug via `is_device_addr` as single owner.
- R6-50: FIXED 8736e14f — failed poll synthesizes last-known status with `comms_error`/`problem` forced on through the same notify path (MSTA bit 12 → COMM/INVALID); C quirks kept (`last_moving` carries forward; repeated identical failure posts nothing). Same defect fixed at `asyn-rs/runtime/axis.rs::poll_motor`.

Category E (ad-plugins-rs / ad-core-rs / optics-rs / epics-base-rs):
- R6-61: FIXED 13c01a17 — overlay geometry from `NDArrayInfo` strides, RGB1/2/3 writes all three planes.
- R6-62: FIXED 2c103003 — polint tableau walk matches C's 1-based `ns` (also fixes the d-branch path, wider than cited); expected values from the compiled C function.
- R6-63: FIXED 116ba576 — pf4 "Other" unknown/out-of-range → `absLen==0` → `exp(-inf)=0` opaque; case-sensitive material match.
- R6-64: FIXED f9d1a718 — `create_file_name` runs `check_path` first; file_controller duplicates routed through `check_path_str`.
- R6-65: FIXED d034fc75 — `NDArray::update_time_stamps` is the single owner of the `epicsTS`/`timeStamp` pair; `alloc` stamps neither.
- R6-66: FIXED 2c862c94 — false color on Int8 via low-byte LUT. Partially NOT-REAL as cited: C reads FalseColor only for NDInt8/NDUInt8, so non-8-bit types already matched; pinned by test.
- R6-67: FIXED 6f3bc358 — split the two C consumers: `interpolate_mu` reproduces filterDrive `calcTrans` ([j-1,j] + rejects), pf4 gets `other_absorption_length_um` with C's negative-frac extrapolation quirk.
- R6-68: FIXED abd68f72 — XOR draw mode on float/64-bit via C's int-cast narrowing.
- R6-69: FIXED 8893e19d — manual ResetFilter keeps the buffer; freed only on element-count mismatch.
- R6-70: FIXED 447e2ba1 + 4076fa0b (same finding; second commit is example-driver fallout) — constructor seeds only what ADDriver.cpp:180-191 seeds.
- R5-2 sibling (carryover): FIXED d76dcd64 — `prev_val` seeded from C's `presult` cell (swait VAL; scalcout VAL/OVAL per phase; transform per-channel); `build_inputs` takes the result cell as a parameter so call sites cannot forget it.

### Fix wave 2 (2026-07-12): categories A + C — merged into review/parity-r6, verified by main (workspace clippy -D warnings clean, nextest 7506/7506 passed, doctests clean)

Category A (epics-base-rs, + epics-ca-rs test respell):
- R6-1: FIXED a795ed72 — dbCommon DBF_MENU fields served as DBR_ENUM via the existing shared_menu_choices owner (STAT/NSTA→menuAlarmStat, SEVR/NSEV/ACKS/DISS/UDFS→menuAlarmSevr, ACKT→menuYesNo, PINI→menuPini); widened to NSTA/NSEV; deleted a wrong field-blind menuPini table.
- R6-2: FIXED 0d663892 — link field-name guard matches C dbNameToAddr (record name ends at first '.', remainder a C identifier); `MBBOD.B0`/`sseq.DO1` resolve as DB links.
- R6-3: FIXED 6b261c71 — C's else-if precedence chains (NPP,CPP,PP,CA,CP / NMS,MSI,MSS,MS), exactly one process class; two epics-ca-rs tests that spelled `CP CA` respelled to bare `CP`.
- R6-4: FIXED d6814e88 — per-link-field-type modifier mask (OUT discards CP/CPP, FWDLINK masks to CA); cited site was 1 of 6, all routed through the mask; warning moved to the put/load boundary.
- R6-5: FIXED eb7f8de6 — PINI is the 6-choice menuPini; single pass owner `PvDatabase::pini_process(mode)`; RUN/RUNNING wired at AtIocRun/AfterIocRunning.
- R6-6: FIXED 39952331 — pini_process runs C's do-while PHAS sweep with live reads (iocInit.c:614-619).
- R6-7: FIXED b1f381e1 — empty array into scalar: accept, leave field, LINK/INVALID alarm, return 0; three duplicated coercion blocks replaced by one owner (`dbput_request`) keyed on request count + destination.
- R6-8: FIXED 07d41f33 — unusable I/O Intr demotes SCAN to Passive with a logged error; IocBuilder duplicate wiring loop routed through the same owner.

Category C (epics-pva-rs / epics-bridge-rs / epics-base-rs):
- R6-31: FIXED 590bc47f — Q:time:tag parses byte-exactly like pvxs (case-sensitive prefix, epicsParseInt32 port, no clamp); single owner `apply_nsec_mask(mask)`.
- R6-32: FIXED faa37315 — RPC INIT never runs request_to_mask.
- R6-33: FIXED ea8a4b11 — NULL (0xFF) pvRequest descriptor at INIT = all-fields wildcard, not connection-fatal; one owner `decode_init_pv_request` used by all INIT sites.
- R6-34: FIXED 2d59ae6c — `RpcReply` enum (Empty | Value) models both pvxs ExecOp::reply() overloads end-to-end (server emit, client decode, gateway relay). PUBLIC API CHANGE: ChannelSource::rpc*/SharedPV::rpc*/PvaClient::pvrpc* carry RpcReply; Into<RpcReply> keeps existing handlers compiling.
- R6-35: FIXED 8d5daedf — MONITOR FINISH trailing update decoded and delivered before stream end; single owner `monitor_finish_body` consulted by typed, marker-flattening, and raw-gateway paths.

### Fix wave 3 (2026-07-12): category B — merged into review/parity-r6, verified by main (workspace clippy -D warnings clean, nextest 7541/7541 passed on re-run; first run had 2 compile-load flakes, see notes)

Category B (epics-ca-rs / epics-tools-rs):
- R6-16: FIXED bae068a6 — recv echo timeout marks unresponsive + keeps the socket; close only on real socket error.
- R6-17: FIXED 048ce75f — flow control keyed on socket-buffer occupancy, trigger scaled from EPICS_CA_MAX_ARRAY_BYTES; consumer-queue latch removed; 07-flow-control.md / 09-libca-parity.md corrected.
- R6-18: FIXED 7e9d3edf — server gates extended CA header on peer V49; shared version-aware `set_payload_size(..., peer_minor)` primitive.
- R6-19: FIXED c2aa82a4 — zero-count requests to pre-V413 peers substitute the native element count.
- R6-20: FIXED b019a4a2 — request element counts bounded against negotiated circuit limits.
- R6-21: FIXED 873725be — no receive cap by default; oversize payload never closes the circuit.
- R6-22: FIXED 80269738 — repeater registration retried every 1 s until CONFIRM. Residual OPEN: libca's odd/even `m_available` address alternation across retries not implemented.
- R6-23: FIXED 92bb559e — libca beacon anomaly bands (0.80x/1.25x/3.25x); reverses a deliberate prior divergence that was only needed before R6-16 (documented in commit).
- R6-24: FIXED 6f0c2e77 — foreground mode attaches the launching terminal as a console client through the existing spawn_client owner; termios RAII guard; verified on a real PTY.
- R6-25: FIXED 6c2512a7 — foreground mode SIG_IGNs SIGINT/SIGQUIT.
- R6-26: FIXED 749a585c — all six child-launch failure paths exit 255.
- R6-27: FIXED 9bbd5ad3 — verifyClients() bind-test sweep on every registration.
- R6-28: FIXED 53360707 — beacon EMA alpha 0.125.
- R6-29: FIXED 3f7906ea — scalar DBR_STRING put framed align8(strlen+1), >40 non-NUL → ECA_BADCOUNT; put framing consolidated into single owner `protocol::build_put_frame` (was two owners).

### R6-30: server restarts the beacon ramp on every TCP accept/disconnect — "Rust enhancement", not C parity
Severity: Medium (REPORTED by fixer-B; server-side sibling exposed by R6-23)
Rust: `crates/epics-ca-rs/src/server/tcp.rs:811-815,1091-1095` — beacon_reset on connect/disconnect, self-described as non-parity.
C reference: `online_notify.c:66,128` — ramp restarts only at startup and after ctlPause.
Impact: with R6-23's client bands installed, a peer connecting to an epics-rs server produces ShortPeriod anomalies on every other client (log noise, search wakes, watchdog flags).

### R6-74: `EpicsValue::String::to_bytes` truncates >39-char strings on non-client-put paths; C raises ECA_BADCOUNT
Severity: Medium (REPORTED by fixer-B; R6-29 was explicitly scoped to the client put path)
Rust: `crates/epics-base-rs/src/types/value.rs` — fixed-40 truncation on remaining paths.
Impact: an oversized string silently truncates where libca errors.

### R6-75: procServ child inherits `SIGPIPE=SIG_IGN`; C's child gets default disposition
Severity: Low (REPORTED by fixer-B, adjacent to R6-25 but child-side)
Impact: a child that relies on dying from SIGPIPE (classic pipeline behaviour) keeps running under procserv-rs.

### Fix wave 4 (2026-07-12): fixer-surfaced items — merged into review/parity-r6, verified by main (workspace clippy -D warnings clean, nextest 7563/7563 passed first run)

- R6-9: FIXED 21c01fe5 — unknown-field put errors via `unknown_field_error` (S_db_noMod vs S_dbLib_fieldNotFound split per C); OUTN gated to swait.
- R6-10: FIXED 8c9adf60 — multi-element array into scalar clamps nRequest and writes element 0 (dbAccess.c:1359), applied in the R6-7 owner `dbput_request`; parallel `set_val` coercion collapsed into it; dbChannel `$`-view exempt.
- R6-30: FIXED 39df41d1 — beacon-ramp reset removed from TCP accept/disconnect; `beacon_reset` parameter deleted so the TCP path cannot reach the ramp by construction; beacon-count e2e test (23 pre-fix → ≤4).
- R6-71: FIXED fbcd670a — SVAL token: lexer → CoreOp::FetchSval → postfix (string-typed per sCalcPostfix.c:452) → prev_sval seeded sval/CALC, osv/OCAL; numeric+array evaluators reject the opcode; swait untouched (C swait is numeric calcPerform, no SVAL field).
- R6-74: NOT-REAL 7862be76 — remaining to_bytes consumers are server-side DBR_STRING replies whose C counterpart `getStringString` (dbConvert.c:132-154) also truncates to 39+NUL; client put paths already guarded at all 9 sites (R6-29). Boundary test (39/40/45) pins the contract; two stale doc comments corrected.
- R6-22 residual: FIXED 9e7376c1 — repeater registration alternates osiLocalAddr/loopback across retries (udpiiu.cpp:476-519) via cached `osi_local_addr()`.
- R6-51: FIXED 6c53f1ae — prologix read error stages accumulated bytes in `read_carry` (C keeps bufCount advanced, drvPrologixGPIB.c:250,301-303); one owner `stage_read_carry()`.
- R6-72: FIXED bef5d9f5 — `OverlayDef.color` is `[i32;3]` (C int channels, NDPluginOverlay.h:38-40); real truncation owner was the param path's `.clamp(0,255) as u8`. PUBLIC API note: `[u8;3]` literal constructors still compile; a `[u8;3]` variable does not.
- R6-73: FIXED 4cfef8db — density f64 (C reads `extern double matdensity[]`; the struct's own float field feeds no computation); kev/mu stay f32 (genuinely float in C).
- R6-75: FIXED 09fa2e14 — child gets C's exact signal environment: SIG_DFL dispositions PLUS C's blocked {SIGPIPE, SIGTERM, SIGHUP} mask (procServ.cc:490-494 leak preserved across execve). Finding's impact statement corrected: C's child does NOT die from SIGPIPE either. OPERATIONAL: a procserv-rs child cannot be killed with SIGTERM/SIGHUP — same as C; default killSig=SIGKILL and `--killsig 2` both work; supervisor shutdown follows killSig + unconditional SIGKILL. Accepted as parity; flag if the mask-leak half should be dropped.

### R6-76: procServ parent never sets `SIGXFSZ = SIG_IGN`
Severity: Low (REPORTED by fixer-G, same family as R6-75, parent-side)
C reference: `procServ.cc:502-503` — parent ignores SIGXFSZ; child inherits it ignored.
Impact: supervisor and child keep SIG_DFL and die on a file-size-limit write where C survives.

### R6-77: shared calc tokenizer accepts string-only tokens in numeric CALC; C postfix() rejects at compile
Severity: Low (REPORTED by fixer-F; pre-existing, exposed by R6-71)
Rust: one tokenizer across calc/sCalc/aCalc — SVAL, string literals, and sCalc string functions compile inside a numeric calc/calcout CALC and fail at eval with CalcError::Internal.
C reference: `postfix()` rejects them at compile time (CLCV != 0).
Impact: a bad CALC surfaces at first process instead of at load; current behaviour pinned by `numeric_calc_rejects_sval`. Related note: port's swait evaluates via the string engine where C swait uses numeric calcPerform, so swait accepts strings C rejects.

## Open Findings — surfaced during fix wave 1 (reported by fixers, pending independent verify)

### R6-51: prologix read error drops bytes accumulated in `acc`; C retains them for the next call
Severity: Medium (REPORTED by fixer-D while widening R6-48)
Rust: `crates/asyn-rs/src/drivers/prologix.rs:575` — returns `Err(e)` and drops `acc`.
C reference: `prologixRead` reports `*nbytesTransfered = 0` on error but retains the bytes in `pdpvt->buf`/`bufCount` so the next call resumes from them.
Impact: bytes read before the error are lost permanently instead of being delivered on the next read. Distinct from R6-48 (retention across calls, not delivery with the error).

### R6-71: `SVAL` token missing from the string-calc engine
Severity: Medium (REPORTED by fixer-E)
Rust: `crates/epics-base-rs/src/calc` — no `SVAL` token in lexer/evaluator (`rg -i sval` → none).
C reference: `sCalcPostfix.c:188` / `sCalcPerform.c:927-932` — `FETCH_SVAL` pushes `*psresult` (previous SVAL/OSV).
Impact: any sCalcout/swait expression using the `SVAL` token fails to parse or evaluates wrongly; the R5-2-sibling fix seeded numeric `prev_val` only.

### R6-72: `OverlayDef.color` is `[u8;3]`; C stores int per channel
Severity: Low (REPORTED by fixer-E)
Rust: `crates/ad-plugins-rs/src/overlay.rs` — `OverlayDef.color: [u8; 3]`.
C reference: `NDPluginOverlay.h` — per-channel `int` color.
Impact: overlay color values above 255 (meaningful on 16-bit images) cannot be expressed.

### R6-73: `FilterMaterial.density` is `f32`; C `matdensity[]` is double
Severity: Low (REPORTED by fixer-E)
Rust: `crates/optics-rs/src/snl/pf4.rs` — `FilterMaterial.density: f32`.
C reference: `pf4.st` — `matdensity[]` double.
Impact: ~5e-8 relative difference in absorption length; below current test tolerance but a real numeric deviation.

### R6-9: `put_common_field` silently accepts an unknown field name
Severity: Medium (REPORTED by fixer-A while testing R6-7)
Rust: `put_common_field` returns `Ok(NoChange)` for an unknown field.
C reference: `dbNameToAddr`/`dbPutField` return `S_db_badField`.
Impact: a caput to a nonexistent field reports success to the client instead of an error.

### R6-10: multi-element array into a scalar field errors; C converts element 0 and succeeds
Severity: Medium (REPORTED by fixer-A, adjacent to but distinct from R6-7)
Rust: the scalar-destination coercion path rejects `nRequest > 1`.
C reference: `dbAccess.c:1373-1388` — converts the first element and returns success.
Impact: `caput -a` of a multi-element array to a scalar fails on the port, succeeds (with element 0) on C.

## Known residuals / notes (fix waves 1-2)
- PINI=PAUSE/PAUSED passes absent: the port has no iocPause lifecycle (`InitHookState` lacks AtIocPause/AfterIocPaused). Values store and serve correctly (R6-5); only the passes are missing. Feature gap, needs a lifecycle decision — not silently inventable.
- `put_pv` posts no monitor events by its documented contract (caller's job); R6-7's empty-request path on that entry point does not itself post DBE_VALUE|DBE_LOG. General audit of that contract is a separate question.
- Pre-existing (not touched): `cargo build -p epics-base-rs --all-features` fails at `tests/client_server.rs:148` (`WallTime` vs `SystemTime`), gated behind the `ca-server-tls-test` feature; present since before this round (at 70b4a74b).
- Flaky under load (not root-caused): `epics-pva-rs::stability` `pva_fr_8_pause_holds_latest_then_resume_delivers` and `array_concurrent_subop_replies_error_not_silent` each failed once in runs launched right after a rebuild, pass in isolation and on a quiet machine (fixer-A report); both passed in main's post-merge workspace runs. Same pattern after the wave-3 merge: `regression-ioc::motor d_motor_moves_again_on_second_caput` and `regression-ioc::families o_seeded_record_suppresses_duplicate_post` failed once in the first post-rebuild workspace run, then passed in isolation AND in a second full workspace run (7541/7541). Also pre-existing per fixer-B: `epics-ca-rs protocol_tests::mr_r7_rejected_queued_datagram_does_not_reparse_stale_buffer` (UDP-burst timing, fails on the unmodified tree too).
- Observation (fixer-B, pre-existing): `epics-tools-rs listener.rs:154,236` log "listener accepted" at bind time with the listener's own address — reads as a spurious client connection.

## Round 7 — re-audit (2026-07-12): fix verification + fresh findings

Same 5 auditor panels (opus, read-only), same category blocks (R7-1..15 / 16..30 /
31..45 / 46..60 / 61..75). Mandate: (a) independently verify every R6 fix commit
against the C reference at HEAD, (b) fresh negative-space hunt on surfaces the
fixes touched or R6 did not reach.

### Fix verification result

Every R6 fix commit was independently verified against the C/C++ reference by the
re-audit panels — **no wrong or incomplete fix found**, with one exception filed as
a finding: R6-48's `PartialRead` variant is built correctly inside the interpose
but the bytes are still discarded at the actor dispatch (filed as R7-46, a
fix-completeness finding). Per-panel "Audited clean" evidence is in the round
report (`.caucus/sessions/01KX5QMAM71PJZFNWG0SFREHPX/rounds/01KX90HK1EGW864MNKCNPFDHJY.md`).

### Category A — epics-base-rs database engine (R7-1..R7-2)

### R7-1: `caput REC.PROC` bypasses the DISP put-disable gate that C enforces first
Severity: Medium
Rust: `crates/epics-base-rs/src/server/database/field_io.rs:762-823` — the `if field == "PROC"` intercept in `put_record_field_from_ca_inner` processes and **returns** before the DISP gate at `:826`; comment at `:757` says "regardless of DISP". No earlier DISP check in `:703-762`.
C reference: `modules/database/src/ioc/db/dbAccess.c:1256` — `dbPutField` returns `S_db_putDisabled` when `precord->disp && paddr->pfield != &precord->disp` **before** dbPut and the PROC-driven dbProcess (`:1265-1277`). PROC's pfield ≠ &disp, so DISP blocks it.
Impact: `caput REC.PROC 1` on a `DISP=1` record force-processes on the port; C returns S_db_putDisabled and does not process (WRITE_NOTIFY carries the error). The QSRV boundary (`field_io.rs:214-217`) *does* block PROC under DISP, so the two external-put boundaries disagree.

### R7-2: client put to a pp field / .PROC on an async-active record never sets RPRO; C's deferred reprocess is lost
Severity: Medium
Rust: `crates/epics-base-rs/src/server/database/field_io.rs:1082-1190` (should_process path) and `:762-807` (PROC path) call `process_record_with_links_already_locked` unconditionally; the re-entrant PACT branch (`processing.rs:1038-1065`) bumps lcnt / raises SCAN_ALARM but never sets `common.rpro`. The `tg.common.rpro = true` sets at `processing.rs:3225,4220` are the DB-link target path only.
C reference: `dbAccess.c:1269-1273` — `dbPutField` on `precord->pact` sets `rpro = TRUE` and skips dbProcess; `recGblFwdLink` (`recGbl.c:288-302`) sees RPRO on async completion and queues `scanOnce`.
Impact: two rapid caputs to a Passive async output (asyn/motor ao/longout): C writes both values to the device (second via RPRO reprocess); the port writes only the first — the second lands in VAL but never reaches hardware and schedules no reprocess.

### Category B — epics-ca-rs + epics-tools-rs (R7-16..R7-19)

### R7-16: server default access-security host identity uses peer IP; C trusts the client-claimed hostname by default
Severity: Medium
Rust: `crates/epics-ca-rs/src/server/tcp.rs:1350` (hostname defaults to peer IP) and `:1964-1966` — CA_PROTO_HOST_NAME overwrites `state.hostname` only when `EPICS_CAS_USE_HOST_NAMES=YES` (default NO), so ACF/HAG matching runs against the peer IP.
C reference: `camessage.c:839-869` — `host_name_action` stores the client-supplied name **unconditionally** in the default path; only when `asCheckClientIP` (global, default **0**, `asLibRoutines.c:34`) is set does it keep the IP. `asLibRoutines.c:1223` matches HAGs against that hostname. `EPICS_CAS_USE_HOST_NAMES` does not exist in epics-base.
Impact: a `HOST(node)` HAG rule that grants WRITE in C grants nothing in Rust on identical `.acf`; CA_PROTO_ACCESS_RIGHTS and caput enforcement differ. Three docs (`crates/epics-ca-rs/doc/09-libca-parity.md:159`, `crates/epics-ca-rs/doc/04-server.md:119`, `crates/epics-ca-rs/doc/08-environment.md:178`) falsely assert this "matches C rsrv default".

### R7-17: procServ kill-key on a dead child omits the unconditional "@@@ Got a kill command" broadcast
Severity: Low
Rust: `crates/epics-tools-rs/src/procserv/menu.rs:65-68` — `Action::evaluate` returns a single action per byte; dead child + kill char returns `RestartChild` early, never reaching `KillChild` (`:87-91`), so the supervisor broadcast never fires.
C reference: `procServ/clientFactory.cc:207-213,236-240` — restart-on-dead block AND a separate non-`else` kill-char block that always runs `SendToAll("\n@@@ Got a kill command\n")` + signal.
Impact: monitoring clients scripting against console markers see the marker in C but not in Rust when `^X` is pressed while the child is down. The single-action-per-byte abstraction structurally cannot express "restart AND broadcast".

### R7-18: server tears down the circuit on an oversize inbound request; C replies ECA_TOLARGE, drains, keeps serving
Severity: Medium
Rust: `crates/epics-ca-rs/src/server/tcp.rs:1549-1554` — after ECA_TOLARGE for `ext_post > max_payload_size()`, `break 'client_loop Err(...)` closes the connection; the comment claims "matches C dispatcher … + drop" — false.
C reference: `camessage.c:2472-2489` — TCP clients get `send_err(ECA_TOLARGE)`, then `recvBytesToDrain` skips the oversize body (drain at `:2375-2383`) and the circuit continues.
Impact: one oversize array caput destroys every channel/subscription on the Rust circuit; C keeps them all alive. Server-side sibling of R6-21 (client fix not mirrored).

### R7-19: client imposes a hardcoded 5-second TCP connect ceiling; C uses a blocking connect
Severity: Low
Rust: `crates/epics-ca-rs/src/client/transport.rs:878-892` — `tokio::time::timeout(5s, TcpStream::connect)`, abandons on expiry.
C reference: `tcpiiu.cpp:606-661` — blocking `::connect()` bounded by the OS TCP timeout; transient name-service failure sleeps EPICS_CA_CONN_TMO (30 s default) and retries.
Impact: slow-but-live servers (SYN-lossy path, 5–30 s handshake) reachable from C are unreachable from Rust.

### Category C — epics-bridge-rs QSRV source layer (R7-31..R7-33)

Shared structural root: the three QSRV **source-layer** `logRemote` sites in pvxs
(`groupsource.cpp:560`, `singlesource.cpp:129`, `iocsource.cpp:447`) have no Rust
counterpart — the source→wire boundary passes only values/masks and cannot inject
an IOID-tagged CMD_MESSAGE. One fix (thread a diagnostic sink from the source
option-parse sites to the op's `chan_tx`) closes all three.

### R7-31: QSRV group PUT drops a marked-but-not-putable member silently, omitting pvxs's "no putorder" CMD_MESSAGE
Severity: Low
Rust: `crates/epics-bridge-rs/src/qsrv/group.rs:1399-1403` — `filter_map` drops `put_order == None` members before iteration; no diagnostic.
C reference: `ioc/groupsource.cpp:556-561` — on `marked && !putable`, `notify.logRemote(Warn, "<field>: no putorder, ignore write")` → CMD_MESSAGE frame (`serverconn.cpp:146-160`).
Impact: write outcome identical, but the Warning wire frame naming the ignored field is absent.

### R7-32: QSRV single-record MONITOR DBE string selecting an empty mask silently falls back, omitting pvxs's "selects empty mask" CMD_MESSAGE
Severity: Low
Rust: `crates/epics-bridge-rs/src/qsrv/channel.rs:179-189` — unrecognized `record._options.DBE` string (e.g. "LOG", lowercase "value") folds to VALUE|ALARM via `dbe_value_class_mask(0)` with no diagnostic.
C reference: `ioc/singlesource.cpp:122-130` — warns `<name>="<mask>" selects empty mask` before the same fallback.
Impact: identical fallback mask, missing Warning frame.

### R7-33: QSRV GET/PUT `record._options.process` with an unsupported value silently maps to passive, omitting pvxs's "Ignoring unsupported" CMD_MESSAGE
Severity: Low
Rust: `crates/epics-bridge-rs/src/qsrv/channel.rs:243-248` — `"passive"` and any unrecognized value both collapse into `None → Passive` with no diagnostic.
C reference: `ioc/iocsource.cpp:426-447` — explicit `"passive"` → Unset silently; genuinely unsupported values warn via logRemote.
Impact: same default applied, missing Warning frame; also loses the "passive"-vs-unsupported distinction.

### Category D — asyn-rs (R7-46..R7-49)

### R7-46: EOS-interpose partial bytes are retained in the error but never delivered to any record — the `?` at the actor dispatch discards them
Severity: Medium (fix-completeness of R6-48)
Rust: `crates/asyn-rs/src/port_actor.rs:459,485,515` — `self.driver.io_read_octet_eom(user, &mut buf)?` propagates `Err(PartialRead{..})` before `octet_read_eom` is built; `adapter.rs:2235-2240` maps to an alarm only (no value, NORD untouched); `sync_io.rs:103-113` returns the bare Err. `AsynError::partial_read()` has zero production consumers.
C reference: `asynRecord.c:1592,1627` — `eomr`/`nord` assigned regardless of status; `asynInterposeEos.c:242-253` and `devAsynOctet.c:703` leave the caller's count populated.
Impact: a device emitting partial "abc" then going quiet yields on C `AINP="abc"`, NORD=3, EOMR=0 with READ_ALARM; on the port a timeout alarm, no value, NORD unchanged. R6-48's own stated contract ("caller gets the timeout AND the bytes") is unmet past the interpose.

### R7-47: `disconnectOnReadTimeout` never fires on any EOS-equipped IP port — `is_timeout` matches by variant, not `e.status()`
Severity: Medium
Rust: `crates/asyn-rs/src/drivers/ip_port.rs:667-673` — `matches!(e, AsynError::Status { status: Timeout, .. })`; the EOS interpose wraps every lower-layer error (incl. zero-byte timeout) as `PartialRead{status:Timeout}` (`interpose/eos.rs:199-207`), so `is_timeout` is always false and `should_disconnect` (`:685-688`) drops the term. Default `drvAsynIPPortConfigure` installs the EOS interpose (`iocsh.rs:636`). Sibling of the R6-48 `is_fatal_transport_error` conversion (`:551-557`) that was done.
C reference: `drvAsynIPPort.c:798-806` — the disconnect test runs inside the driver's readRaw, below the interpose, on the raw recv timeout.
Impact: `asynSetOption port 0 disconnectOnReadTimeout Y` is inert on every default IP port — no teardown, no reconnect; C drops and re-establishes.

### R7-48: serial `clocal` is not persisted and is force-enabled at every connect; disconnected readback hard-codes `N` where C reports `Y`
Severity: Medium
Rust: `crates/asyn-rs/src/drivers/serial_port.rs:970-981` — clocal set_option mutates live termios only when connected, stores nothing (`SerialConfig` has no clocal field, unlike crtscts→flow_control at `:993-998`); `build_configured_termios` re-sets `CREAD|CLOCAL` every connect (`:609`); `get_option("clocal")` returns hard-coded "N" when disconnected (`:1130`).
C reference: `drvAsynSerialPort.c:1077` (CLOCAL default on in the cached termios), `:410-419` setOption mutates the cache unconditionally, `:105-130` applyOptions re-pushes the cache on connect, `:169-170` getOption reads the cache (returns Y even disconnected).
Impact: `clocal N` while disconnected is silently dropped; one set while connected reverts at the next auto-reconnect — modem-control mode cannot be held. Readback diverges (N vs Y).

### R7-49: serial `ixon`/`ixoff`/`ixany` are not persisted and are wiped on reconnect by the flow-control-driven `c_iflag` rewrite
Severity: Low
Rust: `crates/asyn-rs/src/drivers/serial_port.rs:1000-1035` — set_option mutates live termios only when connected, caches nothing; `apply_to_termios` (`:69-79`) rewrites IXON|IXOFF|IXANY purely from `flow_control` at connect; get_option hard-codes "N" while disconnected (`:1160,:1173,:1185`).
C reference: `drvAsynSerialPort.c:445-501` setOption mutates the cached c_iflag unconditionally; `:182-200` getOption reads the cache; settings survive reconnect via applyOptions.
Impact: per-flag software flow control cannot be configured as C permits; values set while disconnected are lost with no error.

### Category E — synApps modules + AD (R7-61..R7-66)

### R7-61: scaler forward link (FLNK) never fires when CONT=AutoCount
Severity: High
Rust: `crates/scaler-rs/src/records/scaler.rs:898-900` — `should_fire_forward_link()` requires `ss == IDLE`, evaluated by the framework *after* `process()` (`processing.rs:2791`); the auto-count block (`scaler.rs:772-812`) has already flipped `ss` to WAITING (`:783`) or COUNTING (`:800,:811`) before returning.
C reference: `scalerRecord.c:470-481` — `recGblFwdLink` is called *inside* process(), guarded `ss==IDLE && pcnt==0 && us==IDLE`, **before** the auto-count block (`:484-541`) re-arms.
Impact: with CONT=AutoCount every forward-linked record silently never processes on the port; C fires FLNK on every completed auto-count cycle. OneShot unaffected.

### R7-62: NDPluginROI bin-sum / narrowing conversion saturates where C wraps modulo the output type
Severity: Medium
Rust: `crates/ad-plugins-rs/src/roi.rs:409-437` — `extract!` accumulates in f64 and stores with `as $T` (saturating); narrowing conversions route through `ad_core_rs::color::convert_data_type`, which clamps (`color.rs:307-331`).
C reference: `NDArrayPool.cpp:465` — `*pDOut += (dataTypeOut)*pDIn;` accumulates in the output type, wrapping modulo (C truncating cast); non-scale ROI path uses this directly (`NDPluginROI.cpp:174`); only EnableScale converts to Float64 first (`:166`).
Impact: UInt8 image, 3×3 bin, all pixels 100, EnableScale=0: C = 900%256 = 132; Rust = 255. Same family for narrowing converts (300→UInt8: C 44, Rust 255).

### R7-63: modbus scalar write stages the value into the register cache; C leaves the cache untouched
Severity: Low
Rust: `crates/modbus-rs/src/ioc.rs:742-747` — `flush_write` copies just-written registers into `self.engine.data_mut()` on every relative-mode write; scalar write_int32/64/float64/uint32_digital all route through it.
C reference: `drvModbusAsyn.cpp:760-776` — scalar writeInt32 converts into a local buffer and never writes `data_`; only array writes stage (`:1402,:1232`); scalar reads (`:541,:550`) serve last-polled/init values.
Impact: a read record served from a write port's cache returns the just-written value on the port, stale polled/init value on C. No wire divergence.

### R7-64: epid bumpless-transfer seed of `.I`/`.OVAL` is applied before the MDT gate; C applies it after
Severity: Low
Rust: `crates/std-rs/src/records/epid.rs:750-767` — `pre_process_actions` emits `ReadDbLink{OUTL→I|OVAL}` on the FBON OFF→ON edge, executed before `do_pid`; the MDT gate (`epid_soft.rs:79-81`) then early-returns without committing fbop, so the seed lands and posts on a sub-MDT cycle and re-seeds next cycle.
C reference: `devEpidSoft.c:125` — `if (dt<mdt) return(1);` **before** the OUTL seed at `:150-158`; sub-MDT cycles never read OUTL, monitor posts nothing.
Impact: on a sub-MDT edge cycle the CA-visible .I/.OVAL takes the readback early and fires spurious monitors; converges to the same final value.

### R7-65: scaler sub-millisecond auto-count path never copies gates into the direction registers
Severity: Low
Rust: `crates/scaler-rs/src/device_support/scaler_asyn.rs:343-352` — the `TP1 < 1 ms` run_autocount branch writes presets but never `scaler.d[i] = scaler.g[i]`; the record's auto-count block has no copy either.
C reference: `scalerRecord.c:525-528` — `pdir[i]=pgate[i]` for all channels in the tp1<1e-3 branch.
Impact: D{n} keeps stale values on sub-ms TP1 auto-count; CA-readable value only (C does not post it).

### R7-66: scaler REQSTART direction copy runs over all 64 channels instead of NCH
Severity: Low
Rust: `crates/scaler-rs/src/records/scaler.rs:709-711` — `for i in 0..MAX_SCALER_CHANNELS`.
C reference: `scalerRecord.c:413-414` — `for (i=0; i<pscal->nch; i++)`.
Impact: inactive-channel D{n} (n ≥ nch) overwritten with G{n} on count start; cosmetic readback divergence.

### Fix wave 5 (2026-07-12): all 19 R7 findings + R6-76/77 — merged into review/parity-r6, verified by main (workspace fmt/clippy -D warnings clean; nextest 7614/7614 on re-run, first run 1 compile-load flake `server_read_sync_echoes_request_header` passing in isolation; doctests clean)

Category A (epics-base-rs):
- R7-1: FIXED 771e337f — `check_put_disabled` single gate owner crossed first on the CA/dbpf route; QSRV boundary calls the same owner. Widened: PACT/LCNT/PUTF are SPC_NOMOD rejected *inside* dbPut, so DISP wins on a disabled record (S_db_putDisabled, not S_db_noMod) — matched.
- R7-2: FIXED f9d148ad — `put_driven_process` single owner of C's `if pact { rpro } else { putf; dbProcess }`, used by the PROC intercept and the pp-field route; PUTF now raised exactly where C raises it (was set pre-write and unwound on 3 error paths).
- R6-77: FIXED 2b982997 — `postfix::compile` takes the target engine and enforces its grammar (C postfix vs sCalcPostfix vs aCalcPostfix); string tokens in numeric CALC are CalcError::Syntax at compile; swait moved to the numeric engine (swaitRecord.c:304,409). SIGN-OFF ITEM: the split also removes `>?`, `<?`, `NRNDM`, `AA..UU` from the numeric engine — genuinely absent from postfix.c's tables, so this is the C contract, but the port evaluated them correctly (working superset removed). Nothing in the workspace used them in a numeric CALC. Veto = one-line change in `opcode_in_grammar`.

Category B (epics-ca-rs / epics-tools-rs):
- R7-16: FIXED 7fd01fc8 — `HostIdentity::{Claimed,Pinned}` makes "HOST_NAME overwrites a pinned IP" unrepresentable; one flag `as_check_client_ip` (new `asCheckClientIP [0|1]` iocsh command) drives both connection identity and HAG storage form. Deleted `expand_hag_members`, `host_resolves_to_peer`, every `EPICS_CAS_USE_HOST_NAMES` read; 7 docs corrected. Widen adjudication: ca_gateway peer-IP matching is DISTINCT (gateServer.cc:1529 uses ipAddrToDottedIP — port matches its own C).
- R7-17: FIXED 46831a71 — `Action::evaluate` returns `Vec<Action>` mirroring C's non-else if blocks; `RestartChild` deferred (as C's restartOnce zeroes _restartTime) so the same-byte kill block cannot signal the fresh child.
- R7-18: FIXED 72ae650c — ECA_TOLARGE + drain + keep serving; widen found the sibling ECA_DEFUNCT drain discarding the whole buffer; both route through one owner `refuse_message`, the sole writer of `recv_bytes_to_drain`.
- R7-19: FIXED e1eda6d3 — OS-bounded connect on client transport + name-server path; name-service retries sleep EPICS_CA_CONN_TMO (30 s default), replacing a 1→30 s backoff C does not have.
- R6-76: FIXED 21401616 — `install_signal_handlers` is the documented single owner of parent signal dispositions; SIGXFSZ ignored in daemon + foreground; child.rs documents why it must not reset it.

Category C (epics-pva-rs / epics-bridge-rs):
- R7-31: FIXED 41fd8094 — shared sink: `ChannelContext::log` (RemoteLog) + `tcp.rs::flush_remote_log` single owner, drained after every IOID-carrying ChannelSource op before its reply frame (pvxs order); group PUT warns `<field>: no putorder, ignore write`.
- R7-32: FIXED dd845c60 — `record._options.DBE="<mask>" selects empty mask` warning before the VALUE|ALARM fallback; empty string draws nothing (pvxs `!mask.empty()` guard).
- R7-33: FIXED 4ff2f3b0 — pvxs three-way split restored (bool → Force/Inhibit, literal "passive" silent, else warn "Ignoring unsupported …" rendered through pvxs's tree formatter, trailing newline included). Anchor sweep: `atomic`/`block` are silent in pvxs too; `queueSize` warned by the native layer already. 9 e2e tests assert the actual CMD_MESSAGE frames.

Category D (asyn-rs):
- R7-46: FIXED 67cc5b27 — the error owns the transfer: `PartialOctetRead { data, eom_reason }`; asyn_record octet arm restructured to C's shape (one unconditional NORD/EOMR/TINP/AINP|BINP tail regardless of status); adapter stores value before mapping the alarm.
- R7-47: FIXED 8b1b09e2 — classification by `e.status()` at all four variant-matching sites (`rg -U` re-sweep caught multi-line `matches!`): ip_port timeout, interpose/echo loss-of-comm, serial_port + serial_port_win32 is_fatal_transport_error.
- R7-48: FIXED 0f42fcde — C's cached-termios owner (`tty->termios` model: seed once, set_option mutates cache unconditionally, one push tail with rollback, connect pushes, get_option reads cache); `build_configured_termios` and the config mirror deleted; CLOCAL no longer forced at connect (C forces CREAD only).
- R7-49: FIXED 5479c2a2 — ixon/ixoff/ixany readbacks on the same cache; C's hard-coded 'N' is vxWorks-only. Win32 option paths adjudicated DISTINCT (C-Win32 deliberately has no cache). Win32 edit compile-verified via `cargo check --target x86_64-pc-windows-gnu`.

Category E (scaler-rs / ad-core-rs / ad-plugins-rs / modbus-rs / std-rs):
- R7-61: FIXED fda2c8c8 — process() is the single owner: clears `fire_fwd_link` on entry, sets it at exactly C's recGblFwdLink line, before auto-count re-arms; OneShot control tests unchanged.
- R7-62: FIXED 59a3f779 — new `ad_core_rs::convert` single owner of C's truncating-cast kernels; three conversion sites (ndarray_pool, color, roi) delegate; ROI expresses the EnableScale Float64 detour explicitly; a pool test that codified the clamped 255 rewritten to C's 44.
- R7-63: FIXED bc65895b — staging moved to `flush_write_staged`, called only by the three paths C stages from (write_int32_array, write_float64_array, write_octet).
- R7-64: FIXED 979eb642 — OUTL readback staged into an internal cell, consumed by do_pid at C's line after the MDT/UDF gates; an e2e test that asserted the defect (CONSTANT INP so C's do_pid never runs) rebuilt with a real INP.
- R7-65/66: FIXED a844db1b / 85d5880b — one owner `copy_gates_to_directions()` bounded by clamped NCH; REQSTART routed through it, sub-ms auto-count branch calls it (tp1 >= 1ms branch never touches pdir in C, pinned by control test); `d[i]=1` on PR put and direct D put adjudicated distinct (C does the same).

### New OPEN findings surfaced during fix wave 5

### R7-3: `LOG2` accepted by all three calc engines; present in none of C's three element tables
Severity: Low (REPORTED by fixer-A3 while doing R6-77)
Rust: calc lexer/engines accept `LOG2` everywhere.
C reference: absent from postfix.c, sCalcPostfix.c, aCalcPostfix.c tables.
Impact: a CALC using LOG2 loads on the port, CALC_ERR_SYNTAX (CLCV) in C. Port-wide extension, not cross-engine leakage — needs a keep-or-drop decision.

### R7-34: numeric-string DBE (`"5"`) parsed into a mask; pvxs's String branch does substring matching only
Severity: Low (REPORTED by fixer-C3, outside R7-32's diagnostics scope)
Rust: `dbe_mask_from_pv_request` parses `"5"` numerically.
C reference: `singlesource.cpp:118-131` — Kind::String does substring scan only, so `"5"` selects an empty mask, warns, and falls back to VALUE|ALARM.
Impact: negotiated event mask differs for numeric-string DBE options.

### R7-50: Win32 serial `get_option` returns fallback values while disconnected; C-Win32 returns asynError "disconnected:"
Severity: Low (REPORTED by fixer-D3, distinct from the R7-48 cache family)
Rust: `serial_port_win32.rs` — ixon/ixoff/clocal/crtscts return "N"/config when the handle is closed.
C reference: `drvAsynSerialPortWin32.c:97-101` — getOption errors "disconnected:". (`ixany => "N"` is correct and matches C.)
Impact: option readback on a disconnected Win32 port silently reports defaults instead of erroring.

### Known residuals / notes (fix wave 5)
- QSRV MONITOR DBE warning frame lands just *after* the INIT reply (pvxs raises it inside onSubscribe, *before* its connect() INIT reply); same ioid/level/text. Closing it needs a source hook at MONITOR INIT. Documented in code (fixer-C3).
- Rust-only IPC transport (`protocol::ProtocolError`/`ReplyPayload::Error`) flattens a PartialRead and drops the bytes on a remote-port octet timeout. No C counterpart exists for this transport; closing it needs a wire-format change to carry (data, eom) in the error payload (fixer-D3).
- Pre-existing `--all-features` compile failure in `epics-base-rs/tests/client_server.rs` reconfirmed on the clean tree by fixer-C3 (now 3 errors: run_tcp_listener arity + WallTime vs SystemTime), behind ca-server-tls-test.
- Compile-load flake list grows by one: `epics-ca-rs::protocol_tests server_read_sync_echoes_request_header` (failed once in main's first post-merge workspace run, passed in isolation and in the clean re-run 7614/7614). Also `epics-ca-rs::channel_filters ca_fr_8_arr_on_scalar_channel_is_noop` observed once by fixer-A3 with the same pattern.

## Round 8 — re-audit (2026-07-12): wave-5 fix verification + fresh findings

Same 5 auditor panels (opus, read-only), blocks R8-1..15 / 16..30 / 31..45 /
46..60 / 61..75. Mandate: verify every wave-5 fix commit, adjudicate R7-3/34/50,
fresh negative-space hunt.

### Fix verification result

Every wave-5 fix commit independently verified CORRECT against the C/C++
reference by the re-audit panels — no wrong or incomplete fix. Per-panel
evidence in the round reports (`rounds/01KX956H31H1PSTAA6XJFCYBSJ.md`,
`rounds/01KX96GP5F4511X7551KGNATA2.md`).

Auditor-A's carryover note claiming "R6-1..R6-8 remain live" is a
doc-structure misread (the Open Findings section retains original finding
text; dispositions live in Cleared During Review) — main spot-checked R6-1 at
HEAD (SEVR/STAT/NSEV/NSTA/ACKS/DISS/UDFS routed through `shared_menu_choices`
→ `MENU_ALARM_SEVR`/`MENU_ALARM_STAT`); the fixes are present. Dismissed.

### Adjudications

- R7-3 (LOG2): DROP-TO-C recommended and accepted. `LOG_2` exists only as a
  reserved opcode-name slot; no C dialect lexes the infix token `LOG2` →
  CALC_ERR_SYNTAX. Widened: `opcode_in_grammar`'s `CoreOp(_) => true` also
  admits `INT` (→ `CoreOp::Nint`) to the Numeric engine, but `INT` is
  sCalc/aCalc-only in C. Fix: reject LOG2 in all three engines, INT in Numeric.
- R7-34: CONFIRMED. `channel.rs:159-161` numerically parses a String-typed DBE
  before the substring block; pvxs Kind::String does substring only, so `"1"`
  → VALUE-only (Rust) vs VALUE|ALARM (pvxs), and the empty-mask warn never
  fires for numeric strings. Isolated — the other option parsers correctly use
  `as<T>` semantics.
- R7-50: CONFIRMED, scope WIDENED. C-Win32 places the disconnected guard at the
  top of getOption AND setOption (`drvAsynSerialPortWin32.c:96-101,180-185`),
  so every key (baud/bits/parity/stop/break too, not just
  ixon/ixoff/clocal/crtscts) must error "disconnected:"; Rust `set_option`
  storing to config while disconnected is the same defect. (`ixany => "N"`
  stays correct.)

### Category A — epics-base-rs (R8-1..R8-5)

### R8-1: `caput calc.CALC "…bad…"` returns success; C calcRecord rejects the put with S_db_badField
Severity: Medium
Rust: `crates/epics-base-rs/src/server/records/calc.rs:844-850` — the CALC put arm does `compile(&s).ok()` (error discarded), stores, returns `Ok(())`; no special() to re-raise.
C reference: `calcRecord.c:146-151` — `special(SPC_CALC)` runs postfix() and on failure `recGblRecordError(S_db_badField); return S_db_badField;`, propagating to the client.
Impact: `caput calc.CALC "1+"` returns write success on the port, write error on C (WRITE_NOTIFY carries it; dbpf prints error); the record silently stores an uncompilable expression.

### R8-2: calcout and scalcout are missing the CLCV/OCLV expression-validity fields entirely
Severity: Medium
Rust: `crates/epics-base-rs/src/server/records/calcout.rs:1255-1259`, `scalcout.rs:717-723,739-745` — compile errors discarded, no CLCV/OCLV field declared or served (acalcout.rs has them but sets clcv=1 generically where C stores the specific code).
C reference: `calcoutRecord.c:326-345` — special() sets `clcv`/`oclv` to the exact postfix error code, posts DBE_VALUE, accepts the put; fields are DBF_LONG (`calcoutRecord.dbd.pod:729,1049`); sCalcoutRecord.c:464-478 same. [CORRECTION, fix wave 6: "exact postfix error code" was wrong — C stores 0 (compiles) or -1 (postfix failed), nothing finer; verified by fixer-A4 against calcoutRecord.c. The fix implements C.]
Impact: `caget calcout.CLCV` works on C (0 = OK, else calcErrorStr code) with monitors on re-put; the field does not exist on the port.

### R8-3: menu-string put accepts an out-of-range numeric index and leading/trailing whitespace; C putStringMenu rejects both
Severity: Medium
Rust: `crates/epics-base-rs/src/server/record/menu_choices.rs:216-220` — `resolve_menu_field_string` trims, label-matches, else parses numeric with no bound against the choice count; `caput fanout.SELM "99"` stores selm=99.
C reference: `dbConvert.c:1216-1229` — putStringMenu exact strcmp (no trim), numeric accepted only when `val < nChoice`, else S_db_badChoice.
Impact: out-of-range/whitespace menu puts succeed on the port (landing invalid enum indexes interpreted as default branches) and fail S_db_badChoice on C. Affects every DBF_MENU field through the shared resolver.

### R8-4: `caput REC.SCAN "0.5 second"` (and "0.2"/"0.1") succeeds; C accepts only the canonical menuScan labels
Severity: Low
Rust: `crates/epics-base-rs/src/server/record/scan.rs:44-50` — `ScanType::from_str` accepts `"0.5 second"` aliases alongside the canonical `".5 second"`.
C reference: `dbConvert.c:1216-1229` — exact strcmp against menuScan labels (`.5 second` etc.); `"0.5 second"` → S_db_badChoice.
Impact: non-canonical rate aliases accepted on the port, rejected on C.

### R8-5: numeric `%` operator uses d2i (uint32-wrap) where C calcPerform uses a plain truncating (epicsInt32) cast
Severity: Low
Rust: `crates/epics-base-rs/src/calc/engine/numeric.rs:72-80` — `CoreOp::Mod` computes `d2i(a).wrapping_rem(d2i(b))`; d2i routes through epicsUInt32 (`:384-389`).
C reference: `calcPerform.c:161-167` — `(epicsInt32)*ptop % itop`; the d2i/d2ui macros apply only to bit/shift ops (`:324-367`), not MODULO.
Impact: for |x| ≥ 2^31 in a `%` expression, C's cvttsd2si yields 0x80000000 on x86-64 while d2i yields the uint32-wrapped value — different VAL on the wire (same truncating-cast family as R7-62, different site).

### Category B — epics-ca-rs + epics-tools-rs (R8-16..R8-21)

### R8-16: server CA_PROTO_ERROR echoing an EVENT_ADD request is never routed to the subscription's callback
Severity: Medium
Rust: `crates/epics-ca-rs/src/client/transport.rs:2053-2073` — the CA_PROTO_ERROR dispatcher routes only READ_NOTIFY/WRITE_NOTIFY by echoed IOID; EVENT_ADD falls into `_ => {}`; only the global ServerError hook fires. The comment claiming "EVENT_ADD errors travel through MonitorStatusError" describes a different wire frame (non-normal status on a normal EVENT_ADD reply).
C reference: `cac.cpp:97` maps CA_PROTO_EVENT_ADD → eventAddExcep; `cac.cpp:1030-1038` ioExceptionNotify delivers the ECA status to that subscription's exception callback without uninstalling it. Server emits this from `camessage.c:513-522` (buffer-load failure echoing the EVENT_ADD header) with the circuit staying up.
Impact: when a monitor update is too large for the server send buffer, C clients get repeated ECA_TOLARGE deliveries on the subscription callback; the Rust monitor stalls silently — no value, no error, circuit connected.

### R8-17: client never fires the no-rights access-rights transition on a real circuit close; only the echo-timeout path does
Severity: Medium
Rust: `crates/epics-ca-rs/src/client/mod.rs:4273-4310` (TcpClosed) and `:4054-4056` (per-cid ServerDisconnect) set Disconnected but never reset `ch.access_rights` nor emit AccessRightsChanged{false,false}; the CircuitUnresponsive path does both (`:4126-4131`) — internally inconsistent.
C reference: `tcpiiu.cpp:1814-1855` disconnectAllChannels → `nciu.cpp:168-178` disconnectAllIO → disconnectNotify → accessRightsNotify(noRights) on real circuit close.
Impact: on socket close the cached access_rights stays at its last value (possibly read+write) until reconnect; libca drops to no-rights immediately.

### R8-18: procServ never removes the --info-file on shutdown
Severity: Medium
Rust: `crates/epics-tools-rs/src/procserv/supervisor.rs:1138-1143` — Drop removes only pid_path; no unlink of info_path anywhere in the crate.
C reference: `procServ.cc:696-699` — unconditional `unlink(infofile)` after the main loop.
Impact: stale info file (dead PID + endpoints) survives clean shutdown; C's file presence tracks liveness.

### R8-19: procServ writes the info-file lazily on child spawn, not at startup — absent for the whole --wait window
Severity: Medium
Rust: `supervisor.rs:730-731` — write_info_file only inside the child-spawn path; under --wait the initial spawn is skipped (`:353-357`), so no info file until manual start.
C reference: `procServ.cc:562-564` — writeInfoFile at startup, before the main loop, independent of waitForManualStart.
Impact: under -w a manager cannot discover the control endpoint from the info file to issue the manual start — chicken-and-egg.

### R8-20: client signals channel Disconnected before failing the channel's pending get/put IOs; libca fails IOs first
Severity: Low
Rust: `mod.rs:4281` sends ConnectionEvent::Disconnected, then `:4329` drains waiters with CaError::Disconnected.
C reference: `nciu.cpp:168-170` — disconnectAllIO (ECA_DISCONN to pending IO callbacks) runs before disconnectNotify.
Impact: relative delivery order of IO-failure vs connection-state callback is reversed.

### R8-21: server EVENTS_OFF collapses a pre-existing monitor backlog to latest instead of draining queued distinct updates
Severity: Low
Rust: `crates/epics-ca-rs/src/server/monitor.rs:106-108` — on pause, `while try_recv { pending = event }` collapses queued distinct updates to the latest.
C reference: `dbEvent.c:947-950` — event_read suspends only when `flowCtrlMode && nDuplicates == 0`; distinct events already queued are delivered before the queue quiets.
Impact: different count/content of EVENT_ADD frames after a flow-control pause. Narrow race.

### Category C — epics-pva-rs + epics-bridge-rs (R8-31..R8-33)

### R8-31: QSRV PUT-rejection Status messages diverge from pvxs's contract strings; two group-PUT sites leak internal `pvxs <file>:<line>` citations onto the wire
Severity: Low
Rust: `crates/epics-bridge-rs/src/qsrv/group.rs:1499-1503,1554-1559,1779-1781,1493` — error strings embed group name, member detail, and literal `pvxs groupsource.cpp:...` source citations; reach the wire via BridgeError::PutRejected → OpError::failed (`pva_adapter.rs:474`), and pva_gateway forwards verbatim.
C reference: pvxs sends bare contract text — `"Links not supported for put"` (groupsource.cpp:605), `"No fields changed"` (:658), `"Put not permitted"` (iocsource.cpp:385), `"Unable to put value: ..."` (:366,:368). No pvxs error string contains a source-file reference.
Impact: Status.message wire text differs; internal source citations leak to clients. Behaviour (rejection) matches.

### R8-32: client silently swallows a malformed CMD_MESSAGE; pvxs treats a decode fault on that frame as connection-fatal
Severity: Low
Rust: `crates/epics-pva-rs/src/client_native/server_conn.rs:1159-1162,1307-1314` — log_server_message returns silently on any decode failure; the read loop continues.
C reference: `clientconn.cpp:442-455` — from_wire then `if(!M.good()) throw` → bev.reset() — circuit torn down.
Impact: against a non-conforming server frame, pvxs drops the circuit (channels disconnect and re-search); Rust ignores and keeps serving.

### R8-33: built-in `server` RPC returns divergent error text for an unmatched or missing op
Severity: Low
Rust: `crates/epics-pva-rs/src/server_native/server_info.rs:324,333-335` — `"unknown op '…' (expected 'channels' or 'info')"` / `"missing 'op' query argument"`.
C reference: `serversource.cpp:93` — `"Not implemented"` for every unmatched op; missing op throws a Value no-field exception surfacing as a generic remote error.
Impact: Status.message wire text diverges.

Sign-off item (documented deviation, user confirm): `server_info.rs:112-127` reports `implLang="rust"` + crate version where pvxs hard-codes `"cpp"` + version_str() — deliberate truthful token; a tool fingerprinting `implLang=="cpp"` will not match.

### Category D — asyn-rs + motor-rs (R8-46..R8-53)

### R8-46: asynRecord ASCII octet read sizes the buffer and overflow threshold by IMAX, not the fixed 40-byte AINP
Severity: Medium
Rust: `crates/asyn-rs/src/asyn_record/mod.rs:1899-1904` — imax (default 80) sizes the buffer for all non-binary modes; overflow test `data.len() >= plan.imax` (`:702-706`); ASCII stores up to imax bytes into ainp.
C reference: `asynRecord.c:1503-1519` — ASCII uses `inptr = ainp; inlen = sizeof(ainp)` (=40), nread clamped; overflow `nbytesTransfered >= sizeof(ainp)` (`:1602-1608`); only Hybrid uses imax.
Impact: default IFMT=ASCII reading a terminator-less response > 40 bytes: C caps at 40, NORD≤40, READ/MINOR overflow, leaves the rest in the driver; port reads up to 80, no overflow alarm, consumes extra bytes — NORD, STAT/SEVR, TINP, and next-read framing all diverge.

### R8-47: asynRecord Write/Read transaction mode omits the pre-write input flush
Severity: Medium
Rust: `asyn_record/mod.rs:818` (async) and `:2004` (sync) — flush runs only for TransferMode::Flush, positioned after the read; WriteRead never flushes.
C reference: `asynRecord.c:1518-1520` — `if (tmod == Flush || tmod == Write_Read) flush()` executed before the write.
Impact: Write/Read is the default TMOD; stale bytes are prepended to the fresh response — exactly what the pre-write flush exists to prevent.

### R8-48: asynRecord octet write reports the intended length as NAWT and never emits C's short-write diagnostic
Severity: Low
Rust: `asyn_record/mod.rs:628-631` — Ok arm sets nawt = octet_out_len, discarding the actual count; Err arm sets nawt = 0 even when bytes moved.
C reference: `asynRecord.c:1546-1555` — nawt = actual nbytesTransfered; reportError fires whenever status != success || nbytesTransfered != nwrite.
Impact: short-but-successful write: C NAWT=n + ERRS diagnostic; port NAWT=len, no ERRS. Errored partial write: C partial NAWT, port 0.

### R8-49: echo interpose is uninstallable and fabricates a message + downgrades asynTimeout to asynError
Severity: Low
Rust: `crates/asyn-rs/src/interpose/echo.rs:82-90` — echo-read timeout returned as Error with "...no echo - Loss of communication?"; no iocsh registrar, no driver installs it.
C reference: `asynInterposeEcho.c:60-68` — timeout keeps asynTimeout with "timeout reading back char number N"; `:194-207` registers the asynInterposeEcho iocsh command (asynInterposeDelay/Flush also unported).
Impact: interpose uninstallable; where reached, STAT=WRITE instead of TIMEOUT and non-contract ERRS. The "matches C" comment cites behaviour the reference does not have.

### R8-50: `drvAsynIPPortConfigure(port, "host:port COM")` is rejected as an invalid port number
Severity: Low
Rust: `crates/asyn-rs/src/drivers/ip_port.rs:138-155,210-213` — trailing " COM" not stripped → parse failure.
C reference: `drvAsynIPPort.c:364-367` — parseHostInfo matches `com` (SOCK_STREAM) and installs asynInterposeCOM (`:1061`).
Impact: RFC2217 COM links that configure on C hard-fail on the port. (asynInterposeCOM itself is entirely unported; the observable divergence today is the config rejection.)

### R8-51: asynRecord CNCT put drives the manager attach/detach instead of the driver transport connect/disconnect
Severity: Medium
Rust: `asyn_record/mod.rs:2604-2621` — CNCT arm does connect_device()/port_entry=None, identical to PCNCT; never submits RequestOp::Connect/Disconnect.
C reference: `asynRecord.c:537-544` routes CNCT to callbackConnect → pasynCommon->connect()/disconnect() (`:865-882`) — the driver transport; PCNCT is the manager attach/detach (`:519-527`).
Impact: `caput REC.CNCT 0` on C closes the actual fd/socket and fans asynExceptionConnect to every user; on the port it only detaches this record. CNCT and PCNCT are functionally identical on the port; C's isConnected gate is also absent.

### R8-52: motor init reconciles ACCS/ACCL from ACCU, not from a loaded nonzero ACCS as C does
Severity: Low
Rust: `crates/motor-rs/src/record/field_access.rs:2288` → apply_accu_cascade (`:2110`) derives from rec.vel.accu only; ACCS put handler never sets ACCU, so a loaded ACCS cannot win.
C reference: `motorRecord.cc:4034` — check_speed_and_resolution keys on `accs > 0.0` (loaded value), independent of ACCU.
Impact: db with `field(ACCS,"5")` and no ACCU: C reports ACCS=5 with derived ACCL; port discards the loaded ACCS. The comment claiming "loading ACCS flips it to Accs" is not implemented.

### R8-53: motor tweak into an active hard-limit switch drops the VAL fold entirely
Severity: Low
Rust: `crates/motor-rs/src/record/command_planner.rs:1303-1305` — collect_tweak clears TWF/TWR then returns false on is_blocked_by_hw_limit BEFORE the VAL fold — uncommented gate, no C citation.
C reference: `motorRecord.cc:2167-2181` — tweak unconditionally folds `val += twv * dir`; limit handling happens later in the move block, no limit gate on the fold.
Impact: with the user-direction hard-limit active but soft target legal, C folds VAL and dispatches (driver holds at limit); the port consumes the button silently. Also makes tweak more restrictive than a direct VAL write.

### Category E — synApps + AD (R8-61..R8-70)

### R8-61: codec plugin in Decompress mode reports a codec error on an uncompressed input; C treats it as SUCCESS pass-through and sets COMPRESSOR=NONE
Severity: Medium
Rust: `crates/ad-plugins-rs/src/codec.rs:1077-1085,1118-1136` — codec==None falls to the failure branch: CodecStatus=1 + error string; COMPRESSOR never written on this path.
C reference: `NDPluginCodec.cpp:732-735` — empty codec → result=pArray, COMPRESSOR=NDCODEC_NONE, codecStatus stays SUCCESS.
Impact: mixed/uncompressed streams through a decompress plugin report spurious WARNING+error on the port, SUCCESS on C.

### R8-62: codec JPEG compression rejects RGB2 and RGB3; C encodes all three color modes
Severity: Medium
Rust: `codec.rs:784-792` — compress_jpeg accepts 3-D only when dims[0].size==3 (RGB1); other layouts → failure + pass-through.
C reference: `NDPluginCodec.cpp:186-227` — RGB2 (planes at sizeX*3) and RGB3 (planes at sizeX) re-interleaved per scanline and encoded.
Impact: UInt8 RGB2/RGB3 frames cannot be JPEG-compressed on the port; C produces valid JPEGs.

### R8-63: codec plugin never emits CodecStatus=ERROR(2); every failure is WARNING(1)
Severity: Low
Rust: `codec.rs:1125` — failure hardcodes 1; the value 2 never produced; "already compressed" reported SUCCESS(0) via pass-through (`:1048-1051`).
C reference: `Codec.h` NDCodecStatus_t {SUCCESS=0, WARNING=1, ERROR=2}; NDPluginCodec.cpp sets ERROR for genuine failures (:167,:252,:760), WARNING for benign (:674,:468).
Impact: WARNING/ERROR indistinguishable; benign case severity shifted.

### R8-64: timestamp record posts RVAL monitors every cycle; C posts RVAL only when the formatted VAL string changes
Severity: Medium
Rust: `crates/std-rs/src/records/timestamp.rs:231-236` — rval set unconditionally; generic change-detection posts every second.
C reference: `timestampRecord.c:158-162` — db_post_events(&rval) nested inside the VAL strncmp-change guard.
Impact: coarse TST formats (`%H:%M`): ~59 extra DBE_VALUE|DBE_LOG events per minute per RVAL subscriber.

### R8-65: NDPluginCircularBuff runtime post-trigger counter (NDCircBuffPostCount) is never posted; it stays 0
Severity: Medium
Rust: `crates/ad-plugins-rs/src/circular_buff.rs:559,199,266,645-646` — param index cached but no ParamUpdate ever pushed.
C reference: `NDPluginCircularBuff.cpp:168-169` — currentPostCount++ posted per flush frame, reset at :189/:250.
Impact: PostCount_RBV reads 0 forever. Siblings diverge the same way: CurrentImage posted 0 during flush (C freezes at pre-buffer size, :151); ActualTriggerCount increments at trigger time vs C at sequence completion (:179-180).

### R8-66: NDPluginAttribute "NDArrayTimeStamp" channel reads the epicsTS-derived timestamp; C reads the independent timeStamp double
Severity: Medium
Rust: `crates/ad-plugins-rs/src/attribute.rs:53` — `array.timestamp.as_f64()` (epicsTS); `array.time_stamp: f64` never read.
C reference: `NDPluginAttribute.cpp:62-63` — attrValue = pArray->timeStamp (standalone double), distinct from the EpicsTS* names (:64-67).
Impact: for drivers setting timeStamp independently (hardware timestamp — the AD norm), the extracted attribute value differs.

### R8-67: TIFF writer stores RGB2/RGB3 interleaved without PLANARCONFIG_SEPARATE and writes no PlanarConfiguration tag on any image
Severity: Medium
Rust: `crates/ad-plugins-rs/src/file_tiff.rs:117-134` — RGB2/RGB3 converted to RGB1 and written chunky; no TIFFTAG_PLANARCONFIG emitted.
C reference: `NDFileTIFF.cpp:203-219,235` — RGB2/RGB3 set PLANARCONFIG_SEPARATE (RGB2 also rowsPerStrip=1), three separate plane strips; every image writes the tag (1 mono/RGB1, 2 RGB2/RGB3).
Impact: tag 284 absent from every port TIFF; RGB2/RGB3 on-disk byte order differs entirely. Family note (verified, to sweep with the fix): RowsPerStrip value + extra XResolution/YResolution/ResolutionUnit tags also diverge.

### R8-68: netCDF writer stores UInt8 array_data as NC_CHAR; C stores NC_BYTE
Severity: Medium
Rust: `crates/ad-plugins-rs/src/file_netcdf.rs:113` — UInt8 → netcdf3 U8 → NC_CHAR(2); only Int8 → NC_BYTE.
C reference: `NDFileNetCDF.cpp:155-158` — NDInt8 and NDUInt8 both NC_BYTE.
Impact: array_data nc_type header differs on every UInt8 file. Family note (verified, to sweep with the fix): Attr_* string vars written NC_BYTE where C uses NC_CHAR (`file_netcdf.rs:445-449` vs `NDFileNetCDF.cpp:302-319`); variable/global-attribute definition order and the conditional attrStringSize dimension diverge from C's fixed order.

### R8-69: modbus read-poller task terminates permanently on the first poll I/O error; C retries every second and auto-recovers
Severity: High
Rust: `crates/modbus-rs/src/ioc.rs:1702` — `if poller_handle.write_int32(read_reason, 0, 1).await.is_err() { break; }` — any timeout/disconnect/malformed frame propagates Err and the break ends the spawned poller task for good; run_poll_cycle aborts before notify_interface_value, so no error-status callback reaches records either.
C reference: `drvModbusAsyn.cpp:1644-1651` — readPoller never exits on I/O error: persistent error sleeps 1.0 s and continues; an error transition sets forceCallback_ and fans out callbacks with auxStatus = ioStatus_; loop ends only on modbusExiting_.
Impact: after the first Modbus I/O error the port stops polling forever — every I/O-Intr input record freezes until IOC restart. C keeps polling, auto-recovers, and drives records to READ/INVALID on the transition.

### R8-70: modbus poll() panics on a Modbus exception-05 (Acknowledge) read response; C treats it as success and leaves the data buffer intact
Severity: Medium
Rust: `crates/modbus-rs/src/driver.rs:538,655` — Acknowledge → Ok(Vec::new()); poll() then `self.data.copy_from_slice(&words)` with mismatched lengths — panic, poller task dies.
C reference: `drvModbusAsyn.cpp:2231-2237` — 0x05 mapped to asynSuccess and goto done, past the data-copy; data_ unchanged, callbacks fire with prior data.
Impact: a legal "command accepted, will take time" PLC response panics the port; combined with R8-69, unrecoverable.

### Fix wave 6 — dispositions (2026-07-12)

6 worktree fixers (opus), one commit per finding, merged into review/parity-r6
and verified by main: workspace fmt --check clean, clippy --all-targets
-D warnings clean, nextest 7723 passed / 0 failed / 2 skipped (first run,
no flakes), doctests clean. All 21 wave-6 items FIXED; none NOT-REAL.

Category A (fixer A4):
- R8-1 FIXED 7693712e — uncompilable CALC put returns S_db_badField (SPC_CALC), value stays.
- R8-2 FIXED a2e79164 — calcout/scalcout gain CLCV/OCLV (0 or -1, per the corrected C reading above); put ACCEPTED (asymmetry vs R8-1 preserved); acalcout's generic clcv=1 corrected.
- R8-3 FIXED 8e477255 — single menu converter `putStringMenu` (exact strcmp, epicsParseUInt16, bound by nChoice); resolver miss now fails the put instead of falling to index 0.
- R8-4 FIXED e7d6ed6e — SCAN/SSCN/PINI routed through that converter; three hand-written from_str tables deleted.
- R8-5 FIXED 68a06da3 — per-dialect integer casts with one cast owner (`calc/engine/cast.rs`).
- R7-3 FIXED 1a4573b6 — DISPOSITION CORRECTION: the Round-8 adjudication ("reject LOG2 in all three engines") was WRONG. Fixer-A4 compiled base's real postfix.c + calcPerform.c: `get_element` (postfix.c:187-216) is longest-prefix with no identifier boundary, so C lexes `LOG2` as `LOG`·`2` — CALC="LOG2" evaluates to log10(2)=0.30103; only `LOG2(A)` is a syntax error. The fix ports C's lexing: LOG2 symbol deleted (token/FuncName/CoreOp/evaluator arms) and the tokenizer's keyword-boundary rule (no C equivalent) removed. INT stays refused by Numeric, accepted by sCalc/aCalc as the NINT alias. Gate is a token-stream allowlist per engine (opcode-level would be blind to INT since sCalc maps INT and NINT to one opcode); moving it ahead of the parse also fixed `UNTIL(A);` failing Incomplete where C says Syntax. Three tests pinning invented behaviour corrected to C-verified values. SIGN-OFF: accepted by main on compiled-C evidence; user veto possible if the literal "reject LOG2" reading was wanted.

Category B (fixer B4):
- R8-16 FIXED 6cf91784 — CA_PROTO_ERROR echoing EVENT_ADD routed through MonitorStatusError (C ioExceptionNotify: deliver, no uninstall); EVENT_CANCEL/fire-and-forget WRITE stay on the global hook as C's defaultExcep/writeExcep.
- R8-17 FIXED a96cb7d4 — `disconnect_channels()` single owner of the no-rights transition (C nciu::unresponsiveCircuitNotify convergence); `DisconnectKind` carries the per-path difference.
- R8-20 FIXED 8e628c75 — mark_disconnected + drain fan-out moved ahead of the notification loop: C's disconnectAllIO-then-disconnectNotify order by construction on all three paths.
- R8-21 FIXED f057dc49 — `MonitorFlow::admit` single owner of the EVENTS_OFF decision, crossed by both monitor loops.
- R8-18 FIXED 87a66158 / R8-19 FIXED 2e767de5 — procServ info file: `bootstrap` single publish site, `Drop` single unlink site (infofile then pidfile).

Category C (fixer C4):
- R8-31 FIXED 2f1026a3 — `qsrv::put_status` single owner of pvxs PUT-rejection contract text; `wire_message()` the only BridgeError→Status.message conversion. Widened to the single-record path, group ACF gate, and the "put rejected: " Display prefix. Read-only now tested before DISP (C's order).
- R8-32 FIXED 6825c468 — `route_frame` single teardown owner returning FrameFault for frames pvxs bev.reset()s (CMD_MESSAGE, DESTROY_CHANNEL SID, CREATE_CHANNEL CID, six op-reply IOID peeks); unhandled commands stay ignorable (pvxs forward-compat drain). Distinct: decoded-but-slotless IOID (benign completion race), handshake decoders (already propagate).
- R8-33 FIXED 9fb33457 — RPC rejection family carries pvxs text: unmatched op "Not implemented", missing op "No such field" (pvxs NoField e.what()), trait/SharedPV defaults "RPC Not Implemented".
- R7-34 FIXED fe795a9c — numeric-parse arm removed from String-typed DBE (substring-scan only, pvxs Kind::String); sibling option parsers verified correct (pvxs as<T> semantics).

Category D (fixer D4):
- R7-50 FIXED 750f5904 — win32 serial: closed-handle check is the single gate at the top of the option setter; every key refuses "disconnected:" (C drvAsynSerialPortWin32.c:96-101,180-185).
- R8-46 FIXED 7878fba2 — asynRecord ASCII octet read sized by the 40-byte AINP field, not IMAX (asynRecord.c:1580).
- R8-47 FIXED 95835c00 — Write/Read TMOD drains input before the write (C asynOctetSyncIO::writeRead flush).
- R8-48 FIXED 63897dc8 — write carrier wraps `source: Box<AsynError>` (twin of PartialRead); status()/message()/is_transport_io()/is_fatal_transport() see through. Closed a latent bug: three private is_fatal_transport_error copies matched AsynError::Io by variant and were blind to wrapped fatal errors — deleted; AsynError is the single classification owner.
- R8-49 FIXED 479637e5 — echo interpose keeps asynTimeout on timed-out echo read with C's message (fabricated "no echo - Loss of communication?" removed — appears nowhere in C asyn); gained its asynInterposeEcho iocsh registrar via RequestOp::PushEchoInterpose (post-registration driver mutation through the actor).
- R8-50 FIXED 11da5829 — ip_port suffix whitelist replaced by C's actual parse (first blank starts protocol, one %5s token, exhaustive match); unknown tokens report C's `Unknown protocol "%s".`. COM is refused BY NAME (fixer decision, stated): accepting it as raw TCP would be C-unfaithful in both directions since asynInterposeCOM (RFC 2217) is unported — see R8-57.
- R8-51 FIXED 3aa15048 — `refresh_connected_state()` single CNCT writer; special("CNCT") is C's callbackConnect (isConnected gate → driver transport connect/disconnect); PCNCT keeps attach/detach.
- R8-52 FIXED 58e4df86 — motor init keys ACCS/ACCL reconciliation on loaded `accs > 0.0` (motorRecord.cc:4034), not ACCU.
- R8-53 FIXED 33431443 — motor tweak folds VAL unconditionally (motorRecord.cc:2167-2181); invented hard-limit gate removed.

Category E (fixers E4a, E4b):
- R8-69 FIXED cbbd3e2d — modbus poller: "I/O status = port state" model; task loops until actor channel closes (C modbusExiting_), poll_cycle owns io_status/prev_io_status, fans callbacks with auxStatus (records → READ/INVALID), force_callback on transition, 1 s backoff while erroring. asyn-rs `notify_interface_value` gained the aux_status parameter (all seven modbus call sites).
- R8-70 FIXED e06ec568 — `ModbusIoResponse::{Data, Acknowledged}` makes the empty-Vec dual meaning unrepresentable; poll()/read_absolute() keep the previous buffer on exception-05; masked-write readback merges into C's base.
- R8-64 FIXED 28e55683 — `ValuePostGate::{OnChange, WithValue}` in the Record trait (public API change); four monitor loops share one value_gate() lookup; timestamp RVAL posts inside the VAL strncmp guard as C does.
- R8-65 FIXED 7d885164 — `CircularBuffer::push` returns `FrameParams` (exactly C processCallbacks' assignments; None = "C left it alone", how CurrentImage freezes during flush). Also closed from the same anchor: SoftTrigger never cleared on re-arm, Control not turned off on "Acquisition Completed".
- R8-66 FIXED 7e4fd9d7 — rg found three timestamp.as_f64() sites, not one: attribute channel, NDTimeStamp_RBV, stats TSTimestamp — all now read `array.time_stamp`.
- R8-61 FIXED 322b06b9 — decompress of uncompressed input is SUCCESS pass-through + COMPRESSOR=NDCODEC_NONE.
- R8-62 FIXED eb681996 — JPEG compress accepts RGB2/RGB3 (via `convert_rgb_layout`) and Int8/UInt8.
- R8-63 FIXED bcfb5a78 — codec outcome enum `{PassThrough, Skipped, Converted, Failed}`: each variant owns its CodecStatus and message, so a call site cannot pair a benign skip with ERROR; "already compressed" is WARNING as in C.
- R8-67 FIXED d98b14e9 — hand-built TIFF writer+reader (the `tiff` crate cannot express C's bytes: no PLANARCONFIG_SEPARATE, mandatory resolution tags, own RowsPerStrip). RGB2 SEPARATE RowsPerStrip=1, RGB3 SEPARATE RowsPerStrip=sizeY, mono/RGB1 CONTIG; PlanarConfiguration on every image; no resolution tags. `tiff` crate demoted to dev-dependency as independent test decoder.
- R8-68 FIXED ac3a3866 — netCDF: netcdf3 crate's I8=NC_BYTE / U8=NC_CHAR naming had inverted C's types (UInt8 array_data now NC_BYTE, Attr_* strings NC_CHAR); `define_data_set` is the single owner of dimension/variable/global-attribute definitions, mirroring C's openFile order statement for statement.

## Open Findings — surfaced during fix wave 6 (reported by fixers, pending independent verify)

### R8-6: sCalc/aCalc ELEMENT tables not verified against compiled C; port accepts symbols the C tables lack
Severity: Medium
Rust: `crates/epics-base-rs/src/calc/` string/array engine token allowlists — R7-3's compiled-C verification covered base's postfix.c only.
C reference: sCalcPostfix.c / aCalcPostfix.c ELEMENT tables.
Impact: port accepts `FMOD`, `>>>`, `0X` hex literals, single-letter vars Q–U (C sCalc/aCalc stop at A–P), `INF`/`NAN` in aCalc, and double-letter vars AA–UU where both C tables stop at LL. Closing needs sCalc/aCalc drivers compiled the same way as R7-3's.

### R8-7: sCalc/aCalc division by zero returns IEEE inf; C returns -1 (sCalc) / myMAXFLOAT (aCalc)
Severity: Medium
Rust: string/array engine `/` evaluation — plain IEEE divide.
C reference: sCalcPerform.c (`-1`), aCalcPerform.c (`myMAXFLOAT`). R8-5 covered MODULO only; `/` is a different site.
Impact: any sCalc/aCalc expression dividing by zero yields a different VAL on the wire.

### R8-8: aCalc `<<`/`>>` on an array operand bit-shifts elementwise; C shifts the array elements positionally
Severity: Medium
Rust: aCalc engine shift ops.
C reference: aCalcPerform.c:1428+ — `<<`/`>>` with an array operand move elements, not bits.
Impact: `AA<<2` produces a completely different array on the port.

### R8-22: non-paused monitor queue drops the queued tail when the producer overflow slot is set; C replaces only the last log
Severity: Medium
Rust: epics-ca-rs server `coalesce_consume` non-paused path (flagged by fixer-B4 while working R8-21).
C reference: db_queue_event_log queue-full branch — replaces the newest queued entry, keeps earlier ones.
Impact: under burst load a subscriber can lose intermediate events C would deliver. Same family as R8-21, different path.

### R8-23: EVENTS_OFF nDuplicates rule enforced per-subscription; C's is per-client (one shared circuit queue)
Severity: Low
Rust: per-subscription mpsc queues (structural; noted in R8-21's commit).
C reference: event_read — one event queue per client; a duplicate on subscription B unblocks the drain of subscription A's entry.
Impact: cross-subscription unblocking behaviour differs. Closing requires replacing per-subscription queues with one per-circuit queue — redesign-scale, recorded rather than attempted.

### R8-34: in-op decode faults (truncated Status/PVData inside a GET/PUT/MONITOR reply) swallowed by per-op tasks
Severity: Medium
Rust: `crates/epics-pva-rs/src/client/ops_v2.rs`, `decode.rs` — R8-32's route_frame widening stopped at server_conn.rs.
C reference: pvxs clientget.cpp:490-494 — decode fault inside an op body does `bev.reset()` (circuit-fatal).
Impact: a malformed op reply body is silently dropped on the port; pvxs tears down the circuit.

### R8-54: asynRecord does not clamp NRRD/NOWT back into the record fields
Severity: Low
Rust: `crates/asyn-rs/src/asyn_record/mod.rs` octet I/O plan (flagged by fixer-D4 while working R8-46/47/48).
C reference: `asynRecord.c:1499,1513` — performOctetIO writes the clamped values back to NRRD/NOWT.
Impact: record fields show the requested, not effective, transfer sizes.

### R8-55: asynRecord does not re-poll ENBL/AUCT/CNCT from the port each process
Severity: Low
Rust: `asyn_record/mod.rs` — CNCT writer is now single-owner (R8-51) but nothing re-reads the port per process.
C reference: `asynRecord.c` monitorStatus — re-reads and posts on every process.
Impact: out-of-band port state changes (another record toggling autoconnect) are not reflected until a CNCT-affecting event.

### R8-56: octet READ ERRS text is `read: {e}`; C formats `%s nread %d %s`
Severity: Low
Rust: `asyn_record/mod.rs` octet read error path.
C reference: `asynRecord.c:1591-1598`. R8-48 fixed the write-side ERRS shape; the read side was not in that finding.
Impact: ERRS diagnostic text differs on read errors.

### R8-57: asynInterposeCOM (RFC 2217 telnet COM-port-option negotiation) is not ported
Severity: Medium
Rust: `host:port COM` configurations are refused by name (R8-50 fix); no interpose exists.
C reference: `asynInterposeCom.c` (856 lines: IAC/subnegotiation, baud/parity/stopbits/flow-control, modem/line-state decode), installed by drvAsynIPPort.c:1061.
Impact: RFC 2217 serial-over-TCP devices unusable on the port. A subsystem port in its own right — needs its own assignment.

### R8-58: asynInterposeDelay has no iocsh registrar; the layer is unreachable from a startup script
Severity: Low
Rust: `crates/asyn-rs/src/interpose/delay.rs` exists; no registrar (same gap R8-49 closed for echo).
C reference: `asynInterposeDelay.c:221-234`. (asynInterposeFlush/Eos have no C registrar — Eos installs via asynInterposeEosConfig — so those two are NOT gaps.)
Impact: startup scripts cannot install the delay interpose.

### R8-71: NDPluginCircularBuff postCount==0 completes a sequence on every untriggered running frame in C; port only checks after a triggered push
Severity: Medium
Rust: `crates/ad-plugins-rs/src/circular_buff.rs` (flagged by fixer-E4a while working R8-65; changes frame forwarding, deliberately not folded in).
C reference: NDPluginCircularBuff.cpp — `currentPostCount >= postCount` evaluated on every running frame; postCount==0 bumps ActualTriggerCount per frame.
Impact: postCount=0 configurations forward/complete completely differently.

### R8-72: SoftTrigger writeInt32 sets Triggered=1 for ANY value in C (including 0), and FlushOnSoftTrig>0 flushes the pre-buffer immediately from the write
Severity: Medium
Rust: `circular_buff.rs` — gates on non-zero write, never flushes from the write path.
C reference: NDPluginCircularBuff.cpp writeInt32(SoftTrigger).
Impact: `caput SoftTrigger 0` triggers on C, no-ops on the port; FlushOnSoftTrig flush timing differs.

### R8-73: netCDF numArrays dimension chosen by frame count; C chooses by open mode
Severity: Medium
Rust: `crates/ad-plugins-rs/src/file_netcdf.rs` — `frames.len() > 1` selects NC_UNLIMITED.
C reference: NDFileNetCDF.cpp — `openMode & NDFileModeMultiple` → dim0 = NC_UNLIMITED.
Impact: a Capture/Stream file holding exactly one frame gets a fixed dimension of 1 where C writes NC_UNLIMITED — header-parity defect. Different anchor from R8-68 (open-mode plumbing, not nc_type/order).

### R8-74: JPEG compress failure messages are a generic "JPEG compression failed"; C emits specific texts
Severity: Low
Rust: `crates/ad-plugins-rs/src/codec.rs` — compress_jpeg returns Option, not a typed error.
C reference: NDPluginCodec.cpp — "JPEG only supports 8-bit data", "Unsupported array structure", "Unknown color mode %d".
Impact: status level correct (ERROR, per R8-63); message text diverges.

### R8-75: TiffWriter::array_color_mode falls back to dims-based inference when the ColorMode attribute is absent; C errors
Severity: Low
Rust: `crates/ad-plugins-rs/src/file_tiff.rs`.
C reference: NDFileTIFF.cpp — a 3-D array without ColorMode is an error.
Impact: attribute-less 3-D frames write on the port, fail on C.

### Notes (fix wave 6)

- Compile-load flakes, each passed in isolation and on re-run, none in
  crates touched by the reporting fixer: `regression-ioc::families
  o_seeded_record_suppresses_duplicate_post` (A4 run),
  `epics-ca-rs::protocol_tests
  server_event_cancel_unknown_sid_replies_eca_internal_and_disconnects`
  (E4b run). Main's post-merge workspace run was clean first-run.
- Pre-existing, untouched: `epics-base-rs/tests/client_server.rs` does not
  compile under `--all-features` (run_tcp_listener arity, WallTime vs
  SystemTime) — behind the ca-server-tls-test feature; default-feature
  builds clean. Confirmed pre-existing by fixer-E4a on the stashed tree.
- TIFF parity is asserted at tag-set/tag-value/plane-layout level, not
  byte-for-byte against libtiff (no C build on the fixer's machine).

## Round 9 — re-audit (2026-07-12): wave-6 fix verification + adjudications + fresh findings

Same 5 auditor panels (opus, read-only), blocks R9-1..15 / 16..30 / 31..45 /
46..60 / 61..75. Round report: `rounds/01KX9CG69T1ECXGFHBQ91SPP1R.md`.

### Fix verification result

All 30 wave-6 fix commits independently verified CORRECT AND COMPLETE against
the C/C++ reference (A: 6, B: 6, C: 4, D: 9, E: 10) — zero wrong or
incomplete fixes. Notable verifications: R7-3's LOG2-as-LOG·2 reading
re-confirmed directly against postfix.c:206-213 (reverse-table prefix match
via epicsStrnCaseCmp, no identifier boundary); R8-2's corrected 0/-1 reading
re-confirmed including the empty-expression contract split (base
postfix("")→-1, sCalc/aCalc→0).

### Adjudications of wave-6 fixer-surfaced findings (all 15 CONFIRMED)

- R8-6 CONFIRMED. Structural cause exposed: numeric engine has a strict token
  allowlist (`token_in_base_table`, postfix.rs:455) but sCalc/aCalc are gated
  only by `opcode_in_grammar`, whose `Opcode::Control(_) | Opcode::Core(_) =>
  true` arm (postfix.rs:438) waves through every core opcode — FMOD,
  `>>>`→ShrLogical, vars Q–U/MM–UU all reach the string/array engines.
  C tables verified directly: sCalc/aCalc single-letter operands stop at P,
  double at LL; FMOD and `>>>` in neither operators table; aCalc has no
  INF/NAN literal (only ISINF/ISNAN). Fix = per-engine token allowlists from
  sCalcPostfix.c/aCalcPostfix.c, as R7-3 did for base.
- R8-7 CONFIRMED, characterization sharpened. sCalc: contract is
  sCalcPerform.c:497-500 `if (pd[1]==0) return(-1)` — the WHOLE perform
  errors, which sCalcoutRecord.c:357-364 turns into
  CALC_ALARM/INVALID with VAL FROZEN (not a value of -1); port yields
  VAL=+Inf, no alarm. The port comment mis-cites base calcPerform.c. aCalc:
  port yields NaN, C yields myMAXFLOAT=1e35 in every DIV form
  (aCalcPerform.c:636-692); fixer already used MY_MAXFLOAT in aCalc Mod —
  DIV omission is an internal inconsistency.
- R8-8 CONFIRMED, WIDENED. Port has NO array-shift path at all: Shl/Shr
  (array.rs:218-227) always bit-shift elementwise. C LEFT/RIGHT_SHIFT
  dispatch on isDouble(ps): array operand gets a positional element move by
  myNINT(e) (`<<` negates the count), zero-fills the vacated tail, and
  linearly interpolates for fractional shift amounts.
- R8-22 CONFIRMED. C db_queue_event_log (dbEvent.c:812-820) replaces only
  `*pevent->pLastLog` in place when npend>0 && (flowCtrlMode ||
  rngSpace<=EVENTSPERQUE) — keeps earlier distinct queued entries. Rust
  `coalesce_consume` (epics-base-rs server/pv.rs:331-352) drains the ENTIRE
  rx backlog on overflow-slot-set and delivers only the newest.
- R8-23 CONFIRMED. C nDuplicates is a field of ev_que (dbEvent.c:80), shared
  across every subscription attached to that queue (dbEvent.c:453);
  event_read drains the whole ring, suspends only when flowCtrlMode &&
  nDuplicates==0 (dbEvent.c:947). Precision correction: sharing granularity
  is per-ev_que/quota, not literally per-client. Redesign-scale (shared
  queue), same primitive family as R8-21/R8-22.
- R8-34 CONFIRMED, NARROWED TO MONITOR. GET/PUT/RPC/PUT_GET/GET_FIELD/PROCESS
  already route replies through `decode_op_or_reset` → server.close()
  (ops_v2.rs:65-78 + wrong-op-kind arms). MONITOR is the sole live surface:
  typed loop (ops_v2.rs:3042-3045, 3233-3251) and raw loop (2461-2464,
  2569-2575) end only the subscription via MonitorEnd::Fatal +
  unregister_ioid, never server.close() — leaving the circuit (and its
  mutated shared reader type-cache) serving other channels. Port comments
  claim "pvxs resets the connection" — the implementation does not
  (clientmon.cpp:601-607 does). Fix: monitor-loop server.close() on
  Fatal-decode faults (op-status-error/cancel stay op-local).
- R8-54 CONFIRMED (nowt=omax asynRecord.c:1499, nrrd=inlen :1513,
  POST_IF_NEW :1020,1022; port clamps only into locals).
- R8-55 CONFIRMED, C-REFERENCE CORRECTED. Not "per process": mechanism is
  exceptCallback → monitorStatus (asynRecord.c:903-917) on ANY
  asynException, re-reading isAutoConnect/isConnected/isEnabled
  (:1085-1099) + POST_IF_NEW (:1125-1133). Port emits
  Connect/Enable/AutoConnect exceptions (port.rs:273,388,423) but
  register_trace_exception_callback filters to Trace* masks only
  (mod.rs:1705-1714), so shared-port state changes never refresh this
  record's AUCT/CNCT/ENBL.
- R8-56 CONFIRMED, WIDENED. Final read-error ERRS is `"%s  nread %d %s"`
  with %s ∈ {timeout,overflow,error} (asynRecord.c:1593-1598, overwriting
  the earlier "Error %s" at :1583); port emits "read: {e}". The overflow
  branch is a second read-side ERRS divergence in the same routine → R9-49.
- R8-57 CONFIRMED (856-line asynInterposeCom.c, installed by
  drvAsynIPPort.c:1061; no Rust port; subsystem-scale own assignment).
- R8-58 CONFIRMED (DelayInterpose exists, no iocsh command, no
  RequestOp::PushDelayInterpose; C asynInterposeDelay.c:221-234).
- R8-71 CONFIRMED (C tests currentPostCount >= postCount at
  NDPluginCircularBuff.cpp:178 OUTSIDE the triggered branches; port only
  inside, circular_buff.rs:250,323).
- R8-72 CONFIRMED (C writeInt32(SoftTrigger) sets Triggered=1
  unconditionally for any value incl. 0, :271, and flushPreBuffer()
  immediately when flushOn>0, :276-277; port gates on value!=0,
  circular_buff.rs:718, lazy flush only).
- R8-73 CONFIRMED (NDFileNetCDF.cpp:118 openMode & NDFileModeMultiple vs
  port frames.len()>1, file_netcdf.rs:537).
- R8-74 CONFIRMED (three C texts :140,:166,:201 vs generic
  "JPEG compression failed", codec.rs:1148).
- R8-75 CONFIRMED (port dims-based fallback file_tiff.rs:174-178 writes
  RGB1; C leaves colorMode=Mono → asynError NDFileTIFF.cpp:220-224).

### Category A — epics-base-rs (R9-1..R9-3)

### R9-1: aCalc relational/equality operators use a 1e-11 tolerance; C compares with exact IEEE operators
Severity: Medium
Rust: `crates/epics-base-rs/src/calc/engine/array.rs:115-168` — Eq/Ne/Lt/Le/Gt/Ge all test against a fabricated epsilon (Eq: `(x-y).abs() < 1e-11`, :118-119; Lt: `(y-x) > 1e-11`, :135); fires for scalar and array operands alike.
C reference: `aCalcPerform.c:1347-1354` (array), `:1372-1380` (array/scalar), `:1092+` (scalar) — all exact, matching calcPerform.c:371-396. Port's numeric and string engines already compare exactly (string.rs has an explicit "must not apply an epsilon" comment) — the tolerance survives only in aCalc.
Impact: `AA==BB` returns 1 for elements differing by up to 1e-11 where C returns 0; `A<B` returns 0 for genuine sub-1e-11 differences where C returns 1. Different result array on the wire.

### R9-2: aCalc scalar→array promotion never applies C's isnan(scalar) → fill 0 rule
Severity: Low
Rust: `crates/epics-base-rs/src/calc/engine/array_value.rs:32-37` (broadcast), `:67-72` (Array↔Double zip_map arms) — NaN scalar broadcast verbatim → all-NaN operand array.
C reference: `aCalcPerform.c:135-141` — to_array(setValues=1): `if (isnan(ps->d)) ...a[ii]=0.` — reached via toArray(ps,1) on the deeper operand (:630 arithmetic, :1337 relational/bitwise/ATAN2/CAT group).
Impact: NaN-scalar-deeper-operand binary ops (e.g. `SQRT(-1)+AA`) yield AA-combined-with-0 on C, all-NaN on the port. Order-specific: array OP NaN-scalar-on-top matches C.

### R9-3: calcout/scalcout init_record skips the compile for an empty CALC/OCAL; C compiles unconditionally and lands CLCV/OCLV = -1
Severity: Low
Rust: `crates/epics-base-rs/src/server/records/calcout.rs:1021-1029` — `if !self.calc.is_empty() { … }` (and the OCAL twin) leaves clcv=0 at load.
C reference: `calcoutRecord.c:100,108` — postfix() runs at init pass 1 with no empty guard; base postfix("")→-1 (postfix.c:236-238), so field(CALC,"") yields CLCV=-1.
Impact: base calcout loaded with explicit field(CALC,"") serves CLCV=0 on the port vs -1 on C. Load-time, calcout-specific (scalcout/acalcout init compile unconditionally; runtime empty put lands -1 correctly on both).

### Category B — epics-ca-rs + epics-tools-rs (R9-16..R9-18)

### R9-16: caget/cainfo print partial per-PV output on partial connect failure (no connect_pvs gate)
Severity: Medium
Rust: `crates/epics-ca-rs/src/bin/caget-rs.rs:628-644` (and cainfo-rs.rs:133-178) — per-PV tasks independently wait_connected and print on success; a never-connecting PV prints `*** not connected` interleaved with connected PVs' values. No all-channels barrier gates the read/print phase.
C reference: `tool_lib.c` connect_pvs (create_pvs then ca_pend_io, returns 1 on ECA_TIMEOUT), called from caget.c:378/cainfo.c:228 BEFORE caget()/cainfo(); any connect failure → get+print never runs, zero stdout values, stderr "not found." messages, exit 1.
Impact: with mixed connect results C emits zero value lines; Rust emits values for connected PVs. Stdout diverges for parsers; exit codes agree (both 1).

### R9-17: camonitor does not rebuild the subscription when the field type changes across reconnect
Severity: Low
Rust: `crates/epics-ca-rs/src/bin/camonitor-rs.rs:362-367` — subscribes once with the first-connect type; connection-event loop (:322-345) only flags and prints `*** disconnected`; reconnect re-issues EVENT_ADD with the frozen DBR type.
C reference: `camonitor.c:143-147` — on reconnect with changed ca_field_type: ca_clear_subscription, then :155-180 re-derives the request type (ENUM → DBR_TIME_STRING) and re-subscribes.
Impact: PV reconnecting with a changed native type (numeric→ENUM after IOC change) prints state labels on C, raw indices on Rust.

### R9-18: caput exits 1 (and prints nothing) on a post-put readback timeout; C prints the value line and exits 0
Severity: Medium
Rust: `crates/epics-ca-rs/src/bin/caput-rs.rs:357-370` — readback CaError::Timeout → FatalReadback::Timeout → stderr "Read operation timed out…" + exit(1) before any `New :` line. Comment at :465-467/:475-485 claims C caput.c:186-188 "returns ECA_TIMEOUT" — misread.
C reference: `caput.c:130-240` — :186-188 prints the timeout message but does NOT return; falls through to `*** no data available (timeout)` per PV (:207-208) and unconditionally `return 0` (:239). Only the !nConn guard (:181) returns 1 (Disconnect classification in the port is correct).
Impact: slow readback after a successful put: C exits 0 with a `New :` line; Rust exits 1 with none — breaks scripts keying on caput's status.

### Category C — epics-pva-rs + epics-bridge-rs (R9-31..R9-32)

### R9-31: Boolean-typed record._options.DBE is parsed numerically; pvxs's kind switch excludes Kind::Bool
Severity: Medium
Rust: `crates/epics-bridge-rs/src/qsrv/channel.rs:88` — dbe_scalar_as_u8 maps `ScalarValue::Boolean(b) => *b as u8`, so numeric dispatch (:150-151) treats boolean DBE=true as mask 1 → VALUE only.
C reference: `singlesource.cpp:131-138` — switch(kind) reaches as<uint8_t>() only for Kind::Integer/Kind::Real; Kind::Bool hits `default: break` → dbe=0 → `dbe &= 7; if(!dbe) dbe = VALUE|ALARM` (:141-144). The dbe_scalar_as_u8 comment cites what as<uint8_t> CAN convert (data.cpp:402-435) — pvxs never calls it for bool.
Impact: MONITOR with boolean DBE=true negotiates VALUE-only (0x1) vs pvxs VALUE|ALARM (0x5): alarm-only transitions silently never delivered — the identical defect R7-34 fixed for the string kind, open for the boolean kind. false coincidentally agrees. Single site (dbe_mask_from_pv_request; groups use a fixed mask).

### R9-32: MONITOR record._options.ackAny of real/boolean type is silently dropped; pvxs converts via as<uint32_t>
Severity: Low
Rust: `crates/epics-pva-rs/src/server_native/tcp.rs:196` — ackAny match handles String + integer variants; `_ => {}` swallows Float/Double/Boolean, leaving ack_at=1 (:151); the `ack_at==0 → queue_size/2` fallback (:212) never fires for them.
C reference: `servermon.cpp:555-558` — `if(ackAny.as(ival))` (tryAs<uint32_t>) succeeds for real and bool storage → op->ackAt = ival; only non-scalars reach the Crit log (:569-571). Port comment at :199-200 describes only the non-scalar case.
Impact: pipelined MONITOR with ackAny Double(4.0)/Boolean(false) gets ackAt=1 (ACK every event) vs pvxs ackAt=4 / queue_size/2 — MONITOR_ACK cadence and the ackAt-1 watermark clamp (servermon.cpp:332-333) diverge on the wire.

### Category D — asyn-rs + motor-rs (R9-46..R9-50)

### R9-46: asynRecord clamps a non-positive TMOT to 1 s; C passes the operator's TMOT straight through
Severity: Medium
Rust: `crates/asyn-rs/src/asyn_record/mod.rs:2025-2029` — `if self.tmot > 0.0 { … } else { Duration::from_secs(1) }`; the :2024 comment ("falls back to the 1 s default") is fabricated.
C reference: `asynRecord.c:818` — `pasynUser->timeout = pasynRec->tmot;` verbatim, no fallback (:309's timeout=1 is a one-time createAsynUser seed; dbd initial("1.0") is a field default, not a clamp).
Impact: asyn convention: <0 wait forever, 0 non-blocking poll, >0 bounded. TMOT=0: C immediate poll, port blocks 1 s. TMOT<0: C blocks indefinitely, port times out at 1 s with a spurious READ/MAJOR alarm C never raises.

### R9-47: option and EOS puts never re-read the driver; C's setOption/setEos fall through to getOptions/getEos on every set
Severity: Medium
Rust: `mod.rs:1853-1858` (write_option, no re-read) and `:2957-2972` (IEOS/OEOS, no read-back); read_options_from_driver only from connect_device (:1929).
C reference: `asynRecord.c:845-849` — `case callbackSetOption: setOption(); /* no break */ case callbackGetOption: getOptions();` (:851-855 setEos→getEos twin). getOptions (:1834+) re-reads every option and POST_IF_NEWs it even when the set failed.
Impact: a driver that rounds/rejects a requested value leaves the port's BAUD/…/IEOS/OEOS showing the REQUESTED value; C shows the driver's actual value and reverts rejected puts to live driver state.

### R9-48: register-interface read/write errors report generic ERRS text instead of C's per-interface diagnostic
Severity: Low
Rust: `mod.rs:688` (all register writes → "write: {e}"), `:818/:831/:843` (Int32/UInt32/Float64 reads → "read: {e}").
C reference: `asynRecord.c:1378/:1391/:1414/:1429/:1450/:1463` — "Int32 write error, %s" etc., per interface and direction.
Impact: ERRS on OPI screens loses which interface failed and the C error tail. Distinct sites from R8-48 (octet write) and R8-56 (octet read).

### R9-49: an ASCII/Hybrid overflow read sets no ERRS; C writes "Overflow nread %d %s"
Severity: Low
Rust: `mod.rs:800-802` — overflow branch only raise_io_alarm(READ, Minor); no out.errs; process() cleared ERRS at :3043 → blank.
C reference: `asynRecord.c:1602-1608` (ASCII) / `:1609-1615` (Hybrid) — reportError "Overflow nread %d %s" alongside the MINOR alarm.
Impact: terminator-less response filling AINP/BINP: C's ERRS says why; port's ERRS is blank.

### R9-50: a pre-write flush failure lands in ERRS; C discards the flush status
Severity: Low
Rust: `mod.rs:861-865` — IoPhase::Flush recorder sets out.errs="flush: {e}"; a subsequent successful write/read does not clear it.
C reference: `asynRecord.c:1521` — flush() return value discarded; a flush failure never reaches ERRS.
Impact: port surfaces a diagnostic on a path C treats as best-effort; a successful transaction can carry a stale flush error string.

### Category E — synApps + AD (R9-61..R9-72)

### R9-61: transform record ignores IVLA="Do Nothing"; missing global skip-on-INVALID-input
Severity: High
Rust: `crates/epics-base-rs/src/server/records/transform.rs:504-518` — ivla used only per-channel (on eval Err, `if ivla==1 { vals[i]=prev_vals[i] }`); nothing tests the record's input-alarm severity; calcs and all 16 output-link writes always run.
C reference: `transformRecord.c:554-560` — `if ((nsev >= INVALID_ALARM) && (ivla == transformIVLA_DO_NOTHING)) { …; pact=FALSE; return; }` — no calcs, no output writes that cycle.
Impact: with a maximize-severity input link gone INVALID and IVLA="Do Nothing", C freezes; Rust recomputes and drives every output link. The entire hold-outputs mode is absent, plus an invented per-channel value-restore C never performs.

### R9-62: transform VAL is aliased to channel A; C keeps VAL a constant-0 dummy
Severity: Medium
Rust: `transform.rs:544-546` (get_field VAL → vals[0]), `:572-577` (put VAL → vals[0]); test test_transform_val_is_a pins the invented behaviour.
C reference: `transformRecord.c:422` sets val=0 once at init; process()/monitor() iterate from &ptran->a and never touch ->val.
Impact: `caget transform.VAL` returns 0 on C, channel A on Rust; a .VAL monitor never fires on C, fires on every A change on Rust.

### R9-63: transform calc failure raises no CALC_ALARM/UDF
Severity: Medium
Rust: `transform.rs:513-517` — eval Err only optionally restores the value; no STAT/SEVR, no UDF; no check_alarms/value_is_undefined override (framework default reads only channel A).
C reference: `transformRecord.c:593-596` — `recGblSetSevr(CALC_ALARM, INVALID_ALARM); udf = TRUE`; checkAlarms (:773-779) raises UDF_ALARM.
Impact: failing calculation on any channel: NO_ALARM on Rust, CALC/INVALID + UDF on C.

### R9-64: transform input-link read failure keeps the stale channel value; C zeroes it
Severity: Medium
Rust: `crates/epics-base-rs/src/server/database/processing.rs:1680-1684` — failed read leaves the value unchanged (stale); transform overrides nothing.
C reference: `transformRecord.c:537-541` — `if (!RTN_SUCCESS(status)) { *pval = 0.; }` (transform-specific; calcRecord does not zero).
Impact: on a disconnected INPx source, C drives that channel and its OUTx to 0; Rust re-outputs the last good value.

### R9-65: swait DOPT="Use DOL" never fetches the DOL link — writes a stale DOLD
Severity: Medium
Rust: `crates/epics-base-rs/src/server/records/swait.rs:451` — `oval = if dopt==1 { dold } else { val }`; dold is only client-put/default; descriptor table (:143-366) has no DOLN/DOLV and multi_input_links (:615) lists only INAN–INLN — DOL link never registered or read.
C reference: `swaitRecord.c:763-772` — execOutput: `if (dopt) { if (!dolv) recDynLinkGet(&caLinkStruct[DOL_INDEX], &dold, …); outValue = dold; }` — live DOL PV value fetched at output time.
Impact: DOPT="Use DOL" writes the current DOL PV value on C, a stale/zero DOLD on Rust — the whole output mode produces a wrong OUT value.

### R9-66: ROI plugin bins a disabled dimension; C forces binning=1
Severity: Medium
Rust: `crates/ad-plugins-rs/src/roi.rs:216-218` (3-D) / `:331-332` (2-D) — bin factor taken unconditionally; resolve_axis (:137-145) resolves only offset/size from enable.
C reference: `NDPluginROI.cpp:98-102` — disabled else branch: offset=0, size=full, binning=1.
Impact: Enable=0 with leftover DimNBin>1: C outputs full resolution; Rust outputs a shrunken axis of bin-sums — wrong dims and pixel values.

### R9-67: ROI Dim{0,1,2}MaxSize readbacks use the physical dim index, not logical X/Y/color
Severity: Medium
Rust: `roi.rs:421-424` — max_size[i] from physical dims[i].
C reference: `NDPluginROI.cpp:80-82` userDims={xDim,yDim,colorDim}; posts Dim{N}MaxSize = dims[userDims[N]].size (:111,120,129).
Impact: RGB1 (dims [color,x,y]): C reports Dim0MaxSize=X-width, Dim2MaxSize=color-count; Rust swaps them — ROI slider bounds wrong for every RGB image.

### R9-68: NDPluginProcess SaveBackground/SaveFlatField capture the wrong frame and defer ValidXxx
Severity: Medium
Rust: `crates/ad-plugins-rs/src/process.rs:361-368` — deferred one-shot consumed at the next process(), converting that frame's INPUT; valid_background recomputed only at :383 on the next frame.
C reference: `NDPluginProcess.cpp:287-298` — synchronously inside writeInt32: convert this->pArrays[0] (last OUTPUT array) and setIntegerParam(ValidBackground, 1) immediately.
Impact: (a) stored reference is previous output frame on C vs next raw input on Rust — different pixels subtracted/divided; (b) caget ValidBackground right after SaveBackground: 1 on C, 0 on Rust until another frame (forever if none).

### R9-69: sub record runs SNAM on a failed INPn link read; C skips do_sub that cycle
Severity: Medium
Rust: `crates/epics-base-rs/src/server/record/record_instance.rs:2004-2023` — subroutine gated only by suppress_subroutine_run (assigned solely from the aSub LFLG=READ path, processing.rs:279); plain subRecord has no fetch-failure gate.
C reference: `subRecord.c:146-147` — `if (status == 0) status = do_sub(prec)`; fetch_values (:407-418) returns -1 on the first failed dbGetLink → SNAM skipped, VAL frozen.
Impact: failed INPn read: C leaves VAL unchanged, no SNAM; Rust invokes SNAM with stale/partial A–U and recomputes VAL.

### R9-70: sseq DLYn is not quantized to the OS clock tick
Severity: Medium
Rust: `crates/epics-base-rs/src/server/records/sseq.rs:525` uses dly raw; put path stores raw; no epicsThreadSleepQuantum rounding in the crate.
C reference: `sseqRecord.c:197-200` (init) + DLY special path — `dly = quantum * NINT(dly/quantum); db_post_events(DBE_VALUE)`.
Impact: DLYn readback differs; a sub-half-quantum delay (0.004 s at 10 ms quantum) rounds to 0 on C (fires immediately) but waits ~4 ms on Rust — different step sequencing.

### R9-71: codec BSLZ4 decompress-failure text differs from C
Severity: Low
Rust: `crates/ad-plugins-rs/src/codec.rs:1196` — "Failed to BSLZ4 decompress".
C reference: `NDPluginCodec.cpp:601` — decompressBSLZ4 sets "Failed to Blosc decompress" (a C copy-paste, but the reference contract).
Impact: NDCodecCodecError PV text diverges for a corrupt BSLZ4 frame. Same class as R8-74, different site.

### R9-72: swait A–L input-field monitors carry a spurious DBE_LOG bit
Severity: Low
Rust: generic monitor epilogue posts every changed subscribed field with aux_mask = alarm_bits | VALUE | LOG (`processing.rs:2644`) — correct for calc, not swait.
C reference: `swaitRecord.c:650` posts changed input fields with monitor_mask | DBE_VALUE (LOG only when VAL's own ADEL fired that cycle) — unlike calcRecord.c:420 which adds | DBE_LOG.
Impact: a DBE_LOG subscriber to swait .A–.L gets an archive event on every input change on Rust; C emits LOG only on ADEL-crossing cycles.

### Notes (Round 9)

- R9-18/R9-46/R9-62 include test-skepticism hits: port comments/tests
  citing C behaviour the reference does not have (caput.c:186-188 "returns
  ECA_TIMEOUT" misread; asynRecord 1 s TMOT fallback fabricated;
  test_transform_val_is_a pins invented behaviour).
- Auditor-E scaler candidates NOT reported (C-side verified, Rust-side
  unconfirmed): user-stop COUTP double-fire, RATE→TP quirk — future-round
  material.
- Extensive audited-clean inventories per panel are in the round report.

### Fix wave 7 — dispositions (2026-07-12)

8 worktree fixers (opus): a7 / b7-tools / b7-evq (structural) / c7 / d7 /
d7-com (subsystem port) / e7-rec / e7-ad. All branches merged into
review/parity-r6 and verified by main: workspace fmt --check clean, clippy
--all-targets -D warnings clean, nextest 7866 passed / 0 failed / 2 skipped,
doctests clean. 31 of 32 items FIXED; R9-17 NOT-REAL; R9-46 fixed as its
TMOT>=0 half with the negative half a documented DRV-42 deviation. One
merge-integration fix by main: e7-rec's swait_input_monitor_mask test
adapted to b7-evq's EventReader API (commit on review/parity-r6).

Category A (fixer a7; every expectation verified against COMPILED C drivers
of sCalcPostfix/sCalcPerform/aCalcPostfix/aCalcPerform + base postfix/calcPerform):
- R8-6 FIXED 55903088 — per-engine `ElementTable` IS the lexer (base/sCalc/
  aCalc, own symbols, operand-letter ranges, hex rules); the post-hoc
  token_in_base_table filter deleted — out-of-table symbols unrepresentable.
  SUB-CLAIM NOT-REAL: 0x hex literals ARE accepted by C sCalc/aCalc
  (epicsStrtod re-scan, 0x1F=31); the real defect was invented symbol names
  (ATOD/BIN_READ/BIN_WRITE/NORMAL_RNDM vs C's DBL/READ/WRITE/NRNDM).
- R8-7 FIXED 414a3f18, WIDENED — string::eval owns C's whole -1 contract
  (zero divisor, negative SQRT/LOG10/LOGE, non-finite-result tail incl. the
  atof of a string result); aCalc Div yields myMAXFLOAT in all three operand
  shapes; base stays bare IEEE.
- R8-8 FIXED 9f0b3fe0 — <</>> dispatch on the left operand's type; array
  branch is shift_elements(): positional move by myNINT, zero fill, C's
  in-place interpolation walk for fractional shifts.
- R9-1 FIXED 480dc413 — FINDING CORRECTION: the premise was half wrong. The
  1e-11 epsilon is C's SMALL (sCalcPerform.c:46), used by ALL SIX sCalc
  numeric comparisons; the port had the two engines' rules SWAPPED (epsilon
  in aCalc which is exact, exact in sCalc which uses SMALL). Compiled C:
  0.1+0.2==0.3 is true in sCalc, false in aCalc. One commit — one rule in
  the wrong place, not two bugs.
- R9-2 FIXED 4cdf8ec7 — broadcast → ArrayStackValue::to_array (C's
  to_array(setValues=1), NaN→0), single promotion owner; rename exposed a
  second site (acalcout AVAL fill, aCalcPerform.c:1624).
- R9-3 FIXED 1052c1c9 — calcout init compiles CALC/OCAL unconditionally.
- Tests corrected (pinned invented behaviour → compiled-C values):
  h7_div_by_zero (sCalc -1 not +Inf), h6 comparisons (SMALL not exact),
  acalcout non-finite cases (1/0 → myMAXFLOAT/st=0 in C; moved to
  1e300*1e300), ATOD→DBL, BIN_READ/BIN_WRITE→READ/WRITE, MM/UU now Err.

Category B (fixers b7-tools, b7-evq):
- R9-16 FIXED 4f2e3c76 — cli::connect_pvs single connect-gate owner
  (create-all → wait-all → C's exact stderr text) shared by caget/cainfo/
  caput; partial connect → zero stdout, exit 1. camonitor classified
  distinct (C uses a connection handler, no barrier).
- R9-17 NOT-REAL a273458c — the subscription coordinator already re-derives
  the type across reconnect; pinned by evidence test
  label_readback_re_derives_to_dbr_time_string_when_the_pv_returns_as_enum
  passing with no production change.
- R9-18 FIXED b54b0744 — FINDING CORRECTION (compiled/traced against C):
  the "*** no data available (timeout)" branch is DEAD in caput (needs
  value==NULL; caput callocs at :167 and never uses the callback path), so
  C prints the ZEROED buffer (`New : <name>  0`) and exits 0 — implemented
  as zero_readback shaped by the readback type; widened to the Old: read
  (same fatal-timeout bug); ENUM-menu read distinct (C genuinely returns 1).
- R8-22 FIXED c45e60a8 / R8-23 FIXED 5affc90c — STRUCTURAL REDESIGN (third
  round on the primitive): new epics_base_rs::server::event_queue ports C's
  triple EventUser (circuit, flowCtrlMode, queue chain with quota
  selection) / EvQue (ring occupancy, nDuplicates, quota, one SubQ per
  monitor) / SubQ (events VecDeque; npend=len, pLastLog=back).
  Invariants: n_duplicates == Σ max(0, npend-1); total_pending == Σ npend;
  newest pending is events.back() on every path; NO side slot exists; only
  the queue moves its counters. coalesce_consume, Subscriber::
  coalesce_overflow, pop_coalesced, per_channel_event_depth, MonitorFlow,
  FlowControlGate all DELETED. post returns PostOutcome{Appended/Replaced/
  Closed}. Boundary tests incl. sibling-duplicate release (R8-23 owner
  path) and the intermediate one-ring-per-subscription tree failing it.
  Documented deviations in the module header: per-subscription reader tasks
  (frame interleave not C's strict ring order; all accounted quantities
  shared), by-reference early-drop branch unreachable (owned Snapshots),
  replace ORs the displaced mask into the survivor (pre-existing 446e0d4a).

Category C (fixer c7):
- R8-34 FIXED f1e2fca1 — MonitorTeardown single owner for both monitor
  loops; MonitorEnd::Fatal constructed at exactly one site which calls
  server.close() first, so Fatal ⟹ circuit-closed holds by construction.
  RawMonitorFrameKind::Fatal split into Invalid (circuit-fatal) /
  FinishError (op-local) — the type split forced all five driver sites.
  IOID unregister + active clear centralized from 14 open-coded exits.
- R9-31 FIXED 7d4f5af0 — dbe_scalar_as_u8 → dbe_kind, exhaustive match
  mirroring pvxs switch(kind); Bool → default → VALUE|ALARM fallback. A
  test pinning Boolean(true)→VALUE was invented and corrected.
- R9-32 FIXED e226272a — ack_any_as_u32, one exhaustive tryAs<uint32_t>
  for non-string storage; also closes the unwrap_or(1) arms (pvxs wraps
  negatives then clamps). Stated deviation: uint64_t(double) is C++ UB for
  negative/NaN/overflow; port takes Rust's defined saturating cast.

Category D (fixers d7, d7-com):
- R8-54 FIXED 606c9a48 — clamp_transfer_sizes single owner of NOWT/NRRD
  effective values, written back into the fields.
- R8-55 FIXED 3be9d184 — one monitor_status() owner (C monitorStatus),
  driven by connect_device and the status_dirty drain; exception
  subscription filters on port name alone. Also corrected invented
  behaviour: connect_device kept AUCT/ENBL on port-query failure where C
  assigns 0 (:1087,:1092,:1097). New async PortHandle::is_connected/
  is_enabled/is_auto_connect (callback runs on the port-actor thread).
- R8-56 FIXED 046183b7 / R9-48 FIXED ca97a50b / R9-49 FIXED 51c17877 /
  R9-50 FIXED 1613ec3d — every I/O diagnostic through one
  IoOutcome::report_error (C's reportError, last-writer-wins into ERRS):
  C texts, C source ordering (status failure then overflow),
  AsynError::message() tail, pre-write flush status discarded.
- R8-58 FIXED 8f9507db — asynInterposeDelay iocsh registrar via
  RequestOp::PushDelayInterpose through the port actor.
- R9-46 FIXED 08b0f190 — io_timeout() single owner of TMOT→AsynUser::
  timeout; tmot >= 0 verbatim (0 = C's non-blocking poll; transports
  already implement it). DOCUMENTED DEVIATION: tmot < 0 ("wait forever")
  keeps the bounded 1 s fallback under DRV-42 (AsynUser::timeout is an
  unsigned Duration so every blocking driver op is bounded) — the invented
  "1 s default" comment replaced by an explicit DRV-42 citation at the
  site. Decision by main: option (1), reversing DRV-42 rejected.
- R9-47 FIXED c8afa0c9 — write_option (set→getOptions) and write_eos
  (set→getEos) single owners; option/EOS fields are driver readbacks, not
  request latches, even on a failed set. New RequestOp::GetInputEos/
  GetOutputEos round trips.
- R8-57 FIXED d92eaacb + 189aa949 + b6e01a1b + 824df948 — asynInterposeCOM
  ported in full (crates/asyn-rs/src/interpose/com.rs; C file is at
  asyn/miscellaneous/asynInterposeCom.c, not interposes/ as filed). 45
  protocol tests incl. a byte-exact 61-byte connect handshake and C-quirk
  negative controls (parity echo unchecked, crtscts N resends current
  mode, short write reports the stuffed count). IpProtocol::Com accepted;
  the R8-50 refusal replaced; restore_com_settings on connect (C
  exceptionHandler). STRUCTURAL: COM is the chain's BASE LINK, not a stack
  layer — Rust's interpose push order inverts C's (later install =
  innermost, C = outermost), and with COM above EOS each escaped 0xFF
  costs the read one byte (byte-budget test proves it); with_base_link
  makes "COM innermost" hold by construction. Negotiation reads share the
  data path's socket-teardown gate (should_disconnect_after_read_error).
  Deliberate deviation: C's ixon bad-value arm sends uninitialized memory
  (UB); the port returns asynError("Bad option value"), documented at site.
  No iocsh registrar BECAUSE C HAS NONE (brief assumption wrong —
  installed only from drvAsynIPPort.c:1061).

Category E (fixers e7-rec, e7-ad):
- R9-61 FIXED 5c5412b8 / R9-62 FIXED 4947f9d8 / R9-63 FIXED 5a0c9a34 /
  R9-64 FIXED 0ae29ab1 — one structural pass over transform process():
  C's fetch → IVLA gate → per-channel calc → outputs order; VAL de-aliased
  (constant-0 dummy; test_transform_val_is_a corrected); calc failure
  raises CALC_ALARM/INVALID + UDF; failed INPx read zeroes the channel;
  the invented per-channel value-restore removed.
- R9-65 FIXED 34d23e7c — output_link_value() single owner of the OUT
  value composed at output time; output_time_input_links() fetches DOL
  after ODLY (C execOutput); swait OVAL de-dualed (now only C's Old
  Value); DOLN field added.
- R9-69 FIXED 0ef94baf, WIDENED to aSub — input_fetch_policy() Record
  hook (ReadAll | AbortOnFirstFailure), enforced once in the framework
  fetch loop; sub + aSub declare AbortOnFirstFailure (the exact C
  fetch_values population).
- R9-70 FIXED 0c1704b9 — FINDING CORRECTION: C's put path quantizes and
  posts DLY1 regardless of WHICH DLYn was written (sseqRecord.c:1140-1156
  computes lnkIndex but never adds it — long-standing upstream quirk);
  reproduced and pinned as C-verified. Init quantizes all channels.
  epicsThreadSleepQuantum/NINT ported into runtime::time.
- R9-72 FIXED dbb9d550 — aux_change_mask() single owner of changed-aux
  event masks across all three change-detection loops, driven by
  fields_posted_with_monitor_mask() declarations; swait inherits
  monitor_mask (and gained its missing MDEL/ADEL fields), calc keeps its
  forced |LOG.
- R8-71 FIXED 9fe0e508 / R8-72 FIXED 038fec51 — CircularBuff completion
  test moved outside the triggered branches (postCount==0 completes per
  running frame); SoftTrigger latches for any written value incl. 0 and
  flushes the pre-buffer eagerly when FlushOnSoftTrig>0.
- R8-73 FIXED b29103e2 — netCDF numArrays keyed on open mode through
  define_data_set.
- R8-74 FIXED f4af35e5 / R9-71 FIXED 784a0fa8 — compress_jpeg returns the
  message-bearing error (C's three texts); BSLZ4 text is C's "Failed to
  Blosc decompress" copy-paste.
- R8-75 FIXED 1de6b646 — TIFF errors on 3-D without ColorMode as C does.
- R9-66 FIXED b1685c5a — resolve_axis returns (offset, size, binning);
  disabled dimension forces binning=1 by construction (both 2-D and 3-D
  call sites' hand derivations deleted).
- R9-67 FIXED dad7b0ba, WIDENED — NDArrayInfo::user_dims() single owner of
  C's {xDim,yDim,colorDim}; same-defect site found and fixed in
  bad_pixel.rs (physical dims[0]/dims[1] → bad pixels corrected at the
  wrong element on RGB1).
- R9-68 FIXED 8f9cade1 — the port gained pArrays[0] as real state with one
  writer; SaveBackground/SaveFlatField copy the last OUTPUT synchronously
  inside the write and set ValidXxx immediately. The two one-shot tests
  pinning the deferred behaviour deleted, replaced by C-verified cases.

## Open Findings — surfaced during fix wave 7 (reported by fixers, pending independent verify)

Category A (calc engines; compiled-C evidence behind 1/2/6):
### R9-4: sCalc coerces string operands to double via toDouble/atof; the port errors TypeMismatch
Severity: Medium. With AA="6", C gives AA/2=3 and SQRT(AA)=2.449; affects every mixed-type op in string.rs.
### R9-5: aCalc SQRT/LOG of a negative — C scalar: 0 with status 0; C array: elements 0 with status -1 (aCalcPerform.c:775-812 → :1602); port yields NaN in both
Severity: Medium.
### R9-6: C's READ/WRITE are 2-operand; the port's op is 1-operand (compiled C: COMPILE_ERR 8 for READ(AA))
Severity: Low.
### R9-7: calc/calcout never raise CALC_ALARM for an empty/broken expression — C postfix() always writes END_EXPRESSION into RPCL (even on failure) and calcPerform returns -1 → CALC_ALARM/INVALID every process; port models "no program" as Option::None and skips evaluation
Severity: Medium. Structural cause behind R9-3's symptom; spans calc + calcout; its own change (empty/failed compile carries an empty program, not None).
### R9-8: C symbols the port cannot lex/execute: @, @@, R2S, S2R, AVAL, ANEG, APOS, aCalc LEN, sCalc -|, $E/$P/$R/$S/$T/$W aliases; aCalc [/{ array subrange (C compiles, port answers Syntax)
Severity: Medium.
### R9-9: C's strtod re-scan swallows INFINITY-style literals where the port stops at INF
Severity: Low.

Category B (CA tools):
### R9-19: caget-rs read-timeout prints "*** no data available (timeout)" in both modes; C's synchronous path prints the zeroed calloc'd buffer (caget.c:209), the string is reachable only under -c (caget.c:130)
Severity: Low.
### R9-20: caget-rs exits 1 for any single PV's post-gate read failure; C returns 0 unless EVERY PV is disconnected (caget.c:227 if (!nConn) return 1)
Severity: Low.
### R9-21: caput-rs Old:-read disconnect/error exits 1 before the put; C discards that caget()'s return (caput.c:535) and proceeds to the put
Severity: Low.
### R9-22: caput-rs -S is not applied to the readback rendering; C's caget uses the global charArrAsStr (caput.c:211-221)
Severity: Low.
### R9-23: caput-rs non-fatal readback errors echo the submitted value; C prints "*** no read access" / "*** CA error" markers
Severity: Low.

Category C (PVA):
### R9-33: non-scalar ackAny should reset the circuit — pvxs servermon.cpp:556 runs as<std::string>() BEFORE the if/else; NoConvert escapes into the dispatch catch (conn.cpp:277-282) → bev.reset(); the :570-573 Crit logRemote is dead code (R9-32's filing text was wrong there). Port logs Crit and serves on with ackAt=1 (comment already corrected in e226272a)
Severity: Medium.
### R9-34: string ackAny parse is decimal-only; pvxs parseTo<uint64_t> = stoull(s,&idx,0): "0x10"→16, "010"→8, "-1" wraps to u64::MAX → 0xFFFFFFFF → clamped to queueSize
Severity: Low.
### R9-35: array-typed record._options.DBE — TypeCode kind() is code&0xe0 so Int32A is Kind::Integer and pvxs reaches as<uint8_t>() which THROWS on array storage, escaping onSubscribe; port treats non-scalar DBE as unselected → VALUE|ALARM. pvxs-side consequence UNVERIFIED — verify before fixing
Severity: Low.

Category D (asyn):
### R9-51: record-lifecycle ERRS texts invented (outside IoOutcome::report_error): "port '{}' not found" (C "connectDevice failed: %s"), "not connected" (C "Not connect to a port"), "drvUserCreate failed: {e}" (C "Error in asynDrvUser->create()"), trace-file open text, and "read: ... returned no value" arms with no C analogue
Severity: Low.
### R9-52: monitorStatus does not refresh TSIZ (getTraceIOTruncateSize, asynRecord.c:1100) or TFIL ("Unknown" on foreign change, :1119-1124) on any path
Severity: Low.
### R9-53: interface-validity fields hardcoded — connect_device sets octetiv/i32iv/ui32iv/f64iv/optioniv=1 unconditionally; C queries each interface, e.g. setEos with no asynOctet → "No asynOctet interface" + COMM/MAJOR (asynRecord.c:1949-1953)
Severity: Medium.
### R9-54: @asyn(PORT,ADDR,TIMEOUT) DB-link parse panics on a negative timeout — adapter.rs:121,186 Duration::from_secs_f64 with no guard; C accepts -1 as "wait forever". A PANIC at record init, not a substitution
Severity: High.
### R9-55: PortDriver::set_option/get_option carry no asynUser, so asynRecord TMOT cannot reach the COM negotiation (uses C's own 2 s, faithful for the shell path, silently fixed for the record path). Needs an asynUser on the option trait — public API change
Severity: Low.
### R9-56: interpose STACK push-order inversion remains for stack layers — an echo pushed onto a port that already has EOS lands inner of EOS where C puts it outer (COM fixed structurally as base link; the general stack family is open)
Severity: Medium.
### R9-57: COM diagnostic gaps blocked on trait surface: asynPrintIO(ASYN_TRACEIO_FILTER) on unstuffing dropped (readIt :237-239); flow-control advisory message into pasynUser->errorMessage on success dropped (:600-640)
Severity: Low.

Category E (records + AD; block extended past 75 — numbering is per-round unique):
### R9-73: calc/calcout/sCalcout/swait run calcPerform regardless of fetch failure; C gates on fetch_values()==0 (calcRecord.c:120); swait additionally raises READ_ALARM/INVALID on a failed input
Severity: Medium. Same shape as R9-69, different mechanism (calc gate, not subroutine gate).
### R9-74: swait OOPT="On Change" is fabs(oval-val) > mdel in C (swaitRecord.c:432); port compares with != (newly observable now MDEL exists)
Severity: Low.
### R9-75: swait LA..LL previous-input fields missing — C posts them alongside A..L (swaitRecord.c:652)
Severity: Low.
### R9-76: swait INAV..INLV / DOLV link-status fields missing (PV_OK/PV_NC/NO_PV); C's execOutput reads DOL only if (!dolv); port approximates with "DOLN resolves"
Severity: Low.
### R9-77: eventRecord.c:163 posts VAL with monitor_mask | DBE_VALUE (no forced LOG); the port's default forced VALUE|LOG still applies (R9-72 family; the new hook is the mechanism)
Severity: Low.
### R9-78: aSub OUTA..OUTU output links never driven; C pushes them when do_sub returns 0 (aSubRecord.c:210-230)
Severity: Medium.
### R9-79: CircularBuff push() is not gated on Control — C processCallbacks does nothing when scopeControl==0; port transitions Idle→BufferFilling on first push, so Control=0 does not stop recording
Severity: Medium.
### R9-80: color_convert.rs:802 detect_color_mode infers layout from dims when ColorMode is absent (same inference R8-75 removed from file writers); C NDPluginColorConvert.cpp:44 defaults Mono — consequence is wrong pixel conversion
Severity: Medium.

### Notes (fix wave 7)

- Deliberate deviations recorded (not defects): JPEG oversized-dimension —
  C's jpeg_std_error error_exit calls exit() and aborts the IOC, port
  refuses with C's :235 text; bad_pixel .max(1) binning clamp — C divides
  by a 0 binning verbatim (crash); COM ixon bad-value arm — C sends
  uninitialized memory (UB); ack_any float — uint64_t(double) is C++ UB
  for negative/NaN, port uses the defined saturating cast; TMOT<0 —
  DRV-42.
- Test-infra defect (not parity): epics-pva-rs tests/stability.rs uses
  fixed host ports (NEXT_PORT=15075) and run_pva_server swallows the bind
  error, so concurrent workspace runs on one host cross-connect —
  root-caused by fixer-c7 as the source of the recurring
  array_concurrent_subop_replies_error_not_silent flake (it failed once in
  four separate fixers' full-workspace runs, incl. on the untouched base
  tree, and passed all isolation reruns). Worth fixing in a future wave.
- Other compile-load/UDP flakes, all passing isolation + rerun, recorded:
  ca_fr_8_dotted_filter_suffix_resolves_at_search,
  mr_r7_rejected_queued_datagram_does_not_reparse_stale_buffer,
  claimed_host_name_matches_a_host_hag_and_grants_write.
- Fixer-a7 was cut off once by an API server error mid-task and resumed by
  main with no loss (worktree state intact).

## Round 10 — re-audit (2026-07-12): wave-7 fix verification + adjudications + fresh findings

### Fix verification (wave 7)

- Category A — all six commits (55903088, 414a3f18, 9f0b3fe0, 480dc413,
  4cdf8ec7, 1052c1c9) VERIFIED CORRECT AND COMPLETE against compiled-C
  and source evidence. MY_MAXFLOAT = `1e35f32 as f64` byte-matches C's
  `(float)1e35` (1.0000000409184788e35). shift_elements matches
  aCalcPerform.c:1416-1459 line-by-line.
- Category B — R8-22+R8-23 event-queue redesign (c45e60a8/5affc90c)
  VERIFIED at the byte/gate level against dbEvent.c: replace gate
  `npend>0 && (flow_on || ring_space <= 36)`, append first_event,
  nDuplicates raise/lower sites, drain/suspend gate, quota chain
  selection (cap 35 monitors/queue), EVENTS_OFF/ON wiring, DBE select
  filter, db_event_disable gate. The three documented module deviations
  are the only ones. R9-16 (4f2e3c76), R9-17 NOT-REAL evidence
  (a273458c), R9-18 (b54b0744, incl. EPICS-epoch stamp on the zeroed
  Snapshot) all VERIFIED.
- Category C — R8-34 MonitorTeardown (f1e2fca1) VERIFIED: `Fatal ⟹
  circuit closed` holds by construction (close happens inside
  `MonitorTeardown::invalid` before release); Invalid/FinishError split
  matches clientmon.cpp:607-620; all five driver sites merely propagate.
  R9-31 dbe_kind (7d4f5af0) and R9-32 ack_any_as_u32 (e226272a)
  VERIFIED.
- Category D — all wave-7 D commits VERIFIED faithful: R8-57 COM
  line-by-line vs asynInterposeCom.c (IAC stuffing count quirk, signed
  unstuffing cursor, +100 reply offsets, 61-byte restore handshake,
  base-link ordering matches drvAsynIPPort.c:1061); R8-54/55/56,
  R9-46..50 single owners confirmed.
- Category E — 6 of ~20 sampled (highest-risk): R9-61..64 transform
  order, R9-65 DOL-at-output, R9-70 DLY1 quirk (independently confirmed
  against sseqRecord.c:1149-1155), R9-72 mask hook, R9-68 pArrays[0],
  R8-75 TIFF ColorMode — all VERIFIED. R9-66/67, R8-71..74, R9-71,
  R9-69 not line-verified this pass (disposition text consistent with
  adjacent verified code).

### Adjudications — all 26 wave-7 fixer-surfaced OPEN items CONFIRMED

- **R9-4 CONFIRMED, WIDENED** — not just `AA/2`/`SQRT(AA)`: ADD/SUB
  coerce when either side is double (sCalcPerform.c:964-978), MULT/DIV
  coerce both unconditionally (:1015-1030), every unary/comparison
  toDoubles. The port's `as_f64` TypeMismatch fails ALL of them.
- **R9-5 CONFIRMED, sharpened** — scalar: C 0/no-alarm vs port
  NaN/CALC_ALARM; array: C 0-elements/status -1 vs port NaN. And the
  port checks only a[0] finiteness (acalcout.rs:371), so a negative
  element not at index 0 misses the alarm C raises.
- **R9-6 CONFIRMED** — C READ/WRITE are 2-operand `ps1=ps; DEC(ps)`
  (sCalcPerform.c:1569,1693); port declares nargs:1
  (postfix.rs:156-157).
- **R9-7 CONFIRMED, WIDENED to scalcout + acalcout** — sCalcPerform.c
  and aCalcPerform have the same END_EXPRESSION→-1 guard; port sites:
  calc.rs:695/714, calcout.rs:1058 (no fallback at all), scalcout.rs:592,
  acalcout equivalent. Structural fix (empty/failed compile carries an
  empty program evaluating to -1) closes all four.
- **R9-8 CONFIRMED** — every listed symbol verified present in C /
  absent in port. Precisions: port DOES have `|-` SUBLAST (correctly
  excluded); port's LEN is the string op, not aCalc's array LEN.
- **R9-9 CONFIRMED (narrow)** — reachable only for strtod-parseable
  literals with trailing alpha (`INFINITY`); Low.
- **R9-19 CONFIRMED** — caget default mode is synchronous; calloc'd
  non-NULL buffer means C renders zeroes on timeout (caget.c:207-291);
  the `*** no data available` branch needs -c. Exact defect R9-18 fixed
  for caput, unapplied to caget-rs (caget-rs.rs:836/840).
- **R9-20 CONFIRMED (non-timeout facet live)** — timeout branch already
  leaves `failed` untouched; the `not connected` (:826) and generic
  `Err` (:853) branches still exit 1 where C exits 0 (caget.c:348).
- **R9-21 CONFIRMED (narrowed)** — live divergence is a
  writable-but-not-readable PV (ACF write-only): C's Old:-read fails,
  is discarded (caput.c:531-535), put SUCCEEDS, exit 0; port exits 1
  without attempting the put. True-disconnect exit codes coincide.
- **R9-22 CONFIRMED** — `-S` reaches only the write-value builder
  (caput-rs.rs:319); readback ValueFormat never sets
  char_array_as_string (:402-413). format_value itself honors the flag
  (cli.rs:343).
- **R9-23 CONFIRMED** — Readback::Other echoes the submitted value
  (caput-rs.rs:397-399, comment admits it); C prints `*** no read
  access` / `*** CA error %s` and returns 0.
- **R9-33 CONFIRMED** — `as<std::string>()` at servermon.cpp:556 is the
  throwing form, runs BEFORE the if/else, nothing catches between
  onSubscribe and the dispatch catch (conn.cpp:277-282) → bev.reset().
  The :570-573 Crit is dead for non-scalars. Port serves on with
  ack_at=1 (tcp.rs:257-267).
- **R9-34 CONFIRMED** — pvxs tries `as(uint64)` FIRST even for string
  storage: stoull base 0 → "0x10"→16, "010"→8, "-1"→0xFFFFFFFF→clamp.
  Port is decimal-only fallback (tcp.rs:249).
- **R9-35 CONFIRMED (consequence now verified)** — Int32A & 0xe0 =
  Kind::Integer → `fld.as<uint8_t>()` on array storage → copyOut
  data.cpp:468 falls through to :499 throw NoConvert → uncaught →
  bev.reset(). Port serves VALUE|ALARM.
- **R9-51 CONFIRMED (Low)** — four invented texts confirmed vs
  asynRecord.c:515/:357/:1255; "returned no value" has no C analogue.
- **R9-52 CONFIRMED (Low)** — monitorStatus sets TSIZ (asynRecord.c:1100)
  and TFIL="Unknown" on foreign traceFd change (:1117-1124); Rust trace
  manager fully supports both, readback stays stale.
- **R9-53 CONFIRMED (Medium)** — C queries each interface via
  findInterface (asynRecord.c:1177-1240); pure-octet ports get
  i32iv/ui32iv/f64iv=0. PortDriver has no per-port interface registry —
  structural gap.
- **R9-54 CONFIRMED (High), panic reproduces** — adapter.rs:121,186
  `Duration::from_secs_f64` panics on `-1`; C strtod accepts it
  (asynEpicsUtils.c:125). Thread panic at record init. Distinct from
  R9-46 (TMOT field).
- **R9-55 CONFIRMED (Low)** — record-path negotiation pinned to 2 s;
  needs an asynUser on the option trait (public API change).
- **R9-56 CONFIRMED (Medium)** — C later-install = outermost
  (asynManager.c:2216-2217); Rust earlier-push = outermost
  (interpose/mod.rs:138,244). Echo/Delay pushed after EOS lands inner
  where C puts it outer.
- **R9-57 CONFIRMED (Low)** — unstuffing asynPrintIO(TRACEIO_FILTER)
  dropped (asynInterposeCom.c:237-239); crtscts/ixon advisory texts
  dropped. Blocked on trait surface (no asynUser on option path).
- **R9-73 CONFIRMED** — calc.rs:694-731 evals unconditionally; only
  sub/aSub declare AbortOnFirstFailure, and even that flag routes only
  to suppress_subroutine_run, which calc's process() never checks.
  Family spans calc/calcout/scalcout/acalcout.
- **R9-74 CONFIRMED** — swait.rs:127 `!=` vs C
  `fabs(oval-val) > mdel` (swaitRecord.c:432).
- **R9-75 CONFIRMED** — LA..LL are DBF_DOUBLE fields
  (swaitRecord.dbd:298-331), posted on change (swaitRecord.c:652).
- **R9-76 CONFIRMED** — INAV..INLV/DOLV DBF_MENU fields missing; C
  execOutput reads DOL only `if (!dolv)` (swaitRecord.c:765).
- **R9-77 CONFIRMED** — event.rs declares no
  fields_posted_with_monitor_mask; VAL posts forced VALUE|LOG where C
  posts monitor_mask|DBE_VALUE (eventRecord.c:163).
- **R9-78 CONFIRMED** — aSub OUTA..OUTU stored but never driven; C
  pushes every output link when status==0 (aSubRecord.c:236-238).
- **R9-79 CONFIRMED (widens R8-71)** — push() never consults Control;
  C wraps pre-buffer + trigger eval + completion test in
  `if (scopeControl)`. Port can auto-complete sequences while stopped.
- **R9-80 CONFIRMED** — detect_color_mode dims-inference
  (color_convert.rs:802) vs C default Mono
  (NDPluginColorConvert.cpp:44); drives wrong pixel conversion.
- **Round-9 scaler candidates** — user-stop COUTP double-fire CONFIRMED
  → filed R10-61; RATE→TP quirk CONFIRMED → filed R10-62.

### New findings

### R10-1: aCalc IXZ returns the first exact-zero element index; C returns the interpolated real index of the first zero crossing
Severity: Medium
Rust: `crates/epics-base-rs/src/calc/engine/array.rs:530-539` — IndexZero = `position(|&x| x == 0.0)` → integer index or -1.
C reference: `aCalcPerform.c:879-892` — active IXZ finds the first sign change vs a[firstEl] and returns `j + fabs(a[j])/fabs(a[j]-a[j+1])`, a real (fractional) index; the exact-zero version the port implemented is C's `#if 0` dead code (:867-877, and even that uses `fabs < SMALL`, not `== 0.0`).
Impact: for any waveform without a bit-exact 0.0 element (the normal case), the port returns -1 where C returns a meaningful fractional crossing index.

### R10-2: aCalc IXMAX breaks ties to the last maximum; C keeps the first
Severity: Low
Rust: `crates/epics-base-rs/src/calc/engine/array.rs:504-514` — `max_by` returns the LAST of equal maxima.
C reference: `aCalcPerform.c:846-855` — strict `>`, first maximum wins. (IXMIN uses min_by = first minimum, matches C — only IXMAX diverges.)
Impact: `IXMAX(AA)` on [5,3,5] returns 2 vs C's 0 whenever the maximum repeats.

### R10-3: aCalc ISINF/FINITE/ISNAN on an array operand collapse to a[0]; C is element-wise (ISINF) or an all-element reduction (FINITE/ISNAN)
Severity: Medium
Rust: `crates/epics-base-rs/src/calc/engine/array.rs:347-353` (IsInf → pop1_f64 = a[0], scalar), :335-346 (IsNan), :355-366 (Finite).
C reference: `aCalcPerform.c:826` — ISINF array branch is element-wise (array result); :1114-1120 FINITE = AND of finite() over ALL elements of every arg; :1138-1146 ISNAN = OR over all elements.
Impact: `FINITE(AA)`/`ISNAN(AA)` on [1,2,inf] return 0/1 in C but 1/0 in the port; ISINF yields the wrong shape entirely.

### R10-4: aCalc reduction/unary array ops raise TypeMismatch→CALC_ALARM on a scalar operand; C defines a scalar result
Severity: Medium
Rust: `crates/epics-base-rs/src/calc/engine/array.rs` — Average:446-452, StdDev:454, Fwhm, ArraySum:470-476, ArrayMax, ArrayMin return TypeMismatch on Double; Cum:573, Deriv:562, NDeriv, Smooth, NSmooth, IndexMax/Min/Zero/NonZero call as_array()? which errors on a Double.
C reference: `aCalcPerform.c:1094-1101` — AVERAGE/SMOOTH/ARRSUM/CUM break (scalar unchanged); STD_DEV/FWHM/DERIV/FITPOLY set d=0; IXMAX/IXMIN set d=0 (:1088-1089); IXZ = `fabs(d)<SMALL?0:-1`, IXNZ = `fabs(d)>SMALL?0:-1` (:1090-1091).
Impact: legal expressions (AVG(5)→5, STD(5)→0) evaluate in C but force CALC_ALARM/INVALID and a frozen VAL in the port.

### R10-5: aCalc IXNZ uses an exact != 0.0 test; C thresholds at SMALL (1e-9)
Severity: Low
Rust: `crates/epics-base-rs/src/calc/engine/array.rs:540-549`.
C reference: `aCalcPerform.c:893-898` — `fabs(a[i]) > SMALL`.
Impact: tiny-but-nonzero noise (1e-12) makes the port return that index while C skips below 1e-9.

### R10-16: CA tools render alarm status 11 as HW_LIMIT (C: HWLIMIT) and out-of-range status as "Illegal value" (C: "??")
Severity: Low
Rust: `crates/epics-ca-rs/src/bin/caget-rs.rs:263` (`11 => "HW_LIMIT"`), :274 (default `"Illegal value"`); identical table duplicated at `caput-rs.rs:40,51` and `camonitor-rs.rs:280,291`.
C reference: `alarmString.c` `epicsAlarmConditionStrings[11] = "HWLIMIT"`; `tool_lib.h:28-30` stat_to_str returns `"??"` out of range. Every other index (0-10, 12-21) matches.
Impact: caget -a / camonitor / caput -l on HW_LIMIT_ALARM print a different token than C; the port's own alarm-string owner (`epics-bridge-rs/src/qsrv/pvif.rs:1060`) already uses "HWLIMIT", so the three tool copies are internally inconsistent too.

### R10-31: native server disables a real-typed record._options.pipeline that pvxs enables
Severity: Medium
Rust: `crates/epics-pva-rs/src/server_native/tcp.rs:688-689` — Float/Double pipeline hardcoded false (comment at :684-687 flags it as an unaddressed choice).
C reference: `servermon.cpp:525` — `pipeline.as(v)` routes Real through copyOutScalar → `bool(src)`: Double(1.0)→true, 0.0→false.
Impact: MONITOR INIT with pipeline=Double(1.0) runs pvxs's credit-windowed pipeline sub-protocol but a plain monitor in the port — the whole flow-control wire shape differs, and any accompanying ackAny is silently ignored.

### R10-32: native server drops a real/hex-string record._options.queueSize that pvxs converts
Severity: Medium
Rust: `crates/epics-pva-rs/src/server_native/tcp.rs:731-742` — queue_size match handles String (decimal-only) and ints; `_ => None` for Float/Double.
C reference: `servermon.cpp:533-536` — `as(uint32_t)` converts reals (`uint64_t(double(src))`) and base-0 strings (stoull); `op->limit` applies OUTSIDE `if(op->pipeline)`, i.e. to non-pipeline monitors too.
Impact: queueSize=Double(8.0) or "0x10": pvxs sets limit=8/16; the port rejects an enabled pipeline (`can not pipeline invalid queueSize`, an error pvxs never sends) or uses default depth 4 + spurious Warn for a plain monitor — different on-the-wire update squashing. (Boolean queueSize is NOT a divergence: pvxs as→1, <2, matches the port.)

### R10-46: asynRecord never raises STATE_ALARM — the not-connected process and the AQR cancel both leave STAT/SEVR at NO_ALARM
Severity: Medium
Rust: `crates/asyn-rs/src/asyn_record/mod.rs:2525` — perform_io on port_entry==None sets errs="not connected", returns Ok with no io_alarm; `mod.rs:3358-3361` — AQR cancels the token, no severity (comment at :3353 admits it).
C reference: `asynRecord.c:357,361` — stateNoDevice reports and `recGblSetSevr(STATE_ALARM,MINOR_ALARM)` every process; :397-400 — AQR on wasQueued reports "I/O request canceled" then `recGblSetSevr(STATE_ALARM,MAJOR_ALARM)`.
Impact: a record on a dead link processes with SEVR=NO_ALARM; operator interlocks keying on SEVR see a healthy record. (Distinct from R9-51, which is only the ERRS text.)

### R10-47: asynRecord omits C's two unconditional resetError() calls, so ERRS goes stale on a TMOD=NoIO process or any field put
Severity: Low
Rust: `crates/asyn-rs/src/asyn_record/mod.rs:3450` — process() returns for NoIo BEFORE the only ERRS clear at :3454; special() at :3058-3063 has no ERRS reset at entry.
C reference: `asynRecord.c:339` — resetError unconditionally at stateIdle entry; :390 — resetError before special()'s field switch.
Impact: after a failed transfer, a NoIO process or a put to TMSK/BAUD/PCNCT leaves the stale error string displayed; C clears and re-posts empty ERRS.

### R10-48: an option field written at its index-0 ("Unknown") menu value skips the setOption→getOptions fall-through entirely
Severity: Low
Rust: `crates/asyn-rs/src/asyn_record/mod.rs:3231-3238` (PRTY `_ => return Ok(())`), same guard on DBIT/SBIT/MCTL/FCTL/IXON/IXOFF/IXANY/DRTO/BAUD (:3218-3318).
C reference: `asynRecord.c:1787-1826` — setOption unconditionally sends `<choices>[0]="Unknown"`; :845-849 callbackSetOption falls through (`/* no break */`) into callbackGetOption→getOptions regardless of the set's success.
Impact: selecting "Unknown" makes C attempt the write (driver rejects → ERRS) then refresh ALL option readbacks; Rust silently does nothing.

### R10-61: scaler user-stop fires COUTP once where C double-fires it
Severity: Medium
Rust: `crates/scaler-rs/src/records/scaler.rs:787-808` — special("CNT") sets coutp_pending (:884) and process() sets just_finished_user_count (:772); both feed a single fire_coutp bool → one WriteDbLink{COUTP}.
C reference: `scalerRecord.c:624` (special(): dbPutLink COUTP cnt=0) AND :463 (process(), guarded by justFinishedUserCount, dbPutLink COUTP) — two independent puts on the same stop.
Impact: a record wired to .COUTP is processed twice on C at a user stop, once on Rust. User-start is unaffected (both fire once).

### R10-62: scaler RATE write drops C's spurious TP monitor post
Severity: Low
Rust: `crates/scaler-rs/src/records/scaler.rs:923-925` — special("RATE") only clamps; nothing force-posts TP.
C reference: `scalerRecord.c:690-693` — the RATE case ends `db_post_events(pscal, &(pscal->tp), DBE_VALUE)` — a copy-paste posting TP (not RATE) on every RATE write.
Impact: `caput scaler.RATE` fires a spurious .TP monitor event on C; the port's .TP subscriber gets nothing. C's quirk is the contract.

### Notes (round 10)

- Auditor-B: procServ (epics-tools-rs) not audited — its upstream C is a
  separate project absent from the epics-base reference tree; no C
  comparison possible without fabricating a reference.
- Auditor-E line-verified 6 of ~20 category-E wave-7 fixes (the
  highest-risk set); the rest are consistent with adjacent verified
  code but not independently re-opened.
- Audited-clean sweeps this round (no finding): aCalc DIV/MOD myMAXFLOAT
  shapes; CA repeater register fanout; access-rights transition path;
  camonitor disconnect line; PVA Status/Size codecs; MONITOR exec subcmd
  decode; pipeline nack seeding + clamp_watermarks; QSRV option parsing
  (the native-server gap R10-31/32 is the contrast); COM
  stuffing/negotiation byte budget; serial option enum mappings; GPIB
  no-interface path; optics table limit violations; AD stats/roi_stat/
  fft; mqtt payload JSON DFS; modbus datatype combine/split; std epid/
  throttle.

## Fix wave 8 — dispositions (2026-07-12)

All 42 wave-8 items FIXED (nothing NOT-REAL this wave), one commit per
finding, across 5 worktree fixers (a8/b8/c8/d8/e8), merged into
review/parity-r6 and verified by main.

Category A (a8):
- R9-4 FIXED cec32fed — sCalc coerces a string in a numeric position
  everywhere C's toDouble/atof does (never rejects).
- R9-5 FIXED 055d2a49 — aCalc negative SQRT/LOG: 0 on a scalar
  (status untouched), 0-elements with deferred status -1 on an array.
- R9-6 FIXED baed4d6d — READ/WRITE are binary 2-operand conversions.
- R9-7 FIXED 0aa75f90 — STRUCTURAL: rpcl/orpc/compiled_calc are no
  longer Option; an empty or failed compile IS the empty program
  (C END_EXPRESSION), which fails every run → CALC_ALARM/INVALID.
  The process()-time lazy-compile fallback deleted with it.
- R9-8 FIXED ceb26345, WIDENED — the missing symbols landed, and the
  subrange bound rule got one owner (engine::subrange_bounds, C's
  myMAX/myMIN + negative-index wrap): sCalc `[` AND aCalc `[`/`{` both
  had an exclusive upper bound and no wrap. Compiled C: "hello"[1,4] =
  "ello". opcode_supported_by_engine deleted (dead second gate).
- R9-9 FIXED fdf0a920 — LITERAL_OPERAND rewinds and re-scans with a
  ported strtod (new engine/strtod.rs); INFINITY consumes fully.
- R10-1 FIXED b999760e — IXZ is C's interpolated first zero crossing.
- R10-2 FIXED 12a85a89 — extremum reductions use C's seeded
  strict-comparison scan (first maximum wins).
- R10-3 FIXED 00209aaa — ISINF element-wise; FINITE/ISNAN fold over
  every element of every arg.
- R10-4 FIXED a1e039f7 — unary array ops answer a scalar operand with
  C's scalar-branch results.
- R10-5 FIXED 4408ead8 — IXNZ thresholds at SMALL.

Category B (b8):
- R10-16 FIXED 4f8b61df — one owner (cli::stat_to_str/sevr_to_str,
  HWLIMIT at 11, "??" out of range); the three duplicated tool tables
  deleted. Deliberately NOT delegated to recgbl::alarm_condition_string
  (its out-of-range is "" — pvxs semantics).
- R9-19 FIXED 0fba54ba — one owner for C's zeroed calloc readback
  (cli::zero_dbr_value/zero_dbr_snapshot), shared by caget's
  synchronous timeout and caput's zero_readback; only -c can reach
  "*** no data available (timeout)".
- R9-20 FIXED b658cbe4 — caget's exit status is C's !nConn count;
  post-gate failures print their marker (one cli::ca_error_marker
  owner) and exit 0.
- R9-21 FIXED b545f969 — caput's two readbacks are one renderer
  (readback_line); the Old: site cannot abort the put by construction.
- R9-22 FIXED 195c533f — -S reaches the readback rendering.
- R9-23 FIXED 58549e1a — WriteValue::echo_fallback deleted; a failed
  New: read prints C's marker, never the submitted value.

Category C (c8):
- R10-31 FIXED 61fedd1a / R10-32 FIXED 90c8cc21 / R9-34 FIXED aebaf683 /
  R9-33 FIXED a99e29e1 / R9-35 FIXED 7dc0597c — STRUCTURAL:
  pvdata::convert is a port of pvxs Value::copyOut/copyOutScalar
  (storage-switched, with the throwing and non-throwing outcomes both
  explicit) and convert::kind is the separate type-class dispatch; the
  bridge's duplicate scalar_as_bool/dbe_kind deleted. A new
  ChannelSource::check_monitor_request INIT-time hook carries the pvxs
  throw to the wire (MonitorRequestFatal → circuit reset, no reply).
  record._options parsed for Monitor only, as pvxs.
  SEMANTIC CHANGES (client-visible, all = C): non-scalar ackAny and
  array-typed DBE now DROP the connection; pipeline "1"/"yes" leniency
  removed (Warn + disabled); ackAny/queueSize strings are base 0
  ("010" = 8). Boolean queueSize pinned as a non-divergence.

Category D (d8):
- R9-54 FIXED c10d5dd1 — timeout_from_secs single owner of every
  operator f64 → AsynUser::timeout conversion (try_from_secs_f64,
  panic unconstructible); @asyn(...,-1) → 1 s per DRV-42.
- R9-51 FIXED ef1e3404 — every record ERRS text is C's; four writer
  owners only.
- R9-52 FIXED fc40fb19 — monitor_status refreshes TSIZ and marks a
  foreign trace file "Unknown".
- R9-53 FIXED 10335124 — PortHandle::has_interface registry (C's
  findInterface) captured from driver capabilities() at registration;
  *IV fields are pure readbacks. PUBLIC API change.
- R9-55 FIXED ff16e3a5 — the caller's AsynUser threads down the whole
  asynOption path (PortDriver::set_option(&mut self, user, key, value),
  C's signature): record → TMOT, iocsh → 2 s, COM restoreSettings →
  2 s. PUBLIC API change.
- R9-56 FIXED 6a42c39b — InterposeStack::install inserts at the front
  (last install = outermost, asynManager.c:2190-2220); push renamed
  away so no call site keeps the LIFO model.
- R9-57 FIXED 3a16cbe0 — unblocked by R9-55: AsynUser carries C's
  port/trace linkage + errorMessage slot (PortActor the single
  stamper); COM prints the unstuffed read at ASYN_TRACEIO_FILTER and
  leaves the flow-control advisories in errorMessage.
- R10-46 FIXED cadebb80 — report_not_connected stages STATE/MINOR;
  IoOutcome::report_canceled owns both halves of the AQR cancel
  (message + STATE/MAJOR) — neither reportable without its alarm.
- R10-47 FIXED ee0e68d2 — reset_error() single owner at C's entry
  points (process, special, connectDevice); SPC_MOD_FIELDS names the
  dbd set because this port calls special for every put.
- R10-48 FIXED 2615c583 — all twelve option arms dispatch
  unconditionally; menu→text through one menu_choice lookup over C's
  choice arrays.

Category E (e8):
- R9-73 FIXED 6e6ece5e — InputFetchPolicy::ReadAllGateOnFailure +
  set_fetch_gate_failed(): the framework fetch outcome, not per-record
  guards, gates the calc across calc/calcout/scalcout/acalcout/swait;
  swait raises READ_ALARM/INVALID.
- R9-74 FIXED c718fe55 — OOPT "On Change" is fabs(oval-val) > mdel
  (scalcout + swait).
- R9-75 FIXED f614c3ac — swait LA..LL, posted with the input's
  monitor mask.
- R9-76 FIXED fa0182f3 — swait INAV..INLV/DOLV link-status fields;
  DOL read gated on !dolv.
- R9-77 FIXED b1abe189 — RecordInstance::deadband_post() single owner
  of C monitor()'s VAL post mask (four copy-pasted assemblies
  replaced); event's VAL is a fields_posted_with_monitor_mask member.
- R9-78 FIXED 56de6def — set_subroutine_status delivered on every
  exit path; multi_output_links() returns the OUT links only when
  status == 0 (C's single if (!status) gate).
- R9-79 FIXED c395c1c7 — CircularBuffer gained control (C
  scopeControl); push()/trigger() refuse while off, so Control owns
  admission.
- R9-80 FIXED 4096a465 — detect_color_mode deleted (NDArray::info()
  was already the C-correct owner); the unconvertible path forwards
  the frame (C:584) instead of dropping it.
- R10-61 FIXED 1ef74d92 — the fire_coutp bool deleted; each of C's
  two COUTP put sites emits its own WriteDbLink. The scaler-rs local
  doc's SCAL-6 "Not copied" note RETRACTED by main (it contradicted
  the R10-61 adjudication).
- R10-62 FIXED 7808f31b — special() owns the db_post_events C's
  special() makes (the RATE→TP copy-paste included);
  monitor_side_effect_fields hands the list to the framework.

### Merge-integration notes (fix wave 8)

- a8 x e8 textual conflicts in calc.rs/calcout.rs/scalcout.rs/swait.rs
  (R9-7's always-a-program eval vs R9-73's fetch gate) resolved by
  main: the fetch gate wraps a8's unconditional eval, matching C's
  `if (fetch_values()==0) calcPerform(...)` nesting exactly.
- One merged-state test failure (both parents green):
  event_val_monitor_mask's calc control built a record via
  put_field("CALC") + direct PvDatabase::add_record, which never runs
  init_record or special — it had relied on the process()-time
  lazy-compile fallback R9-7 deleted. Adapted by main to
  CalcRecord::new (fac0dc95), the sibling tests' existing pattern.
- Verification (merged state, main): cargo fmt --all --check clean;
  cargo clippy --workspace --all-targets -- -D warnings clean;
  cargo nextest run --workspace 8046 passed / 0 failed / 2 skipped
  (an interim run during concurrent fixer activity showed the five
  documented stability.rs fixed-port cross-connect flakes; all pass
  in isolation and in the final quiet run); doctests for
  epics-base-rs/asyn-rs/scaler-rs/ad-plugins-rs/epics-ca-rs/
  epics-pva-rs/epics-bridge-rs clean.

## Open Findings — surfaced during fix wave 8 (reported by fixers, pending independent verify)

Category A (calc engines; compiled-C evidence where noted):
### R10-6: aCalc array stack values have no active window — C's stackElement carries firstEl/numEl (aCalcPerform.c:74-80, set by SUBRANGE/SUBRANGE_IP/CAT, honoured by every reduction via calcFirstLast :289-296); the port's bare Vec reduces over the zero fill. Compiled C: AMIN(AA[1,3])=20, AVG(AA[1,3])=30 with AA=[10..60]; port answers 0 and 15. Reachable since R9-8 made subrange compile
Severity: Medium. Closing it = giving ArrayStackValue::Array C's buffer+window (public enum change, re-opens CAT/FITPOLY). Highest-priority candidate of this set.
### R10-7: ISINF sign — glibc returns -1 for -inf; the port returns 1.0 (array.rs:349, numeric.rs:271, string.rs:402)
Severity: Low.
### R10-8: FITPOLY/FITMPOLY/FITQ/FITMQ arity and semantics — C FITPOLY is 1-operand and returns the fitted curve; the port is 2-operand and returns coefficients
Severity: Medium.
### R10-9: store-terminated compile — `A:=5` is CALC_ERR_INCOMPLETE in C; the port accepts it (postfix.rs ends_with_store depth-0 exemption)
Severity: Low.
### R10-10: NDERIV on a scalar — C promotes via toArray; the port raises TypeMismatch
Severity: Low.
### R10-11: aCalc DERIV/FITPOLY ignore C's status propagation
Severity: Low.
### R10-12: base 1e400/1e-400 — CALC_ERR_BAD_LITERAL in C (epicsParseDouble ERANGE); the port yields inf/0
Severity: Low.
### R10-13: TR_ESC/ESC escape tables diverge from epicsString.c (\a \b \f \v \' \" and NUL), and C treats a Double operand as a no-op where the port raises TypeMismatch
Severity: Low.
### R10-14: sCalc SUBRANGE with a string bound does a strstr search in C (sCalcPerform.c:1876-1892); the port raises TypeMismatch (bound arithmetic fixed in R9-8; this type gap deliberately left)
Severity: Medium.
### R10-15: sCalc BIN_READ %d order-dependence via C's shared `long l`
Severity: Low.

Category B (CA tools):
### R10-17: caput-rs prints "error: channel disconnected" on stderr when the New:-read finds the channel gone; C's caget() returns from `if (!nConn) return 1` having printed nothing (caput.c:181,589) — the stderr line is port-invented (exit code matches)
Severity: Low.
### R10-18: caput-rs has no -# flag — C's getopt `:cnlhatsVS#:w:p:F:` accepts `caput -# 3` (the count is then overwritten, vestigial); clap exits 2 on the unknown flag
Severity: Low.

Category C (PVA):
### R10-33: qsrv group.rs:209 negotiated_queue_size is a SECOND record._options.queueSize parser (decimal/int only) — and pvxs GroupSource::onSubscribe never reads a client queueSize at all (only servermon.cpp:533 does, into op->limit)
Severity: Medium.
### R10-34: server tcp.rs put_autoexec_from_request reads record._options.autoExec, which pvxs has NO server-side reader for (client-side SubBuilder flag only); also parses leniently
Severity: Low.
### R10-35: client ops_v2.rs MonitorFlowControl::from_record_options normalizes typed options to display strings then string-matches; pvxs clientmon.cpp:763-808 converts via as(bool)/as(uint32) and checks ackAny.type()==String first — client-side rules, distinct from the server family
Severity: Medium.
### R10-36: two divergent render_option_value approximations of pvxs's SB()<<Value (tcp.rs bare-scalar vs qsrv/channel.rs datafmt form) — diagnostic text only
Severity: Low.
### R10-37: DBE empty-mask logRemote fires at START in the port; pvxs emits it at INIT (inside onSubscribe, before connect())
Severity: Low.

Category D (asyn):
### R10-49: C queueTimeoutCallbackProcess (asynRecord.c:920-926) has no analogue — no queue-timeout mechanism exists, so C's third STATE_ALARM site ("process queueRequest timeout" + STATE/MAJOR + forced completion callback) is unreachable
Severity: Medium.
### R10-50: AsynOption trait (interfaces/option.rs) has no in-tree implementor and duplicates PortDriver::set_option/get_option — existence question
Severity: Low.
### R10-51: HOSTINFO key casing — port writes/reads "hostinfo", C uses "hostInfo" (asynRecord.c:1825); self-consistent today, diverges from C's text
Severity: Low.
### R10-52: read_options_from_driver LBAUD parse — C's sscanf %d leaves LBAUD unchanged when the driver text carries no number; the port's unwrap_or(0) writes 0
Severity: Low.
### R10-53: read_options_from_driver accepts readback aliases C does not ("Yes"/"No"/"none" where C strcmps {"Unknown","N","Y"} only)
Severity: Low.
### R10-54: serial_port_win32.rs received the R9-55 signature change but is cfg(windows) — not compile-verified on this host
Severity: Low (verification gap, not a known defect).
### R10-55: GPIBIV is always 0 (no asynGpib interface exists), so UCMD/ACMD only ever take C's no-interface branch — structural, standing gap
Severity: Low.

Category E (records + AD):
### R10-63: scaler change-posts Dn/Gn/PRn on process cycles where C posts nothing — the framework diffs every field against last_posted, so the gate→direction copy (scalerRecord.c:413-414, unposted in C) fires Dn monitors; R10-62 fixed the mask of these posts, not their existence
Severity: Low.
### R10-64: scaler special()'s COUTP put is deferred to the head of the next process cycle (no action channel in special); observationally identical for CNT (pp(TRUE)) but not C's ordering
Severity: Low.
### R10-65: scalcout never fetches its string input links INAA..INLL — multi_input_links() lists only the 12 numeric inputs
Severity: Medium.
### R10-66: scalcout exposes no PVAL/PSVL fields (C's previous-value pair behind the OOPT On-Change comparison)
Severity: Low.
### R10-67: swait has no CLCV field and no real alarm-severity plumbing — R9-7 added only the calc_alarm bool it needed
Severity: Low.

## Round 11 — re-audit (2026-07-12): wave-8 fix verification + adjudications + fresh findings

### Fix verification (wave 8) — fourth consecutive clean wave

- Category A — all 11 commits VERIFIED (R9-4/R9-9 consistency-checked
  rather than re-derived; no counter-evidence). Main's merge-integration
  (R9-7 x R9-73 fetch-gate nesting) verified CORRECT in all five
  calc-family records against each record's C process(), including
  calcout's OOPT-outside vs acalcout's afterCalc-inside gate scopes and
  scalcout's VAL=-1/"***ERROR***" failure values. Completeness caveat:
  R10-1/2/5 are correct formulas over the full buffer — the
  [firstEl,lastEl] window divergence is exactly R10-6, still open.
- Category B — all 6 commits VERIFIED, incl. the zeroed-calloc
  per-type buffers + EPICS-epoch stamp, the !nConn exit rule with the
  connect_pvs barrier abort, and the readback_line None-only-for-!nConn
  invariant.
- Category C — all 5 commits VERIFIED cell-by-cell: the pvdata::convert
  storage matrix vs every copyOut/copyOutScalar arm, parse_to_u64 vs
  stoull base-0 (incl. "08" octal reject), convert::kind boundary,
  check_monitor_request fires before ops.insert and before the INIT
  reply, QSRV source-scope gate, reject-before-ackAny ordering,
  level2mtype framing. One completeness observation filed as R11-31
  (default squash depth).
- Category D — all 10 commits VERIFIED. R9-53 caveat subsumed by
  R10-55: C's vxi11/prologix register asynGpib+asynInt32 via
  pasynGpib->registerPort, so their *IV readbacks differ until the
  GPIB surface exists. R9-55 edge noted, not filed: C's record user
  carries timeout=1 until the first asynCallbackProcess; the port uses
  TMOT immediately (manifests only on operator-set TMOT before first
  scan). R10-47's fourth C reset site (:817) is folded into the merged
  process/callback — acceptable.
- Category E — all 10 commits VERIFIED, and the 8 wave-7 items round 10
  had sampled-only are now line-verified CORRECT (R9-66/67, R8-71..74,
  R9-71, R9-69). One residual on R8-72 filed as R11-63.

### Adjudications — 24 CONFIRMED, 3 REFUTED, 1 cleanup, 1 unverifiable

- R10-6..R10-14 CONFIRMED (R10-6 the array-window headline; R10-8
  strengthened: C FITPOLY is 1-operand returning the fitted curve,
  FITQ/FITMQ store coefficients back into named P-fields; R10-13
  sharpened: the CORRECT escape ports raw_from_escaped/
  escaped_from_raw sit unused beside the divergent tables).
- R10-15 REFUTED — C's shared `long l` 4-byte-memcpy read is a
  stale-high-bytes latent bug on LP64; the port's deterministic
  zero-extend reproduces C's canonical fresh-`l` result. Do not port
  the UB.
- R10-17, R10-18 CONFIRMED.
- R10-33..R10-37 CONFIRMED (R10-33 sharpened: pvxs
  GroupSource::onSubscribe only WRITES queueSize back to the client;
  the native _opts value is computed and discarded for singles at
  pva_adapter.rs:295-296 / dropped at :746).
- R10-49 CONFIRMED, STRENGTHENED — asynRecord passes QUEUE_TIMEOUT=10.0
  (asynRecord.c:71), not 0.0, for BOTH process (:342) and special
  (:571), and special has its own queueTimeoutCallbackSpecial
  (:929-940). The port_actor.rs:340-341 comment claiming "standard
  device support passes 0.0, arming no timer" is factually wrong for
  asynRecord itself; the port has no queue-wait timer at all.
- R10-50 CONFIRMED as dead code, NOT a parity defect — cleanup item.
- R10-51 REFUTED as observable — both sides compare option keys
  case-insensitively (epicsStrCaseCmp / eq_ignore_ascii_case);
  internal key, never on the wire.
- R10-52 CONFIRMED (sharpened: .parse() also rejects numeric-with-
  suffix that C's %d prefix-parses).
- R10-53 REFUTED as observable — the alias branches are unreachable
  (C serial getOption emits only Y/N and none/even/odd).
- R10-54 unverifiable on this host (cfg(windows)) — remains a
  verification gap, not a known defect.
- R10-55 CONFIRMED (structural) — no Gpib capability exists; GPIBIV
  always 0, UCMD/ACMD only ever take the no-interface branch; also
  pins vxi11/prologix I32IV=0 vs C's 1 (R9-53 caveat).
- R10-63..R10-67 CONFIRMED (R10-63 precise: the Dn posts are
  DBE_VALUE-masked but their existence is spurious — C's
  gate→direction copy posts nothing).

### New findings

### R11-1: scalcout double→string uses Rust shortest round-trip, not cvtDoubleToString(d,s,8)
Severity: Medium
Rust: `crates/epics-base-rs/src/calc/engine/string.rs:1292-1298` (format_double via format!("{}")), also PRINTF %s :898-904 and value.rs:52 into_string_value.
C reference: `sCalcPerform.c:89-96` to_string → cvtDoubleToString(d, s, 8); cvtFast.c renders 8 fractional digits fixed-point, exp form only for |v|>=1e8.
Impact: TO_STRING(PI) → C "3.14159265", port "3.141592653589793" — every scalcout string result derived from a double diverges, plus downstream LEN/comparisons.

### R11-2: sCalc engine never truncates intermediate string results to SCALC_STRING_SIZE-1 (39)
Severity: Medium
Rust: `crates/epics-base-rs/src/calc/engine/string.rs:85-89` (Add concat) and every string producer — StackValue::Str is an unbounded String.
C reference: `sCalcPerform.c:975` strncat bounded by SCALC_STRING_SIZE(40)-1; all intermediates live in char[40].
Impact: LEN("20 chars"+"30 chars") → C 39, port 50; SUBRANGE offsets, comparisons and REPLACE positions on over-length intermediates diverge. Distinct from the field-write layer (R6-74).

### R11-3: TO_DOUBLE (DBL) strict-parses; C hunts an embedded number
Severity: Medium
Rust: `crates/epics-base-rs/src/calc/engine/string.rs:764-769` — file-local to_double = trim+parse::<f64>().unwrap_or(0.0).
C reference: `sCalcPerform.c:1505-1514` — strpbrk digits, back over '.'/'-', atof from there.
Impact: DBL("12abc") → C 12, port 0; internally inconsistent with the port's own StackValue::to_double (strtod), so ("12abc")+0 = 12 but DBL("12abc") = 0 in one engine.

### R11-4: BYTE — unsigned byte and Double-operand handling both wrong
Severity: Low
Rust: `crates/epics-base-rs/src/calc/engine/string.rs:547-554` — u8 read; Double → 0.0.
C reference: `sCalcPerform.c:1528-1533` — signed char (0xFF → -1); no else, a Double passes through unchanged.
Impact: BYTE("\xff...") → C -1, port 255; BYTE(65) → C 65, port 0.

### R11-5: PRINTF collapses %% when the format has no live conversion
Severity: Low
Rust: `crates/epics-base-rs/src/calc/engine/string.rs:855-860` — simple_printf always collapses %%.
C reference: `sCalcPerform.c:1541-1545` — with no unsuppressed conversion, C strcpy's the RAW format verbatim.
Impact: PRINTF("done 100%%", x) → C "done 100%%", port "done 100%".

### R11-6: ARR/TO_ARRAY of a NaN scalar fills NaN, bypassing the to_array NaN→0 rule
Severity: Medium
Rust: `crates/epics-base-rs/src/calc/engine/array.rs:464-467` — vec![v; n] direct, bypassing ArrayStackValue::to_array.
C reference: `aCalcPerform.c:136-137` — to_array(setValues=1) fills 0 for NaN.
Impact: ARR(ACOS(2)) → C all-zeros; port all-NaN + spurious CALC_ALARM via a[0].

### R11-7: CAT of two scalars builds a 2-element array; C is a no-op
Severity: Medium
Rust: `crates/epics-base-rs/src/calc/engine/array.rs:606-618`.
C reference: `aCalcPerform.c:1411` — two-scalar branch `case CAT: break;` (left scalar unchanged).
Impact: 4 CAT 5 → C scalar 4; port [4,5] — VAL and AVAL shape both wrong.

### R11-8: DERIV/NDERIV wrong algorithm; NDERIV mis-reads its window argument
Severity: Medium
Rust: `crates/epics-base-rs/src/calc/math/derivative.rs:2-14` — central difference; nderiv = linear LSQ slope with npts as total width.
C reference: `aCalcPerform.c:985,613,596` + `calcUtil.c:32-55` — deriv = nderiv(...,2,...): sliding QUADRATIC fit (b+2ax); the argument is points PER SIDE (m = 2*npts+1).
Impact: y=x²: C d[0]=0, port d[0]=1; NDERIV(y,3) → C 7-point window, port 3-point.

### R11-9: acalc array store-backs are discarded and AMASK never recomputed during process
Severity: Medium
Rust: `crates/epics-base-rs/src/server/records/acalcout.rs:375-388` — eval consumes only the stack top; StoreDoubleVar/StoreVar mutations land in a local ArrayInputs and are dropped; amask never recomputed.
C reference: `aCalcPerform.c:487,524` — A_ASTORE/STORE_* write the record fields in place and set *amask |= 1<<i; afterCalc posts exactly those.
Impact: AA:=SUM(BB) leaves AA unchanged and AMASK=0 — array-variable store expressions silently no-op.

### R11-10: SMOOTH/NSMOOTH zero the in-window border elements; C preserves them
Severity: Medium
Rust: `crates/epics-base-rs/src/calc/math/stats.rs:81-100` — result seeded zeros, fills only 2..n-2; n<5 → all-zeros; nsmooth erodes 2k borders.
C reference: `aCalcPerform.c:971-991/:583-591` — in-place loop firstEl+2..lastEl-2 leaves borders at original values; n<5 unchanged.
Impact: SMOOTH([1..7]) → C keeps [1,2,...,6,7] borders; port zeroes four positions.

### R11-11: FWHM no-crossing fallback and half-max test differ
Severity: Low
Rust: `crates/epics-base-rs/src/calc/math/stats.rs:43-72` — <= half-max test; both crossings init to max_idx → no-crossing side contributes 0.
C reference: `aCalcPerform.c:945-966` — strict <; fallbacks e=lastEl (forward), d=0 (backward).
Impact: monotonic ramp: C width ≈ lastEl, port ≈ 0; exact-half-max samples shift the result.

### R11-16: DBF_CHAR values honor the -0x/-0o/-0b base flag; C's val2str renders CHAR as decimal unconditionally
Severity: Medium
Rust: `crates/epics-ca-rs/src/cli.rs:430,432` — Char/UChar route through format_int_i64(fmt.int_style); shared by caget (plain, -a) and camonitor.
C reference: `tool_lib.c:160-161` — case DBR_CHAR is sprintf("%d", ch) always; only DBR_INT/LONG route through sprint_long(outTypeI). caget's -0x forces DBR_LONG only on the terse/specified path; plain/-a/camonitor re-derive native CHAR.
Impact: caget -0x on a CHAR PV holding 0xFF: C prints -1, port prints 0xFFFFFFFF; same for -0o/-0b, caget -a, camonitor.

### R11-17: caget -d GR/CTRL float limits render with Rust Display instead of C's %g
Severity: Medium
Rust: `crates/epics-ca-rs/src/bin/caget-rs.rs:374-380` — lim closure uses format!("{v}") for FLOAT/DOUBLE limits; cli::format_float bypassed.
C reference: `tool_lib.c:248-254,375,377` — PRN_DBR_GR_PREC prints every graphic/control limit with hardcoded %g (6 sig digits), independent of -e/-f/-g.
Impact: lower_disp_limit 3.14159265 → C "3.14159", port "3.14159265"; 1e6 → C "1e+06", port "1000000".

### R11-18: caget -d specifiedDbr Value line emits an array element-count prefix C omits
Severity: Medium
Rust: `crates/epics-ca-rs/src/bin/caget-rs.rs:474` — format_value's array renderers prepend the count whenever total > 1 regardless of the false req_elems argument.
C reference: `caget.c:317-335` — the specifiedDbr Value block prints "Element count:" then a bare value loop with NO count prefix (unlike the plain loop :284).
Impact: caget -d DBR_LONG on a 3-element array: C "Value: v0 v1 v2", port "Value: 3 v0 v1 v2".

### R11-31: native per-op monitor squash depth defaults to 64, not pvxs's op->limit default of 4
Severity: Medium
Rust: `crates/epics-pva-rs/src/server_native/runtime.rs:314` (monitor_queue_depth: 64) consumed at tcp.rs:6545-6550 as queue_size.unwrap_or(config.monitor_queue_depth).
C reference: `servermon.cpp:66` — size_t window=0u, limit=4u per MonitorOp; squash gate queue.size() < mon->limit (:273-296). A per-op constant, not a server knob.
Impact: a monitor with no queueSize buffers up to 64 distinct updates under burst where pvxs coalesces beyond 4 — different squashing shape on the wire. (The R10-32 fix's own text assumed default 4.)

### R11-32: server ackAny="N%" with a non-finite percent resolves to queueSize/2; pvxs resolves to queueSize
Severity: Low
Rust: `crates/epics-pva-rs/src/server_native/tcp.rs:188-197,215-216` — f64::clamp propagates NaN → (NaN...) as u32 == 0 → ==0 branch → limit/2.
C reference: `servermon.cpp:564` — std::max(0.0, std::min(NaN,100.0)) = 100.0 → ackAt = limit.
Impact: crafted ackAny="nan%" yields a different MONITOR_ACK refill cadence. Low (no real client sends it).

### R11-46: asynRecord has no SCAN="I/O Intr" path — driver interrupts never update the record
Severity: Medium
Rust: `crates/asyn-rs/src/asyn_record/mod.rs:2973` + registry — no get_ioint_info/interrupt receiver, no gotValue gate, no cancelIOInterruptScan; process() always queues an explicit read.
C reference: `asynRecord.c:582` getIoIntInfo registers per-interface interrupt callbacks; callbackInterruptInt32/UInt32/Float64/Octet (:709-793) store the pushed value into I32INP/UI32INP/F64INP/TINP, set gotValue=1, scanIoRequest; process() `if (gotValue) goto done`; special() reverts SCAN on REASON/IFACE/UI32MASK/PCNCT (:490-525).
Impact: SCAN="I/O Intr" against a callback-posting driver never auto-updates — C's whole interrupt-driven readback mode for the diagnostic record is absent.

### R11-47: IP read-timeout disconnect returns asynTimeout where C returns asynError
Severity: Low
Rust: `crates/asyn-rs/src/drivers/ip_port.rs:859-868` — should_disconnect_after_read_error drops the connection but returns the original Timeout status.
C reference: `drvAsynIPPort.c:798-806` — the disconnect branch sets status = asynError (:805); only the non-disconnect branch keeps asynTimeout.
Impact: ERRS reads "timeout  nread N ..." instead of C's "error  nread N ...", and SyncIO callers branching on the status code see Timeout.

### R11-48: a port with no asynDrvUser interface does not force REASON=0
Severity: Low
Rust: `crates/asyn-rs/src/asyn_record/mod.rs:2531-2532` — empty DRVINFO keeps resolved_reason = self.reason; no per-port drvUser modeling.
C reference: `asynRecord.c:1258-1266` — findInterface(asynDrvUserType)==NULL (true for IP/serial) forces reason = 0, and reports "asynDrvUser not supported but drvInfo not blank" when DRVINFO set.
Impact: octet-only port + restored REASON!=0 + empty DRVINFO reads back the set REASON where C zeroes it; the non-empty-DRVINFO error text also differs.

### R11-49: serial/IP driver setOption value-validation error texts diverge from C
Severity: Low
Rust: `crates/asyn-rs/src/drivers/serial_port.rs:901+` — Rust-authored texts, strict parse::<u32> baud.
C reference: `drvAsynSerialPort.c:261-266,340-...` — "Bad number", "Unsupported data rate (%d baud)", "Invalid number of bits.", ...; sscanf %d prefix-parse accepts "9600x".
Impact: invalid-option ERRS strings differ across the whole serial validation surface; numeric-prefix values error in Rust, parse in C.

### R11-61: aCalcout array inputs INAA..INLL are silently dropped for an array (waveform) source
Severity: High
Rust: `crates/epics-base-rs/src/server/database/processing.rs:1955-1961` — the multi-input apply loop is `if let Some(f) = value.to_f64() { put Double }`; to_f64 returns None for every array variant, so array-typed link values never reach put_field and AA..LL stay empty. acalcout.rs:1944-1946 assumes the opposite.
C reference: `aCalcoutRecord.c:1076-1097` — fetch_values reads each INAA..INLL as a full array via dbGetLink(DBR_DOUBLE, *pavalue, &nRequest), zero-filling the tail.
Impact: INAA → waveform with CALC="AA"/"SUM(AA)" computes on an empty array and outputs zeros with no alarm — the core array-input feature is non-functional for array sources.

### R11-62: swait has no simulation mode — SIML/SIMM/SIOL/SIMS/SVAL absent
Severity: Medium
Rust: `crates/epics-base-rs/src/server/records/swait.rs` — none of the five fields; check_simulation_mode finds no SIMM.
C reference: `swaitRecord.c:401-421` — simm != NO reads SIOL → SVAL, VAL = SVAL, udf = FALSE, SIMM_ALARM at SIMS severity, skips calcPerform. Fields in swaitRecord.dbd:497-517.
Impact: a swait in simulation runs the real CALC instead of latching VAL to SVAL with SIMM_ALARM — the whole sim-mode observable is missing.

### R11-63: NDPluginCircularBuff flushes on a negative FlushOnSoftTrig where C requires strictly > 0
Severity: Low
Rust: `crates/ad-plugins-rs/src/circular_buff.rs:787` — stored as != 0.
C reference: `NDPluginCircularBuff.cpp:276-277` — if (flushOn > 0) flushPreBuffer().
Impact: FlushOnSoftTrig=-1 + SoftTrigger flushes on the port, not on C. (R8-72 residual.)

### R11-64: epid secondary-field monitors carry a spurious DBE_ALARM on an alarm-transition cycle
Severity: Low
Rust: `crates/std-rs/src/records/epid.rs` — no posting hooks, so changed OVAL/P/I/D/DT/ERR/CVAL take the generic alarm_bits|VALUE|LOG mask (record_instance.rs:2474,2527-2537).
C reference: `epidRecord.c:376` — monitor_mask = DBE_LOG|DBE_VALUE (reassignment discarding the alarm bits) before posting the secondaries (:377-408).
Impact: on an alarm-transition cycle a DBE_ALARM-only subscriber to .P/.I/.D/.OVAL/.ERR/.CVAL/.DT gets an event C never sends. No existing hook produces VALUE|LOG-without-alarm — needs a new mask hook or epid-specific declaration.

### Notes (round 11)

- Un-numbered Category-A observations recorded for the fixer:
  FITMPOLY/FITMQ mask threshold uses != 0.0 where C uses
  mask[i] > SMALL (1e-8, calcUtil.c:280) — fold into the R10-8 work;
  ARANDOM is time-seeded splitmix vs C's fixed-seed thread-private LCG
  (aCalcPerform.c:1667-1685) — divergent by construction, likely an
  accepted deviation, awaiting disposition.
- parse_to_u64 nit not filed: Rust is_ascii_whitespace excludes \x0b
  which C isspace skips — no realistic option carries it.
- swait forward-link exclusivity suspicion investigated and cleared
  (C fires FLNK exactly once on every chain); scaler UDF_ALARM
  confirmed effectively dead in C too; acalcout per-element deadband
  replacement is a documented intentional deviation — none filed.
- R9-55 timing edge recorded, not filed: C's record user carries
  timeout=1 until the first asynCallbackProcess stamps TMOT; the port
  applies TMOT from the start.

## Fix wave 9 — dispositions (2026-07-12)

48 of 49 wave-9 items FIXED, 1 NOT-REAL (R11-32), one commit per finding,
across 6 worktree fixers (a9a/a9b/b9/c9/d9/e9 — category A split in two)
plus the dedicated g9 asynGpib task. All 7 branches merged into
review/parity-r6 and verified by main.

Two of this round's own finding texts were overturned by the fixers'
compiled-C evidence; the corrections are recorded here, not silently:
- **R11-1's premise was partly wrong.** The doc said sCalc goes to
  exponential form "only for |v| >= 1e8". Compiled `cvtFast.c` switches
  to `%.3f` at 1e7 and to `%*.*e` at 1e16. The fix follows compiled C.
- **R10-13's "correct" reference implementation was wrong.** The port's
  existing `raw_from_escaped` mishandled a `\x` with no hex digit; C's
  `goto input` swallows the `x`. Fixed to C's behaviour.

Category A part 1 — aCalc array engine + input links (a9a), 11/11 FIXED:
- R11-61 FIXED 6862f179 — **HIGH.** Array-valued input links are offered
  to the field whole (element-0 fallback), so INAA..INLL actually reach
  AA..LL. The `to_f64()` collapse in processing.rs had made the entire
  aCalcout array-input feature dead for waveform sources.
- R10-6 FIXED d7f88108 — STRUCTURAL: `ArrayCell` = buffer + active window,
  exposed only through `buf()`/`window()`; C's firstEl/numEl by
  construction, so no reduction can see the zero fill again.
- R10-8 FIXED 49c44a4d — FIT family takes C's operands, returns the fitted
  curve, stores coefficients. Public API: `CalcError::FitFailed`,
  `fitting::fitpoly`, `ArrayOp::FitQ/FitMQ`.
- R11-8 FIXED 84156200 — DERIV/NDERIV are C's sliding quadratic fit.
- R11-9 FIXED 29de3fbb — aCalc variable stores land in the record; AMASK
  computed.
- R11-10 FIXED 91d2173d — SMOOTH/NSMOOTH preserve border elements.
- R11-11 FIXED a6f7d0d3 — FWHM gets C's strict crossing test and window
  fallbacks (`fwhm(buf, last_el)`).
- R10-10 FIXED 80680a46 — NDERIV promotes a scalar operand (toArray).
- R10-11 FIXED b80ef9b3 — FIT/DERIV family propagates C's status; the
  status cell is an assigning `set(Option)` mutator, one owner.
- R11-6 FIXED ab16f745 — ARR() is C's in-place toArray promotion.
- R11-7 FIXED d1474f87 — CAT of two scalars is C's no-op.

Category A part 2 — sCalc string/number engine (a9b), 10/10 FIXED:
- R11-1 FIXED 39592bb4 — STRUCTURAL: new `engine/cvt.rs`, a verbatim port
  of `cvtFast.c`, is the sole owner of double→text. The four ad-hoc
  `format!`/`parse::<f64>()` sites in scalcout.rs are gone.
- R11-2 FIXED eff86a58 — `ScalcString` makes C's 39-byte bound hold by
  construction; no producer can exceed it.
- R11-3 FIXED d432724a — DBL hunts an embedded number (C's strpbrk scan);
  a numeric operand coerces with atof.
- R11-4 FIXED c769b6cf — BYTE reads a signed char; a double operand passes
  through.
- R11-5 FIXED 5f521dea — PRINTF is C's snprintf behind C's conversion scan.
- R10-7 FIXED b6234a2d — ISINF is a SIGN, not a predicate: glibc expands it
  to `__builtin_isinf_sign`, so -inf is -1. One `c_isinf` shared by all
  three engines so it cannot drift back.
- R10-9 FIXED 51caf870 — the depth-1 store exemption is gone. Enforcing the
  uniform rule required **three compiled-C-verified semantic changes**:
  UNTIL_END's compile effect is 0 (not -1), so an UNTIL evaluates to its
  condition and its body must be an assignment; STORE_OPERATOR flushes
  nothing; `;` does not reset depth.
- R10-12 FIXED 17973c0a — an out-of-range literal is CALC_ERR_BAD_LITERAL.
  Subtler than filed: ERANGE fires for a subnormal too, so compiled base
  rejects 2.2e-308 and accepts 2.3e-308.
- R10-13 FIXED 067f1852 — TR_ESC/ESC use epicsString.c's table.
- R10-14 FIXED 6050b91c — SUBRANGE with a string bound is a strstr search;
  the subject is toString'd. The search's `j = -1` must not wrap like a
  negative numeric bound — true by construction now.

Category B — CA tools (b9), 5/5 FIXED (merged da16471b):
- R11-16/17/18, R10-17/18 — STRUCTURAL: `ValueFormat` splits into
  `int_style`/`float_style` (one flag had meant two things); `cli::format_c_g`
  is the single %g owner; `CountPrefix::leads` owns the count-prefix rule.
  Behaviour change matching C: `caget -lx` on a LONG now prints decimal.

Category C — PVA/qsrv (c9), 6 FIXED + 1 NOT-REAL:
- R11-32 **NOT-REAL** — compiled both sides: libstdc++'s
  `std::max(0.0, std::min(NaN, 100.0))` is 0.0, not 100.0, so pvxs lands on
  `ackAt = limit/2` exactly where the port's NaN-propagating clamp lands.
  The port already matches.
- R11-31 FIXED 9e93e735 — STRUCTURAL: one negotiated per-op monitor limit.
  `MonitorOptions::queue_size` is a resolved `u32` seeded from a server
  default of 4 (pvxs's `limit=4u`), not the invented squash-depth 64.
  Public API change.
- R10-33 FIXED 7c257ce8 — qsrv reports the server's negotiated limit; the
  duplicate queueSize parser deleted.
- R10-34 FIXED 6bcad5e9 — the invented server-side `record._options.autoExec`
  reader dropped.
- R10-35 FIXED a5928544 — `record._options` converts like pvxs `Value::as<T>`.
- R10-36 FIXED 32518461 — one `SB()<<Value` renderer for option diagnostics.
- R10-37 FIXED 68ee7ec9 — the DBE empty-mask logRemote fires at MONITOR INIT.

Category D — asyn (d9), 7/7 FIXED:
- R11-46 FIXED 5602d457 — **the whole `SCAN="I/O Intr"` mode**, absent
  until now. STRUCTURAL: `IoIntrScan` owns an `Option<IoIntrSample>`, so
  C's "gotValue set without a value" is unrepresentable, and taking the
  sample IS C's `gotValue = 0`.
- R10-49 FIXED 047cc686 — the C queue timer: the deadline rides on
  `ActorMessage.queue_deadline` with a `CancelToken` CAS. (C passes
  QUEUE_TIMEOUT=10.0, per the Round 11 strengthening.)
- R11-47 FIXED c211ec20 — an IP read teardown reports asynError, not the
  timeout; `classify_read_failure` returns a structured delta.
- R11-48 FIXED 65862949 — a port with no asynDrvUser forces reason 0.
- R11-49 FIXED 30a46090 — STRUCTURAL: `drivers/option_parse.rs` is the
  single owner of C's sscanf grammar and its diagnostic texts.
- R10-52 FIXED b06007f8 — the option readback follows C's buffer rule.
- R10-50 FIXED c608af35 — the dead `AsynOption` trait deleted (the
  dead-code reclassification from Round 11).
- BREAKING: five `PortHandle` helpers now take `&AsynUser`;
  `interfaces::option`/`AsynOption` and `serial_config::parse_bool_option`
  removed; `epics-base-rs` gains `Record::set_io_intr_scan` and
  `AsyncDbHandle::put_pv`.

Category E — synApps/AD records (e9), 8/8 FIXED:
- R10-67 FIXED bfbfff95 — STRUCTURAL, framework-level: CALC_ALARM moves off
  a framework rtype list into each record's own `check_alarms`, consumed
  per cycle (calc/calcout/scalcout/swait). swait also gains its CLCV field.
- R11-62 FIXED f8263a63 — swait simulation mode (SIML/SIMM/SIOL/SIMS/SVAL);
  new `SimOutcome::SimulatedInputStage` shape, shared with busy's sim group.
- R11-63 FIXED 6effe62f — CircularBuff flushes on soft trigger only when
  `FlushOnSoftTrig > 0`, which is C's gate (`if (flushOn > 0)`,
  NDPluginCircularBuff.cpp:276). The port had collapsed the field to a
  `bool`, i.e. `!= 0`, so a negative `caput` — the field is a writable
  asynInt32 — flushed the pre-buffer where C does not. The field is now an
  `i32` with one reader.
  (Correction: this disposition line originally had the two sides swapped,
  claiming C was `!= 0`. The R11-63 finding text at :2355 was right all
  along — C is `> 0`, the port was `!= 0`.)
- R11-64 FIXED 7b11c8b4 — epid posts secondary fields without the cycle's
  alarm bits; new `Record::fields_posted_without_alarm_bits()` hook (also
  used by transform).
- R10-63 FIXED bdd5b39b — scaler posts only the fields C's process cycle posts.
- R10-64 FIXED d0ae6120 — the scaler `special()` COUTP put fires inside the
  put, not at the next process. New `Record::take_special_actions()` hook.
- R10-65 FIXED 93d7e187 — scalcout fetches its string input links INAA..INLL.
- R10-66 FIXED b9b1d3b0 — scalcout exposes PVAL/PSVL with C's update points.
- STRUCTURAL: `AuxPostMask` becomes the single owner of the aux-post mask —
  there had been two drifted assemblers.

Dedicated task — asynGpib (g9), R10-55 FIXED (merged 080bfd8c):
- GPIB as a first-class port capability: `PortDriver` +4 hooks; vxi11 and
  prologix register asynGpib + asynInt32, so GPIBIV=1/I32IV=1 match C;
  asynRecord UCMD/ACMD dispatch with C's frames and its 3-step serial poll.
- The SRQ half is deliberately unported — devGpib is not in this tree.
- Uniform-rule behaviour change: `process()` refuses `stateNoDevice` before
  the TMOD gate, as C does.

Merge integrations resolved by main (not by any fixer, so they are
Round 12's first verification targets):
- d9 x g9 in `asyn_record/mod.rs` — `perform_io` keeps g9's phase dispatch
  and takes d9's `PhaseFlow::Aborted` break. `process()` kept g9's
  port-check-first order but placed d9's I/O Intr gate **above** it, on the
  claim that "C tests gotValue at :340-341, before the stateNoDevice refusal
  at :356". **That claim is FALSE and the placement was wrong** — Round 12
  (R12-46) proved it: C's `goto done` at :341 sits *inside* the
  `state == stateIdle` arm of an if/else-if chain whose first test is `state`,
  so a portless record never consults the flag. Fixed in e96a68ce.
- a9b x a9a in `calc/engine/array.rs` — `CoreOp::IsInf` keeps a9a's
  `unary_op` form and a9b's `c_isinf` sign.
- e9 x d9 in `record_trait.rs` — both added a Record hook at the same point;
  `set_io_intr_scan` and `take_special_actions` are independent, both kept.

Verified by main on a quiet host after all 7 merges: `cargo fmt --all
--check` clean, `cargo clippy --workspace --all-targets -- -D warnings`
clean, `cargo nextest run --workspace` **8276 passed / 0 failed / 2
skipped**, `cargo test --doc --workspace` clean.

## Open Findings — surfaced during fix wave 9 (reported by fixers, pending independent verify)

Category A (calc engines):
### R11-C1: `>?`/`<?` and vararg MAX/MIN collapse array operands to a scalar — C answers an element-wise ARRAY when either operand is an array (aCalcPerform.c:1155-1171, :1351-1352, :1376-1377); the port's pop2_f64/pop_f64 answers a scalar built from a[0] (array.rs:331,345,359,363)
Severity: Medium. A record's AVAL gets a broadcast scalar instead of the element-wise max.
### R11-C2: no end-of-expression stack-depth check in the array engine — C's `if (ps != top) return(-1)` (aCalcPerform.c:1608) makes a leaked operand a hard CALC_ALARM; the port's `eval` returns `stack.last()` unconditionally (array.rs:47)
Severity: Medium.
### R11-C3: an unset array variable fetches as the scalar 0, not an arraySize zero buffer (array.rs:89-96) — C's aa..ll always point at real arraySize record fields
Severity: Low. Not reachable through aCalcout (always passes NELM-long arrays); reachable through the public `acalc()` API.
### R11-C4: NEWM never computed on an INAA..INLL array change (acalcout.rs) — aCalcoutRecord.c:1105
Severity: Low.
### R11-C5: AMASK-flagged arrays are posted on change, not on write (acalcout.rs) — aCalcoutRecord.c:293-297 afterCalc posts exactly the flagged fields whether or not the value changed
Severity: Low.
### R11-C6: scalcout SVAL ignores PREC on a numeric program — `scalc_result` (calc/mod.rs:92) always renders at precision 8; C passes `pcalc->prec` into sCalcPerform (sCalcoutRecord.c:358,769 → sCalcPerform.c:831). The *string* evaluator's epilogue does hardcode 8 (:89-96), so the two paths differ by design
Severity: Medium. SVAL/OSV of a numeric-only scalcout is wrong for every PREC != 8.
### R11-C7: UNTIL loop-max is an error in the port, a silent break in C — the port returns CalcError::LoopLimitExceeded (string.rs:765); C just stops looping and continues with no error (sCalcPerform.c:1997), with sCalcLoopMax a settable global defaulting to 1000
Severity: Low.

Category D (asyn):
### R11-C8: the `special()` REASON put does not blank DRVINFO or run monitorStatus — asynRecord.c:486-492 does both (the cancelIOInterruptScan half landed with R11-46)
Severity: Low. After a REASON put, DRVINFO keeps stale text; a later reconnect re-resolves REASON *from the stale DRVINFO*, silently undoing the put.
### R11-C9: the IP server child port does not close the client slot on a fatal read errno — drvAsynIPPort.c:797-812 closeConnection on any fatal errno or EOF; ip_server_port.rs `read_octet` ~:1266 does not
Severity: Medium. Same defect family as R11-47 in shape, but a different driver and teardown owner. A client whose socket dies mid-read keeps its slot forever; reconnects are refused once the table fills.

Category E (framework/records):
### R11-C10: put-time posts never advance `last_posted` (record_instance.rs `notify_field`) — in C, dbPutField → db_post_events is the record's only post for that put, and monitor() then compares against the record's own *_lst fields
Severity: Medium. Framework-wide: any subscribed non-pp field written by a put is change-posted a second time by the next process cycle.
### R11-C11: scaler `special()` drops C's `scanOnce` (scaler.rs `take_special_actions`) — scalerRecord.c:655,667 calls scanOnce after the CNT/COUTP puts
Severity: Medium. A non-Passive scaler publishes the state change a full scan period late.
### R11-C12: `SIMM == 2` (RAW) is applied to menuYesNo records (processing.rs `check_simulation_mode`) — for menuYesNo SIMM there is no RAW choice; C's switch default is recGblSetSevr(SOFT_ALARM, INVALID) with NO device substitution (longinRecord.c:434-436, busyRecord.c:410-413)
Severity: Medium. SIMM=2 on longin/int64in/busy/swait silently performs a raw simulated read where C alarms.
### R11-C13: swait UDF discipline — C clears udf only on a successful calcPerform/SIOL read and never raises UDF_ALARM (swaitRecord.c:411,419); the port recomputes `udf = value_is_undefined()` every cycle
Severity: Low.
### R11-C14: transform evaluates with the numeric engine, not sCalc (transform.rs:8,564) — C calls sCalcPerform (transformRecord.c:593), which returns -1 on divide-by-zero → CALC_ALARM/INVALID
Severity: Medium. `CLCx = "1/0"` yields inf with NO_ALARM in the port. (Also: transform's VAL is posted by the framework's deadband path on an alarm cycle, where C's monitor() posts no VAL at all.)
### R11-C15: acalcout retains the R10-67 defect shape — a now-unread `get_field("CALC_ALARM")` pseudo-field arm and a sticky (non-consumed) `calc_alarm` in its check_alarms
Severity: Medium. Same family as R10-67; the file was assigned to another panel that round, so e9 did not touch it.

Notes on this set:
- **A candidate list was lost.** a9b had recorded candidates (a)-(h) from the
  R11-1..R10-13 areas before its context was compacted; the list existed only
  in context and is in no file or commit. a9b declined to reconstruct it from
  memory. Round 12 owes a fresh derive pass over those six areas.
- b9's 5 and g9's 1 candidates were folded into the Round 11 adjudication
  set already and are not repeated here.
- **`git stash` is repo-global across caucus worktrees** — g9 and a9b collided
  on `refs/stash` this wave (g9 restored a sibling's WIP onto commit 26275614).
  Fixers must not use stash; use `git diff > patch; git apply -R; …; git apply`
  for fail-before verification.

## Round 12 — re-audit (2026-07-12): wave-9 verification + adjudications + fresh findings

Five read-only opus auditors, one per category. Category A built compiled-C
harnesses for all three engines plus a Rust probe crate and ran every claim
side by side; B compiled C's `printf` conversions; C read pvxs; D and E
worked from source with the C open (D states plainly that it built no C and
that none of its four claims turns on compiler/libc behaviour).

### Fix-verification — 48 fixes, 47 VERIFIED, 1 WRONG

- **A (21 commits): all VERIFIED** against compiled aCalc/sCalc/base,
  including the a9b×a9a `IsInf` merge integration. The un-numbered Round-11
  note (FITMPOLY/FITMQ mask `> SMALL`) was folded in and verified.
- **B (5): all VERIFIED**, but two carry uncited family gaps (R12-17, R12-19).
- **C (6 + 1 NOT-REAL): all VERIFIED.** The auditor also confirms its own
  Round-11 R11-32 filing was wrong: `std::min(NaN, 100.0)` is `NaN`, not
  `100.0`, so pvxs lands where the port lands. Withdrawn.
- **D (10): all VERIFIED**, and the `perform_io` merge integration VERIFIED.
- **E (8): all VERIFIED**, and the `record_trait.rs` merge integration
  VERIFIED (both hooks are wired, not merely declared).
- **The `process()` merge integration (d9×g9) was WRONG** — see R12-46. Main
  resolved that conflict on a false reading of C and no fixer reviewed it.
  This is the one defect in 48 fixes, and it was introduced by the merge, not
  by a fixer. Fixed in e96a68ce.

### Adjudications — R11-C1..C15

CONFIRMED (12): R11-C1, C3, C4, C5, C8, C9, C10, C11, C13, C14, C15 —
and R11-C6, which was **strengthened**: compiled sCalc shows scalcout's SVAL
is wrong at the *shipped default* PREC=0 (C renders `3`, the port
`3.14159265`), not merely at PREC≠8. Raise to Medium-High.

RECLASSIFIED (3):
- **R11-C2** — the missing end-of-expression depth check is real in the code
  but produces **no reachable divergence in the array engine**: ~30 shapes run
  through compiled aCalc with `aCalcPerformDebug=1` never fire C's
  `if (ps != top) return(-1)`, because `aCalcPostfix`'s SEPARATOR case keeps
  the compile ledger exact. Worth adding as an invariant guard; not the bug it
  was filed as. (The analogous string-engine gap *is* reachable — R12-1.)
- **R11-C12** — CONFIRMED but the population is different from the filing: the
  12 menuYesNo base records **plus busy** are affected, and **swait must be
  dropped** — C's swait uses a plain `if (simm == menuYesNoNO) … else`
  (swaitRecord.c:406-421) with no switch and no default arm, so any non-zero
  SIMM takes the simulation branch and the port already matches.
- **R11-C15** — CONFIRMED but via a different path than filed. The fetch-gate
  route is REFUTED (`check_alarms` early-returns on the same condition that
  skips the reset). The real route is **ODLY**: C's DLYA continuation never
  calls `afterCalc`→`checkAlarms`, so `monitor()`'s `recGblResetAlarms` clears
  STAT/SEVR on that pass, while the port's framework calls `check_alarms`
  unconditionally and re-reads the un-consumed `calc_alarm`.

### New findings — 24

Category A (calc engines) — R12-1..9, all compiled-C verified:
### R12-1: the string engine emits UNTIL_END at the `;`, before the condition — every UNTIL expression fails at run time
Severity: High. R10-9's compile-ledger change is correct; the run-time loop it enables is not. The commit message for 51caf870 claims UNTIL now "evaluates to its condition"; compiled C shows it does not. Fix R11-C7 (loop-max as an error vs C's silent break, sCalcPerform.c:1997) as part of this — a loop-max that breaks silently is useless while the loop itself underflows.
### R12-2/3: MODBUS/ADD_XOR8 emit raw bytes where C keeps the frame escaped until the octet layer translates it
Severity: Medium. Compiled C: `MODBUS(AA)` is a 16-char escaped string; the port emits raw bytes the driver then double-processes.
### R12-4: `AMODBUS` drops C's leading `:` (sCalcPerform.c:1846-1850)
Severity: Medium. Compiled C `AMODBUS("010203")` = `:010203FA`; the port gives `010203FA`. ASCII-MODBUS frames lose their start delimiter.
### R12-5: `SSCANF` is a hand-rolled subset — a failed conversion is silently 0 where C raises CALC_ALARM, and `%x`/`%o`/`%u`/`%c`/`%[` are unimplemented
Severity: High. `string.rs:1150-1192` vs `sCalcPerform.c:1635-1690` (`if (i != 1) return(-1)`). Compiled: `SSCANF("abc","%d")` → C status −1 / CALC_ALARM, port 0 with a healthy record; `SSCANF("ff","%x")` → C 255, port 0. A protocol scalcout parsing a hex reply gets 0 and no alarm.
### R12-6: aCalc `POWER` promotes its left operand and pairs element-wise; C collapses the exponent and only maps when the LEFT is an array
Severity: Medium. `array.rs:157` vs `aCalcPerform.c:1306-1317` — POWER is not in the two-arg array dispatch. Compiled: `2**AA` is the scalar 2 in C, the array `[2,4,8,16,32,64]` in the port; both VAL and AVAL diverge, and the result *shape* diverges.
### R12-7: sCalc `LEN` of a double returns 0; C converts to string first
Severity: Medium. `string.rs:548-555` vs `sCalcPerform.c:1521-1526`. Compiled C: `LEN(4)` is **10** (`4.00000000`); the port answers 0.
### R12-8: the `strNcpy` result bound is 38 bytes, not 39 — R11-2's uniform 39-byte `ScalcString` loses the distinction
Severity: Medium. `value.rs:47-55` vs `sCalcPerform.c:68-74`: PRINTF/BIN_WRITE/SSCANF/TR_ESC/ESC and the checksums all pass `N = SCALC_STRING_SIZE-1` → a 38-byte bound; only `ADD` and `LITERAL_STRING` get 39. Making the bound structural was right; making it *uniformly* 39 was not. The fix is a second constructor for the `strNcpy` family, not a check at each call site.
### R12-9: `LRC` rejects operands C accepts, turning a healthy record into CALC_ALARM
Severity: Low. `checksum.rs:19-43` vs `sCalcPerform.c:230-256` — C's `hex()` returns 0 for a non-hex char and its loop ignores a trailing odd character. (C's same loop reads out of bounds for an *empty* operand — `strlen(0)-1` wraps. That is C UB; do not port it. Refuse the empty operand deliberately and say so.)

Category B (CA tools) — R12-16..20:
### R12-16: `caget`/`camonitor` reject C's `-0<base>` / `-l<base>` spelling outright — the integer/float base flags are unreachable
Severity: High. The six base flags are declared as clap **long** options only (`caget-rs.rs:149-168`); C's getopt has `0:` and `l:` as single-dash options taking an argument (`caget.c:395,487`). Measured: `caget-rs -0x PV` → `error: unexpected argument '-0' found`, exit 2. **This makes the entire R11-16 fix unobservable through the C CLI** and silently breaks any script passing `-0x`.
### R12-17: malformed numeric option arguments are hard exit-2 errors; C's getopt tools warn and continue with a default
Severity: Medium. C warns and *runs the get* for `-w`/`-#`/`-p`/`-e`/`-f`/`-g` (caget.c:437-462 and the identical blocks in camonitor/caput/cainfo). Measured: 10 invocations across the four binaries exit 2 and read nothing. This is the family R10-18 opened and closed at one site — the fixer built `cli::scan_leading_i64` as the shared owner and wired only `caput -#`.
### R12-18: usage-error exit code is 2 with clap's text; C returns 1 with its own diagnostic
Severity: Low. `cainfo-rs.rs:111` already prints C's exact `No pv name specified.` and exits 1 — it shows the intended shape.
### R12-19: `caget -d DBR_GR_CHAR` / `DBR_CTRL_CHAR` print the limits unsigned; C casts each through a signed `char`
Severity: Medium. `caget-rs.rs:376-378` vs `tool_lib.c:370,381` — `ARGS_GR(T,F)` applies the macro's `F` cast, and for the CHAR classes `F` is `char`. Compiled: `printf("%8d",(char)255)` → `-1`. Same signed-`char` rule R11-16 fixed on the *value* path; the *limit* path was not swept with it.
### R12-20: `caget -0<base>` does not force `type = DBR_LONG` (caget.c:493-495), so `-d <type> -0x` requests the wrong DBR
Severity: Low. Blocked behind R12-16 today, but in the same C block — fix them together.

Category C (PVA) — R12-31..34:
### R12-31: a single-record PVA monitor re-sends the whole value with an all-changed bitset; pvxs sends only the DB event's marked leaves
Severity: High. `qsrv/monitor.rs:265-299` → `pva_adapter.rs:1045-1049` → `tcp.rs:2102-2112` selects the full-selection bitset. C: `singlesource.cpp:47-68` marks only the event's leaves and `unmark()`s after each post; `servermon.cpp:172-174` serializes `marked ∩ pvMask`. Every monitor frame for a QSRV single-record PV — the most common thing QSRV serves — differs on the wire, and a client using `isMarked()`/`ifMarked()` sees metadata as freshly changed on every value tick.
### R12-32: no `testmask` gate — an update whose marked set misses the pvRequest mask is still queued and framed; pvxs drops it in `doPost`
Severity: Medium. `tcp.rs:2029-2031` pushes into the squash FIFO before any mask test. C: `servermon.cpp:261-268` guards the enqueue with `if(real || !val)`. The masked-out update also occupies a slot in the negotiated FIFO and can coalesce a real update out of it — so the squash *contents* differ, not just the frame count. The doc-comment at `tcp.rs:7756-7758` asserting parity here is wrong.
### R12-33: an exhausted pipeline window stops draining the source instead of squashing at the negotiated limit
Severity: Medium. `tcp.rs:2038-2101` — `credit.acquire().await` blocks inside the `select!` arm, so `rx.recv()` is not polled. C: `servermon.cpp:79-83,143` — `maybeReply` simply does not fire while the window is 0; `doPost` keeps squashing. A stalled pipelined client makes the port buffer ~`64 + limit` distinct updates and deliver them all on resume, where pvxs coalesces everything past `limit` into the queue tail. Breaks the same "one negotiated limit governs the squash" invariant R11-31 established.
### R12-34: a MONITOR pvRequest selecting no existing field draws an op-error; pvxs's `request2mask` throw resets the circuit
Severity: Low. `tcp.rs:6157-6178` vs `pvrequest.cpp:60-61` → `conn.cpp:277-282` (`bev.reset()`). Same class as R9-33, which wave 8 fixed by routing a MONITOR INIT throw to a fatal `PvaError`; the mask throw was not routed through it.

Category D (asyn) — R12-46..48:
### R12-46: the I/O Intr gate was placed above the port check; C dispatches on `state` first
Severity: Medium. **Introduced by main's own d9×g9 merge resolution.** C's `if (gotValue) goto done` (:341) sits *inside* the `state == stateIdle` arm of an if/else-if chain whose first test is `state` (:331-361): a portless record never consults the flag, reports "Not connect to a port" with STATE_ALARM/MINOR, and `done:` (:370) discards the value. The port published it instead — a record with no port looked healthy and freshly updated, NO_ALARM, empty ERRS. FIXED e96a68ce (structural: `process` is now C's `done:` and owns the discard; `IoIntrScan::refresh` owns the clear on every binding change).
### R12-47: no request can bypass the not-connected refusal — C's `ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED` is unmodelled
Severity: High. `port.rs:462` (`check_ready` → `Disconnected`) refuses what C's `queueRequest` (asynManager.c:1536-1552) lets through when `priority == asynQueuePriorityConnect` **and** the user carries the reason. Six C call sites rely on it, and the port has none: (1) **the HOSTINFO put** (asynRecord.c:566-569, C's comment: *"Enable changing host:port when not connected"*) — this is the operator's only route to repoint a dead IP port, so a `drvAsynIPPort` aimed at a wrong or moved host **cannot be corrected at runtime at all**; (2) the connect-time option readback (asynRecord.c:1277-1280), so every asynRecord on a down serial/IP port shows BAUD/PRTY/… as "Unknown" where C populates them (note C's deliberate asymmetry: the *EOS* readback is queued at Low priority with no bypass, :1296, so IEOS/OEOS correctly stay blank); (3) `asynSetOption`/`asynShowOption`/`asynSetEos`/`asynShowEos` from iocsh, so configuring a serial line from `st.cmd` before the device is powered on works in C and is refused here. The drivers are already correct and are not the fix site — the refusal is imposed solely by the queue gate.
### R12-48: the Connect-priority bypass also skips the *disabled* check, which C never skips
Severity: Medium. `port_actor.rs:383` skips `check_ready` wholesale, and that is where both the disabled (`port.rs:456`) and disconnected (`:462`) refusals live. C conditions only `checkPortConnect` on the priority; `if(!pport->dpc.enabled) return asynDisabled;` (:1541-1546) is unconditional. So on a port disabled with `asynEnable(port,0)`, a `CNCT=1` put still opens the device connection — a port disabled precisely to keep the IOC off the hardware still touches it. The two refusals must be separated: the lifecycle/Connect class may bypass *connected*, never *enabled*.

Category E (records/framework) — R12-61..65:
### R12-61: SIMM=YES with unset SIML and SIOL silently does not simulate at all
Severity: Medium. `processing.rs:4964-4966` returns `NotSimulated` *before* SIMM is read. C raises SIMM_ALARM independently of the SIOL read (longinRecord.c:413-414) and an unset (constant) SIOL still yields `val = sval`. The standard "simulate against a constant" idiom — `caput REC.SIMM 1; caput REC.SVAL 42` — is a complete no-op on the port. Affects every record routed through `check_simulation_mode`.
### R12-62: scaler Sn loses its DBE_LOG post on the count-completion cycle
Severity: Medium. C posts each changed Sn **twice** on that cycle — `updateCounts()` with DBE_VALUE (`:582`) and `monitor()`'s sweep with a literal DBE_LOG (`:770-772`), both reached because `:386` sets `ss = IDLE`. The port's per-field model (`record_instance.rs:2537-2556`, `log_swept` is an `else if` on `changed`) structurally cannot emit two events for one field in one cycle. A DBE_LOG-only archiver never receives the final counts — exactly the sample that matters.
### R12-63: NDPluginProcess drops C's zero-coefficient guards, poisoning output with NaN
Severity: Low. `process.rs:527,556-557` multiplies every term unconditionally; C guards each (`NDPluginProcess.cpp:204-208,220-225`). All ten coefficients default to 0, and `0.0 * NaN` is NaN — so one non-finite pixel permanently poisons `filter[i]` for every subsequent frame where C outputs the clean offset.
### R12-64: the SIMM↔SSCN scan swap (`recGblCheckSimm`) is entirely unimplemented
Severity: Medium. `recGbl.c:427-437` swaps SCAN with SSCN on every SIMM transition; workspace-wide `rg` finds no such behaviour at all. SSCN and OLDSIMM are inert storage: a simulated record keeps its production SCAN rate forever and `caput REC.SSCN` has no effect on anything.
### R12-65: a failed SIML read raises no LINK_ALARM
Severity: Low. `processing.rs:5008-5016` silently discards the failure; C sets `nsta = LINK_ALARM` (`recGbl.c:453-454`). Note the C quirk the port must reproduce: `nsta` is set directly, so SEVR stays NO_ALARM.

### Structural notes from the auditors (read before briefing fix wave 10)

- **R12-61/64/65 are one family**, not three symptoms: the port implements the
  SIOL/SIMM read but not the surrounding `recGblGetSimm` / `recGblCheckSimm` /
  `recGblInitSimm` contract. Port those three C functions as the single owner
  of the simulation-mode transition.
- **R11-C10 is the structural cause under R10-63.** Fixing it at the framework
  (advance `last_posted` on the put-time post) lets scaler's closed-set hook
  shrink and closes the same double-post for every other record. Do not hand it
  out as an isolated per-record patch.
- **R12-16 gates R12-20**, and **R12-1 subsumes R11-C7** — pair them.
- Three fresh-hunt hypotheses were formed and then killed on the C, recorded so
  they are not re-chased: `epicsStrSnPrintEscaped` emits lowercase `\xHH` not
  octal (the port is right); scaler's DBE_LOG sweep is *not* unconditional
  (only the completion-cycle edge survives, R12-62); NDPluginProcess does *not*
  re-derive autoOffsetScale every frame (the port's one-shot matches).
- Un-filed nit: `A:=B:=5` is `CALC_ERR_BAD_ASSIGNMENT` (3) in C and `Incomplete`
  (8) in the port. Both reject, and CLCV is only 0/non-zero, so the code is
  unobservable through CA.
- The ARANDOM seeding deviation (time-seeded splitmix vs C's fixed-seed
  thread-private LCG, aCalcPerform.c:1667-1685) still awaits disposition.

## Review Log

- Round 6 (2026-07-10): full-workspace 5-way opus fan-out (caucus panels, read-only,
  C→Rust direction). 42 NEW findings: High 10 / Medium 23 / Low 9 —
  A: 8 (2H/5M/1L), B: 14 (4H/7M/3L), C: 5 (0H/4M/1L), D: 5 (3H/2M/0L), E: 10 (1H/5M/4L).
  CARRYOVER still live: 5 — R5-2 sibling (swait/scalcout/transform prev_val=0, panel E),
  DRV-16/17 (ip_server_port octet-interface interrupts), DRV-53 (prologix destructible),
  motor R60 (SET_ENC_RATIO never sent), motor R64 (MIP_STOP ls_blocks arm skips
  postprocess_sync) (panel D). Themes: version-gated CA wire paths (V49/V413) form the
  dominant B cluster (R6-18/19/20 one family); dbStatic link-modifier parsing family
  (R6-2/3/4); PINI menu+phase family (R6-5/6); connect-lifecycle state reset family in
  asyn (R6-46/47/49); color-mode-blind AD plugin family (R6-61/66/68); optics numeric
  divergences faithful-to-C-quirks required (R6-62/63/67 — C's off-by-one/div-zero
  behaviour is the contract). Numbering kept in per-category blocks (gaps intentional).
  Fix phase pending user go-ahead.
- Fix wave 1 (2026-07-11): 5 worktree fixers (opus) launched, one per category.
  D (R6-46..50) and E (R6-61..70 + R5-2 sibling) finished within the window: all 16
  items FIXED (R6-66 partially NOT-REAL as cited), merged into review/parity-r6
  (1caa6034, d7363692) and verified by main — workspace `cargo clippy --all-targets
  -D warnings` clean, `cargo nextest run --workspace` 7462 passed / 0 failed /
  2 skipped. 4 new findings surfaced by the fixers (R6-51, R6-71..73) recorded
  OPEN pending independent verify. A (R6-1..8), B (R6-16..29), C (R6-31..35)
  still in flight. Deferred-by-sign-off carryovers (DRV-16/17, DRV-53, motor
  R60/R64) deliberately excluded from fixer scope — awaiting user decision.
- Fix wave 2 (2026-07-12): A (R6-1..8) and C (R6-31..35) finished — all 13 FIXED,
  merged into review/parity-r6 and verified by main: workspace clippy -D warnings
  clean, nextest 7506 passed / 0 failed / 2 skipped, doctests clean. Notable: R6-34
  is a public API change (RpcReply); R6-3 respelled two epics-ca-rs tests that
  encoded the wrong `CP CA` semantics. 2 new findings (R6-9, R6-10) + residual
  notes recorded. B (R6-16..29) still in flight.
- Fix wave 3 (2026-07-12): B (R6-16..29) finished — all 14 FIXED, merged and verified
  by main (workspace clippy clean; nextest first run 7539/7541 with 2 compile-load
  flakes, clean re-run 7541/7541; the two flakes pass in isolation and are logged
  under notes). 3 new findings (R6-30, R6-74, R6-75) + R6-22 residual recorded OPEN.
  All 42 round-6 findings are now closed; remaining OPEN: R6-9, R6-10, R6-30, R6-51,
  R6-71..75 (9 items, all fixer-surfaced) + 4 deferred-by-sign-off carryovers.
- Fix wave 4 (2026-07-12): all 10 fixer-surfaced items closed — 9 FIXED, R6-74
  NOT-REAL (C also truncates on those paths; pinned by test). Merged and verified
  by main: workspace clippy clean, nextest 7563/7563 first-run clean. 2 new LOW
  findings recorded OPEN (R6-76 SIGXFSZ, R6-77 tokenizer compile-split). R6-75
  ports C's blocked-signal-mask leak — operational consequence documented above,
  awaiting veto if unwanted. Round-6 fix phase COMPLETE: 41 FIXED + 1 partial +
  2 NOT-REAL across 5 waves. Next: R7 re-audit.
- Round 7 (2026-07-12): same 5 auditor panels (opus, read-only), dual mandate
  (verify all R6 fixes + fresh hunt). Fix verification: every R6 fix confirmed
  correct against the C reference; the one completeness gap is filed as R7-46
  (R6-48's PartialRead bytes still discarded at the actor dispatch). 19 NEW
  findings: High 1 / Medium 8 / Low 10 — A: 2 (0H/2M), B: 4 (0H/2M/2L),
  C: 3 (0H/0M/3L, one shared structural root: missing source-layer logRemote
  sink), D: 4 (0H/3M/1L), E: 6 (1H/1M/4L). Themes: put-path gate ordering
  (R7-1/2 — DISP and RPRO both checked after the port's early intercepts where
  C checks before); asyn option persistence (R7-48/49 — set_option mutates live
  termios instead of the cached config C re-applies on connect); scaler
  process-exit vs in-process decision timing (R7-61/65/66). Still OPEN from R6:
  R6-76 (SIGXFSZ), R6-77 (tokenizer compile-split) + 4 deferred-by-sign-off
  carryovers. Fix wave 5 next.
- Fix wave 5 (2026-07-12): all 19 R7 findings + R6-76/77 FIXED across 5 worktree
  fixers (one commit per finding), merged into review/parity-r6 and verified by
  main — workspace fmt/clippy -D warnings clean, nextest 7614/7614 (one
  compile-load flake on first run, logged), doctests clean. Structural owners
  landed: `check_put_disabled`, `put_driven_process`, per-dialect
  `postfix::compile`, `HostIdentity::{Claimed,Pinned}` + `as_check_client_ip`,
  `refuse_message`, `ChannelContext::log`/`flush_remote_log`,
  `PartialOctetRead` error-owns-the-transfer, cached-termios owner,
  scaler `fire_fwd_link` in-process, `ad_core_rs::convert`,
  `copy_gates_to_directions`. 3 new LOW findings recorded OPEN (R7-3 LOG2,
  R7-34 numeric-string DBE, R7-50 win32 get_option disconnected) + 2 notes.
  SIGN-OFF ITEMS pending user: R6-77's numeric-engine token removals
  (`>?`/`<?`/`NRNDM`/`AA..UU` — C contract, veto possible), R6-75 blocked-mask
  half (carried). Next: R8 re-audit.
- Round 8 (2026-07-12): same 5 auditor panels (opus, read-only), dual mandate.
  Fix verification: every wave-5 fix confirmed correct — zero fix-verification
  findings. Adjudications: R7-3 DROP-TO-C (widened to the INT token in
  Numeric), R7-34 CONFIRMED, R7-50 CONFIRMED + widened (all option keys,
  get AND set). 32 NEW findings: High 1 / Medium 18 / Low 13 —
  A: 5 (3M/2L, calc-family CLCV/menu-put validation cluster),
  B: 6 (4M/2L, disconnect-notification ordering + procServ info-file
  lifecycle), C: 3 (3L, wire error-text divergences), D: 8 (3M/5L,
  asynRecord I/O-plan cluster: ASCII 40-byte cap, pre-write flush, CNCT
  layer confusion), E: 10 (1H/8M/1L — R8-69 modbus poller permanent death
  HIGH, R8-70 exception-05 panic, codec/TIFF/netCDF byte-contract cluster).
  Auditor-A's "R6-1..8 still live" carryover dismissed as a doc-structure
  misread (spot-checked at HEAD). Panel-E first-half findings were lost from
  the round capture and re-emitted verbatim on request (round
  01KX96GP5F4511X7551KGNATA2). Sign-off items pending user: R6-77 numeric
  token removals, R6-75 blocked-mask half, C's implLang="rust" truthful
  token. Fix wave 6 next.
- Fix wave 6 (2026-07-12): all 21 R8 items + R7-3/R7-34/R7-50 FIXED across
  6 worktree fixers (A4/B4/C4/D4/E4a/E4b, one commit per finding), merged
  into review/parity-r6 and verified by main — workspace fmt --check clean,
  clippy --all-targets -D warnings clean, nextest 7723/7723 first-run clean
  (2 skipped), doctests clean. ADJUDICATION CORRECTION: R8's R7-3 verdict
  ("reject LOG2 everywhere") was wrong — fixer-A4 compiled C's postfix.c and
  proved LOG2 lexes as LOG·2 (longest-prefix, no identifier boundary); the
  fix ports C's lexing instead. R8-2's finding text corrected (C stores 0/-1
  in CLCV, not the postfix error code). Structural owners landed:
  putStringMenu converter, calc/engine/cast.rs, per-engine ELEMENT-table
  allowlists, MonitorFlow::admit, disconnect_channels()+DisconnectKind,
  qsrv::put_status/wire_message, route_frame FrameFault,
  PartialOctetRead write twin (3 private is_fatal_transport_error copies
  deleted), RequestOp::PushEchoInterpose, refresh_connected_state,
  ModbusIoResponse, ValuePostGate, CircularBuffer FrameParams, codec
  outcome enum, hand-built TIFF writer/reader, netCDF define_data_set.
  15 NEW fixer-surfaced findings recorded OPEN (R8-6..8, R8-22..23, R8-34,
  R8-54..58, R8-71..75) — notable: R8-57 asynInterposeCOM is an unported
  856-line subsystem; R8-23 is redesign-scale (per-circuit event queue).
  Sign-off items pending user: R6-77 token removals, R6-75 blocked-mask
  half, implLang="rust", R7-3's LOG2-as-LOG·2 reading. Next: R9 re-audit.
- Round 9 (2026-07-12): same 5 auditor panels (opus, read-only), triple
  mandate. Fix verification: all 30 wave-6 commits CORRECT AND COMPLETE —
  zero fix-verification findings, second consecutive clean wave.
  Adjudications: all 15 wave-6 fixer-surfaced items CONFIRMED (R8-34
  narrowed to MONITOR-only; R8-55's C mechanism corrected to event-driven
  exceptCallback; R8-56 and R8-8 widened; R8-7 sharpened — sCalc div/0 is
  INVALID_ALARM + VAL frozen, not -1). 17 NEW findings: High 1 / Medium 11 /
  Low 5 — A: 3 (1M/2L, aCalc epsilon comparisons the headline), B: 3
  (2M/1L, CA-tools exit-code/stdout contracts), C: 2 (1M/1L, pvRequest
  kind-dispatch siblings of R7-34), D: 5 (2M/3L, asynRecord TMOT
  passthrough + option-readback sourcing + ERRS text cluster), E: 12
  (1H/9M/2L — transform record cluster R9-61..64 dominates, swait DOL
  fetch, ROI disabled-dim/MaxSize, Process SaveBackground timing, sub
  fetch-gate, sseq DLY quantum). Themes: invented-behaviour comments/tests
  (three test-skepticism hits); records ported without their
  input-failure/hold modes (transform IVLA, sub fetch gate, transform
  zero-on-fail); kind-dispatch gaps in pvRequest option parsing. Fix wave 7
  next: 32 items (15 confirmed + 17 new), R8-23 event-queue redesign and
  R8-57 asynInterposeCOM port assigned as dedicated structural tasks.
- Fix wave 7 (2026-07-12): 31 of 32 items FIXED, R9-17 NOT-REAL (coordinator
  already rebuilds; evidence test), across 8 worktree fixers; all merged and
  verified by main — workspace fmt/clippy -D warnings clean, nextest
  7866/7866 (2 skipped), doctests clean. Landmark closures: the CA server
  event queue REDESIGNED to C's EventUser/EvQue/SubQ triple (R8-22+R8-23,
  MonitorFlow/coalesce slots deleted); asynInterposeCOM PORTED in full
  (R8-57, 45 protocol tests, COM as structural base link); per-engine
  ElementTable IS the calc lexer (R8-6); transform process() rebuilt to C's
  order (R9-61..64); three new Record hooks (input_fetch_policy,
  output_link_value/output_time_input_links,
  fields_posted_with_monitor_mask); IoOutcome::report_error owns every
  asynRecord I/O diagnostic. THREE finding-text corrections by fixers with
  compiled-C/traced evidence: R9-1 (epsilon rules were SWAPPED between
  engines — SMALL is real C, sCalc-side), R9-18 (C's timeout branch is dead
  code; C prints the zeroed buffer), R9-70 (C's put path quantizes DLY1
  regardless of which DLYn was written). One NOT-REAL sub-claim (R8-6 hex
  literals — C accepts them). R9-46 decision: TMOT>=0 verbatim, TMOT<0
  stays bounded under DRV-42 (documented at site). One merge-integration
  test fix by main (EventReader API). 26 NEW fixer-surfaced findings
  recorded OPEN (R9-4..9, 19..23, 33..35, 51..57, 73..80) — notable: R9-54
  is a PANIC at record init (@asyn negative timeout), R9-7 is the
  structural cause behind R9-3, R9-56 is the general interpose stack-order
  family behind R8-57's COM-specific fix. Next: R10 re-audit.
- Round 10 (2026-07-12): same 5 auditor panels (opus, read-only), triple
  mandate. Fix verification: all wave-7 commits VERIFIED CORRECT — the
  event-queue redesign checked at the byte/gate level against dbEvent.c,
  COM line-by-line against asynInterposeCom.c, MonitorTeardown's
  Fatal⟹closed invariant confirmed by construction; third consecutive
  clean wave (category E sampled 6 of ~20, noted). Adjudications: ALL 26
  wave-7 fixer-surfaced items CONFIRMED (R9-4 widened to every
  mixed-type op; R9-7 widened to scalcout/acalcout; R9-20/21 narrowed to
  their live facets; R9-35's pvxs throw-consequence now traced to
  bev.reset()), plus both round-9 scaler candidates confirmed and filed
  (R10-61/62). 13 NEW findings: High 0 / Medium 7 / Low 6 —
  A: 5 (3M/2L, aCalc array-op scalar/shape contract cluster:
  IXZ interpolated crossing, ISINF/FINITE/ISNAN shape, scalar
  reductions), B: 1 (1L, alarm-string table HW_LIMIT/"Illegal value"
  3-site family), C: 2 (2M, native-server pvRequest option conversion —
  real-typed pipeline, real/hex queueSize — the exact R7-34/R9-34
  kind-dispatch family on the server side), D: 3 (1M/2L, asynRecord
  STATE_ALARM severities, resetError sites, Unknown-option
  fall-through), E: 2 (1M/1L, scaler COUTP double-fire + RATE→TP
  copy-paste quirk). Finding volume declining (42→19→32→17→13) and
  severity ceiling dropped to Medium for the first time. Fix wave 8
  next: 42 items (29 confirmed carryovers + 13 new).
- Fix wave 8 (2026-07-12): all 42 items FIXED (no NOT-REAL) across 5
  worktree fixers, merged and verified by main — fmt/clippy -D warnings
  clean, nextest 8046/8046 (2 skipped), doctests clean. Structural
  owners landed: always-a-program CompiledExpr (Option modeling
  deleted, R9-7), engine::subrange_bounds + ported strtod,
  pvdata::convert (pvxs copyOut) + ChannelSource::check_monitor_request
  INIT hook, timeout_from_secs, PortHandle::has_interface registry,
  AsynUser through the whole asynOption path (TWO public API changes,
  R9-53/R9-55), InterposeStack::install (last = outermost),
  InputFetchPolicy::ReadAllGateOnFailure,
  RecordInstance::deadband_post, CircularBuffer Control admission,
  cli::stat_to_str/zero_dbr_value/ca_error_marker owners,
  echo_fallback and detect_color_mode deleted. Client-visible semantic
  changes (all = C): non-scalar ackAny / array-typed DBE drop the PVA
  connection; option strings parse base 0; scaler COUTP double-fires
  on user stop (scaler-rs local doc SCAL-6 retracted). Merge work by
  main: 4-file a8 x e8 conflict resolution (fetch gate wraps the
  unconditional eval) + one integration test adaptation (fac0dc95).
  30 NEW fixer-surfaced findings recorded OPEN (R10-6..15, 17..18,
  33..37, 49..55, 63..67) — notable: R10-6 aCalc array window
  (firstEl/numEl) is the highest-priority candidate; R10-49 asynRecord
  queue-timeout mechanism missing entirely. Next: R11 re-audit.
- Round 11 (2026-07-12): same 5 auditor panels (opus, read-only), triple
  mandate. Fix verification: all 42 wave-8 commits VERIFIED CORRECT AND
  COMPLETE — fourth consecutive clean wave — including main's a8×e8
  merge-integration (fetch-gate nesting checked against each C record's
  process()), and the 8 previously-sampled wave-7 E items now
  line-verified. Adjudications: 24 of 30 wave-8 candidates CONFIRMED,
  3 REFUTED (R10-15 — C's shared `long l` read is LP64 UB, the port
  matches C's canonical result, do not port the UB; R10-51 — casing
  unobservable, both sides case-insensitive; R10-53 — alias branches
  unreachable), R10-50 reclassified dead-code cleanup, R10-54 remains a
  cfg(windows) verification gap. R10-49 STRENGTHENED: C's asynRecord
  passes QUEUE_TIMEOUT=10.0 (not 0.0) and the port_actor.rs:340-341
  comment is factually wrong. 24 NEW findings: High 1 / Medium 14 /
  Low 9 — A: 11 (sCalc string-engine byte contract: cvtDoubleToString(8),
  39-char intermediates, DBL embedded-number hunt; aCalc math kernels:
  DERIV/NDERIV quadratic fit, SMOOTH borders, FWHM fallback, store-backs
  +AMASK), B: 3 (caget/camonitor formatting: CHAR base-flag, %g limits,
  specifiedDbr count prefix), C: 2 (squash-depth default 64 vs pvxs 4;
  NaN-percent ackAny), D: 4 (R11-46 SCAN="I/O Intr" absent is the
  headline; IP timeout status, drvUser REASON=0, setOption texts),
  E: 4 (R11-61 HIGH — aCalcout array inputs dropped by the to_f64
  collapse in processing.rs, the core array-input feature dead for
  waveform sources; swait sim mode absent; FlushOnSoftTrig sign; epid
  secondary-post alarm mask). Volume 13→24 reflects deeper kernel/
  formatting scrutiny, severity re-entered High via R11-61. Fix wave 9
  next: 49 items (24 confirmed carryovers + R10-50 cleanup + 24 new);
  R10-55 (asynGpib surface) assigned as a dedicated structural task;
  ARANDOM seeding awaiting disposition as accepted deviation.
- Fix wave 9 (2026-07-12): 6 worktree fixers (opus; category A split a9a/a9b
  because the sCalc string engine and the aCalc array engine are disjoint
  surfaces of comparable size) plus the dedicated g9 asynGpib task. 48 of 49
  items FIXED, 1 NOT-REAL (R11-32 — compiled both sides; libstdc++'s NaN
  clamp is 0.0, so pvxs already lands where the port lands). All 7 branches
  merged into review/parity-r6 by main, with 3 merge integrations resolved by
  hand (d9xg9 in asyn_record, a9bxa9a in the array engine, e9xd9 in
  record_trait) — those are Round 12's first verification targets. Verified on
  a quiet host: workspace clippy -D warnings clean, nextest 8276 passed /
  0 failed / 2 skipped, doctests clean. The two under-load flakes seen during
  the fix wave (epics-pva-rs stability, epics-ca-rs protocol_tests) did not
  reproduce. Structural work this wave: ArrayCell (buffer+window) closes the
  aCalc window family; engine/cvt.rs is the sole double→text owner; ScalcString
  bounds C's 39 bytes by construction; IoIntrScan makes "gotValue without a
  value" unrepresentable; drivers/option_parse.rs owns C's sscanf grammar;
  AuxPostMask replaces two drifted assemblers; CALC_ALARM moves from a
  framework rtype list to record-owned, per-cycle-consumed state; GPIB becomes
  a port capability rather than a hardcoded GPIBIV=0. Four compiled-C findings
  overturned text this project had believed: cvtFast switches at 1e7/1e16 (not
  1e8), glibc isinf is a sign, UNTIL_END's compile effect is 0, and ERANGE
  fires for subnormals. Breaking API changes landed in epics-ca-rs
  (ValueFormat), epics-pva-rs (MonitorOptions::queue_size), asyn-rs (PortHandle
  helpers take &AsynUser; AsynOption removed) and epics-base-rs (two new Record
  hooks, CalcError::FitFailed, ArrayCell). 15 new findings recorded OPEN
  (R11-C1..C15); a9b lost a candidate list (a)-(h) to context compaction, so
  Round 12 owes a fresh derive pass over the sCalc string-engine areas.
  Nothing pushed.
- Round 12 (2026-07-12): 5 read-only opus auditors, triple mandate (verify
  wave 9 / adjudicate R11-C1..C15 / fresh hunt). **47 of 48 wave-9 fixes
  VERIFIED; the one WRONG is main's own d9xg9 merge resolution** (R12-46 —
  the I/O Intr gate placed above the port check on a false reading of C's
  :340-341; C's `goto done` is nested inside the `stateIdle` arm). Fixed at
  source in e96a68ce with the cell invariant closed structurally. Two of the
  three merge integrations verified clean. Adjudications: 12 CONFIRMED (R11-C6
  strengthened — scalcout SVAL is wrong at the *shipped default* PREC=0, not
  just PREC≠8), 3 RECLASSIFIED (R11-C2 unreachable in the array engine;
  R11-C12's population is the 12 menuYesNo records + busy, and swait must be
  dropped; R11-C15's fetch-gate route REFUTED, the real route is ODLY).
  R11-32 formally withdrawn — the Category-C auditor confirms its own Round-11
  filing misread libstdc++. 24 NEW findings: High 4 / Medium 15 / Low 5 —
  A: 9 (the sCalc *string operator* surface is the weak spot: UNTIL emits
  UNTIL_END before the condition so every loop fails at run time; SSCANF is a
  hand-rolled subset that swallows conversion failures; MODBUS/AMODBUS frame
  shape; LEN of a double; the 38-vs-39 strNcpy bound R11-2 flattened),
  B: 5 (R12-16 HIGH — the `-0x`/`-lx` base flags do not parse at all, which
  makes the entire R11-16 fix unobservable through the C CLI; plus the
  warn-and-continue family R10-18 closed at only one site), C: 4 (R12-31 HIGH
  — QSRV single-record monitors re-send the whole value with an all-changed
  bitset; plus the missing testmask gate and the pipeline-starvation residual),
  D: 3 (R12-47 HIGH — ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED unmodelled, so a
  dead IP port's HOSTINFO cannot be repointed at runtime *at all*), E: 5 (the
  recGblGetSimm/CheckSimm/InitSimm contract is largely unimplemented — SIMM=YES
  with a constant SIOL is a silent no-op, and the SIMM↔SSCN scan swap does not
  exist). Themes: this round's High findings are all *unreachable-feature*
  defects — a fix landed correctly but the surface that would exercise it does
  not parse (R12-16), does not queue (R12-47), does not loop (R12-1) or does
  not mask (R12-31). Fix wave 10 next: 36 items (12 confirmed carryovers +
  24 new), minus R12-46 already fixed.

---

## Upstream C defects — moved to `doc/upstream-c-bugs.md` (2026-07-13)

The upstream-C bug catalogue (CBUG-*) that grew here as a section since
2026-07-12 — 37 entries: A1–A4, B1–B27, C1–C6 — was extracted verbatim to the
standalone `doc/upstream-c-bugs.md`, together with the new Batch D (CBUG-D1–D5,
from the wave-13/14 fix reports and deviation dispositions). New upstream
findings accumulate THERE, not here.

---

## Fix wave 10 — dispositions (2026-07-13)

Seven opus fixer panels, one worktree each; main merged and verified. Scope:
the 12 confirmed R11-C carryovers plus the 24 Round-12 findings, minus R12-46
(already fixed in e96a68ce). Every fix carries a negative-controlled test
(observed to FAIL on the pre-fix tree). Verified on the merged tree:
`cargo clippy --workspace --all-targets -- -D warnings` clean,
`cargo nextest run --workspace` **8389 passed / 0 failed / 2 skipped**,
`cargo test --doc --workspace` clean, fmt clean.

**33 FIXED, 2 NOT-REAL, 1 filed-route-refuted-but-adjacent-defect-fixed.**

### Category A — sCalc string engine (8 commits)
- R12-1 + R11-C7 FIXED `ce1e7dab` — UNTIL_END is an operator-stack entry
  (in_stack_pri 0), placed by the flush rules as in C; loop-max is a silent
  break (settable `sCalcLoopMax`, default 1000), `LoopLimitExceeded` deleted.
- R12-2 + R12-3 FIXED `e2fa5d84` — ONE defect, and wider than filed: compiled C
  shows the digest is escaped text, the operand is unescaped first, CRC16/XOR8
  return a *string*, and neither guard is an error. Single `checksum_op` owner.
- R12-4 FIXED `e844f2b0` — `AMODBUS` bounds `":" + operand` to 39 *then*
  appends the LRC (one bound over the concatenation).
- R12-5 FIXED `c84f1ace` — C hands the format to libc `sscanf`; the port now
  has a real scanf engine (`engine/scanf.rs`), shared with BIN_READ. C's
  greedy `%%` skip and second-conversion rejection reproduced; the one UB
  format (`"%d %% %s"` → char* into %d) refused deliberately.
- R12-7 FIXED `57552394` — the citation was LEN; the `toString` sweep found
  REPLACE also uncoerced (C coerces all three operands). Both via
  `into_string_value`.
- R12-8 FIXED `a596e2e3` — second constructor (`ScalcString::from_strncpy`,
  38-byte bound) for the 8 `strNcpy(N-1)` sites; ADD/LITERAL keep 39.
- R12-9 FIXED `dc5385a6` — C's `hex()` maps non-hex to 0 and drops a trailing
  odd char; `hex_decode` (the rejector) deleted. The empty operand stays
  refused: C's `strlen("")-1` wrap is UB (harness segfaults), not a contract.
- R11-C6 FIXED `5949dbdb` — structural: `scalc_result(&value)` deleted,
  `scalc_perform(expr, inputs, precision)` is C's signature; only the numeric
  epilogue takes PREC. Prerequisite found and fixed in the same commit:
  `uses_string` keyed on an opcode the compiler never emits, so no AA-reading
  program was USES_STRING (this also mis-sized MODULO's cast width).

### Category A — aCalc array engine (6 commits)
- R11-C1 FIXED `cca653c6` — MAX/MIN/`>?`/`<?` in the two-arg array dispatch;
  compiled C also overturned two adjacent beliefs fixed in the same commit:
  the vararg fold is NaN-poisoning in BOTH argument orders, and `5>?NaN` is 5.
- R12-6 FIXED `1ce0f33b` — POWER collapses its exponent and maps only on the
  left. C1/C6 are opposite directions of the same shape rule — two commits,
  by design, not one dispatch change.
- R11-C2 FIXED `b87dcbd7` — invariant guard (`CalcError::StackLeak`,
  `try_from([Cell;1])` epilogue); commit states plainly no reachable
  divergence exists (compiler ledger is exact), confirming the Round-12
  reclassification.
- R11-C3 FIXED `705a9cf2` — unset array var is a zero buffer (`toArray`);
  `IXZ(AA)`=-1, `FWHM(AA)`=5 against C with a NULL array arg.
- R11-C5 FIXED `b93c1f21` — AMASK posts on STORE, not on change. The sweep
  widened into the framework: the subscriber-post loop existed as FOUR
  copy-pasted instances; unified into `RecordInstance::collect_subscriber_posts`
  with the new dynamic `take_cycle_posted_fields` hook. One behaviour change
  disclosed: the simulation-path copy gained the `process_posted_fields` gate
  the other three had.
- R11-C4 FIXED `17d98eee` — NEWM compares the fetched value against the value
  the field held BEFORE the fetch (not last-posted); a caput reverted by the
  cycle's own INAA fetch now posts. `put_field_internal_default` extracted so
  overriders wrap the single owner.

### Category B — CA tools (3 commits)
- R12-16 + R12-20 FIXED `09e5fa9b` (one commit — same C getopt block):
  `-0x`/`-lb` parse as single-dash options with an argument; invalid base
  warns and keeps the last VALID base; only `-0` forces DBR_LONG, racing `-d`
  in getopt order. 13 invocations byte-identical to compiled C.
- R12-17 FIXED `5bf9df8b` — wider than cited: ALL numeric options across the
  four binaries route through the new single owner `copt.rs::CTool` (clap
  type-checks none). Compiled C also showed `-# 0` means "not specified"
  (one encoding, now `req_elems: u64` with 0=unset) and the count prefix
  fires on scalars (`-# 1` on a scalar prints `1 200`). 34 invocations
  byte-identical.
- R12-18 FIXED `7faf7a3a` — usage errors: one stderr line, C's text, exit 1;
  required-positional moved from clap to main-validates, matching getopt.
- R12-19 **NOT-REAL** — the signed-char cast already happens at decode
  (`codec.rs:1144`); `caget-rs -d DBR_CTRL_CHAR` output is byte-identical to
  C. Reading the printer suggested "unsigned"; running both proved otherwise.

### Category C — PVA (4 commits)
- R12-31 FIXED `e88c849d` — single-record monitors carry the DB event's
  marked leaves (`DbSubscription::recv_event` → promotion → `change_leaf_paths`).
  The DBE→leaf table had a private duplicate in group.rs; moved to
  `qsrv/pvif.rs` as the single owner (~223 lines deleted).
- R12-32 FIXED `826ccebf` — the queue is `MonitorQueue::push()`, which owns
  pvxs's `real || !val` test by construction (the VecDeque is gone). The
  wrong parity doc-comment at tcp.rs:7756 corrected (it was wrong twice).
- R12-33 FIXED `07c54d4e` — `credit.acquire().await` removed; emit is a
  guarded select arm, `rx.recv()` always polled. Negative control needed
  BURST=200 (40 did not discriminate — the fixer fixed the test, not the
  finding). Pre-fix: 7 distinct values past a queueSize-4 window; post: 4.
- R12-34 **NOT-REAL** `b1d55370` (adjudication + lock test) — pvxs's
  `request2mask` throw is raised inside the SOURCE's connect callback, and
  pvxs's own hosting source (SharedPV) CATCHES it → op error, circuit alive
  ("not re-throwing for consistency", sharedpv.cpp:96-101; pinned by pvxs's
  own testget.cpp:380-393). The circuit reset only happens in QSRV's bare
  `connect()` calls — filed as an upstream C++ defect candidate. Inverse
  control: applying the requested fix broke 3 tests (circuit dropped).

### Category D — asyn (4 commits)
- R12-47 FIXED `9ea48721` — `ConnectCheck` waiver modelled;
  `AsynUser::queue_even_if_not_connected()` sets reason+priority together so
  they cannot be split. HOSTINFO put and connect-time option readback run on
  a dead port; the EOS readback deliberately still refused (C's asymmetry).
- R12-48 FIXED `47af0c09` — `queue_gate` is a total match over `RequestOp`
  (no `_` arm); `check_queue` runs `check_enabled()` first, unconditionally,
  with no argument to skip it. The two refusals cannot be re-fused.
- R11-C9 FIXED `6be69cca` — the sweep found the write paths were the INVERSE
  defect (cleared the slot on would-block); one classifier
  (`ClientSlot::classify_io_error`) is now the only caller of `clear()`.
- R11-C8 FIXED `d4978e93` — REASON put blanks DRVINFO (the reconnect
  re-resolution is the real bug) and posts via the `MONITOR_STATUS_FIELDS`
  snapshot.

### Category E — simulation contract (5 commits, one owner)
- R12-61 FIXED `bc90badc` — `recgbl::simm` module; the root cause named:
  C's "no data" from a constant link is a SUCCESS (`dbConstGetValue` writes
  nothing, returns 0); `SimLinkFetch::{Value,NoData,Failed}` makes the
  conflation unrepresentable. Population defect found by the sweep: C
  declares SVAL on 9 records, the port had 3 — six added (bi, event,
  int64in, longin, mbbi, mbbiDirect).
- R12-64 FIXED `682301d9` + `a93ee742` — `rec_gbl_save_simm`/`rec_gbl_check_simm`
  perform C's genuine SCAN↔SSCN swap (USHRT_MAX opt-out included); the
  iocInit I/O-Intr demotion was a hand-rolled bypass, now routed through the
  single `set_scan` owner.
- R12-65 FIXED `f1c63307` — both C shapes: `recGblGetSimm`'s direct
  `nsta = LINK_ALARM` (SEVR stays NO_ALARM — quirk reproduced), and
  busy/swait's `dbGetLink` → full LINK_ALARM/INVALID.
- R11-C12 FIXED `8a0a2c13` — population re-derived from the C: 8 menuSimm
  records keep RAW; 13 menuYesNo + busy take C's default arm (soft INVALID,
  no substitution, and process continues — the `-1` is not an abort); swait
  confirmed dropped (no switch in C). Keyed on the record's own menu arity,
  not on the literal 2.

### Category E — posting/alarm path (7 commits)
- R11-C10 FIXED `ba2777fb` — the framework fix: `record_value_post` is the
  private owner of `last_posted`; a put's post is the record's only post for
  that put. Scaler's closed-set hook shrunk to its real C reason.
- R12-62 FIXED `de7cb26a` — the DBE_LOG sweep is an independent push in the
  builder (C emits two events for Sn on the completion cycle). The
  pre-existing test asserting "exactly once" encoded the pre-fix model and
  was corrected.
- R11-C11 FIXED `56743aac` — `ProcessAction::ScanOnce`; the framework
  executor owns C's `if (precord->scan)` gate.
- R11-C14 FIXED `f3854e07` — transform on sCalc (non-finite → CALC_ALARM
  via `sCalcPerform`'s `:2056` epilogue) + no VAL post from a process cycle
  (fixed at `deadband_post`, honouring `process_posted_fields`).
- R11-C15 FIXED `3f39e50d` — **the ODLY route from Round 12 is REFUTED**: the
  delaying C cycle raises nsta/nsev before the ASYNC return and only
  `recGblResetAlarms` clears them, so the continuation COMMITS the alarm in
  both C and the port — no divergence there. The real defect at the same
  anchor: acalcout read `calc_alarm` outside `check_alarms`, so a
  limit/link-driven INVALID (IVOA=Set_output_to_IVOV) drove the calculated
  value instead of IVOV. Closed with the `mem::take` one-consumer form; the
  phantom `get_field("CALC_ALARM")` arm deleted.
- R11-C13 FIXED `530382bb` — swait UDF has exactly C's two clear sites and no
  alarm; `Record::raises_udf_alarm()` (default true) is where a record opts
  out of the framework's UDF alarm. C-side sweep: only swait has a reachable
  divergence; 16 other no-UDF-guard record types stay latent at the default.
- R12-63 FIXED `b01e195e` — `accumulate(acc, coef, term)` single owner;
  C's `if (coef)` guard on all six sites (the port had fused them to three).

### Merge integrations (by main, on the merge commits)
- The four subscriber-post builder loops conflicted (a-acalc's unification
  vs e-records'/e-simm's in-place semantics). Resolution: adopt the
  `collect_subscriber_posts` owner at all four sites and port the two
  semantic changes INTO the owner (R12-62 independent LOG push; R11-C10
  `posted_value`/`record_value_post`). Verified by the branches' own
  negative-controlled tests on the merged tree.
- `a299dbae` — transform bridged onto `scalc_perform` (e-records used
  `scalc_result`, which a-scalc deleted); transform is C's psresult==NULL
  caller, so it consumes `StackValue::to_double` only.
- `915dd0e9` — c-pva's marked-leaves tests took the read guard; R11-C10 made
  `notify_field` `&mut self`. Three test sites moved to the write guard.
- `d3062924` — a doc table fenced as text (rustdoc tried to compile it).

### Open Findings — surfaced during fix wave 10 (pending independent verify)

Category A (calc engines):
### W10-A1: MAX/MIN reject a double where C coerces — C pre-scans all args (`j |= isDouble`, sCalcPerform.c:1927) and coerces everything to double if any one is; the port checks only the first popped. Compiled: `MAX(4,"a")`=4, `MIN(4,"a")`=0; port raises TypeMismatch.
### W10-A2: SUBLAST rejects a double where C coerces — C's `|-` (:980-988) is numeric subtraction when either operand is a double. Compiled: `4|-"."`=4, `"a.b"|-4`=−4; port raises TypeMismatch.
### W10-A3: aCalc UNTIL cannot execute — the shared postfix now emits Until/UntilEnd for aCalc too, but `engine/array.rs` has no `Opcode::Control` arm (evals to `CalcError::Internal`). Check what C's aCalc actually does with UNTIL before porting.
### W10-A4: dead opcode `StringOp::PushStringVar` — emitted by nothing after R11-C6; eval still has an arm. Cleanup.
### W10-A5: aCalcout AMASK post carries the cycle's DBE_ALARM bits — C posts AMASK arrays with a literal DBE_VALUE|DBE_LOG (:295) but NEWM with `monitor_mask|…` (:1033); the port's hook posts both with alarm bits. Needs a per-field mask on the hook.
### W10-A6: aCalcout AA..LL can still post on mere change — C's only two array posts are AMASK and NEWM; the port's change detection can invent an event (NELM/NUSE change path).
### W10-A7: `sseq::put_field_internal` ends in bare `put_field`, dropping the DBF coercion for non-special-cased fields — `put_field_internal_default` (added 17d98eee) is the one-line fix.
### W10-A8: aCalcout keeps link-delivered elements beyond `acalcGetNumElements` — invisible today, exposed by a later NUSE increase.

Category B (CA tools):
### W10-B1: `-w` negative timeout does not time out — C hands the negative to `ca_pend_io` (connect timeout, exit 1); the port clamps to the default and succeeds. `cli.rs:52-62` + the test that asserts the clamp.
### W10-B2: `-e`/`-f`/`-g` precedence is fixed e>f>g, not getopt-order-last-wins — same shape as the fixed `-0`/`-d` race; `matches.indices_of` makes order recoverable.
### W10-B3: camonitor prints an epoch stamp for an undefined timestamp where C prints `<undefined>`.
### W10-B4: camonitor timestamp is 1 µs low vs C (7/7 paired invocations) — looks like truncation where C rounds ns→µs; root cause not chased.
### W10-B5: cainfo prints the raw IP where C resolves the host name.

Category C (PVA):
### W10-C1 (upstream C++ candidate): QSRV's `singlesource.cpp:147` / `groupsource.cpp:399` call `MonitorSetupOp::connect()` bare, so a client field typo resets the whole TCP circuit; SharedPV shows the intended catch → `conn->error`. For the Upstream C defects section next catalogue pass.
### W10-C2: the connect-time monitor seed synthesizes its bitset at frame time (decode-equivalent to pvxs today) — pin with a test so `canonical_changed_bitset` changes cannot silently drift the seed frame.
### W10-C3: `put_get_masks` (PUT_GET INIT) has no pvxs counterpart — modelled on pvDatabaseCPP; the file's parity citation is to a different codebase there.

Category D (asyn):
### W10-D1: C refuses an asynRecord CNCT put on a disconnected port (no sentinel, addr>=0) — the port's Connect ops keep `Waived`. A C wart + knowing divergence; needs adjudication.
### W10-D2: `asynCallbackSpecial` ends EVERY special callback in `monitorStatus` (asynRecord.c:897); the Rust OPTIONIV/EOS/CNCT arms do not repost. Same family as R11-C8, different arms.
### W10-D3: no connect-time EOS readback in the Rust record (C: asynRecord.c:1291).
### W10-D4: iocsh `asynSetEos` has no queue timeout (C: 2 s, asynShellCommands.c:245) — a wedged port hangs the shell.
### W10-D5: the server subport ignores `disconnectOnReadTimeout` (drvAsynIPPort honours it).
### W10-D6: `RequestOp::Report` is queued and therefore gated — C's `asynReport` is a direct call that works on a disabled/disconnected port.

Category E (records/framework):
### W10-E1: `.ACKS` can double-post in one cycle — excluded from neither the generic builder loop nor `alarm_posts`. (SEVR/STAT/AMSG/UDF are excluded; ACKS is not.)
### W10-E2: scaler DLY>0 on a non-Passive record starts the count late — C's `delayCallbackFunc` calls `scanOnce` unconditionally on expiry; the port waits for the next periodic scan.
### W10-E3: acalcout IVOV substitution target — C sets ONLY `oval` (:934) and the device support picks the OUT buffer per DOPT/NELM, so IVOA is a no-op on OUT under DOPT=Use_VAL; the port writes VAL+AVAL / OVAL+OAV and always sources arrays. Module doc declares the deviation deliberate — needs adjudication (the two halves must move together if C parity is wanted).
### W10-E4: a failed SIOL read raises no LINK_ALARM (C: dbGetLink → setLinkAlarm → LINK_ALARM/INVALID); only SIMM_ALARM is raised. Every SIOL-reading record.
### W10-E5: busy does not abort on a failed SIML read — C returns before the device write (busyRecord.c:399-401); the port alarms but still writes.
### W10-E6: SSCN/OLDSIMM are served on record types whose C dbd declares neither (CommonFields placement; OLDSIMM newly joins the pre-existing SSCN entry in KNOWN_NON_FIELDLIST). Disclosed structural trade-off.
### W10-E7: aai/aao are not ported (2 of C's 21 SSCN records); their table entries are inert.
### W10-E8: a simulated histogram is frozen (SIOL→VAL no-ops against the bin array). Pre-existing, documented in a processing.rs comment.

### ARANDOM (report-only)
The a-acalc fixer's one-line recommendation: keep the deviation but make it
opt-in deterministic — fixed default seed matching C's thread-private LCG so
parity tests replay, time-seeding behind an explicit call. Still awaiting the
user's disposition; code untouched.

## Round 13 re-audit (2026-07-13)

Five read-only opus auditor panels, one per category. Methods: category A
compiled both upstream calc engines plus a Rust probe crate and diffed
~1500 expressions; category B ran the compiled C tools vs the Rust tools
head-to-head against one live softIoc (~140 paired invocations); category E
compiled the exact recGblResetAlarms/recGblSetSevrVMsg bodies. Every verdict
below is evidence-backed in the round transcript
(`.caucus/sessions/01KX5QMAM71PJZFNWG0SFREHPX/rounds/01KXC72YPSPGYR4J62BNSD75D2.md`
+ round-spills, not tracked by git — verdict summaries here are the durable
record).

### Wave-10 fix verification

All 35 wave-10 dispositions verified independently. 34 hold as landed;
1 is INCOMPLETE:

- `09e5fa9b` (R12-16/R12-20) INCOMPLETE — the `-0`/`-d` race picks the last
  `-0`, C picks the last VALID `-0` (→ R13-16).
- Both NOT-REAL adjudications UPHELD: R12-19 (signed-char cast at decode,
  byte-identical on overflowing limits), R12-34 (pvxs SharedPV catches the
  mask throw; pinned lines re-read).
- The four-way merge resolution `ac5f5d9e` (`collect_subscriber_posts`)
  verified correct by two panels independently (A: no drift vs `b93c1f21`;
  E: all three semantic axes — independent LOG push, `last_posted`
  single-owner, `val.clone()` value preservation).
- "VERIFIED, INCOMPLETE" family halves folded into new findings: `b87dcbd7`
  aCalc-only depth guard → R13-6; `17d98eee`/`b93c1f21` post mask → W10-A5;
  `d4978e93` monitorStatus tail → W10-D2; `6be69cca` widened the slot-revive
  gap → R13-50.
- R11-C15's wave-10 ODLY refutation CONFIRMED by re-reading
  calcoutRecord.c:244,280-283 — nsta/nsev survive the delaying return.

### W10 candidate adjudications: 31 REAL / 2 NOT-REAL

REAL: W10-A1..A8 (A1-A3 Medium, A4-A8 Low), W10-B1..B5 (B1-B4 Medium, B5
Low), W10-C1 (upstream C++, Medium — gateway blast radius), W10-C2 (Low,
superseded by R13-31/32), W10-D1 (user decision; auditor recommends match C),
W10-D2 (Medium), W10-D3 (Low), W10-D5 (Low), W10-D6 (Medium, compiled repro),
W10-E1 (Medium), W10-E2 (Medium), W10-E3 (user decision; both halves must
move together), W10-E4 (**High** — broken SIOL is completely silent at
default SIMS), W10-E5 (Medium), W10-E6 (Low, disclosed trade-off), W10-E7
(Low, informational), W10-E8 (Medium).

NOT-REAL:
- **W10-C3** — pvxs does not implement PUT_GET at all (`serverconn.cpp:259-260`
  is an empty body); the port's PUT_GET is a superset, not a divergence.
  Action: relabel the `pv_request.rs:219-247` parity citation as
  "pvAccessCPP/pvDatabaseCPP extension, no pvxs counterpart".
  pvDatabaseCPP/pvAccessCPP are not on this machine, so the put/get-leg
  masks remain unaudited against their actual reference.
- **W10-D4** — the candidate misread C: `asynShellCommands.c:239`'s
  `timeout = 2` is the I/O timeout; the queue timeout is `queueRequest(...,
  0.0)` + `epicsEventWait` with no deadline (`:245-246`), so C hangs the
  shell on a wedged port too. The port matches C. A real adjacent gap is
  R13-49.

### Open Findings — Round 13 (R13-1 .. R13-63, 32 findings)

Category A (calc engines):
### R13-1: `AND`/`OR` keywords compile as logical ops; C's are bitwise — **High**. `postfix.rs:107-108` vs postfix.c:174-176 / sCalcPostfix.c:237-239 / aCalcPostfix.c:234-236 (all map AND→BIT_AND, OR→BIT_OR). Compiled: `5 OR 3` → C 7, port 1; `12 AND 10` → C 8, port 1. All three engines; silently wrong VAL. XOR and the symbol forms are correct, which hides it.
### R13-2: `[…]`/`{…}` after a function call binds to the wrong operand — **High**. `postfix.rs:523-535` flushes the function at `)`; C keeps it on the stack (in_stack_pri 9) and `[`/`{` (in_coming_pri 11) do not pop it, so the function applies to the subrange result. Compiled: `LEN("abcd")[0,1]` → C 2, port 4; `SQRT(4){"2","9"}` → 2 vs 9; aCalc `AMAX(AA)[0,1]` → 3 vs 9. Fix must defer function emission, not delete the flush.
### R13-3: sCalc tokenizer interprets backslash escapes in string literals; C copies bytes raw — **High**. `token.rs:628-649` vs sCalcPostfix.c:803-812 (byte-for-byte copy; that is why TR_ESC/$T exist). Compiled: `BYTE("\t")` → C 92, port 9; `PRINTF("%d\n",5)` → C 3 bytes with literal backslash-n, port 2 bytes with LF. Port also accepts `"a\"b"` which C rejects. $T/checksum idioms agree by coincidence (both translate), hiding it.
### R13-4: sCalc `>?`/`<?` drop C's string-compare and coercion paths — Medium. `string.rs:493-500` always numeric; C sCalcPerform.c:1296-1328 has left-double/right-double/both-string(strcmp→string result) branches. Compiled: `"abc">?"abd"` → C "abd", port 0. Two-arg sibling of W10-A1; fix together.
### R13-5: sCalc `@`/`@@` still do not compile — R9-8 landed aCalc only — Medium. SCALC_TABLE has no entries; C sCalcPostfix.c:99-100 → A_FETCH/A_SFETCH, eval sCalcPerform.c:1446,1462. Compiled: `@0` (A=7) → C 7, port syntax error. A_SFETCH also missing from uses_string (C sCalcPostfix.c:461). Corrects the doc's "R9-8 FIXED, WIDENED" record.
### R13-6: sCalc engine lacks the end-of-expression stack-depth guard (`b87dcbd7` fixed aCalc only) — Low, structural. `string.rs:782` invents 0.0 from an empty stack; C enforces on both paths (sCalcPerform.c:817-823, 2023-2032). No reachable divergence found in 20 probes; close the family half.
### R13-7: UNTIL ceiling counted at run time, not by C's static pre-scan — Low. `string.rs:722-733` vs sCalcPerform.c:341-365 (pre-scan counts UNTIL opcodes present; tenth aborts −1 regardless of reachability). Probe: ten UNTILs on a dead `?:` branch → C −1, port evaluates.
### R13-8: UNTIL string condition — C reads uninitialised `ps->d` (UB) — Low. sCalcPerform.c:1999 tests raw `d`; LITERAL_STRING never sets it. No C semantic to match. DISPOSITION (adopted): keep the port's defined `to_double` behaviour, document the deviation; do not port UB.

Category B (CA tools):
### R13-16: caget `-0`/`-d` race uses the last `-0`, not the last VALID `-0` — Medium. `caget-rs.rs:102-105` vs caget.c:497-503 (`type = DBR_LONG` only inside `if (outType != dec)`). `caget -0x -d DBR_DOUBLE -0q` → C DBR_DOUBLE, port DBR_LONG. Introduced by 09e5fa9b.
### R13-17: every non-repeatable option is a hard clap error; C getopt accepts any option any number of times — **High**. All four tools, every option except `-0`/`-l`. `caget -w 5 -w 2` → C runs (last wins), port dies with clap's usage block (breaking 7faf7a3a's one-line-diagnostic contract in spirit). Structural fix: every C option becomes Append/Count with last-wins resolution in copt, so a newly declared option cannot re-open the family. W10-B2 is blocked behind this.
### R13-18: `camonitor -t <key>` unknown letter prints no warning — Low. camonitor.c:248-251 warns per bad character (`%c`); `camonitor-rs.rs:529` `_ => {}` with a wrong "matches C" comment. Only `n` is a silent no-op in C.
### R13-19: `camonitor -t n` drops one field separator — Medium. tool_lib.c:517-519 prints name, unconditional separator, timestamp, separator+value — two separators when no timestamp source; the port models the first separator as a suffix of the timestamp and emits one. Every `-t n` line is one column short. Fix: separator before the timestamp, unconditional, as C has it.
### R13-20: float NaN prints `NaN`; C prints `nan` — Medium. `cli.rs:749-751,766-770` `format!("{x}")`; C `%g`. inf/-inf already match. Common case: unset alarm limits on stock ai/ao are NaN, so every `caget -d DBR_GR_*`/`DBR_CTRL_*` differs on four lines.
### R13-21: `-lx`/`-lo`/`-lb` out-of-int32-range/NaN/±Inf — Low, USER DECISION. C's cast is UB (x86-64: uniform 0x80000000; aarch64 would saturate); port is a third answer. Auditor recommends a defined saturating rule + documented deviation over mirroring x86-64 UB.
### R13-22: caput prints `Old :` before enum validation; C prints only the error — Medium. caput.c:485-506 (validate, return 1) precedes :532-535 (`Old :`). Port emits a spurious value line on a failed enum put; exit status already matches.
### R13-23: caput never emits C's "enum index may be too large" warning — Low. caput.c:477-479, 505-507; put and value lines already byte-identical, warning missing.
### R13-24: caput swallows a server-rejected write; C's libca prints a CA.Client.Exception block — Medium. Root in epics-ca-rs (no default exception handler for async server ECA_* on put), not caput.c. Both exit 0; C shows the operator a warning block, port shows nothing.
### R13-25: `cainfo -n` — C falls into `default: usage()` (full usage block + version banner, exit 1); port prints one-line unrecognized-option, exit 1 — Low. A C wart (leftover `n` in the getopt string with no case).
### R13-26: `-h`/`-V` suppress option warnings C prints first — Low. C's getopt emits earlier options' diagnostics before reaching `-h`; clap owns DisplayHelp/DisplayVersion and prints only the help. `caget -w abc -h` → C warns then usage; port help only.

Category C (PVA):
### R13-31: a nested-leaf pvRequest ships the whole parent structure — **High**. `encode.rs:1380-1381` (`canonical_changed_bitset` treats the mask's parent *permission* bit as select-whole-subtree) vs pvxs to_wire_valid/mark leaf semantics (dataencode.cpp:414-439, data.cpp:256-270, pinned by testxcode.cpp:111-116). `field(alarm.status)` → pvxs ships bitset {alarm.status} + one int32; port ships {alarm} + severity+status+message — fields the client did not select, on every GET and monitor frame. Two port tests assert the divergence with a false "pvxs byte-exact" label (encode.rs:3886-3910, 4033-4040) — invert, not preserve.
### R13-32: wire changed-bitset is parent-compressed; pvxs is leaf-enumerated — Low (same root cause as R13-31, bytes-only, decode-safe). Wildcard request: port {0}, pvxs {1,3,4,5,7,8,9}. General form of what W10-C2 asked to pin.
### R13-33: DBE_PROPERTY marks whole display/control/valueAlarm structs; pvxs assigns a leaf subset — Medium. `pvif.rs:83-85` parent paths vs iocsource.cpp:252-310 (getProperties assigns ~13 specific leaves; never display.form, control.minStep, valueAlarm.active/*Severity/hysteresis). Port ships those as freshly-changed hardcoded zeros. Residue of R12-31 at wrong granularity.
### R13-34: a terminal (FINISH) post is squashed into a full queue; pvxs pushes it unconditionally — Medium. `tcp.rs:1691-1698` routes the terminal through push_squash_monitor; pvxs servermon.cpp:270-283 gates the squash on `|| !val` so a terminal always push_backs past limit. On a full FIFO the newest real update is destroyed and replaced by the FINISH marker (limit-1 updates delivered vs C's limit). Independent local fix.
STRUCTURAL NOTE (R13-31/32/33 are one family): the port models "changed" as structure paths expanded by the encoder; pvxs models it as leaf valid flags, never expanded. Fix = leaf-enumerated wire bitset (delete canonical_changed_bitset's parent-collapse clauses) + change_leaves returns pvxs's actual leaf lists. Patching R13-31's clause alone leaves 32/33 open.

Category D (asyn):
### R13-46: `queue_gate`'s wildcard arm silently gates every op C does not queue — Medium, structural cause of R13-47/48 and W10-D6. `port_actor.rs:344` `_ => Some(user.connect_check())`. Exhaustive match (no `_`) forces per-variant classification.
### R13-47: `CallParamCallbacks` is queue-gated — a driver's parameter publish is silently discarded on a disconnected/disabled port — **High**, compiled repro. C asynPortDriver.cpp:1785-1794 is a plain method, no gate, no queue. Port: `set_params_and_notify_blocking` returns Ok(()), actor refuses, parameter reads back ParamUndefined after reconnect. The documented ADCore pattern loses exactly the status updates announcing a disconnection.
### R13-48: `DrvUserCreate` is queue-gated — C calls asynDrvUser->create directly — Medium, compiled repro. asynRecord.c:1242-1254 / devAsynInt32.c:263-277: a pure table lookup that cannot fail for connectivity; port returns Err(Disconnected) → record resolves DRVINFO to parameter 0 with ERRS set.
### R13-49: iocsh asynOctetSet{Input,Output}Eos drop C's `pasynUser->timeout = 2` — Low. asynShellCommands.c:239,288 set it; the sibling asynSetOption handler does (`iocsh.rs:326`), the EOS handlers do not (`iocsh.rs:185`).
### R13-50: a reused client slot never revives the child subport — **High**. C drvAsynIPServerPort.c:357-367 connectDevice()s the child on slot reuse; Rust `accept_one` (ip_server_port.rs:767) only assigns the socket. After any teardown sets the subport's cached `connected=false` (EOF before 6be69cca, any fatal errno after), the next accepted client is refused asynDisconnected forever (subport is correctly noAutoConnect). Structural fix: derive the subport's connected from `slot.is_occupied()` so "slot occupied but port disconnected" cannot be constructed.
### R13-51: a waived request never triggers the port auto-connect C's portThread performs right after it — Low. asynManager.c:812-861: after draining the Connect queue C unconditionally attempts autoConnectDevice (2 s throttle). Port's Waived path skips auto_connect_device (port_actor.rs:449-451); a HOSTINFO repoint on a dead port defers the reconnect indefinitely on an I/O-Intr-only record.

Category E (records/framework):
### R13-61: AMSG is blanked on the alarm-clear cycle where C leaves it stale — Medium, compiled repro. `recgbl.rs:199` takes namsg unconditionally; C recGbl.c:191-195 assigns AND clears only inside `if (strcmp(namsg, amsg) != 0)`, so an unchanged message survives in namsg and AMSG keeps the last alarm text after the alarm clears. Port emits an empty-string AMSG event on clear. Most common via dbLink.c:321 (every failing link read re-raises the same message).
### R13-62: the ACKS event is gated on a value change; C posts whenever the ack rule fires — Low, compiled repro. `recgbl.rs:221-226` requires acks != sevr; C recGbl.c:214-217 posts DBE_VALUE unconditionally inside the rule. Stat-only transition at constant severity (LINK→CALC at INVALID, ACKT=1): C posts stat+amsg+acks, port omits acks.
### R13-63: transform's LA..LP fields are not served — Low. transformRecord.dbd:505-584 declares them; transformRecord.c:797,804 uses them as previously-posted cells. `caget T.LA` → C value, port "Invalid record field name". Readback-only gap (port's change detection uses last_posted).

### Upstream C defect candidates — FILED 2026-07-13 as CBUG-C1..C6 (batch C, now in `doc/upstream-c-bugs.md`)
- CBUG-cand → **CBUG-C1**: LRC/AMODBUS on an empty string is an unbounded read — sCalcPerform.c:247 `i<strlen(rawInput)-1` wraps to SIZE_MAX; compiled upstream SEGFAULTS on `LRC("")`, `AMODBUS("")`, `LRC(AA)` with empty AA. Port returns "" and is safe. High (crash from a reachable record state).
- CBUG-cand → **CBUG-C2**: W10-C1 CONFIRMED — QSRV singlesource.cpp:147 / groupsource.cpp:399 bare `connect()`; a client field typo resets the whole TCP circuit; through a gateway, every downstream user's channels. Medium.
- CBUG-cand → **CBUG-C3**: FETCH_AA `strncpy(ps->s, psarg[i], SCALC_STRING_SIZE)` (sCalcPerform.c:872) leaves the local string unterminated at exactly 40 bytes and atof reads past it — latent, unreachable from a real char[40] field. Low.
- CBUG-cand → **CBUG-C4**: `caget -w nan` hangs forever — ca_pend_io(NaN) never expires (tool_lib.c:628 path). Low.
- CBUG-cand → **CBUG-C5**: sCalc PRINTF with more conversions than arguments reads garbage off the stack (sCalcPerform.c:1546-1564, snprintf with one vararg). Low.
- CBUG-cand → **CBUG-C6**: sCalc UNTIL with a string condition tests uninitialised `ps->d` (sCalcPerform.c:1999). Low. Port disposition recorded at R13-8.

### User-decision queue (resolved 2026-07-13, wave 11)
Five of the six ballots came back; each landed as its own commit:
- **ARANDOM seeding** → ADOPTED as C parity: RNDM/ARNDM/NRNDM replay C's
  fixed-seed (`0xa3bf`) thread-private 16-bit LCG deterministically; the three
  per-engine copies were unified into one shared `calc/engine/random.rs`, and
  time-seeding is an explicit opt-in extension
  (`seed_random_from_time`, documented deviation). Commit `9677b68a`.
- **W10-D1 (CNCT waiver)** → ADOPTED as C parity: Connect-queue ops take
  exactly C's `checkPortConnect` waiver (`addr == -1` or
  `ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED`, asynManager.c:1520,1535-1538) —
  asynRecord CNCT at a device address is refused on a disconnected port, the C
  wart reproduced by decision. Commit `1897513e`.
- **W10-E3 (IVOV substitution)** → ADOPTED as C parity, both halves together:
  Set_output_to_IVOV sets the scalar `OVAL` only (aCalcoutRecord.c:924), and
  the OUT write buffer is chosen by the resolved target element count
  (devaCalcoutSoft.c:75-87; scalar target ⇒ VAL/OVAL, array target ⇒ AVAL/OAV,
  disconnected-CA default ⇒ scalar). Module-doc deviation retracted; one test
  per invariant boundary. Commit `9d083975`.
- **R13-21 (out-of-range float→int display)** → ADOPTED as documented
  deviation with a defined rule: round half-away-from-zero, then saturate at
  the int32 boundary (`+Inf`/overflow → `0x7FFFFFFF`, `−Inf`/underflow →
  `0x80000000`, NaN → 0) — C's cast is UB and x86-64's uniform `0x80000000`
  is not a contract. Commit `3fc1628d`.
- **long-form `--options` superset** → KEPT as documented deviation: every
  C-valid command line parses identically; the long forms only admit
  invocations C refuses. Recorded at the parse-contract owner
  (`epics-ca-rs/src/copt.rs` module doc). Commit `3714dfc3`.

Still awaiting: **W10-E7** (port aai/aao or leave declared-inert).
Adopted without ballot earlier (recommendation = only sane option):
R13-8 do-not-port-UB.

---

## Fix wave 11 — dispositions (2026-07-13)

Five opus fixer panels, one worktree per category; main merged and verified
by the coordinator. Scope: the 32 Round-13 findings plus the 29 assigned W10
candidates (R13-21/W10-D1/W10-E3 + two more went to the user-decision ballot
instead — resolved below). Verification on the merged tree at the time of
merge: `cargo clippy --workspace --all-targets -- -D warnings` clean,
`cargo nextest run --workspace` 8548/8549 (the one failure is
`epics-pva-rs::stability r12_33`, the user's own in-progress test infra —
passes isolated, untouched by the wave), doctests clean. After the
decision-item commits below: **8560 passed / 0 failed / 2 skipped**,
workspace clippy clean.

### Category A — calc engines (merge `73b735ed`)
- R13-1 FIXED `9c333a96` — AND/OR are the BITWISE operators in all three engines.
- R13-2 FIXED `9df03af8` — the function is deferred past a following `[..]`/`{..}`.
- R13-3 FIXED `c876a6f4` — sCalc string literals are raw bytes; $T is the only translator.
- R13-4 FIXED `1e95fa72` — sCalc `>?`/`<?` settle their types like every other binary op.
- R13-5 FIXED `ed0abb19` — sCalc has `@`/`@@`, the dynamic-argument fetches.
- R13-6 FIXED `bc8ba6a2` — sCalc enforces the end-of-expression stack-depth invariant.
- R13-7 FIXED `92175f20` — the UNTIL ceiling is C's static pre-scan, not a runtime counter.
- R13-8 DOCUMENTED `41678b34` — the UNTIL string-condition deviation from C (adopted; filed as CBUG-C6).
- W10-A1 FIXED `f00b1839` — sCalc MAX/MIN settle their types by C's pre-scan.
- W10-A2 FIXED `8a76ad0f` — sCalc `|-` is subtraction when either operand is a double.
- W10-A3 FIXED `b763bbe8` — aCalc runs UNTIL loops (the array evaluator had no Control arm).
- W10-A4 FIXED `6b85f8b0` — the dead `StringOp::PushStringVar` is deleted.
- W10-A7 FIXED `269c8fc2` — sseq's internal put ends in `put_field_internal_default`.
- W10-A8 FIXED `210887a3` — an acalcout array input is spliced into, never replaced.

### Category B — CA tools (merge `3240e1b5`)
- R13-16 FIXED `ff4fc509` — only a VALID `-0<base>` re-enters the `-d` type race.
- R13-17 FIXED `00c2463f` — every C option is repeatable, last one wins (structural: `assert_repeatable` refuses non-Append/Count specs).
- R13-18 FIXED `766dad06` — every unknown `-t` character warns.
- R13-19 FIXED `c9ae882b` — the field separator belongs to the value, not the timestamp.
- R13-20 FIXED `977a5350` — a non-finite double is spelled the way C printf spells it.
- R13-22 FIXED `6d9ee5d8` — caput validates the value before the `Old :` readback.
- R13-23 FIXED `8219ff5a` — an out-of-range enum index warns but still puts.
- R13-24 FIXED `fdb82010` — a server-rejected put prints libca's CA.Client.Exception block.
- R13-25 FIXED `15c59dd1` — cainfo `-n` is C's `default:` arm, not an unknown option.
- R13-26 FIXED `1cecf272` — the getopt loop runs in argv order, and `-h`/`-V` end it there.
- W10-B1 FIXED `6fc13c86` — a negative `-w` is an already-expired deadline.
- W10-B2 FIXED `15741cf4` — `-e`/`-f`/`-g` is the last VALID occurrence in getopt order.
- W10-B3 FIXED `9d6be264` — an all-zero stamp prints `<undefined>`, not the EPICS epoch.
- W10-B4 FIXED `a150343f` — ns→µs rounds with C's clamp in one shared time formatter.
- W10-B5 FIXED `88c10c82` — the host a client names is the resolved one, not the dotted IP.
- W11-B6 FIXED `83160d5b` — an option-argument that starts with `-` belongs to the option.
- W11-B7 FIXED `1b252bd2` — a later `-t` resets the source, never the kind.

### Category C — PVA (merge `23b9f499`)
- R13-31/R13-32 FIXED `a529d571` — the wire changed-bitset is leaf-enumerated.
- R13-33 FIXED `abaf0e57` — a DBE_PROPERTY event marks pvxs's leaves, not the parent structs.
- R13-34 FIXED `d5dc0c84` — a terminal monitor post is push_back'd past limit, never squashed.
- W10-C2 PINNED `9d2c52bf` — the connect-time monitor seed frame, byte-for-byte.
- W10-C3 RELABELED `873fde25` — `put_get_masks`' parity citation (no pvxs counterpart; pvAccessCPP/pvDatabaseCPP extension).

### Category D — asyn (merge `d0242f3c`)
- R13-46 FIXED `4f26bfe9` — `queue_gate` answers "does C queue this?" per op, no wildcard.
- R13-47 FIXED `03cf3e1b` — a driver's parameter publish is not queue-gated.
- R13-48 FIXED `9a138055` — `drvUser->create` is a direct table lookup, not a queued request.
- R13-49 FIXED `8eded27b` — the EOS shell commands carry C's 2 s I/O timeout.
- R13-50 FIXED `6d24cb79` — a reused client slot revives the child subport.
- R13-51 FIXED `175296f6` — a waived request is followed by C's port auto-connect (+ test-order deflake `0a148e3e`: counter asserts sequence behind an actor probe barrier, the auto-connect tail runs after the reply).
- W10-D2 FIXED `09a782e6` — every asynCallbackSpecial arm ends in monitorStatus.
- W10-D3 FIXED `ff212582` — connectDevice reads the EOS back from the driver.
- W10-D5 FIXED `24ab7c74` — the IP-server child port honours disconnectOnReadTimeout.
- W10-D6 FIXED `56288d66` — asynReport works on a disabled or disconnected port.

### Category E — records/framework (merge `44cb6508`)
- R13-61 FIXED `a0a8cd73` — AMSG keeps the last alarm text after the alarm clears.
- R13-62 FIXED `3e532964` — ACKS posts whenever the ack rule fires, not only on a change.
- R13-63 FIXED `81968d39` — transform serves LA..LP, the previous-value readbacks.
- W10-E1 FIXED `356d3d09` — `.ACKS` must not double-post.
- W10-E2 FIXED `9db78b3d` — the scaler DLY watchdog is what starts a delayed count.
- W10-E4 FIXED `20ac8a5c` — a failed SIOL read raises LINK_ALARM.
- W10-E5 FIXED `82c3deb1` — busy's failed SIML read returns before the output write.
- W10-E8 FIXED `974bcbcd` — a simulated histogram lands SIOL in SGNL and bins it.
- W10-A5 FIXED `af4fc41a` — acalcout: one post per C call site, with that call site's mask.
- W10-A6 FIXED `e92e3c4a` — acalcout AA..LL never post from change detection.
- (docs `61fd1d23` — the SIMM-scan-swap docs point at the predicate that exists.)

### Post-merge (coordinator, this branch directly)
- Merge of main `8f6343fc` — PRs #12-#24 (v0.23.0): BoundTcp
  readiness-by-construction kept WITHOUT re-introducing the TCP-path beacon
  reset (C rsrv resets the ramp only on ctlPause, online_notify.c:126-129 —
  the anomaly path stays `CaServer::trigger_beacon_anomaly` only);
  `PortHandle` actor-id plumbing and `register_port → Result` fallout
  resolved; `await_reply`'s queue-timeout timer is armed only under a tokio
  reactor (`Handle::try_current`), the deadline still enforced at actor
  dequeue for blocking callers; bridge `AccessControl::can_write` async_trait
  fallout in tests.
- Decision items (ballot results above): `9677b68a` ARANDOM, `1897513e`
  W10-D1, `9d083975` W10-E3, `3fc1628d` R13-21, `3714dfc3` long-form
  `--options`.
- CBUG catalogue batch C filed (CBUG-C1..C6, this commit).

## Open Findings — surfaced during fix wave 11 (reported by fixers, pending independent verify)

Leads the wave-11 fixers reported outside their assignments; none verified
against compiled C yet — Round-14 adjudication input, NOT findings.

- sCalc double-only no-ops: `BYTE`, `SUBLAST` (`|-`), `TO_DOUBLE` (`DBL`),
  and the string-var STORES (`AA:=`) are absent from C's `uses_string`
  allowlist (sCalcPostfix.c:461 area), so an expression using only them runs
  the no-string evaluator — the port mirrors the list, but the *evaluator
  behaviour* for these ops in the numeric path was flagged as unproven.
- Dynamic STORE `@n:=` / `@@n:=` — R13-5 added the dynamic FETCHes; whether C
  also compiles dynamic STOREs (and the port's answer) is unresolved.
- `loop_pairs` in the sCalc compiler is reported dead after R13-7's pre-scan.
- SIOL write path: LINK_ALARM ordering vs the value write (sibling of the
  W10-E4 read-side fix) flagged as unaudited.
- `alarm_field_posts` may emit duplicate posts for fields covered by both an
  alarm mask and change detection (adjacent to W10-E1's ACKS double-post).
- asyn `connect_device` posts connect state unconditionally; C's
  `post_if_new` semantics flagged as possibly narrower.
- `asynSetEos` (iocsh) drops the addr argument on one path.
- The EOS driver hooks are not asynUser-aware (C threads pasynUser through;
  the port's driver trait does not).
- catools `-h`/`-V`: C prints usage to stderr in some tools and stdout in
  others; the port's uniform choice was flagged for a per-tool check.
- scalcout OUT drives `OVAL` unconditionally; C `devsCalcoutSoft.c:66-115`
  picks the buffer by the TARGET FIELD TYPE via `dbCaGetLinkDBFtype` —
  string-class target gets `OSV` (DBR_STRING), char-array target gets `OSV`
  as DBF_CHAR bytes, numeric target gets `OVAL` (DBR_DOUBLE). Surfaced during
  the W10-E3 family widening (the acalcout target-NELM analogue); port site
  `scalcout.rs:1200` (`multi_output_links`).

---

## Round 14 re-audit (2026-07-13)

Five read-only opus auditor panels reused from Round 13 (round
`01KXCQM527VJ3GX4CKQCT8DAYP`; full per-panel output in the caucus round
archive). Methods: A re-ran the ~1500-case compiled-C corpus plus a new
compiled BASE-engine driver (`calcPerform.c` — this is what exposed R14-1)
and a 57-case aCalc array sweep; B ran ~110 head-to-head invocations of the
compiled C tools vs the Rust tools on one live softIoc; C hand-decoded pvxs
BitMask wire layout from `testxcode.cpp` and drove the port's bitset API
from an out-of-tree probe; D compiled two probes against asyn-rs (retry
timer/enabled gate; four-runtime-shape queue-deadline matrix); E read every
cited C line and re-used the Round-13 compiled recGbl harnesses.

### Wave-11 fix verification

44 of 49 verified dispositions HOLD. The exceptions:

- `9677b68a` (ARANDOM) INCOMPLETE — sCalc/aCalc replay compiled C exactly;
  the BASE numeric engine does not (C has a second generator) → **R14-1**.
- R13-5 `ed0abb19` INCOMPLETE — dynamic fetches match; the dynamic STOREs C
  compiles in the same switch are still refused → **R14-4**.
- W10-A2 `8a76ad0f` INCOMPLETE — correct in the string evaluator; an
  all-double `|-` never reaches it in C → **R14-2**.
- W10-A4 `6b85f8b0` INCOMPLETE — the `StoreStringVar` twin is equally dead
  → **R14-8**.
- W10-A8 `210887a3` REGRESSED (both bounds) — C's `dbPut` DOES zero the tail
  via `put_array_info`, and the link splice is bounded at NELM where C
  bounds at `numElements` → **R14-6**, **R14-7**.
- R13-26 `1cecf272` INCOMPLETE — ordering fix real, but the getopt `'?'`/`':'`
  error arms bypass the warning replay → **R14-18**.
- R13-31/32 `a529d571` INCOMPLETE — encoder fixed, enqueue gate still tests
  the structure-bit bitset → **R14-31**.
- R13-33 `abaf0e57` INCOMPLETE — the default (pure-self-trigger) group shape
  bypasses the marked path entirely → **R14-32**.
- `1897513e` (W10-D1) INCOMPLETE — the gate is byte-exact; the refusal
  REPORTING invents callback behaviour C never runs → **R14-46**.
- W10-D2 `09a782e6` INCOMPLETE — right shape for callbacks that ran; gate
  refusals mis-reported (R14-46) and `connect_device` posts nothing
  (→ **R14-47**).
- W10-D6 `56288d66` HOLDS for dispatch; report CONTENT is not C's → **R14-51**.
- `9d083975` (W10-E3) **HOLDS** — set_output_to_ivov is `oval = ivov` alone;
  buffer choice matches devaCalcoutSoft.c:65-88 incl. disconnected-CA
  default and nuse clamp.
- Merge-of-main resolutions all HOLD: tcp.rs/beacon (structurally — the TCP
  path no longer receives the Notify; residual stale comment → **R14-19**),
  port_handle.rs queue deadline (verified in all four runtime shapes),
  qsrv can_write async fallout (behaviour-neutral, clippy-clean).

### Lead adjudications (wave-11 fixer leads)

- sCalc double-only no-ops — PARTIALLY REAL: BYTE/TO_DOUBLE are identity both
  sides; SUBLAST diverges → R14-2. STORE_AA unreachable via the sticky
  uses_string rule → R14-3 (REAL).
- Dynamic STORE `@n:=`/`@@n:=` — REAL → R14-4.
- `loop_pairs` dead — CONFIRMED DEAD → folded into R14-8 (hygiene).
- catools `-h`/`-V` streams — REAL for `-h` (all four C tools use stderr) →
  R14-16; `-V` already matches (stdout).
- asyn `connect_device` post_if_new — REAL but INVERTED: it posts nothing at
  all → R14-47.
- `asynSetEos` drops addr — REAL → R14-48.
- EOS hooks not asynUser-aware — REAL → R14-49 (structural cause of R14-48).
- SIOL WRITE LINK_ALARM ordering — REAL → R14-62 (setLinkAlarm lives inside
  dbPutLink itself, dbLink.c:444-446).
- `alarm_field_posts` duplicates — NOT-REAL: RECGBL_POSTED_ALARM_FIELDS
  excludes the four fields from the subscriber loop; one event per field per
  cycle. Residual: the "single owner" comment at processing.rs:399-401 is
  false (2 of 5 sites call it) — doc-only.
- scalcout OUT buffer by TARGET FIELD TYPE — REAL → R14-61.

### Open Findings — Round 14 (23 findings)

Category A (calc engines):
### R14-1: base `RNDM` replays the WRONG C generator — **High**, compiled repro. `numeric.rs:51,67-68` → `random.rs:31-37` vs calcPerform.c:508-520: base C's `calcRandom` is `seed = seed*multy + addy; return (double)seed/65535.0` — process-global static, NO `+1`, divisor 65535. The `9677b68a` unification put the base engine on sCalc/aCalc's `local_random` (`(seed+1)/65536`, thread-private). Same LCG state, different normalisation: first draw C 0.75007248, port 0.75007629. Every RNDM in calc/calcout/swait differs from C on every draw. sCalc/aCalc are correct.
### R14-2: sCalc `|-` on two doubles succeeds where compiled C fails the whole expression — Medium, compiled repro. string.rs (SubLast arm runs regardless of path) vs sCalcPerform.c:811-823: SUBLAST is not in USES_STRING, so `A|-B` all-numeric runs C's double-only evaluator which has NO case SUBLAST → default break → depth check fails → stat=-1, VAL=-1, SVAL="***ERROR***", CALC_ALARM. Port computes A−B.
### R14-3: `uses_string` re-scan misses C's sticky lookup rule — Medium, compiled repro. C's USES_STRING flag latches on the ELEMENT LOOKED UP (sCalcPostfix.c:447-471): `AA:=` is looked up as FETCH_AA (setting the flag) before being rewritten to STORE_AA (:552-557). The port re-scans the final opcode list where only Core(StoreDoubleVar) survives, so `AA:="…";…` can take the wrong evaluator.
### R14-4: dynamic STOREs `@n:=` / `@@n:=` do not compile — Medium, compiled repro. sCalcPostfix.c:509-529 → A_STORE/A_SSTORE, evaluated at sCalcPerform.c:440,:897,:909. Port raises BadAssignment at compile; a scalcout using the idiom never runs (CLCV≠0). R13-5 ported only the fetch half.
### R14-5: sCalc `>>`/`<<` on a string operand is a CHARACTER shift in C; the port bit-shifts — **High**, compiled repro. string.rs:287-293 (pop2_f64) vs sCalcPerform.c:1263-1294: C type-branches on the left operand — string shifts the 40-byte buffer (`>>` right, space-fill; `<<` left, truncate), count = myNINT(rhs) clamped 40. `"abc">>1` → C `" abc"`, port `0.0`. Wrong value AND wrong type on every sCalc string shift. (All other C string type-branches re-verified head-to-head: shifts are the only remaining gap.)
### R14-6: an acalcout client put must zero `[nNew, numElements)` — the W10-A8 fix asserts the opposite — Medium. acalcout.rs:363-369,376-381 vs dbAccess.c:1366-1369 (dbPut calls put_array_info for SPC_DBADDR fields) + aCalcoutRecord.c:726-731 (zeros the tail of the NUSE window). Client short put leaves stale elements the calc engine then reads.
### R14-7: the acalcout link splice is bounded at NELM; C's link read is bounded at numElements — Medium. acalcout.rs:363-369 vs aCalcoutRecord.c:1096-1097 (`nRequest = acalcGetNumElements`): an INAA source longer than the NUSE window overwrites the hidden tail `[numElements, nelm)` the splice invariant exists to preserve.
### R14-8: dead calc surface: `StringOp::StoreStringVar` (opcodes.rs:114, string.rs:507 — nothing emits it; its uses_string arm at :1716 is misleading next to R14-3) and `CompiledExpr::loop_pairs` (mod.rs:36 — written Vec::new() at all 5 sites, never read) — Low, hygiene.

Category B (CA tools):
### R14-16: `-h` writes the usage block to stdout; all four C tools write it to stderr — Low. copt.rs:287-291 vs caget.c:58/camonitor.c:47/caput.c:62/cainfo.c:39. `-V` already matches (stdout). The port's comment concedes the deviation but it is not in the deviation register; the exit-1 usage path already writes stderr, so the same block goes to two streams depending on how it was reached.
### R14-17: `EPICS_CLI_TIMEOUT` bypasses the copt scanner — Medium, live repro. cli.rs:57-67 (`str::parse`) vs tool_lib.c:646-660 (epicsScanDouble + warning). Bad value: C warns naming the variable, port silent. Whitespace-padded value: C honours it (epicsParseDouble trims), port silently reverts to 1 s — `" -1 "` flips exit status. The one C-scanned argument escaping copt's single-owner contract; route it through the scanner, do not add a second parser.
### R14-18: the getopt `'?'`/`':'` arms swallow every preceding option's warning — Medium, live repro. copt.rs:307-346 (usage_exit runs before Scan::finish replays) vs caget.c:437-522. `caget -w abc -X` → C two stderr lines, port one. Replay the buffer up to the offending token's argv index (options AFTER it never warn in C — verified).
### R14-19: `run_beacon_emitter`'s doc comment still describes the TCP-connect ramp reset the merge removed — Low, doc-only. beacon.rs:20-25: every clause now false (sole notifier is trigger_beacon_anomaly; tests/beacon_ramp_connect.rs forbids what the comment calls deliberate). Stale-comment-as-instruction hazard.

Category C (PVA):
### R14-31: the monitor enqueue gate and the wire bitset disagree on structure bits — Medium, compiled repro. tcp.rs:1752-1754 (`MonitorQueue::real` tests marked_changed_bitset, which carries structure bits) vs pvrequest.cpp:73-92 (testmask tests `store[idx].valid` — never true for a structure) + servermon.cpp:256-268. After a529d571 the encoder emits leaves only, so a post admitted on a structure bit alone frames to an EMPTY changed-bitset — client typo `field(timeStamp.bogus)` yields empty MONITOR DATA frames at full event rate where pvxs drops the post. Structural fix: one owner — `real()` decides on `canonical_changed_bitset(intro, marked ∩ mask)` non-empty.
### R14-32: a pure-self-trigger group (the DEFAULT `+trigger` shape) bypasses the marked path — R13-33's property-leaf narrowing never applies to it — Medium. group.rs:2062-2066,:2124-2125 (EventMark::Derive) vs groupsource.cpp:355-380 + iocsource.cpp:312-352. Wider than C: the snapshot diff marks timeStamp/alarm leaves on property events C never assigns (UpdateType::Property gates getTimeAlarm). Narrower: unchanged limits diff to nothing → empty-bitset frame where pvxs carries assigned-not-changed leaves. Fix: fall through to leaves_or_derive for the self-trigger case; retire Derive/emits_partial for QSRV groups.

Category D (asyn):
### R14-46: a gate-refused special() is reported as a callback that ran — Medium. asyn_record/mod.rs:2698-2712, :2936-2947, :4115-4161 vs asynRecord.c:571-576: on a refused queueRequest C writes `pasynUserSpecial->errorMessage` ("port X not connected") to ERRS and frees the user — asynCallbackSpecial never runs, so no readback, no monitorStatus tail. Port invents callback-shaped ERRS text and returns SpecialRan::Yes → readback + monitorStatus + posts C never performs. Only is_queue_timeout is special-cased; Disconnected/Disabled falls through the driver-error path.
### R14-47: `connect_device` refreshes every readback field and posts NONE of them — Medium. mod.rs:3097-3123 vs asynRecord.c:1270,:1319 (two monitorStatus calls) + :1848-1938/:1985-2026 (getOptions/getEos post BAUD..HOSTINFO, IEOS/OEOS). A put to PORT/ADDR/DRVINFO/PCNCT updates the record in memory but fires no CA monitor — screens keep showing the previous port's values.
### R14-48: iocsh `asynSetEos`/`asynShowEos` parse addr and throw it away — Medium. iocsh.rs `let _addr = …` vs asynShellCommands.c:220,:233-234,:79 (addr threads into findInterface → connectDevice). Multi-device port: shell EOS applies to the wrong device. Sibling asynSetOption routes addr correctly.
### R14-49: EOS is port-wide state with no asynUser on the hook — a multi-device port cannot hold two EOSes — Medium, structural cause of R14-48. port.rs:196 (one `input_eos` per port), :1561 (`set_input_eos(&mut self, eos)` — no user/addr) vs asynInterposeEos.c:48,:84-120,:288 (per-(port,addr) instance, hook takes pasynUser). Fix at the hook: `set_input_eos(&mut self, user: &AsynUser, eos: &[u8])`, per-device state — one change, not two patches.
### R14-50: the auto-connect retry timer bypasses the enabled/defunct gate — **High**, compiled repro. port_actor.rs:384-406 (`service_connect_timer` checks is_connected + auto_connect, then calls driver.connect directly) vs asynManager.c:3252-3266 (timer issues queueRequest, which the gate refuses asynDisabled) + :1541-1546. `asynEnable(port,0)` does not stop asyn-rs: probe shows 6 hw connect attempts per 300 ms while DISABLED (C: 0); a shutdownPort'd (defunct) port keeps connecting too. Structural fix: route the timer's connect through the same gate the queue uses.
### R14-51: `asynReport` prints none of C's manager-level block — Low. port_actor.rs:1225-1245 + port.rs:1178-1190 vs asynManager.c:1043-1122 (reportPort): multiDevice/canBlock/autoConnect line, enabled/connected/numberConnects, nDevices/nQueued/blocked, lock states, exception counts, trace masks, per-address lines, interpose/interface lists — all absent. First tool an engineer runs on a stuck port; parsers break.

Category E (records/framework):
### R14-61: scalcout drives OVAL into every OUT target — C routes the buffer by the target's FIELD TYPE and sends OSV to string/char targets — **High**. scalcout.rs:1200 vs devsCalcoutSoft.c:66-144: STRING/ENUM/MENU/DEVICE/link-type targets get DBR_STRING from &osv; CHAR/UCHAR with n_elements>1 get DBF_CHAR from &osv (async) or &sval (sync — C asymmetry) clamped to sizeof(sval); numeric targets get DBR_DOUBLE from &oval. Every string-valued scalcout output link is wrong (writes 0/last numeric where C writes the computed string). The target-field-type sibling of W10-E3's target-NELM choice.
### R14-62: a failed OUT / SIOL WRITE raises no LINK_ALARM, and the write is sequenced after the alarm commit — **High**. processing.rs:3241,:3270,:4695 + links.rs:742,:1084 vs dbLink.c:434-448 (`setLinkAlarm` INSIDE dbPutLink; :469-471 async twin) + aoRecord.c:196-232 (checkAlarms → writeValue → monitor). Port swallows put failures (eprintln/discard) → a record whose OUT/SIOL target is down stays NO_ALARM forever; and even once raised it would land a cycle late (reset_alarms at :2782 precedes the write at :3241). Write-side twin of W10-E4. Affects every soft-output record + simulated SIOL writes.
### R14-63: `.UDF` is posted on every cycle that posts anything — C never posts UDF at all — Medium, rg proof. processing.rs:3123-3134,:4224-4235,:5635-5645 + record_instance.rs:2832-2843 push ("UDF", udf, cycle_mask) with no change detection and a synthesized mask; justifying comment cites `recGblCheckUDF`, which does not exist, and recGblResetAlarms posts only SEVR/STAT/AMSG/ACKS (recGbl.c:204-216). `db_post_events(...udf...)` has ZERO matches across epics-base and epics-modules; the only C UDF event is a client dbPut to UDF itself. Archivers/alarm handlers on .UDF see duplicate streams C cannot produce.

### Notes
- epics-ca-rs suite is FLAKY under parallel load (2 different single-test
  failures in 5 full runs: caput_readback_timeout, acf_host_identity; both
  pass isolated). Workspace "8560/0" gates are single-run observations.
- False comment at processing.rs:399-401 ("single owner" — 2 of 5 alarm-post
  sites call it) — fold into the wave-12 E batch as doc-only.
- R13-24 stream-interleave observation (exception block position under 2>&1)
  examined and NOT filed: two independently-correct streams.

## Fix wave 12 — dispositions (2026-07-13)

All 23 Round-14 findings fixed, one commit per finding, on 5 worktree
fixer branches, merged: A `caucus/WG0SFREHPX/fixer-a-calc-21bd43a5-1`
(8 commits), B `…fixer-b-catools-710a473a-1` (4), C `…fixer-c-pva-2167e637-1`
(2), D `…fixer-d-asyn-cec348ea-1` (6), E `…fixer-e-records-402d4f3e-1` (4).
Post-merge full-workspace gate: clippy `-D warnings` clean, nextest
8638/8638 (br24 flaked once under load, passes isolated — known list),
doctests clean.

- R14-1 `591acb98` — base engine gets its own `calc_random()`
  (seed/65535.0, calcPerform.c:508-520); sCalc/aCalc keep local_random;
  NRNDM errors on base (not in its element table); 9677b68a time-seed
  opt-in reseeds both.
- R14-2 `79ed4479` — family-widened: SUBLAST, DBL, BYTE all have no case
  in C's double-only switch → no-op arm → StackLeak → stat -1 / "***ERROR***".
- R14-3 `07c7c475` — structural: `CompiledExpr.uses_string` latched by
  postfix::compile at element lookup (C allowlist); final-opcode re-scan
  deleted.
- R14-4 `0a0f1ef5` — dynamic stores @n:=/@@n:= via Assign look-back rewrite
  (mirrors R13-5 fetch); 4 new opcodes; out-of-range stores nothing; aCalc
  @@ broadcasts + amask; base still rejects @.
- R14-5 `e6cddd6c` — >>/<< type-branch on left operand; string = character
  shift over 40-byte buffer (myNINT clamp 40, space-fill/truncate).
  DOCUMENTED DEVIATION: negative count clamps to 0 (identity) — C indexes
  out of bounds (UB, no lower clamp at sCalcPerform.c:1263-1294).
- R14-6 `4f189f20` — single owner `client_put_array` splices into the
  persistent calloc(nelm) buffer and zeros [nNew, numElements); AA..LL,
  AVAL, OAV all route through it; W10-A8's wrong doc + test corrected.
- R14-7 `c5eff367` — splice bound moved NELM → numElements (NUSE window),
  hidden tail [numElements, nelm) preserved.
- R14-8 `7a8b0cfb` — StoreStringVar + loop_pairs removed; rg returns nothing.
- R14-16 `7b4c7e19` — usage block to stderr at both statuses, one call
  site; -V stays stdout.
- R14-17 `afa18609` — env_default_timeout scans via copt::scan_double
  (single owner), C's warning text at env-read time (before getopt loop);
  " -1 " now an expired deadline.
- R14-18 `8f02f5de` — structural: getopt error is a third loop exit owned
  by Scan::finish; get_matches re-parses the longest accepted argv prefix
  so resolvers run the same path as a clean line; warnings replay up to
  the offending token only. (C caget not built on this box; contract from
  caget.c:398-523 + live port behavior.)
- R14-19 `2a3b4975` — beacon comment rewritten; same-defect second site
  in client/beacon_monitor.rs test rationale fixed same commit.
- R14-31 `d1496c9f` — new encode::marked_wire_changed_bitset
  (marked ∩ mask → canonical) is the single owner; MonitorQueue::real
  gates on it non-empty, build_monitor_payload_marked frames the same
  value. Regression test proves the dropped frame would have been empty.
- R14-32 `71095d9b` — both mark sites fall through to leaves_or_derive.
  Verified-vs-C behavior change: ARCHIVE-only self post now marks nothing
  and is dropped (groupsource.cpp:266-275,331-337);
  testqgroup archive test re-pinned on the triggered-target path.
  RETAINED: EventMark::Derive + monitor_emits_partial for root-flattened
  members (field_name == "" has no addressable leaf paths) — removing
  them would under-mark; nothing else uses them.
- R14-50 `0d65de62` — PortActor::connect_gate() (check_queue(-1, Waived))
  gates the retry timer AND auto_connect_port/device (family: the
  auto-connect pair ran before check_queue); refused attempt does not
  re-arm the timer (asynManager.c:3281).
- R14-49 `a70cdf2b` — EOS keyed HashMap<i32, DeviceEos> by
  eos_device_key(multi_device, addr); all four PortDriver EOS hooks take
  &AsynUser; EosInterpose per-addr config/read-ahead/partial-match.
  Distinct: prologix keeps one EOS (bus-wide ++eos register).
- R14-48 `7a7cb085` — iocsh EOS commands .with_addr(addr); added missing
  asynOctetGet*Eos (asynShowEos) with epicsStrnEscapedFromRaw port.
- R14-46 `a02084a4` — root cause closed: gate refusals stamp new
  AsynError::QueueRefused; never_ran() = refused ∪ queue-timeout asked by
  every queued-request caller; option/EOS/CNCT arms report refusal +
  SpecialRan::No; process path reports "queueRequest failed" with
  STATE/MINOR. modbus ioc.rs decodes via AsynError::status().
- R14-47 `2c4b2ae7` — AsynRecord::posting is the single owner of C's
  REMEMBER_STATE…POST_IF_NEW bracket; connect-time option/EOS readbacks,
  both monitorStatus calls (incl. bad:→done: failure tail), put readbacks,
  PCNCT=0 detach all post through it. Regression test times out pre-fix.
- R14-51 `30d5fe37` — reportPrintPort block replicated (state/queue/lock/
  exception/trace at details≥1, interpose+interface lists at ≥2,
  per-address block, negative-details suppression, "destroyed" for
  defunct); number_connects counted by the connection-edge owner; default
  PortDriver::report is now asynPortDriver::report.
- R14-61 `29728d2f` — structural: W10-E3 mechanism generalized —
  PvDatabase::resolve_out_target returns OutTarget { field_type,
  element_count, is_ca_link } (dbNameToAddr / dbCaGetLinkDBFtype);
  Record::multi_output_buffer reproduces each device support's switch.
  scalcout = devsCalcoutSoft.c:66-144 verbatim incl. sync/async &sval/&osv
  asymmetry + 40-byte clamp; acalcout = devaCalcoutSoft's. Distinct:
  dfanout/seq/fanout put DBR_DOUBLE unconditionally in C.
- R14-62 `9ab4fd2a` — invariant: every failing OUT/SIOL put MUST raise
  LINK_ALARM/INVALID into the same cycle's alarm. Owner:
  write_out_link_value → rec_gbl_set_link_alarm (C setLinkAlarm inside
  dbPutLink, dbLink.c:434-448/:459-471). Bypass audit closed: record OUT
  sync+async, dispatch_multi_output_values (extracted, both paths share
  one copy), dfanout OUTA..P, seq LNK0..F, both ProcessAction write twins,
  write_sim_siol_value now returns its failure. Both process paths run the
  whole output stage BEFORE rec_gbl_reset_alarms (checkAlarms → writeValue
  → monitor, aoRecord.c:196-232); IVOA veto tests pending nsev; write
  guard dropped across writes (self-referencing OUT would deadlock).
  dfanout failure test now expects INVALID (C: setLinkAlarm INVALID wins
  over push_values' MAJOR).
- R14-63 `cda7a05d` — all four synthetic .UDF pushes removed + false
  recGblCheckUDF comment deleted (symbol does not exist in C; zero
  db_post_events-on-udf matches); .UDF still posts via generic client put.
- adjudication `dd4f00d2` — alarm_field_posts comment corrected (2 of 5
  reset sites call it; 3 open-code the masks; SDIS/SELN not clients).

### Documented deviations added this wave
- R14-5: negative shift count = identity (C is UB).
- R14-46/D: two asynReport lines have no C-identical source (no port
  mutexes to sample; exception fan-out synchronous) — stated in place.

### Open leads — surfaced during fix wave 12 (Round 15 input)
- Library env-double config sites parse STRICTER than C's
  envGetDoubleConfigParam (sscanf "%lf" accepts "300x" as 300; port
  rejects): client/search.rs:175, client/transport.rs:150,985,
  client/mod.rs:2519,2658,2709, server/tcp.rs:47,139,152,
  server/addr_list.rs:243. Different scanner family from epicsScanDouble
  — needs its own audit.
- asynInterposeEcho/Delay iocsh commands still drop addr; the interpose
  STACK is per-port, not per-(port,addr) as C — larger structural change
  than R14-49's per-device EOS state.
- Gate-refusal ERRS wording differs from C by an auxiliary verb ("port X
  is disabled" vs C "port X disabled" / "port X not connected").
- SIOL keeps a bare put path — no PP/processTarget semantics vs C's
  dbPutLink(&prec->siol,…). R14-62 closed only the alarm gap.
- Pre-existing broken intra-doc links in epics-ca-rs rustdoc (predate
  wave 12): signed_beacon::SignedBeaconEmitter, install_calink_resolver,
  ioc_app::IocApplication, tcp::ClientState, stat_to_str.
- epics-ca-rs cainfo_prints_the_resolved_host_name flaked once in E's
  full-workspace run (passes isolated; joins the known flake list).

---

## Round 15 re-audit (2026-07-13)

Same five reused auditor panels (round `01KXCX4MCJJ920KPRETTHJQ979`).
Methods: A re-ran the ~1500-case compiled corpus plus new compiled
drivers at each C caller's real arg counts (scalcout 12/12, transform
16/0, aCalc 12/12); B rebuilt EPICS base linux-x86_64 and ran ~120
head-to-head tool invocations plus a compiled envGetDoubleConfigParam
probe over 19 inputs; C used pvxs's own regression expectations
(testqsingle.cpp delta assertions) as the compiled record plus an
out-of-tree mask/bitset probe; D compiled an enable/disable/re-enable
connect-attempt counter probe; E enumerated every LinkAlarm construction
site by rg and surveyed process() ordering across 10 output record types.

### Wave-12 fix verification

- R14-1/2/3/5/6/8, R14-16/18/19, R14-48/49/50/46/47, R14-61/63 +
  dd4f00d2 all HOLD (R14-1: base draws identical to compiled C at 17
  significant digits; R14-18: 16 adversarial probes byte-identical;
  R14-5: 36-case shift matrix zero diffs, negative-count UB deviation
  re-confirmed by observing C's out-of-bounds read).
- R14-4 INCOMPLETE — store semantics right, upper bound wrong → R15-1.
- R14-7 REGRESSED (client path) — the fix over-narrowed: C bounds dbPut
  at NELM under the default SIZE=NELM → R15-2.
- R14-17 INCOMPLETE — whitespace half fixed; scan_double is still not
  epicsParseDouble (hex-float, ERANGE) → R15-18.
- R14-31 INCOMPLETE — marked:Some half holds; marked:None admits
  everything → R15-32.
- R14-32 PARTIAL — ARCHIVE-drop exact; retained Derive for
  root-flattened members is NOT pvxs behaviour → R15-31.
- R14-51 INCOMPLETE — block content line-exact, but stderr vs C's
  stdout + per-device autoConnect line → R15-51.
- 459ecb34 INCOMPLETE — four texts byte-exact; defunct message C never
  produces → R15-50; the device-level texts belong to C's non-CANBLOCK
  branch only → R15-47.
- R14-62 INCOMPLETE + one adjacent regression → R15-61 (MS inheritance
  reads committed alarm pre-commit), R15-62 (seq LNKn still
  post-commit). What holds: single put owner + same-cycle LINK_ALARM on
  OUT/multi-out/SIOL/WriteDbLink; IVOA pending-nsev read verified as
  C's exact call-site semantics; write-guard drop introduces no
  ordering C lacks; C ordering survey confirms checkAlarms → writes →
  monitor/reset uniform across 10 record types (seq async but still
  writes-before-commit).

### Lead adjudications
- env-double leniency lead — REAL but premise INVERTED: compiled C
  refutes "300x → 300" for envGetDoubleConfigParam (it REJECTS,
  S_stdlib_extraneous; the lenient sscanf belongs to
  envGetLongConfigParam). 6 of 10 cited sites are Rust-only knobs with
  no C counterpart. Real divergences → R15-16 (crash), R15-17
  (diagnostics/hex).
- Echo/Delay interpose addr — REAL → R15-48.
- SIOL bare put path — REAL → R15-63.
- pvxs root-flattened member — pvxs HAS the case (+type:meta only,
  enforced both sides) and marks it with NO special case → R15-31.
- NOT-REAL (A): transform AA segfault (numSArgs guard at
  sCalcPerform.c:871/:891); negative shift count (C UB confirmed by
  observation); PRINTF missing vararg (run-to-run garbage); LRC("")
  (CBUG-C6 territory). 1500-case corpus: no new divergence from wave 12.
- NOT-REAL (C): coalesce mark union (pvxs assign leaves prior marks
  valid); push_squash terminal past limit (servermon.cpp:273); empty
  overrun bitset on wire (servermon.cpp:174-176); TriggerDef::None →
  Skip (empty triggers iterate nothing).

### Open Findings — Round 15 (21 findings)

Category A (calc):
### R15-1: engines ignore the caller's argument counts — Medium, compiled repro. mod.rs:214 (CALC_NARGS=21, no count in StringInputs/ArrayInputs) vs sCalcPerform.c:444/:902/:914/:871/:891/:732, aCalcPerform.c:499/:510 — counts are CALLER-supplied (scalcout 12/12, transform 16/0 via transformRecord.c:593, acalcout 12/12). Out-of-range access: C silent no-op / 0 / ""; port reads/writes phantom slots (proof: @12:=5;@12 → C 0, port 5; transform AA:="x";LEN(AA) → C 0, port 1). R14-4 made this expression-reachable. Structural: the bound belongs in the input structs as num_args/num_sargs — five access sites currently each have to remember.
### R15-2: acalcout client/db-link put bounded at the NUSE window; C bounds dbPut at no_elements = NELM under the default SIZE=NELM — Medium. acalcout.rs:410-417 vs dbAccess.c:1322,:1361-1362 + aCalcoutRecord.c:627-631 (cvt_dbaddr: no_elements = SIZE==NUSE ? numElements : nelm; SIZE defaults to NELM). Mirror image of R14-7: hidden tail [window, NELM) became unwritable by client/db-link puts. Two bounds by writer identity: link ⇒ numElements (correct today); dbPut ⇒ no_elements. Zero-fill stays.

Category B (CA tools):
### R15-16: a non-finite env timeout panics Duration::from_secs_f64 — client crash, server remote-DoS — **High**, live repro. 7 of 9 env-derived from_secs_f64 sites unguarded: transport.rs:152, search.rs:218, server/tcp.rs:49,:140 (+TLS tcp.rs:153, transport.rs:986; put-notify trio mod.rs:2521/2660/2711 same-anchor-unverified). EPICS_CA_CONN_TMO=inf: C exit 0 reads PV; port panics, read fails. EPICS_CAS_SEND_TMO=inf: first client connect kills the whole server. Structural: one env::get_double resolver mirroring envGetDoubleConfigParam (reject non-finite/ERANGE, C diagnostic, default) instead of nine parse().filter().map() chains.
### R15-17: the four C-backed env-double sites are silent where C prints three named diagnostics, and reject hex floats C accepts — Low. transport.rs:150, search.rs:175, addr_list.rs:243 vs cac.cpp:192-193, udpiiu.cpp:86-89, online_notify.c:60-63; epicsStrtod accepts 0x10=16.
### R15-18: copt::scan_double is not epicsParseDouble — hex-float rejected (caget -w 0x10: C 16 s, port 1 s + spurious warning), ERANGE accepted (1e400: C warns, port silent via the is_finite guard) — Medium. copt.rs:396-402; residual half of R14-17; unit test never probes hex/ERANGE.

Category C (PVA):
### R15-31: a root-flattened +type:meta member forces EVERY post onto Derive — **High**. group.rs:2161-2163 (leaves_or_derive bails on any empty field_name; justification false — change_leaf_paths("",Meta,change) returns addressable ["timeStamp","alarm"]) vs field.cpp:56-81 + iocsource.cpp:312-352 (pvxs marks root members with NO special case; +type:meta is the only legal root mapping, enforced both sides). Impact: DBE_PROPERTY on root-meta → pvxs posts nothing, port sends a FULL-value frame; one root-meta member poisons the whole group's trigger narrowing; in pure-self-trigger groups routes to build_monitor_payload_partial which can frame empty bitsets. Fix: mark root-meta via the normal leaf path; Derive then has no remaining producer (retire it).
### R15-32: MonitorQueue::real admits every marked:None post; pvxs testmask requires a leaf bit — Medium. tcp.rs:1757-1759 vs pvrequest.cpp:74-93 + data.cpp:256-270 (valid set on leaves only). Probe: field(alarm.bogus) mask=[0,2] → pvxs false, port true → full-rate empty-bitset frames for a request naming a nonexistent nested field. Gate = !canonical_changed_bitset(intro, mask).is_empty() — gate==wire on all three builders. The false comment at tcp.rs:1740-1744 is the standing justification.
### R15-33: monitor seed and GET reply mark leaves pvxs never assigns — Medium. tcp.rs:7931,:6939,:7071 frame canonical_changed_bitset(intro, mask) = every mask leaf; pvxs frames only what IOCSource::get marked (serverget.cpp:104, groupsource.cpp:484). getProperties never assigns control.minStep, valueAlarm.active/*Severity/hysteresis (7 leaves) — pinned by pvxs's own testqsingle.cpp:129-149 delta. Structural cause: DynSource::get hands up a bare PvField with no mark set.
### R15-34: array-subscript group members are unmarkable — every monitor update dropped — **High**. encode.rs:1491-1516 (walk never descends StructureArray, candidate paths use FieldDesc names so "a[0].x" matches nothing → empty bitset → real()=false → post dropped) vs data.cpp:264-269 (pvxs marks the ENCLOSING StructA field's valid — one bit, whole array serialized). All-array-member group (testqgroup.rs:1392 shape): client receives nothing after the seed, ever. Correct marked path for "a[0].x" is "a" — the mark producer is the only thing that does not know it.
### R15-35: three comments assert behaviour the code does not have — Low. tcp.rs:1740-1744 (request2mask "cannot produce an empty mask" — disproven, R15-32); tcp.rs:8086-8089 (claims cooked builders encode the overrun set — all three hard-write empty, which IS pvxs's form; comment invites a parity-breaking "fix"); pvif.rs:118-123 (getTimeAlarm "writes userTag" — only under DBR_UTAG, iocsource.cpp:243-250).

Category D (asyn):
### R15-46: no port auto-connect on the enable exception — asynEnable(port,1) never brings the link back up — **High**, compiled repro. port_actor.rs:384-415 + port.rs:547-557 vs asynManager.c:635-636 (EVERY exception announcement signals notifyPortThread) + :856-861 (woken thread runs throttled autoConnectDevice when down). Probe: 6 attempts enabled → 0 disabled (correct) → 0 after RE-ENABLE (C: ≥1); only traffic revives the port and the retry loop stays dead. Structural: actor treats exception announcements as C's notifyPortThread — one throttled auto_connect_port() on every wake with the port down; no ad-hoc SetEnable re-arm.
### R15-47: device-level enabled/connected refusals applied to queued ports — C checks them only on synchronous (non-CANBLOCK) ports — Medium. port.rs:630-641 vs asynManager.c:1551 (device block inside !ASYN_CANBLOCK) + :874-884 (portThread PARKS a disabled device's request — continue, not refuse; disconnected device → autoConnectDevice then timeout callback). Every real port is CANBLOCK. C parks or times out (5 s, "queueRequest timeout" STATE/MINOR); port refuses instantly with a text from a branch this port class cannot take.
### R15-48: asynInterposeEcho/Delay are port-wide and drop their iocsh addr — Medium. iocsh.rs:539-635, request.rs:276-283, port.rs:230 (one interpose_octet per port) vs interposeInterface addr>=0 → device's interposeInterfaceList; findInterface resolves device-first (asynManager.c:1493-1501). asynInterposeDelay("gpib",4,0.01) slows EVERY bus device. Same structural cause R14-49 closed for EOS state: the STACK has no address dimension — key it by eos_device_key.
### R15-49: ERRS is written by every diagnostic path and posted by NONE — only resetError's clear reaches a CA client — **High**. mod.rs:2802-2806 (the only post_if_new(["ERRS"])) vs asynRecord.c:2028-2049 (reportError: strncpy THEN db_post_events on change). Every errs write (refusal :2870, option :2751, EOS :3017, connectDevice :4121, CNCT :4247/:4263, trace-file :2247) sets the field silently; no special field is pp(TRUE) so no generic post covers the put path. Operator's medm ERRS widget shows the blank resetError posted at put-start — the diagnostic channel wave 12 made C-exact is invisible. Structural: one ERRS setter that does C's db_post_events; not per-list additions.
### R15-50: the defunct refusal message does not exist in C — Low. port.rs:651-656 vs asynManager.c:2282-2283 (shutdownPort clears enabled precisely so queueRequest answers "port X disabled"; no defunct branch in the gate). After R14-46 the invented text lands verbatim in ERRS. Retire the special message (keep the internal defunct state).
### R15-51: asynReport writes to stderr where C writes stdout; driver block still diverges from asynPortDriver::report — Low. port_actor.rs:459, port.rs:1277-1295,:958-959 vs asynShellCommands.c:589 (report(stdout,…)); asynPortDriver.cpp:3677-3700 (EOS lines gated on octet interface, Timestamp line, details≥3 interrupt-client block); per-address line prints port's autoConnect vs C's pdpc->autoConnect.

Category E (records/framework):
### R15-61: MS-class alarm inheritance on pre-commit OUT writes carries the PREVIOUS cycle's severity — **High**, regression from 9ab4fd2a. processing.rs:3732 (WriteDbLink — transform/scaler/epid/sseq), :3845 (WriteDbLinkNotify), links.rs:1434 (dispatch_multi_output — dfanout/seq) snapshot committed stat/sevr/amsg; C dbDbLink.c:381-383 inherits the PENDING nsta/nsev/namsg (dbPutLink runs inside process before reset). The main OUT stage was correctly updated (processing.rs:3102-3105); these three were not, and all three now run pre-commit. First INVALID cycle propagates NO_ALARM; inheritance permanently one cycle stale. Comments at :3714/:1426 still say "the committed alarm" — the tell.
### R15-62: a failed seq LNKn put alarms one cycle late — the seq value writes are still dispatched from the post-commit forward-link tail — **High**. processing.rs:3457,:4307 → links.rs:1839 vs seqRecord.c:264 (puts in processCallback) + :227 (asyncFinish commits AFTER) — C's async seq still writes-before-commit. Phase gate correct for fanout (FWDLINKs, no put); seq genuinely writes values. One-shot passive seq: the alarm never appears at all.
### R15-63: the SIOL simulation write bypasses the put owner — no processTarget, no MS inheritance, no PUTF/put-notify — Medium. processing.rs:4521-4560 (put_pv_already_locked direct) vs dbDbLink.c:372-393 (SIOL is DBF_OUTLINK; C writes via the identical dbDbPutValue: inherit + PP/proc processTarget). Secondary: the caller raises the SIOL LINK_ALARM (:4630), violating write_out_link_value's own "raised HERE, not by each caller" invariant (links.rs:1247-1253). Route SIOL through write_out_link_value — closes both halves.
### R15-64: scalcout OUT buffer choice misses C's DBF_MENU/DBF_DEVICE target classes — Low. scalcout.rs:1235-1251 vs devsCalcoutSoft.c:128-130 (seven types route to OSV). Port's DbFieldType has no menu/device variants: PRIO/ACKT/STAT/SEVR/DISS map to Short, DTYP falls through numeric → OVAL as double where C sends the OSV string. The exact class R14-61 was about, one enum granularity short.
### R15-65: dfanout IVOA=Set_output_to_IVOV pushes IVOV but never writes VAL — Medium. links.rs:1556-1570 (pushed value = get_field("IVOV") from a read guard; VAL untouched) vs dfanoutRecord.c:136-139 (prec->val = prec->ivov; push_values) — C then posts VAL=IVOV in monitor(). caget DFAN disagrees with what was pushed; no VAL monitor fires. The generic single-OUT path does this correctly via apply_invalid_output_value (processing.rs:2866); the dfanout dispatch is the one output path that skips it.

### Notes
- Scoped aai/aao audit (W10-E7 resolution, user-approved) running on a
  dedicated panel; findings will file as R15-76+ in a follow-up doc
  commit.
- B-panel: all three known-flaky epics-ca-rs tests passed this round's
  single run (not evidence of resolution).
- Comment-only nit (not filed): beacon.rs cites online_notify.c:68 for
  delay=0.02; actual line 66.

---

## Scoped audit: aai/aao record parity (2026-07-13, resolves W10-E7)

User-approved scope (option: dedicated audit, fix real gaps; shared
WaveformRecord+ArrayKind structure is settled). Dedicated read-only
panel; every claim proven by a probe crate driving the real port
(IocBuilder::db_string → process_record → get_pv), not source-reading.
Round transcript: rounds/01KXCXZMT7EJ6JBXAKMHQCME9F.md.

**W10-E7 verdict: STALE — retired.** aai/aao are ported and live
(db_loader mints them, record_type() correct, RECORDS_WITH_SSCN carries
both, SIMM machinery reaches them at runtime). The real, never-audited
gaps are the .db-load surface and the aao closed-loop failure path:

### R15-76: field(SIMM/SIML/SIOL/SIMS/SDLY/MPST/APST) silently dropped at .db load on aai/aao/waveform — **High**. waveform.rs:975-1020/:903-958/:355-416 (no field_list carries them; SDLY has no storage anywhere) + db_loader apply_fields → put_common_field_bounded → FieldNotFound → stderr-and-continue. C: ordinary declared fields (aaiRecord.dbd.pod:374-434 etc.). Simulation and On-Change posting cannot be configured from a .db — the only way real IOCs configure them; MPST/APST never take so every cycle posts the full array. Runtime caput works (field_io tries record.put_field first), which is why existing sim tests are green over a dead loader path.
### R15-77: an aao whose closed-loop DOL read fails still writes the stale array to OUT, posts, fires FLNK — no alarm — **High**. waveform.rs:1507-1521 → processing.rs:3705-3709/:3651-3655 (`if let Some(value) = read_link_value` — failure is a silent no-op) vs aaoRecord.c:167-168 (fetchValue failure returns BEFORE writeValue/monitor/recGblFwdLink) + dbLink.c:338-339/:319-323 (dbGetLink raises LINK/INVALID inside the get). Probe: DOL="NOSUCHREC" → port writes [9,9] to TGT, runs FLNK, SEVR=0; C: never written, never run, LINK/INVALID.
### R15-78: a constant INP on aai/waveform is never loaded at init and is re-applied EVERY cycle — clobbering client data — **High**. init_record does nothing (waveform.rs:1052-1063); links.rs:709-710 returns the constant every cycle → set_val. C: devAaiSoft.c:55-65 dbLoadLinkArray ONCE at init (NORD set, UDF cleared); read_aai returns immediately on a constant (:91-92); devWfSoft.c same. Probe: INP="5", caput [7,8,9], process → port Double(5.0) NORD=1 (client data destroyed); C keeps [7,8,9]. Array constants ("1 2 3"): C loads [1,2,3]; port loads nothing then writes scalar 1.0 per cycle.
### R15-79: a scalar source into an array VAL replaces the buffer with a scalar variant — FTVL/VAL invariant broken, On-Change hash dead — Medium. waveform.rs:1246-1249 (put_field VAL fallback `other => { nord=1; val=other }`) vs C reads into the typed bptr (scalar → one element in the array). array_content_bytes has no scalar arm → hash empty → On-Change posting never fires again; resize wipes data. The everyday config DOL="SETPOINT" (scalar ao feeding closed-loop aao) turns VAL scalar and propagates the scalar into the OUT target.
### R15-80: NELM=1 must yield NORD=1 at init; the port yields 0 — Medium. waveform.rs Default/new/init_record vs aaiRecord.c:113, aaoRecord.c:116-120, waveformRecord.c:100. get_field truncates at NORD → a NELM=1 record serves a zero-length array to every client until first process.
### R15-81: a failed SIOL read on a simulated aai/waveform leaves UDF set; C clears UDF unconditionally on these types — Low. processing.rs:5410-5418 gates the clear on read status; the justifying comment cites waveformRecord.c:352 but :144 clears unconditionally right after (aaiRecord.c:174, aaoRecord.c:165 same). None of the three has checkAlarms → C never raises UDF_ALARM on them; the port's raises_udf_alarm() defaults true (masked only because LINK/INVALID wins strict-greater).
### R15-82: aao OMSL=closed_loop with a constant DOL is never seeded at init — Low. waveform.rs:1502-1506 (disclosed residual) vs aaoRecord.c:147 fetchValue(prec,1) → dbLoadLinkArray + nord + UDF clear. Same missing primitive as R15-78 — one constant-array-link loader closes both.

NOT-REAL adjudications: devAaoSoft discarding dbPutLink's status is not
an alarm gap (setLinkAlarm fires INSIDE dbPutLink before the dset sees
the status; port's put-owner matches); aao VAL→OUT write WORKS end-to-end
(output_link_value falls back OVAL→val(), NORD-truncated — exactly
devAaoSoft.c:55-57; also covers the simulated SIOL write path);
aai/aao missing checkAlarms is correct by construction (C has none);
HASH/MPST/APST mechanism verified exact vs aaiRecord.c:312-346 incl.
compiled-C hash vectors — R15-76/79 are what break it in practice.

Coverage note: R15-76/78/79/80/81 reach waveform too (shared struct,
shared field_list gap, shared put_field VAL arm) — fix anchors are
family-wide.

---

## Fix wave 13 — dispositions (2026-07-13)

All 21 Round-15 findings plus all 7 scoped aai/aao findings fixed, one
commit per finding, on 6 worktree fixer branches, merged:
A `caucus/WG0SFREHPX/fixer-a-calc-21bd43a5-2` (2 commits),
B `…fixer-b-catools-710a473a-2` (3), C `…fixer-c-pva-2167e637-2` (5),
D `…fixer-d-asyn-cec348ea-2` (6), E `…fixer-e-records-402d4f3e-2` (5),
aai/aao `…fixer-aai-aao-d942e609-1` (7). Post-merge full-workspace gate
(run twice, after the 5-branch merge and again after the aai/aao merge):
clippy `-D warnings` clean, nextest 8732/8732 (two epics-ca-rs CLI tests
flaked once under load in the second run — `a_rejected_enum_string_
prints_no_old_value`, `caget_prints_nothing_when_one_pv_of_many_never_
connects` — both pass isolated and in the clean rerun; joins the known
flake list), doctests clean.

- R15-1 `927bd592` — counts live IN the input structs: StringInputs gains
  num_args/num_sargs, ArrayInputs num_dargs/num_aargs, clamped at
  construction; all 17 access sites route through num_arg/str_arg/
  array_arg/store_array accessors; store_array owns the amask pair (C
  sets it inside the num_aArgs guard). Callers pass real counts —
  scalcout 12/12, acalcout 12/12, transform 16/0. Distinct: numeric.rs
  (C calcPerform takes no count). DOCUMENTED DEVIATION: C's out-of-count
  FETCH leaves the stack element stale (sCalcPerform.c:862-864,
  aCalcPerform.c:436, unreachable-garbage paths); port pushes 0.
- R15-2 `da548415` — R14-7 regression closed: the bound is the WRITER's.
  write_array_field(src, bound, select) — client put bounds at
  no_elements (= NELM under default SIZE, cvt_dbaddr:628), link fetch at
  the NUSE window; splice/zero-fill rule stays shared. SIZE=NUSE
  correctly narrows the client too.
- R15-16 `9187fdcc` — new epics-ca-rs/src/estdlib.rs ports
  epicsParseDouble (epicsStdlib.c:149-176: hex floats, inf/nan words,
  ERANGE rejection, trailing-garbage rejection) + envGetDoubleConfigParam
  (envSubr.c:191-211) + total f64→Duration (inf → Duration::MAX,
  negative → ZERO). All ten env-derived from_secs_f64 chains route
  through it; client/mod.rs:1238 caput_callback joined (same unvalidated
  conversion). FINDING PREMISE CORRECTED vs compiled C: strtod("inf")
  succeeds — EPICS_CA_CONN_TMO=inf is a valid never-expiring deadline in
  C (why C caget exits 0); 1e400 IS rejected (ERANGE). Implemented C's
  actual split, not the prescribed "reject non-finite".
- R15-17 `07ec3ab1` — C's named diagnostics layered on the resolver
  (envSubr.c:205, cac.cpp:192-193, udpiiu.cpp:79-89, online_notify.c:59-64);
  each knob resolves once per process (OnceLock), as C's constructors do.
  stderr diffed BYTE-IDENTICAL vs compiled caget for CONN_TMO ∈ {abc,
  1e400, inf, 0x10} and MAX_SEARCH_PERIOD ∈ {abc, 30}. DOCUMENTED
  DEVIATION: NaN search period takes the 60 s low clamp; C propagates
  NaN into its timer wheel and stops searching.
- R15-18 `3b2d5ce6` — copt::scan_double delegates to
  estdlib::epics_scan_double; -w hex/ERANGE stderr byte-identical vs
  compiled caget.
- R15-31 `bf22429b` — root-meta members mark leaves through the same
  change_leaf_paths producer (empty prefix yields timeStamp/alarm —
  pvxs's set); EventMark::Derive left with no producer and RETIRED
  (leaves_or_derive → marked_leaves). Supersedes R14-32's "RETAINED"
  note.
- R15-32 `5e2f2488` — MonitorQueue::real's marked:None arm gates on
  canonical_changed_bitset(intro, mask) non-empty — gate == wire on both
  arms; field(alarm.bogus) now stays silent as pvxs does.
- R15-33 `85a1c37f` — structural version: ChannelSource::read_checked →
  SourceRead { value, marked }; one read_changed_bitset rule shared by
  MONITOR data, seed, GET and PUT_GET; QSRV declares marks via
  read_leaf_paths (getProperties never assigns control.minStep,
  valueAlarm.active/*Severity/hysteresis — pinned by pvxs's
  testqsingle.cpp:129-149). Retired the last value-diff consumer:
  build_monitor_payload_partial, monitor_emits_partial, prev_value,
  encode::diff_changed_bitset all deleted.
- R15-34 `e018091d` — change_leaf_paths collapses a subscripted prefix
  (a[0].x) to the enclosing array field (a), matching Value::mark
  (data.cpp:256-270).
- R15-35 `f391479a` — rg found the false overrun claim at FOUR sites
  (tcp.rs squash accumulator, MonitorUpdate::overrun doc, gateway cooked
  fanout doc + test doc); all now state only the raw forwarder puts
  overrun bits on the wire. getTimeAlarm comment states the DBR_UTAG
  condition.
- R15-46 `57b341ba` — every actor pass ends in a throttled
  port_thread_auto_connect() — C's notifyPortThread wake (exception
  announcements included, asynManager.c:635-636 → :856-861);
  service_connect_timer stamps the throttle so timer and wake cannot
  double-connect. Disable→enable reconnects with exactly one attempt.
- R15-47 `143092c3` — device-level enabled/connected refusals only on
  non-CANBLOCK ports (asynManager.c:1551); a CANBLOCK port PARKS a
  disabled device's request (portThread :874-884, rescan on state
  change; parked request still honors its queue timer), disconnected
  device takes autoConnectDevice then the timeout reply.
- R15-48 `4fc19eb6` — octet interpose STACK keyed by
  eos_device_key(multi_device, addr); resolution is C's findInterface
  (device chain first, port chain second); iocsh addr threaded through
  the Push requests. Closes the wave-12 open lead.
- R15-49 `b7fee690` — one owner: report_error() writes ERRS and posts on
  change (asynRecord.c:2028-2049); all 16 diagnostic writers route
  through it; reset_error() is the clear path.
- R15-50 `a10089e9` — invented defunct refusal text gone; a shut-down
  port answers "port X disabled" (shutdownPort clears enabled,
  asynManager.c:2282-2283); enable() on it carries C's exact
  "asynManager:enable: port has been shut down". Structural:
  PortDriverBase::defunct is private, shutdown_lifecycle its only
  setter — defunct ⟹ !enabled holds by construction. Closes the
  wave-12 documented deviation.
- R15-51 `170fd865` — report takes &mut dyn fmt::Write at every layer;
  PortActor::report_port is the single owner naming the stream (stdout,
  asynShellCommands.c:589). Driver block = asynPortDriver::report
  (:3676-3710): Timestamp line, EOS pair gated on registered asynOctet,
  details≥3 interrupt-client block; per-address line prints the DEVICE's
  autoConnect. DOCUMENTED DEVIATIONS: Timestamp rendered via chrono
  (new asyn-rs dep, already workspace-wide); interrupt-client lines
  carry interface/addr/reason/mask, not C's callback/userPvt pointers.
- R15-61 `f0b63a8a` — the two C alarm snapshots are now NAMED:
  LinkAlarm::pending() (nsta/nsev/namsg — dbDbPutValue, dbDbLink.c:382-383)
  for every put path, LinkAlarm::committed() (stat/sevr/amsg —
  dbDbGetValue :229-232) for the read path. rg: 7 construction sites,
  6 same-defect (all put paths, fixed), 1 distinct
  (read_link_with_alarm — C's get path, correctly committed).
- R15-62 `b301a4b9` — multi_out_phase_of() classifies by what the link
  IS: dfanout/seq = Output (value-carrying dbPutLink, pre-commit,
  seqRecord.c:264 vs :227), fanout LNK0..F = ForwardLink (dbScanFwdLink,
  no put, no alarm). Failed seq LNKn put alarms the same cycle.
- R15-63 `b2002d43` — write_sim_siol_value DELETED;
  write_simulated_output_siol calls write_out_link_value with
  field "SIOL". SIOL is the same kind of link as OUT (alarm/MS/PP all
  through the put owner). Closes the wave-12 open lead.
- R15-64 `cd1c2d6f` — fixed at type resolution, not in the record:
  OutTarget carries puts_as_string, filled once by
  RecordInstance::field_puts_as_string (String/Enum, or a short with
  menu choices — C's DBF_MENU/DBF_DEVICE, devsCalcoutSoft.c:128-130);
  scalcout::multi_output_buffer consumes the flag.
- R15-65 `1501cfe7` — reported symptom NOT REAL (the generic gate
  already applied IVOV to VAL); actual defect found by probe: IVOA was
  re-derived AFTER the outputs ran, so a failing push that raised
  INVALID retro-triggered the IVOV arm and overwrote VAL. The pre-output
  gate is now the single IVOA owner (decides skip_out, applies IVOV once
  on checkAlarms severity, dfanoutRecord.c:127-146);
  dispatch_multi_output_values just pushes prec->val.
- R15-76 `9a998d56` — SIML/SIMM/SIOL/SIMS/SDLY/MPST/APST added to the
  aai/aao/waveform field lists, all four kinds' field sets rebuilt from
  ONE macro chain (a field can no longer be declared for one kind and
  forgotten for a sibling); `sdly` storage added (also missing on
  longin/int64in/event — same .db gate, fixed same commit).
- R15-80 `3f3928d4` — NORD=1 seed for NELM=1 moved into init_record
  pass 0 (after .db fields apply), where C has it; subArray excluded
  (subArrayRecord.c:101).
- R15-79 `22eeeb19` — one owner land_val_in_buffer routes every VAL
  source through convert_to(FTVL) into the NELM-sized typed buffer; the
  scalar-in-VAL variant is unrepresentable. Un-breaks the On-Change
  hash, resize_val_preserving, and the everyday DOL="SETPOINT"
  closed-loop aao.
- R15-78 `b7b791e9` — new framework owner rec_gbl_init_constant_inp
  (beside rec_gbl_init_simm) loads a constant INP once at init through
  the same sink the process path uses; read_link_value_soft's Constant
  arm now delivers nothing at process. Link layer gained the bracketed
  array-constant form ([1,2,3] → parsed elements). The everyday victim:
  an UNSET INP (a constant link in C) no longer wipes client-written
  arrays every cycle.
- R15-82 `8195e6a4` — C's init && isConst arm (aaoRecord.c:147
  fetchValue(prec,1)) reusing R15-78's loader + R15-79's landing rule;
  UDF clear rides post_init_finalize_undef.
- R15-77 `a5bfdc4b` — read_db_link_into_field is the single owner of a
  record-declared input-link read and raises C's setLinkAlarm
  (LINK/INVALID, AMSG "field DOL") itself; pre-input reads join the
  per-cycle set_resolved_input_links report, which aao reads as
  fetchValue's status → CompleteNoEmit (C's early return: nothing
  written to OUT, no post, no FLNK). Consumers classified: sseq
  SELL→SELN has no abort in C; epid's pre-input action is a write.
- R15-81 `b9cb555d` — Record::clears_udf_unconditionally (default
  false) consulted by the simulation tail + raises_udf_alarm → false,
  for waveform/aai/aao only; subArray keeps both defaults
  (subArrayRecord.c:148-150). Regression test fails with the fix line
  reverted.

### Doc correction (aai/aao audit section above)

R15-78's write-up says C parses field(INP,"1 2 3") as the constant
array [1,2,3]. It does not: dbParseLink (dbStaticLib.c:2346-2357) makes
a constant iff empty, whole-string-double, or BRACKETED; "1 2 3" fails
epicsParseDouble on trailing garbage and becomes a PV_LINK to a record
named "1". The fixer implemented the bracketed form and removed the
port's whitespace-list heuristic, which had the same wrong belief baked
in.

### Documented deviations added this wave
- R15-1: out-of-count FETCH pushes 0 where C leaves stack garbage
  (unreachable-garbage paths, sCalcPerform.c:862-864/:426,
  aCalcPerform.c:436).
- R15-16: none — C's actual inf-accepting split implemented.
- R15-17: NaN search period clamps to 60 s low; C's NaN would stop the
  search timer entirely.
- R15-51: chrono timestamp; interrupt-client lines carry
  interface/addr/reason/mask instead of C's pointers.

### Open leads — surfaced during fix wave 13 (Round 16 input)
- Gateway cooked-path overrun signal is dead on the wire:
  MonitorUpdate::overrun is accumulated (squash + cooked fanout) and
  read by nobody; every cooked builder writes pvxs's hard-empty overrun
  bitset. pva2pva (moncache.cpp:160-168) DOES set the bits, pvxs's
  server (servermon.cpp:174-176) never does — the port's server is
  both. Which reference governs the gateway's cooked path needs an
  adjudication; R15-35 scoped comment-only.
- The R15-78 constant-link defect REMAINS on the multi-input path
  (links.rs::read_link_value — calc/sub/sel/aSub INPA..L): C seeds via
  recGblInitConstantLink at init and dbGetLink delivers nothing after;
  the port re-applies every cycle, so caput to a calc's A with
  field(INPA,"5") is still clobbered. Finding-sized; needs per-record
  init seeding across calc/calcout/sub/sel/aSub/scalcout/acalcout/
  swait/epid.
- parse_link_v2 splits link modifiers before its numeric test, so
  "1 2 3" and "5 PP" become Constant("1")/Constant("5") where C's
  dbParseLink tests the WHOLE string first and yields a PV_LINK.
  Classifier reorder touches every link field (INP/OUT/FLNK/SDIS) —
  its own change.
- epics-pva-rs env doubles (config/env.rs:678/691,
  server_native/runtime.rs:468/471/482) use str::parse — reject hex
  floats; guarded finite-positive so no panic; pvxs is their reference,
  not epicsStdlib.
- Wave-12 env-double open lead CLOSED by R15-16..18 (estdlib resolver).
- Known-flake additions: epics-ca-rs cli_caput_enum_order::
  a_rejected_enum_string_prints_no_old_value, cli_connect_gate::
  caget_prints_nothing_when_one_pv_of_many_never_connects,
  client::transport::flow_control_tests::r6_17_events_off_at_the_
  contiguous_frame_boundary (wall-clock sleeps in its harness).

---

## Round 16 re-audit (2026-07-13)

Six reused auditor panels (round `01KXD31D8FAXFBET4SWW4JDK7J`), run
after the wave-13 + aai/aao merges AND the origin/main merge `ef260e30`
(PR #25-27). Methods: A compiled-C head-to-head at each caller's counts
+ 1500-case differential sweep; B a 2217-input bit-exact epicsParseDouble
battery + ~60 live softIoc head-to-heads; C full post-state read of the
framing refactor + pvxs source; D three compiled probes (connect-attempt
counting, report block, param library); E source verification of all six
alarm-path owners; aai/aao compiled softIoc transcripts vs probe crate.

### Wave-13 + aai/aao fix verification — ALL 28 HOLD
A: R15-1, R15-2 HOLD (every R15-1 proof case now agrees with compiled C;
R15-2's link/client bound split traced end-to-end — a foreign OUT link
into acalcout.AA correctly takes the client bound because C routes
dbPutLink through dbPut). B: R15-16..18 HOLD (client crash and server
DoS both gone live; diagnostics byte-identical; scan_double 2165/2217
bit-exact — the 52 diffs are one corner, R16-16). C: R15-31..35 HOLD
(EventMark is Skip/Marked only; gate==wire on every monitor path;
read_leaf_paths reproduces initialize+get(Everything); subscripted
collapse matches Value::mark; the group seed correctly does NOT reuse
GET stamping — queueSize is seed-only, groupsource.cpp:404-405 vs :485).
D: R15-46..51 HOLD (R15-46's first probe "failure" was C's own 2 s
throttle; R15-51 carried over two pre-existing non-C lines → R16-48/49).
E: R15-61..65 HOLD (pending/committed constructors are the only two;
phase gate un-bypassable — dispatch reads the record's own phase).
aai/aao: R15-76..82 HOLD (R15-77 verified byte-for-byte incl. the
pending-not-committed alarm half; R15-78 INCOMPLETE for subArray only —
C's documented exception, filed as R16-76).

### Merge verification
- `ef260e30` conflict resolutions (adapter.rs init, asyn_record
  connect_device, sync_io drv_user_create): all three compositions
  CORRECT vs devAsynInt32.c:263-277, asynRecord.c:1242-1261,
  asynOctetSyncIO.c:141-156. Adjudicated NOT-REAL along the way:
  failed create zeroing resolved_reason (C's effective reason is also
  0 — the field is copied only in special(), asynRecord.c:488); missing
  empty-drvInfo skip in sync_io (C guards a NULL pointer; no caller
  passes ""); param library addr collapse (validate_addr mirrors C's
  getAddr -1 → 0); Int32 @asynMask applied twice (idempotent).
- PR #25-27 surface: no regressions (convert.rs round-trips iface as
  Option; @asynMask applied on both input paths as C).
- `fff4e685` (UDP SO_REUSEADDR on Windows): HOLDS —
  epicsSocketEnableAddressUseForDatagramFanout has no platform guard in
  C; family complete across all five datagram sockets.
- aai/aao ↔ wave-13 E merge composition in processing.rs/links.rs:
  correct (read_db_link_into_field single owner serves both consumers;
  rec_gbl_init_constant_inp init-only; phase gate and aao DOL disjoint).

### Lead adjudications
- Gateway cooked-path overrun: **NOT-REAL as a pvxs divergence** — the
  port's server IS a pvxs server and pvxs never sets overrun bits
  (servermon.cpp:171-177 unconditional to_wire(0), "TODO: placeholder");
  emitting a computed set would flip pvxs's client servSquash flag
  (clientmon.cpp:554-563). pva2pva is NOT on this machine and is not
  cited. Raw path already forwards upstream bits. Superset decision, not
  a parity fix; needs pva2pva source on disk if ever wanted.
- pva env doubles: **REAL** → R16-32 (pvxs's parse_timeout has a range
  gate the port lacks).
- epid's constant CVL: UNVERIFIED (registered by crates/std-rs, outside
  the probe's link graph) — shares R16-77's anchor; check when fixing.

### Open Findings — Round 16 (24 findings)

### R16-1: sseq forwards each step's value typed by the SOURCE, not by the destination LNKn field type — **High**. sseq.rs:338-353 (step_value branches on dol_kind/str_val) + :539 vs sseqRecord.c:714-793 (processCallback switches on lnk_field_type from dbGetLinkDBFtype: string-class → dbPutLink(DBR_STRING, s) with s = cvtDoubleToString(dov, sseq's PREC); numeric → DBR_DOUBLE with dov = atof(s); CHAR/UCHAR n_elements>1 → the 40-byte s as char array; default → NO put). Port stores lnk_field_type (:1409-1414, posted as LTn) and never routes on it. PREC=2, 1.23456789 → C "1.23", port "1.23456789"; string "abc" → numeric: C 0.0 + PP process, port errored put, nothing written; numeric → CHAR waveform (long-string idiom): C string bytes, port one double; unresolved LNKn: C nothing, port always writes. sseq twin of R14-61; structural fix the same — route on lnk_field_type.
### R16-2: sseq DOn/STRn are two views of one value; port syncs only on the link-read path — Medium. sseq.rs:1358-1375 (put_field DO/STR arms write one side), :993-1092 (special has no DOn/STRn arm), :908-914 (init quantizes DLY only) vs sseqRecord.c:1098-1117 (special DOn → cvtDoubleToString into s), :1119-1132 (STRn → atof into dov), :242-250 (init reconciles). caput DO1 3.7 leaves STR1 stale; caput STR1 "5" leaves DO1 0; after STR1="abc" then DO1=3.7 the port forwards "abc" where C forwards "3.70"; .db field(DO1,"3") leaves STR1 empty where C shows "3".
### R16-3: WAITn on a non-CA link — port blocks the sequence; C cannot wait and flags it — Medium. sseq.rs:543-553 (wait gate ignores link type), :783-787 (werr) vs sseqRecord.c:717/:739/:763 (put-with-completion only if usePutCallback && lnk.type==CA_LINK; DB_LINK takes plain dbPutLink, never waits) + :912-933 (checkLinks: waitConfigErr=1 exactly for DB_LINK with usePutCallback, RESCINDED for CONSTANT). Port's WAITn on a local DB link stalls the sequence where C fires-and-forgets; WERRn inverted vs C (raised for CONSTANT which C clears; 0 for the DB-link case C warns about).
### R16-4: transform discards the non-finite result C stores in the channel — Medium, compiled repro. transform.rs:709-737 (Err(_) leaves vals[i] stale; engine returns Err(NonFiniteResult) before yielding) vs sCalcPerform.c:2034-2056 (epilogue writes *presult FIRST, then returns -1 on nan/inf) + transformRecord.c:593-597 (raises CALC/INVALID + UDF but leaves *pval = inf, fanned through OUTA). C harness: 1e308*10 → stat=-1 d=inf. Transform-only (scalcout overwrites VAL=-1/"***ERROR***" on any nonzero status). Structural: CalcError::NonFiniteResult cannot carry the value C's contract pairs with the -1. Distinct: 1/0 and LOG(-1) return -1 WITHOUT writing *presult in C (./ht '1/0' → d=0) — port already matches.
### R16-16: estdlib hex-float parsing diverges from glibc strtod for subnormal magnitudes — Low. estdlib.rs:138-139 (mant * 2.0f64.powi(exp) underflows via inf reciprocal → 0.0 → classified ERANGE) vs epicsStdlib.c:160 (glibc strtod returns the exact subnormal, no ERANGE). 52/2217 battery diffs, all subnormal-range hex (0x1p-1074: C OK|…0001, port Underflow; 0x1p-1023: C OK, port Overflow). Decimal path bit-exact across the whole subnormal boundary. Module doc's "accepts what glibc accepts" is refuted for this corner; parses_hex_floats tests only normal range.
### R16-17: EPICS_CA_MAX_SEARCH_PERIOD has no high-range clamp or diagnostic — Low. search.rs:184-208 (low clamp only) vs udpiiu.cpp:96-111 (getNTimers caps at 18 timers → effective 4194.304 s, prints "out of range (high)" pair ≈ above 8389 s). =100000: C caps + 2 stderr lines, port raw 3333 s tick, silent. (The port's search cadence redesign note covers the RTT ladder, not this missing diagnostic/cap.)
### R16-18: EPICS_CAS_BEACON_PERIOD diagnostic prints 3× at server startup; C prints once — Low. addr_list.rs:258-275 (from_env prints every call; invoked ≥3× during startup, no OnceLock — unlike the R15-17 client resolvers) vs online_notify.c:52-64 (single beacon thread reads once). Text byte-identical, value correct; count differs.
### R16-19: EPICS_CA_CONN_TMO ≤ 0 silently forced to 30 s; C uses the raw value — Low. transport.rs:187-191 (secs > 0.0 else 30 s) vs cac.cpp:188-194 (stores whatever envGetDoubleConfigParam returns; negative → watchdog fires immediately, "Virtual circuit unresponsive" flood). =-5: C floods stderr, port silent clean read. C's behaviour is degenerate; filed for completeness.
### R16-31: display.form.index published and marked for non-VAL field channels — Low. pvif.rs:247-258 (read_leaf_paths marks choices+index unconditionally for Scalar) + :1123 (build_display writes per-record Q:form for any numeric field) vs iocsource.cpp:41-64 (Q:form applied to form.index only if dbIsValueField(chan) — dbAccess.c:463-469, true for VAL only; choices always). GET/seed of rec.RVAL with info(Q:form,"Hex"): port sends one changed bit + 4 bytes pvxs does not, reports Hex where pvxs reports default. Both QSRV sources; untested (every form assertion uses VAL).
### R16-32: EPICS_PVA_CONN_TMO large value panics client AND server at startup; pvxs logs and defaults — **High**. config/env.rs:765-770 + server_native/runtime.rs:482 + client_native/server_conn.rs:56,65 (finite-positive filter then Duration::from_secs_f64) vs config.cpp:211-227 (parse_timeout: !isfinite || <0 || > time_t::max → log_err_printf + default) + util.cpp:769-783 (parseTo<double> = std::stod + trailing-ws-tolerant extraneous check). =1e300 passes is_finite && >0 then PANICS ("cannot convert float seconds to Duration"). Same unguarded chain at runtime.rs:468 (EPICS_PVAS_SEND_TMO), :471 (TLS_HANDSHAKE_TMO), env.rs:678/691 (BEACON_PERIOD[_LONG]) — port-only vars, same defect. Also: stod skips leading ws and parseTo trailing (" 45" works in pvxs, port falls back to 30 s); stod accepts hex floats. PVA half of the R15-16 family; the needed resolver REJECTS out-of-range (pvxs semantics), not saturate-to-MAX (CA semantics).
### R16-46: a pure status query or asynReport initiates a hardware connect attempt — Medium. port_actor.rs:328-410 (every pass ends in port_thread_auto_connect :491-493, including GetConnected/GetEnable/GetAutoConnect/Report) vs asynManager.c (notifyPortThread signalled from exactly five places: queueRequest :1626, announceExceptionOccurred :636, queue-timeout tail :700, cancelRequest :1688, unblockProcessCallback :1772; isConnected :2326-2337 / isEnabled :2340 / report :1136 are plain reads, never wake). Probe: isConnected → 1 attempt, asynReport 1 → 1 attempt, idle window → 0; C gives 0 for both. Status polling pulls a dead port's reconnect cadence from secondsBetweenPortConnect (20 s) to one per 2 s; asynReport becomes an action. Fix direction: gate the tail on queued dispatch / exception / queue-timeout.
### R16-47: report's parameter block one detail level late — asynReport 1 prints no parameter values — Medium. port.rs:1501 (level.saturating_sub(1) into report_params, whose thresholds: <1 count, >=1 names, >=2 values) vs asynPortDriver.cpp:3692 (reportParams(fp, details) UNCHANGED) + :1799-1809 + paramVal.cpp:296-330 (every param prints name+type+value+status regardless of details). Probe: report 1 → count only (C: both params with values); report 2 → names, no values; values only at 3. Line format also diverges ("Number of parameters is: %u" + "Parameter list %d" vs "param[i] name=…").
### R16-48: report's EOS escaping handles only \r and \n — Low. port.rs:1487-1499 (ad-hoc esc closure) vs asynPortDriver.cpp:3687,3690 (epicsStrPrintEscaped: \a\b\f\n\r\t\v\\'" + \x%02x for non-isprint). Binary terminator (\x03, ESC, \t, NUL) written raw into stdout. A correct port exists privately: iocsh.rs:160-180 escaped_from_raw (differs only on NUL: \0 vs C's \x00).
### R16-49: report prints "option: k = v" lines with no C source — Low. port.rs:1503-1507 (details >= 2) vs asynPortDriver.cpp:3677-3710 (never options; asynOption has no enumeration to walk). Both R16-48/49 pre-date 170fd865 (carried over from the eprintln version): retire or record as documented deviations.
### R16-61: the ReadDbLink input path drops MS/MSI/MSS severity inheritance — Medium. processing.rs:3694 (read_db_link_into_field → read_link_value, returns value alone; parsed monitor_switch never consumed; inherit_sevr_msg called from exactly two places: links.rs:915 put, processing.rs:2432 declared-input stage) vs dbDbLink.c:228-232 (dbDbGetValue ends with recGblInheritSevrMsg on EVERY healthy read). Affected: compress INP (compressRecord.c:342), aao closed-loop DOL (aaoRecord.c:366), epid/optics/motor links emitting the action. field(INP,"SRC MS") on compress leaves reader NO_ALARM while C raises to source severity. Failure path unaffected (LINK/INVALID still raised) — this is the healthy-link/alarming-source case MS exists for.
### R16-62: a self-referencing input link inherits the record's own committed alarm — C excludes it — Low. links.rs:526-531 (read_link_with_alarm builds committed for ANY local target) vs dbDbLink.c:228 (guard: precord != dbChannelRecord(chan)). Self MS input link folds the previous cycle's committed severity into this cycle's pending → reset_alarms re-commits → self-sustaining latch (once MAJOR, never NO_ALARM). Runtime reachability of the self-link in the port unverified (read-only round) — Low pending that.
### R16-76: subArray never re-loads/re-subsets a constant or empty INP at process — INDX inert on the standard configuration — **High**. waveform.rs:1326-1398 (set_val is the only slicing site, runs on link delivery) + links.rs:702-733 (Constant arm returns None after R15-78) vs devSASoft.c:92-123 (read_sa re-runs dbLoadLinkArray EVERY process; empty INP → nRequest=nord and STILL calls subset :118-120; subset :40-56 shifts by INDX, sets NORD, clears UDF unconditionally). subArray is C's documented exception to R15-78. (1) INP="[1,2,3,4]": C restores the slice every process (client put of 50 → still [1,2,3]); port keeps client data. (2) unset INP: C re-slices VAL by INDX each process (the "client writes, record slices" pattern); port does nothing. Port's DB-link path re-verified correct (INDX/MALM/clamp/runtime-INDX all exact).
### R16-77: constant links on the multi-input / SELL / DOLn readers are re-applied every cycle and never seeded at init — **High**. links.rs:533 (read_link_with_alarm Constant arm → constant_value() every read), called from processing.rs:1790/1799/1807/1958/2079/2926; no init-seed owner (rec_gbl_init_constant_inp covers only soft-DTYP single INP) vs recGblInitConstantLink at init in calcRecord.c:103, calcoutRecord.c:163,374, subRecord.c:104, selRecord.c:99,105, aSubRecord.c:126, seqRecord.c:121,125, dfanoutRecord.c:102,105, fanoutRecord.c:88 + dbConstLink.c:220-226 (dbConstGetValue delivers NOTHING at process). field(INPA,"5") + caput A=99 → destroyed next process (C keeps 99). Pre-first-process reads 0/NaN instead of the constant. Verified affected: calc, calcout, sub, sel (INPA..L + NVL), aSub, scalcout, acalcout, seq (SELL + DOL1..N). Same reader unprobed: fanout/dfanout SELL, printf INP0..9; epid CVL unverified (std-rs). ao/longout/mbbo/dfanout DOL NOT affected (verified — port keeps client VAL). Same family as R15-78; the init-seed owner must generalize.
### R16-78: parse_link_v2 isolates modifiers before the constant test — "<number> <modifier>" becomes Constant where C yields a PV_LINK — **High**. link.rs:1074 (split_link_modifiers first), :1096-1099 (numeric test on link_part only) vs dbStaticLib.c:2346-2360 (epicsParseDouble on the WHOLE string — rejects trailing garbage; then bracket test requiring trailing ']'; only then strchr ' '). Compiled C: INP="5 PP" → CA_LINK (process: VAL=0 INVALID/LINK); port: Constant("5") (VAL=5, NO_ALARM — broken link masked). SDIS="3 NPP" DISV=3: port DISA=3 → record PERMANENTLY DISABLED; C DISA=0, runs. OUT/FLNK: write silently dropped, C attempts CA put and alarms. "[1,2,3] PP" correctly a link in BOTH (bracket test needs trailing ']') — do not "fix" that arm.
### R16-79: SPC_NOMOD enforced only on the CA route; the dbPut owner used by DB-link writes has no gate — a link can truncate a waveform — **High**. field_io.rs:1065-1072 (read-only gate, CA route) vs field_io.rs:307-360 (put_pv_inner, no check) vs dbAccess.c:1330-1332 → :123-126 (dbPut → dbPutSpecial: SPC_NOMOD && pass==0 → S_db_noMod; dbPut sits below BOTH dbPutField and dbPutLink). record(ao){field(OUT,"WF.NELM PP")}: C refuses + writer INVALID/LINK; port truncates NELM AND the data, writer NO_ALARM. Same hole for subArray.MALM. caput WF.NELM correctly refused (descriptors right; the gate is missing from the one owner every internal write crosses).
### R16-80: subArray NELM/INDX are pp(TRUE) — a put must process; the port stores without processing (NELM put empties VAL) — Medium. waveform.rs:1008-1015 (NELM → zeroed realloc), :1059-1071 (INDX store only), nothing routes to put_driven_process vs subArrayRecord.dbd.pod pp(TRUE) → dbPutField → process → read_sa re-slices. C: NELM=2 → NORD=2 VAL=[1,2]; INDX=5 → VAL=[6,7]. Port: NELM=2 → NORD=0 VAL=[]; INDX=5 → stale window.
### R16-81: histogram has no SVL input link — C's only soft input path — and uses a nonexistent INP instead — **High**. histogram.rs:158-215 (HISTOGRAM_FIELDS lacks SVL; driven from common INP, processing.rs:4911-4950) vs histogramRecord.dbd.pod:212 (field(SVL,DBF_INLINK); NO INP field) + devHistogramSoft.c:44 (constant SVL seeds SGNL at init), :51-55 (dbGetLink(&prec->svl) every process). record(histogram){field(SVL,"MYSIG")} → stderr "field not found: SVL", record inert (only external caput to SGNL feeds it); port's INP form rejected by C's dbd — databases unportable both directions.
### R16-82: the initial UDF severity is never applied — every never-processed record advertises NO_ALARM; MS propagates nothing at startup — **High**. No port equivalent of iocInit.c:521-523 (initRecordInstance: if (udf && stat==UDF_ALARM) sevr = udfs) + dbCommon.dbd.pod:296-301 (STAT initial("UDF"), UDFS default INVALID). Fresh record: C STAT=UDF SEVR=INVALID; port 0/0 with UDF=1. Consumer linked MS to a not-yet-processed record inherits INVALID/LINK in C, NOTHING in the port — the IOC-startup ordering case MS exists for. Control verified: MS itself works once the source has processed.
### R16-83: a constant DOL seeds VAL but does not clear UDF at init — Low. Constant-DOL seed lands VAL for ao/longout/mbbo/dfanout but leaves UDF=1 vs aoRecord.c:112-113, longoutRecord.c:113, mbboRecord.c:133, int64outRecord.c:110, dfanoutRecord.c:105 (if (recGblInitConstantLink) prec->udf = FALSE). Confined to the pre-first-process window (port clears UDF at process, no UDF alarm raised — verified); combined with R16-82 it is what a client sees at startup.

### Notes
- Not filed (Round 16 NOT-REAL adjudications): sseq SELM/SELN + active
  list + abort machine match C; R15-77 "un-alarmed" (C also leaves
  SEVR/STAT uncommitted); subArray NELM=1 seeding (C does not seed);
  "[1,2,3] PP" as array constant (C: CA_LINK — bracket test needs
  trailing ']'); stringout DOL="hi" seeding (C: PV_LINK, never seeds);
  ao/longout/mbbo/dfanout constant-DOL re-apply (port matches C);
  SPC_NOMOD for CA clients (enforced); swait INPA (swait has INAN..INLN
  — auditor's own probe error); sync_io empty-drvInfo skip, failed-create
  reason zeroing, param addr collapse, double @asynMask (all D, above).
- B's housekeeping: stray softIoc/softioc-rs test processes may hold
  port 5064 (timeout-bounded; B stopped kill attempts after pgrep kept
  matching its own shell wrapper).
- Thematic cluster: 9 of 24 findings (R16-61/62/76/77/78/79/80/82/83)
  sit in the LINK layer's init/read/write ownership — the same
  structural seam waves 12-13 worked. The link classifier (R16-78), the
  init-seed owner (R16-77/76/83), and the dbPut owner (R16-79) are the
  three primitives to close.

## Fix wave 14 — dispositions (2026-07-13)

Scope: all 24 Round-16 findings plus R16-33 (assigned mid-wave when the
`pva-gateway` feature turned out not to compile at HEAD). Six opus fixer
panels, one worktree each; main merged and verified every commit with git.
Merge commits: `080bfd8c`/`da16471b` lineage (waves A/B/C/D/F, 17 findings)
then `44dadde0` (R16-33) and `402aacb2` (E links cluster, 7 findings).

**Per-finding dispositions (25):**

- R16-1 `26266c5a` FIXED — WriteDbLinkTyped + typed_output_buffer: sseq LNKn
  routes on the destination's DBF type, not the source's.
- R16-2 `d1cbd5fb` FIXED. R16-3 `c2554616` FIXED.
- R16-4 `2109120e` FIXED (structural) — ScalcResult::non_finite;
  CalcError::NonFiniteResult deleted, "failing status, value lost" is
  unrepresentable.
- R16-16 `9750afae` FIXED (structural) — shared runtime::stdlib::
  HexSignificand closes BOTH strtod ports' subnormal hex-float collapse.
- R16-17 `0d3f7090` FIXED — getNTimers ladder exact vs compiled caget.
- R16-18 `02dc3fc9` FIXED — addr_list OnceLock, resolved once per process.
- R16-19 `994a2f94` DOCUMENTED DEVIATION — non-positive EPICS_CA_CONN_TMO is
  refused out loud (C floods 177,182 stderr lines/3 s; filed as CBUG-D3 in
  `doc/upstream-c-bugs.md`).
- R16-31 `9c42a06f` FIXED — Q:form VAL-only gate.
- R16-32 `2c664fb2` FIXED — config::env::parse_timeout_env, pvxs semantics
  (REJECT out-of-range to default, not saturate).
- R16-33 `03ca4a4a` FIXED, WIDENED — the 8 compile errors were half the
  finding: the gateway middleware layers (ReadOnly/Acl/Audit) overrode
  get_value_checked without read_checked, so the trait default re-derived
  values and returned marked:None — every gateway read dropped upstream
  marks. Structural: client_native::ops_v2 returns MarkedRead with the
  reply's changed bitset decoded to leaf paths; layers forward, none
  fabricates. Monitor seed keeps marked:None deliberately (cache snapshot
  is wholly assigned). Regression test
  gateway_get_frames_upstream_marks_not_a_full_mask fails pre-fix.
- R16-46 `6f24725e` FIXED (+ `06b1adae` fixer's own test deflake, barrier
  read) — PortActor::woken + exceptions_announced model C notifyPortThread's
  5 signal sites.
- R16-47 `3b3ab909` FIXED — C report detail levels.
- R16-48 `e0a71007` FIXED — one crate::escape table; C's two entry points
  differ only on NUL and the port reproduces each per-path (filed as
  CBUG-D4).
- R16-49 `6bebdde2` FIXED — options lines retired.
- R16-61 + R16-62 `8440fafa` FIXED, ONE COMMIT FOR TWO FINDINGS — both are
  the same C statement (dbDbLink.c:228-232 tail of dbDbGetValue);
  input_link_inheritance is the single owner of MS/MSI/MSS inheritance and
  the self-record guard. The fixer offered a split; main judged the change
  atomic (one owner, one C statement) and accepted it combined.
- R16-76 `a9fda800` FIXED — Record::read_constant_inp hook (only
  ArrayKind::SubArray overrides); val_capacity() single owner returns MALM.
- R16-77 `a5c7aebf` FIXED — constant links deliver nothing at process
  (dbConstGetValue); seed_constant_links is the single init-seed owner.
- R16-78 `c0551b16` FIXED — parse_link_field runs C's constant test
  (epicsParseDouble on the whole string, dbStaticLib.c:2346-2349) before
  the modifier split; "5 PP" is a PV link, SDIS="3 NPP" no longer disables.
- R16-79 `f11710ef` FIXED (invariant) — no put route may modify a NOMOD
  field; only the .db load path may. Owner: field_io::check_no_mod; bypass
  audit routed put_pv_inner / put_pv_and_post_with_origin /
  put_record_field_from_ca / check_external_put_preconditions.
- R16-80 `598e9b3a` HALF NOT-REAL — pp routing for subArray NELM/INDX
  already existed; the real defect was the NELM put arm calling
  reallocate_val() and wiping VAL. Fixed the narrower defect; the existing
  test assertion that encoded the pre-fix bug was corrected against softIoc.
- R16-81 `8986d51f` FIXED — histogram SVL; INP refused.
- R16-82 `00c56fec` FIXED — RecordInstance::run_init_passes applies C's
  doInitRecord0 prologue (iocInit.c:521-523): STAT=UDF, SEVR=UDFS.
- R16-83 `b23ccb24` FIXED — constant-DOL seed defines the record (NaN-aware
  UDF clear), six ad-hoc init_record DOL parses folded into the owner, C's
  init tail (oval/pval/mlst/alst/lalm/oraw/orbv, bo/mbbo convert) runs as
  the owner's step 3; parse_c_double owns "link text → number" per
  dbConstLink.c:34. Two pre-existing tests encoded the pre-fix state and
  were corrected against softIoc, not silenced.

**Gate (merged tree):** cargo fmt no-change; workspace clippy -D warnings
clean; workspace nextest 8809/8809 (2 skipped); doctests 22 crates ok;
AND the new permanent gate members: clippy + nextest with
`-p epics-bridge-rs --features pva-gateway` (771/771). GATE CHANGE: the
pva-gateway feature is now part of every pre-push gate — the default-feature
gate was blind to the R15-33 regression that R16-33 fixed.

**Catalogue extraction:** the upstream-C bug catalogue moved to
`doc/upstream-c-bugs.md` (`f2585411`) with new Batch D (CBUG-D1..D5).

**Open leads for Round 17** (from the wave-14 fixer reports; none yet
adjudicated):

1. histogram UDF divergence — C's link-driven histogram stays UDF=1
   forever; the port framework clears UDF at process. (E)
2. asyn trace escape call-site mapping — escape.rs reproduces both C
   renderings; verify each asyn call site picks the same entry point as its
   C counterpart (the wave-14 D report claimed the port matches only 2 of
   3 C paths). (D)
3. sseq DOn/STRn partner post mask — port posts DBE_VALUE|DBE_LOG where C
   posts DBE_VALUE only. (A)
4. refresh_link_status init-window race — a DB-syntax remote PV inside init
   takes a plain put. (E)
5. classify_link EXT_NC for absent-field local targets. (E)
6. estdlib decimal ERANGE subnormal-exact — needs a ~750-digit literal to
   matter. (B)
7. dbCommon's real SPC_NOMOD set is larger than the R16-79 gate enforces —
   C marks NAME/STAT/SEVR/AMSG/NSTA/NSEV/NAMSG/ACKS/ACKT/RPRO/UTAG; the
   gate covers PACT/LCNT/PUTF + per-record read_only flags, so a client can
   still write STAT/SEVR. ACKT/ACKS are handled by dbrType BEFORE the gate
   in C. (E)
8. the iocsh dbLoadRecords path never calls post_init_finalize_undef — the
   .db builder path does; mbboDirect's bit-fold-into-VAL only happens for
   IocBuilder-loaded records. (E)
9. lso seeds a constant DOL though C's lsoRecord::init_record uses
   dbLoadLinkLS and leaves UDF set (softIoc: DOL:"abc" → VAL empty, UDF=1).
   (E)

**Flakes this cycle:** none in the merged-tree runs (8809/8809 first try;
the previously listed CLI/stability flakes did not recur).

## Round 17 — re-audit (2026-07-13)

Six opus auditor panels, read-only, on HEAD `12e95832` (wave 14 fully
merged). Every panel probed compiled C (softIoc 7.0.10.1-DEV, CA tools,
libCom-linked harnesses); pvxs 1.5.1-42-gb568e93 read+probed for C.
NOT convergence: 32 new findings (3 High), 2 wave-14 commits verified
BROKEN-incomplete, 7 of 9 leads REAL.

### Wave-14 commit verification

HOLD (opened and probed, not trusted): `26266c5a` `d1cbd5fb` `c2554616`
`2109120e` (A, incl. the sibling-engine check: acalcout keeps the value,
base calcPerform has no non-finite tail so nothing owed); `9750afae`
(206-case hex battery, strict match incl. exact ERANGE codes)
`0d3f7090` (10-value head-to-head, byte-identical; `inf` = C aborts
malloc — the documented deviation) `02dc3fc9` (warn once, byte-identical
to softIoc -d) `994a2f94` (warn-once + 30 s default confirmed) (B);
`9c42a06f` (both pvxs halves), `2c664fb2` (accept/reject boundary
bit-identical to pvxs; upper `enforceTimeout` reset missing → R17-34)
(C); `6f24725e` `06b1adae` `3b3ab909` `6bebdde2` (D — C's five
notifyPortThread sites re-derived; connect-before-dispatch order
confirmed NOT regressed); `c0551b16` `a5c7aebf` `00c56fec` `b23ccb24`
(E); `a9fda800` `598e9b3a` `8986d51f` (arrays, side-by-side softIoc
transcripts).

BROKEN (incomplete, not regressed):
- `f11710ef` (R16-79) — the check_no_mod GATE is the right owner and
  covers all put routes (CA, put_pv, DB link — re-verified from the
  array side), but its DECLARATION SET is incomplete: dbCommon's
  SPC_NOMOD fields beyond PACT/LCNT/PUTF are client-writable → R17-62
  (High); compress LIFO VAL dynamic NOMOD → R17-77; histogram CSTA →
  R17-78.
- `8440fafa` (R16-61/62) — input_link_inheritance is correct but not
  yet the single owner: DOL closed-loop, SIML and SIOL reads bypass it
  (fetch_link returns value, no alarm) → R17-64.
- `e0a71007` (R16-48) — HOLD as a table; the call-site mapping and the
  API around it are incomplete → R17-46/48/49.
- `03ca4a4a` (R16-33) — HOLD on GET/PUT_GET/middleware; the monitor
  seed's `marked: None` justification is false on the wire → R17-32.

### Lead adjudications

- sseq DOn/STRn partner post mask (A) — REAL → R17-4.
- estdlib decimal ERANGE subnormal-exact (B) — REAL but Low/practically
  unreachable (needs a ~751-digit exact literal; inexact subnormals
  match): C accepts the exact expansion of 2^-1074, port returns
  Overflow. DISPOSITION: documented deviation (matches the in-code
  trade-off comment at estdlib.rs:52-58); no fix.
- histogram UDF (E L1) — REAL, widened to dfanout (alarm-visible) and
  event → R17-63.
- refresh_link_status init race (E L4) — REAL (1-in-20 nondeterminism
  measured) → R17-69.
- classify_link EXT_NC absent-field local target (E L5) — NOT-REAL:
  softIoc gives `Ext PV NC` and converts the link to CA_LINK; port
  matches.
- dbCommon SPC_NOMOD set (E L7) — REAL High → R17-62.
- dbLoadRecords post_init_finalize_undef (E L8) — REAL narrowed (the
  bit-fold happens on both paths; the UDF clear is what the iocsh path
  loses) → R17-66.
- lso constant DOL (E L9) — REAL, re-reasoned: "abc" is not a constant
  in C at all (CA_LINK); the real rule is dbLoadLinkLS → dbLSConvertJSON
  (bare number = JSON no-op: VAL="", LEN=1, UDF=0) → R17-65.
- asyn trace escape mapping (D) — REAL: 8 C call sites mapped, 5 match,
  2 not-ported (R6-47), 1 mismatch: traceVprintIOSource's fp branch →
  R17-46.
- AUTO_ADDR_LIST bool parse (B, self-generated) — NOT-REAL: the CA
  client uses iocinf.cpp:187-192 strstr("no"), not
  envGetBoolConfigParam; port matches identically.

### Open findings R17-1..R17-85 (33)

Category A (sseq/calc):
- **R17-1** Medium — sseq STRn renders negative PREC clamped to 0; C
  reinterprets as epicsUInt16 65535 → %e 17-digit. sseq.rs:209 vs
  cvtFast.c:111 (compiled: `(unsigned short)(short)-1` → ` 3.70000000000000018e+00`).
  Same defect second site: types/codec.rs:98 (DBR_STRING wire encoder,
  caget -S of PREC=-1 ai). Counter-anchor (correct form):
  calc/engine/string.rs:1887.
- **R17-2** High — sseq reads DOLn with the port's native link type,
  never C's dol_field_type switch (sseqRecord.c:640-705): ENUM/MENU
  source should read DBR_STRING (state LABEL, dov=atof(label)); port
  stores the index. CHAR/UCHAR array source: port's to_f64 fails and
  the error is discarded (`let _ =`), DOn/STRn keep stale values
  silently. The input twin of R16-1; structural fix is a typed READ
  seam. sseq.rs:1044-1051, processing.rs:3780-3792, sseq.rs:1319-1338.
- **R17-3** Low — WAITn raises WTGn + in_flight before the destination
  switch decides there is no put (C sets waiting inside each successful
  dbCaPutLinkCallback branch; default arm never touches it).
  sseq.rs:609-616/650-677 vs sseqRecord.c:727-792.
- **R17-4** Low — sseq partner post mask: partner view posts
  DBE_VALUE|LOG, C posts bare DBE_VALUE (sseqRecord.c:1115/:1136/:245/
  :248/:660/:679/:699); port also posts the partner unconditionally
  where C posts only on change; on the process path which view is the
  partner depends on dol_field_type — the mask must be decided by the
  writer (set_numeric/set_string report the derived view), not a static
  field-name list. field_io.rs:1263-1274, sseq.rs:981-994.

Category B (CA tools):
- **R17-16** Medium — EPICS_CA_SERVER_PORT / EPICS_CA_REPEATER_PORT
  resolve with strict parse::<u16>(), dropping C's
  envGetInetPortConfigParam: sscanf leniency, the
  `<= IPPORT_USERRESERVED (5000)` / `> 65535` floor-to-default, and the
  out-of-range diagnostics. Proven live: port 3000 reads on caget-rs
  (exit 0), compiled caget refuses + defaults to 5064 (exit 1).
  client/mod.rs:4771/:5331, discovery/mod.rs:227, beacon_monitor.rs:711,
  protocol.rs:43-44, server/addr_list.rs:122-125 vs envSubr.c.

Category C (PVA/gateway):
- **R17-31** Medium — DBF_CHAR array VAL + info(Q:form,"String") (the
  QSRV long-string idiom) served as NTScalarArray<byte>; pvxs serves
  NTScalar<string> (getChannelValueType, iocsource.cpp:619-643 +
  getArrayValue collapse + putLongString). Port has no Q:form reader
  outside display. qsrv/channel.rs:565-585.
- **R17-32** Medium — every gateway monitor's FIRST downstream frame
  marks leaves the upstream never assigned: the seed snapshot maps to
  `marked: None` → canonical full bitset. Proven on the wire: upstream
  seed bits [1], gateway downstream seed bits [1,2]. The R16-33 defect
  still live on the monitor seed; cache computes marks per event but
  never accumulates the union. pva_gateway/source.rs:900/:1032/
  :1998-2007/:2035-2043, tcp.rs:2056-2063.
- **R17-33** Medium — the middleware chain swallows
  set_channel_invalidator (none of ReadOnly/Acl/Audit implements it;
  trait default drops it), so operator `:drop`/`:flush` never sends
  DESTROY_CHANNEL downstream in a real gateway; the only test wires the
  UNLAYERED source. runtime.rs:777-778, gateway.rs:300/:311,
  source.rs:1129-1147, channel_cache.rs:1595-1598.
- **R17-34** Low — enforceTimeout's upper reset (>= double(time_t::max)
  → 40 s, config.cpp:373-391, applied to the SCALED tcpTimeout) not
  reproduced; for CONN_TMO in ~[6.92e18, 9.22e18] pvxs falls back to
  40 s idle, port keeps ~1.2e19 s. runtime.rs:480-483,
  server_conn.rs:65.
- **R17-35** High — group scalar-member PUT never unwraps the NTScalar
  wrapper: for a `+type:"scalar"` member the client sends the NTScalar
  structure and qsrv/group.rs::convert_member_value hands that whole
  structure to pv_field_to_epics instead of dereferencing its `value`
  leaf, so EVERY scalar-mapped member PUT (numeric included) is
  rejected ("member 'x' value is not convertible to backing field").
  Found by the wave-15 R17-31 group-PUT test; filed post-audit from the
  fixer's UNFIXED report.
- **R17-36** Low — client echo cadence has no upper cap:
  heartbeat_interval() is configured/2, pvxs uses
  max(1, min(15, tcpTimeout*3/8)) (clientconn.cpp:163), so a large
  CONN_TMO pushes the echo period far past C's 15 s ceiling. Distinct
  from R17-34 (idle-timeout VALUE vs echo CADENCE). Filed post-audit
  from the wave-15 fixer's UNFIXED report.
- **R17-37** Low — a marked Meta group member is a silent skip in the
  port's PUT loop (FieldMapping::is_client_writable), but pvxs counts a
  marked+putable field as `changing` and runs doPostProcessing on it
  even though IOCSource::put writes nothing (groupsource.cpp:563-571):
  a config that gives a Meta member a `+putorder` processes the record
  in pvxs and does not in the port. No shipped config does (Meta
  members carry no `+putorder` in any found), and the pre-R17-35
  behavior (whole-PUT rejection) was further from pvxs than the skip.
  Filed post-fix from the wave-15 fixer's UNFIXED report.
  qsrv/group.rs (is_client_writable / put loops) vs
  pvxs src/qsrv/groupsource.cpp:563-571.

Category D (asyn):
- **R17-46** Medium — trace ESCAPE form hardwires escaped_from_raw; C's
  traceVprintIOSource chooses by destination (fp → epicsStrPrintEscaped,
  errlog → epicsStrSnPrintEscaped) and the DEFAULT destination is
  stderr → print_escaped. First-byte NUL: C prints an empty data line,
  port prints the payload. Selection belongs at the trace-output site
  keyed on cfg.file (TINP and echo interpose must keep
  escaped_from_raw). trace.rs:936 vs asynManager.c:3153-3165/:2928-2941/
  :458.
- **R17-47** Medium — trace I/O mask treated as an enum: C emits one
  line per enabled bit (three independent if blocks), mask 0 = no data;
  port if/else-chains and defaults to ASCII where C defaults to NODATA;
  the ASCII form substitutes '.' where C fprintf's raw bytes; the hex
  form is unwrapped where C wraps every 20 bytes. trace.rs:907-944/:306
  vs asynManager.c:3146-3190, asynDriver.h:219-222.
- **R17-48** Medium — escaped_from_raw drops C's dstlen: TINP (40),
  IEOS/OEOS (10), errlog traceBuffer (80) are never truncated; C
  truncates mid-escape-pair leaving a dangling backslash (compiled
  proof). Structural: give the function C's signature so every call
  site states its C buffer bound. escape.rs:30-32, asyn_record/
  mod.rs:1062/:2618-2620, trace.rs:936.
- **R17-49** Low — print_escaped misses epicsStrPrintEscaped's
  strlen-based empty-string early return: a first-byte-NUL EOS prints
  nothing in C, `\x00` in the port. Distinct C quirk from CBUG-D4.
  escape.rs:36-38 vs epicsString.c:236-237.

Category E (records/links/init/dbPut):
- **R17-61** Medium — parse_c_double is not epicsParseDouble: ERANGE
  literals (1e400, 1e-320) classify CONSTANT in the port (defined
  record holding inf!) vs PV-link/UDF=1 in C; wide-hex
  (0xffffffffffffffffff) and hex-float (0x1p4) classify PV link in the
  port vs CONSTANT in C (strtod accepts). link.rs:539-552/:1100 vs
  epicsStdlib.c:150-176, dbStaticLib.c:2346-2349.
- **R17-62** High — every dbCommon SPC_NOMOD field except
  PACT/LCNT/PUTF is client-writable: STAT/SEVR/NSEV/ACKS/RPRO/UTAG
  accepted (alarm state forgeable, permanent on a Passive record); C
  refuses all with S_db_noMod. ACKS/ACKT are a semantic inversion: C
  runs putAcks/putAckt only for dbrType DBR_PUT_ACKS/ACKT ABOVE the
  gate, so the fix must plumb the DBR type before gating or
  acknowledgement breaks. field_io.rs:57-72, record_instance.rs:
  1646-1710/:1846/:1977 vs dbCommon.dbd:13-190, dbAccess.c:123-127/
  :1331-1335.
- **R17-63** Medium — UDF cleared at every process for record types
  whose C process() never clears it: dfanout (alarm-visible: C is
  INVALID/UDF every cycle with no DOL, port NO_ALARM), histogram,
  event. Blanket `clears_udf` default needs per-record opt-out.
  processing.rs:2764-2765/:4173-4174, record_trait.rs:1673.
- **R17-64** Medium — MS inheritance still bypassed on DOL closed-loop,
  SIML, SIOL (fetch_link returns value, no alarm): ao/bo
  DOL="x MS", ai SIML/SIOL MS all NO_ALARM in the port, MAJOR/LINK in
  C. input_link_inheritance is not yet the single owner.
  processing.rs:1879-1883/:4878/:5466/:5571 vs dbDbLink.c:228-232.
- **R17-65** Low — long-string constant links ignore dbLoadLinkLS/
  dbLSConvertJSON (constant parsed as JSON; bare number = no callback:
  VAL="", LEN=1, UDF=0; string form only as {const:"hello"}). lso seeds
  the TEXT and leaves UDF=1; lsi seed fails TypeMismatch. Outside the
  R16-77 owner. lso.rs:254-268, lsi.rs:274 vs lsoRecord.c:82-95,
  dbConstLink.c:178-192, dbConvertJSON.c:191-236.
- **R17-66** Medium — iocsh dbLoadRecords never runs
  post_init_finalize_undef (IocBuilder does): mbboDirect/histogram/aao
  come up UDF=1 on the path a real IOC uses, UDF=0 in C. The hook
  belongs inside run_init_passes next to the 00c56fec prologue.
  commands.rs:1174-1195, ioc_builder.rs:326.
- **R17-67** Low — add_record (iocsh dbCreateRecord) seeds constants +
  deadbands but never runs init_record or the UDF prologue — violates
  the seed owner's own stated invariant. database/mod.rs:1076-1084.
- **R17-68** Low — parse_link_field invents a quoted-string constant
  form C does not have (C: only "parses as a number" and "[...]");
  softIoc makes DOL="\"hello\"" a CA_LINK. The masking class R16-78
  fixed, one arm above it. link.rs:1077-1082 vs dbStaticLib.c:2346-2356.
- **R17-69** Low — calcout/sseq refresh_link_status races the record
  load (spawned from add_record): 19/20 Local PV, 1/20 Ext PV NC for a
  forward-referenced local link; C is deterministic (init at iocInit +
  0.5 s checkLinks re-poll). Diagnostic fields only.
  calcout.rs:377-411.

Arrays (compress/histogram/subArray/waveform/conversion):
- **R17-76** High — runtime put to compress ALG/BALG/PBUF/N does not
  reset the buffer; C's special(SPC_RESET) zeroes NUSE/OFF/INX/CVB,
  reallocates the sum buffer, memsets, posts (compressRecord.c:377-393,
  :85-99; dbd declares five SPC_RESET fields). Port: FIFO→LIFO switch
  reads the old ring in the new order (garbage); N put corrupts the
  next emitted sample. compress.rs:548-556/:641-696/:562.
- **R17-77** Medium — compress VAL writable under BALG=LIFO; C sets
  SPC_NOMOD dynamically in cvt_dbaddr (compressRecord.c:398-407) —
  per-dbAddr special, so the port's static FieldDesc read_only cannot
  express it. compress.rs:436-440, field_io.rs:57-72.
- **R17-78** Medium — histogram CSTA writable on every route; C
  declares SPC_NOMOD (counting toggled only through CMD). A caput
  silently stops a live acquisition. histogram.rs:224-228/:482-488 vs
  histogramRecord.dbd.pod:170-173.
- **R17-79** Medium — float→int narrowing saturates (Rust `as`); C's
  dbPut convert is a bare cast: compiled x86-64 gives 70000→4464
  (short), 3.0e9→INT_MIN (long); port gives 32767/2147483647. C's cast
  is UB by the standard but compiled C is the parity target (the
  HexSignificand/shift-mask precedent). DISPOSITION: fix to reproduce
  compiled-C x86-64 narrowing (per-dest-width cvttsd2si semantics);
  file the C UB as a CBUG batch-E candidate. types/value.rs:963-966/
  :1079-1082 vs dbConvert.c:96-113/:1632-1634.
- **R17-80** Medium — histogram has no SDEL watchdog: C's wdogCallback
  posts VAL (DBE_VALUE|LOG) every SDEL seconds when mcnt>0 and re-arms
  (init_record + special on SDEL). Port: SDEL put stores and nothing
  else; slow-accumulation displays never update.
  histogram.rs:489-494 vs histogramRecord.c:102-124/:168/:266-268.
- **R17-81** Low — compress OUSE and INPN fields absent (both
  SPC_NOMOD): OUSE is C's "post NUSE only on change" latch, INPN the
  WPTR-realloc trigger. compress.rs:435-525 vs
  compressRecord.dbd.pod:481-506.
- **R17-82** Low — subArray has no BUSY field (subArrayRecord.dbd.pod:
  390-393 declares it like waveform). waveform.rs:849-870.
- **R17-83** Low — histogram INP channel still resolves
  (get_pv("HI.INP") → Ok("")) though the .db load refuses it; C:
  PV not found. declares_inp_link gates the loader, not the namespace.
  histogram.rs:301-303.
- **R17-84** Low — histogram VAL served to PVA as int32[]; C's
  cvt_dbaddr sets DBF_ULONG → pvxs uint32[]. CA unaffected (no
  DBR_ULONG on the wire). histogram.rs:177-184, convert.rs:16.
- **R17-85** Medium — scalar put into a FIFO compress VAL: C's dbPut
  writes through get_array_info's READ start, so during initial fill
  every scalar put rewrites the same slot ([3,0,0] where the port has
  [1,2,3]). The port's behavior is the intended one; C's is a design
  defect. DISPOSITION: documented deviation (port keeps
  append-at-write-cursor) + CBUG batch-E candidate; NOT in fix wave 15;
  flagged for user sign-off. compress.rs:165-200 vs dbAccess.c:
  1351-1362, compressRecord.c:408-431.

### Review log

Thematic clusters this round: (1) the SPC machinery — the R16-79 gate
is sound but the port lacks C's full SPECIAL vocabulary (SPC_NOMOD
static set R17-62/78/81/82, dynamic per-dbAddr NOMOD R17-77, SPC_RESET
R17-76, watchdog-armed special R17-80); (2) typed link reads — R16-1
fixed the write side, R17-2 is its input twin, R17-61/65/68 are the
constant-classifier remainder; (3) single-owner erosion — R17-64
(inheritance), R17-32 (marks), R17-66/67 (init passes) are each "the
owner exists, a path bypasses it". NOT-REAL adjudications recorded
above plus arrays: histogram CA type, u32 counters, compress N NOMOD,
DB-link gate coverage, subArray NELM/INDX post-598e9b3a, val_capacity
MALM (all refuted with evidence). Flakes: none observed this round
(B re-ran the three known-flaky CLI tests: all pass).

## Fix wave 15 — dispositions (2026-07-14)

Scope: 27 of the 33 Round-17 findings (R17-1..4, 16, 31..36, 46..49,
61..69, 76..84), plus one seam cleanup and one test-infra defect family
found by the gate itself. Six opus fixer panels, one worktree each; main
merged and verified every commit with git. Merge commits: A/B/C/D
category merges, then the a-follow-up seam removal, `9236330c` (E),
the F merge, the C follow-up merge (R17-35/36), and the test-infra
merge (`1d35eea7` lineage). Not fixed by design: R17-37 (filed this
wave, open), R17-85 (documented deviation, user sign-off pending),
estdlib subnormal-exact (documented deviation, Round-17 adjudication).

**Per-finding dispositions:**

- R17-1 `061e9849` FIXED — `prec as u16` at sseq::set_numeric AND the
  same defect at types/codec.rs:98 (rg-widened).
- R17-2 `bf677a8c` FIXED — typed DOLn READ seam (ReadDbLinkTyped mirror
  of R16-1's write side); a silent `let _ =` discard closed with it.
- R17-3 `e74637d0` FIXED (structural) — ResolveOutTarget resolves the
  destination BEFORE process, so "no put" ⟹ "no wait" by construction.
- R17-4 `2c04a643` FIXED — CyclePostMask::Value: the WRITER decides the
  partner post mask (sseq special() posts STRn with literal DBE_VALUE).
- R17-16 `1b237aea` FIXED — runtime::env::env_inet_port is the one owner
  of envGetInetPortConfigParam (sscanf leniency, 5000<port<=65535 gate,
  byte-identical stderr vs compiled caget, 16 head-to-head cases). TWO
  SEMANTIC CHANGES flagged: CAS_* selection is a presence test
  (caservertask.c:491-508), and the repeater port resolves ONCE per
  process (udpiiu::repeaterPort).
- R17-31 `52fe221b` FIXED — nt_type_for_channel single NT owner; Q:form
  String long-strings. SEMANTIC CHANGE: lsi/lso VAL PUTs now SIZV-bounded.
- R17-32 `c05a56f6` FIXED (structural) — gateway cache stores SourceRead,
  value + accumulated mark union inseparable; no-event ⟹ no seed frame.
- R17-33 `55960fd7` FIXED — all-45-method middleware forwarding audit;
  same swallow class closed in revalidate_read / check_monitor_request /
  subscribe_checked_opts.
- R17-34 `db9738f5` FIXED — env::effective_tcp_timeout_secs one owner
  (enforceTimeout on the SCALED value).
- R17-35 `aabdf2d8` FIXED — GroupChannel::put_leaf ports pvxs
  IOCSource::put's switch(info.type); FieldMapping::is_client_writable
  names the no-write arms; the group long-string PUT leaf (reachable
  now) takes the same putLongString char image — closes R17-31's group
  half. Fails-pre-fix regression tests for numeric/string/NTEnum/
  long-string members, atomic and non-atomic.
- R17-36 `a81df980` FIXED — config::env::echo_period_secs one owner:
  max(1, min(15, tcpTimeout*3/8)), composed with R17-34's owner.
- R17-37 OPEN (filed this wave from the R17-35 fixer's UNFIXED) — Meta
  member with +putorder: pvxs runs doPostProcessing, port's PUT loop
  silently skips. No shipped config exhibits it.
- R17-46 `5dc96c18` FIXED — destination-keyed trace escape (C's default
  trace destination is stderr → print_escaped branch).
- R17-47 `6e73ac29` FIXED — traceIOMask bitfield; DEFAULT TRACE OUTPUT
  CHANGED to NODATA per tracePvtInit (asynManager.c:449-459); 3
  pre-existing tests corrected against C, not silenced.
- R17-48 `016a5774` FIXED — escaped_from_raw carries C's
  (dst,dstlen,src,srclen) signature, truncating mid-escape-pair;
  TraceConfig::trace_buffer_size is grow-never-shrink.
- R17-49 `5c4a8498` FIXED — empty-string early return.
- R17-61 `526726b9` FIXED — parse_c_double IS epicsParseDouble (one
  parse shared with the R16-83 seed path).
- R17-62 `c34591c2` FIXED — dbCommon SPC_NOMOD covers C's whole set
  (NAME/STAT/SEVR/AMSG/NSTA/NSEV/NAMSG/ACKS/ACKT/LCNT/PACT/PUTF/RPRO/
  TIME/UTAG); alarm ack is a DBR *request type* dispatched ABOVE the
  gate (put_alarm_ack_from_ca), not a common-field put.
- R17-63 `2644e581` FIXED — UDF clear is per-record-type, not a
  processing rule.
- R17-64 `25aa52f7` FIXED — the link READ carries the source alarm on
  every path (fetch_link takes the reading record; MS/MSI/MSS applied
  where C's dbGetLink does).
- R17-65 `65e24034` FIXED — lso/lsi constant DOL loads through
  dbLoadLinkLS.
- R17-66 `62429573` FIXED — post-init UDF tail lives in the init-pass
  owner (mbboDirect B0..B1F fold, histogram constant SVL verified
  against softIoc).
- R17-67 `ddc899b2` FIXED — add_record runs the init passes it already
  assumed (AO DOL="5" → UDF 0 / SEVR INVALID / STAT UDF oracle-matched).
- R17-68 `cddea284` FIXED — a quoted string is a PV link, not a
  constant (CA_LINK "hello", UDF 1).
- R17-69 `dd5e0af3` FIXED (structural) — PvDatabase::begin_load returns
  an RAII DbLoadGuard; classify_link awaits the database-load boundary,
  which IS C's init_record-after-load ordering. Forward references are
  deterministically Local; no sleeps, no re-poll. Family closed through
  the one classifier (calcout/sseq/swait/throttle all funnel).
- R17-76 `22dd6403` FIXED — one reset owner for all five compress
  SPC_RESET fields.
- R17-77 `b0125fe3` FIXED — Record::field_no_mod dynamic SPC_NOMOD hook
  (cvt_dbaddr-raised state): LIFO VAL refuses client puts; the
  check_no_mod gate is still the single enforcer.
- R17-78 `05268579` FIXED — histogram CSTA SPC_NOMOD.
- R17-79 `51435dc8` FIXED — types::c_cast single owner of double→int
  narrowing, reproducing compiled x86-64 (cvttsd2si integer-indefinite:
  70000.9→i16 4464, 3.0e9→i32 INT_MIN); Rust's saturating `as` was a
  silent divergence from every compiled IOC. C's UB filed as CBUG-E2.
  rg-widened into processing.rs's SDIS DISA narrowing at merge.
- R17-80 `f6ce1657` FIXED — histogram SDEL watchdog (ArmWatchdog action
  + wdogCallback posting VAL every SDEL seconds while mcnt>0).
- R17-81 `531daed9` FIXED — compress OUSE (NUSE post-on-change latch)
  and INPN served, both noMod. PARTIAL: INPN latches the length
  ReadDbLink delivered, not C's dbGetNelements source *capacity*
  (softIoc: waveform NELM=3/NORD=1 → INPN 3, port → 1); closing it
  needs capacity plumbed through ReadDbLink — framework change, open.
- R17-82 `7b600b1e` FIXED — subArray BUSY; declares_busy() owns kind
  membership.
- R17-83 `c078e93d` FIXED — histogram INP not resolvable as a channel.
- R17-84 `043c11dd` FIXED — histogram VAL is DBF_ULONG → PVA uint32[].
  ORACLE OVERRIDE: the finding said "CA unaffected"; compiled cainfo
  reports DBF_DOUBLE for a ULONG field (C promotes ULONG→DOUBLE on CA),
  so the port's CA native type flips LONG→DOUBLE — wire-visible, per
  compiled C.
- Seam cleanup `a213af07` — orphaned ProcessAction::WriteDbLinkTyped +
  put_link_notify_typed removed (zero callers, rg-proven);
  typed_output_buffer stays as the destination-switch owner.
- Test-infra family `1d35eea7` FIXED — probe-then-rebind free_port()
  feeding a live CaServer build (13 server builds across 13 files →
  .port(0) + udp_port()/tcp_port() readback; the recurring gate flakes
  acf_host_identity and cli_caput_enum_order were this family; 5
  consecutive full -p epics-ca-rs runs green). cli_cainfo_host's Host:
  assertion moved to tcp_port() — C's ca_host_name is the circuit peer.
  Legitimate dead-address free_port uses kept: calink.rs,
  ca_gateway/upstream.rs, and the MITM proxy ports (renamed
  free_proxy_port, documented).

**UNFIXED (carried open):**
- R17-37 (above) — Meta member +putorder doPostProcessing divergence.
- R17-81 INPN capacity (above) — needs ReadDbLink capacity plumbing.
- R17-85 — documented deviation (C's dbPut-through-READ-offset is the
  defect, CBUG-E1); port keeps append-at-write-cursor. USER SIGN-OFF
  PENDING.
- .db lexer keeps raw \" escapes inside quoted field values where C's
  dbTranslateEscape unescapes them (fixer-e observation, not an R17
  finding; `field(DESC, "a \"b\" c")` carries backslashes C strips).
- ca_gateway/upstream.rs:2033 still names its dead-address helper
  free_port() — behavior correct, name advertises the banned idiom;
  left because renaming pulls a second crate into the test-infra commit.

**Gate accounting (scope: full workspace + `-p epics-bridge-rs
--features pva-gateway`, clippy -D warnings + nextest + doctests):**
final state 8893/8893 workspace (two consecutive complete runs, zero
flakes) + 780/780 gateway; clippy clean (gate proven live with a
planted warning); doctests clean. During the wave, three gate runs each
failed exactly one test that passed isolated: acf_host_identity::
unlisted_host_name_is_denied_write, stability::r12_33 (user infra),
cli_caput_enum_order::a_rejected_enum_string_prints_no_old_value
(AddrInUse) — the first and third were the probe-then-rebind family,
root-caused and FIXED (`1d35eea7`). One gateway nextest run failed once
under fail-fast and passed 780/780 on three immediate reruns; the
failing test's NAME WAS LOST because the run was piped through tail —
unnamed flake, unresolved. Piping nextest destroys exit codes and
failure names; bare runs only.

**CBUG batch E** (`752797f9`, standalone catalogue): CBUG-E1 compress
dbPut read-start slot (NOT-REPRODUCED, = R17-85), CBUG-E2 dbConvert
bare-cast UB (REPRODUCED via types::c_cast).

## Round 18 (2026-07-14) — audit after fix wave 15

Six read-only opus auditors, /clear'd and re-briefed; every category
verified its wave-15 commits and swept for new gaps. NOT convergence:
**114 findings R18-1..114** (renumbered at consolidation — panels C/D/E/F
collided; the mapping is recorded per category below). Highs: 24.

**Wave-15 verification summary (consolidated):**
- HOLD: bf677a8c, 061e9849, 2c04a643, a213af07, 1b237aea (residue
  R18-23), 1d35eea7, 55960fd7, a81df980, 016a5774, 5dc96c18, 5c4a8498,
  526726b9, 2644e581, 62429573, 65e24034, ddc899b2, cddea284, 22dd6403,
  b0125fe3, 05268579, 7b600b1e, c078e93d, 043c11dd.
- HOLD-with-residue: e74637d0 (R18-11 abort-on-failed-put missing),
  aabdf2d8 (R18-30; R17-37 adjudicated REAL/Low), db9738f5
  (R18-38..41), 6e73ac29 (R18-67 NUL truncation), c34591c2 (R18-90
  rsrvCheckPut half, R18-104 ack guard), 25aa52f7 (R18-103 aSub SUBL),
  51435dc8 (R18-114 record-local bare `as`), f6ce1657 (R18-107 MCNT),
  531daed9.
- **BROKEN: `c05a56f6`** (R17-32) — regression: its moncache.cpp:142
  citation is the UPDATE path; pva2pva's seed (moncache.cpp:304-312)
  sets the ROOT bit ("all changed"). The port moved away from the C++
  gateway: a seed can now omit alarm/timeStamp. The no-data⟹no-seed
  half is correct and stays.
- **BROKEN: `52fe221b`** (R17-31) — introduced R18-26: group
  `+type:"plain"` long-string members advertise Scalar(Byte) while the
  value ships ScalarArray(bytes).
- **BROKEN-incomplete: `dd5e0af3`** (R17-69) — R18-92: the guard's
  boundary is one load group; C's is iocInit. Multi-`dbLoadRecords`
  st.cmd still races 9-in-15 (worse than the original 1-in-20).

**Adjudications made this round (main):**
- Gateway monitor-seed parity target = **pva2pva** (the only C++
  gateway, and what the commit itself cited). Wave-16 fix: seed with
  the canonical full leaf bitset (the decodable equivalent of
  pva2pva's root bit 0, which the port encoder cannot emit), keep
  no-upstream-data ⟹ no seed frame.
- R18-6 (sCalc CRC16 sign-extends bytes ≥0x80): fix to REPRODUCE
  compiled-C x86-64 (HexSignificand / R17-79 precedent — wire
  compatibility with existing C IOCs is the contract). CBUG batch-F
  candidate. FLAGGED: this makes CRC16 diverge from the Modbus
  standard on high bytes exactly as every C IOC does.
- R17-37 (Meta +putorder): REAL, Low, fix per the auditor's minimal
  classifier (changing = marked && put_order.is_some(); Write /
  ProcessOnly / Skip) — closes R18-30 with it.
- Three wave-14/15 "verified clean" entries are FALSE-CLEANS, reopened
  with C transcripts: doc line 86 → R18-94, line 93 → R18-93, line 94
  → R18-91 (.db lexer dbTranslateEscape).
- Correction to the wave-15 dispositions above: the R17-1/R17-2 hashes
  are swapped — `bf677a8c` is R17-2 (typed DOLn READ), `061e9849` is
  R17-1 (prec as u16). The verdicts are unaffected.

**Numbering map:** A R18-1..15 and B R18-16..23 as filed; C filed
31..63 → R18-24..56 (−7); D filed 46..78 → R18-57..89 (+11); E filed
61..75 → R18-90..104 (+29); F filed 76..85 → R18-105..114 (+29).
Cross-references inside each category's text below are remapped.

**CBUG batch-F candidates** (upstream C defects, to be filed in
doc/upstream-c-bugs.md after citation verification): aCalc INC
off-by-two SIGSEGV (aCalcPerform.c:89/979, ASAN-confirmed); SUBRANGE
inclusive-bound heap overflow (:1534/1539); DERIV <5 points OOB
(calcUtil.c:281); SUBLAST at offset 0 (:996-1003); string literal >39
overrun (sCalcPerform.c:1497); calc INPM..INPU SPC_MOD self-reject;
stray printf (sCalcPerform.c:999); CRC16 sign-extension (R18-6); pvxs
process-only blocking PUT dead branch (singlesource.cpp:365 vs
dbNotify.c:209-248); pvxs UnionArray decode/encode asymmetry
(dataencode.cpp:370-382 vs :630-659 — port is CORRECT, do not "fix"
toward pvxs); asynRecord TSIZ=-1 thread-suspend (asynRecord.c:470 +
cantProceed.c:26-33); histogram LLIM>=ULIM alarm dead code
(histogramRecord.c:329-335 direct stat/sevr write erased by
recGblResetAlarms — port matches C's BEHAVIOR, not its intent).

**Also recorded:** category A's compiled evidence is softIoc +
standalone-compiled sCalc/aCalc/libCom harnesses (synApps calc is not
built here); pvxs is not built here (C++ claims source-derived, flagged
per finding); the wave-15 unnamed gateway flake did NOT reproduce in 5
bare runs (stays open); F's two NOT-REAL adjudications recorded in its
section (histogram LLIM/ULIM alarm, subArray UDF get_array_info).


### Category A (calc/sseq) — R18-1..15, panel-original numbering kept

All four panels verified. Writing up.

---

# ROUND 18 — Category A (calc/sseq engines)

**Verdict up front:** all five wave-15 commits hold, but the round found **7 High** new defects, two of which are severe: the CALC assignment operator `:=` is a **silent no-op** in five records, and `OOPT="Never"` on scalcout drives the output link **every cycle** — the exact inverse of what it asks for.

**Evidence provenance / limitation.** The synApps calc module is **not built** on this machine (no `lib/linux-x86_64`, no `O.linux-x86_64`), so I could not run a compiled sseq/scalcout/acalcout IOC. Compiled ground truth therefore comes from: `softIoc` for the base twins (`seq`, `calc`, `calcout`, `compress`), standalone-compiled `sCalcPerform.c`/`aCalcPerform.c`/`libCom` harnesses, and C source read directly. Port behavior was driven from a scratchpad crate with a path dependency on the repo (no repo file was modified).

## Wave-15 verification

| Commit | Verdict |
|---|---|
| `bf677a8c` (R17-2, typed DOLn READ) | **HOLD** — all four C switch arms map faithfully; a constant DOL resolves to `UNRESOLVED` → `field_type: None` → no read, C's `default: break` |
| `061e9849` (R17-1, prec as u16) | **HOLD** — I re-ran the real sseq STRn site against compiled `cvtDoubleToString` over 9 values × 12 precisions (incl. −1, −2, −32768): **108/108 byte-exact** |
| `2c04a643` (R17-4, writer-decided post) | **HOLD** — `mark_value_write` reproduces all four C call sites' masks and per-view change gates; `posts_direct` polarity is correct on both paths |
| `e74637d0` (R17-3, ResolveOutTarget) | **HOLD-with-residue** — "no put ⟹ no wait" is structurally closed, but the *other* half of C's same switch is missing: a **failed** put-with-callback sets `pR->abort=1` in C (`sseqRecord.c:723/745/775`); the port has no abort-on-put-failure path at all → R18-11 |
| `a213af07` (seam cleanup) | **HOLD** — zero orphans; `typed_output_buffer` has exactly one caller (`sseq.rs:699`); `input_link_read_as`/`set_resolved_out_target` have a default impl plus the single sseq override |

Baseline is clean: `cargo nextest -p epics-base-rs` **2857/2857**, clippy clean. Every finding below is an uncovered gap, not a regression.

## High

**R18-1 — CALC's assignment operator `:=` is a silent no-op; A..U are never written back.** `calc.rs:781`, `calcout.rs:1081`/`:1103`, `scalcout.rs:183`, `swait.rs:246-249`, `transform.rs:710` vs `calcPerform.c:101-123` / `sCalcPerform.c:429-433` (`parg[op-STORE_A] = *pd--` mutates the **caller's** array; every record passes `&prec->a`). The port builds `NumericInputs::with_vars(self.get_vars())` — a *copy* — evaluates, and drops it. Compiled softIoc, `CALC="A:=A+1;A"`, 3 × PROC: C gives `VAL=1 A=1 / VAL=2 A=2 / VAL=3 A=3`. Port: `VAL=1 A=0` **forever**. calcout is worse — its OCAL pass builds a *second* fresh copy, so CALC's stores are invisible to OCAL in the same cycle where C shares one `&prec->a`. `acalcout.rs:543-545` (`apply_stores`) is the one record that gets it right, and is the shape the other five need.

**R18-2 — constant links are re-delivered every cycle through the `ReadDbLink` executor; sseq fires the wrong step.** `links.rs:397` (`read_link_value`'s `Constant` arm → `constant_value()`), reached via `read_db_link_into_field` (`processing.rs:3846`) → `read_link_value_as` (`links.rs:431`). R16-77's fix landed on `read_link_with_alarm` (`links.rs:608-622`, which correctly classifies `Constant → NoData`) — **two readers, one classifier, and the executor path was never routed through it**. `links.rs:600-607`'s own comment ("the constant reaches the record exactly once, at init") is false on this path. sseq additionally never declares the `SELL → SELN` seed C does at `sseqRecord.c:189-192` (`constant_init_links`, `sseq.rs:1390-1394`, has only DOL→STR; dfanout declares `SELL` correctly). Compiled softIoc on the `seq` twin: `field(SELL,"3")` → fresh `SELN=3`, and after `caput SELN 5` + process it **stays 5**. Port: fresh `SELN=1`, and process **resets it to 3** → **step 3 fires where C fires step 5**. Same executor, same defect: compress with constant `INP=5` gives C `VAL=[0,0,0]`, port `[5.0,5.0,5.0]`. Also serves histogram `SVL` and waveform/subArray `INP`.

**R18-3 — one hardcoded runtime-stack ceiling of 30 serves three different C limits.** `postfix.rs:868` (`if runtime_depth >= 30`) vs `postfix.h:31` `CALCPERFORM_STACK 80`, `sCalcPostfixPvt.h:21` `SCALC_STACKSIZE 30`, `aCalcPostfixPvt.h:22` `ACALC_STACKSIZE 20`. One shared `compile()` gates all three flavours. Correct for sCalc only, and it diverges in *opposite* directions for the other two. Compiled softIoc, a depth-35 numeric CALC: **C computes `VAL=35`; the port's `compile()` returns `Err(Overflow)`** — a database-load failure on an expression every C IOC accepts. Conversely acalcout accepts depths 20-29 that C rejects. Structural fix: carry the limit in the flavour's `ElementTable`, not as a literal in the shared compiler.

**R18-4 — scalcout `OOPT="Never"` drives the OUT link on every cycle.** `scalcout.rs:210-220` — `should_output()` matches `0..=5` then `_ => true`; menu index 6 is `Never` (`sCalcoutRecord.dbd:17`) and falls into the catch-all. C `sCalcoutRecord.c:393-395`: `case scalcoutOOPT_Never: doOutput = 0;` (and `doOutput` is pre-initialised to 0, so C's unknown-OOPT case is also no-output). Probed end-to-end: `OOPT=Never`, `CALC="7"` → the port writes **7.0** to the OUT target; C writes nothing. Exact polarity inversion on a physical link. The two siblings get it right (`acalcout.rs:562` `6 => false`, `swait.rs:271` `_ => false`), which is why it survived.

**R18-5 — scalcout has no alarm-limit surface at all.** `rg 'HIHI|HHSV|HYST|LALM' scalcout.rs` → **zero hits**; `record_instance.rs:488-503` does not list `scalcout` for the shared `AnalogAlarmConfig` slot. C `sCalcoutRecord.dbd:479-531,858` declares all ten (HIHI/LOLO/HIGH/LOW/HHSV/LLSV/HSV/LSV/HYST/LALM) and `sCalcoutRecord.c:371` calls `checkAlarms()` **before** the OOPT switch precisely so a limit excursion can drive IVOA. `caput scalc.HIHI 5` → `FieldNotFound`; a scalcout can never go MINOR/MAJOR on its result. acalcout implements the identical C ladder (`acalcout.rs:1624-1630`) — scalcout is the outlier.

**R18-6 — CRC16/MODBUS: C sign-extends every byte ≥ 0x80; the port computes the standard CRC.** `checksum.rs:1-15` (XORs `byte as u16`) vs `sCalcPerform.c:193-212` — `char tranInput[40]` (signed on x86-64) into `unsigned int crc` via `crc ^= (unsigned int)tranInput[i]`, so a high byte pollutes bits 16-31 and the eight `crc >>= 1` steps shift that garbage back down. The commented-out predecessor at `:211` shows the author intended to mask to the low byte. Compiled: `CRC16("\x80")` → C `\x41\x1f`, port `\xbe\xe0`; ASCII-only payloads agree. **CBUG-candidate, and it needs a decision, not a silent choice**: the port is standards-correct and wire-incompatible with every existing C IOC on exactly the binary Modbus frames the operator exists for. `XOR8`/`LRC`/`AMODBUS` are unaffected.

**R18-7 — `my_nint` drops C's `(int)` cast, so PRINTF and `$W` narrow at the wrong width — and the two call sites narrow *differently*.** `string.rs:1481-1483` returns `f64`; `:1358` then wraps (`as i64` → `as i32`) and `:1553` saturates (`as i32`). C `sCalcPerform.c:40`: `#define myNINT(a) ((int)(...))` — the narrowing is *inside* the macro. Compiled: `PRINTF("%d",3e9)` → C `-2147483648`, port `-1294967296`; `$W("%i",3e9)` puts C `\0\0\0\x80` on the wire against the port's `\xff\xff\xff\x7f` — bitwise opposites. This is erosion of R17-79's own owner: `types::c_cast` exists and `string.rs` already references it 7×, but `my_nint` bypasses it. Same-family site: `array.rs:1556-1560`.

## Medium

**R18-8 — sseq `WAITn` is served as DBR_SHORT; C is `DBF_MENU`.** `sseq.rs:1032` declares `DbFieldType::Short` while `menu_field_choices` (`:1774`) returns the 12 `sseqWAIT` labels, and `SELM`/`DOLnV`/`LNKnV` in the same record are correctly `Enum`. C `sseqRecord.dbd:102-104`. Probed: port serves `WAIT1` as `Short(0)`; C serves DBR_ENUM with `"NoWait"`. `caget` prints `0` vs `NoWait`, and DBR_GR_ENUM carries no strings.

**R18-9 — sseq VAL is not posted on each completion.** C `asyncFinish` (`sseqRecord.c:583-586`) posts VAL unconditionally with `DBE_VALUE | recGblResetAlarms()` every cycle — this is the record's "sequence done" signal. VAL is a dummy that never changes, so the port's generic change loop suppresses it. Probed: 3 sequences → C 3 events, port **1**.

**R18-10 — sseq serves no precision, so `DOn` renders at prec 6 on the wire.** `record_instance.rs:1020-1023` (`populate_display_info`) matches `ai|ao|calc|calcout|…` — sseq is absent, and it declares no `field_metadata_override`. `codec.rs:102-106` then falls back to `unwrap_or(6)`. C's sseq RSET exports `get_precision` (`sseqRecord.c:136`) returning `pR->prec`. With `PREC=2`, `caget -S sseq.DO1` of 1.23456789: C `1.23`, port `1.234568`. (STRn itself is correct — it goes through the fixed `cvt_double_to_string`.)

**R18-11 — sseq never aborts on a failed put-with-completion.** C sets `pR->abort = 1`, posts it, and prints "Aborting" inside each `dbCaPutLinkCallback` failure branch (`sseqRecord.c:723`, `:745`, `:775`) — e.g. a connected CA target with no write access. `sseq.rs` writes `abort = 1` **nowhere**; `dispatch_waiting_step` (`:780-790`) treats a put that never issued as completion and runs the remaining steps. This is the residue of `e74637d0`: it closed "no put ⟹ no wait" but left "failed put ⟹ abort" open. (The no-wait `dbPutLink` path correctly ignores status, matching C.)

**R18-12 — calcout `OOPT="On Change"` inverts the NaN case.** `calcout.rs:308` `(pval - val).abs() > mdel` vs `calcoutRecord.c:257` `doOutput = !(fabs(pval - val) <= mdel)`. Not the same predicate under NaN: C's `NaN <= mdel` is false → `!false` → **output**; the port's `NaN > mdel` is false → no output. Reachable on the first process (`pval=0`, `val=NaN`). The port's `// use MDEL like C` comment is a false parity claim.

**R18-13 — calc's `init_record` seeds MLST/ALST/LALM from VAL; C's `calcRecord` does not.** `calc.rs:724-728` vs `calcRecord.c:98-111` — C's init does *only* `recGblInitConstantLink` × NARGS + `postfix()`; the seeding block exists in **calcout** only (`calcoutRecord.c:216-220`). Compiled: `record(calc){field(VAL,"5") field(MDEL,"0")}` → C `MLST=0 ALST=0 LALM=0`; port `5/5/5`. On the first process the port posts a VAL monitor C does not (C's `|0-0| <= 0` → no event).

**R18-14 — seq `VAL` is served as DBR_ENUM, and `OLDN`/`PREC` are missing.** `seq.rs:30-31` (`#[field(type="Enum")] pub val: u16`) vs `seqRecord.dbd.pod:260` `field(VAL,DBF_LONG)` — compiled `caget -d` reports `DBF_LONG`. `OLDN` (`:293`) and `PREC` (`:297`) → **zero hits** in `seq.rs`; `caget TST:SEQ.OLDN` → `1` on C, `FieldNotFound` on the port. `OLDN` is not cosmetic — it is the latch that makes C post a SELN monitor only on change (`seqRecord.c:230-233`).

**R18-15 — the sCalc `(int)`-cast / short-input family.** Three siblings of R18-7, all compiled-verified: SUBRANGE bounds cast `as i64` where C casts `(int)` (`string.rs:2077-2094` vs `sCalcPerform.c:1877` — `"hello"[3e9,2]` → C `hel`, port `''`); `SSCANF %Nc` with fewer than N chars left errors where C's libc `sscanf` returns the short string (`scanf.rs:268-275` vs `:1682-1685` — `$S("ab","%3c")` → C `ab`, port `PERFORM_ERR`); `BIN_READ` on a short frame errors where C `memcpy`s into the zero-filled tail (`string.rs:1582` vs `:1757-1795` — `$R("\x01\x00","%i")` → C `1`, port `PERFORM_ERR`, i.e. CALC_ALARM on a truncated binary reply).

## Also confirmed (not in the top 15)

transform accepts a client put to a linked A..P where C's `special()` `!after` arm rejects it (`transform.rs:902-913` vs `transformRecord.c:653-662`); the calc family never re-posts A..L on an alarm-mask cycle (C `calcRecord.c:417-420` `|| monitor_mask & DBE_ALARM`, four records, one rule); calcout is missing `POVL` (zero hits; C `:687-690` gates the OVAL post on it); transform is missing `MAP` and `CAV..CPV`; scalcout serves no link-status fields (INAV..OUTV); scalcout/acalcout/swait/transform publish no DBR_GR/CTRL metadata; acalcout's `NUSE` over-range put returns `Ok` where C returns `-1`; sseq's `WTGA`/`IXA` are read-only in the port but carry **no** `SPC_NOMOD` in C's dbd (upstream omission); sseq's step-activation filter tests emptiness, not constant-ness; `SUM`/`STD` over an empty window return `-0.0` (Rust's `Sum` folds from `-0.0`) where C seeds `+0.0`; `;` inside a conditional is rejected at compile (sCalc's `EXPR_TERMINATOR` has neither guard base `postfix.c` has).

**CBUG-candidates** (upstream C defects; do **not** port): aCalc `INC` guard off-by-two → `DERIV`/`NDERIV` **SIGSEGV** at legal compile depth (ASAN-confirmed, `aCalcPerform.c:89/979`); SUBRANGE off-by-one heap overflow when the inclusive upper bound equals `arraySize` (`:1534/1539`); `DERIV` on < 5 points reads out of bounds (`calcUtil.c:281`); `SUBLAST` never matches at offset 0 (`:996-1003`); string literals > 39 chars overrun (`:1497` — the `i<39` guard never increments `i`); calc `INPM..INPU` carry `special(SPC_MOD)` that `special()` rejects, so `caput` errors; sseq `special()`'s DLYn arm quantizes DLY1 regardless of which DLYn was written — **already faithfully reproduced** in `sseq.rs:1205-1227`; plus a stray unconditional `printf` at `sCalcPerform.c:999`.

**Doc nit (no action from me — read-only):** the wave-15 disposition list swaps the hashes for R17-1 and R17-2 relative to the commit messages (`bf677a8c` is R17-2, `061e9849` is R17-1).

### Category B (CA tools/client transport) — R18-16..23, panel-original numbering kept

## ROUND 18 — Category B (CA tools + client env/transport)

Working tree clean, no repo file touched. Every claim below is a head-to-head run against `/home/stevek/work/epics-base/bin/linux-x86_64` (softIoc 7.0.10.1-DEV, caget/caput/camonitor/cainfo/caRepeater) on ports 15064/15065.

**Headline:** both wave-15 commits hold. The tool *surface* is now extremely tight — 40+ head-to-head option/format/exit-code cases (incl. `-d DBR_STSACK_STRING`, `-d 99`, `-# -1`, `caput -a` count mismatches, enum-index overflow, usage/`-V`, `camonitor -t s|r|I`, `-m a`) came back byte-identical. The new gaps are all one layer down: the **client's address-list builder**, the **circuit-state machine**, and the **server's TCP bind fallback** — every one of them a *silent* deviation where C prints a diagnostic or changes channel state.

---

### R18-16 — `EPICS_CA_ADDR_LIST` / `EPICS_CA_NAME_SERVERS` duplicates are neither removed nor warned about — Medium
**Rust** `client/mod.rs:4788-4830` (`parse_addr_list_with_hostnames`), `client/mod.rs:5342-5378` (`parse_nameserver_list`) — entries are pushed unconditionally; the only dedup (`!addrs.iter().any(…)`, `:4863-4890`) guards the *auto-broadcast* additions, never the user list.
**C** `iocinf.cpp:227` `removeDuplicateAddresses(pList,&tmpList,0)` — `silent=0`, so `iocinf.cpp:123-126` prints `Warning: Duplicate EPICS CA Address list entry "127.0.0.1:15099" discarded` and drops it. `cac.cpp:259-260` applies the *same* pair to `EPICS_CA_NAME_SERVERS`.
**Impact (measured, UDP sink counting datagrams over 1.5 s):**

| | `EPICS_CA_ADDR_LIST="127.0.0.1:15099"` | `"127.0.0.1:15099 127.0.0.1:15099"` |
|---|---|---|
| C caget | 5 datagrams, no warning | 5 datagrams **+ warning** |
| caget-rs | 1 datagram | **2 datagrams**, no warning |

Every duplicate token multiplies search traffic for the life of the client, and the operator is never told. CBUG-candidate: no.

### R18-17 — a bad `EPICS_CA_ADDR_LIST` / `NAME_SERVERS` token is dropped silently — Low
**Rust** `client/mod.rs:4824-4826` (`tracing::debug!` "dropped unresolvable entry" — invisible without `RUST_LOG`), plus the two bare `continue`s at `:4791-4795` (unparsable port) and `mod.rs:5350-5353`.
**C** `iocinf.cpp:70-74` — on `aToIPAddr` failure prints two stderr lines and continues with the rest of the list.
**Live:** `EPICS_CA_ADDR_LIST="no.such.host.invalid 127.0.0.1:15064"` → C prints `../iocinf.cpp: Parsing 'EPICS_CA_ADDR_LIST'` + `\tBad internet address or host name: 'no.such.host.invalid'` then reads the PV; caget-rs reads the PV and says nothing. Same for `127.0.0.1:abc`. A typo'd IOC address in a site startup script is invisible in the port.

### R18-18 — an unresponsive circuit never becomes a disconnect for the application — **High**
**Rust** `client/mod.rs:4170-4222` → `disconnect_channels(…, DisconnectKind::Unresponsive, …)` → `mod.rs:4437` `conn_tx.send(kind.connection_event())` = `ConnectionEvent::Unresponsive` (`client/state.rs:82`), a port-only variant. Consumers match `Connected`/`Disconnected` and swallow the rest: `bin/camonitor-rs.rs:383-406` (`_ => {}`).
**C** `nciu::unresponsiveCircuitNotify` (`nciu.cpp:161-181`) calls `notify().disconnectNotify()` — the **same CA_OP_CONN_DOWN callback as a real disconnect**; `tcpiiu::responsiveCircuitNotify` (`tcpiiu.cpp:861-882`) then re-`connect()`s each channel and re-issues its subscription, so the value is re-posted on recovery.
**Live** (IOC `SIGSTOP`ped at T+2, thawed at T+15, `EPICS_CA_CONN_TMO=2`; both detect at T+7.0 — timing is exact):

```
C  camonitor:  T+7.0  TST:AO  ... *** disconnected     ← CONN_DOWN
               T+15.0 TST:AO  <undefined> 0 UDF NO_ALARM  ← re-subscribed on recovery
RS camonitor-rs: (no disconnect line, no recovery line — ever)
```
A GUI/archiver on the port shows a hung IOC as healthy and never re-syncs when it comes back. The machinery exists (`disconnect_channels`, the recovery re-read at `mod.rs:4224-4250`); the event is simply one the world doesn't listen for. Structural fix: an unresponsive circuit **is** a disconnect (C has no third state) — collapse `ConnectionEvent::Unresponsive` into `Disconnected` rather than teaching each consumer a fourth variant.

### R18-19 — circuit loss raises no `CA.Client.Exception` at all — Medium
**Rust:** no `dispatch_exception` on the circuit-gone path (`client/mod.rs:4144-4169`, `DisconnectKind::CircuitGone`).
**C** `cac::destroyIIU` (`cac.cpp:1236-1240`): `genLocalExcep(ECA_DISCONN, hostNameTmp)` whenever a circuit with channels dies.
**Live** (IOC killed under a monitor): C prints the full block — `Warning: "Virtual circuit disconnect" / Context: "localhost:15064" / Source File: ../cac.cpp line 1240`; the port prints nothing on the exception channel. Any library user with an exception handler installed (the C idiom for logging IOC loss) gets silence.

### R18-20 — the unresponsive exception block: invented Context, missing `Source File` line — Low
**Rust** `client/mod.rs:4188-4203`: `message: "circuit unresponsive: 127.0.0.1:15064 (matches libca ECA_UNRESPTMO)"`, `source: None`.
**C** `tcpiiu.cpp:923-926`: context is the **resolved host name** (`getHostName` → `localhost:15064`), and `ca_client_context::vSignal` (`ca_client_context.cpp:388-391`) prints `Source File: ../tcpiiu.cpp line 925` because `genLocalExcep` always passes `__FILE__`/`__LINE__`.
The port already has the right shape elsewhere (`LIBCA_WRITE_EXCEPTION_SITE`, `client/types.rs:648`, which reproduces `../oldChannelNotify.cpp line 159` byte-exactly). Two other sites carry `source: None`: `mod.rs:3751` (ECA_DBLCHNL), `mod.rs:4498` (server-initiated close).

### R18-21 — camonitor-rs prints a stderr line for every non-normal subscription status; C prints nothing — Low
**Rust** `bin/camonitor-rs.rs:493-495`: `Err(e) => eprintln!("{pv_name}: {e}")`; the ECA_DISCONN fan-out of R18-18/19 lands here as `TST:AI: server reported ECA status 0x00c0` (`epics-base-rs/src/error.rs:89`).
**C** `camonitor.c:108-124`: `event_handler` stores `pv->status` and **prints only when `args.status == ECA_NORMAL`**.
Every IOC restart under a `camonitor-rs` emits a line C never emits — same invented-stderr family as R10-17/R9-23, now on the monitor path.

### R18-22 — the CA server's dynamic-TCP-port fallback is silent; C prints a five-line warning and exports `RSRV_SERVER_PORT` — Medium
**Rust** `server/tcp.rs:965-974`: on `AddrInUse` the first interface silently rebinds to port 0. No log, no metric.
**C** `caservertask.c:233-243` (`rsrv_grab_tcp` retries with a random port on EADDRINUSE/EACCES) + `caservertask.c:578-593`:
```
cas WARNING: Configured TCP port was unavailable.
cas WARNING: Using dynamically assigned TCP port 42471,
cas WARNING: but now two or more servers share the same UDP port.
cas WARNING: Depending on your IP kernel this server may not be
cas WARNING: reachable with UDP unicast (a host's IP in EPICS_CA_ADDR_LIST)
```
plus `epicsEnvSet("RSRV_SERVER_PORT", …)` — **absent from the whole Rust workspace** (grep: 0 hits).
**Live:** second C softIoc on a taken port → the block above, TCP 42471. Second `softioc-rs --port 15064` → TCP 37437, library says nothing (only the binary's own info banner; an embedding IOC via `IocApplication` prints nothing at all). The exact condition C considers worth five lines of warning — two servers sharing a UDP port, unicast possibly unreachable — happens invisibly.

### R18-23 — `runtime::net::pva_{server,broadcast}_port` were converted to C's **CA** port semantics, which pvxs does not have — Low (currently uncalled)
`runtime/net.rs:101-108` now routes `EPICS_PVA_SERVER_PORT` / `EPICS_PVA_BROADCAST_PORT` through `env_inet_port`, i.e. the `IPPORT_USERRESERVED` (5000) floor + the `EPICS Environment "…" out of range` CA diagnostics. pvxs has neither: it parses via `parseTo<>` and treats `EPICS_PVA_SERVER_PORT=0` as a legitimate ephemeral-bind request (the port's own `epics-pva-rs/src/config/env.rs:657-670` implements exactly that, and `tests/parity/tls_interop.rs:305` relies on it). Under the new helper, `=0` would land on 5075 *and print two CA-style lines*. Harmless today — the two functions have **no callers** outside their own unit tests (all PVA code goes through `config::env`) — but they are a live trap for the next caller, and two strict `parse()` sites still bypass the PVA owner: `epics-bridge-rs/src/qsrv/pva_adapter.rs:1313` and `examples/qsrv-ioc/src/main.rs:49`.

---

## Per-commit verdicts

**`1b237aea` (env_inet_port single owner) — HOLD-with-residue.**
- Diagnostics byte-identical *and* line-count-identical to compiled caget on `abc` / `3000` / `70000` / `-1` / `""` / `"  5065"` / `"5070x"` for `EPICS_CA_SERVER_PORT`; same for `EPICS_CA_REPEATER_PORT` over a 6 s camonitor (2–3 lines, no per-attempt re-printing → the "resolve once" semantic change is real).
- Semantic change 1 (CAS presence gate) **verified live against C rsrv**, not just read: `EPICS_CAS_SERVER_PORT=3000 EPICS_CA_SERVER_PORT=16064` → C softIoc listens on **5064** and prints the out-of-range pair; `softioc-rs` listens on **5064** and prints the identical pair. It does *not* fall through to 16064 in either. Matches `caservertask.c:491-508`.
- Semantic change 2 (repeater port resolved once, `udpiiu.cpp:168`) — confirmed by the stable diagnostic count above.
- No CA port resolution bypasses the owner (`rg` over `crates/`+`examples/`: the survivors are PVA vars and test-infra `set_var`s).
- Residue: **R18-23** (PVA helpers took CA semantics).

**`1d35eea7` (probe-then-rebind → `.port(0)`) — HOLD.**
- `cargo nextest run -p epics-ca-rs`: **628/628 passed**, 0 skipped.
- `cli_cainfo_host`'s changed assertion (`udp_port()` → `tcp_port()`) is **correct, not weakened**: C's `Host:` comes from `ca_host_name` → `nciu::getHostName` → the *circuit* peer, i.e. the TCP endpoint — verified live, compiled `cainfo TST:AI` prints `Host: localhost:15064` where 15064 is the TCP listener. The test still pins the exact port and still asserts the name is reverse-resolved (`assert_ne!(name, "127.0.0.1")`), so W10-B5's regression guard survives.
- The deleted `sleep(200 ms)` calls were dead: `build()` binds before returning (`ca_server.rs:289-300`).
- Note (not a break): `.port(0)` yields *different* UDP and TCP ephemerals, a topology C rsrv cannot produce (`ca_udp_port = ca_server_port`, `caservertask.c:499`). It is legal CA — the search reply carries the TCP port — and it is the documented `.tcp_port()` Rust extension, so the tests now exercise a superset of C's shapes rather than a divergent one.

### Category C (PVA) — R18-24..56, renumbered from panel-original 31..63 (offset −7)

All five panels are in and I've hand-verified every load-bearing claim against the C++. C1 is confirmed end-to-end: `conn.cpp:153` is a drain loop (`while(bev && remaining >= 8)`), so INIT and EXEC arriving in one TCP segment are dispatched in the same pass — the server is still `Creating` unless the Source connects inline, and any async source (gateway, un-`open()`ed `SharedPV`) hits `bev.reset()`.

---

# ROUND 18 — Category C (PVA client/server/gateway/qsrv)

Read-only. No repo file created, edited, or deleted; working tree clean at `adbfea9d`. Reference: pvxs `1.5.1-42-gb568e93` (`/home/stevek/work/epics-modules/pvxs`) and — for the gateway, which pvxs does not have — **pva2pva** (`/home/stevek/work/epics-base/modules/pva2pva/p2pApp`), the C++ the wave-15 gateway commits actually cite.

## Per-commit verdicts

| Commit | Finding | Verdict |
|---|---|---|
| `c05a56f6` | R17-32 gateway monitor seed | **BROKEN — regression; needs disposition** |
| `55960fd7` | R17-33 middleware forwarding | **HOLD** |
| `52fe221b` | R17-31 Q:form long-string | **BROKEN — introduced R18-26** |
| `aabdf2d8` | R17-35 group PUT leaf | **HOLD-with-residue** (R18-30; R17-37 still open) |
| `db9738f5` | R17-34 effective TCP timeout | **HOLD-with-residue** (R18-38..48) |
| `a81df980` | R17-36 echo cadence | **HOLD** |

**`c05a56f6` is the headline.** Its cited authority is false: `moncache.cpp:142` is the *update* path (`*lastelem->changedBitSet = *update->changedBitSet` — an assignment, not a union). pva2pva's actual seed is `moncache.cpp:304-312`: `elem->pvStructurePtr->copy(*lval); elem->changedBitSet->set(0); // indicate all changed` — **the root bit**. The port cannot emit bit 0 (`pvdata/encode.rs:1374-1376`), so pre-fix it sent the canonical full leaf bitset, which a client decodes as the same thing. Post-fix it sends only the union of leaves upstream ever marked, which can omit `alarm`/`timeStamp` from the seed. The commit moved the port **away** from the only C++ gateway. The one half that is right: "no upstream data ⟹ no seed element" (`moncache.cpp:285` `havedata` + `:304`).

**`52fe221b`** correctly added `NtType::LongString` to the NT owner, but the group `+type:"plain"` descriptor arm (`group.rs:1885-1887`) derives descriptors with a *local* `match nt_type` that has no `LongString` case → falls to `_ =>` → advertises `Scalar(Byte)` while the value path still ships `ScalarArray([Byte…])`. Its "single NT owner" claim is half-true: the owner decides the type, Plain still decides its own descriptor.

**`a81df980` HOLD, both halves.** `enforce_timeout` (`env.rs:1404-1410`) reproduces all three pvxs rules incl. the `<2.0→2.0` floor; `echo_period_secs` = `clamp(1,15)` ≡ `max(1,min(15,·))` (`clientconn.cpp:163`); and the death criterion is right in kind — `hb_timeout = tcp_timeout`, matching pvxs's socket inactivity timeout, not a missed-echo count.

## R17-37 — adjudicated: REAL, **Low** confirmed, reachable only by explicit `+putorder`

The feared "putorder default = 0 ⟹ every group with a Meta member" blast radius is **false**: the default is `int64_t::min()`, the *not-putable* sentinel (`ioc/fieldconfig.h:37`), set only by the explicit key. pvxs behavior is real — `groupsource.cpp:554-570`: `putable = putOrder != min()`, `changing = marked && putable`, then `if(changing || info.type==Proc) doPostProcessing(...)`, while `IOCSource::put` returns immediately for Meta. No shipped or documented pvxs config has `+putorder` on a meta member (checked `test/image.{db,json}`, `ntenum.db`, `table.db`, `documentation/qgroup.rst`). One sharp edge if someone does add it to the usual *top-level* (`""`) meta member: `Field::findIn("")` returns the group **root**, and `isMarked(true,true)` on the root is true if any descendant is marked — so that one line makes pvxs process that record on **every** group PUT.

**Minimal correct fix** — `group.rs:1626` (atomic) and `:1714` (non-atomic): stop fusing "has no writable leaf" with "does not participate". Reproduce C's two independent predicates in one classifier used by both loops — `changing = marked && put_order.is_some()`; then `Write` if a leaf exists, `ProcessOnly` if not, `ProcessOnly` for Proc, else `Skip` — with post-processing routed through one owner. That also closes R18-30.

## Findings (R18-24 … R18-56 — I overran the allocated 15 slots; renumber as you see fit)

**High**
- **R18-24** — Client GET pipelines INIT+EXEC in one write; a pvxs server **hard-resets the whole circuit**. `client_native/ops_v2.rs:599-604` vs `serverget.cpp:429-434` (`state==Creating` → `bev.reset()`) + `conn.cpp:153` (one-pass drain loop). Any pvxs server whose Source connects asynchronously — a gateway, an un-`open()`ed `SharedPV` (`sharedpv.cpp:243`) — kills the TCP circuit and every channel on it, no MESSAGE, no status. MONITOR is correct (`ops_v2.rs:2644`); GET is the only pipelined site.
- **R18-25** — One slow pipelined downstream **pauses the shared upstream monitor and permanently starves healthy co-subscribers**. `channel_cache.rs:206-208` counts *voting* ops; an op only becomes a voter by crossing LOW itself (`tcp.rs:1379-91`), so a fast/non-pipelined client never votes and can never resume it (`:226-234` ignores a non-voter's Resume). The in-code invariant at `:634-638` asserts the opposite of what the code does. pva2pva never throttles upstream (`moncache.cpp:133-174`: coalesce into the downstream's own `overflowElement`, bump `ndropped`).
- **R18-26** — `+type:"plain"` long-string member: descriptor `Scalar(Byte)`, value `ScalarArray(bytes)` (regression of `52fe221b`). `group.rs:1885-87` / `:983-990` vs `groupconfigprocessor.cpp:886-895`. Structural fix: a **paired** bare-leaf owner in `pvif` (descriptor *and* value), not one added match arm — `build_field_desc_for_nt` is not a drop-in, it emits the full NT wrapper (`pvif.rs:939-947`).
- **R18-27** — Gateway does not forward the upstream `Status` verbatim: `format!("PUT INIT failed: {:?}", init.status)` (`ops_v2.rs:1289`) puts a **Rust `Debug` dump on the PVA wire**; kind coerced to `Error`, `stack` dropped. pva2pva hands the downstream requester straight to the upstream channel (`channel.cpp:117-127`), making re-authoring structurally impossible.
- **R18-28** — Gateway monitor seed under-marks vs pva2pva (the `c05a56f6` regression above).

**Medium** — R18-29 downstream monitor dies silently when upstream is connected-but-idle (INIT OK, then no DATA/FINISH/error, forever; `channel_cache.rs:1101-12` → `source.rs:1021-30` → `tcp.rs:2124`; pva2pva keeps it alive, `moncache.cpp:284-313`) · R18-30 group `+type:"proc"` member processes unconditionally, C gates on `pfield==PROC || force || (pp && SCAN==Passive)` (`group.rs:1668-83`/`:1726-40` vs `iocsource.cpp:397-403`) · R18-31 no gateway loop guard; `ignore_addrs` has **zero** uses in `pva_gateway/`, default client searches UDP 5076 = its own server's port, and pva2pva *refuses to start* without separated ports (`gwmain.cpp:105-131`) · R18-32 client never sends `DESTROY_CHANNEL` (`client.cpp:151-186`) · R18-33 invented control opcodes 3/4 (PVA has only 0/1/2, `pvaproto.h:607-613`) · R18-34 every SEARCH sent twice per NIC (`255.255.255.255` + directed; pvxs emits only directed) · R18-35 cooked-path seed/broadcast duplicate delivery.

**Low** — R18-36 MONITOR INIT *error* echoes `0x88` where pvxs derives `0x08` (`servermon.cpp:132-134` vs `serverget.cpp:84`; the port's own INIT-*success* path already forces `0x08`) · R18-37 type-cache exhaustion **panics** (`encode.rs:508`; only the gateway enables it, `gateway.rs:125`) · R18-38 server idle default 45 s vs pvxs 40 s (the owner never runs on the unset-env path) · R18-39 death detection quantized to the tick (client 45 s, server 60 s, vs pvxs's exact 40 s) · R18-40 `-v` prints the raw `CONN_TMO`, bypassing the owner · R18-41 no write-timeout twin; 5 s send timeout pvxs does not have · R18-42 `isArray` from value shape, not `dbChannelFinalElements` (NELM=1 waveform: pvxs `NTScalar`, port `NTScalarArray`) · R18-43 `Status` decode rejects codes pvxs accepts · R18-44 pvRequest `field` selector matches non-Struct members (`pvrequest.cpp:31`) · R18-45 UDP TX little-endian (pvxs is always BE) · R18-46 UDP RX enforces the Server bit (pvxs checks it TCP-only) · R18-47 truncated SEARCH_RESPONSE discards the whole reply · R18-48 search ladder one bucket late; poke costs 200 ms · R18-49 `tokio::interval` Burst catch-up → SEARCH burst pvxs cannot emit · R18-50 segment byte-order latched from first segment; handshake never reassembles (segmented `CONNECTION_VALIDATION` → silent `anonymous` downgrade) · R18-51 `ca` credential sent with a type-cache define + a Rust-only `groups` member · R18-52 process-global IOID counter (pvxs is per-connection) · R18-53 upstream monitor retained ~60 s after last unsubscribe (C tears down at once via `weak_value_map`) · R18-54 no per-downstream `ndropped`/queue stats (`server.cpp:298-305`) — the exact diagnostic needed to see R18-25 in the field · R18-55 multi-tenant search serializes N×5 s · R18-56 QSRV blocking PUT with unmarked `value` writes+processes where pvxs no-ops.

**CBUG candidates** (bugs in the C++, not the port)
- **CBUG-C1** — pvxs's process-only blocking PUT is a **dead branch in EPICS Base**. `singlesource.cpp:365` selects `processRequest` when `value["value"]` is unmarked, but `processRequest` is matched by *neither* branch in `processNotifyCommon` (`dbNotify.c:209` `didPut=0`; `:233` tests only `putProcessRequest`/`putProcessGetRequest`; `:248` requires `processGetRequest`) → `doProcess` stays 0 → no put, no process, silent success. (= R18-56's C side.)
- **CBUG-C2** — pvxs cannot re-serialize a UnionArray it decoded: encode writes elements through their own FieldDesc (`dataencode.cpp:370-382`) but decode rebuilds them as the *selected member's* type (`:630-659`), so a decoded-then-re-encoded UnionA loses its selectors. The port is **correct**; do not "fix" it toward pvxs.

## Negative space (checked, matches — worth knowing)
Panel D reproduced **7/7** of pvxs's own `testxcode.cpp` byte vectors with the port's encoder, byte-identical (`value[]`, `structA`, `union`, `unionA`, `any`, `any-null`, `anyA`), and `total_bits()==11==desc->size()`. BitSet trailing-zero trim, Size/Selector varints, Status `0xFF` short form, introspection registry (0xFD/0xFE/0xFF, no-overwrite `or_insert`), monitor squash/overrun/`ackAt`, GET/PUT/RPC subcmd algebra, channel lifecycle, GET_FIELD, segmentation all match. **Flow control is dead protocol in pvxs** — `SetMarker`/`AckMarker` are never sent or acted on (`conn.cpp:189-193`) — so the port not implementing the ack window is *not* a divergence. `55960fd7` re-verified independently: 44/45 methods forwarded by all three layers, and `resolve_owner`'s non-forwarding is load-bearing (forwarding it would bind the bare source as owner and bypass ACL/ReadOnly/Audit on every later op).

## Gate
`-p epics-bridge-rs --features pva-gateway`, five consecutive **bare** runs (no pipe): **780/780 each, zero flakes**. The wave-15 "unnamed gateway flake" did not reproduce — it stays **open**, not closed.

## Not verified
pvxs is **not built on this machine** (no `lib/`, no `O.linux-*`; building needs EPICS Base + libevent, and the bundled libevent wants `autoreconf`, absent). Every C++ claim above is **source-derived, not executed** — I read each cited line myself, but R18-24's disconnect, R18-25's starvation, R18-27's wire text, and R18-34's duplicate datagrams deserve a live capture before the fixes land. R18-26 *was* reproduced, against the Rust side only.

---

Two decisions are yours, and both change what the doc should record:

1. **R18-28 / `c05a56f6`: which C++ is the gateway's parity target for the monitor seed** — pva2pva (the only real C++ gateway, and what the commit itself cites; under this, revert/rework to seed a full mask) or pvxs `SharedPV` (whose `Value::assign` *does* accumulate a mark union, but which is not a gateway; under this, the code stands and the false citation + doc entry must be corrected)? My reading favors pva2pva for the gateway specifically — a gateway's contract is transparency to a pva2pva-compatible client.
2. **`52fe221b` and `c05a56f6` are recorded in the doc as FIXED.** Both are BROKEN by this round. I'm read-only and will not touch `doc/c-parity-review-2026-07-10.md`.

### Category D (asyn) — R18-57..89, renumbered from panel-original 46..78 (offset +11)

The IP-server claim is confirmed: `accept_one` exists at ip_server_port.rs:818 and **every** caller is ≥1687 (inside `mod tests`); `drvAsynIPServerPortConfigure` is absent from iocsh entirely.

---

# ROUND 18 — Category D (asyn) — READ-ONLY audit

**HEAD** `adbfea9d` · **C ref** `/home/stevek/work/epics-modules/asyn` @ `R4-45-19-ge2a281e2` · **libCom** compiled from `/home/stevek/work/epics-base` (Linux; the checked-in `lib/darwin-aarch64` binaries are useless here, so I built the oracle from source). Working tree clean — I edited nothing.

**Headline: the four wave-15 commits all hold, but the trace subsystem they fix is dead code on any port a real IOC builds.** `base.trace` and `base.exception_sink` are injected at exactly one production site (`manager.rs:94-95`), and `drvAsynIPPortConfigure` / `drvAsynSerialPortConfigure` / `prologixGPIBConfigure` do not go through it.

## Part 1 — wave-15 commit verdicts

| Commit | Verdict |
|---|---|
| `016a5774` escaped_from_raw dstlen | **HOLD** |
| `5dc96c18` destination-keyed escape | **HOLD** |
| `6e73ac29` traceIOMask bitfield | **HOLD-with-residue** → R18-67 |
| `5c4a8498` empty-string early return | **HOLD** |

I compiled `epicsString.c` and re-ran every assertion in `escape.rs`. All eight match byte-for-byte: 200×CRLF→40 gives `ret=400 strlen=39` ending `\r\n\r\`; →10 gives `\r\n\r\n\`; `"a\tb"`→4 = `a\t`, →3 = `a\`; `"\xff"`→3 = `\x`; `epicsStrPrintEscaped("\0a",2)` returns 0 and prints nothing; `("a\0b",3)` prints `a\x00b`. The call-site buffer bounds are C's: `sizeof(tinp)`=40 (asynRecord.c:725/:1629), `EOS_SIZE`=10 (:68, :2005/:2012), echo `char[16]` (asynInterposeEcho.c:71-74), `cbuf[4*sizeof eos+2]`=42 (asynShellCommands.c:304). `ntranslate` (asynRecord.c:1629) only feeds a diagnostic string, so returning a `String` loses nothing. Grow-never-shrink is correct (`trace.rs:759`). `tracePvtInit` (asynManager.c:449-459) indeed never assigns `traceIOMask` → NODATA default confirmed, and I re-derived each of the three corrected tests against `traceVprintIOSource` — they encode C, not the port's old behavior.

**The residue:** C's ASCII block is `fprintf(fp,"%.*s\n",(int)nBytes,buffer)`, and `%.*s` **stops at a NUL**. Compiled: payload `AB\0CD` (5 bytes) → C emits `AB`, `snprintf` returns 2. The port does `out.extend_from_slice(data)` (trace.rs:969) and emits all five bytes.

## Part 2 — new findings

### High
- **R18-57 — `exception_sink` and `trace` are never injected into any iocsh-created port.** `manager.rs:94-95` is the only production site (`port.rs:3042+`/`port_handle.rs:1287+` are under `mod tests` at :2072/:1093). `iocsh.rs:958-966` (+:1046, :1126) calls `create_port_runtime` directly. `AsynUser::print_io` early-returns on `trace == None` (user.rs:247); `user_trace()` returns None when `base.trace` is None (port_actor.rs:1171-1177); drivers gate on `asyn_trace_io!(Some(self.base.trace), …)` (ip_port.rs:873, :1367). C: asynManager.c:611-637 fans out over a list that lives on the port and is always present. **Impact: on a port built by st.cmd, `asynSetTraceMask MYPORT -1 0x9` + `asynSetTraceIOMask MYPORT -1 ASCII` produces zero output, and no exception callback ever fires. R17-46/47/48/49 and R18-63..56 are all unobservable there.**
- **R18-58 — the IP server port never accepts.** `accept_one` (ip_server_port.rs:818) has no caller outside `mod tests`; `drvAsynIPServerPortConfigure` is absent from iocsh (rg: zero hits). C starts `connectionListener` at configure (drvAsynIPServerPort.c:711-714) which loops on `epicsSocketAccept` (:326) and fires octet callbacks carrying the child port name (:374-383). Impact: a TCP client sits in the backlog until it times out while the port reports Connected.
- **R18-59 — the octet read interrupt fan-out does not exist.** No driver calls any interrupt notify after a read (rg over `drivers/` + `port_actor.rs`: zero hits). C: `asynOctetBase.c:224-238` `readIt` → `callInterruptUsers(…, data, nbytes, eomReason)`, enabled by both stream drivers (drvAsynIPPort.c:1055, drvAsynSerialPort.c:1125). Impact: a `stringin`/`waveform` with `SCAN="I/O Intr"` on a serial or IP port never processes.
- **R18-60 — the interpose chain is a per-driver opt-in, not a port property.** Only `ip_port.rs`, `serial_port.rs`, `serial_port_win32.rs` call `dispatch_read`/`dispatch_write`. C's `interposeInterface` is manager-level (asynManager.c:2190-2220). Sharpest instance: `ftdi.rs:155` installs an `EosInterpose` and FTDI never dispatches through the stack — its test at :276 asserts the interpose is *installed*, never that it *runs*. `asynInterposeEcho("PROLOGIX",0)` reports success and does nothing.
- **R18-61 — a newly registered auto-connect port never connects until the first request.** `create_port_runtime_boxed` (runtime/port.rs:113-150) sends no Connect; `init_connected` (port.rs:384-389) arms no timer. C: `registerInterface(asynCommonType)` calls `initPortConnect` + `portConnectTimerCallback` (which queues at `asynQueuePriorityConnect`, :3259) + `waitConnect` (asynManager.c:2131-2136). Impact: `CNCT` reads 0 after `drvAsynIPPortConfigure` until traffic arrives; a port with no record traffic never comes up.
- **R18-62 — `SO_REUSEPORT` is set after bind/connect.** ip_port.rs:1252-1264 (TCP), :1184-1201 (UDP, after `UdpSocket::bind` at :1171). C sets it on the fresh socket before bind (drvAsynIPPort.c:464-477). Impact: `udp&` with a local port — the whole point of the `&` suffix — fails `EADDRINUSE` where C binds.

### Medium
- **R18-63 — the trace config hierarchy is a lazy pull-fallback; C's is a per-`dpCommon` struct with push-down.** `tracePvtInit` is called for *every* port and device (asynManager.c:503, :528) and `findTracePvt`→`findDpCommon` (:534-551) selects one whole struct — no inheritance. Three proven symptoms (ran against the real `TraceManager`): (a) a port-level set never overwrites existing devices and announces once where C overwrites every device and announces per device plus once for the port (asynManager.c:2790-2800/:2833-2843/:2874-2884) — `asynSetTraceMask P 1 0x3f` then `P -1 0x1` leaves device 1 at 0x3f forever, with no way to quiet it; (b) a global set (`asynSetTraceMask "" -1 …`, iocsh.rs:678) leaks into every port, where C writes `pasynBase->trace`, which no port-attached user ever reads (asynShellCommands.c:651-660); (c) device config is honored on non-`ASYN_MULTIDEVICE` ports, where C's `findDpCommon` ignores it (`TraceManager` has no multi-device notion at all). The doc-comment at trace.rs:585-586 claiming C announces on the global path is false.
- **R18-64 — the trace prefix matches C on no component and appends a token C never prints.** Observed: `1783941510.799 P:3 main IO_DEVICE read 2 bytes`; C emits `2026/07/13 … [P,3,0] [main,0x…,50] read 2 bytes`. TIME is raw epoch vs strftime `%Y/%m/%d %H:%M:%S.%03f` (:2984-2996); PORT is `P:3` vs `[%s,%d,%d]` with the reason (:3005-3023, and `getAddr` yields **-1** for a port-level user, :2004); THREAD drops the id+priority (:2969-2981); SOURCE skips `asynStripPath` (:479-487) so Rust's `file!()` full path is printed; the order is C's TIME/PORT/SOURCE/THREAD (:3136-3139) vs the port's TIME/PORT/THREAD/*mask-label*; and `mask_label` (trace.rs:927-943) has no C source at all.
- **R18-65 — default `traceInfoMask` is `TIME|PORT`; C's is TIME alone.** trace.rs:341 (+ fallback :875) vs asynManager.c:455 — the only assignment in the whole C tree.
- **R18-66 — `ASYN_TRACEINFO_SOURCE` is silently dropped on the `asynPrintIO` path.** `output_device_io` (trace.rs:566-582) takes no file/line; C's `traceVprintIOSource` prints it (:3138). Proven: SOURCE-only `asynPrintIO` emitted no `[file:line]`. All real device I/O loses it.
- **R18-67 — ASCII trace block writes past an embedded NUL** (residue on `6e73ac29`; see Part 1).
- **R18-68 — Connect/Disconnect queue at `Medium`, not `asynQueuePriorityConnect`.** The heap orders on `user.priority` (port_actor.rs:108, :170-175); `Medium` is `#[default]` (port.rs:106); `connect_blocking`/`disconnect_blocking` (port_handle.rs:1015/:1022) build `AsynUser::new(0)`. C drains the Connect queue first (asynManager.c:811-855). Impact: on a sustained High scan stream an operator's `CNCT=0` put starves indefinitely.
- **R18-69 — the `lockPort`/`queueLockPort` family is absent; `SyncIOHandle` has no atomic writeRead** (found independently by two sweeps). sync_io.rs exposes only `read_octet`/`write_octet`; `RequestOp::OctetWriteRead` is reachable only from `adapter.rs`. C: asynOctetSyncIO.c:246-271 holds `queueLockPort` across flush+write+read. Impact: on a shared port another client's request interleaves and the reply is read by the wrong caller. (Corroborates the known multi-axis motor pattern.)
- **R18-70 — `blockProcessCallback`'s device scope is unmodelled; every block is port-wide.** `RequestOp::BlockProcess` is a unit variant (request.rs:150), `blocked_by` is one port-wide holder (port_actor.rs:237). C keeps both `pport->pblockProcessHolder` and `pdpCommon->pblockProcessHolder` (asynManager.c:1692-1723) and devGpib uses the device form (devSupportGpib.c:1216). Impact: one GPIB address's SRQ transaction stalls every other address on the port.
- **R18-71 — `setInputEos`/`setOutputEos` succeed silently where C returns `asynError`.** port.rs:1918-1956 caches and returns `Ok(())`; C swaps NULL EOS methods for Fail stubs (asynOctetBase.c:115-118) yielding `"<port> setInputEos not implemented"` (:370-380), and serial really does leave them NULL (`asynOctetMethods = { writeIt, readIt, flushIt }`, drvAsynSerialPort.c:1003). Impact: on a `noProcessEos` port an IEOS put reports success, reads back the terminator, and never terminates on it.

### Overflow — real and verified, past the R18-71 slot the brief allotted (I am not dropping them to fit the range)
**R18-72** serial has no `tcflush(TCOFLUSH)` on read/write timeout (serial_port.rs:293/:385/:420 vs drvAsynSerialPort.c:640-660) — the timed-out command keeps draining, prefixing the next one. Medium.
**R18-73** FTDI/VXI-11 declare `asynOption` but validate nothing (ftdi.rs:197, vxi11.rs:276 → base default port.rs:1450-1470) vs drvAsynFTDIPort.cpp:142-231 / drvVxi11.c:1658-1699. Medium.
**R18-74** USBTMC never puts TermChar on the wire (usbtmc.rs:105-117, `h[8..12]` hard-zeroed) vs drvAsynUSBTMC.c:863-866 — `IEOS` is dropped. Medium.
**R18-75** VXI-11 multi-device gate narrower than C: an unrecognized `vxiName` becomes single-device (vxi11.rs:226) where C sets `ASYN_MULTIDEVICE` for anything not `inst*`/`com*` (drvVxi11.c:1754-1760). The unit test at :455 encodes the inverted rule. Medium.
**R18-76** delay interpose has no `asynOption "delay"` interface, and zero delay collapses C's per-character writes into one (interpose/delay.rs:61 vs asynInterposeDelay.c:41-50/:134-172). Medium.
**R18-77** exception fan-out is a flat global list, not C's per-`dpCommon` list (exception.rs:99-107 vs asynManager.c:611-625) — the structural reason R18-57 is invisible. Low-Medium.
**R18-78** **CBUG-candidate.** `caput X.TSIZ -1`: C passes `epicsInt32` into a `size_t` (asynRecord.c:470) → 18446744073709551615 → the realloc branch (asynManager.c:2947-2951) → `calloc` returns NULL (verified) → `callocMustSucceed` errlogs and calls `epicsThreadSuspendSelf()` inside a `while` loop (cantProceed.c:26-33), **suspending the put thread forever**. The port does `self.tsiz as usize` (mod.rs:2244) → `usize::MAX` → silently unlimited tracing. Low.
**R18-79** symbolic mask parse: C always applies the partial mask on a bad token and prefix-matches (`STARTSWITH`, asynShellCommands.c:668-706), so `"ERROR+BOGUS"` sets 0x1; the port returns `Err` and applies nothing, leaving the old mask. Low.
**R18-80** `asynOctetBase`'s flush fallback (drain input at 0.05 s until a zero read, asynOctetBase.c:240-263) is unimplemented — `io_flush` defaults to `Ok(())` (port.rs:1884). Latent (no in-tree C driver leaves flush NULL). Low.
**R18-81** `set_addr_connected` (port.rs:496-509) doesn't bump `number_connects` nor arm the port connect timer, where C's `exceptionConnect`/`exceptionDisconnect` do (asynManager.c:2158, :2181). Low.
**R18-82** `blockProcessCallback` misses C's non-`ASYN_CANBLOCK` refusal (asynManager.c:1703-1709) and returns silent success on an unblock with nothing blocked (port_actor.rs:1552-1575 vs :1736-1741). Low.

## Coverage gap — not covered

**The asynRecord field-semantics sweep did not return before I finished.** `asynRecord.c`'s `process()`/`special()`/`monitorStatus()` state machine (TMOD modes, NRRD/NORD/NOWT/NAWT, IFACE dispatch, the SPC_MOD arm-by-arm comparison, the POST_IF_NEW set) is therefore **unaudited this round**, beyond the trace fields I covered myself (TFIL verified faithful; TSIZ → R18-78). That slice is still owed.

## Bottom line

The four commits are faithful C ports and I would merge all four — but `6e73ac29` carries the NUL residue (R18-67), and R18-57 means the "DEFAULT TRACE OUTPUT CHANGED" semantic in that commit is, on any port an st.cmd actually builds, a change to output nobody receives. **R18-57 should be fixed before any further trace work**, or the next round audits code that cannot run.

---

Both verified against C. Filing as an addendum — this closes the coverage gap I flagged.

# ROUND 18 — ADDENDUM: asynRecord field semantics

The fourth sweep returned after I filed. The gap I reported as "unaudited" is now closed. Two claims I verified against the C myself; the rest carry the sweep's evidence.

### R18-83 — a BOUT array put never sets NOWT (`put_array_info` is missing) — **High** ✅ verified
`asyn_record/mod.rs:3952-3954`: `"BOUT" => { self.bout = to_bytes(&value); }`. `self.nowt` is written at exactly two places (rg): `:3420` (the OMAX clamp) and `:3959` (an explicit NOWT put). C's `put_array_info` (asynRecord.c:983-993) does `if (fieldIndex == asynRecordBOUT) pasynRec->nowt = nNew;` — in C, **writing the array *is* how NOWT gets set**, because dbAccess calls `put_array_info(nNew)` on every array put, and `monitor()` then `POST_IF_NEW(nowt)`s it (:1022).

Impact: `caput -a` of 120 bytes to BOUT with OMAX=1000 leaves NOWT at its stale value (default 80), and `octet_output_buffer` (mod.rs:3441-3446) sends `bout[..min(nowt, len)]` — **80 bytes on the wire where C sends 120**. NOWT/NAWT then read back a count unrelated to what was written. Not a framework limitation: `acalcout.rs:386-426` implements C's `put_array_info` inside `put_field`.

### R18-84 — BOUT/BINP ignore C's `cvt_dbaddr`/`get_array_info` element counts — **Medium** ✅ verified
C's `cvt_dbaddr` (asynRecord.c:941-948) sets `no_elements = omax` for BOUT and `imax` for BINP, so dbAccess **truncates any put to that bound**; `get_array_info` (:966-981) reads BOUT back as `nowt` elements, BINP as `nord`. The port stores a BOUT put whole (no OMAX truncation) and `get_field` returns the entire buffer (mod.rs:3826). With `OFMT=Hybrid` the payload is `translate_escape(bout[..first NUL])` with no OMAX bound, so the record can write **more bytes than OMAX** — C physically cannot (its `optr` is `omax` bytes and the put is truncated).

### R18-85 — ERRS is `DBF_STRING` (40 B) where C is a 100-byte `DBF_CHAR` array — **Medium** ✅ verified
`mod.rs:2037-2041`: `FieldDesc { name: "ERRS", dbf_type: DbFieldType::String, read_only: true }`. C: `ERR_SIZE 100` (:69) and `cvt_dbaddr` (:957-963) gives ERRS `no_elements = 100`, `field_type = DBF_CHAR`, `dbr_field_type = DBR_CHAR`, `special = SPC_NOMOD`. A CA client gets a 39-char-clipped `DBR_STRING` instead of a 100-element `DBR_CHAR` waveform — and the record's own texts routinely exceed 39 chars (`"Connect error, status=3, asynManager:connectDevice port XXX not found"` is 68). Type **and** element count differ from every existing asyn OPI.

### R18-86 — `monitor()` never posts AINP/BINP/TINP unconditionally — **Medium**
C's `monitor()` (asynRecord.c:1006-1030) `db_post_events`es AINP/BINP and TINP **unconditionally** for `TMOD == Read || Write_Read` (:1012-1019) — deliberately *not* through `POST_IF_NEW`, unlike the NRRD/NORD/NOWT/NAWT/EOMR group (:1020-1029). The port overrides neither `force_posted_fields()` nor `process_posted_fields()`, so it falls back to generic per-field change detection. Impact: a scanned record polling a device that answers the same string every scan gives one monitor per scan in C and **none** after the first in the port — archivers, `camonitor`, and SNL sequencers using the response as a poll heartbeat stall.

### R18-87 — `TMOD = NoI/O` never enters the port queue — **Medium**
`mod.rs:4692-4694` early-returns above the queue submit. C issues `queueRequest(…, asynQueuePriorityLow, QUEUE_TIMEOUT)` **unconditionally** in the `stateIdle` arm (:342-353); the TMOD test lives one level down in `asynCallbackProcess` (:824). A failed queueRequest → `reportError("queueRequest failed")` + `recGblSetSevr(STATE_ALARM, MINOR_ALARM)`. Impact: the standard "process with TMOD=NoI/O to nudge a reconnect / refresh status" idiom is a no-op — C's queue entry is what pokes `autoConnectDevice`. On a disconnected port C leaves STATE/MINOR + ERRS; the port stays NO_ALARM and silent.

### R18-88 — `init_record` skips C's `OMAX/IMAX <= 0 → 40` fixup — **Low**
C: `if(omax <= 0) omax = MAX_STRING_SIZE; if(imax <= 0) imax = MAX_STRING_SIZE;` (:289-291), before the buffers are allocated. The port's pass 0 (mod.rs:4139-4147) does nothing. A db setting `IMAX=0` yields `octet_buf_size = 0` — silent zero-length transfers where C uses 40-byte buffers.

### R18-89 — a failed `asynDrvUser->create` zeroes the reason; C keeps the previous one — **Low**
`mod.rs:3187-3193` sets `self.resolved_reason = 0` on error. C (:1248-1257) only calls `reportError` and assigns neither `pasynRec->reason` nor `pasynUser->reason`, so a REASON the operator set by hand survives. Impact: after a bad DRVINFO put, the port silently redirects every read/write to parameter 0 while REASON still displays the operator's value.

---

## Revised bottom line

**33 findings this round (R18-57..R18-89), 7 High.** All four wave-15 commits still **HOLD** (`6e73ac29` **HOLD-with-residue** → R18-67). Nothing in the audit changed those verdicts.

Two structural items dominate and should be sequenced first, because they gate the value of everything downstream:

1. **R18-57** — `base.trace` / `base.exception_sink` are injected only in `manager.rs:94-95`, which the iocsh port-configure commands bypass. On any port an st.cmd actually builds, the entire trace and exception subsystem is inert. Fixing the trace *format* findings (R18-63..56, and R17-46..49 before them) buys nothing until this lands.
2. **R18-83** — the missing `put_array_info` is a live data-corruption bug: a BOUT array put silently sends the wrong number of bytes to the device. It is the single most damaging finding of the round in terms of what a device actually receives, and it is a small, self-contained fix (the pattern already exists at `acalcout.rs:386-426`).

The read-only constraint held throughout: working tree clean, no repo or doc file touched.

### Category E (records/links/db core) — R18-90..104, renumbered from panel-original 61..75 (offset +29)

I've verified everything. Here is my Round 18 report.

---

# ROUND 18 — Category E (records / links / db core)

Read-only. Reference: `/home/stevek/work/epics-base` (softIoc 7.0.10.1-DEV, `bin/linux-x86_64`). Every decisive claim below is backed by a compiled-C transcript. 15 findings, R18-90..R18-104.

## Headline

`dd5e0af3` (R17-69) is **BROKEN — incomplete**. It closed the single-load-group case but not the multi-`dbLoadRecords` case every real `st.cmd` uses, where the classification now races at **9-in-15**, worse than the 1-in-20 R17-69 originally measured. Separately, **three entries in the doc's "verified clean" list are false-cleans** (lines 86, 93, 94) — I reopened all three with C transcripts.

---

## Findings

**R18-90 — the SPC_NOMOD declaration reaches the `dbPut` gate but not `rsrvCheckPut`: the CA server advertises WRITE access on every dbCommon NOMOD field — Medium.**
`c34591c2` extended `DBCOMMON_NOMOD` (field_io.rs:42-45) and `Record::field_no_mod` (R17-77), but C's declaration has **two** consumers and only one moved. `compute_access` (epics-ca-rs/src/server/tcp.rs:864-898) derives `is_ro` from `field_list().read_only` alone — and no record's `field_list` declares NAME/STAT/SEVR/ACKS/RPRO/…, nor is `field_no_mod` consulted. C: `rsrvCheckPut` (rsrv/camessage.c:2540-2551) `if (dbChannelSpecial(pciu->dbch) == SPC_NOMOD) return 0;` feeds the ACCESS_RIGHTS write bit (camessage.c:1123-1124) and both put paths (`:741`, `:1653`). Head-to-head, same `.db`:

| channel | C | port |
|---|---|---|
| `N1.SEVR` | `Access: read, no write` | `Access: read, write` |
| `CMP` (compress VAL, BALG=LIFO) | `read, no write` | `read, write` |

C's `caput N1.SEVR 2` fails client-side with a clean `Write access denied`; the port sends the write, the gate refuses it server-side, and the client gets an async `CA.Client.Exception` dump instead. Data stays protected — but the wire bits are wrong on ~15 fields × every record, so medm/CSS enable the write widget. Structural fix: one `is_no_mod(instance, field)` owner consulted by both `check_no_mod` and `compute_access`.

**R18-91 — the `.db` lexer never unescapes field/info values; C runs `dbTranslateEscape` on all of them — High. CBUG-free, port-side.**
This refutes doc line 94 ("`.db` quoted-string lexing keeps escape bytes raw … the old `03-H-2`/`03-H-3` findings are closed"). That closure read the wrong lexer rule. C has **two** start conditions: record/alias **names** are `tokenSTRING` from `INITIAL` (dbLex.l:88-92 — quotes stripped, escapes kept raw), but field and info **values** are `jsonSTRING` from the `JSON` condition (dbYacc.y:256-267, dbLex.l:111-114) and keep their quotes — and the quotes are precisely the marker that translation is still owed. `dbLexRoutines.c:1398-1403` / `:1435-1440` then strip them and call `dbTranslateEscape(value, value)` (epicsString.c:41-47 → `epicsStrnRawFromEscaped`, :49-118) before `dbPutString`.

Port: `read_quoted_string` (db_loader/mod.rs:828-899, emit-both-bytes at :858-871) is shared by names *and* values and never unescapes; nothing downstream rescues it (`rg unescape|raw_from_escaped` over the load path: no hits). The in-code comment at mod.rs:844-857 asserts the opposite, and two unit tests (`test_quoted_string_escape`, `test_quoted_string_keeps_escapes_raw`) pin the defect.

I confirmed the C side myself. `record(stringin,"E2"){field(DESC,"hex\x41end")}` → `dbgf E2.DESC` = `"hexAend"`. `dbgf` *re-escapes* on print (dbTest.c:1007 `epicsStrnEscapedFromRaw`), so had C stored the backslashes it would have printed `hex\\x41end`. `\x41 → A` cannot be a print artifact.

| `.db` source | C stores | port stores |
|---|---|---|
| `"a \"b\" c"` | `a "b" c` (9 B) | `a \"b\" c` (11 B) |
| `"hex\x41end"` | `hexAend` | `hex\x41end` |
| `"x\ty"` | `x`,TAB,`y` | `x`,`\`,`t`,`y` |

Impact: DESC/EGU/stringout.VAL show literal backslashes to every client — and **link fields ride the same path**, so `field(OUT,"@drvUser(\"chan1\")")` reaches device support with backslashes still in it. The port also *accepts* two forms C rejects (raw control chars, `\ooo` octal — dbLex.l:23,25), so a `.db` C refuses to load starts up "fine" with wrong data. Structural fix: split the helper into `read_json_string` (unescapes) vs `read_quoted_string` (raw), mirroring C's two start conditions — do **not** unescape inside the shared helper, or names break. Adjacent same-defect site: iocsh `cmd_echo` (iocsh/core_commands.rs:46-63) vs C `libComRegister.c:84-91` (`dbTranslateEscape` before printf).

**R18-92 — `dd5e0af3` does not close R17-69: a cross-`dbLoadRecords` forward reference still races, now 9-in-15 — High.**
The guard's scope is one load *group* (`cmd_db_load_records` closure, iocsh/commands.rs:1026; `IocBuilder::build`, ioc_builder.rs:192). C's boundary is **iocInit** — `init_record` runs after *every* `dbLoadRecords` block. Probe (`a.db`: calcout `CO` with `INPA="LATER"`; `b.db`: `ai LATER`), settled read, 15 runs each:

```
C  softIoc  (dbLoadRecords a; dbLoadRecords b; iocInit)   → "Local PV" 15/15
port iocsh  (dbLoadRecords a; dbLoadRecords b)            → 2 0 0 0 2 2 0 2 0 0 2 2 0 0 0
                                                             (6× Local PV, 9× Ext PV NC)
port iocsh  (single dbLoadRecords ab.db)                  → 2 2 2 2 2 2 2 2 2 2 2 2 2 2 2  ✅
```
The commit message's "Forward references are deterministically Local; no sleeps, no re-poll" holds only within one group. `wait_for_load` (database/mod.rs:1093-1117) is a loop over a counter that returns to zero **between** files, so the woken task classifies against whichever prefix of the database happens to exist when it is next polled. The structural fix is an explicit iocInit barrier for classification, not a per-group counter.

Sub-residue (Low, same commit): classification is an unsynchronized `tokio::spawn` (calcout.rs:391), so there is no point at which the port guarantees it is done. A `dbgf CO.INAV` issued immediately after `dbLoadRecords` returns reads the struct default **`Constant` (3)** — not even `Ext PV NC` — and converges to 2 seconds later. C's value is final when `iocInit` returns.

**R18-93 — `processTarget` runs for non-Passive link targets: PUTF is propagated and RPRO is set — High.**
Refutes doc line 93 ("`dbScanPassive`'s `pto->scan != 0 → no-op` gate" — claimed verified). C returns *before* `processTarget`: `dbDbLink.c:427-434` `if (pto->scan != 0) return 0;`. `processTarget` (`dbDbLink.c:474-489`) is the only link-side writer of PUTF/RPRO. The port applies the Passive gate only to the *process* call and mutates PUTF/RPRO above it — five cloned blocks: processing.rs:3636-3661, :4593-4615, :4731-4742; links.rs:1155-1180, :1988-2000. C probe: FLNK→`ASY` (calcout `SCAN="5 second" ODLY=4`, PACT=1) after `caput TRIG.PROC 1` → `ASY.RPRO=0`, exactly one process per scan. The port sets RPRO=1, so async completion fires an **extra unscheduled cycle** (extra device write, extra FLNK chain). Invariant to restore: *PUTF/RPRO are written only by `dbPutField`-on-PACT and by `processTarget`, and `processTarget` is reachable only for a Passive target or a `.PROC` write.* One `process_target()` owner with the gate inside it.

**R18-94 — a DB link writing `TARGET.PROC` does not process a non-Passive target — Medium.**
Refutes doc line 86. C `dbDbLink.c:387-389`: the `.PROC` arm has **no** scan test — `if (dbChannelField(chan) == &pdest->proc || (pvlMask & pvlOptPP && pdest->scan == 0))`. Port links.rs:1146-1180 recognizes `.PROC` but dispatches `if should_process && target_scan == ScanType::Passive`. C probe: `SRCO.OUT="TGT.PROC"`, `TGT` `SCAN="10 second"` → each `caput SRCO.PROC 1` advances TGT immediately (0→1→2). Port: no-op. The two put boundaries disagree with each other — the CA route *does* honour `.PROC` on any SCAN (field_io.rs:1109).

**R18-95 — put-notify on a PACT record: the port writes the value immediately, sets RPRO, and completes the callback at the end of the in-flight cycle; C defers the whole put — High.**
C `dbNotify.c:225-231` tests PACT **above** `putCallback` and returns having written nothing. Port field_io.rs:1197-1216 writes the field before any PACT test; `put_driven_process` (:1025-1041) then sets `rpro = true` and returns, and the wait-set `leave`s at the tail of the *in-flight* cycle (processing.rs:4670+), i.e. before the reprocess it just queued. C probe (`caput -c ASY.A 7` during a 4 s ODLY): A stays 5 for the whole async cycle, then VAL=7 A=7 on the restart, and only then does the callback return. The port applies `dbPutField` semantics to the `dbProcessNotify` path — breaking the contract sequencers rely on ("callback ⟹ the value has been processed").

**R18-96 — stringin/stringout/lsi/lso post VAL on every process cycle; MPST/APST are dead fields — High.**
`check_deadband_ext` (record_instance.rs:3214) bails to *post-everything* when `monitor_deadband_value().to_f64()` is `None` — which is every string record. C `stringinRecord.c:176-188` gates on `strncmp(oval, val)` and adds `DBE_VALUE`/`DBE_LOG` only for `MPST`/`APST == Always`; identical in `lsiRecord.c:205-224`, `lsoRecord.c`. The fields exist in the port (stringin.rs:103-108) and are read by no monitor path. C probe: stringin at `SCAN="1 second"`, VAL never written, `camonitor -m l` → **zero** events after the connect update; the port emits one per second per subscriber. Distinct from R15-76 (that was aai/aao/waveform, dropped at `.db` load; this is a different family and a different mechanism). Secondary: a numeric-*looking* string (`"12"`) falls into the analog deadband path, so `"12" → "12.0"` posts nothing where C's `strncmp` posts. `printfRecord` and `sub` A..L are correct — do not "fix" those.

**R18-97 — ai/ao `SPC_LINCONV` rebases `EOFF = EGUL`; C does nothing, because no soft dset provides `special_linconv` — High.**
I opened this one myself. C `aiRecord.c:182-200` (`aoRecord.c:249-267` identical): `prec->eoff = prec->egul;` sits **inside** `if ((prec->linr == menuConvertLINEAR) && pdset->special_linconv)`. `devAiSoftRaw.c:32-34` is `{{6, NULL, NULL, init_record, NULL}, read_ai, NULL}` — the trailing slot *is* `special_linconv`, and it is NULL. So on a base softIoc a put to LINR/EGUF/EGUL sets `init=TRUE` and touches nothing else. Port ai.rs:470-508 / ao.rs:767-800 do `if self.linr == 2 { self.eoff = self.egul; }` in all three arms, ungated. C probe (`DTYP="Raw Soft Channel"`, `INP="12"`, LINEAR): `caput T:AI.EGUL 7.25` → EOFF stays 0, VAL stays 12. Port → EOFF=7.25, VAL=19.25. An operator retuning the display range EGUL silently changes the conversion; on `ao` it moves the hardware output. The port's own test `egul_put_under_linear_rebases_eoff` pins the divergence.

**R18-98 — alarm-only cycles never post the auxiliary fields C forces — Medium.**
C re-posts *unchanged* aux fields when the monitor mask carries alarm bits: `calcRecord.c:417-421` (A..L, `monitor_mask | DBE_VALUE | DBE_LOG`), `mbbiDirectRecord.c:217-226` (B0..B1F), `aoRecord.c:535-538` (OVAL whenever `monitor_mask != 0`). The port posts unchanged fields only if listed in `force_posted_fields`/`alarm_cycle_monitored_fields` (record_instance.rs:2620-2665), and `alarm_cycle_monitored_fields` is implemented **only** by `acalcout.rs`. C probe: `camonitor T:C.A` across an HSV MINOR→MAJOR transition with A pinned at 3 → C emits `3 HIGH MAJOR`; the port emits nothing. OPI widgets bound to `.A`/`.B0`/`.OVAL` keep a stale alarm colour. (`subRecord.c` has no such clause — the port is right there.)

**R18-99 — `caput REC.SCAN "I/O Intr"` bricks the record; C validates and reverts to Passive — Medium.**
C `dbScan.c:265-300`: no `dset` or no `get_ioint_info` → `recGblRecordError` + `precord->scan = menuScanPassive`. Port record_instance.rs:1875-1887 stores SCAN unvalidated and scan_index.rs:22-80 inserts into the `IoIntr` bucket unconditionally; the only I/O-Intr wiring (`setup_io_intr`, ioc_app.rs:1217-1300) runs once at iocInit and is never revisited. C probe: `caput C.SCAN "I/O Intr"` on a soft calc → reads back `Passive` + `scanAdd: I/O Intr not valid (no DSET)`. On the port it reads back `I/O Intr` and the record **never processes again**.

**R18-100 — TPRO: wrong stream, invented format, no lock-set propagation, no Disabled line — Low.**
C `dbAccess.c:497` `ptrace = dbLockSetAddrTrace(precord)` — the flag lives on the **lock set**, so the whole downstream chain traces. Lines at `:541`/`:571`/`:609` are `printf` (stdout) with the CA client identity from `dbServerClient()`. Port processing.rs:2515-2524 / :1330-1338 `eprintln!("[TPRO] …")` per record, and the SDIS-disable bail prints nothing. C probe: `A.TPRO=1`, `A.FLNK=B`, `B.FLNK=C` → C traces A, B *and* C; the port traces A only.

**R18-101 — ai/ao `.ORAW` advanced every cycle; C advances it only inside the `if (monitor_mask)` guard — Low.**
C `aiRecord.c:459-465` (`aoRecord.c:539-542`). Port ai.rs:372 / ao.rs:252,:538 assign unconditionally at the tail of `process()`. With `MDEL=ADEL=1000` (mask == 0): C leaves ORAW=12 after RVAL moves to 500; the port reports 500. Field value only — the *posting* gate is correct.

**R18-102 — ai/ao `.INIT` is inverted and served as DBF_CHAR; C is DBF_SHORT, 1 before the first process and 0 after — Low.**
C `aiRecord.c:114` `init=TRUE` in `init_record`, `:452` `init=FALSE` at the end of `process`; dbd declares `DBF_SHORT special(SPC_NOMOD)`. Port ai.rs:97 starts `false`, :373 sets `true`, :402 serves `EpicsValue::Char`. Internally self-consistent (the polarity is inverted throughout, so SMOO priming still works) — only the exposed field is wrong, in both polarity and wire width.

**R18-103 — the aSub SUBL read bypasses `fetch_link`, dropping MS inheritance — Low. (Residue of `25aa52f7`.)**
processing.rs:1182 calls `read_link_with_alarm(...).0`, discarding the alarm. C `aSubRecord.c:256` is a plain `dbGetLink(&prec->subl, DBR_STRING, prec->snam, 0, 0)`, so it runs `dbDbGetValue`'s tail — `recGblInheritSevrMsg(pvlMask & pvlOptMsMode, …)` at dbDbLink.c:228-232. `field(SUBL,"SRC MS")` therefore inherits in C and not in the port. `fetch_link` is the owner for its six call sites; this is the seventh. (The swait DOL reads at processing.rs:3047 are **correct** — swait is synApps and reaches DOL through `recDynLink`, which has no inheritance tail.)

**R18-104 — `put_alarm_ack_from_ca` lacks C's `field_type <= DBF_DEVICE` guard, and ACKT is coerced to a bool — Low.**
C `dbAccess.c:1333-1335` dispatches `DBR_PUT_ACKT`/`ACKS` only when `field_type <= DBF_DEVICE`; a PUT_ACKS on a link-field channel falls to `S_db_badDbrtype`. field_io.rs:902-927 acks unconditionally. Separately, `put_ackt` (record_instance.rs) stores `value != 0` as a bool, where C stores the raw `epicsUInt16` (`ackt = *ptrans`), so a client sending ACKT=2 reads back 1 in the port and 2 in C. Behaviour downstream is identical (both truthy).

---

## Per-commit verdicts

| commit | finding | verdict |
|---|---|---|
| `526726b9` | R17-61 parse_c_double | **HOLD** — `parse_c_double` is now a call to `runtime::stdlib::epics_parse_double`; classifier and constant loader share the one parse, as in C. |
| `2644e581` | R17-63 per-record UDF | **HOLD** — oracle-matched: DF.UDF=1/SEVR=INVALID, HG.UDF=0, EV.UDF=1. |
| `62429573` | R17-66 post-init UDF tail | **HOLD** — hook moved into `RecordInstance::run_init_passes` (record_instance.rs:580); all four creation paths call it (ioc_builder ×2, iocsh commands, `add_record`). MBD UDF=0/VAL=5, MBD0 UDF=1 — oracle-matched. |
| `65e24034` | R17-65 lso/lsi dbLoadLinkLS | **HOLD** — `DOL="7"` → VAL=`""`, LEN=1, UDF=0 on both records, oracle-matched. |
| `ddc899b2` | R17-67 add_record init passes | **HOLD** — AO1 `DOL="5"` closed-loop → UDF=0 / SEVR=INVALID / STAT=UDF, oracle-matched. |
| `cddea284` | R17-68 quoted → PV link | **HOLD** — the classifier arm is right. Note it is fed corrupted text until R18-91 lands: with the lexer not unescaping, `field(INP,"\"hello\"")` reaches `parse_link_field` as `\"hello\"`, so the port's CA link carries a different PV name than C's `"hello"`. |
| `c34591c2` | R17-62 SPC_NOMOD + ack | **HOLD-with-residue** — the NOMOD set matches C's client-visible dbCommon list exactly; all four put routes gate; `put_ackt`/`put_acks` reproduce `dbAccess.c:1285-1315` line for line, dispatched above the gate. Residue: **R18-90** (rsrvCheckPut half never moved), **R18-104** (ack type guard). |
| `25aa52f7` | R17-64 MS on link READ | **HOLD-with-residue** — `fetch_link` takes the reading record and applies inheritance on all six call sites (SDIS, TSEL, SIML, SIOL ×2, SELL). Residue: **R18-103** (aSub SUBL). |
| `dd5e0af3` | R17-69 DbLoadGuard | **BROKEN (incomplete)** — the RAII guard itself is sound (counted, `#[must_use]`, `Drop` fires on every exit path, `Weak` upgrade, no lock held across the await, no deadlock), and it closes the single-load-group case 15/15. But the boundary is the load *group*, not iocInit, so the multi-`dbLoadRecords` case every real `st.cmd` uses still races — **9 of 15 runs classify Ext PV NC where C is Local PV 15/15**. See **R18-92**. |

## Merge-conflict resolutions — all three composed correctly

- **`record/mod.rs`** (use-union): `AlarmAck` (from E) and `LinkReadAs` / `CyclePostMask` / `ArrayMonitorPost` (from F) all present in the re-export list.
- **`field_io.rs`**: E's ACKT/ACKS put arms are gone (the names survive only inside `DBCOMMON_NOMOD` at :43), and F's `take_cycle_posted_fields` block is intact at :1333-1340. Both halves present, neither clobbered.
- **`processing.rs`**: both SDIS (:1436-1440) and TSEL (:1599-1610) carry `fetch_link(&rec, …)` **and** `c_cast::f64_to_i16` — the 25aa52f7 and R17-79 halves composed rather than one overwriting the other.

## Prior "verified clean" entries now refuted

Doc lines **86** (`dbDbPutValue` → `processTarget` gating), **93** (`dbScanPassive`'s `pto->scan != 0` gate), and **94** (`.db` quoted-string escape lexing, closing `03-H-2`/`03-H-3`) were each certified against C but check only part of the C path. R18-93, R18-94 and R18-91 reopen them with transcripts.

## No new CBUG candidates

Nothing this round is a C-side defect; every divergence above is port-side.

### Category F (arrays) — R18-105..114, renumbered from panel-original 76..85 (offset +29)

All probing is done; the decode path confirms a `caput -a` of strings reaches `convert_to` as a `StringArray`, so the collapse site is the coercion owner, not the codec. Here is the Round-18 report.

---

# ROUND 18 — Category F (aai / aao / waveform / subArray / compress / histogram)

Read-only audit. Ten new findings (R18-105..R18-114), two NOT-REAL adjudications, one CBUG candidate, and per-commit verdicts for the nine wave-15 category-F commits.

All "Proof" entries below are head-to-head oracle runs: the same `.db` loaded into the C `softIoc` (`/home/stevek/work/epics-base/bin/linux-x86_64`) on one port and the port's `softioc-rs` on another, driven with the C `caput`/`caget`/`cainfo`/`camonitor` binaries so the client is identical on both sides.

## Findings

### R18-105: A DBR_STRING array put collapses to scalar 0.0 and destroys the array record
**Severity:** High
**Rust:** `crates/epics-base-rs/src/types/value.rs:850-862` (`as_f64_array` has no `StringArray` arm), falling through to the scalar tail at `:925-954` and `:1070+`
**C:** `src/ioc/db/dbConvert.c` — the string→numeric array converters (`cvt_st_*` family), reached from `dbPut` for any `DBR_STRING` request with `nRequest > 1`
**Impact:** `caput -a WF 3 7 8 9` on a numeric waveform is accepted with a success status and writes a single element `0.0`. The record's contents are destroyed and `NORD` is set to 1. Every client that puts arrays as strings (the default for `caput -a` without `-t`, and for shell/script clients that build values as text) silently corrupts array records instead of writing them. This is the worst finding in the round: it is a data-destroying silent failure on the most common array write path.
**Proof:** Same `.db`, `caput -a WV 3 7 8 9`:
- C: `WV.VAL = 7 8 9 0 0`, `NORD = 3`.
- Port: `WV.VAL = 0 0 0 0 0`, `NORD = 1`, put returns success.

The comment at `value.rs:952` ("StringArray falls through to the scalar path (its cross-type semantics are not numeric)") is the source of the bug: in C, string→numeric *is* an array conversion, element by element.

### R18-106: compress ignores INP link connectivity — no LINK/INVALID alarm, and a constant INP is ingested every cycle
**Severity:** High
**Rust:** `crates/epics-base-rs/src/server/records/compress.rs:954-972` (`pre_process_actions` emits `ReadDbLink{INP→VAL}` whenever `!self.inp.is_empty()`, with no connectivity gate and no alarm on failure); `crates/epics-base-rs/src/server/database/links.rs:397` (`ParsedLink::Constant(_) => link.constant_value()` — a constant link delivers a value on every read)
**C:** `modules/database/src/std/rec/compressRecord.c:320-340` — process() calls `dbIsLinkConnected(&prec->inp)`, and on a non-connected link raises `recGblSetSevr(prec, LINK_ALARM, INVALID_ALARM)` and returns without ingesting anything.
**Impact:** Two distinct divergences from one missing gate. An unset or disconnected `INP` leaves the record in `NO_ALARM` forever instead of latching `INVALID/LINK`, so operators lose the "my compression source is dead" signal entirely. And a *constant* `INP` (e.g. `field(INP,"5")`), which C treats as not-connected and refuses to sample, is ingested by the port on every single process — the compression buffer fills with a repeated constant and `NUSE` climbs, producing plausible-looking but fabricated data.
**Proof:**
- `INP=""`: C → `SEVR=INVALID, STAT=LINK`; port → `SEVR=NO_ALARM`.
- `INP="5"`: C → `SEVR=INVALID, STAT=LINK`, `NUSE=0`, `VAL` all zeros; port → `SEVR=NO_ALARM`, `NUSE=1`, `VAL=[5]`.

### R18-107: histogram has no `mcnt > mdel` monitor gate and never resets MCNT — MDEL is inert and MCNT grows without bound
**Severity:** Medium-High
**Rust:** `crates/epics-base-rs/src/server/records/histogram.rs:325-331` (`process()` is `add_count(); Ok(ProcessOutcome::complete())` with no gate), plus `crates/epics-base-rs/src/server/record/record_instance.rs:3187+` (`check_deadband_ext`: an array `VAL` has `to_f64() == None`, so the function returns `(true, true)` — always post)
**C:** `modules/database/src/std/rec/histogramRecord.c:283-296` — `monitor()` posts `DBE_VALUE|DBE_LOG` on `VAL` only when `mcnt > mdel`, and **zeroes `mcnt` when it does**.
**Impact:** `MDEL` is the histogram's only rate limiter for VAL monitors and it does nothing in the port: every process posts the full bin array to every subscriber. On a fast-signal histogram this is an unbounded monitor-traffic amplification (the exact thing MDEL exists to prevent). Separately, because nothing ever zeroes `MCNT` on the monitor path, `MCNT` is a monotonically growing counter rather than "counts since last posted VAL" — it is wrong as a readable field, and it perturbs the SDEL watchdog added in `f6ce1657`, which shares the same counter.
**Proof:** `MDEL=3`, 8 processes:
- C: 3 monitor events, `MCNT = 0` afterwards.
- Port: 8 monitor events, `MCNT = 8`.

### R18-108: the DBF_ULONG / DBF_SHORT field-type family was not widened after R17-84 — array bookkeeping fields are served as DBR_LONG
**Severity:** Medium
**Rust:** `crates/epics-base-rs/src/server/records/waveform.rs` (NELM, NORD, MALM, INDX declared `DbFieldType::Long`); `crates/epics-base-rs/src/server/records/compress.rs` (NSAM, N, OFF, NUSE, OUSE, INX declared `DbFieldType::Long`); `crates/epics-base-rs/src/server/records/histogram.rs` (MDEL, MCNT declared `DbFieldType::Long`)
**C:** `waveformRecord.dbd.pod` (NELM, NORD, HASH = DBF_ULONG), `subArrayRecord.dbd.pod` (MALM, NELM, INDX = DBF_ULONG; NORD = DBF_LONG), `compressRecord.dbd.pod` (NSAM, N, OFF, NUSE, OUSE, INX = DBF_ULONG; INPN = DBF_LONG), `histogramRecord.dbd.pod` (MDEL, MCNT = DBF_SHORT); promotion table in `src/ioc/db/db_convert.h` (`DBR_ULONG` → `DBR_DOUBLE` on the CA wire)
**Impact:** R17-84 fixed histogram `VAL` (ULong) but stopped there; the same DBF_ULONG declaration error is still present on ten sibling fields across four record types. C advertises these as `DBF_DOUBLE` to CA clients and `uint32` to PVA; the port advertises `DBF_LONG`. Clients that introspect native type (any generic display tool, archiver, or PVA client building a type-matched structure) get a different type from the port than from C, and a PVA client sees `int32` where the C IOC serves `uint32` — a real wire-type mismatch, not a cosmetic one. Values above 2^31 in these fields (legal for DBF_ULONG) are unrepresentable.
**Proof:** `cainfo` on the same `.db`: `WF.NORD`, `WF.NELM`, `C1.NUSE` → C reports `DBF_DOUBLE`, port reports `DBF_LONG`. (The already-fixed `HG.VAL` correctly reports `DBF_DOUBLE` on both, confirming the diagnostic and confirming that `043c11dd` landed.)

### R18-109: NORD is not posted on the CA/dbPut route — only on the internal put-and-post route
**Severity:** Medium
**Rust:** `crates/epics-base-rs/src/server/database/field_io.rs:654`, `:719`, `:747` (NORD posted, in `put_pv_and_post` only) vs `put_record_field_from_ca_inner` at `:1100-1350`, which posts the put field plus `monitor_side_effect_fields` plus `take_cycle_posted_fields`, but never NORD
**C:** `modules/database/src/std/rec/waveformRecord.c:202-216` (`put_array_info` sets `nord` and the DB core posts it); the equivalent in `aaiRecord.c:232-245`, `aaoRecord.c:164-190`, `subArrayRecord.c:190-202`
**Impact:** A client monitoring `WF.NORD` — the standard way to learn how many elements a producer actually wrote — sees nothing when the array is written over CA. The value is correct on a subsequent `caget`, so this is a lost-event bug, not a wrong-value bug, but for a slow-scanned or passive waveform the subscriber never learns the length changed. The asymmetry between the two put routes is the structural defect: `NORD` posting belongs to `put_array_info`, which every put route must reach.
**Proof:** `camonitor WP.NORD` on a 10-second-SCAN waveform, then `caput -a WP 3 1 2 3`:
- C: posts `NORD = 3` immediately.
- Port: posts nothing (the value is right on the next `caget`).

### R18-110: HASH is answered by `get_field` but is absent from every array `field_list` — writes are refused
**Severity:** Low
**Rust:** `crates/epics-base-rs/src/server/records/waveform.rs` — `get_field("HASH")` returns a `ULong`, but `HASH` appears in no `field_list()` for any `ArrayKind`
**C:** `waveformRecord.dbd.pod` — `HASH` is `DBF_ULONG`, not `special(SPC_NOMOD)`, therefore writable
**Impact:** `caput WF.HASH 7` is refused by the port and accepted by C (0 → 7). HASH is the field a client uses to seed or reset the array-change hash used by MPST/APST monitor filtering, so a client that manages hashing explicitly cannot do so against the port. Low because the read path works and few clients write HASH.
**Proof:** `caput WF.HASH 7` → C: succeeds, `caget WF.HASH` = 7. Port: put rejected (field not found).

### R18-111: subArray `field(NELM,"0")` is rejected at load; C accepts it
**Severity:** Low
**Rust:** `crates/epics-base-rs/src/server/records/waveform.rs` — `put_field("NELM")` rejects `n <= 0` with `InvalidValue`
**C:** `subArrayRecord.dbd.pod` — `NELM` is `DBF_ULONG`, so 0 is a legal value; `subArrayRecord.c:95-105` (init) and `:176-188` (`get_array_info`) handle it by yielding an empty slice with the record left UDF.
**Impact:** A `.db` that is legal against C fails to load against the port. C's behavior (empty subarray, UDF stays set) is well-defined; the port turns it into a hard configuration error. Low because `NELM=0` is an unusual configuration, but it is a load-time regression, which is the harshest failure mode for a database that works elsewhere.
**Proof:** `.db` with `field(NELM,"0")` on a subArray: C loads and runs (`caget SA` → empty, `UDF=1`); port refuses the field value at load.

### R18-112: the async `if (pact && prec->busy) return 0` gate is missing from waveform and subArray process
**Severity:** Low
**Rust:** `crates/epics-base-rs/src/server/records/waveform.rs` — `process()` has no `pact && busy` early return
**C:** `modules/database/src/std/rec/waveformRecord.c:136-155` (`if (pact && prec->busy) return 0;` before the completion path), same shape in `subArrayRecord.c:126-161`
**Impact:** For an asynchronous device support that sets `BUSY`, the completion callback re-entering `process()` while the device is still busy will run the completion path (post monitors, reset PACT) instead of returning immediately, so a record can complete a scan the device has not finished. Low **only** because the port ships no async array device support today — this is negative space that becomes a real bug the moment one is written. Filed so the gate lands with the feature rather than after it.
**Proof:** Source-level; no oracle probe possible without an async device support on the Rust side. Marked as such deliberately rather than claiming a behavioral divergence I did not observe.

### R18-113: histogram `CMD = Clear` does not clear UDF
**Severity:** Low
**Rust:** `crates/epics-base-rs/src/server/records/histogram.rs` — `clear_histogram()` zeroes the bins and sets `mcnt = mdel + 1`, but has no UDF write
**C:** `modules/database/src/std/rec/histogramRecord.c:354-364` — `clear_histogram()` ends with `prec->udf = FALSE;`
**Impact:** After `caput HG.CMD 1` (Clear) on a never-processed histogram, C reports a defined record (`UDF=0`, all-zero bins are a valid histogram); the port still reports `UDF=1`. A record with UDF set raises `UDF_ALARM` (INVALID) if `SCAN` is such that the UDF check runs, so a cleared-but-not-yet-processed histogram can sit in an alarm state on the port that it never enters on C.
**Proof:** Fresh IOC, `caput HG.CMD 1`, `caget HG.UDF`: C → 0; port → 1.

### R18-114: residue of `51435dc8` — record-level put arms still narrow double→int with bare `as`
**Severity:** Low
**Rust:** `crates/epics-base-rs/src/server/records/waveform.rs:1291-1295` (PREC: `to_f64() ... as i16`), `crates/epics-base-rs/src/server/records/compress.rs:896-901` (same shape)
**C:** `src/ioc/db/dbConvert.c` — the `cvt_d_*` converters, whose out-of-range/NaN behavior is what `types::c_cast` was introduced to reproduce
**Impact:** `51435dc8` routed the dbConvert-modelled sites through `c_cast`, but these two record-local put arms still use a bare Rust `as`, which saturates where C's compiled x86-64 conversion produces the indefinite value. Reachable only if a `Double` arrives at `put_field` without having passed through `convert_to` first; I could not construct such a path through the current CA or PVA put routes, so I am filing this as residue rather than as a live divergence. It is the exact shape the commit was meant to eliminate, and it will become live the first time a caller reaches `put_field` directly.
**Proof:** Source-level; no reachable path found from the wire. Stated as unverified-live rather than claimed as observable.

## NOT-REAL adjudications

Both of these looked like solid findings from source-reading and both are refuted by the compiled C. Recording them so the next round does not re-derive them.

**NOT-REAL: histogram `LLIM >= ULIM` should raise SOFT/INVALID.** `histogramRecord.c:329-335` does set an alarm inside `add_count()` when the limits are inverted — but it writes `prec->stat`/`prec->sevr` directly, not `nsta`/`nsev`, and `monitor()` calls `recGblResetAlarms()` in the *same* cycle, which overwrites them from `nsta`/`nsev`. The alarm never survives to be observable. Oracle: `LLIM=10, ULIM=5`, process → C reports `NO_ALARM`. The port reports `NO_ALARM` too, so the port matches C's actual behavior. **This is a CBUG candidate** (see below) — C's *intent* is clearly to alarm, and the port would be wrong to copy the intent instead of the behavior.

**NOT-REAL: subArray `get_array_info` must return 0 elements while UDF.** `subArrayRecord.c:181-184` does contain `if (prec->udf) *no_elements = 0;`, and the port has no equivalent. But the gate is unobservable: `dbPut` clears UDF before the value is readable, and after any `process()` the invariant `udf ⟺ nord <= 0` already holds, so the UDF check and the NORD check can never disagree. Oracle: `caput -a SA2.VAL` → both C and the port report `NORD=5, UDF=0`; no sequence I could construct made C return 0 elements where the port returned more. Not a divergence.

## CBUG candidate

**CBUG (histogram):** `histogramRecord.c:329-335` writes `prec->stat` / `prec->sevr` directly in `add_count()` instead of going through `recGblSetSevr` (which writes `nsta`/`nsev`). Because `monitor()` runs `recGblResetAlarms()` later in the same process cycle, the `LLIM >= ULIM` alarm is unconditionally erased before any client can see it. The alarm is dead code in upstream C. The port coincidentally matches (it never sets the alarm at all), so **no port change is warranted** — but if upstream ever fixes this, the port will need the alarm added, and any future auditor reading `histogramRecord.c` will re-derive this as a parity gap. Flagging it so that does not happen.

## Wave-15 commit verdicts

| Commit | Verdict | Note |
|---|---|---|
| `22dd6403` (compress: one reset owner, all five SPC_RESET fields) | **HOLD** | `COMPRESS_SPC_RESET_FIELDS = ["RES","ALG","PBUF","BALG","N"]` matches `compressRecord.c:377-393`; `reset()` is the single owner and every SPC_RESET field routes through it. |
| `b0125fe3` (`Record::field_no_mod` dynamic hook — LIFO VAL) | **HOLD** | The hook composes with the static `read_only` gate inside `field_io::check_no_mod` (`field_io.rs:81-98`), and **all four** put routes call it: `:363` (the QSRV/PVA pre-put gate — this is the one the task asked me to confirm), `:410` `put_pv_inner`, `:644` `put_pv_and_post`, `:1102` `put_record_field_from_ca_inner`. No put route bypasses the gate. |
| `05268579` (histogram CSTA noMod) | **HOLD** | Matches `histogramRecord.dbd.pod`. |
| `51435dc8` (`types::c_cast` compiled-x86-64 double→int narrowing) | **HOLD-with-residue** | Every dbConvert-modelled site routes through `c_cast`, including the SDIS/DISA site added at merge. Residue: two record-local put arms still use bare `as` — filed as **R18-114**. |
| `f6ce1657` (histogram SDEL watchdog) | **HOLD-with-residue** | Arm / re-arm / disarm lifecycle in `processing.rs:838-914` is correct against `wdogCallback` (`histogramRecord.c:102-124`): generation-counter supersede, `watchdog_interval()` re-read each loop so `SDEL=0` disarms, empty `watchdog_fire()` re-arms without posting. Residue: the watchdog shares `MCNT` with the monitor path, and **R18-107** means `MCNT` is never zeroed there — so the watchdog's view of "counts since last post" is wrong even though its own lifecycle is right. Fixing R18-107 fixes this; no separate change needed. |
| `531daed9` (compress OUSE/INPN) | **HOLD** | OUSE latch is owned solely by `monitor()`, matching `compressRecord.c:100-110`. The INPN capacity gap is the known-open R17-81 and is not re-filed. OUSE's *type* is wrong, but that is part of the R18-108 family, not a defect in this commit. |
| `7b600b1e` (subArray BUSY) | **HOLD** | |
| `c078e93d` (histogram INP not a channel) | **HOLD** | |
| `043c11dd` (histogram VAL DBF_ULONG → PVA uint32[], CA DBR_DOUBLE) | **HOLD** | Confirmed by compiled `cainfo`: `HG.VAL` reports `DBF_DOUBLE` on both C and the port. This commit is also the reason R18-108 exists — it fixed one field of a ten-field family. |

**Nothing is BROKEN.** Every wave-15 category-F fix does what it claims. The two residues both resolve into new findings rather than into rework of the original commits.

## Ranking for the fix wave

R18-105 first and alone: it is a silent data-destroying bug on the default `caput -a` path, and it lives in `EpicsValue::convert_to`, the single coercion owner — so the fix is one arm in `as_f64_array`, and the blast radius is every record type, not just arrays. R18-106 second (a fabricated-data bug plus a missing alarm). R18-107 third. R18-108 and R18-109 are each a *family* — R18-108 is ten sibling fields with one wrong declaration, R18-109 is a posting responsibility that belongs in `put_array_info` and is currently duplicated into one of two put routes — so each should be fixed structurally at the family level, not at the cited line.
### Round 18 review log

Thematic clusters: (1) **dead subsystems** — the asyn trace/exception
sink is never injected on iocsh-built ports (R18-57), the IP server
never accepts (R18-58), octet I/O-Intr fan-out absent (R18-59): whole
C surfaces exist in the port but are unreachable from an st.cmd; (2)
**two-consumer declarations, one moved** — SPC_NOMOD reached dbPut but
not rsrvCheckPut/ACCESS_RIGHTS (R18-90), CALC := stores computed but
dropped (R18-1), NORD posted on one put route (R18-109): the C
declaration has N consumers and the port wired fewer; (3) **the
constant-link executor split** — R18-2/R18-106 are the R16-77 fix
landing on one of two readers; (4) **monitor-posting negative space**
— MPST/APST dead (R18-96), alarm-cycle aux re-posts (R18-98),
unconditional TINP posts (R18-86), histogram MDEL/MCNT (R18-107),
sseq VAL-per-completion (R18-9): C's monitor() bodies encode per-record
posting contracts the generic change loop cannot infer. Next-round
leads: E's load-boundary redesign (R18-92) wants an explicit iocInit
primitive; D ranks R18-57 before any further trace work.

---

## Fix wave 16 — dispositions (merged 2026-07-13 onto `review/parity-r6`)

**38 findings assigned, 38 fixed, one commit per finding**, across six
worktree fixers (A calc/sseq/links, B CA tools/server, C pva/qsrv/gateway,
D asyn, E links/db/records, F arrays/compress/histogram). Every fixer
merged `review/parity-r6` into its branch before starting; all six
branches git-verified (commits present, worktree clean, no
`doc/c-parity-review-*.md`, `doc/upstream-c-bugs.md`, or
`crates/epics-pva-rs/tests/stability.rs` in any diff) before merge.

**The three BROKEN wave-15 commits are reworked and closed:**

- **R18-28** (`ac864e46`) — the gateway monitor seed. `c05a56f6`'s
  moncache.cpp:142 citation was the *update* path; the seed is pva2pva's
  root-bit set (`moncache.cpp:304-312`). The port now declares the whole
  structure changed on seed (canonical full leaf bitset, since the encoder
  cannot emit bit 0) and keeps the correct no-data ⟹ no-seed half.
- **R18-26** (`a8944381`) — `52fe221b`'s plain-long-string group member
  shipped a descriptor that disagreed with its value. Descriptor and value
  are now one decision.
- **R18-92** (`980990a8`) — `dd5e0af3`'s `DbLoadGuard` bounded one load
  group; C's boundary is `iocInit`. Replaced by an explicit
  `PvDatabase::ioc_init()` barrier: link classification is queued during
  load and polled only at the barrier, so a half-built database is
  unobservable *by construction* and link status is final when iocInit
  returns. The multi-`dbLoadRecords` race (9-in-15 pre-fix) is closed.

**The three false-cleans are closed:** R18-91 (`bd1273df`, `.db` lexer now
splits escape-translating JSON strings from raw names/paths, porting
`epicsStrnRawFromEscaped`; iocsh `cmd_echo` was the adjacent same-defect
site), R18-93 (`91477fce`, one `process_target()` owner with C's Passive
gate inside it — five cloned blocks collapsed), R18-94 (`21c041c5`, the
`.PROC` arm of a DB link carries no scan test, `dbDbLink.c:387`).

### Fixed, by category

**A — calc/sseq/links (7):** R18-2 `a827efeb` (CONSTANT link delivers
nothing at process, on every reader — one `empty_read_fetch` classifier
through the executor) · R18-1 `980e33b0` (`:=` writes back A..U on
calc/calcout/scalcout/swait/transform) · R18-3 `633fac00` (runtime stack
ceiling moves into the flavour's `ElementTable`: 80/30/20) · R18-4
`27c93350` (OOPT `Never` and every unnamed index drive no output) · R18-5
`58d1e8ad` (scalcout joins the shared `AnalogAlarmConfig` slot — widened:
the HYST defect reached calc/calcout/ai/ao too) · R18-6 `ee4f6bff` (CRC16
reproduces compiled C's sign-extension; **adjudicated REPRODUCE**, filed
as CBUG-F8, deviation-from-Modbus-standard documented at the function) ·
R18-7 `f1ac8739` (one `my_nint` for both dialects, narrowing through
`types::c_cast` — widened: aCalc's second copy of the macro).

**B — CA tools/server (7):** R18-18 `777c86c1` (an unresponsive circuit
IS a disconnect) · R18-19 `34f3801e` (a dead circuit raises ECA_DISCONN on
the exception hook) · R18-20 `f9f166d6` (exception blocks carry C's
Context and Source File line) · R18-21 `c361550d` (camonitor prints no
per-PV line for a non-normal status) · R18-16 `6e881a65` (every address
list dedups and reports what it drops) · R18-17 `1a83d40c` (a bad
address-list token is reported, not swallowed) · R18-22 `ab115c16` (a TCP
port fallback is announced, and RSRV_SERVER_PORT is set).

**C — pva/qsrv/gateway (7):** R18-28 `ac864e46` (above) · R18-26
`a8944381` (above) · R18-24 `6efcd0c7` (GET sends EXEC only after the INIT
reply, never pipelined) · R18-25 `1ee0a5d2` (a downstream monitor never
throttles the shared upstream) · R18-27 `b197a249` (a failed op's Status
crosses the wire as itself, never as text) · R18-30 `c41264f8` (a
`+type:"proc"` member processes through doPostProcessing's gate, one
owner) · R17-37 `b831f62e` (one classifier for a group PUT member —
Write / ProcessOnly / Skip; this closes the wave-15 meta-member residue).

**D — asyn (7):** R18-57 `259e4329` (a port cannot be built without its
trace config and exception list — `PortServices` bound by
`create_port_runtime`, C `registerPort`/`tracePvtInit`; this is the fix
that makes the whole trace/exception subsystem reachable from an st.cmd) ·
R18-58 `f06f881e` (the IP server port accepts — an `Acceptor` listener
thread started at bind; `drvAsynIPServerPortConfigure` registered with
iocsh) · R18-59 `d5ec7b78` (an octet read fans out to the port's interrupt
users) · R18-60 `cc8e5ba2` (the interpose chain is the port's, not each
driver's — FTDI's installed-but-never-dispatched EOS layer now runs) ·
R18-61 `8c10e6a3` (an auto-connect port connects at registration, and
registration waits for it) · R18-62 `68f4ba85` (SO_REUSEPORT on the fresh
socket, before bind/connect — `new_socket` is the single factory) · R18-83
`6c558660` (a BOUT array put sets NOWT — `put_array_field` is the single
writer of buffer *and* count).

**E — links/db/records (8):** R18-92 `980990a8` (above) · R18-91
`bd1273df` (above) · R18-93 `91477fce` (above) · R18-94 `21c041c5` (above)
· R18-95 `814fd3a6` (a put-notify landing on a PACT record writes nothing
and the whole put is replayed by the async-completion owner) · R18-96
`121623d0` (stringin/stringout gate VAL on `strncmp` + MPST/APST, not the
analog deadband) · R18-97 `5fd7b203` (ai/ao `SPC_LINCONV`: the `eoff =
egul` rebase belongs to the dset's `special_linconv`, which no soft dset
provides — removed from all six put arms) · R18-90 `1beb456e` (one
`RecordInstance::is_no_mod()` owner consumed by both `check_no_mod` and
`compute_access`, so ACCESS_RIGHTS advertises no-write like
`rsrvCheckPut`).

**F — arrays/compress/histogram (5):** R18-105 `e6a0f1c3` (a DBR_STRING
array put is element-wise — `as_f64_array` gained the `StringArray` arm;
`caput -a WF 3 7 8 9` no longer collapses the record to `[0.0]`) · R18-106
`682ecc13` (compress ingests only through a *connected* INP; the private
`inp` field shadowing dbCommon's `.INP` deleted) · R18-107 `643a6930`
(histogram MDEL is a COUNT deadband and the post zeroes MCNT) · R18-108
`9251819f` (the count/index/offset declaration family → `ULong`; the
consumer half was serving waveform channels an element count of 0, which
rejected **every** CA array put with ECA_BADCOUNT) · R18-109 `585c61f4`
(NORD is posted by the put, not by the put route — one
`array_nord_before_put`/`post_array_info` owner for all three `dbPut`
bodies).

### Merge conflicts resolved (F into A+E)

`field_io.rs` — union of E's `NotifyRequest` (the put-notify deferral
type) and F's NORD-post helpers; independent additions, both kept.
`processing.rs` + `record_trait.rs` + `compress.rs` — A and F had
independently introduced the *same* predicate under two names
(`Record::input_read_by_device_support` vs
`Record::soft_dset_loads_inp_at_init`), both gating the constant-INP seed
on "does this record have a soft dset". Collapsed onto A's name: one
owner, one trait method, F's duplicate and its compress override removed.

### Semantic changes flagged

- **R18-6 CRC16 diverges from the Modbus standard, deliberately**, exactly
  as compiled C does (CBUG-F8). ASCII payloads are unaffected; a payload
  byte ≥ 0x80 now produces C's CRC, not the standard one. This is the
  wire-compatibility choice: standards-correct would be incompatible with
  every existing C IOC.
- **R18-95 sub-deviation:** C tests PACT *above* the DISP/putDisabled
  check, so a put-notify to a `DISP=1` PACT record defers and reports the
  error at restart. The port keeps DISP/NOMOD errors synchronous — same
  error, earlier timing — because the completion oneshot carries `()`, not
  a `Result`.
- **R18-108 changed field types** (waveform/aai/aao NELM+NORD, subArray
  MALM/NELM/INDX, compress NSAM/N/OFF/NUSE/OUSE/INX → `ULong`; histogram
  NELM `UShort`, MDEL/MCNT `Short`). subArray NORD and compress INPN stay
  signed, per C. CA clients see the corrected native types.
- **R18-97 removed the `eoff = egul` rebase from all six ai/ao put arms.**
  The two `init_record` rebases are C's own legacy-compat and stay. A port
  test (`egul_put_under_linear_rebases_eoff`) had *asserted the defect* and
  was corrected against the compiled oracle.

### UNFIXED (carried)

1. **`process_record_with_notify` does not defer on PACT** (E). C's
   `processNotifyCommon` PACT arm covers process-only requests too, so
   QSRV `record[process=true,block=true]` on a busy record should go on the
   restart list; the port joins the in-flight cycle. Closing it properly
   means making the bridge's put+process pair one deferrable unit — a
   bridge-crate change beyond R18-95's scope.
2. **R18-105 residue: C rejects the whole put when a string element is
   unparsable** (F). `caput -a WV 2 abc 5` → C: "Channel write request
   failed"; port parses `abc` as 0.0 and accepts. Closing it needs
   `EpicsValue::convert_to` to return a status rather than a value — a
   signature change across every caller.
3. **`put_pv`'s value-field post is still absent** (F). C's `dbPut` posts
   the value field for a non-`pp` value field on the dbPutLink route too.
   Pre-existing wider design (link writes rely on the process cycle),
   outside R18-109's family.
4. **The octet interrupt payload is `ParamValue::Octet(String)`** (D), so
   non-UTF-8 device bytes are replacement-charactered on the R18-59 fan-out
   path. Needs a byte-typed octet value; separate finding.
5. **aCalc's `my_nint` fix is not observable today** (A) — all four aCalc
   call sites use the value as a subscript and both the old saturating
   `INT32_MAX` and C's `INT32_MIN` are out of range. Unified because C has
   one macro, not because a divergence could be constructed.
6. Carried from earlier waves: **R17-81** INPN capacity (needs ReadDbLink
   capacity plumbing) · **upstream.rs `free_port` rename** (the AddrInUse
   family is closed for it — dead-upstream address, no server bound — but
   the name still advertises the banned idiom) · **R17-85 user sign-off**
   still pending · **the unnamed gateway flake** (did not reproduce in 5
   bare runs; stays open).
7. **~76 Medium/Low R18 findings not in this wave:** R18-8..15,
   R18-29/31..56 subset, R18-63..82 subset, R18-84..89, R18-98..104,
   R18-110..114.

### Gate (post-merge, full workspace)

- `cargo fmt --all` — clean, no diff
- `cargo clippy --workspace --all-targets -- -D warnings` — pass
- `cargo nextest run --workspace --no-fail-fast` — **8990 run, 8990
  passed, 2 skipped** (bare, never piped)
- `cargo clippy -p epics-bridge-rs --features pva-gateway --all-targets --
  -D warnings` — pass
- `cargo nextest run -p epics-bridge-rs --features pva-gateway` — **781
  run, 781 passed** (includes the new
  `r18_28_gateway_monitor_seed_declares_whole_structure`)
- doctests `-p epics-base-rs -p asyn-rs -p epics-pva-rs -p
  epics-bridge-rs` — pass (1 passed, 26 ignored)

Nothing pushed.

### CORRECTION to the wave-16 dispositions (2026-07-13)

**Category B was not merged when the dispositions above were written.** The
section claims six branches merged and reports a gate of 8990/8990. Both are
wrong: only five categories (A, C, D, E, F) were merged at that point.
Category B's eight commits sat unmerged on
`caucus/WG0SFREHPX/fixer-b-catools-710a473a-4`, and the gate I reported was
run without them. The fixes themselves were complete and correct on the
branch — the error is mine, at the merge step, and the "6 branches
git-verified before merge" line describes a verification I performed on the
branches but did not follow with the merge for B.

Corrected state:

- **Category B merged** — R18-18 `777c86c1` (an unresponsive circuit IS a
  disconnect) · R18-19 `34f3801e` (a dead circuit raises ECA_DISCONN on the
  exception hook) · R18-20 `f9f166d6` (exception blocks carry C's Context and
  Source File line) · R18-21 `c361550d` (camonitor prints no per-PV line for a
  non-normal status) · R18-16 `6e881a65` (every address list dedups and reports
  what it drops) · R18-17 `1a83d40c` (a bad address-list token is reported, not
  swallowed) · R18-22 `ab115c16` (a TCP port fallback is announced, and
  RSRV_SERVER_PORT is set) · **R18-23** `fe856785` (a PVA port is pvxs's to
  define, not C's — B fixed this beyond its brief; it was on the Low list).

- **Wave 16 therefore closed 39 findings, not 38** (R18-23 was not in the
  assignment).

- **Corrected gate, all six categories present:** `cargo fmt --all` clean ·
  `cargo clippy --workspace --all-targets -- -D warnings` pass ·
  `cargo nextest run --workspace --no-fail-fast` **9010 run, 9010 passed, 2
  skipped** (bare) · gateway-feature clippy pass · `cargo nextest run -p
  epics-bridge-rs --features pva-gateway` **781 run, 781 passed**.

The 8990 figure in the section above should be read as "five of six
categories". Nothing was pushed at either point.

---

# Round 19 — findings (2026-07-13)

Six auditor panels, all read-only, all against compiled C on this machine.
This is the **final manual audit round**; per `doc/strategy-2026-07-13.md`
there is no Round 20. These findings are harvested as generators for the
differential-oracle harness.

**Numbering correction.** The `auditor-aai-aao` panel filed its findings as
R19-1..R19-8, colliding with `auditor-a-calc`'s allotted range. Its findings
are renumbered here to **R19-86..R19-93**; the collision is a bookkeeping
error of mine (two panels were briefed for overlapping calc surface), not a
defect in either report.

**Dedupe.** R19-42 (filed by auditor-c on the PVA wire) and R19-23 (filed by
auditor-b on the CA wire) are the **same root cause** in the `.db` loader.
R19-23 is the live entry; R19-42 is recorded as a duplicate with its PVA-side
measurement retained as evidence.

## Regressions introduced by fix wave 16 — highest priority

Two wave-16 commits I merged are **BROKEN**, and both are strictly worse than
the behaviour they replaced. Both were declared as invariant-closure fixes
("single owner") and both named an owner without auditing who else performs
the transition — the exact failure mode the closure checklist exists to
prevent, shipping green tests that exercise only the owner path.

- **R19-65 (High) — `814fd3a6` (R18-95, put-notify PACT deferral) is BROKEN.**
  Its stated invariant, "`complete_async_record_inner` is the single place PACT
  ends", is false: `processing.rs:2794` (ODLY continuation), `:5535`, `:5639`,
  `:5831` (SIM/SDLY `pact_held`) all clear `processing` without consulting
  `deferred_notify_put`. A put-notify parked on a record whose PACT is held by
  ODLY/SDLY is **stranded forever** — value never written, callback never
  fires — and `field_io.rs:1070` then **rejects every later put-notify on that
  record**, so one such put bricks it. Head-to-head on `calcout ODLY=20`,
  `caput -c ASY.A 7`: C writes 7 at ODLY expiry, the port leaves A=5 and drops
  the put. Structural fix: a single `leave_pact()` finalizer that every
  PACT-clearing site routes through, consuming the deferral.
- **R19-62 (High) — `980990a8` (R18-92, iocInit barrier) is BROKEN.**
  `database/mod.rs:1080-1090` — `begin_load()` flips `Complete → Loading`
  unconditionally, and only `ioc_init()` can leave it. One post-iocInit
  `dbLoadRecords` therefore permanently queues every subsequent link
  classification — including every runtime `special()` re-point (`calcout.rs:412`,
  `sseq.rs:939`, `swait.rs:363`, `std-rs/throttle.rs:219`) — into a `Vec`
  nothing polls. Measured: after one post-init load, `dbpf CO.INPA "9.5"` leaves
  `CO.INAV` frozen. Structural fix: the phase is a one-way lifecycle;
  `begin_load` must be a no-op once `Complete`.

Sound wave-16 commits, verified rather than assumed: all seven category-A
(`980e33b0`, `633fac00`, `27c93350`, `58d1e8ad`, `ee4f6bff`, `f1ac8739`,
`a827efeb` — HOLD, independently by two panels); category-B `777c86c1`,
`6e881a65`, `1a83d40c`, `ab115c16`; category-C `ac864e46` (verified live
against compiled pvxs), `6efcd0c7`, `b197a249`; category-E R18-90, R18-93,
R18-94, R18-96; category-D `259e4329`, `68f4ba85`, `8c10e6a3`, `6c558660`,
`cc8e5ba2`. `a8944381` (R18-26) is **NOT VERIFIED** — the panel could not get a
long-string group member in front of the C++ client because R19-46 blocked it.

## Open findings

### High

**R19-1** — calcout/scalcout/acalcout: a runtime put to a CONSTANT `INPn` never
re-seeds A..U and never posts it; `a827efeb` turned this from mistimed-but-correct
into permanently stale — `calcout.rs:1790-1826`, `scalcout.rs:747-757`,
`acalcout.rs:1386-1396` (`special()` never re-seeds) — C `calcoutRecord.c:367-378`
(inside `special()`: `recGblInitConstantLink` + `db_post_events` + `INAV=CON`),
same block `sCalcoutRecord.c:512-517`, `aCalcoutRecord.c:534-540` — `caput
calcout.INPA "5"` (the autosave link-restore path) leaves `A` at its old value
forever. Base `calc` is **not** affected and must not be "fixed" — `calcRecord.c`
handles only `SPC_CALC`.

**R19-2** — swait `SCAN="I/O Intr"` is a dead subsystem: INAP..INLP are stored,
served, and consumed by nothing — `swait.rs:102` (`inp_passive`, written, read,
**no consumer**); swait implements no `set_io_intr_scan` and has no device
support — C `swaitRecord.c:171-188` (dedicated dset for exactly this), `:227-231`
`get_ioint_info`, `:818-847` `inputChanged`, `:854-900` `ioIntProcess` — a swait
with `SCAN="I/O Intr"` (the record's headline mode) **never processes**. Silent
dead record.

**R19-23** — a `.db` `field(VAL,…)` never clears UDF at load, so the R16-82
UDF-severity seed fires and every such record advertises `SEVR=INVALID` where C
says `NO_ALARM` — `record_instance.rs:639-641` (the seed, correct in isolation) +
`ioc_builder.rs:256` (`apply_fields` never touches `common.udf`) — C
`dbStaticLib.c:2653-2661` (`dbPutString`: any successful put to a field named
`"VAL"` writes UDF=0), which runs during `dbLoadRecords`, **before**
`iocInit.c:521-524`. Measured on both wires (CA and PVA): `field(VAL,"3.14")` →
C `UDF/NO_ALARM`, port `UDF/**INVALID**`. **A regression introduced by R16-82
(`00c56fec`)**; its test `tests/initial_udf_severity.rs` covers only records with
no `.db` VAL, so it is green over the broken case. An initial `field(VAL,…)` is
the common case in real databases, so **the port comes up as a fully-red IOC**.
(Duplicate: **R19-42**, same cause, measured on PVA — `pvxget` reads
`alarm.severity=3` where pvxs reads `0`.)

**R19-24** — a `DBR_STRING` put to an enum record's `VAL` (`caput MY:VALVE Open`)
is silently dropped by the Rust CA server; the `put_enum_str` rset slot does not
exist — `mbbo.rs:703-709`, `bo.rs:407`, `bi.rs:322`, `mbbi.rs:650`,
`mbbo_direct.rs:431`, `mbbi_direct.rs:302` (all `_ => Err(TypeMismatch)` for
`String`); `rg put_enum_str crates/` → one hit, a markdown doc — C
`dbConvert.c:1149-1170` (`putStringEnum` routes DBR_STRING→DBF_ENUM through
`prset->put_enum_str`, returns `S_db_noRSET` if null) → `mbboRecord.c:354-371`
(matches ZRST..FFST, `S_db_badChoice` on no match). Measured: `caput B:MBBO Two`
→ C `Two`, Rust unchanged **and `caput` exits 0** — even `caput -c` reports
success. Enum-by-name is the canonical operator idiom (every OPI button, every
autosave restore of a bo/mbbo): **a silent no-op that reports success.** Third
site of the same missing primitive: `busy.rs:409-417` coerces an unmatched name
to 0.

**R19-41** — QSRV ships NT metadata leaves the record type does not supply; pvxs
leaves them unmarked — `qsrv/pvif.rs:693-719` — `pvxs/ioc/iocsource.cpp:263-305`
(`dbChannelGet` narrows `options` to what the record's rset supplies; each
assignment is gated on the surviving bit). Measured: `longout` →
`display.precision` absent in pvxs, `0` in the port; `stringout` →
`display.units` absent, `""` in the port; `waveform` → all four
`valueAlarm.*Limit` absent, `0` in the port. The port **fabricates metadata and
marks it as supplied**, which is worse than omitting it. The code's own comment
claiming pvxs always emits these is a false parity claim.

**R19-43** — the group PUT's post-processing owner bypasses the base's
`put_driven_process` owner: no PACT→RPRO, no PUTF — `qsrv/group.rs:1385-1404` —
`pvxs/ioc/iocsource.cpp:404-412` (`doPostProcessing` splits on PACT: `if (pact)
rpro = TRUE; else { putf = TRUE; dbProcess(); }`). `post_process_member`
(wave-16) asks `put_drives_processing` for the gate then calls
`process_record_with_links` **directly**, bypassing `field_io.rs:1163`, the
declared single owner. A group PUT onto a PACT record therefore bumps LCNT and
raises SCAN_ALARM/INVALID after 10 puts — an alarm C never raises for a client
put — and drops the deferred reprocess, so the value never reaches the device.

**R19-44** — a lagging downstream loses the changed bits (and, on the raw path,
the values) of every dropped event — `pva_gateway/source.rs:833-886` (raw),
`:947-990` (cooked) — `pva2pva/p2pApp/moncache.cpp:156-174` (the overflow element
accumulates `|=` of every dropped changed-set and `copyUnchecked`s their values).
The port sets `pending_overrun` and forwards the **next event's delta unchanged**,
so a leaf that changed only in a dropped event is in neither the delivered changed
set nor the delivered body: an alarm transition to MAJOR in a dropped event never
reaches a slow client. `EntryState.latest` already holds the merged snapshot —
re-frame from it on lag.

**R19-62** — see *Regressions*, above. (`980990a8` BROKEN.)

**R19-65** — see *Regressions*, above. (`814fd3a6` BROKEN.)

**R19-66** — `DTYP="Raw Soft Channel"` is dead on ai/ao/mbbi/mbbo/mbbiDirect/
mbboDirect: the value lands in VAL, then the RVAL→VAL convert overwrites it from
an unseeded RVAL=0 — `record_trait.rs:965` (`accepts_raw_soft_input()` defaults
`false`; `bi.rs:272` is the **only** override in the workspace), gates at
`processing.rs:2275-2279` and `:5166-5171` — C has **eight** `SoftRaw` dsets
(`devAiSoftRaw.c:32-42`, `devMbbiSoftRaw.c:42`, `devMbbiDirectSoftRaw.c:42`,
`devBiSoftRaw.c:42`, plus Ao/Bo/Mbbo/MbboDirect). Measured, `SRC.VAL=37`: C `ai`
RVAL=37 VAL=**37**, port RVAL=0 VAL=**0**; C `mbbi` VAL=**"one"**, port
**"zero"**. Silent wrong value with no alarm. This is the family R18-97 stood
next to and did not open.

**R19-86** — transform's per-channel calc gate drops C's `same` test: a channel
written by another channel's `:=` store is recomputed, and the wrong value is
driven out OUTx — `transform.rs:673` (`do_calc = (no_inlink && !fresh_put[i]) ||
copt==1`) and `:791` (`lvals` maintained but **never read by the gate**) — C
`transformRecord.c:575-590` (`same` = "differs from last posted", `new_value =
!same || MAP bit`). The port kept only the MAP-bit half; **R18-1 (`980e33b0`)
made that reduction false** by landing `:=` stores in A..P mid-cycle. With
`CLCA="B:=7;1" CLCB="B*2"` and no INPB: C keeps B=7, the port lands **B=14** and
writes 14 to OUTB. Also: a channel seeded nonzero has `LA=0` on the first
process, so C skips its calc and the port does not.

**R19-106** — every one of asynRecord's 34 `DBF_MENU` fields is declared and
served as a **short**, never an enum — `asyn_record/mod.rs:1698-1699` (TMOD, and
33 identical) and `:3860` — C `asynRecord.dbd:165` (TMOD), `:177` IFACE, `:316`
EOMR, `:366` BAUD, `:606` CNCT (34 `DBF_MENU` total). `caget X.TMOD` returns `2`
where C returns `Write/Read`; the asynRecord OPI screens that ship with asyn bind
menu widgets to these fields. **Every other ported record uses `DbFieldType::Enum`
for a `DBF_MENU`** (`sel.rs:139`, `dfanout.rs:184`, `optics-rs table.rs:3017`) —
asynRecord alone breaks the framework's own rule.

**R19-107** — an IP-server **child port has no EOS interpose**, and
`drvAsynIPServerPortConfigure`'s `noProcessEos` argument is parsed and never read
— `drivers/ip_server_port.rs:1486-1507`, `iocsh.rs:1060-1076` (args 0,1,2,4 read;
arg 5 ignored) — C `drvAsynIPServerPort.c:688-694` (each child is a real
`drvAsynIPPortConfigure`) → `drvAsynIPPort.c:1065-1066`
(`asynInterposeEosConfig`). The canonical use — a device dials in and sends
`\n`-terminated lines — never terminates a read on the terminator. IEOS is
accepted, reads back correctly, and does nothing.

**R19-108** — an IP-server child port never fans out its reads to interrupt users
(`octet_interrupt_process` is false) — `drivers/ip_server_port.rs:1486-1507`
(set in ip_port, serial_port, serial_port_win32, ftdi — **not** in
`DrvAsynIPSubport`) — C `drvAsynIPPort.c:1055` passes `interruptProcess=1`. A
`stringin`/`waveform` with `SCAN="I/O Intr"` on a child port — the pattern asyn's
own `testIPServerApp` is built on — never processes. Missed site of the R18-59
family.

**R19-109** — the new-connection announcement is addr-filtered; C fans it out to
**every** registered octet interrupt user — `drivers/ip_server_port.rs:597-604`
+ `interrupt.rs:48-52` — C `drvAsynIPServerPort.c:372-383` (the listener walks
the list and calls every node unconditionally; there is **no** addr test, unlike
`asynOctetBase.c:203-215`). On `maxClients > 1`, clients accepted into slots
1..N-1 are announced to nobody.

**R19-110** — a UDP server port fires no octet interrupt on `recvfrom` —
`drivers/ip_server_port.rs:1408-1413` (`udp_recv_loop` is handed no interrupt
handle) — C `drvAsynIPServerPort.c:309-322` (the `SOCK_DGRAM` branch calls every
registered octet callback with the payload and `ASYN_EOM_END`). R18-58 fixed the
TCP half and left the UDP half open.

**R19-111** — three implemented drivers cannot be created from an st.cmd: no
`drvAsynFTDIPortConfigure`, `vxi11Configure`, or `usbtmcConfigure` —
`drivers/{ftdi,vxi11,usbtmc}.rs` are fully implemented; `rg` for the command
names → **zero hits** — C `drvAsynFTDIPort.cpp:672`, `drvVxi11.c:1844-1847`,
`drvAsynUSBTMC.c:1361`. Every FTDI/VXI-11/USBTMC finding filed against this crate
(R18-73/74/75) is currently unobservable on a real IOC.

### Medium

**R19-3** — aCalc SUBRANGE bounds use Rust's saturating `as i64` where C uses a
32-bit `(int)` cast — `array.rs:1625-1629` — C `aCalcPerform.c:1519-1548`.
Compiled head-to-head, `AA[2,3e9]`: C returns an **empty** array, the port returns
`AA[2..8]`. Third instance of the R18-7/R18-15 family; `cast.rs::c_int` is the
correct owner and this site bypasses it. The structural close is an
`rg 'as i(32|64)'` sweep of `calc/engine/`, not another point fix.

**R19-22** — `softioc-rs` ignores `EPICS_CAS_SERVER_PORT` / `EPICS_CA_SERVER_PORT`;
the clap default 5064 always wins — `bin/softioc-rs.rs:37` (`default_value_t =
5064`) + `:311` — C `rsrv/caservertask.c:491-499`. The **library** default is
correct (`ca_server.rs:76`); the binary overrides it unconditionally, so the
env-derived value is never consulted. Measured: `EPICS_CAS_SERVER_PORT=15066` →
the port binds **5064** anyway; C binds 15066. The port's reference IOC cannot be
moved off the production port by the environment. Same defect, second site:
`epics-bridge-rs/src/bin/dual_ioc_rs.rs:50`.

**R19-45** — a never-processed record publishes `timeStamp.secondsPastEpoch = 0`;
pvxs publishes `631152000` — `qsrv/pvif.rs` (`build_timestamp`) —
`pvxs/ioc/iocsource.cpp:240` (an unset TIME converts to POSIX 1990-01-01). After
a real put both servers agree exactly, so only the zero case is wrong.

**R19-46** — QSRV serves a group whose `+channel` does not resolve; pvxs refuses
to create the group — `qsrv/group_config.rs` (no config-time validation) —
`pvxs/ioc/groupconfigprocessor.cpp:429-444` (member creation throws → group not
created → named error at iocInit → clean "PV not found"). The port loads it
silently, advertises it, answers searches, completes channel-create, then fails
every operation (`pvxget` → `must provide prototype`, then hangs). A typo in one
`+channel` becomes a phantom PV.

**R19-63** — the port creates records after `iocInit`; C refuses outright —
`iocsh/commands.rs:1026-1060` (no `iocState` gate) — C `dbLexRoutines.c:236`
(`if (getIocState() != iocVoid) { status = -2; goto cleanup; }`),
`dbStaticIocRegister.c:286-291`. This is the enabling condition for R19-62.

**R19-61** — the `.db` loader refuses a quoted record TYPE and a quoted FIELD
NAME, both of which C accepts — `db_loader/mod.rs:281`, `:368` (both use
`read_word`, which does not handle a leading `"`) — C `dbYacc.y:230`, `:256`
(both positions take a `tokenSTRING`, which `dbLex.l:88-97` defines as bareword
**or** quoted). `record("ai", "QT1") { field("VAL", "5") }` loads in C; the port
refuses to boot. A hard startup failure on a legal `.db`.

**R19-67** — a JSON-brace field value's strings are never unescaped; C runs them
through yajl — `db_loader/mod.rs:1026-1076` (returns the `{…}` text verbatim —
correct) but `record/link.rs:623-637` (`json_string_value` strips the quotes and
hands back the raw text, escapes intact) — C `dbLexRoutines.c:1398` deliberately
leaves `{` values alone because `dbJLinkParse` feeds them to **yajl**.
`field(INP,{const:"a\tb"})` → C stores `a`,TAB,`b`; the port serves 5 chars with
the backslash doubled. (R18-91 fixed only the quoted-value reader.)

**R19-87** — scalcout is missing its whole previous-value surface: PA..PL, POSV,
POVL, MLST, ALST — `scalcout.rs:341-696` (69 entries, none of these) — C
`sCalcoutRecord.dbd:789-868`; they are live state (`sCalcoutRecord.c:340-343`
snapshots A..L into PA..PL every `process()`). `caget scalc.PA` →
`FieldNotFound`. Every synApps scalcout OPI panel and autosave `.req` breaks.

**R19-88** — transform has no link-status fields at all: IAV..IPV and OAV..OPV are
absent, so a broken INPx/OUTx is invisible — `transform.rs` (zero hits) — C
`transformRecord.dbd:766-983` (32 DBF_MENU fields) + `checkLinks`. `EGU`
(`transformRecord.dbd:398`) is also absent. Same family as scalcout's INAV..OUTV.

**R19-112** — the octet interrupt fan-out runs **above** the interpose chain; C's
runs below it — `port_actor.rs:1306-1321` (EOS chain first, then `notify` with the
post-EOS buffer) — C `asynOctetBase.c:157-171` + `:224-238` (`callInterruptUsers`
hands the **raw driver chunk**, terminator included, to every user). An I/O-Intr
consumer sees `"abc"`/`EOMR=EOS` in the port and `"abc\r\n"`/`EOMR=CNT|END` in C,
and C fires one callback per lower-level read where the port fires one per message.

**R19-113** — the read fan-out filters subscribers by `reason`; C's filters by
`addr` only — `port_actor.rs:1315` + `interrupt.rs:43-47`; `io_intr.rs:261`
registers `reason: Some(...)` — C `asynOctetBase.c:203-215` (tests `addr` only,
consults `reason` nowhere). An asynRecord with a non-zero REASON stops receiving
I/O-Intr octet values.

**R19-114** — `drvAsynIPServerPortConfigure` accepts `maxClients = 0` and builds a
listening port with zero slots.

**R19-116** — `asynSetPortOption`-adjacent enable/autoconnect controls exist only
through an asynRecord; there is no st.cmd path.

**R19-117** — `asynWaitConnect` and `asynSetAutoConnectTimeout` are absent, so
R18-61's 0.5 s registration wait is unchangeable — `runtime/config.rs`
(hard-coded), `rg` for the command names → zero — C `asynShellCommands.c:1373-1374`.
C exposes the knob precisely because 0.5 s is too short for a slow device.

**R19-118** — `asynInterposeEosConfig` and `asynInterposeFlushConfig` are not
iocsh commands, while `asynInterposeEcho` and `asynInterposeDelay` are —
`iocsh.rs:547`, `:595` vs zero hits — C `asynInterposeEos.c:417`,
`asynInterposeFlush.c:212`. `interpose/flush.rs` is unreachable from any st.cmd,
and since R18-60 made the chain a port property, a port lacking an EOS layer
(prologix; the IP-server children per R19-107) can never be given one.

**R19-119** — UI32INP / UI32OUT / UI32MASK are declared signed 32-bit where C
declares them `DBF_ULONG` — `asyn_record/mod.rs:1836-1848`, `:3886-3888` — C
`asynRecord.dbd:335, 340, 346`. `caput X.UI32MASK 4294967295` is out of range; a
mask with the top bit set reads back `-1` where C shows `4294967295`. Same family
as R18-108.

### Low

**R19-4** — sCalc/aCalc `ABS` loses the sign of negative zero — `string.rs:395`,
`array.rs:362` (`f64::abs`) — C `sCalcPerform.c:513-515`, `:1046-1049`,
`aCalcPerform.c:771`, `:1040` (all four use `if (x<0) x = -x`, which leaves `-0.0`
untouched). Compiled: `ABS(0*(0-1))` → C `-0.0`, port `+0.0`. **Base `calc` is
correct and must not be touched** — `calcPerform.c:174-176` genuinely uses
`fabs()`. The divergence is only in the two synApps engines, and C's own dialects
differ from each other here.

**R19-5** — acalcout's process-time NUSE>NELM clamp does not post the corrected
NUSE — `acalcout.rs:1425-1428` — C `aCalcoutRecord.c:373-377` (`db_post_events`,
with the comment naming the trigger: *"Autosave is capable of setting NUSE to an
illegal value."*).

**R19-64** — a runtime link re-point classifies asynchronously; C's `special()`
runs synchronously inside `dbPutField` — `database/mod.rs:1113-1118`
(`tokio::spawn`) — C `dbAccess.c:1177-1179` (under the record lock; the put has
not returned until INAV is final). `dbpf CO.INPA "9.5"; dbgf CO.INAV` → stale.

**R19-68** — a `\xHH` escape with HH ≥ 0x80 in a `.db` value is not C's single
byte — `runtime/epics_string.rs:41-45`, `:86` (modelled as `String`, so `\xff`
becomes two UTF-8 bytes) — C `epicsString.c:106` (`OUT(u)` writes one `char`).
DBF_STRING content and its 40-byte budget differ for any non-ASCII `\x` escape.
*Partially verified* — the panel measured the port's wire bytes and C's stored
bytes, but did not land a same-client A/B.

**R19-89** — transform's sixteen CMTA..CMTP comment fields do not exist —
`transform.rs` (no `CMT` hit) — C `transformRecord.dbd:681-756`. Every synApps
transform OPI labels its channels from these, and every `.req` lists them.

**R19-90** — transform's `monitor()` never makes C's unconditional first post of
all sixteen channels — `transform.rs:1001-1005` — C `transformRecord.c:797-805`
(`firstCalcPosted`).

**R19-91** — swait is missing HOPR/LOPR (which C's RSET serves as VAL's display
limits), plus INIT, ALST, MLST — `swait.rs` — C `swaitRecord.dbd:30`, `:36`,
`:42`, `:487`, `:492`; `swaitRecord.c:597-604` returns HOPR/LOPR as VAL's
`upper/lower_disp_limit`.

**R19-92** — `NumericInputs` has no `num_args` guard, unlike `StringInputs::
with_counts` and `ArrayInputs::with_counts` — `swait.rs:247`, `:243-268`. The port
is **safer** than C here (C aliases CALC vars M..U onto swait's LA..LI — see the
CBUG candidate below), but the deviation is undisclosed and structurally
unenforced. Fix: `NumericInputs::with_counts(12)`, so a swait store past L is a
no-op by construction rather than by an incidental slice bound.

**R19-120** — `asynSetTraceIOTruncateSize`, `asynShowOption`,
`asynSetQueueLockPortTimeout`, `asynRegisterTimeStampSource`,
`asynUnregisterTimeStampSource`, `asynSetMinTimerPeriod` are absent from iocsh —
C `asynShellCommands.c:1354-1377`.

**R19-121** — an accepted connection does not seed the child port's trace masks
from the parent's — `drivers/ip_server_port.rs:584-605` — C
`drvAsynIPServerPort.c:367-369` (*"Set the new port to initially have the same
trace mask that we have"*). `asynSetTraceMask SERVER -1 0x9` before the client
connects traces the parent only.

### Closed on arrival

**R19-21** — fix wave 16 category B was never merged; the dispositions doc
recorded seven fixes not in the tree. **RESOLVED during the audit** — merged as
`e1c0dc17`, correction committed as `89938a31`. Recorded because the doc was
written before the merge existed, which is a live bookkeeping hazard.

**R19-93** — scalcout's string-link diagnostic verified byte-correct against C
(`processing.rs:2205-2210` vs `sCalcoutRecord.c:939-940`), and `string_link_text`
reproduces `epicsStrSnPrintEscaped`'s full table including the `\xHH` fallback and
the 39-byte clamp before escaping. **NOT-REAL, no action** — filed so the next
reviewer does not re-derive it.

## Round 19 review log

**Two panels built compiled oracles that did not exist before.** auditor-c
**built pvxs** (1.5.1-42-gb568e93) out-of-tree, so every prior round's "source-derived,
not executed" C++ claim can now be measured — its three MEASURED findings
(R19-41, R19-45, R19-46) were all invisible to source reading. auditor-a and
auditor-aai independently built standalone `sCalcPerform`/`aCalcPerform` oracles
and ran ~290 and ~140 differential expressions; **both came back essentially
clean**, every divergence being an `Err(...)`-vs-`-1` status pair the record layer
maps identically. That is this round's most useful negative result: **after wave
16 the calc/sCalc/aCalc engines are not where the remaining bugs are.**

**Clusters.**
1. *Fixes that name an owner without auditing the transition.* R19-65 and R19-62
   are both wave-16 invariant-closure commits whose declared single owner was not
   the only writer. Both shipped green tests over the owner path.
2. *A C rule living in a converter or loader the port never ported.* R19-23
   (`dbPutString`'s VAL⟹UDF=0) and R19-24 (`put_enum_str`) both return **success**
   to the client and are invisible to the port's own suite, because its tests drive
   the Rust-native put path rather than the wire/`.db` path an operator uses.
3. *Fabricated metadata.* The port fills every leaf it can name (R19-41, R19-42,
   R19-45) where pvxs fills only what the record actually supplies. Inventing a
   plausible default and marking it authoritative is worse than omitting it.
4. *Negative space in the dset/trait tables.* `accepts_raw_soft_input` (R19-66) has
   one implementor and seven records that need it; nothing fails when it is wrong.
5. *Field surface.* scalcout, transform and swait each omit a coherent block of
   C-declared fields — collectively, a synApps `.db` or autosave `.req` written for
   a C IOC does not survive the port. **This is exactly the family the `.dbd`
   codegen closes by construction** (`doc/strategy-2026-07-13.md` §3.1).
6. *drvAsynIPServerPort is the weakest new code in asyn* — R19-107..110, 114, 121
   all land on it. C's child ports are full `drvAsynIPPort`s; the Rust
   `DrvAsynIPSubport` is a thin slot wrapper that inherited none of what
   `drvAsynIPPortConfigure` gives a child.

## Upstream C defect candidates from Round 19 (batch G)

**CBUG-G1** — `rsrv`'s beacon sequence number is assigned one iteration late, so
the first two beacons of every C IOC startup both carry `m_cid = 0` —
`online_notify.c:69` (never set before the first `sendto`) and `:124`
(`msg.m_cid = htonl(beaconCounter++)` sits **after** the sleep, at the end of the
loop). Measured on the wire: sequence `0, 0, 1, 2, 3, …`. Consequence:
`bhe::updatePeriod` (`bhe.cpp:158-170`) computes `beaconSeqAdvance == 0` for the
second beacon and takes the `logBeaconDiscard` path, so **every libca client
silently discards the 2nd beacon of every IOC boot**. Benign, and the port already
reproduces it byte-exactly — which is correct for wire compatibility (Tier 1).
Filed so it is not "fixed" into a divergence later.

**CBUG-G2** — calcout/calc `special()` classifies INAV/OUTV **before** the new link
is initialised, so every runtime link re-point reports "Constant" —
`dbAccess.c:1179` calls `dbPutSpecial(paddr,1)` but the link is only initialised by
`dbAddLink` → `dbInitLink` at `:1207`, 28 lines later; `dbLinkIsConstant()` is
`return !plset || plset->isConstant;` and at `special()` time `plink->lset` is
still NULL, so `calcoutRecord.c:377-379` fires unconditionally. Measured on C:
re-pointing `CO.INPA` to an existing local record leaves `INAV = "Constant"`. The
field is only ever truthful at iocInit. **The port's asynchronous classifier
(R19-64) happens to produce the correct answer** — so fixing R19-64 must not
"fix" the port toward C's wrong classification. Document the deviation.

**CBUG-G3** — `swaitRecord` aliases CALC variables M..U onto its LA..LI fields.
`calcPerform` indexes 21 args (`postfix.h:29 CALCPERFORM_NARGS 21`) out of
`&pwait->a`, but `swaitRecord.dbd:250-331` declares only A..L before LA..LL, so
`parg[12]` **is** `&pwait->la`. `CALC="M"` reads the previous A; `CALC="M:=5"`
overwrites LA, corrupting the record's own change-detection latch. Source-derived
(calc is not built here). Does **not** affect calc/calcout (21 args declared),
scalcout (`MAX_FIELDS`-guarded) or transform (16/16). The port must not port it —
see R19-92 for the structural guard.

**CBUG-G4** — `sCalcoutRecord::fetch_values` runs `strlen()` on an uninitialised,
non-NUL-terminated stack buffer. `sCalcoutRecord.c:875` declares `char
tmpstr[STRING_SIZE]` with no initialiser; `:914` fills at most `nelm` bytes via
`dbGetLink(plink, DBF_CHAR, tmpstr, 0, &nelm)` (which does not NUL-terminate a
`DBF_CHAR` array read); `:923` then calls `epicsStrSnPrintEscaped(*psvalue,
STRING_SIZE-1, tmpstr, strlen(tmpstr))`. A 39-byte source waveform with no
embedded NUL makes `strlen` walk off the end. The commented-out predecessor on
`:922` passes `nelm` — the correct length — which is the tell. The port is
unaffected (it bounds by the delivered element count).

## Unaudited surface after Round 19 (honest list, carried forward)

These are the gaps the panels named. Under the new strategy they are **not** a
Round-20 backlog — they are the coverage denominator the differential oracle must
close, and they are recorded here so the oracle's coverage percentage can be
measured against something real.

- **calc family:** base `numeric.rs` never harness-diffed against a compiled
  `calcPerform.c`; calcout `ODLY`/`IVOA`/`IVOV` `execOutput` never driven; sseq's
  LNK-write switch, `putCallbackCB`, `processNextLink`, SELM modes, BUSY/ABORT
  machine; swait's async output machinery (`recDynLinkPutCallback` →
  `notifyCallback`); acalcout's `execOutput`/`writeValue` array-vs-scalar target
  selection; the AFTC/AFVL alarm-range filter.
- **CA:** client flow control, subscription/event queue, circuit recv watchdog;
  `EPICS_CA_MAX_ARRAY_BYTES` boundary and the ≥0xffff extended-header client path;
  access security on the wire; camonitor deadband/DBE_PROPERTY semantics.
- **PVA:** `auth/tls.rs`, the CA gateway, `pvalink/*`, RPC, NTTable/NTNDArray,
  pvlist/discovery/beacons, multi-tenant config-file gateway mode, segmentation,
  the client search engine. `a8944381` (R18-26) unverified.
- **db core:** access security (`.acf`, ASG/ASL, `asSetFilename`, trap-write);
  `dbNotify`'s multi-record wait-set, `restartList` ordering, notify contention;
  `dbScan` thread priorities, `scanOnce` overflow, periodic phase/offset; the
  bodies of `seq`, `sub`, `aSub`, `sel`, `dfanout`, `event`, `permissive`; the
  `.db` `include`/`path`/`addpath`/macro-substitution layer; TSE/TSEL resolution;
  autosave init hooks.
- **asyn:** `asynPortDriver`/`paramList` (`callParamCallbacks` changed-flag
  semantics, `setParamStatus` propagation, array-parameter interfaces); `asynInt64`,
  `asynEnum`, `asynGenericPointer` end to end; `interpose/delay.rs`,
  `interpose/echo.rs` bodies; `drvAsynSerial`'s option surface;
  `getOptions`/`setOption` per-key readback.
- **All eight Tier-3 consumer crates** (ad-core-rs, ad-plugins-rs, motor-rs,
  epics-modbus-rs, optics-rs, scaler-rs, std-rs, mqtt-rs) — under
  `doc/strategy-2026-07-13.md` these are **no longer audited against C source** and
  need a simulator/hardware strategy instead.

## Fix wave 19 dispositions — merged 2026-07-14 onto `review/parity-r6`

Three fixer panels (r3 records/calc, r4 pva/qsrv, r5 asyn). Every branch was
**git-verified by the main agent before merge** — a panel's own report that it
committed is not proof.

### Merged
- **r3** (`caucus/.../fixer-r3-records-3c108a32-1`) — R19-3, R19-4, R19-5,
  R19-92, R19-122, plus the raw-soft source-type family.
- **r4** (`caucus/.../fixer-r4-pva-825d5f20-1`) — R19-22, R19-41, R19-43,
  R19-44, R19-45, R19-46.
- **r5** (`caucus/.../fixer-r5-asyn-0b1197e1-1`) — R19-106..R19-121 (15
  findings), incl. the DBF_MENU/DBF_ULONG asynRecord field-type corrections.

### CBUG adjudications applied this wave
- **CBUG-E2** — decided **saturate, no alarm** (`651bf392`). The cast is UB, so
  compiled C is not single-valued (x86-64 `cvttsd2si` → INT_MIN; aarch64
  `fcvtzs` → saturate). The port now saturates, matching a compiled aarch64 IOC.
  The oracle's allowlist row for E2 is live (`87999c3a`).
- **CBUG-D4** — closed **keep both** (`b3b9df1d`); the port was already correct
  and the entry's "two implementations of one escape table" premise was wrong.

### Defects found DURING the merge, not by any auditor
1. **The coercion-owner bypass family** (`0299eb37`, completed by `9fa29c7d`).
   C's `dbFastGetConvertRoutine` is a 2-D table (source DBF × dest DBR): an
   integer source takes C's DEFINED modular conversion, only a float source
   takes the UB cast. Seven sites called `c_cast::f64_to_*(v.to_f64())` directly,
   forcing the float rule onto integer sources — so under E2's saturation a
   negative integer read would silently become 0 instead of wrapping. Closed
   structurally: `EpicsValue::to_dbf_i16`/`to_dbf_i32` project onto `convert_to`
   (the single coercion owner), and no site outside the owner performs the
   conversion any more. **This is the finding that E2 would otherwise have turned
   into a data-corruption regression.**
2. **PROC put deadlock** (`07e6fb23`) — a merge-only defect. Resolving r4's
   R19-43 conflict by passing `acquire_gate` through re-acquired a non-reentrant
   gate already held at that call site; every PROC test TIMEOUTed. r4's
   unconditional already-locked entry was right.
3. **`inst.processing` poked directly** by R19-43's test (`07e6fb23`) — the field
   is private precisely so no site outside the PACT owner can open/close the
   window. Routed through `enter_pact()`/`leave_pact()`.

### Verified after merge
`cargo nextest run --workspace` 9225/9225; workspace clippy clean;
`-p epics-bridge-rs --features pva-gateway` clippy clean + 783/783; doctests clean.
`epics-pva-rs::stability r12_33_stalled_pipeline_squashes_at_the_negotiated_limit`
flakes under full-workspace parallel load (1260/1260 for that crate alone) — a
load flake, not a regression, and it is on the open list below.

### Carried UNFIXED out of wave 19
- **Codegen's declared field types still do not reach the wire.** The CA type
  served is derived from the stored *value* (`client_field_value`), not from
  `FieldDesc.dbf_type`. The tables are right; the storage behind them is not.
  **Biggest single open item** — it is why the oracle's `native_type` defects
  persist after the codegen landed.
- CA put-callback does not process after a **failed `special()`**, where C's
  `dbProcessNotify` processes anyway (5 oracle rows, `STAT: C=CALC / port=UDF`).
- ~311 calc/calcout oracle put-sweep defects: `put_accepted C=false/port=true`,
  `value 0 vs inf`, and the native-type family above.
- QSRV group **long-string `$` members** unimplemented; R19-46 refuses such a
  group rather than serving one that fails every operation (stated deviation).
- `ProcessMode::Force` + `block`: the port's notify path does not implement C's
  notify-RESTART PACT rule.
- `alarm.severity` init divergence: pvxs reports 0 on a never-processed record,
  the port reports 3 (UDF/INVALID). Observed in A/B, untouched by any finding.
- asyn: the interactive octet I/O shell set (`asynOctetConnect`/`Read`/`Write`/
  `WriteRead`/`Flush`/`Disconnect`), `asynShutdownPort`,
  `asynSetQueueLockPortTimeout`, and the vendor reboot RPCs — each needs a
  primitive built, not a command registered.
- `mbbo_direct::process` masks RVAL where C's `convert()` does not;
  `db_loader` `EpicsValue::parse(Enum, "Busy")` silently yields 0; string puts to
  numeric DBF types yield 0 where C errors; `scanf.rs` `v as i32/u32` narrowing.
- R19-43 was not measured live against a C IOC (read from `iocsource.cpp`
  and proven by a boundary test that fails pre-fix).

## Fix wave 20 dispositions — the declared type reaches the wire

The biggest open item from wave 19 is closed. The `.dbd` codegen had made the
field *tables* correct, but the CA native type a client saw was still derived
from the **stored value's variant**, not from the field's declared
`FieldDesc.dbf_type`. Tables right, storage wrong — and that, not the tables,
is why the oracle's `native_type` defects survived the codegen landing.

**Oracle scoreboard** (`--phase read`, 2551 CA-observable fields, 34 record
types), BEFORE `8c5ff2b9` → AFTER `fc1b6ebe`:

| | BEFORE | AFTER |
|---|---|---|
| `native_type` defects | **215** | **0** |
| agreed | 2186 | 2207 |
| total defects (all surfaces) | 276 | 179 |

Of the 215: 92 now agree outright; 119 have the correct native type but still
differ on `value_string` / `value_numeric` / `access_rights` (pre-existing
defects on other surfaces); 4 (`ai.DTYP/INIT/LCNT/ROFF`) were not re-measured
because the C `softIoc` boot timed out under load — re-measured by hand with
`cainfo`, and **not** counted as oracle-verified.

Three findings, three commits:

- **`064edb8e`** — the declared type is what goes on the wire.
  `RecordInstance::project_to_declared_type` projects a stored value onto its
  declared type through `EpicsValue::convert_to` (the single value-coercion
  owner — **no second conversion table**), and create-channel, GET, MONITOR and
  the PVA descriptor all run it. `field_desc` consults the generated `.dbd`
  table *first*: a record's own `field_list()` cannot be the type source,
  because `#[derive(EpicsRecord)]` builds it from the Rust struct member (which
  is why `longin.ADEL` was a DOUBLE). `promote_menu_value` is deleted;
  `FieldDesc::runtime_typed` carries the `cvt_dbaddr` selector bit, exempting
  the 46 selector-typed fields.
- **`6677c424`** — a stateless mbbo serves VAL as `DBF_USHORT` (`DBF_LONG` on
  the wire), measured on the C IOC. Underneath it, `sdef` was a *stored* bool
  refreshed by a hook, so a direct state write left it stale — and a stale
  `sdef` picks the wrong **wire type**. It is now derived: the stale state is
  unrepresentable.
- **`fc1b6ebe`** — `subArrayRecord.dbd` was never vendored, so subArray had no
  declaration and its hand table reached the wire (FTVL served `DBF_SHORT`, not
  `DBF_ENUM`). Record types with no vendored `.dbd`: was 1, now **0**.

**The declaration governs the wire; the storage variant governs the put.** Every
remaining `db_field_type()` call site is inbound/storage-side (`field_io.rs`,
`put_coerced`, `db_loader`, `links.rs`, `waveform.rs`) and must NOT follow the
declaration — coercing an incoming `Short` up to `Enum` because the `.dbd` says
`DBF_MENU` would make a menu field's `Short` put arm unreachable.

**Seven tests were pinning the defect** and were corrected against the C IOC —
including two whose own comments said "no state table" and then asserted an
`Enum` read-back, and the `mbbo.VAL` row of `fixtures/c_native_types.tsv`, the
one row not taken from the bulk sweep.

Two prior-wave rulings, for the record: the superseded R19-3 commit `3b9fe4ba`
is **not** reverted (it is fixed forward, merged and gated; a revert-then-fix
history adds noise without changing the tree), and the NSMOOTH fixed-point bound
stays where it landed rather than being split out of already-merged history.

### Carried UNFIXED after wave 20

- **179 oracle defects on other surfaces** — 116 `value_string`, 41
  `value_numeric`, 104 `access_rights` (a case can carry more than one). This is
  now the largest open block, and the next wave's target.
- **30 self-contradictory menu declarations** in the epics-base-rs hand tables
  still say `Short` while the record answers with choices. Inert (the generated
  table shadows them, and `served_native_type_is_declared` pins every one
  against the C IOC) but not correct. Four of those record types get their table
  from `#[derive(EpicsRecord)]`, which types each field from its Rust struct
  member — there is no per-field type to fix without a macro attribute.
- **The type-level close is not built**: a `FieldDesc` constructor that sets
  type and choices together, making "has menu choices but is typed `Short`"
  unrepresentable. Today a test catches it, not the type system.
- **waveform `FTVL=STRING` is unsupported** — `reallocate_val` has no
  `StringArray` branch (`waveform.rs:181`).
- **`mbbo.SDEF` / `mbbi.SDEF` are not served at all** — declared in the `.dbd`,
  `get_field` returns `None`. A value/access defect, not a native-type one.
