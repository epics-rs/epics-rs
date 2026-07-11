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
