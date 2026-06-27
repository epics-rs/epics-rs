# motor-rs Codex C-Parity Audit — 2026-06-28 (Round 1)

Method: Codex-style line-by-line C-parity audit (`/parity-audit` playbook).
Four read-only auditor panels (caucus opus), one per `motorRecord.cc`
call-graph cluster, each grounding every finding in BOTH the Rust `file:line`
and the C `file:line`.

- **Port:** `crates/motor-rs/src/` (+ `crates/asyn-rs/src/interfaces/motor.rs`)
- **Reference:** `/Users/stevek/codes/epics-modules/motor/motorApp/MotorSrc/`
  at `R7-4-5-g78b474cd` (PR #250 merge, 2026-04-06), `motorRecord.cc` VERSION 7.4
- **Baseline (not re-audited):** the 2026-05 changelog gap-analysis
  `crates/motor-rs/doc/parity-review.md` (kept) + the 30 recent line-parity fix
  commits `1440f1d0`..`d25bb7f9` (verified for completeness, not re-filed).

This doc is the Codex inventory. The older `parity-review.md` was a PR/issue
changelog mine (Sprint/§N numbering, no R-N); this is the first call-graph
semantics sweep, so findings number from **R1**. Panel R-ranges were carved
non-colliding (A: R1–R20, B: R21–R40, C: R41–R58, D: R59–R80), so the
panel-assigned numbers are preserved here for traceability to the audit files
rather than re-sequenced.

---

## Severity summary

| Sev | IDs | Count |
|---|---|---|
| **DEFECT** | R1, R23, R43, R59 | 4 |
| **CONCERN** | R2, R3, R5, R21, R22, R24, R25, R41, R42, R44, R45, R60, R61 | 13 |
| **NIT** | R6, R26, R27, R46, R62 | 5 |
| withdrawn | R4 (verified false positive) | — |
| merged | R63 → R45 (same EA_SLIP gap, found by two panels) | — |

22 distinct active findings.

---

## Review Log

### Round 1 — 2026-06-28 — Codex fan-out (4 panels, opus)
Four call-graph clusters audited in parallel:
- **A — Process & motion engine** (`process`/`do_work`/`postProcess`/`maybeRetry`):
  1 DEFECT, 3 CONCERN, 1 NIT; R4 withdrawn.
- **B — Field access / special / coordinate / limits**: 1 DEFECT, 4 CONCERN, 2 NIT.
- **C — Status / readback / monitor / MSTA-MIP / alarm**: 1 DEFECT, 4 CONCERN, 1 NIT.
- **D — Device support / asyn base / driver / profile**: 1 DEFECT, 2 CONCERN, 2 NIT.

**Thematic clusters (each points at a structural seam, not an isolated line):**

1. **Init-time seeding diverges from C across three sites** — R41 (URIP read one
   pass early), R42 (DMOV from driver vs C's unconditional `dmov=TRUE`), R59 (driver
   reseeded ignoring RSTM/#231/#196). The root is that motor-rs's init order
   (`device_support.init()` reseeds the driver → first `poll()` → record `Startup`
   → `initial_readback`) inverts C's order (`process_motor_info(initcall=true)` →
   `init_controller` gated by RSTM, all *before* the first poll). `initial_readback`
   owns the correct RSTM/loadpos/#196 gates but runs too late to gate the driver.
   **R59 is the DEFECT; R41/R42 are the same init-order seam.**

2. **MSTA bit ownership** — R43 (RA_HOMED latched sticky-on, C never manages it) and
   R45/R63 (EA_SLIP bit 4 unrepresentable). C copies the driver status word wholesale
   (`devMotorAsyn.c:467`); the record manages only the CNTRL_COMM_ERR clear. Any
   record-level MSTA bit latch/synthesis is a divergence.

3. **Field-type / metadata fidelity** — R23 (NTMF DBF_USHORT served as Double, the one
   DEFECT in this cluster), R27 (menu fields as Short), R26 (units truncation). The
   `FieldDesc.dbf_type` table is the single owner; NTMF is the only *behaviorally*
   wrong one.

4. **ACCU/ACCL/ACCS cascade** — R21 (slave recompute skipped when VELO≤VBAS) and R22
   (ACCL/ACCS put auto-switches ACCU + seeds ACCS=1.0, which the cited C commit
   `36177f7b` does NOT do in the reference checkout). R22 needs a decision: track a
   newer upstream or a behavior bug — verify before fixing.

5. **Intentional / documented deviations for sign-off** — R5 (SPMG=Move STOP_AXIS
   suppression — Rust cleaner, diverges from C wire), R6 (accel floored positive vs C
   omitting SET_ACCEL at ≤0). Both in-code documented; surface, likely keep.

6. **Driver-boundary negative space** — R25 (MRES<0 soft-limit register swap deferred to
   an unverified asyn bridge), R60 (encoder ratio never forwarded), R61 (idle poll stops
   → MIP_EXTERNAL dead while idle). These hinge on the deliberate motor-rs dial-EGU
   driver boundary (vs C raw-steps); judge the command/status *contract*, not the frame.

---

## Open Findings

### Category A — Process & motion engine

#### R1 — Positional move stopped by a limit switch never syncs VAL/DVAL/RVAL ← readback
- **Severity:** DEFECT — **CLEARED** (commit `dbfff1dd`): `ls_blocks_retry` branch of
  `evaluate_position_error` now calls `postprocess_sync()`. Scoped to the LS-stop alone
  (a plain `MOVE_ABS` never sets C `pp`, so close-enough/rtry-disabled stay un-synced,
  matching C). MIP_STOP Pause-at-limit split out as R64.
- **Rust:** `record/state_machine.rs:660-671` — `evaluate_position_error` ls-blocked
  branch calls `finalize_motion` (`:437-476`), which sets `mip=empty`/`dmov=true` and
  `lval/ldvl/lrvl = current val/dval/rval` (`:461-463`) but never calls
  `postprocess_sync()`. `check_completion` has no limit-switch sync branch for
  MainMove/Retry/BacklashFinal (contrast the `Jog` arm `:290-308` which *does* sync).
- **C:** `motorRecord.cc:1367-1380` "Do another update after LS error" → on the GET_INFO
  callback `if(pp)` at `:1382` runs `postProcess` (`:827-849`), which sets `val=rbv`,
  `dval=drbv`, `rval=NINT(dval/mres)`, `diff=0`, `rdif=0` with MARKs — drive triplet
  reset to the stopped (limit) position.
- **Divergence:** C resets VAL/DVAL/RVAL to readback after a positional move halts on a
  hardware limit in the travel direction; Rust leaves them at the unreached target.
  The whole `1367-1380` LS-error→postProcess path is absent for positional phases.
- **Impact:** After driving into a limit, clients read VAL=target while RBV=limit;
  DIFF/RDIF stay non-zero. `finalize_motion` set `ldvl=dval=target`, so a later process
  will not re-derive from readback even after the limit clears. Persistent, wire-visible.

#### R2 — DIFF/RDIF not posted on the put-initiated move-start notify
- **Severity:** CONCERN (one-poll latency, self-correcting)
- **Rust:** `record/mod.rs:268-301` — the first `DMOV 1→0` notify lists a fixed field set
  (DMOV/MOVN/VAL/DVAL/RVAL/RBV/DRBV/MIP/RCNT) with **no DIFF/RDIF**; `plan_absolute_move`
  (`command_planner.rs:554-565`) recomputes diff/rdif but does not set `diff_rdif_marked`,
  so `force_posted_fields` omits them on this pass.
- **C:** `motorRecord.cc:2248-2256` — the `do_work` move block sets `diff=dval-drbv` +
  `MARK(M_DIFF)` and `rdif=NINT(diff/mres)` + `MARK(M_RDIF)`; `monitor()` posts both on
  the move-start pass.
- **Divergence:** C posts the new (full-distance) DIFF/RDIF immediately on move-start;
  Rust posts them one device-poll later.
- **Impact:** Clients monitoring DIFF/RDIF see move-start following-error one poll late.
  No end-state divergence.

#### R3 — Held jog re-activates one poll late after a positional move gives up
- **Severity:** CONCERN (timing; jog not lost)
- **Rust:** `record/state_machine.rs:623-631` (give-up) and `:660-671` (close-enough/rtry==0)
  call `finalize_motion`, which clears `mip` wholesale and ignores a held jog button.
  Re-activation happens on a later idle poll via `dispatch_latent_buttons`
  (`command_planner.rs:1076-1110`).
- **C:** `maybeRetry` `motorRecord.cc:1063-1065` re-arms `mip |= MIP_JOG_REQ` in the give-up
  branch (and `:1055`/`:1088` preserve it); because `maybeRetry` is called from `process`
  (`:1431`) and the same pass falls into `do_work`, the jog re-fires in the *same* pass.
- **Divergence:** C re-activates a held jog in the completion pass; Rust defers to the next
  poll. If polling quiesces after give-up, restart could be later than ~one poll.
- **Impact:** Narrow (jog held across a positional give-up/close-enough). Jog not dropped.

#### R4 — WITHDRAWN (verified false positive)
RMOD_I dispatch suppression: C `motorRecord.cc:2384-2385` (`else if(rmod==motorRMOD_I)
return(OK)`) is structurally unnecessary in Rust — RMOD_I is handled earlier in
`evaluate_position_error:633-647` (re-arm settle watchdog, `mip=RETRY|DELAY_REQ`) and never
routes into `plan_absolute_move`. No defect.

#### R5 — Rust suppresses C's redundant STOP_AXIS + transient MIP=STOP after an SPMG=Move completion
- **Severity:** CONCERN (intentional, in-code-documented — for sign-off)
- **Rust:** `record/state_machine.rs:486-494` — `restore_spmg_move_to_pause` sets
  `spmg=Pause` AND `lspg=Pause`, so no top-block stop fires and no STOP_AXIS/transient-MIP
  is emitted.
- **C:** `maybeRetry` `:1097-1101` sets `spmg=Pause` leaving `lspg=Move`; the re-entered
  `do_work` top block (`:1902-1911`) sends `STOP_AXIS` and posts a transient `mip=MIP_STOP`.
- **Divergence:** C emits an extra STOP_AXIS + transient MIP=MIP_STOP under SPMG=Move; Rust
  deliberately suppresses both.
- **Impact:** Wire-traffic / transient-MIP only. Rust is arguably cleaner; surfaced for
  sign-off, not a bug. **Likely keep (don't copy C's redundant stop).**

#### R6 — Move/backlash/home acceleration floored positive; C omits SET_ACCEL at ≤0
- **Severity:** NIT (degenerate-config-only, documented)
- **Rust:** `command_planner.rs:1554-1574`/`1578-1595`/`1602-1619` — each falls back to a
  strictly positive rate and always carries an `acceleration`.
- **C:** `motorRecord.cc:2529` — `if(accel>0.0) WRITE_MSG(SET_ACCEL,&accel)`; a computed
  accel ≤0 sends no SET_ACCEL (driver keeps its prior/default).
- **Divergence:** at computed accel ≤0 (VELO=0, or ACCU=Accs with ACCS≤0) C sends nothing,
  Rust sends a forced positive rate.
- **Impact:** Only a degenerate/misconfigured axis. Documented intentional deviation.

### Category B — Field access / special / coordinate / limits

#### R21 — ACCU cascade skips the slave-accel recompute when VELO ≤ VBAS — CLEARED (`f23f51f1`)
- **Severity:** CONCERN
- **Rust:** `record/field_access.rs:2144-2148` (`apply_accu_cascade`):
  `span = velo - effective_vbas; if span <= 0.0 { return; }` — leaves ACCL/ACCS untouched.
  Same skip inlined at `:1457`/`:1471` (`&& span > 0.0`).
- **C:** `motorRecord.cc:503/515/529/539` — all three accel helpers use
  `(velo>vbas)?(velo-vbas)/X : velo/X` (commit `b201e40e`); when `velo<=vbas` the numerator
  is the full `velo` and the slave field is still recomputed/posted.
- **Divergence:** at `velo==vbas` (reachable — a VELO put ≤ VBAS is range_check'd up to VBAS
  then the cascade runs with span==0) C sets `accs=vbas/accl` (or `accl=vbas/accs`); Rust
  keeps the stale value.
- **Impact:** stale ACCU slave-field readback (the master field and the accel sent to the
  driver are unaffected) — wrong slave-field value, not wrong motion.

#### R22 — ACCL/ACCS puts auto-switch ACCU and seed ACCS=1.0 — neither is in the reference C
- **Severity:** CONCERN (needs decision — verify upstream before fixing)
- **Rust:** `field_access.rs:1455` (ACCL→`accu=Accl`) and `:1467-1469`
  (ACCS→`accs = if v<=0 {1.0} else {v}; accu=Accs`).
- **C:** `motorRecord.cc:2735-2742`/`2745-2752` — neither case assigns `pmr->accu`
  (`rg 'pmr->accu *='` → only the two read sites 489/525). C ACCS with `accs<=0` runs
  `updateACCSfromACCL` → `accs=velo/accl`, not literal `1.0`. The cited commit `36177f7b`
  is present but does NOT implement the auto-switch the Rust comment claims.
- **Divergence:** (a) writing ACCL/ACCS flips ACCU in Rust but not in C; (b) non-positive
  ACCS becomes `1.0` (Rust) vs `velo/accl` (C).
- **Impact:** which of {ACCL,ACCS} is the persistent master differs → different accel
  carried forward. **Decide:** is Rust tracking a post-R7-4-5 upstream, or is this a bug?
  Resolve against the actual C before any edit.

#### R23 — NTMF served as DBF_DOUBLE — the C field is DBF_USHORT — **CLEARED** (`9cb719b2`)
- **Severity:** DEFECT
- **Rust:** `field_access.rs:531-534` `FieldDesc{name:"NTMF", dbf_type:Double}`; read `:789`
  returns `EpicsValue::Double`; put `:1985-1990` clamps as float; stored `f64`
  (`fields.rs:381`).
- **C:** `motorRecord.dbd:789` `field(NTMF,DBF_USHORT){initial("2")}`; special
  `motorRecord.cc:3093-3100` integer-compares `ntmf < 2`.
- **Divergence:** native CA type DBR_DOUBLE vs DBR_USHORT; a put of `2.5` is stored verbatim
  in Rust, truncated to `2` in C. Cascades: `precision_for` (`:2547-2553`) returns PREC vs
  C `recGblGetPrec` forcing 0 for USHORT; `limits_for` (`:2593`) returns `(1e300,-1e300)` vs
  C `(65535,0)`.
- **Impact:** NTMF wire type, value quantization, display precision, and graphic limits all
  wrong for any client introspecting it. Fix: `DbFieldType::UShort`. (Spot-check: NTMF is the
  ONLY DBF_USHORT field; SREV=Long✓, RTRY=Short✓.)

#### R24 — STUP-after-write (≠ON) and NTMF (<2) return Ok where C returns ERROR to block processing — **CLEARED** (`d68cdfde`)
- **Severity:** CONCERN
- **Rust:** `field_access.rs:1866-1870` (STUP `!=1` → set 0, `Ok(())`); `:1985-1990`
  (NTMF `<2` → set 2, `Ok(())`).
- **C:** `motorRecord.cc:3084-3091` (STUP) / `:3093-3100` (NTMF) both `post; return(ERROR);
  /* Prevent record processing. */`. A non-OK `special()` return suppresses the PP-triggered
  process.
- **Divergence:** clamp value matches; C aborts processing on the clamp, Rust completes the
  put and lets PP drive a process pass. (The STUP/HOMF before-write veto and STUP in-progress
  veto are correctly `Err` — only these two after-write clamps miss it.)
- **Impact:** `caput STUP 2`/`STUP 0`/`NTMF 1` triggers a spurious process in Rust that C
  suppresses; largely a no-op extra cycle, not wrong state.

#### R25 — User/dial soft-limit forward picks the command side by DIR only — C also folds sign(MRES)
- **Severity:** CONCERN (negative space; MRES<0 driver path untested)
- **Rust:** `field_access.rs:2438-2446` (`queue_limit_forward`) emits `SetHighLimit/SetLowLimit`
  carrying dial-EGU, side chosen by DIR only (`:1635-1643`/`1655-1663`/`1673`/`1692`). The raw
  register sign-swap lives only in the raw pair (`:2465-2489`).
- **C:** `set_user_highlimit` `motorRecord.cc:4098-4107` selects via `dir_positive ^ (mres<0)`
  and sends raw-steps `tmp_limit/mres`; `set_dial_highlimit` `:4246-4250` selects via `mres<0`.
- **Divergence:** command side matches C only for MRES>0; for MRES<0 the sides are opposite.
  Rust assumes the driver re-derives the raw register (incl. the swap) from dial; the in-repo
  consumer `device_support.rs:184-190` is a logging stub; tests use only MRES=+0.01.
- **Impact:** if the asyn bridge maps `SetHighLimit`→`SET_HIGH_LIMIT(dial/mres)` without the
  swap, a negative-MRES axis programs inverted travel limits. Record-internal dial/user/raw
  math is correct under MRES<0 (negative confirmation). Needs a bridge-side check.

#### R26 — get_units omits the C `dbr_units_size` truncation
- **Severity:** NIT
- **Rust:** `field_access.rs:2514-2526` (`units_for`) builds `EGU+"/sec"` uncapped.
- **C:** `motorRecord.cc:3159/3205/3206` truncates to `dbr_units_size-1` (7 chars + NUL).
- **Divergence:** EGU≥4 chars overflows: `"mrad"` → C `"mrad/se"` vs Rust `"mrad/sec"`.
- **Impact:** CA masks it (fixed 8-byte field); a PVA variable-length units reader sees the
  extra byte. C truncation is arguably an artifact; recorded for completeness.

#### R27 — Menu fields modeled as DBF_SHORT — graphic/precision metadata differ from DBF_MENU
- **Severity:** NIT (systemic baseline modeling choice)
- **Rust:** DIR/FOFF/SET/SPMG/HLSV/RMOD/UEIP/URIP/RSTM/CNEN/STUP/NTM are `DbFieldType::Short`
  in `FIELDS` (`field_access.rs:80-528`).
- **C:** all `DBF_MENU`; `recGblGetPrec`/`getMaxRangeValues` have no DBF_MENU case → precision
  stays `pmr->prec`, graphic/control limits left `0/0`.
- **Divergence:** Rust returns precision 0 and graphic limits `(32767,-32768)`; C returns
  `pmr->prec` and `(0,0)`. With default PREC=0 the precision case collapses; the graphic-limit
  case is meaningless for an enum field served as DBR_ENUM.
- **Impact:** negligible (graphic limits unused in the enum wire path). Flagged once for the
  family; enum-vs-short modeling is a baseline decision, out of this category's remit.

### Category C — Status / readback / monitor / MSTA-MIP / alarm

#### R41 — URIP RDBL readback scaling runs during the initial readback (C suppresses it via `initcall`)
- **Severity:** CONCERN
- **Rust:** `record/status_update.rs:155` — URIP path gates on `self.initialized`, but
  `determine_event` flips `self.initialized=true` *before* returning `Startup` (`:59-61`),
  which dispatches to `initial_readback`→`process_motor_info`. So at the init readback
  `self.initialized` is already true and the URIP branch executes (`rdbl_value` already `Some`).
- **C:** `motorRecord.cc:3682` — `else if(urip==Yes && initcall==false)`; `init_record` calls
  `process_motor_info(pmr,true)` (`:698`), so `initcall==true` skips URIP; init falls to the
  `rmp` default and seeds `val=rbv`/`dval=drbv` from motor position.
- **Divergence:** `self.initialized` is not a faithful port of C's `initcall` — it is true for
  the very pass C marks as the init call. Init RBV/DRBV come from the RDBL link in Rust vs the
  controller position in C.
- **Impact:** a URIP motor whose RDBL source differs from controller position gets a different
  startup VAL/DVAL than C. Part of the init-order seam (see R42/R59).

#### R42 — Initial DMOV derived from driver status instead of C's unconditional `dmov=TRUE`
- **Severity:** CONCERN
- **Rust:** `status_update.rs:447` — `initial_readback` ends `self.stat.dmov = status.done &&
  !status.moving` (then `request_poll` if moving).
- **C:** `motorRecord.cc:721` — `init_record` sets `pmr->dmov=TRUE; MARK(M_DMOV)`
  unconditionally after `process_motor_info`.
- **Divergence:** an axis moving at IOC start seeds DMOV=false in Rust, TRUE in C (corrected on
  first CALLBACK_DATA poll, `:1316-1317`).
- **Impact:** init-time DMOV differs; FLNK fires only on DMOV=true, so C fires the forward link
  once at init for a moving axis and Rust does not. Transient; init monitor/FLNK not C-faithful.

#### R43 — MSTA RA_HOMED (bit 14) is latched at the record level; C copies the driver word wholesale — **CLEARED** (`65df336b`)
- **Severity:** DEFECT
- **Rust:** `status_update.rs:284-286` — building MSTA each poll: `if msta.contains(HOMED) ||
  status.homed { msta |= HOMED }` — OR-ed from the *previous* MSTA, so once set it is never
  cleared (every other bit is pure `status`-derived). Comment claims a "record-managed bit."
- **C:** `devMotorAsyn.c:467` `pmr->msta = pPvt->status.status` copies the full driver word;
  `motorRecord.cc` writes RA_HOMED nowhere (`rg RA_HOMED|HOMED` → no matches). The only
  record-side MSTA mutation is `alarm_sub` clearing CNTRL_COMM_ERR.
- **Divergence:** "record-managed" is false for RA_HOMED — C never manages it. The Rust latch
  makes bit 14 sticky-on regardless of the driver.
- **Impact:** a driver de-asserting homed (re-home, controller reset, SetPosition redefine) →
  C clears bit 14, Rust reports a permanently-homed axis on `camonitor MSTA`. Fix: drop the
  `|| msta.contains(HOMED)` term (rely on `status.homed`).

#### R44 — `udf` re-derived from `VAL.is_nan()` every cycle; C manages motor UDF only at init + DOL block
- **Severity:** CONCERN
- **Rust:** `epics-base-rs/.../processing.rs:2237-2239` — for any record whose `clears_udf()` is
  true, `common.udf = value_is_undefined()` every Complete cycle. Motor overrides neither
  `clears_udf` (default true) nor `value_is_undefined` (default `VAL.is_nan()`). Motor UDF logic
  in `check_alarms` (`mod.rs:547-549`) overrides only when `dol_udf` is `Some` (set only on
  closed-loop DOL-read passes, `mod.rs:388-390`).
- **C:** `motorRecord.cc:679` sets `udf=FALSE` once at init; `:2002`/`:2005` set it in the
  closed-loop DOL read; otherwise sticky.
- **Divergence:** Rust recomputes UDF from VAL every non-DOL pass; C never recomputes from VAL.
  Agree in the open-loop/finite-VAL case; diverge for a closed-loop motor where a DOL read fails
  on a `dmov` pass (udf TRUE, C latches) then a CALLBACK_DATA pass mid-move (`dol_udf=None`)
  resets udf to false in Rust — clearing the UDF alarm C holds until DOL recovers.
- **Impact:** transient loss of a closed-loop DOL-failure UDF alarm. Structural fix: motor
  override `clears_udf()`→false (UDF owned solely by init + the `dol_udf` hook).

#### R45 — MSTA EA_SLIP (bit 4) can never be set — no backing driver field
- **Severity:** CONCERN (independently confirmed by Category D as R63)
- **Rust:** `status_update.rs:234-279` builds MSTA from `MotorStatus`; bit 4 (`MstaFlags::SLIP
  = 0x0010`, `flags.rs:37`) is never assigned. `asyn-rs` `MotorStatus`
  (`crates/asyn-rs/src/interfaces/motor.rs:9-52`) exposes `slip_stall` (→ bit 6) but has no
  encoder-slip field for bit 4. (`asynMotorController.cpp:89` creates `motorStatusSlip_` 4th →
  it occupies bit 4.)
- **C:** `motor.h:176/186` EA_SLIP bit 4 ("encoder slip enabled"); `devMotorAsyn.c:467` copies
  the full driver word incl. bit 4.
- **Divergence:** the MSTA wire value cannot represent EA_SLIP; a driver reporting it loses bit 4.
- **Impact:** purely informational (no record logic consumes EA_SLIP — alarm uses bit 6
  EA_SLIP_STALL); only `camonitor MSTA` diverges by one bit. Fix requires a new `MotorStatus`
  field (cross-crate, asyn-rs).

#### R46 — CNEN refreshed from EA_POSITION on every poll, not only on an MSTA-post cycle — **CLEARED** (`5123734a`)
- **Severity:** NIT
- **Rust:** `status_update.rs:293-295` — `if msta.contains(GAIN_SUPPORT) { cnen =
  msta.contains(POSITION) }` runs every `process_motor_info`.
- **C:** `motorRecord.cc:3541-3549` — the CNEN←EA_POSITION readback lives inside the
  `MARKED(M_MSTA)` branch of `monitor()` (only when MSTA is posted and `pos_maint != cnen`).
- **Divergence:** Rust updates CNEN every poll; C only on MSTA-post cycles.
- **Impact:** steady-state identical; a user-written CNEN the driver hasn't reflected in
  EA_POSITION is reverted one poll sooner in Rust. Posting still change-gated. Cosmetic timing.

### Category D — Device support / asyn base / driver / profile

> Boundary note (governs this category): C `asynMotorAxis::move/moveVelocity/home/setPosition`
> take **raw steps**; motor-rs deliberately moves the driver boundary to **dial-EGU**
> (`device_support.rs:275-277`, `asyn-rs/motor.rs:93-95`). Findings judge SEMANTIC equivalence
> of the command/status contract, not the raw-vs-EGU frame.

#### R59 — `device_support.init()` reseeds the controller position ignoring RSTM, loadpos_blocked (#231), and the #196 MRES guard
- **Severity:** DEFECT
- **Rust:** `device_support.rs:262-278` — at device init, if `was_position_restored()`
  (any pass0 VAL/DVAL/RVAL/RLV write, `mod.rs:158-165`) it calls `motor.set_position(&user,
  dval)` unconditionally — no RSTM/loadpos/MRES check — *before* the first `poll()` (`:284`).
- **C:** `devMotorAsyn.c:166-239` `init_controller` reseeds only when `initPos==1`, decided by
  the `pmr->rstm` switch (`:199-216`) testing the controller's **actual current position**
  (fetched via `readGenericPointer` *before* `init_controller`). RSTM=Never→never reseed;
  NearZero→reseed only if the controller currently sits near zero.
- **Divergence:** motor-rs's RSTM/loadpos/#196 logic lives only in `initial_readback`
  (`status_update.rs:366-455`), which fires on the record `Startup` event — *after* `init()`
  already reseeded the driver and polled it. By then `motor_dial` equals the reseeded DVAL so
  `dval_non_zero_pos_near_zero` is always false; `should_restore` blocks the *record* command
  but the driver was already reseeded.
- **Impact:** a controller that kept its true absolute position across restart (default
  `RSTM=NearZero`, or `RSTM=Never`, or an absolute encoder with `LOADPOS_BLOCK`) has it
  overwritten by the stale autosaved DVAL every boot — the exact failure RSTM/#231 prevent.
  Structural fix: delete the `init()` reseed; let `initial_readback` own it (it already routes
  `SetPosition` through the normal command path).

#### R60 — Encoder ratio (SET_ENC_RATIO / motorEncoderRatio_) is never forwarded to the driver
- **Severity:** CONCERN (negative space; partly mitigated by the dial-EGU boundary)
- **Rust:** `asyn-rs/src/interfaces/motor.rs:96-271` — `AsynMotor` has no `set_encoder_ratio`;
  `flags.rs:250-341` `MotorCommand` has no `EncoderRatio` variant; `init()` never forwards one.
  `rg "enc.?ratio|EncoderRatio"` → nothing in the driver path.
- **C:** `devMotorAsyn.c:188-197` writes `eratio=mres/eres` to `motorEncRatio` unconditionally at
  init (.04 fix "set position failed for Asyn drivers"); `motorRecord.cc:1960-1980` re-sends
  `SET_ENC_RATIO` on a resolution-change special() when `EA_PRESENT`; `devMotorAsyn.c:570-573`
  → `dvalue=mres/eres`; `asynMotorController.cpp:409-411` → `setEncoderRatio`.
- **Divergence:** the entire encoder-ratio command (init + resolution-change) is absent from the
  Rust driver boundary.
- **Impact:** a real driver using `mres/eres` for encoder-scaled closed-loop never receives it.
  The dial-EGU boundary makes the record do the scaling, so the ratio is arguably the record's
  job now — but the C contract is dropped with no replacement/hook; a model-1/2 driver expecting
  SET_ENC_RATIO cannot be ported faithfully.

#### R61 — Idle polling stops entirely — C's always-on idlePollPeriod poller never stops
- **Severity:** CONCERN
- **Rust:** `status_update.rs:86-90` emits `PollDirective::Stop` once `commands.is_empty() &&
  schedule_delay.is_none() && dmov`; `device_support.rs:218-221` sends `StopPolling`;
  `poll_loop.rs:156-174` idle branch is `cmd_rx.recv().await` only — no periodic poll
  (`idle_poll_interval` consulted only while `active`).
- **C:** `asynMotorController.cpp:615-696` `asynMotorPoller` is `while(1)` polling every
  `idlePollPeriod_` when `!anyMoving` and never stops (only `shuttingDown_` breaks).
- **Divergence:** motor-rs goes dark on the wire while idle; C keeps polling at idlePollPeriod.
- **Impact:** external moves, manual/hand moves, limit-switch transitions, power changes while
  idle are not detected — RBV/RMP/REP/MSTA stale until a local command restarts the loop. The
  MIP_EXTERNAL detector (`process_motor_info:211-218`, `#ea063f5f`) can never fire while idle —
  dead in exactly the state it exists for. May be an intentional efficiency choice; documented
  C-semantics divergence (parity-review.md already flags MIP_EXTERNAL as a regression risk).

#### R62 — RMP/REP rounding mode — `round()` (half away from zero) vs C `floor(x+0.5)` (half toward +∞) — **CLEARED** (`7259f9ed`)
- **Severity:** NIT
- **Rust:** `status_update.rs:127` `rmp=(position/mres).round()`; `:144` `rep=(enc/eres).round()`
  — `f64::round` is half away from zero.
- **C:** `devMotorAsyn.c:452/459` `(epicsInt32)floor(status.position+0.5)` — half toward +∞.
- **Divergence:** an exact half-step negative raw value differs by one (raw −2.5 → C −2, Rust −3).
- **Impact:** 1-raw-step RMP/REP discrepancy only at exact .5 boundaries; negligible.

#### R63 — MERGED into R45 (MSTA EA_SLIP bit 4 unrepresentable). Same root, found independently.

#### R64 — MIP_STOP Pause that coasts onto a limit switch does not sync VAL/DVAL/RVAL ← readback
- **Severity:** CONCERN (candidate; reachability + design-intent to verify)
- **Rust:** `record/state_machine.rs:246-265` — the MIP_STOP completion handler's `pp == false`
  (Pause) branch runs an inline `maybeRetry`; its `ls_blocks` else-arm (`:256-264`) clears MISS
  and restores SPMG Move→Pause but never calls `postprocess_sync()`. The comment (`:208-211`)
  reasons "a Pause never set pp → postProcess skipped → LS just blocks the retry."
- **C:** `motorRecord.cc:1366-1380` fires on *any* motor-stopped callback with `mip != MIP_DONE`
  and a struck limit in direction — independent of `pp` — forcing `pp=TRUE` + GET_INFO and
  `mip=MIP_DONE` (`:1377`). The next callback's `postProcess` (`:826-849`) then syncs val=rbv.
  So a paused axis that ends ON a limit in the commanded direction is a *terminal* LS-completion
  in C (synced, DONE), not a resumable pause.
- **Divergence:** Rust treats "paused at a limit in direction" as a resumable-pause with blocked
  retry (target preserved, no sync); C treats it as a terminal LS-stop (synced, mip=DONE).
- **Impact:** Edge — requires a Pause-stop to complete exactly on a limit switch in the commanded
  direction. If reachable, VAL stays at the unreached target after the limit, and Go would
  attempt to resume toward it. Distinct trigger from R1 (commanded Pause vs positional
  auto-completion); fixing it changes Pause-at-limit from resumable to terminal, so it needs the
  review round to confirm reachability before any edit. Found during the R1 defect-family sweep.

---

## Fix-phase classification

- **Mechanical structural fix (clear C parity):** R1, R23, R43, R59, R44 (clears_udf override),
  R21 (full-velo numerator), R41/R42 (init-order seam), R24, R46, R62.
- **Needs a decision before edit:** R22 (verify upstream — bug vs newer-upstream tracking),
  R5 / R6 (documented intentional deviations — likely keep, surface for sign-off).
- **Cross-crate / driver-boundary (asyn-rs):** R25 (MRES<0 limit-register swap in the bridge),
  R45/R63 (MotorStatus EA_SLIP field), R60 (encoder-ratio command + hook), R61 (idle-poll policy).
