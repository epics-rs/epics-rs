# motor-rs Codex C-Parity Audit — 2026-06-28 (Round 1)

Method: Codex-style line-by-line C-parity audit (`/parity-audit` playbook).
Four read-only auditor panels (caucus opus), one per `motorRecord.cc`
call-graph cluster, each grounding every finding in BOTH the Rust `file:line`
and the C `file:line`.

- **Port:** `crates/motor-rs/src/` (+ `crates/asyn-rs/src/interfaces/motor.rs`)
- **Reference:** `/Users/stevek/codes/epics-modules/motor/motorApp/MotorSrc/`
  at `R7-4-5-g78b474cd` (PR #250 merge, 2026-04-06), `motorRecord.cc` VERSION 7.4
- **Baseline (not re-audited):** the 2026-05 changelog gap-analysis
  `crates/motor-rs/crates/motor-rs/doc/parity-review.md` (kept) + the 30 recent line-parity fix
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

**Cleared so far (15):** R1, R23, R43, R59 (all DEFECTs); R2, R3, R21, R22, R24,
R41, R44, R61 (CONCERNs); R46, R62 (NITs). **Dispositioned, no code change (7):**
R5 (KEEP — don't copy C's redundant SPMG=Move stop), R6 (KEEP — don't copy C's
accel≤0 artifact), R26 (keep Rust — C truncation is a CA-DBR artifact), R27
(defer — systemic DBF_MENU baseline), R25 (KEEP — user/dial forward folds DIR
only; the sign(MRES) limit-register swap is a dial→raw flip the DRIVER owns at
its register boundary, and no in-repo driver mis-programs it), R45 (KEEP —
MSTA EA_SLIP bit 4 is an inert informational bit with no in-repo consumer;
adding a MotorStatus field with no producer would be fake parity), R60 (defer —
SET_ENC_RATIO is a real controller-config command but no in-repo controller
consumes it; hook spec recorded in the finding for when a driver needs it).
**Rejected (1):** R42 (empirically falsified — the MIP_EXTERNAL detector fires
during init via the default `dmov=true`, so the boot-mid-motion axis closes the
loop; no defect). **Remaining open (0): converged.** R65 (idle DIFF/RDIF
over-post — surfaced by the 01KW5QV confirming round on R61) is CLEARED in
`8568f173`, validated by the 01KW5SS round (3 opus panels: change-gate
C-faithful, C strands the held jog, preserve-the-resume the decisive call).
The original 22 audit findings are all cleared, dispositioned, or rejected;
R65 was downstream of the R61 always-on poller.

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

#### R3 — Held jog re-activates one poll late after a positional move gives up — CLEARED (`555b1bad`)
- **Severity:** CONCERN (timing; jog not lost)
- **Resolution:** Both maybeRetry give-up analogs now re-fire same-pass via
  `dispatch_latent_collection` after `finalize_motion`: `evaluate_position_error`
  give-up (the normal positional completion) and the Pause/NTM-stop give-up
  (`state_machine.rs:226`). The `can_accept_command` gate inside
  `dispatch_latent_collection` keeps a Pause-reached give-up parked until Go
  (matching the retry branch) while a Go give-up re-fires now. The
  close-enough/rtry==0 branches were deliberately NOT changed: C does
  `mip &= MIP_JOG_REQ` there (`:1055`/`:1088`), which during a positional move is 0
  (special() arms MIP_JOG_REQ only at `mip==MIP_DONE`, 3042-3053), so C drops the jog
  that pass and the next idle poll (Idle-arm `dispatch_latent_collection`) resumes it —
  already matched. 5 tests: give-up+JOGF/HOMF held re-fire same-pass; give-up no-button
  finalizes quiet (no command, no implicit GET_INFO); close-enough defers to the next
  idle poll; Move one-shot close-enough waits for SPMG=Go.
- **C:** `maybeRetry` `motorRecord.cc:1063-1065` re-arms `mip |= MIP_JOG_REQ` in the give-up
  branch; because `maybeRetry` is called from `process` (`:1431`) and the same pass falls
  into `do_work` with dmov still TRUE (`:1489`), the jog re-fires in the *same* pass.

#### R4 — WITHDRAWN (verified false positive)
RMOD_I dispatch suppression: C `motorRecord.cc:2384-2385` (`else if(rmod==motorRMOD_I)
return(OK)`) is structurally unnecessary in Rust — RMOD_I is handled earlier in
`evaluate_position_error:633-647` (re-arm settle watchdog, `mip=RETRY|DELAY_REQ`) and never
routes into `plan_absolute_move`. No defect.

#### R5 — Rust suppresses C's redundant STOP_AXIS + transient MIP=STOP after an SPMG=Move completion — DISPOSITION: KEEP
- **Severity:** CONCERN (intentional, in-code-documented — signed off, keep)
- **Disposition:** KEEP (don't copy C's redundant stop). C's extra STOP_AXIS + transient
  MIP=MIP_STOP under SPMG=Move is wire/transient noise only; Rust's `lspg=Pause` co-set
  cleanly suppresses it. Per the "don't copy C's bugs" steer, the Rust behavior is
  preferred. Strict-wire-parity would require re-emitting the redundant stop — not done.
- **Rust:** `record/state_machine.rs:486-494` — `restore_spmg_move_to_pause` sets
  `spmg=Pause` AND `lspg=Pause`, so no top-block stop fires and no STOP_AXIS/transient-MIP
  is emitted.
- **C:** `maybeRetry` `:1097-1101` sets `spmg=Pause` leaving `lspg=Move`; the re-entered
  `do_work` top block (`:1902-1911`) sends `STOP_AXIS` and posts a transient `mip=MIP_STOP`.
- **Divergence:** C emits an extra STOP_AXIS + transient MIP=MIP_STOP under SPMG=Move; Rust
  deliberately suppresses both.
- **Impact:** Wire-traffic / transient-MIP only. Rust is arguably cleaner; surfaced for
  sign-off, not a bug. **Likely keep (don't copy C's redundant stop).**

#### R6 — Move/backlash/home acceleration floored positive; C omits SET_ACCEL at ≤0 — DISPOSITION: KEEP
- **Severity:** NIT (degenerate-config-only, documented — signed off, keep)
- **Disposition:** KEEP. The floor is only reachable on a degenerate/misconfigured axis
  (VELO=0, or ACCU=Accs with ACCS≤0); the Rust positive-rate fallback avoids C's
  `accel≤0` artifact (C `if(accel>0.0) WRITE_MSG(SET_ACCEL)` leaves the driver at a stale
  rate, and an `ACCL==0` path can compute `+inf`). Strict-parity option (make
  `MotorCommand.acceleration: Option<f64>` and skip the asyn-rs SET_ACCEL when None) is
  larger than the NIT warrants and is flagged for sign-off only — not implemented.
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

#### R22 — ACCL/ACCS puts auto-switch ACCU and seed ACCS=1.0 — neither is in the reference C — CLEARED (`70f3dfc9`)
- **Severity:** CONCERN (needs decision — verify upstream before fixing)
- **Resolution:** Verified upstream — C commit `63bfe5d0` ("Changed ACCU from a
  readback to a control") deliberately removed the 2018 `36177f7b` auto-switch
  from `updateACCSfromACCL`/`updateACCLfromACCS`. Current C `special()` never
  assigns `pmr->accu` (only reads at 489/525); a non-positive ACCS is derived
  from ACCL (`updateACCSfromACCL`, `accs=velo/accl`), not a literal `1.0`. The
  Rust was a half-port (it adopted 63bfe5d0 for the ACCU put but kept 36177f7b
  for ACCL/ACCS puts). Aligned to current C; not a C bug.
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

#### R25 — User/dial soft-limit forward picks the command side by DIR only — C also folds sign(MRES) — **DISPOSITION-KEEP**
- **Disposition:** KEEP. The record speaks dial-EGU to the driver and folds DIR (user→dial) only;
  the `sign(MRES)` high/low swap is a dial→raw register flip that belongs to the driver at its
  register boundary (C does it because `motorRecord.cc` talks raw-steps directly). No in-repo
  driver (`sim_motor.rs`, asyn-rs `runtime/axis.rs`) re-swaps or mis-programs the register, so the
  current selection is correct for every in-repo path. Re-open only when a real MRES<0 raw-register
  driver is ported. Confirmed by the 01KW5P9 review round.
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

#### R26 — get_units omits the C `dbr_units_size` truncation — DISPOSITION: keep Rust (do not copy C artifact)
- **Severity:** NIT
- **Rust:** `field_access.rs:2514-2526` (`units_for`) builds `EGU+"/sec"` uncapped.
- **C:** `motorRecord.cc:3159/3205/3206` truncates to `dbr_units_size-1` (7 chars + NUL).
- **Divergence:** EGU≥4 chars overflows: `"mrad"` → C `"mrad/se"` vs Rust `"mrad/sec"`.
- **Impact:** CA masks it (fixed 8-byte field); a PVA variable-length units reader sees the
  extra byte. C truncation is arguably an artifact; recorded for completeness.
- **Disposition (keep Rust):** the C truncation exists only to fit the fixed 8-byte CA DBR
  units field — a wire-format artifact, not a semantic intent. On the CA path Rust truncates
  at the same 8 bytes anyway; on the PVA variable-length path the full `"mrad/sec"` is the more
  correct value. Copying the record-level truncation would degrade the PVA units string, which
  the "don't copy C's bugs/artifacts" steer forbids. No code change.

#### R27 — Menu fields modeled as DBF_SHORT — graphic/precision metadata differ from DBF_MENU — DISPOSITION: defer (systemic baseline, out of remit)
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
- **Disposition (defer):** DIR/FOFF/SET/SPMG/… as DBF_SHORT is a port-wide baseline modeling
  choice (every menu field in every record), not a motor-specific bug, and the observable
  effect is nil on the enum wire path. A fix is a systemic DBF_MENU migration well beyond this
  audit's scope; deferred as a separate baseline task. No code change here.

### Category C — Status / readback / monitor / MSTA-MIP / alarm

#### R41 — URIP RDBL readback scaling runs during the initial readback (C suppresses it via `initcall`) — CLEARED (`f93eea9b`)
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

#### R42 — Initial DMOV derived from driver status instead of C's unconditional `dmov=TRUE` — REJECTED (empirically falsified)
- **Severity:** CONCERN → not a defect
- **Resolution:** The premise (a boot-mid-motion axis strands DMOV=0 forever) is
  falsified by tracing the actual code. `initial_readback` calls `process_motor_info`
  at `status_update.rs:412` BEFORE the `dmov = status.done && !status.moving`
  assignment at `:484`; the MIP_EXTERNAL detector (`:239-246`, gated
  `movn && dmov && phase==Idle && !EXTERNAL`) runs INSIDE that call while the
  fresh-record default `dmov` is still `true` (fields.rs:293) and `phase` is `Idle`.
  So a moving-at-init status FIRES the detector during init — empirically:
  AFTER INIT `dmov=false mip=EXTERNAL`; AFTER the external move completes `dmov=true
  mip=empty val=10 dval=10`. The axis closes the loop correctly (the EXTERNAL
  completion arm at `:401-416` reseeds VAL/DVAL and finalizes). C's `dmov=TRUE` at
  init is corrected on the first poll to the same end state; the Rust driver-derived
  init dmov reaches the same place via the EXTERNAL path. Blindly writing
  `dmov=true` would have broken `startup_moving_starts_polling` and gained nothing.
- **C:** `motorRecord.cc:721` — `init_record` sets `pmr->dmov=TRUE` unconditionally;
  the MIP_EXTERNAL detector lives in `process()` (~1316, commit ea063f5f), not in
  `process_motor_info`. The init-order seam differs but the observable end state
  (DMOV=1 + reseeded drive values after the external move) matches.

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

#### R44 — `udf` re-derived from `VAL.is_nan()` every cycle; C manages motor UDF only at init + DOL block — CLEARED (`66279d1a`)
- **Severity:** CONCERN
- **Resolution:** Paired structural fix. `MotorRecord::clears_udf()` overridden to
  `false` so the framework no longer recomputes `common.udf = value_is_undefined()`
  per pass; motor UDF is owned solely by the DOL channel (`dol_udf` → `check_alarms`)
  and an init clear. `initial_readback` arms `dol_udf=Some(false)` when DOL is CONSTANT
  (C init 677-681); an unset DOL is C link type CONSTANT, so `ParsedLink::None` counts
  with a literal `Constant`. A DB_LINK/CA DOL is left undefined until the closed-loop
  collection's first successful read, mirroring C leaving `udf` TRUE for a non-CONSTANT
  `.dol`. 3 tests: motor opts out of VAL-derived UDF; CONSTANT/unset DOL clears UDF at
  init; DB_LINK DOL stays undefined until the first DOL read.
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

#### R45 — MSTA EA_SLIP (bit 4) can never be set — no backing driver field — **DISPOSITION-KEEP**
- **Disposition:** KEEP. EA_SLIP bit 4 ("encoder slip enabled") is an inert informational status
  bit; no in-repo record logic or test consumes it, and no in-repo driver produces it. Adding a
  `MotorStatus` encoder-slip field with no producer would be fake parity (a field that is always
  false). The bit becomes representable for free if/when a real model-3 driver that reports it is
  ported (add the `MotorStatus` field + map it at `status_update.rs:234`). Confirmed by the
  01KW5P9 review round.
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

#### R59 — `device_support.init()` reseeds the controller position ignoring RSTM, loadpos_blocked (#231), and the #196 MRES guard — CLEARED (`3644f3a7`)
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

#### R60 — Encoder ratio (SET_ENC_RATIO / motorEncoderRatio_) is never forwarded to the driver — **DEFER**
- **Disposition:** DEFER (cross-crate, no in-repo consumer). `SET_ENC_RATIO` is a real
  controller-config command, but no in-repo driver (`sim_motor.rs`, asyn-rs `runtime/axis.rs`)
  has an encoder-ratio register to receive it, and the dial-EGU boundary already makes the record
  do the scaling — so emitting it now would be a command into the void. Hook spec for when a
  model-1/2 driver needs it: add `AsynMotor::set_encoder_ratio(ratio: f64)` default no-op +
  `MotorCommand::EncoderRatio { ratio: f64 }`; emit at init `ratio = if eres == 0 { 1.0 } else
  { mres / eres }` and re-emit on a resolution change, gated on `has_encoder` (carry the scalar
  `mres`/`eres`, not C's two-element `ep_mp[]`). Confirmed DEFER by the 01KW5P9 review round.
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

#### R61 — Idle polling stops entirely — C's always-on idlePollPeriod poller never stops — **CLEARED** (`bcad9556`)
- **Severity:** CONCERN
- **Fix:** `effects_to_actions` now emits `PollDirective::Start` on the settled idle pass instead
  of `Stop`, so the poller stays alive at `idle_poll_interval` once a move completes — the
  MIP_EXTERNAL detector and the idle-poll button resume keep firing, matching C's always-on
  `asynMotorPoller` (the record never drives a poller stop; C has no record-driven stop). Companion
  poll-loop guard: the `select!` `sleep(interval)` arm is gated on `!interval.is_zero()`, so
  `idle_poll_interval == 0` is C's event-only idle mode (`idlePollPeriod_ == 0`,
  asynMotorController.cpp:633-634 blocks on the event) rather than a `sleep(0)` busy-spin.
  Tests: `settled_idle_keeps_poller_alive_not_stopped` (record-side Start, not Stop),
  `test_zero_idle_interval_is_event_only_not_busy_spin` (zero interval polls once then blocks).
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

#### R65 — Idle poll re-processes a stationary axis every period, re-posting DIFF/RDIF; C posts nothing
- **Status: CLEARED** in `8568f173`, validated by round `01KW5SS` (3 opus panels). Resolution:
  the change-gate (`poll_and_notify` notifies only on a real `MotorStatus` change = C `statusChanged_`;
  `PollDirective::Refresh` forces a notify for `request_poll`/`status_refresh` = C `motorUpdateStatus_`)
  is C-faithful (Panel A VERIFIED-CORRECT; Panel C found no other suppressed path). The held-jog
  interaction was resolved by **preserving** the Rust resume (Panel B decisive: C strands a jog held
  across a positional completion — `special()` arms `MIP_JOG_REQ` only at `mip==MIP_DONE`,
  motorRecord.cc:3045 — and stranding an actively-held button is user-hostile, so per "don't copy C's
  bugs" the resume stays). Structural single-owner fix: `finalize_motion` requests one bounded forced
  poll when a dispatchable button (jog/home/tweak) is held, closing the whole deferral family Panel B
  enumerated (`state_machine.rs:63/205/270/315/340/364/708`, `command_planner.rs:867`). The prior unit
  test passed for the wrong reason (called `check_completion()` directly); a real-poll-loop integration
  test now drives the record only on `io_intr` and fails without the forced poll.
- **Confirming round `01KW5V2`** (3 opus panels) verified the implementation and surfaced one missed
  family member: the SET-mode pp-resync early-return (`state_machine.rs:401`, reached when
  `dispatch_res_reanchor` sets `pp=true`+`request_poll` under `SET && !IGSET`) quiesces the axis
  without `finalize_motion`. Closed in `90d9fd2d` by extracting the predicate into
  `request_poll_for_held_button` (the single owner of the resume rule) and calling it at both quiescing
  sites — refactor, not a per-site patch — guarded by `pp_resync_requests_forced_poll_for_held_jog`.
  Test hardening (`640aa84e`): the unit test now asserts the completion requested the forced poll, and
  the integration-test rationale was corrected (the Phase-2 drain, not the move speed, is load-bearing;
  runtime pinned to `current_thread`). Panel A VERIFIED-CORRECT, Panel B TEST SOUND, Panel C 3/4
  attacks SAFE with the 4th (pp-resync) now closed.
- **Final convergence round `01KW5WQB`** (3 opus panels, pinned to HEAD after the `90d9fd2d`+`640aa84e`
  fixes landed) re-verified the post-fix code directly: Panel A CONVERGED (the `request_poll_for_held_button`
  extraction is behavior-preserving — predicate byte-identical to the prior inline form, `:408`/`:494` on
  mutually exclusive phase arms, no double-fire); Panel C CONVERGED (re-classified all 14 `return effects`
  sites + every `do_process_inner` settle entry against current line numbers, the 5 prior SAFE sites
  un-regressed, no 10th hole — "the deferral family is closed"); Panel B's empirical guard stands (the
  real-poll-loop integration test fails 17/17 with the fix reverted, passes with it). **R65 fully converged.**
- **Severity:** CONCERN (monitor-semantics + CPU divergence; not a wrong-value bug). Surfaced by the
  `01KW5QV` confirming round on R61 (the always-on idle poller). Downstream of R61: once the idle
  poller never stops, the unconditional notify path turns "poller alive" into "record processed
  every period".
- **Rust:** `poll_loop.rs` `poll_and_notify` (pre-fix) bumped `status_seq` and pulsed `io_intr`
  on **every** poll, including an autonomous idle poll of a settled axis whose status did not
  change. `status_update.rs:234` marks `diff_rdif_marked=true` unconditionally on each status
  update, so every idle period drives a full record process that re-posts DIFF/RDIF (and runs the
  whole `do_work` chain) on a stationary axis.
- **C:** `asynMotorAxis::callParamCallbacks` (asynMotorAxis.cpp:316-322) fires the generic-pointer
  status callback — which routes through devMotorAsyn `statusCallback` → `dbProcess` → record
  process — **only when `statusChanged_` is set**. `statusChanged_` is set only when a status field
  actually changed (`setIntegerParam`:261 `if(status!=status_.status)`; `setDoubleParam`:282/287/292
  for position/encoderPosition/velocity). On an unchanged idle poll C posts nothing and never
  processes the record. The forced path is separate: `motorUpdateStatus_` (asynMotorController.cpp:
  217-222, what STUP/GET_INFO triggers) runs `poll(); pAxis->poll(); pAxis->statusChanged_=1;` —
  forcing a callback regardless of value change, so STUP=BUSY clears even on a stationary axis.
- **Divergence:** Rust re-processes + re-posts every idle period on a settled axis; C is silent
  until a real field change or a forced refresh. Extra CPU + monitor traffic proportional to
  `idle_poll_interval` (default 1 s) for every idle motor in the IOC.
- **Candidate fix (WIP, uncommitted at the time of this entry — under review, do NOT treat as
  blessed):** add change-detection to `poll_and_notify`: an autonomous poll (`force=false`) bumps
  the seq / pulses `io_intr` only when the freshly polled `MotorStatus` differs from the last one
  delivered (C `statusChanged_` gate); a forced poll (`force=true`) always notifies (C
  `motorUpdateStatus_`). Forcing is reached via a new `PollDirective::Refresh` for
  `request_poll` / `status_refresh` (STUP, implicit GET_INFO, settle-resume, startup), which
  `device_support` always re-sends as `StartPolling` (no `polling_active` dedup, unlike `Start`).
  Requires `MotorStatus: PartialEq` (asyn-rs, cross-crate).
- **Why a naïve value-gate is INSUFFICIENT — the held-jog interaction (the crux for the round):**
  STUP=BUSY clears only on a fresh seq (`status_update.rs:55-58`, inside `if let Some(stamped)`),
  so the forced path above is mandatory or STUP strands. Separately, the close-enough completion
  branch (`state_machine.rs:688-709`) finalizes a positional move **without** setting `request_poll`
  and **without** re-firing a held jog same-pass (unlike the give-up branch at `:637-653`, which
  calls `dispatch_latent_collection` same-pass). The Rust test
  `close_enough_defers_held_jog_to_next_idle_poll` (tests/scenarios.rs:1613) asserts the held jog
  resumes on the **next process pass** — which production currently guarantees only because the
  idle poll pulses `io_intr` unconditionally. A blanket change-gate suppresses that unchanged idle
  poll on a now-stationary axis → the held jog would never resume. The test masks this by calling
  `check_completion()` directly twice, bypassing the poll loop.
- **C ground truth on the held jog (established in this round's prep):** C **strands** this held
  jog. `special()` arms `MIP_JOG_REQ` from JOGF only at `mip==MIP_DONE` (motorRecord.cc:3045); an
  operator pressing JOGF *during* a positional move (`mip != MIP_DONE`) never arms it, and it is
  never re-armed afterward (no new `dbPutField`). The jog-fire section requires `MIP_JOG_REQ`
  (motorRecord.cc:2081-2082), and the steady-SPMG re-arm at `:1916` is gated on
  `spmg != lspg || stop` (`:1854`) so it does not run on a steady-Go close-enough completion. So C
  never auto-resumes a jog held across a positional completion. The Rust resume is therefore a
  **divergence-toward-better**, not a C behavior — which makes "should the change-gate preserve it?"
  a genuine semantic fork for the round (per "don't copy C's bugs"), not a clear regression.
- **Structural direction to validate in the round:** if the resume is to be preserved, the
  close-enough deferral (and every sibling "resume on the next idle poll" deferral site) must route
  its follow-up through an explicit `request_poll`/`Refresh` rather than relying on an autonomous
  unchanged idle poll. Defect-family anchor for that audit: `rg 'request_poll'` in
  `record/` cross-referenced against every `finalize_motion` call site that leaves a held
  button/jog/queued-motion pending without same-pass dispatch.
- **Impact:** stationary axes do redundant work and emit redundant DIFF/RDIF monitors every idle
  period; the fix must not regress STUP clearing or the held-jog resume. Needs the review round to
  (1) validate the change-gate against C `statusChanged_`, (2) rule on preserve-vs-match-C for the
  held jog, (3) enumerate the full deferral family before any edit.

---

## Fix-phase classification

- **Mechanical structural fix (clear C parity):** R1, R23, R43, R59, R44 (clears_udf override),
  R21 (full-velo numerator), R41/R42 (init-order seam), R24, R46, R62.
- **Needs a decision before edit:** R22 (verify upstream — bug vs newer-upstream tracking),
  R5 / R6 (documented intentional deviations — likely keep, surface for sign-off).
- **Cross-crate / driver-boundary (asyn-rs):** R25 (MRES<0 limit-register swap in the bridge),
  R45/R63 (MotorStatus EA_SLIP field), R60 (encoder-ratio command + hook), R61 (idle-poll policy).
