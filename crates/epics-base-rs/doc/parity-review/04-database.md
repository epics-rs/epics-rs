# Parity Review — server/database (db_access, field_io, link_set, links, processing, scan_index, filters)

Rust root: `crates/epics-base-rs/src/server/database/`
C reference: `epics-base/modules/database/src/ioc/db/` and `.../std/filters/`, `.../std/rec/`

Severity legend: Critical = corruption/crash/deadlock; High = wrong processing / missed
monitors / wrong alarms; Medium = edge-case divergence; Low = minor / feature gap.

---

## Critical

None found.

---

## High

### H1 — `fanout` record omits `LNK0`; forward link layout shifted by one
- **Rust:** `database/links.rs:276-279` (`dispatch_multi_output`, `"fanout"` branch);
  Rust record `server/records/fanout.rs:11-42` defines `lnk1..lnkf` only (15 fields).
- **C:** `std/rec/fanoutRecord.c:39` `#define NLINKS 16`; `dbd/fanoutRecord.dbd:54-129`
  declares `LNK0..LNKF` (16 forward links); `fanoutRecord.c:108-111` iterates
  `&prec->lnk0` for `NLINKS`.
- **Diverges:** The Rust `fanout` record has no `LNK0` field and `dispatch_multi_output`
  reads `["LNK1".."LNK9","LNKA".."LNKF"]` (15 entries). The C record's first forward
  link `LNK0` does not exist in the Rust model and is never dispatched. Every link
  index is shifted: a `.db` file written for C semantics where `LNK0` carries the
  primary fanout target produces a fanout that silently fans out to nothing on that
  slot.
- **Runtime impact:** A fanout configured with `LNK0` (the natural first slot)
  never processes its first downstream record. SELM=Specified / SELM=Mask indices
  are all off-by-one vs. C (see H2/H3).

### H2 — `dfanout` SELM=Specified is off-by-one (SELN is 1-based in C)
- **Rust:** `database/mod.rs:206-218` `select_link_indices`, `selm == 1` branch returns
  `vec![seln as usize]` (0-based); used by `dispatch_multi_output` `"dfanout"` at
  `links.rs:507-519`.
- **C:** `std/rec/dfanoutRecord.c:315-323` — `case dfanoutSELM_Specified`:
  `if (prec->seln == 0) break;` (SELN=0 → no output) and `plink += (prec->seln - 1)`
  (SELN=1 → OUTA).
- **Diverges:** C `dfanout` Specified mode is **1-based** with `SELN==0` meaning
  "drive nothing". The shared `select_link_indices` helper is **0-based**: `SELN==0`
  selects OUTA, `SELN==1` selects OUTB. Every Specified selection on a `dfanout` hits
  the wrong output link, and `SELN==0` wrongly drives OUTA instead of nothing.
- **Runtime impact:** A `dfanout` with `SELM=Specified` distributes the setpoint to
  the wrong downstream record (off by one), and an explicit "disable all outputs"
  (`SELN=0`) instead drives OUTA.

### H3 — `fanout`/`seq` SELM ignores `OFFS` (Specified) and `SHFT` (Mask)
- **Rust:** `database/mod.rs:206-218` `select_link_indices` — `selm==1` uses bare
  `seln`; `selm==2` uses bare `(seln as u16) & (1 << i)`.
- **C:** `fanoutRecord.c:115` `i = seln + prec->offs;` (Specified) and
  `fanoutRecord.c:131-134` `i = prec->shft; seln = (i >= 0) ? seln >> i : seln << -i;`
  (Mask). `seqRecord.c:155` `grpn = prec->seln + prec->offs;` and
  `seqRecord.c:164-171` identical SHFT shift.
- **Diverges:** The Rust helper never consults `OFFS` or `SHFT`. The Rust `FanoutRecord`
  even *defines* `offs` and `shft` fields (`records/fanout.rs:45-48`) but
  `dispatch_multi_output` never reads them. C also raises `SOFT_ALARM/INVALID` when
  the resolved index is out of range or `SHFT` is outside `[-15,15]`; the Rust helper
  silently returns an empty/clamped selection with no alarm.
- **Runtime impact:** Any `fanout`/`seq` that uses `OFFS` to bias the selected group
  or `SHFT` to position the mask processes the wrong links (or none), and the
  out-of-range diagnostic alarm is never raised.

### H4 — MS-class link alarm: `rec_gbl_set_sevr` does not clear the pending `namsg`
- **Rust:** `server/recgbl.rs:100-105` `rec_gbl_set_sevr` sets only `nsta/nsev`,
  never touches `namsg`. Called from `processing.rs:783-797` for `MonitorSwitch::
  Maximize` (MS) and `MonitorSwitch::MaximizeIfInvalid` (MSI).
- **C:** `recGbl.c:237-256` `recGblSetSevrVMsg` — when `msg == NULL` (the path
  `recGblSetSevr` takes) and the severity raises, it executes `prec->namsg[0] = '\0'`,
  i.e. it **clears** the pending alarm message.
- **Diverges:** In C, an MS (or MSI) link that raises the record's pending severity
  clears `namsg`, so the record's final `amsg` reflects "no message" for that
  LINK_ALARM. In Rust, `rec_gbl_set_sevr` leaves whatever `namsg` a prior
  `rec_gbl_set_sevr_msg` (MSS branch, or `evaluate_alarms`) wrote. When an MSS input
  set `namsg` and a later, higher-severity MS input raises the severity above it,
  C ends with the MS LINK_ALARM and an empty message; Rust ends with the MS severity
  but the **stale MSS message string**.
- **Runtime impact:** Wrong `AMSG` propagated to CA/PVA subscribers — a record that
  C reports as `LINK_ALARM`/`INVALID` with no message instead reports an unrelated
  upstream record's alarm text.

---

## Medium

### M1 — RPRO reprocessing is a synchronous inline recurse, not a queued `scanOnce`
- **Rust:** `processing.rs:1204-1221` — step 7 does `visited.remove(name)` then
  `process_record_with_links(name, visited, depth+1).await` inline.
- **C:** `recGbl.c:296-300` `recGblFwdLink` consumes RPRO via `scanOnce(pdbc)` —
  the record is **queued** on the scanOnce ring buffer and reprocessed in a fresh
  pass with a new lock cycle, after the current process chain fully unwinds.
- **Diverges:** Rust reprocesses RPRO records inline within the same link chain and
  the same `visited` set (after removing the record). Records that the reprocess
  fans out to which are already in `visited` are silently skipped by the cycle guard,
  whereas C's separate `scanOnce` pass sees a clean state. Depth/ops budget
  (`MAX_LINK_DEPTH`/`MAX_LINK_OPS`) can also abort an RPRO that C would have run.
- **Runtime impact:** In dense link chains an RPRO-triggered reprocess can silently
  skip downstream records or hit the depth cap; the timing/ordering relative to the
  scan thread differs from C.

### M2 — Channel filters run on single-read context (`dec`/`sync`/`dbnd` not bypassed)
- **Rust:** `database/filters/mod.rs:142-159` `FilterChain::apply_to_read_value`
  builds a synthetic `EventMask::VALUE` event and runs the **whole** chain.
- **C:** `decimate.c:64` and `sync.c:98` both short-circuit
  `if (pfl->ctx == dbfl_context_read ...) return pfl;` — read-context emissions
  bypass the decimator and the sync state machine entirely. `dbnd.c` only operates
  on `dbfl_type_val` but a read still mutates `my->last`.
- **Diverges:** The Rust synthetic read event carries no read-context marker, so a
  `dec` filter on a DB-link/one-shot read consumes a decimation slot, a `sync`
  filter gates the read by an unrelated state, and a `dbnd` filter advances its
  `last_sent` baseline from a read. C explicitly excludes all of these from the
  read path.
- **Runtime impact:** A channel name carrying `{"dec":...}` or `{"sync":...}` used
  for a DB-link read or `caget` returns dropped/None values where C returns the
  value; `dbnd` read calls desync the deadband baseline used by the monitor stream.

### M3 — `seq` record only models 10 link groups; C has 16, plus per-group `DLY`
- **Rust:** `links.rs:371-405` `dispatch_multi_output` `"seq"` branch reads
  `DOL1..DOL9,DOLA` / `LNK1..LNK9,LNKA` (10 groups), no `DLY`.
- **C:** `seqRecord.c:86` `#define NUM_LINKS 16`; `dbd/seqRecord.dbd` declares
  `DOL0/LNK0/DLY0 .. DOLF/LNKF/DLYF` (16 groups). `seqRecord.c` schedules each group
  after its `DLYn` delay.
- **Diverges:** The Rust `seq` is the legacy 3.14 10-group layout (and 1-based:
  starts at `DOL1`, so the C `DOL0` group is absent). Per-group `DLY` (the defining
  feature of `seq` — staggered sequenced writes) is ignored; all groups fire
  immediately.
- **Runtime impact:** `seq` records using groups 11–16 or group 0, or relying on
  `DLY` staggering, behave incorrectly. Feature gap, but `seq`'s purpose (delayed
  sequencing) is lost.

### M4 — `arr` filter ignores circular-buffer offset (`dbChannelGetArrayInfo`)
- **Rust:** `database/filters/arr.rs:92-117` `slice_with` slices a plain `Vec`
  starting at index 0.
- **C:** `arr.c:114-123` calls `dbChannelGetArrayInfo(chan,&pSource,&nSource,&offset)`
  and then `offset = (offset + start) % pfl->no_elements` before `dbExtractArray` —
  it accounts for the record's ring-buffer write offset.
- **Diverges:** For waveform-style records that present a circular buffer with a
  non-zero element offset, the C `arr` filter slices relative to the logical start;
  the Rust filter slices relative to physical index 0.
- **Runtime impact:** `arr` slices on a circular-buffer waveform return elements
  rotated by the buffer offset. Low real-world exposure for soft records (offset is
  usually 0) — flagged Medium because it is a silent wrong-data result when it does
  occur.

### M5 — `dfanout`/`fanout`/`seq` SELM out-of-range does not raise the C alarm
- **Rust:** `mod.rs:206-218` `select_link_indices` returns an empty `Vec` for an
  out-of-range Specified index and silently masks bits for Mask mode.
- **C:** `fanoutRecord.c:116-118`, `dfanoutRecord.c:316-318`, `seqRecord.c:156-158`
  all call `recGblSetSevr(prec, SOFT_ALARM, INVALID_ALARM)` (or `SOFT_ALARM`) when
  `SELN`/`SELN+OFFS` is out of range, and `fanoutRecord.c:128-130`/`seqRecord.c:165`
  raise it when `SHFT` is outside `[-15,15]`.
- **Diverges:** The Rust helper has no record handle and cannot raise an alarm; an
  out-of-range selector is silently a no-op.
- **Runtime impact:** Operator misconfiguration of `SELN`/`OFFS`/`SHFT` produces no
  alarm — the fanout silently does nothing instead of flagging `INVALID`.

---

## Low

### L1 — `ts` filter emits signed `Long` where C emits unsigned `DBF_ULONG`
- **Rust:** `filters/ts.rs:136-151` — `Seconds`/`Nanoseconds`/`Array` modes produce
  `EpicsValue::Long` (i32), clamping seconds to `i32::MAX`.
- **C:** `ts.c:199-238` — `ts_seconds`/`ts_nanos`/`ts_array` set
  `pfl->field_type = DBF_ULONG` and write `epicsUInt32`.
- **Diverges:** C presents the timestamp components as unsigned 32-bit. The Rust
  `Long` is signed; for `nsec` (always < 1e9) this is harmless, but a Unix-epoch
  `sec` value > `i32::MAX` (year 2038) is clamped/misencoded vs. C's full `epicsUInt32`.
- **Runtime impact:** Wire-type mismatch (`DBR_LONG` vs `DBR_ULONG`) and post-2038
  truncation for `epoch=unix` second values. Minor.

### L2 — Disable-bail posts STAT/SEVR with `DBE_ALARM` in addition to `DBE_VALUE`
- **Rust:** `processing.rs:280-298` — SDIS-disable branch builds one snapshot with
  `event_mask = VALUE | ALARM` covering STAT, SEVR and VAL together.
- **C:** `dbAccess.c:586-593` posts `&precord->stat` with `DBE_VALUE`, `&precord->sevr`
  with `DBE_VALUE`, and only the value field with `DBE_VALUE|DBE_ALARM`.
- **Diverges:** Rust attaches `DBE_ALARM` to the STAT/SEVR events as well. C scopes
  `DBE_ALARM` to the value field only.
- **Runtime impact:** A `DBE_ALARM`-only subscriber on `.STAT`/`.SEVR` receives an
  extra event on disable that C would not send. Cosmetic — no wrong value.

### L3 — `seq`/`sseq` decode of DOL/LNK uses `\0` field separators; brittle vs C
- **Rust:** `links.rs:403`, `476` pack `dol\0lnk` (and `dol\0lnk\0do\0str`) into a
  single `String`, then `splitn` it back in `dispatch_multi_output`.
- **C:** `seqRecord.c` keeps each group as a `linkGrp` struct — no string packing.
- **Diverges:** Not a behavioral bug today (record values never contain `\0`), but a
  string-encoded tuple is fragile: any future link string containing an embedded NUL
  (or a DOL link whose JSON form contains one) would mis-split. Flagged Low as a
  latent correctness hazard, not an active defect.

### L4 — `evaluate_calc_link` truncates input arg list at 12 silently
- **CLOSED, and the C claim in it was wrong.** `lnkCalc` supports `CALCPERFORM_NARGS`
  inputs, which `postfix.h:29` defines as **21** (A..U), not 12; `links.dbd.pod:131`
  says "up to 24" and is also wrong. C does not truncate at the limit either — it
  returns `jlif_stop` (`lnkCalc.c:135-139`), refusing the link.
- The parser now refuses more than `CALC_NARGS`, and `evaluate_calc_link` keeps the
  same bound for links built in-process.

---

## Verified correct (spot-checked, no divergence)

- `processing.rs` PACT entry guard (`MAX_LOCK=10`, post-increment semantics) matches
  C `dbAccess.c:545-558`.
- `write_db_link_value` / FLNK `processTarget` PUTF propagation (`!pact` → copy putf;
  `pact && src_putf && !on_chain` → `rpro=true,putf=false`) matches
  `dbDbLink.c:468-498`.
- `dbnd` filter delta rule (`c_delta`) matches `recGblCheckDeadband` exactly,
  including NaN/±inf transitions and strict `>` comparison.
- `dbnd` 446e0d4a behavior (ALARM/PROPERTY bypass the gate, `last_sent` still
  advances when supra-threshold) is correct.
- `arr` index wrapping (`wrapArrayIndices` asymmetric start/end clamp) matches C.
- `decimate` PROPERTY-bypass / ALARM-consumes-slot matches `decimate.c`.
- `sync` six-mode state machine (Before/First/Last/After/While/Unless cache + emit)
  matches `sync.c::filter` switch.
- `rec_gbl_reset_alarms` transfer + `acks` update matches `recGblResetAlarms`
  (the only delta is the suppressed redundant ACKS post when `acks==sevr`, which is
  harmless).
- `apply_timestamp` TSE handling (`0`/`-1`/`-2`/`>0`) matches `recGblGetTimeStampSimm`.
- Alias-aware normalization across `get_pv`/`put_pv`/`put_record_field_from_ca` is
  consistent (PR #336).
- `put_pv` empty-array→scalar rejection matches the C 12cfd41 LINK_ALARM fix intent.
