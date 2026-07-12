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
- Periodic scan-list ordering: `(PHAS, load_order)` `BTreeSet` reproduces `addToList`'s "insert after the last element with `phas <= new phas`" stable ordering (`scan_index.rs:62-95` vs `dbScan.c:1075-1095`).
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
Impact: Two divergences on the wire. (1) A Rust consumer that holds a `MonitorHandle` and stops polling it leaves `outstanding` above 5 forever, so `EVENTS_OFF` is never lifted — every *other* subscription on the same circuit stops receiving monitors indefinitely. libca cannot reach that state: the moment the socket drains it emits `EVENTS_ON`. (2) The trigger is hard-coded at 10 and never scaled by `EPICS_CA_MAX_ARRAY_BYTES`, so a large-waveform circuit trips `EVENTS_OFF` far earlier than libca does. `doc/09-libca-parity.md:78-80` and `doc/07-flow-control.md:40-44` claim libca has a "per-server outstanding-monitor counter" with "hysteresis (10 / 5)" — libca has neither.

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
Rust: `crates/epics-pva-rs/src/client_native/decode.rs:486-491` — the RPC data branch unconditionally calls `decode_type_desc_cached(&mut cur, order, type_cache)?` then `decode_pv_field_cached(&resp_desc, …)?`; a `0xFF` type byte is rejected (`pvdata/encode.rs:683`) and the RPC fails with `PvaError::Decode`. Symmetrically, `crates/epics-pva-rs/src/server_native/tcp.rs:7291-7294` always writes `encode_type_desc(&resp_desc, order, &mut payload)` followed by `encode_pv_field(&resp_value, &resp_desc, order, &mut payload)` — there is no "reply with no value" shape.
C reference: `/home/stevek/work/epics-modules/pvxs/src/serverget.cpp:105-109` — `else if(cmd==CMD_RPC) { auto type = Value::Helper::desc(value); to_wire(R, type); if(value) to_wire_full(R, value); }`. `ExecOp::reply()` (the no-argument overload, `src/pvxs/srvcommon.h:108`) reaches `doReply(Value(), …)`, so `desc()` is `nullptr` and `to_wire(Buf&, const FieldDesc*)` (`dataencode.cpp:29-33`) emits exactly one `0xff` byte with no value body. The pvxs client accepts it: `src/clientget.cpp:415-421` — `from_wire_type(M, rxRegistry, data); if(data) from_wire_full(M, rxRegistry, data);`.
Impact: Against any pvxs RPC handler that calls `op->reply()` instead of `op->reply(value)`, the Rust client's RPC fails with a decode error on a well-formed 6-byte reply body (`ioid | subcmd | Status | 0xFF`) that pvxs's own client completes with an empty `Value`. In the reverse direction the Rust server has no way to express that reply at all.

### R6-35: Rust client discards a MONITOR FINISH frame's trailing value/overrun body; pvxs decodes it
Severity: Low
Rust: `crates/epics-pva-rs/src/client_native/decode.rs:418-426` — `if cmd == Command::Monitor && subcmd & 0x10 != 0 { let status = Status::decode(…)?; return Ok(OpResponse::Status(…)); }`, checked before the INIT branch and before any data decode. Any bytes after the Status are dropped. The raw-forwarding monitor loop then treats the frame as `RawMonitorFrameKind::FinishOk` and returns `Ok(())` (`crates/epics-pva-rs/src/client_native/ops_v2.rs:2541-2552`), so nothing downstream ever sees the body.
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
Impact: a `HOST(node)` HAG rule that grants WRITE in C grants nothing in Rust on identical `.acf`; CA_PROTO_ACCESS_RIGHTS and caput enforcement differ. Three docs (`doc/09-libca-parity.md:159`, `doc/04-server.md:119`, `doc/08-environment.md:178`) falsely assert this "matches C rsrv default".

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
