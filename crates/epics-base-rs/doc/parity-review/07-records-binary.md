# Parity Review 07: Binary / Multibit / Fanout / Sel / Event Records

Scope: bi, bo, mbbi, mbbo, mbbiDirect, mbboDirect, busy, event, fanout, dfanout, sel.
Rust dir: `crates/epics-base-rs/src/server/records/`
C ref: `epics-base/modules/database/src/std/rec/`

---

## CRITICAL

### C-1. fanout: LNK0 link is missing entirely
- Rust: `fanout.rs:13-41` — struct has `lnk1..lnkf` (15 links). No `lnk0`.
- Rust: `links.rs:276-294` — dispatch list is `["LNK1".."LNKF"]` (15 entries).
- C: `fanoutRecord.c:39` `#define NLINKS 16`; `fanoutRecord.c:108-139` iterates `&prec->lnk0` for 16 links. DBD `fanoutRecord.dbd.pod:139-214` defines `LNK0..LNKF`.
- Diverges: The first forward link `LNK0` does not exist in the Rust record. A `.db` file with `field(LNK0, "...")` either fails to set the field or the link is silently dropped.
- Runtime impact: With `SELM=All`, LNK0's target is never processed — a whole downstream chain silently never runs. With `SELM=Specified`, every index is off by one relative to C (see C-2). With `SELM=Mask`, bit 0 of SELN selects nothing. This is data-path corruption of the forward-link fan-out.

### C-2. fanout/dfanout SELM=Specified: wrong index base, OFFS ignored (fanout)
- Rust: `mod.rs:206-218` `select_link_indices`, `selm==1` branch: `let i = seln as usize; if i < count { vec![i] }`.
- C fanout `fanoutRecord.c:114-122`: `i = seln + prec->offs;` then `plink = &prec->lnk0 + i` — index is `SELN + OFFS`, 0-based over LNK0..LNKF.
- C dfanout `dfanoutRecord.c:315-325`: `if(seln>OUT_ARG_MAX) invalid; if(seln==0) break; plink += (seln-1)` — index is `SELN - 1`, **1-based**, SELN=0 means "no link".
- Diverges:
  - fanout: Rust ignores `OFFS` completely (field exists in struct `fanout.rs:45` but is never read in `links.rs`). With OFFS≠0 every Specified selection picks the wrong link.
  - dfanout: Rust treats SELN as a 0-based index into OUTA..OUTP; C treats it as 1-based with SELN=0 = no output. So Rust `SELN=1` drives OUTB where C drives OUTA, and Rust `SELN=0` drives OUTA where C drives nothing.
- Runtime impact: Wrong output link receives the value (dfanout), or wrong record is forward-processed (fanout). Silent mis-routing of setpoint distribution.

### C-3. fanout SELM=Mask: SHFT ignored, no range check
- Rust: `mod.rs:213-215`, `selm==2`: `(0..count).filter(|i| (seln as u16) & (1 << i) != 0)`.
- C `fanoutRecord.c:124-140`: applies `SHFT` first — `seln = (i>=0) ? seln>>i : seln<<-i;` with `i=prec->shft`, and sets `SOFT_ALARM/INVALID` if `shft` outside `-15..15`.
- Diverges: Rust never reads `shft` (`fanout.rs:47` field unused in dispatch). A fanout configured with `SELM=Mask` + `SHFT≠0` selects the wrong set of links. No INVALID alarm on out-of-range shift.
- Runtime impact: Wrong subset of forward links fires; missing alarm.

---

## HIGH

### H-1. event record: VAL has wrong type (i16 instead of String)
- Rust: `event.rs:9-10` — `#[field(type="Short")] pub val: i16`.
- C: `eventRecord.dbd.pod:71` `field(VAL,DBF_STRING)`. `eventRecord.c:102,120,150,187-190` — VAL is a C string event name; `postEvent(eventNameToHandle(prec->val))` posts a *named* event.
- Diverges: Modern EPICS event records post by *event name string* (any string), and historically by numeric subscript. The Rust record models only a numeric short and cannot represent named events at all. `eventNameToHandle` / `postEvent` semantics are entirely absent.
- Runtime impact: An event record loaded with `field(VAL,"myEvent")` cannot store its name; named-event scanning (`SCAN="Event"`, `EVNT="myEvent"`) on other records will never be triggered by this record. The record is effectively non-functional as an event source.

### H-2. event record: process() does not post the event
- Rust: `event.rs` — no `process()` impl; derives the default via `EpicsRecord` macro. No `postEvent` equivalent.
- C: `eventRecord.c:107-132` `process()` calls `postEvent(prec->epvt)` every cycle — that is the record's entire purpose.
- Diverges: The Rust event record's process is a no-op scan trigger. It posts CA monitors on VAL (via macro) but never fires the EPICS event mechanism that wakes `SCAN=Event` records.
- Runtime impact: Event-driven processing chains keyed off this record never run. Feature gap = the record does nothing useful.

### H-3. mbbiDirect / mbboDirect: only 16 bits, C uses 32 (B0..B1F)
- Rust: `mbbi_direct.rs:16` `bits: [u8;16]`; `BIT_NAMES` (`:59-61`) = `B0..BF`. `val_to_bits`/`bits_to_val` loop `0..16`. Same in `mbbo_direct.rs:23,72-74,58-69`.
- C: `mbbiDirectRecord.c:88` `#define NUM_BITS 32`; loops `&prec->b0` over 32 fields `B0..B1F`. `mbboDirectRecord.c:89` same. DBD defines `B0..B1F`.
- Diverges: Rust models a 16-bit record. Fields `B10..B1F` do not exist; bits 16..31 of VAL/RVAL are not exposed and not folded back. `init_record` also caps `nobt<=16` (`mbbi_direct.rs:216`, `mbbo_direct.rs:266`) whereas C allows `nobt<=32`.
- Runtime impact: A mbbiDirect/mbboDirect using more than 16 bits loses the upper half of its bit-field interface; `field(B1A,"1")` fails. With NOBT in 17..32 the MASK is wrongly computed/clamped.

### H-4. mbboDirect process: RBV updated incorrectly and at wrong time
- Rust: `mbbo_direct.rs:309-311`: `self.orbv = self.rbv; self.rbv = self.rval; self.oraw = self.rval;`
- C: `mbboDirectRecord.c` — `convert()` only sets `RVAL`. `RBV` is *never* assigned by record support; it is updated solely by device support (read-back). `monitor()` posts RBV only when device support changed it.
- Diverges: Rust forces `RBV = RVAL` every process, destroying any read-back value device support wrote. C keeps RBV as the true hardware read-back.
- Runtime impact: RBV always mirrors the commanded RVAL, so a client reading RBV to detect hardware disagreement always sees agreement — read-back monitoring is defeated.

### H-5. bo process: missing UDF→INVALID alarm and STATE alarm; HIGH semantics wrong
- Rust: `bo.rs:247-274` `process()` does VAL→RVAL conversion and a HIGH reprocess timer, but never evaluates ZSV/OSV state alarm or COS alarm, and the HIGH timer reprocesses the record after HIGH seconds *with VAL unchanged*.
- C: `boRecord.c:366-387` `checkAlarms` sets UDF, STATE (`zsv`/`osv`), COS (`cosv`). `boRecord.c:100-120,257-262` — the HIGH callback sets `prec->val = 0` then `dbProcess`, i.e. after HIGH seconds the output is driven back to 0 (one-shot pulse).
- Diverges: (a) bo has no state/COS alarm evaluation in `process()` at all — the framework may handle UDF, but ZSV/OSV/COSV are recorded fields with no code path that consults them. (b) The Rust HIGH timer just re-runs `process()`; it does not set `val=0`, so the bo never returns to the Done state. A "momentary" bo configured with HIGH stays at 1 forever.
- Runtime impact: Wrong alarm severity (no state alarm), and momentary/pulsed bo outputs never reset — a relay pulse stays energized.

### H-6. busy record: IVOA "Don't drive" is a no-op; HIGH unimplemented
- Rust: `busy.rs:342-347` — `Ivoa::DontDriveOutputs` branch is empty with a comment admitting "the framework still writes OUT". `busy.rs:355` HIGH timer "Phase C — skip for now".
- C boRecord semantics (busy is a bo variant): `boRecord.c:228-229` IVOA=Don't_drive skips `writeValue` entirely; HIGH drives a one-shot pulse.
- Diverges: With SEVR=INVALID and IVOA=Don't_drive, the busy record still writes its OUT link. HIGH is silently ignored.
- Runtime impact: Invalid-state outputs are still driven to hardware (safety-relevant for busy used as an interlock); HIGH-based auto-clear of the busy flag never happens.

### H-7. bi/mbbi: state alarm (ZSV/OSV, ZRSV..FFSV, UNSV) and COS alarm not evaluated in process()
- Rust: `bi.rs:176-189` `process()` does only RVAL→VAL and `oraw` update. `mbbi.rs:558-569` similar. The AFTC/AFVL alarm-filter fields and ZSV/OSV/COSV/UNSV severities are stored but no `process()` code reads them.
- C: `biRecord.c:227-275` `checkAlarms` — UDF, STATE alarm (zsv/osv) with the AFTC low-pass filter (`THRESHOLD 0.6321`), COS alarm. `mbbiRecord.c:293-349` same for the 16-state severities + UNSV.
- Diverges: The Rust bi/mbbi `process()` never computes STATE_ALARM or COS_ALARM, and the AFTC alarm filter (the `afvl` accumulator, the whole reason `aftc`/`afvl` exist) is dead code. Unless the framework re-implements this elsewhere (no evidence found — these fields are record-local), a bi with ZSV=MAJOR never goes into alarm.
- Runtime impact: Binary inputs configured with state-based alarm severity produce no alarm; change-of-state alarms never fire; AFTC filtering absent. Wrong/missing alarms.

### H-8. mbbo checkAlarms / convert SOFT_ALARM not implemented
- Rust: `mbbo.rs:599-607` `process()` calls only `convert()`. `convert()` (`:188-201`) for `val>15` with SDEF just `return`s — no alarm.
- C: `mbboRecord.c:373-394` `checkAlarms` sets STATE alarm from `zrsv..ffsv`/`unsv` and COS from `cosv`. `mbboRecord.c:418-435` `convert` sets `SOFT_ALARM/INVALID` when `val>15` with SDEF.
- Diverges: mbbo has no state/COS alarm and no SOFT_ALARM on illegal VAL.
- Runtime impact: Multibit outputs with per-state severities never alarm; an out-of-range VAL silently leaves RVAL stale instead of raising INVALID.

### H-9. sel SELM=Specified: SELN range check uses `>= SEL_MAX` but Rust seln is i16 — negative SELN
- Rust: `sel.rs:83-85` `get_value_by_index` uses `.get(idx)` where `idx = self.seln as usize`; a negative `seln` cast to `usize` becomes huge → `get` returns `None` → falls back to old `self.val` silently.
- C: `selRecord.c:355-359` `if (prec->seln >= SEL_MAX) { SOFT_ALARM/INVALID; return; }`; SELN is `DBF_USHORT` (always ≥0). Out-of-range raises INVALID.
- Diverges: Rust `seln` is `i16` (`sel.rs:9`), C is `epicsUInt16`. With SELN out of range Rust silently keeps the previous VAL and raises **no** SOFT_ALARM; C raises INVALID.
- Runtime impact: A misconfigured SELN gives a stale VAL with no alarm — operator sees a frozen value with NO_ALARM.

---

## MEDIUM

### M-1. sel SELM=Specified: VAL not set to UDF / no NaN-driven UDF
- Rust: `sel.rs:241-245` Specified: if selected value not finite, keep `self.val` (and never set UDF).
- C: `selRecord.c:401-402` `prec->val = val; prec->udf = isnan(prec->val);` — VAL is set to the (possibly NaN) selected value and UDF tracks it.
- Diverges: Rust never propagates a NaN input into VAL/UDF for Specified mode; C does, which then triggers `UDF_ALARM` in `checkAlarms`.
- Runtime impact: A sel pointing at an undefined input shows a stale value instead of UNDEFINED/INVALID.

### M-2. sel: limit alarms (HIHI/HIGH/LOW/LOLO + HYST) entirely missing
- Rust: `sel.rs` — `process()` has no `checkAlarms`; struct has no HIHI/HIGH/LOW/LOLO/HHSV/HSV/LSV/LLSV/HYST/LALM fields.
- C: `selRecord.c:250-304` full limit-alarm logic with hysteresis.
- Runtime impact: sel record cannot raise analog limit alarms — feature gap. (dfanout has the same gap, see M-3.)

### M-3. dfanout: limit alarms, MDEL/ADEL deadband, UDF check missing
- Rust: `dfanout.rs` — struct has no HIHI..LOLO, HYST, LALM, MDEL/ADEL/MLST/ALST. `process()` not implemented (macro default); no `checkAlarms`/`monitor`.
- C: `dfanoutRecord.c:227-299` checkAlarms (limit alarms + UDF) and monitor (deadband).
- Note: IVOA handling for dfanout *is* implemented in `links.rs:311-337`, but it keys off `common.sevr==Invalid`; since dfanout never computes limit alarms, SEVR will essentially always be NO_ALARM, so IVOA=Set_to_IVOV / Don't_drive can never actually trigger.
- Runtime impact: dfanout never alarms; its IVOA branch is unreachable in practice.

### M-4. sel SELM=Median: SELN not updated to count; High/Low SELN not updated
- Rust: `sel.rs:246-264` Median branch computes `sorted[len/2]` but never writes `self.seln`. High (`:247`) / Low (`:251`) branches never write `seln` to the winning index.
- C: `selRecord.c:361-395` — High/Low set `prec->seln = i` (index of selected input); Median sets `prec->seln = count` (number of valid inputs).
- Diverges: After process, SELN does not reflect which input won (High/Low) or how many were valid (Median).
- Runtime impact: A client/db reading SELN to learn which signal was selected gets a stale value; `monitor` SELN-change posting also wrong.

### M-5. sel Median: `sorted[len/2]` vs C `order[count/2]` — matches, but empty/odd handling
- Rust: `sel.rs:256-262` returns `self.val` unchanged when no valid inputs.
- C: `selRecord.c:380-395` — `order[0]=epicsNAN`, `count=0`, `val=order[count/2]=order[0]=NaN` when no valid inputs; then `val=NaN`, `udf=true`.
- Diverges: With zero valid inputs Rust keeps old VAL (no UDF); C sets VAL=NaN and UDF.
- Runtime impact: Frozen value vs UNDEFINED alarm — minor but observable.

### M-6. mbbi/mbbo: SDEF recompute on field write missing (special() ZRST..FFVL)
- Rust: `mbbi.rs` / `mbbo.rs` — `compute_sdef()` is called only in `init_record`. Writing ZRVL..FFVL or ZRST..FFST at runtime via `put_field` does not recompute `sdef`.
- C: `mbbiRecord.c:204-227` / `mbboRecord.c:269-292` `special()` calls `init_common()` (recomputes `sdef`) after any ZRVL..FFVL / ZRST..FFST modification, and re-posts VAL if the changed state string is the current one.
- Diverges: After a CA put to e.g. `ONVL`, the Rust record's `sdef` is stale. If the record started with all-zero states (`sdef=false`) and a state value is later written, conversion stays in the no-states `VAL=RVAL` path.
- Runtime impact: Runtime reconfiguration of mbbi/mbbo state tables does not take effect until restart; wrong RVAL↔VAL conversion.

### M-7. bo: DOL constant parsing rejects negative / hex / float, and ignores non-constant DOL at process
- Rust: `bo.rs:90-96` `dol_as_constant` does `s.parse::<u16>()` — fails on `-1`, `0x10`, `1.0`. `bo.rs:248-253` process only handles *constant* DOL; a real DB/CA link DOL (the normal closed-loop case) is not fetched here.
- C: `boRecord.c:191-205` process: `dbGetLink(&prec->dol, DBR_USHORT, ...)` for non-constant DOL when `omsl==closed_loop`, sets `LINK_ALARM` on failure. `recGblInitConstantLink` uses `DBF_USHORT` conversion (handles the numeric forms uniformly).
- Diverges: Closed-loop bo with DOL pointing at another PV — the framework may resolve DOL elsewhere, but bo's own `process()` only special-cases a *constant* DOL string and silently does nothing for a link DOL. No LINK_ALARM on a broken DOL.
- Runtime impact: If the framework's generic DOL handling doesn't cover bo, a closed-loop bo never updates VAL from DOL; broken DOL raises no alarm.

### M-8. bi `apply_raw_input`: MASK applied unconditionally; C devBiSoftRaw differs subtly
- Rust: `bi.rs:203-212` — reads INP into RVAL, then `if mask!=0 { rval &= mask }`.
- C `devBiSoftRaw` (`devBiSoftRaw.c`): reads into RVAL; mask handling — base's Raw Soft devsup uses MASK only when set by the record/dset; the cited PR `f2fe9d12` is real but the C ordering also sets `udf=FALSE`.
- Note: `bi.rs` `apply_raw_input` does not clear UDF. C `readValue` (`biRecord.c:328-330`) sets `prec->udf=FALSE` after a successful raw soft read.
- Runtime impact: A Raw-Soft-Channel bi may stay UDF=true after a successful read → spurious UDF_ALARM.

### M-9. mbbi/mbbiDirect Raw Soft / SIMM: VAL stays UDF
- Rust: bi/mbbi/mbbiDirect — no `process()` path clears UDF on a successful conversion. C `biRecord.c:146`, `mbbiRecord.c:174`, `mbbiDirectRecord.c:163` set `prec->udf=FALSE` after RVAL→VAL.
- Diverges: Whether UDF is cleared depends on framework glue; the record's own `process()` never clears it. If the framework doesn't, every input record stays UDF.
- Runtime impact: Potential permanent UDF_ALARM on inputs. (Severity Medium pending framework verification — flagged because no UDF clear exists in any reviewed record `process()`.)

---

## LOW

### L-1. mbbi/mbbo `set_val` Enum branch bypasses raw conversion (mbbi)
- Rust: `mbbi.rs:625-629` — when a device delivers `EpicsValue::Enum`, VAL is set directly with no SHFT/SDEF mapping. That is correct *only if* the device truly delivers an index. For mbbi the device delivers RVAL (raw), so an Enum-typed device value would skip conversion. Likely benign given device-support typing, but undocumented.

### L-2. busy `put_field("VAL", String)` parses arbitrary integers
- Rust: `busy.rs:409-417` — a VAL string not matching ZNAM/ONAM falls back to `s.parse::<u16>().unwrap_or(0)`. C `put_enum_str` returns `S_db_badChoice` for an unrecognized string. Rust silently coerces "garbage"→0.
- Runtime impact: A bad enum-string put succeeds with VAL=0 instead of being rejected.

### L-3. busy `convert_val_to_rval`: RVAL is u32, framework RBV mirror
- `busy.rs` `process()` sets `rbv` only via `monitor()` which does `orbv = rbv` (never assigns rbv). RBV therefore stays 0 unless device support writes it — this is actually correct vs C. No bug; noting for completeness.

### L-4. event record: SIMM/SIML/SIOL/SIMS present but unused
- `event.rs` has simulation fields but no simulation logic and no `process()`. C `eventRecord.c:167-212 readValue` implements full SIMM simulation. Feature gap, low impact (event records rarely simulated).

### L-5. bi/bo: ORAW updated in `process()` directly instead of in a `monitor()` step
- `bi.rs:187`, `bo.rs:258` set `oraw=rval` at end of process. C updates `oraw` inside `monitor()` *after* posting the RVAL change event (`biRecord.c:294-298`). Functionally the value ends up the same, but the RVAL-change monitor event is the framework's responsibility; if the framework compares against ORAW it will already equal RVAL and suppress the event. Potential missed RVAL monitor — flagged Low pending framework check.

### L-6. mbbo `convert` left-shift uses `u32` cast of i32 rval
- `mbbo.rs:198-200` `((self.rval as u32) << shft) as i32`. For large SHFT/RVAL this wraps silently. C `mbboRecord.c:433-434` `prec->rval <<= prec->shft` on `epicsInt32` — also UB-ish but practically same. Benign.

---

## Summary of counts

- Critical: 3  (C-1 fanout LNK0 missing; C-2 Specified index base/OFFS; C-3 Mask SHFT ignored)
- High: 9  (H-1..H-9)
- Medium: 9  (M-1..M-9)
- Low: 6  (L-1..L-6)
