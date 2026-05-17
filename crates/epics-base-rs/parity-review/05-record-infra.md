# Parity Review 05 — Record Infrastructure (alarm / recGbl / scan)

Scope: `src/server/record/{alarm,common_fields,link,record_instance,record_trait,scan,mod}.rs`,
`src/server/recgbl.rs`, `src/server/scan.rs`, `src/server/scan_event.rs`,
plus scan support in `src/server/database/{scan_index,mod,processing}.rs`.

C reference: `epics-base/modules/database/src/ioc/db/{recGbl.c,dbScan.c}`,
`dbCommon.dbd.pod`, `menuScan.dbd.pod`, `modules/libcom/src/misc/alarm.h`,
`modules/libcom/src/osi/epicsTime.h`.

Verified: alarm severity ordering, alarm status enum values, menuScan rate
values, recGblSetSevr maximize logic, recGblResetAlarms transfer/acks/ackt,
TSE constants, scan-type numbering, FLNK forward-link semantics — all match C.

---

## HIGH

### H1 — Event scan does not route by event number; every Event record fires on any event
- Rust: `src/server/database/scan_index.rs:110-117` (`post_event`), and
  `src/server/scan_event.rs:142-148` (`submit_event` is per-record, not per-event).
- C: `dbScan.c:548-552` `post_event(int event)` → `postEvent(pevent_list[event])`;
  `dbScan.c:535-545` `postEvent` walks only the `scan_list` belonging to that
  one `event_list`. Each `event_list` (`dbScan.c:469-533` `eventNameToHandle`)
  holds exactly the records whose `EVNT` resolves to that event.
- Divergence: Rust `post_event()` takes **no event argument** and runs
  `records_for_scan(ScanType::Event)` — i.e. **every** record with
  `SCAN=Event`, regardless of its `EVNT` value. The `evnt` field is stored
  but never consulted to route the fan-out.
- Runtime impact: a record configured `SCAN=Event, EVNT=5` is processed every
  time *any* event is posted, not only event 5. Event-driven databases that
  partition records across multiple event numbers will mass-process unrelated
  records — wrong processing rate, spurious FLNK chains, wrong timestamps for
  records that expected to fire on their own event. The entire event-routing
  layer (`eventNameToHandle` / `pevent_list[]`) is absent.

### H2 — `EVNT` field has the wrong type (`i16` instead of event-name string)
- Rust: `src/server/record/common_fields.rs:61` (`pub evnt: i16`),
  read/written as `EpicsValue::Short` at `record_instance.rs:646` and `:928`.
- C: `dbCommon.dbd.pod:181-187` — `field(EVNT,DBF_STRING) { size(40) }`.
  Since EPICS 7 `EVNT` is an **event name** (string), resolved by
  `eventNameToHandle` (`dbScan.c:469`) which accepts either a numeric string
  or a symbolic name. `scanAdd` (`dbScan.c:256`) reads `precord->evnt` as
  `char *`.
- Runtime impact: a Rust IOC cannot accept named events, and a CA/PVA client
  doing a string GET/PUT on `.EVNT` sees a numeric field of the wrong DBF
  type — wire-type mismatch vs a C IOC. Combined with H1, named-event
  databases do not work at all. (Pre-EPICS-7 numeric-only events would still
  be salvageable, but the routing in H1 is the blocking gap.)

---

## MEDIUM

### M1 — Same-PHAS records processed in alphabetical name order, not database load order
- Rust: `src/server/database/mod.rs:146`
  `scan_index: RwLock<HashMap<ScanType, BTreeSet<(i16, String)>>>`;
  `scan_index.rs:73-81` `records_for_scan` iterates the `BTreeSet` in
  `(phas, name)` sort order.
- C: `dbScan.c:1075-1095` `addToList` inserts each record after the last
  element with `phas <= precord->phas`; `buildScanLists` (`dbScan.c:1052-1073`)
  walks records in database/record-type **load order**. Within one PHAS value
  the C scan list preserves load order (stable FIFO append).
- Divergence: the `BTreeSet` key `(i16, String)` makes the secondary sort key
  the **record name**, so records sharing a PHAS are scanned alphabetically.
- Runtime impact: PHAS still correctly orders records *across* phases, but two
  records with identical PHAS that depend on intra-phase ordering (a documented
  but discouraged pattern) process in a different order than a C IOC built
  from the same `.db` file. Edge-case behavioral divergence.

### M2 — `recGblResetAlarms` monitor masks collapsed; SEVR over-posted on stat-only change
- Rust: `src/server/recgbl.rs:132-190` returns only booleans
  (`alarm_changed`, `amsg_changed`, `acks_changed`); consumer at
  `src/server/database/processing.rs:1044-1053` posts **both** SEVR and STAT
  whenever `alarm_changed` is true (`alarm_changed = sevr != prev || stat != prev`).
- C: `recGbl.c:178-224`. SEVR is posted **only** when `prev_sevr != new_sevr`
  (`db_post_events(&pdbc->sevr, DBE_VALUE)`, line 204). STAT/AMSG are posted
  when `stat_mask != 0`; `stat_mask` is `DBE_ALARM` for a sevr or amsg change
  and `DBE_VALUE` for a stat change — the per-field event mask is
  field-specific, not a single record-wide mask.
- Divergence:
  1. If only STAT changed (SEVR unchanged), C does **not** post SEVR; Rust
     posts SEVR anyway.
  2. C posts STAT with `DBE_ALARM`-only on a sevr-driven change vs
     `DBE_VALUE` on a stat-driven change; Rust uses one coarse `event_mask`
     for the whole snapshot.
- Runtime impact: extra SEVR monitor updates to subscribers on stat-only
  transitions (e.g. status change within the same severity), and DBE_ALARM /
  DBE_VALUE subscribers may receive a field on a mask they did not select.
  Not corruption — over-notification and slightly wrong mask granularity.

---

## LOW

### L1 — `UTAG` (time-tag) field absent from `CommonFields`
- Rust: `src/server/record/common_fields.rs:8-80` — no `utag` field.
- C: `dbCommon.dbd.pod:570-574` `field(UTAG,DBF_UINT64)`; set together with
  `prec->time` by `recGblGetTimeStampSimm` via `dbGetTimeStampTag`
  (`recGbl.c:317`, `:331`).
- Impact: a CA/PVA client cannot read `.UTAG`; high-resolution / hardware
  time-tag workflows lose the tag. Feature gap, no incorrect behavior for
  records that do not use time tags.

### L2 — `TSEL` time-stamp link is parsed but never used
- Rust: `record_instance.rs` stores `parsed_tsel` (set at the `TSEL` write
  path) but `rg parsed_tsel` shows **no read site**. `apply_timestamp`
  (`src/server/database/mod.rs:47-79`) only switches on `common.tse` and
  never resolves `TSEL`.
- C: `recGbl.c:310-323` `recGblGetTimeStampSimm` — when `TSEL` is a non-constant
  link it either copies a timestamp directly (`DBLINK_FLAG_TSELisTIME`) or
  does `dbGetLink(plink, DBR_SHORT, &prec->tse, ...)` to load `TSE` from the
  link before the event lookup.
- Impact: records configured with `TSEL` pointing at another record's `.TIME`
  or `.TSE` field get the default current-time stamp instead of the linked
  timestamp/TSE. Feature gap; affects only databases that use `TSEL`.

### L3 — Periodic scan threads have no priority ordering
- Rust: `src/server/scan.rs:88-104` and `scan_event.rs:87-96` spawn one tokio
  task per rate, all at equal scheduling priority.
- C: `menuScan.dbd.pod:34-36` and `dbScan.c:952` — each periodic scan thread
  is created with **increasing thread priority** for faster rates, so a
  10 Hz scan preempts a 0.1 Hz scan under CPU pressure.
- Impact: under saturation, fast periodic scans are not prioritized over slow
  ones; cooperative tokio scheduling makes this mostly benign but it is a
  real-time-behavior divergence from C.

### L4 — Customized `menuScan` rates / `Hertz` / `minute` units not supported
- Rust: `src/server/record/scan.rs:6-18` hardcodes exactly the 7 default
  periodic rates as enum variants; `interval()` (`:62-73`) hardcodes their
  durations.
- C: `dbScan.c:857-885` `initPeriodic` parses the menuScan choice strings at
  runtime, accepting `second(s)`, `minute(s)`, `hour(s)`, `Hertz`, `Hz` units
  — an IOC may override `menuScan.dbd` with arbitrary rates.
- Impact: a site that customizes `menuScan.dbd` (documented, supported in C)
  cannot do so here. Standard databases are unaffected.

---

## Non-issues confirmed (checked, no divergence)

- `AlarmSeverity` ordering/values (`alarm.rs:1-22`) match `alarm.h:39-45`.
- `alarm_status` constants (`recgbl.rs:9-30`) match `alarm.h:62-114`
  (0..21, READ..WRITE_ACCESS) — wire values correct.
- `menuScan` rate values & numbering: Passive=0, Event=1, IoIntr=2,
  10s=3 … .1s=9 (`scan.rs:6-18`) match `menuScan.dbd.pod:46-58` and
  `dbScan.h:29-32` `SCAN_1ST_PERIODIC = menuScanI_O_Intr + 1 = 3`.
- `rec_gbl_set_sevr` maximize-only logic (`recgbl.rs:100-105`) matches
  `recGblSetSevrVMsg` (`recGbl.c:242-256`).
- `rec_gbl_reset_alarms` nsta/nsev→stat/sevr transfer, reset, and the
  `ackt`/`acks` sticky-alarm branch (`recgbl.rs:132-190`) match
  `recGblResetAlarms` (`recGbl.c:197-223`). The `nsev > Invalid` clamp is dead
  code in Rust (enum is 0..3) but harmless.
- `recGblInheritSevrMsg` MS/MSI/MSS/NMS handling (`processing.rs:779-808`)
  matches `recGbl.c:263-281`.
- TSE event constants 0 / -1 / -2 (`database/mod.rs:47-79`) match
  `epicsTime.h:102-104` (`Current` / `Best` / `Device`); TSE=-2 correctly
  leaves device-set time untouched.
- FLNK forward-link: target processed only when `SCAN=Passive`, with `putf`
  propagation and `rpro` on busy target (`processing.rs:1176-1199`) matches
  `dbScanFwdLink` / `dbScanPassive`.
- I/O Intr scan exists via device `intr_receiver` (`ioc_app.rs` `setup_io`).
