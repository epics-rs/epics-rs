# Workspace C-parity review — 2026-08-23 (parallel fix rounds)

Baseline: `main` @ 7b4f7f9a (v0.26.2). Prior inventories:
`doc/c-parity-review-2026-06-14.md`, `-06-15.md`, `-06-16.md`,
`-07-10.md` (round 6, 75 findings), plus `doc/upstream-c-bugs.md`.

This round differs from rounds 1–6 in shape: instead of one audit pass
producing a numbered inventory, six reviewer agents and nine fixer
agents ran interleaved rounds — review, dispatch, fix, re-review —
against the C originals until reviewers stopped returning findings that
met the bar. The commits are the inventory: 342 on the integration
branch — 314 from the nine fix branches, 16 merges, and 12 made while
integrating them. This file records only what a reviewer cannot read
off the diff.

## Convergence rule

A finding counted only if it carried BOTH a reproducible trigger and an
observable wrong output. "The port differs from C here" was not enough;
"a client doing X sees Y where C gives Z" was. A surface no reviewer
looked at was never counted clean. A reviewer retired after two
consecutive rounds returning nothing over its whole surface.

Every C citation was opened and read before any fixer was allowed to
act on it.

## Tier policy applied

- Tier 1 INTEROP (wire bytes, status codes, iocsh grammar, CLI output):
  match C exactly. A test asserting our own output rather than C's is
  itself a defect, and several were found and repinned.
- Tier 2 SEMANTICS: match C's observable behaviour.
- Tier 3 DRIVERS/APPS: correctness only; documented deviations
  (SIZV=256, FTVL=DOUBLE, the EGU asyn-motor boundary) are not findings.

## False in-tree parity citations

280 comment lines in our own source kept their prose and changed only
their C `file:line` — a citation that named a neighbouring block as
authority for behaviour that block does not have. Another 218 citation
lines were rewritten or dropped with the claim around them. (Measured
over `7b4f7f9a..HEAD`: strip the citation from every `-`/`+` line that
carries one, and count the pairs whose remaining prose is identical.)

The recurring shape is a range that stops one or two lines short of the
line that decides the behaviour — `usbtmc.rs:103` citing
`drvAsynUSBTMC.c:855-863` for a header with no terminator, where `:863`
is the line that sets the TermCharEnabled bit. The sub/aSub cluster is
the clearest specimen: `subRecord.c`'s pass-0 park release (`:175-178`)
and its pass-1 empty-SNAM re-park (`:182-186`) were cited for each
other's behaviour in six places, in production comments and in the
assertion messages of the tests that pin them. These are worse than a
missing comment: they read as parity evidence for the opposite of what
C does, and they survive review because the file and the line are real.

A mechanical sweep for the shape was attempted (find every citation
whose range ends within two lines of a branch or assignment) and
produced 465 candidates, of which 22 were Tier 1; 12 were verified by
hand and 0 were defects. The shape is not mechanically detectable —
recorded here so it is not attempted again. What did work was opening
the cited function by `grep -n` on its `static long <name>` anchor
rather than counting lines from a remembered offset.

## Integration decisions a reviewer should audit

### INT-1 — `PvDatabase::apply_pact_exit`

The only function in the set whose signature moved on two branches.
Two complementary fixes, not a duplicate: one added a PACT release at
three put bodies that had none (base has zero call sites in that file)
and needed a drop guard because those bodies hold `rec.write()` across
the release point; the other replaced a single-`Option` deferred slot
with C's per-record FIFO (`precord->ppnr->restartList`), because the
single slot could hold only the first queued put and the second
concurrent `caput -c` got an `ECA_PUTCBINPROG` C never sends from that
path, and lost its value. Both needed the record handle, which is why
both widened the same signature independently. The merged form is the
FIFO body reached from the guard plus the seven `processing.rs` sites.

The merged `PactExit` carries a hint **bit**, not a payload. That is
what makes the async-output path safe: the tail re-derives the pending
restart from the record at cycle end, so a token dropped on a path that
does not end the cycle loses nothing. Restoring the payload form would
start losing puts, and no test covers that — it is the one place in
this branch where the type is the proof.

### INT-2 — who arms a queued put-notify restart

The put body and the cycle a put owes are two different owners, and
before this round the put body armed the restart itself. Once the
restart moved onto the background executor it could take the target's
gate before the `PP` cycle the same put had just triggered, so the
record processed the replayed put first. Measured 4/12 failures, 0/30
after. `RestartOwner` names the two cases at the call site;
`put_pv_already_locked_before_process` is the entry for a put that owes
a cycle, and `links.rs::write_db_link_value` and the QSRV group
`ProcessMode::Force` arm are its two callers.

### INT-3 — the cycle-end guard

`recGblFwdLink`'s tail (`recGbl.c:295-302`) runs on every path that ends
a cycle, including a failing one (`subRecord.c:136-167` runs the whole
tail on any status but the documented async `1`). Three exits in
`process_record_inner` did not: `run_registered_subroutine()?`,
`record.process()?`, and the async-output `return Ok(())`.
`CycleEndGuard`'s `Drop` pays the tail for every exit that did not, and
the two ways out are explicit at the site — `take()` for a site that
ends the cycle its own way, `hand_off_to_async_completion()` for the one
early return that does not end the cycle at all. `#[must_use]` on
`PactExit` cannot stand in for this: the lint fires on an unused
*expression*, and each of those paths drops a `let`-bound token.

### INT-4 — the record-processing runtime seam

`runtime::task::spawn` needs a tokio runtime on the calling thread;
record processing has none on a blocking CA/PVA connection thread
(`block_on_sync` → `park_on`), and none at all on the callback pool a
tail was just deferred to. The `_background` counterparts land on the
process-global executor on every backend.

The census that enforces this used to live inside `epics-base-rs` and
read its own nine files with `include_str!`, which may not escape a
published crate's package directory — so the rule stopped at the crate
boundary and the defect walked across it. `ThrottleRecord::spawn_value_sync`
and `AsynRecord::special`/`process_cycle` were all naming `tokio::spawn`
from record-support callbacks; the throttle one began panicking on
`cbMedium` the moment its caller moved. The rule now lives in
`tools/record-seam-gate` with its own slicer fixtures, and each crate
keeps a short `tests/record_seam_gate.rs` naming its own sources.

### The same wire symptom, two layers

That `ECA_PUTCBINPROG` also has an independent cause in the CA server,
where the port sends it on every supersede while C sends it only after
the 60 s `blockSem` timeout (`camessage.c:1745`). Fixing either layer
alone leaves the code still reaching a client. The commit bodies name
which layer owns the refusal.

### Duplicate lanes resolved

Five cases where two branches fixed one defect. Resolution rule: keep
the version from the panel that owns the crate's structural fix, even
when the other landed first, because splitting one structural fix
across two branches guarantees an incoherent merge. Where the losing
side carried a test the winner lacked, the test was kept. A conflict
count is not a duplicate count: `busy.rs` conflicted on four pairs and
only one was duplicated work — the per-branch `git log -- <file>` is
what classifies a hotspot, `git merge-tree` only nominates it.

### CI coverage moved, not added

`eab3a721` drops `reference_trees`, `dbd_initial_parity`,
`base_device_parity` and `stdsupport_device_parity` from `profile.ci`.
Those suites read an upstream C checkout and panic rather than skip when
it is absent, and no cross-platform runner provisions one. The coverage
therefore moves from green-and-vacuous to not-run on CI; it still runs
locally and on any runner with the trees. `profile.interop` is not an
alternative — `interop.yml` provisions epics-base and pvxs but never
epics-modules.

## Public API changes

18 public items removed and 24 signatures changed; the full table is in
`doc/breaking-2026-08-23.md`. `ProcessMode` is not deleted — it moved to
`epics-base-rs` `server::database::field_io::ProcessMode` as the one enum
behind both the PVA sources and the QSRV group put.

## UNFIXED

160 root causes were named and not closed. Each fixer's own list is
reproduced verbatim below — the file:line and the reason are that
panel's, not a paraphrase — except that C citations this round
retargeted are quoted in their corrected form. A branch's commit count is `git rev-list
--count 7b4f7f9a..<branch>`, and all nine are fully merged into the
integration branch (0 unmerged commits each).

### fix-base

Every root cause bypassed, deferred, or left open across all rounds of this
session. Line numbers are at branch HEAD `8832a97c`.

- **bo HIGH one-shot arming panic** — `crates/epics-base-rs/src/server/records/bo.rs:254`: the one-shot arms with the panicking `std::time::Duration::from_secs_f64(self.high)` and HIGH is DBF_DOUBLE, so `caput REC.HIGH 1e300` — which C accepts, `callbackRequestDelayed` merely schedules past any run's lifetime — panics the task; pre-existing (blame `65b977b1`, an ancestor of `7b4f7f9a`), and `24aa99d0` closed only the two re-arm sites, with the arming sites left to fix-infra's `duration_from_secs` sweep.
- **busy HIGH one-shot arming panic** — `crates/epics-base-rs/src/server/records/busy.rs:252`: the identical panicking conversion on busy's HIGH one-shot; pre-existing (blame `07b5c031`, an ancestor of `7b4f7f9a`) and deferred for the same reason.
- **mbbiDirect clears UDF on a failed read** — `crates/epics-base-rs/src/server/database/processing.rs:6555`: the soft-input UDF clear fires on every mbbiDirect cycle including the failed-link one, where `mbbiDirectRecord.c:155-166` clears only under `status == 0`, so an unresolvable INP leaves the record defined instead of undefined; left open because it is upstream of the `udf_alarm_severity` hook `1b04c231` added and I did not isolate whether the defect is in `clear_udf`'s computation or in a per-record declaration (evidence noted at `crates/epics-base-rs/tests/udf_alarm_severity_is_per_record.rs:74`).
- **asTrapWrite put-logging listener absent** — `crates/epics-base-rs/src/server/access_security.rs:844`: `AccessRule::trap` parses and stores `TRAPWRITE`/`NOTRAPWRITE` case-sensitively as `asLib.y:272-283` does, but nothing in this crate consumes the mask, so a TRAPWRITE rule logs nothing; the consumer is a separate subsystem that does not exist here.
- **put_pv_no_process runs neither special() pass** — `crates/epics-base-rs/src/server/database/field_io.rs:2365`: the autosave-restore entry writes a field without `special_before_put` or `special_after_put`, so a restored SNAM, CALC or SIMM leaves its derived state stale where C's autosave goes through `dbPutField` and runs `dbPutSpecial` on both sides; not folded into `8832a97c` because the gap is a whole missing `dbPutSpecial`, not the SNAM half, and closing it changes every SPC_MOD field at once.
- **sub's empty-SNAM arm clears sadr** — `crates/epics-base-rs/src/server/database/field_io.rs:214`: the empty-name arm clears `subroutine` for `sub` as well as `aSub`, but `subRecord.c:182-186` leaves `sadr` alone and sets `pact = TRUE` instead; clearing is only observation-equivalent because this branch has no put-side PACT park, so the arm must stop clearing once fix-rec's `92e0a52b` park lands.
- **seq PREC on a connected DOLn** — `crates/epics-base-rs/src/server/records/seq.rs:27`: when a DOLn link is CONNECTED the override answers seq's own PREC, where C's `get_precision` (`seqRecord.c:78`) answers the upstream record's; `af8b3df0` corrected the false doc claim and pinned the three arms with tests but did not model this one.
- **rev-bridge's four precision findings against 2acc6b9d** — no file:line available: four regressions were filed against `2acc6b9d` (the `get_units`/`get_precision` per-field routing fix) and I could neither fix nor refute them because their text was never forwarded to me; the commit itself should survive, since dropping it also drops its 197-line `rset_units_precision_route_per_field.rs`.
- **transform SHORTCUTS drops C's leading `$` (deliberate deviation, upstream bug left open)** — `crates/epics-base-rs/src/server/records/transform.rs` `SHORTCUTS` / `transformRecord.c:333-341`: C's replacements carry a leading `$` (`{"$S(", "$SSCANF("}`) and `sCalcPostfix` has no `$SSCANF` element, so on C every expanded shortcut becomes uncompilable and the channel is silently never evaluated (measured against this host's libcalc: `$S("7","%d")` status 0, `$SSCANF("7","%d")` status -1 error 11); this port drops the `$` to keep the expression working, so the upstream defect is documented as `doc/upstream-c-bugs.md` CBUG-H1 rather than reproduced.


Branch: `caucus/VS754JQF2H/fix-base-441cddf2-1` (32 commits)

### fix-db

- **LCNT/MAX_LOCK re-entry declines silently** — `crates/epics-base-rs/src/server/database/processing.rs:1451`: the comment claims C's re-entry decline is silent citing `dbAccess.c:537` (only `if (precord->pact) {`); an in-cascade re-entry takes the early return in `process_entry_prelude` and never reaches the port's LCNT gate at `:1640-1680`, so the port declines quietly and `LCNT` reads 0 where C increments it and past `MAX_LOCK` raises `SCAN_ALARM`/`INVALID_ALARM` "Async in progress" (`dbAccess.c:545-559`, reached because `dbDbLink.c:512` calls `dbProcess` unconditionally). Not closed: the freeze arrived while I was reading the C, and the fix belongs in the file fix-rec/fix-ca now hold.
- **`RecordInstance::notify` is still a public field** — `crates/epics-base-rs/src/server/record/record_instance.rs:649`: after `004ab843` no production code assigns the slot outside that module, but the field's `pub`ness still makes the illegal state constructible and four `tests/*.rs` fixtures do assign it directly. Not closed: flipping it private needs an accessor at `processing.rs:4012` (`guard.notify.clone()`), inside the `write_begin`/`sim_pact_exit` region fenced off for fix-rec.
- **Two wait-set installs hold the gate only by naming contract** — `crates/epics-base-rs/src/server/database/field_io.rs:1894` and `:2347`: their gate is the caller's, so `install_or_queue_notify` cannot demand a `&RecordWriteGuard` proof token; the precondition is documented, not enforced by type. Not closed: an atomic group PUT holds a `ManyRecordWriteGuard`, so no single guard value exists to pass.
- **`join_put_notify`'s join guard is narrower than C's** — `crates/epics-base-rs/src/server/record/record_instance.rs:1482`: ours tests only `notify.is_some()`, while C `dbNotifyAdd` also requires `pnotifyPvt->state == notifyProcessInProgress` and `pto != dbChannelRecord(ppn->chan)` (`dbNotify.c:492-494`). Not closed: I moved the function verbatim in `004ab843` and deliberately did not change its semantics; whether the two extra terms matter here is unverified.
- **The `ECA_PUTCBINPROG` half of the X1 brief does not reproduce** — `crates/epics-base-rs/src/error.rs:91`: `CaError::PutCallbackInProgress` has zero constructors on this branch and on `integration/parity-2026-08-23` (only the variant decl and two `match` arms), so no CA put-callback can receive 362 from the put-notify path. Not closed because there is nothing to close: rev-base's measured repro predates `de3bb856`, which replaced both refusal arms with queue-and-wait.
- **`de3bb856`'s two dispatched refusal sites are defended by construction and grep, not by a test** — `crates/epics-base-rs/src/server/database/field_io.rs:1894` and `:2347`: they are unreachable single-threaded (the entry gate catches every fresh notify put and `take_next_notify_restart` will not pop while the slot is owned), so the site-level tests I wrote passed pre-fix and I deleted them rather than ship a vacuous gate. Not closed: the shipped tests fail-first on a third site instead.
- **`const` and `pva` link-value tokens are decoded without the dialect** — `crates/epics-base-rs/src/server/record/link.rs:1034` (and the same shape at `:1239`): `decode_json_string_token` reads the value token by hand, so a comment *inside* a value is still mis-read. Not closed: unreachable through the object grammar `relaxed_to_strict` now owns, so it was out of the fold's family.
- **A trailing comment after the closing brace makes a link `NotJson`** — `crates/epics-base-rs/src/server/record/link.rs:944`: `parse_json_link` tests `s.ends_with('}')` against the *original* text, so `{calc:{…}} // x` is rejected where C accepts it. Not closed: pre-existing and outside the call sites the json5 brief named.
- **Dialect forms C accepts that the owner still hands to `serde_json` unchanged** — `crates/epics-base-rs/src/json5.rs:99`: trailing commas, hex literals, leading `+`/`.` numbers, `Infinity`/`NaN`, and single-quoted strings in a position `serde_json` must parse — all measured ACCEPT in base's yajl, all rejected here. Not closed: the brief scoped the fold to comments and bare identifier keys.
- **Bare `+ident` keys are a superset of C** — `crates/epics-base-rs/src/json5.rs:99`: measured, base's yajl raises a lexical error because `+` opens a number (`yajl_lex.c:702`), while we quote it. Not closed: built to the brief's explicit instruction and documented as a deliberate deviation.
- **Rejecting an unterminated `/*` after a complete top-level value is a subset of C** — `crates/epics-base-rs/src/json5.rs:99`: measured, base's yajl ACCEPTs it. Not closed: same reason — the brief required the error arm.
- **The lock-order document has no same-rung re-entry rule** — `crates/epics-base-rs/src/server/database/record_lock.rs:168`: it constrains ordering *between* rungs only and explicitly lists `add_loaded_record` as safe, which is why the `registration_mutex` self-re-entry deadlock survived review. Not closed: analysis round only, no commit was requested.
- **The scan-index bucket is written directly, bypassing its owner** — `crates/epics-base-rs/src/server/database/mod.rs:1723` (and `remove_record` at `:1801`): both take `scan_index.bucket(list).lock()` themselves instead of routing through `update_scan_index`. Not closed: same analysis round; the deadlock it enabled was closed on the integration branch by someone else.
- **Seven hardcoded `$HOME/codes/pvxs` skip branches in the pvxs interop suite** — `crates/epics-pva-rs/tests/interop_pvxs_mods/be_byte_order.rs:172` (same at `large_array.rs:254`, `monitor_stream.rs:294`, `pipeline_r20.rs:38`): measured post-fix, `interop_pvxs` runs 25/25 while still printing 6 SKIP lines, so the suite can go green with helpers never built. Not closed: same family as `394e0fd3`/`e5e05431` but a different mechanism (they build C++ helpers rather than read sources), and it was not dispatched to me.
- **Three absolute macOS C citations in the PVA request parser** — `crates/epics-pva-rs/src/pv_request.rs:561` (same at `:933`, `:1451`): they name `pvxs/src/clientreq.cpp` under `/Users/stevek`, unusable on this machine. Not closed: classified distinct by the lead and excluded from the reference-resolver commit.
- **An absolute macOS path in a CA test helper doc comment** — `crates/epics-ca-rs/tests/common/mod.rs:159`: names `/Users/stevek/codes/epics-base`. Not closed: classified distinct by the lead.
- **One workspace test failure I could not prove unrelated** — `crates/epics-bridge-rs/tests/qsrv_remote_log.rs:152`: `monitor_dbe_empty_mask_precedes_the_init_reply` failed once with "no complete frame within deadline" after 5.029 s during the `004ab843` gate. Not closed: it passed on rerun of the identical post-fix tree and on the pre-fix tree, so I have two passes and one failure and no reproduction — a load flake on that evidence, but unproven.


Branch: `caucus/VS754JQF2H/fix-db-a8189ca9-1` (26 commits)

### fix-rec

- **table SPC_DBADDR put path** — `crates/optics-rs/src/records/table.rs:2626`: `beb413d1` routes the eight `SPC_DBADDR` fields to their arrays on the read path only, so a client PUT into those fields is not routed to the same arrays; the write half was never scoped to me.
- **PACT guard keyed on an alias (848)** — `crates/epics-base-rs/src/server/database/field_io.rs:848`: `PactExitGuard::new` is handed `base` where every neighbouring `update_scan_index` / `run_special_actions` / `lock_record` call uses the alias-resolved `canonical_base` computed at `:830`, so a put arriving via an alias arms the guard under the wrong name; found while auditing the guard, not assigned as a finding.
- **PACT guard keyed on an alias (1173)** — `crates/epics-base-rs/src/server/database/field_io.rs:1173`: same defect, with `canonical_base` already computed at `:1163`.
- **PACT guard name unclassified (1938)** — `crates/epics-base-rs/src/server/database/field_io.rs:1938`: passes `record_name` inside a function that resolves no alias at all, so it needs its own verdict rather than the same fix; left unclassified because I could not establish whether that entry can ever see an alias.
- **lnkCalc relaxed JSON rejected** — `crates/epics-base-rs/src/server/record/link.rs:958`: the calc-link parser accepts only strict JSON with quoted keys, so base's own relaxed form (`{calc:{expr:'A+5',args:[7]}}` in `linkRetargetLink.db:20`, `lnkCalcTest.c:56`, `linkRetargetLinkTest.c:78`) silently degrades to a `ParsedLink::Db` whose record name is the whole JSON blob and the INP never resolves; out of my assigned findings.
- **lnkCalc `time` char not rejected** — `crates/epics-base-rs/src/server/record/link.rs:958`: C rejects the entire link with `jlif_stop` when `time` is not exactly one in-range character (`lnkCalc.c:180-182`), where the port sets `time_source: None` and continues; same reason.
- **bo init-tail inside seed_deadband_tracking** — `crates/epics-base-rs/src/server/records/bo.rs:228`: convert/`oraw`/`orbv` init work still lives in `seed_deadband_tracking`, the dual meaning the lead had me remove from `busy`; deliberately not moved because `fix-base` was editing that override and moving it manufactures a collision.
- **mbbo init-tail inside seed_deadband_tracking** — `crates/epics-base-rs/src/server/records/mbbo.rs:560`: same defect, same reason.
- **mbbo_direct init-tail inside seed_deadband_tracking** — `crates/epics-base-rs/src/server/records/mbbo_direct.rs:319`: same defect, same reason.
- **ao init-tail inside seed_deadband_tracking** — `crates/epics-base-rs/src/server/records/ao.rs:383`: same defect, same reason.
- **bo comment cites the wrong init span** — `crates/epics-base-rs/src/server/records/bo.rs:225`: cites `boRecord.c:163-172` where the real span is `:166-175`; introduced by `b23ccb24`, an ancestor of `7b4f7f9a`, and left alone rather than touch a file `fix-base` was editing.
- **c_parse fractional-string integer put** — `crates/epics-base-rs/src/types/c_parse.rs:103`: `put_string` handles a fractional string put into an integer field differently from C; never assigned as a finding.
- **inherit_link_severity ungated** — `crates/epics-base-rs/src/server/database/processing.rs:5755`: applied unconditionally where C gates it on `!status`; never assigned.
- **TSEL resolved per cycle** — `crates/epics-base-rs/src/server/database/processing.rs:1616`: `rec_gbl_resolve_tsel` re-resolves TSEL every cycle at stage 0.3, while C resolves it at the `recGblGetTimeStamp` call site; never assigned.
- **dfanout UDF/SELL ordering unverified** — `crates/epics-base-rs/src/server/records/dfanout.rs:234`: I never established whether the port's order matches C's `udf = isnan(val)` (`dfanoutRecord.c:120-121`) → `dbGetLink(SELL)` (`:126`) → `checkAlarms` (`:127`); it is unverified, not proven wrong.
- **mbbi_direct shift asymmetry** — `crates/epics-base-rs/src/server/records/mbbi_direct.rs:163`: uses `checked_shr` where its pair `mbbo_direct.rs:352` uses `wrapping_shl`, and one of the two comments describes the other's behaviour; never assigned.
- **ai SPC_LINCONV has no refusal** — `crates/epics-base-rs/src/server/records/ai.rs` (C anchor `aiRecord.c:183`): C returns `S_db_noMod` when `pdset->common.number < 6` and the port has no such refusal; I could not pin the port-side line, so it is recorded to file granularity rather than invented.
- **ao SPC_LINCONV has no refusal** — `crates/epics-base-rs/src/server/records/ao.rs` (C anchor `aoRecord.c:250`): same defect, port line likewise unpinned.
- **compress N=0 silently becomes 1 (325)** — `crates/epics-base-rs/src/server/records/compress.rs:325`: `self.n.max(1)` rewrites a `DBF_ULONG` `N = 0` instead of honouring it; never assigned.
- **compress N=0 silently becomes 1 (371)** — `crates/epics-base-rs/src/server/records/compress.rs:371`: same defect.
- **compress N=0 silently becomes 1 (500)** — `crates/epics-base-rs/src/server/records/compress.rs:500`: same defect; a fourth `max(1)` at `:824` is the NSAM load-path buffer sizing, which I judged a different thing and did not classify.
- **acalcout has no IAAV gate** — `crates/epics-base-rs/src/server/records/acalcout.rs:726`: the `IAAV..ILLV` link-status gate is absent; never assigned.
- **calcout over-posts DBE_LOG** — `crates/epics-base-rs/src/server/records/calcout.rs:1344`: C posts a literal `DBE_VALUE` on link-status fields and the port ORs `DBE_LOG` on top; never assigned.
- **scalcout over-posts DBE_LOG** — `crates/epics-base-rs/src/server/records/scalcout.rs`: same defect, port line unpinned.
- **acalcout over-posts DBE_LOG** — `crates/epics-base-rs/src/server/records/acalcout.rs`: same defect, port line unpinned.
- **fused link doc comment** — `crates/epics-base-rs/src/server/database/links.rs:880`: the pvalink `time=true` prose describing `external_link_time` (`:904`) is attached to `registered_link_sets` (`:895`), leaving the real function undocumented; cosmetic, so it lost to the assigned findings.
- **motor.CARD not derived from OUT** — `crates/motor-rs/src/record/dbd_generated.rs:437`: C derives CARD from the OUT link type in `init_record` (`motorRecord.cc:650-665`) and the port serves the `INST_IO` answer unconditionally; classified distinct during the motor field sweep and left open.
- **749b7c51 has no fails-first proof on this branch** — commit `749b7c51` (PACT restart drop guard): the red→green proof exists but the lead produced it on the merged tree (`a_parked_put_notify_replays_when_the_snam_put_releases_the_park`, 0.0 vs 8.0 with the guard body disabled), so my own history does not carry it.
- **PACT exit bypass at the async write arm** — `crates/epics-base-rs/src/server/database/processing.rs:3771`: the OUT stage's `write_begin` `Ok(Some(..))` arm returns without consuming the cycle's `PactExit`, dropping the parked put-notify; I had it red on this branch and was mid-fix when the lead withdrew the brief's premise and took the fix onto the integration branch, so nothing landed here — note the token actually lost is `continuation_pact_exit` (bound `:3283`), not `sim_pact_exit`, which is provably empty there because `sim_output.is_some()` forces `out_info = None` at `:3685`.
- **analog_lalm ladder spans stop short** — `crates/epics-base-rs/tests/analog_lalm_latches_only_on_a_raise.rs:173`: cites `selRecord.c:265-300` / `dfanoutRecord.c:242-277`, which cover the four alarm levels but not the `val`/`hyst`/`lalm` load and the no-alarm `prec->lalm = val` store the sentence implies (`:263-302` / `:242-279`); I re-measured it during C1, judged the narrower span defensible, and left it rather than widen a citation outside that finding.


Branch: `caucus/VS754JQF2H/fix-rec-098446bf-1` (32 commits)

### fix-ca

- **Deferred-tail census cannot see new files** — `crates/epics-base-rs/src/server/mod.rs` (`deferred_tail_owner_tests::SOURCES`): the by-construction guard against the ambient seam is a source census over nine listed files, so a deferred record tail added to a tenth file in `server/` is not caught; `include_str!` needs literal paths, and the same limitation already applies to the existing `client_native/mod.rs` and `calink/resolver.rs` censuses.
- **LinkSet contract is undocumented and the no-reactor fallback keeps a dual meaning** — `crates/epics-base-rs/src/server/database/link_put_queue.rs` (`reactor` field / `spawn_network`): when no tokio runtime existed anywhere at database construction, `connect_link`/`put_value` run on a callback-pool thread instead of the reactor, and I did not add that contract note to the `LinkSet` trait docs — it is recorded only on the queue's own field, so an implementor cannot see it.
- **`TickDriver::capture()` still demands an entered runtime** — `crates/epics-base-rs/src/server/scan.rs:126`: it `expect`s `Handle::try_current()` with the stated reason "record processing may spawn tasks and start timers", of which the spawn and timer halves are now false and only the external-link-put half remains; changing how periodic-scan threads drive processing is its own behaviour change and was not attempted.
- **Autosave tails stay on the ambient seam** — `crates/epics-base-rs/src/server/autosave/manager.rs:139`: four spawns and three intervals were reverted to `runtime::task::spawn`/`interval` because every save goes through `runtime::fs::blocking` (hosted `tokio::task::spawn_blocking`), so they are blocking-pool-bound; they are off the record-processing chain, but the split inside one crate is enforced only by the comment there.
- **`realtime-ca-ioc` still refuses to run on the hosted build** — `crates/epics-ca-rs/src/bin/realtime-ca-ioc.rs:58-68`: I corrected only the clause that became factually false ("`background_init` is not even compiled"); the refusal itself is unchanged and now rests solely on the CA/PVA network paths, which was not re-examined.
- **The default gate never compiles `ca-server-tls-test` files** — `crates/epics-base-rs/tests/client_server.rs:150`: the pre-existing `E0308` and the stale `T:CHAR` expectation both survived because the file sits behind a feature the standard `clippy`/`nextest` scope never builds; I fixed the two defects in `0909e7cb` but not the gate blind spot that hid them.
- **Commit `21ca4877` re-anchors C citations to an unreleased local checkout** — `/home/stevek/work/epics-base` at `8f5015b66` (`R7.0.10-146`, carrying unmerged upstream PR #944): a function-resolution check showed 176 of 187 retargeted pairs already resolved correctly at released `R7.0.10`, so that commit moved them onto line numbers no released tree has; I reported it against my own commit under the freeze but did not revert it, because hashes on this branch are cited elsewhere and I was told not to amend.
- **Reactor-bound spawn sites keep the ambient seam by convention** — `crates/epics-libcom-rs/src/runtime/task.rs` (`spawn`): roughly 55 production spawns across `epics-ca-rs`, `epics-pva-rs` and `epics-bridge-rs` genuinely need the tokio reactor, time or signal driver, and nothing enforces that a future spawned there does not later become reactor-free or vice versa; the split between the two named owners is documented but not checked outside `epics-base-rs/src/server`.


Branch: `caucus/VS754JQF2H/fix-ca-8d1ab0bf-1` (37 commits)

### fix-pva

Every root cause bypassed, deferred, or left open across all rounds on this
branch. Severity in the final report; this file is the root-cause list.

- **Initial event mask hardcoded** — `epics-base-rs/src/server/database/filters/mod.rs:198`: the synthetic initial event is built with `mask: EventMask::VALUE` instead of the subscriber's own select, where C seeds `pLog->mask = pevent->select` (`dbEvent.c:746-753`) and `db_post_single_event` (`dbEvent.c:912-927`) deliberately does not overwrite it, so `camonitor -m p` on a `.{sync}` channel drops an initial event C passes; not closed because it is the opposite direction from the delivered-mask finding I was given and was never dispatched.
- **printf bare exponent** — `epics-base-rs/src/server/records/printf.rs:508`: `%e`/`%g` emit Rust's `{:e}` (`1e-4`) where C99 mandates at least two exponent digits (`1e-04`); found by the F3 sweep as a distinct defect from the `%g` style decision I did fix, and left untouched.
- **asyn-rs keeps its own %g copy** — `asyn-rs/src/param.rs:1350`: a second correct-but-separate `%g` implementation survives the shared-owner consolidation because `epics-base-rs` is only an optional dependency of `asyn-rs`, so it cannot reach `format_g` without a dependency change that is out of this branch's shape.
- **build_dbnd rejects what C accepts** — `epics-base-rs/src/server/database/filters/parser.rs:893`: `{"dbnd":{}}` and `{"dbnd":{"m":"rel"}}` are refused where C accepts them, because C prefix-matches both option keys and enum names (`chfPlugin.c:279-300`, `dbnd.c:35-44`) and that prefix matching is not ported.
- **JSON5 numeric literals refused** — `epics-base-rs/src/server/database/filters/parser.rs:333`: `json5_filter_to_json` refuses bare JSON5 numerics with `InvalidJson` where C's yajl is unconditionally in JSON5 mode (`yajl.c:77` sets `yajl_allow_json5 | yajl_allow_comments`), so `0xff` (`yajl_lex.c:466-468` + `yajl_parser.c:57-62` base-16 branch), `Infinity`/`NaN` (`yajl_lex.c:445-459`, `:675-691`), `+1` and `.5` all create a channel in C and are rejected here; filed as an OBSERVATION under the convergence line and left for fix-db, whose file owns this at merge.
- **epics_parse_double hex floats** — `epics-base-rs/src/server/database/filters/parser.rs:615`: glibc hex-float spellings are rejected, and subnormal-result underflow stays accepted; the underflow half is UNVERIFIED because I did not measure C's behaviour for it.
- **read_partial stamps no options branch** — `epics-bridge-rs/src/qsrv/group.rs:991`: the (currently dead) `read_partial` path records no `record._options` branch at all, so if it is ever revived it silently diverges from every other read path.
- **remove_source beacon_change** — `epics-pva-rs/src/server_native/composite.rs:134`: `remove_source` bumps `beacon_change` only when an entry was actually erased, where pvxs bumps unconditionally (`server.cpp:112`), so a no-op removal does not signal the topology change C signals.
- **pvalink FLNK ignores retry** — `epics-bridge-rs/src/pvalink/link.rs:256`: the FLNK connected-gate never consults `retry`, where `pvalink_lset.cpp:685` does, so a link whose retry state should replay a staged write does not; distinct from the `scan_forward` PUT finding I closed and never dispatched.
- **Gateway relays CMD_PROCESS upstream** — `epics-bridge-rs/src/pva_gateway/source.rs:1510`: `process_checked` forwards a downstream `CMD_PROCESS` to an upstream that may be pvxs, which does not implement it the same way; this is the R5-PVA-1 brief and was never assigned to me.
- **One union DbSubscription, not two** — `epics-pva-rs/src/server/native_source.rs:1073`: `subscribe_checked_opts_marked` opens a single `DbSubscription` on the value mask OR'd with `DBE_PROPERTY` and narrows per event via `nt::event_leaves`, where pvxs opens two independent subscriptions (`singlesource.cpp:161-167`); this is the structural cause behind two separate findings this round and the source-level divergence survives even after `cc32300e` split the native monitor.
- **No per-channel object on the native source** — `epics-pva-rs/src/server/native_source.rs:605`: `resolve_channel` builds a fresh `FilterChain` per operation, so the GET chain and the value-subscription chain are separate instantiations, where pvxs shares one `dbChannel` and a GET therefore advances `dbnd`'s `my->last`; closing it needs a per-channel object neither source has.
- **Link-field promotion absent on both sources** — `epics-pva-rs/src/server/native_source.rs:139`: `DBF_INLINK..DBF_FWDLINK` are not promoted to `DBR_CHAR`/`PVLINK_STRINGSZ` with `form="String"` (pvxs `channel.cpp:69-73`) on either the native source or the bridge, and it cannot be closed without the `Q:form` model neither one has.
- **display.form.index always 0** — `epics-pva-rs/src/server/native_source.rs:262`: the form menu is filled but the index is pinned to 0 ("Default"), so `REC.DESC$` serves form `Default` where QSRV and `softIocPVX` serve `"String"`; selecting another entry needs the channel's `Q:form` info tag, which this source does not model.
- **Group +channel `$` and filter suffixes refused** — `epics-bridge-rs/src/qsrv/channel.rs:102`: `resolve_db_channel` refuses a group member channel carrying either the `$` long-string modifier or a filter suffix, both of which pvxs honours; refused with a named reason rather than silently, but refused.
- **Bare parse_pv_name splits in group.rs** — `epics-bridge-rs/src/qsrv/group.rs:648` (also `:680`, `:1053`, ~15 sites): member channel names are split by hand rather than through `parse_channel_name`, and they are currently unreachable only because `resolve_db_channel` refuses every member name carrying a modifier — the sites themselves were not fixed.
- **Monitor ops bypass requeue_on_disconnect** — `epics-pva-rs/src/client_native/ops_v2.rs` (no single line; the wrapper is applied to the 15 one-shot ops only): monitors do not route through `requeue_on_disconnect`; classified distinct rather than bypassed, because `recv_monitor_init` reaching `MonitorEnd::ConnectionLost` already re-searches and resubscribes, so that rule has an owner on the monitor path.
- **4deb23b9 has no fails-first proof** — `crates/epics-pva-rs/src/server/native_source.rs` (commit `4deb23b9`): the commit lifting `resolve_channel`'s filter refusal was never proven by stash-and-rerun; its evidence is three previously-green assertions that contradict the refusal it removes, which is below the bar every other commit on this branch met.
- **CBUG-H2's C citation unverified locally** — `doc/upstream-c-bugs.md` (commit `b479a215`), C anchor `pvDataCPP printer.cpp`: pvDataCPP is not checked out on this machine, so the file:line is recorded explicitly as the lead's read and only the arithmetic (`'A'+9-10 == '@'`) was checked here; the same limit applies to the F2/F5 whitespace choices in `53fab2ac` (union id default `union`, the `(none)` indent, an `any` member printed with an empty field name giving `int  5`), which follow the lead's description rather than a local file.
- **One unexplained procserv_e2e failure** — `epics-tools-rs` test `procserv_e2e toggle_into_oneshot_grants_one_more_run`: failed once during an earlier F1 gate and passed on isolated rerun and on every full-workspace run since; never reproduced, therefore never diagnosed, and explicitly NOT established as a load flake.


Branch: `caucus/VS754JQF2H/fix-pva-d8e228e0-1` (35 commits)

### fix-asyn

- **Split MBAP reply is not reassembled** — `crates/modbus-rs/src/ioc.rs:231`: `SyncIoTransport::write_read` issues one raw `read_octet`, so a 259-byte reply arriving as 140+119 bytes across two `io_read_octet` calls ends the cycle at `io_status == Error` instead of being served; closing it needs `self.mbap`, which does not exist on this branch and which the lead owns after the merge.
- **`transact` discards a short frame instead of accumulating** — `crates/modbus-rs/src/driver.rs:725`: the `FrameTooShort` arm for `Tcp|Udp` skips the partial frame and re-reads, so the next chunk's payload bytes are parsed as an MBAP header, where C accumulates across reads (`modbusInterpose.c:368`, `if (id == pPvt->transactionId) break;`); same root cause as the item above and blocked on the same accumulator.
- **`wait_writable` can return before its deadline** — `crates/asyn-rs/src/drivers/ip_port.rs:449`: it returns on `poll`'s `rc == 0` without re-reading the deadline and `wait_millis` (`crates/asyn-rs/src/drivers/mod.rs:71`) truncates via `Duration::as_millis`, so a bounded write reports `Timeout` up to 1 ms early; left as-is because the shape matches C's single-poll `writeIt`/`readIt` (`drvAsynIPPort.c:649-651`, `:775-777`) and the lead adjudicated in writing against changing the implementation.
- **`readRaw`/`writeRaw` are cited but do not exist in asyn C** — `drvAsynIPPort.c` (real symbols `readIt:745`, `writeIt:614`): 59 in-tree references across `crates/asyn-rs/src/drivers/ip_port.rs`, `drivers/ip_server_port.rs`, `src/port.rs` and `crates/asyn-rs/doc/c-parity-review-drivers-2026-06-29.md` attribute behaviour to C functions that `rg -w --no-ignore` finds nowhere in the asyn tree; pre-existing and outside every dispatched anchor, so left for the next round's inventory.
- **USE_SOCKTIMEOUT fall-through cited at the wrong range** — `crates/asyn-rs/src/drivers/ip_server_port.rs:400`: quotes `drvAsynIPPort.c:744-756` for the failed-`setsockopt` fall-through when the real code is `:778-790` (`status = asynError` at `:783-788`); pre-existing (blame `81c561e2e`, on main), not in a dispatched family.
- **recv fall-through cited at the wrong line** — `crates/asyn-rs/src/drivers/ip_server_port.rs:406`: quotes `drvAsynIPPort.c:791` for "falls through to recv()" when the real code is `:809-831`; same pre-existing comment block as above.
- **Teardown rule cited at the wrong range** — `crates/asyn-rs/src/drivers/ip_server_port.rs:409`: quotes `drvAsynIPPort.c:797-821` for "the recv outcome governs teardown" when the real code is `:832-841`; same pre-existing comment block as above.
- **`procserv_e2e` failure not attributed** — `crates/epics-tools-rs/tests/procserv_e2e.rs:1145`: `toggle_into_oneshot_grants_one_more_run` failed 2 of 4 full-workspace runs at a fixed tip on the `MUST_ARRIVE` deadline (5/5 pass in isolation); my branch touches neither that crate nor any code it reaches on Linux, but I could not re-run it against `7b4f7f9a` without a second checkout, so it is neither confirmed pre-existing nor cleared.
- **Citations in the pre-`af04a4ee` commits were never re-read** — `crates/modbus-rs/src/ioc.rs` and the 30 commits before `af04a4ee`: I re-verified only the 19 addresses I corrected this round, so every other C citation in those commits still rests on its original author's reading — including the 14-site `ioStatus_` list (`drvModbusAsyn.cpp:527/531, 677/681, 838/842, 984/988, 1127/1130, 1296/1299, 1466/1469`) which I took from fix-ad's correction rather than checking myself.


Branch: `caucus/VS754JQF2H/fix-asyn-966856b8-1` (36 commits)

### fix-ad

- **mca SIMM_ALARM ordering** — `crates/epics-base-rs/src/server/database/processing.rs:6247` (C `mcaRecord.c:1131`, SIOL read at `:1116`): the framework raises SIMM_ALARM before the SIOL read while mca's C raises it after, so with `SIMS=INVALID` and a broken SIOL C publishes `STAT=LINK_ALARM` and we publish `STAT=SIMM_ALARM`; closing it needs a new per-record ordering hook on `Record` plus its consumption at this raise site, and processing.rs was fenced to fix-rec mid-integration.
- **SIMM read through `&dyn Record`** — `crates/epics-base-rs/src/server/recgbl/simm.rs:93`: the dispatched dual-storage defect could not be reproduced — `resolve_sim_mode`'s `get_field("SIMM")` reads exactly what `rec_gbl_get_simm`'s `put_field_internal("SIMM", …)` writes, and `McaRecord` round-trips both through `self.simm` (`crates/mca-rs/src/record/mod.rs:658` and `:748`) — so I stopped on the finding rather than redefining it into something smaller.
- **OLDSIMM on the `.db` load path** — `crates/epics-base-rs/src/server/database/mod.rs:1579`: `add_loaded_record` never calls `rec_gbl_init_simm`, so a record loaded from `.db` starts with OLDSIMM uninitialised; not fixed here because fix-db's `f30a46ba` already adds that call and a second commit would duplicate it.
- **Capture-flush readback staleness** — `crates/ad-core-rs/src/plugin/file_controller.rs:693` (C `NDPluginFile.cpp:316` and `:321`): a failed `WriteFile` returns `error_updates()`, which publishes WriteStatus and WriteMessage but never FILE_NUMBER or NUM_CAPTURED, so NDFileNumber_RBV and NDFileNumCaptured_RBV keep their pre-flush values and the operator sees NumCaptured_RBV claiming 3 while the buffer holds 2; found while answering the R-1 divergence questions, never dispatched, and left open under the discovery freeze.
- **Modbus first-read bypasses the MBAP accumulator** — `crates/modbus-rs/src/driver.rs:690`: the first read of every transaction goes through `SyncIoTransport::write_read` rather than `read_frame`, so a 259-byte reply split 140/119 never reaches `MbapAccumulator` and the cycle ends `MalformedResponse` with `io_status=Error` (measured by fix-asyn on their tree; the `write_read` symbol does not exist on mine); neither panel fixed it unilaterally because the resolution belongs after the two modbus branches merge.
- **Two false C citations in asyn-rs** — `asynManager.c:1630` (real `cancelRequest` at `:1632`) and `drvAsynIPPort.c:741-743`/`:615-617` (real Pollmsec computation at `:775-777` and `:649-651`): both predate this round and live in fix-asyn's crate, so I verified and handed them over rather than editing another panel's files; fix-asyn landed them as `a1c83dac` and `df1b83ca`, which must be in the PR for these to be closed.
- **flush_capture deviates from C by design** — `crates/ad-core-rs/src/plugin/file_base.rs:1` (C `NDPluginFile.cpp:325`, `asynNDArrayDriver.cpp:216-219` and `:256-259`): the port keeps unwritten frames for a retry where C calls `freeCaptureBuffer()` unconditionally, and advances NDFileNumber once per completed file where C burns it once per attempt at name-creation time; byte parity is therefore not achieved and will not be, the choice is recorded in the module doc by `ee58f03b`, and it is listed here so the PR states it rather than implying parity.
- **"partial file" claim in the deviation doc is an inference** — `crates/ad-core-rs/src/plugin/file_base.rs:1`: the module doc says C leaves a partial file at the failed number, which follows from `openFileBase` having succeeded before `writeFile` failed but was never measured against a real TIFF/JPEG/HDF5 writer, so a writer that flushes nothing would leave a zero-byte file instead.
- **Wide C citation ranges verified only at their anchor** — `throttleRecord.c:517-600`, `:231-312`, `:540-600`, `:618-656`, `:629-655`, `tableRecord.c:2306-2352`, `:2318-2352`, `PVAttribute.cpp:224-249`, `:257-282`: all 64 C citations this branch adds were opened and every anchor line resolves to the construct its prose names, but for these ranges (up to 83 lines) the semantic claims about the range as a whole were not re-derived line by line.
- **Two load-dependent workspace test failures** — `crates/epics-tools-rs/tests/procserv_e2e.rs:1145` and `crates/epics-oracle-rs/tests/oracle.rs:78`: both are wall-clock deadline assertions that failed in two of four full-suite runs at the identical clean commit `fffbf362` while the box carried nine panels at load 103-190 on 96 cores, and the two red runs had different failure sets; neither crate's test was touched here and neither was fixed, because widening a timeout to make a run green is not a fix.


Branch: `caucus/VS754JQF2H/fix-ad-9c1f458a-1` (36 commits)

### fix-infra

- **iocsh unregistered command is an error** — `crates/epics-base-rs/src/server/iocsh/mod.rs:491`: a registry miss returns `Err`, but C `iocsh.cpp:1307-1310` only `showError`s to stderr and sets neither `scope.errored` nor `ret`, so C runs the next line; not closed because the branch freeze arrived before the first edit.
- **A failed startup line aborts the boot** — `crates/epics-base-rs/src/server/ioc_app.rs:747`: `run_script`'s final `Err` is mapped to `CaError` and the IOC never serves, so one `dblsr` line in `st.cmd` ends the boot where C returns 0.
- **46 db* commands are unregistered** — `crates/epics-base-rs/src/server/iocsh/commands.rs:12`: `register_builtins` installs 12 of the 55 `db*` commands C registers in `dbIocRegister.c` + `dbStaticIocRegister.c` (missing: dba dbap dbb dbc dbcar dbCreateAlias dbd dbDumpBreaktable dbDumpDevice dbDumpDriver dbDumpField dbDumpFunction dbDumpLink dbDumpMenu dbDumpPath dbDumpRecord dbDumpRecordType dbDumpRegistrar dbDumpVariable dbel dbhcr dbior dbjlr dbla dbli dbLoadDatabase dbLockShowLocked dblsr dbNotifyDump dbnr dbp dbPutAttr dbPvdDump dbPvdTableSize dbReportDeviceConfig dbs dbstat dbStateClear dbStateCreate dbStateSet dbStateShow dbStateShowAll dbtgf dbtpf dbtpn dbtr).
- **No lock sets, so dblsr/dbLockShowLocked cannot be ported** — `crates/epics-base-rs/src/server/database/field_io.rs:1559`: the port has a per-record advisory write gate instead of C's `dbLockSet` grouping, so the two cited commands have no table to report and registering them would mean inventing output C does not produce.
- **A test pins the unregistered-command defect** — `crates/epics-base-rs/src/server/iocsh/mod.rs:1244`: `test_execute_line_unknown` asserts `is_err()`, and `:2018 :2147 :2175 :2204 :2236 :2261` use `nonexistent_cmd` as their stand-in for a failing line, so all seven must be repinned to a real command failure before the C behaviour can land.
- **A cross-crate test leans on the same defect** — `crates/ad-plugins-rs/tests/ioc_asyn_commands.rs:92`: it uses `execute_line`'s `Err` to notice an unregistered `asyn*` command and loses that signal the moment the C behaviour lands.
- **PVA CLI rejects JSON5 input (put path)** — `crates/epics-pva-rs/src/client_native/ops_v2.rs:5546`: parses with `serde_json::from_str`, while C's `parseJSON` runs a handle whose default flags include `yajl_allow_json5` (`yajl.c:77`, not cleared by `parseinto.cpp:330-334`); only the emitter half was fixed this round.
- **PVA CLI rejects JSON5 input (second path)** — `crates/epics-pva-rs/src/client_native/ops_v2.rs:5654`: same converter gap on the other input route.
- **No round-trip pin between the two JSON5 owners** — `crates/epics-pva-rs/src/format.rs` (tests): no test feeds `format_json` output back through `epics_base_rs::json5::relaxed_to_strict`; it cannot be added on this branch because `crates/epics-base-rs/src/json5.rs` does not exist here (it arrives with `01588fb8` on `integration/parity-2026-08-23`).
- **relaxed_to_strict cannot read the emitter's full dialect** — `crates/epics-base-rs/src/json5.rs:90-92` (on `integration/parity-2026-08-23`): its own doc excludes `Infinity`/`NaN`, and string interiors pass through untouched, so `yajl_gen_double`'s `NaN`/`+Infinity`/`-Infinity` (`yajl_gen.c:222-247`) and `yajl_string_encode`'s `\0`/`\v`/`\xNN` (`yajl_encode.c:31-95`) still fail; widening it belongs to fix-db, not to me.
- **CA-side Duration::MAX reach is unverified** — `crates/epics-ca-rs/src/estdlib.rs:91`: `duration_from_secs` maps `1e300`/`inf`/`NaN` to `Duration::MAX` (pinned at `:240-242`), the same shape `d0a16dc4` closed in libcom, but whether any consumer reaches an unchecked `Instant + Duration` was never established; my earlier citation of `client/transport.rs:541,580,592,613` and `client/mod.rs:2357` is withdrawn as wrong.
- **Private try_from_secs_f64 guards bypass the one owner** — `crates/epics-base-rs/src/server/records/histogram.rs:441`: four more at `crates/epics-ca-rs/src/estdlib.rs:92`, `crates/asyn-rs/src/user.rs:29`, `crates/asyn-rs/src/interpose/delay.rs:19`, `crates/epics-pva-rs/src/cli.rs:58`, each choosing its own fallback instead of routing through `runtime::time::duration_from_secs`.
- **CommandResult has no silent-diagnostic channel** — `crates/epics-base-rs/src/server/iocsh/registry.rs:39`: `Result<CommandOutcome, String>` conflates "print this" with "the line failed", which is the structural reason an unregistered command had to be an error at all.
- **epicsThreadSleep policy uncompared** — `crates/epics-base-rs/src/server/iocsh/core_commands.rs:352`: the command's blocking/clamping behaviour was never checked against C `epicsThreadSleep`.
- **iocshRun's `;` split ignores quoting** — `crates/epics-base-rs/src/server/iocsh/mod.rs:392`: `cmds.split(';')` cuts inside a quoted argument, so a `;` in a string becomes a command boundary.
- **No path argument type and no filename completion** — `crates/epics-base-rs/src/server/iocsh/registry.rs:9`: `ArgType` is `String|Int|Double` with no analogue of C `iocshArgStringPath`, so the shell offers no filesystem TAB completion.
- **pvaLinkNWorkers is a command, not a variable** — `crates/epics-bridge-rs/src/pvalink/iocsh.rs:335`: pvxs registers it as an IOC variable, so `var pvaLinkNWorkers` does not work the way a site script expects.
- **astac not-found text uncompared** — `crates/epics-base-rs/src/server/iocsh/access_commands.rs:465`: `astac: record '…' not found` was never checked against C's wording.
- **dbCreateRecord missing-type text uncompared** — `crates/epics-base-rs/src/server/iocsh/commands.rs:227`: the diagnostic for an absent record type was never checked against C's wording.
- **dbpr field walk never started** — `crates/epics-base-rs/src/server/iocsh/commands.rs:643`: R8-2, the per-interest-level field enumeration C's `dbpr` performs, is not implemented.
- **run_script's C-parity comment is false and its behaviour diverges** — `crates/epics-base-rs/src/server/iocsh/mod.rs:537`: the comment claims `iocshSetError` propagates a non-zero exit status, but C assigns `ret` only at `iocsh.cpp:1133`/`:1138` under Break/Halt, so C returns 0 from a script whose commands failed while the port returns `Err` and kills the boot; the comment is pre-existing and correcting the behaviour is its own finding.
- **pvULong above i64::MAX prints differently from C** — `crates/epics-pva-rs/src/format.rs:2091`: `jprint.cpp:110-205` routes every non-double scalar through `yajl_gen_integer`, which takes an `int64`, so C prints a negative number where we print the true unsigned value; matching C means copying a C bug and needs a ruling.
- **Unknown-option text is clap's, not C's** — `crates/epics-pva-rs/src/cli.rs:1`: `b00e5ad8` matched C's exit code only; C prints `Unrecognized option: '-Z'. ('pvget -h' for help.)`.
- **The exec-backend deadline fix is unverified on target** — `crates/epics-libcom-rs/src/runtime/time.rs:31`: `deadline_after` was tested on Linux only, so the RTEMS/VxWorks panic it was filed against (`field(HIGH,'1e300')` then caput) is proven fixed in arithmetic but not on hardware.
- **The 24-column sentinel claim rests on source, not a live run** — `crates/epics-pva-rs/src/format.rs:928`: the `<undefined>` field width is taken from `printer.cpp:134` and `testprinter.cpp:139` and six in-tree expectations were repinned to it, but no live `pvget` against a C IOC was run to confirm the byte column.
- **One workspace test failure was never explained** — `crates/epics-oracle-rs/tests/oracle.rs:78`: `the_pair_boots_on_distinct_ports_and_both_serve_the_record` failed once on a 15 s boot deadline under full-suite load and passed isolated (0.755 s) and on a second full-suite run (10472/10472), so it is unreproduced rather than proven a load flake.


Branch: `caucus/VS754JQF2H/fix-infra-d7d786b7-1` (44 commits)

### fix-oracle

- **oracle boot probe timeout** — `epics-oracle-rs/src/ioc.rs:62`: `REACHABLE_TIMEOUT` is a fixed 15 s, which a box at load 100–255 exceeds so the IOC boot probe dies with `BootError` (idle cost of the same probes is 0.78–4.88 s), and it blocked three tests in three of four workspace runs; not fixed because widening a timeout to hide a flake is the wrong instrument and the fix belongs to whoever owns the probe.
- **case id collides read with monitor** — `epics-oracle-rs/src/report.rs` `CaseResult::id()`: it renders `record_type.field[class]`, so a read case and a monitor case on `ai.VAL` share the id `ai.VAL` in both the JSON and the human report; distinct from F7, which is about rendering the given class rather than inferring a phase, so it was not closed with it.
- **oracle binary exit code unobserved** — `epics-oracle-rs` `verdict_exit`: the binary's own exit code is never asserted anywhere, only `run_failures`/`exit_status` are covered by library tests, so the harness's outermost contract is untested.
- **four over-length commit bodies** — commits `ae481936`, `35762dbe`, `ba80e575`, `27d9c135`: each exceeds the 2–4 line body rule (one is multi-paragraph, three are five lines), and correcting them requires rewriting non-HEAD commits, which the standing no-rebase rule prohibits.
- **oracle README PVA line stale** — `epics-oracle-rs/README.md:121`: `- **PVA.** CA only.` is false now that the crate has `pvaread` and `pvamonitor` phases; no commit of mine caused it and the mandate's README instruction named only the array phase, so I left it for the lead.
- **F5 configuration-parity half has no fails-first test** — `epics-base-rs/src/server/db_loader/mod.rs:1381`: the crate registers its own `asyn` stub, so on this box the bare `IocBuilder` and `port_ioc_builder()` both report 40/40 covered with `UNIMPLEMENTED=[]` and no test can fail first; the test I added is only a guard against that ceasing to be true.
- **fat IOC never booted** — `~/work/oracle-ioc`: I confirmed the fat dbd and both fat-IOC paths exist as files but booted neither, because running the oracle end to end against a live C softIoc is the lead's call and needs ground truth on this box.
- **FINDINGS record-type count unverified** — `epics-oracle-rs/FINDINGS.md:26`: the 34-record-type figure is UNVERIFIED because `74096a5b` moved `probe_supported_record_types` from a bare `IocBuilder` to `port_ioc_builder`, so the IOC configuration measured then differs from the one measured now.
- **FINDINGS CA-observable field count unverified** — `epics-oracle-rs/FINDINGS.md:27`: the 2551 figure is UNVERIFIED because the same path counts 2553 today and the dbd the original run read no longer exists, making the delta an attribution to `c9817fa59` (`bi.AFTC`, `bi.AFVL`) rather than a measurement.
- **seven further FINDINGS figures unverified** — `epics-oracle-rs/FINDINGS.md:4`: the remaining seven UNVERIFIED marks (nine in total) still stand, and every units/precision/graphic/alarm number for calc, calcout, sub, seq and aSub arg fields needs re-measuring after `5496b149`, with the allowlist rows that masked those deviations now STALE candidates that will fail the run.
- **published "Six cases" has no derivation** — `epics-oracle-rs/FINDINGS.md`: I could not reconstruct what the figure counted, so I marked it UNVERIFIED and unreconciled rather than guessing a derivation.
- **expected-deviations section header contradicts its row** — `epics-oracle-rs/expected-deviations.toml:306-316`: the section is headed "REPRODUCED entries … (disabled)" while its only row, CBUG-F12, is `bucket = "NOT-REPRODUCED"` and `enabled = true`; same defect family as the false-comment sweep but not a FINDINGS figure, so whether the header moves or the row does is the lead's call.
- **false parity comments left in place** — `/tmp/.../scratchpad/false-parity-comments.md`: all 7 FALSE sites and 17 stale CBUG entries are unedited per the lead's explicit no-edit instruction, and some FALSE rows may already be repaired on the fix branches that `main` lacks.
- **transform.VERS literal** — `epics-base-rs` transform record: its `VERS` should serve literal 2 and does not; held by fix-base, and `8869e936` supplied only the record-level `PREC` half.
- **throttle.DLY precision** — throttle record: `DLY` should take `DPREC`; held by fix-rec, with record-level `PREC` and `VAL`→`HOPR`/`LOPR` now supplied but the `DPREC` route still open.
- **asyn.TMOT literal** — asyn record: `TMOT` should serve literal 4; held by fix-asyn, and since `asyn` declares no `PREC` field `8869e936` changed nothing for it.
- **default_property_support NUMERIC fallback** — `epics-base-rs/src/server/record/record_trait.rs` `default_property_support`: the `_ => P::NUMERIC` arm is correct for `mca`, the only type that reaches it today, but it will silently declare five slots for the next un-transcribed record type.
- **motor units override list unverified** — `motor-rs/src/record/field_access.rs` `field_metadata_override`: its literal units window (`"rev/sec"`, `"steps/rev"`, `EGU + "/rev"`) was never re-checked against `motorRecord.cc`, and because it answers every field my `_ => true` arm cannot reach it, so a mismatch there would be invisible from `epics-base-rs`.
- **CBUG-G1 doc claim now false of the port** — `doc/upstream-c-bugs.md:1930`: it asserts `busy.HIGH` serves 0, which `04a2ce15` makes false of the port as well as C; unedited because nine branches were live and that document belongs to whoever holds CBUG-G1.
- **mca parity comment row stale** — `mca-rs/src/record/mod.rs:831-832`: the comment was false when written and became true as of `8869e936`, so its row in `false-parity-comments.md` should be struck rather than fixed; unedited for the same ownership reason.
- **codec bypasses the supply mask** — `epics-ca-rs` `codec.rs`: five raw `snapshot.display` / `snapshot.control` reads go around the per-field supply mask, so a slot the record does not supply can still reach the wire; left open awaiting the lead's ruling on who lands it.
- **link-backed metadata is cached, not read live (X2)** — `epics-base-rs/src/server/database/links.rs:1176` `refresh_link_backed_metadata`: C re-reads the target on every request (`calcRecord.c:169-182`, `:184-203`, `:205-233`, `:257-280` via `dbDbGetOptionLoopSafe`) while the port refreshes only at `ioc_init` and at the head of each process cycle, so a Passive calc keeps serving the target's old EGU/PREC/HOPR/LOPR forever; not started because the structural fix is a BREAKING `snapshot_for_field` signature across 134 call sites and four crates on an already-merged branch, and I put that cost question to the lead unanswered.
- **head-of-cycle refresh comment is false** — `epics-base-rs/src/server/database/processing.rs:1632`: the comment claims the head-of-cycle refresh "is what makes a runtime change to the target's EGU/PREC/HOPR reach the source's clients", which is false for a Passive source, i.e. the normal calc case; held back deliberately so it can land inside X2's single commit rather than splitting one finding across two.
- **graphic_limit_fields motor arm is dead** — `epics-base-rs/src/server/record/record_trait.rs:650`: the `"motor"` arm never decides anything because `apply_field_metadata_override` runs after `route_field_metadata` and overwrites all 135 served motor fields (proved by deleting the arm and diffing the dumps byte for byte); kept on purpose under the lead's ratification, with `e31c350c` now pinning the ordering it depends on.
- **control_limit_source motor arm is dead** — `epics-base-rs/src/server/record/record_trait.rs:677`: dead for exactly the same reason and measured the same way, kept for the same ratified reason.
- **control limits are not routed through links** — `epics-base-rs/src/server/record/record_instance.rs:4736`: the control slot deliberately does not follow a link because `dbGetControlLimits` has zero callers anywhere in base and `aSubRecord.c:372-376` is a bare `recGblGetControlDouble` with no link branch, so anyone who later "completes" this routing will depart from C.
- **motor four-arm finding closed with pins, not a fix** — commit `606ccf07`: the dispatched finding did not reproduce (all four arms are already correct at `motor-rs/src/record/field_access.rs:1922-1953`), so the commit pins the behaviour instead of changing it; if the lead's C reading differs on any arm, that arm and its span need naming and I will re-measure.
- **pre-existing C citations across the tree unaudited** — repo-wide: this round's sweep covered only the 91 citations my branch authored, and roughly 250 further `motorRecord.cc` citations plus every other pre-existing C span in `motor-rs`, `asyn-rs` and the docs were never checked for the same off-by-one and wrong-function defects.
- **one citation unverifiable locally** — `calcRecord.dbd.pod:792-980`: the sweep could not resolve a local file for it, so its span is the single branch-authored citation I state as unchecked rather than correct.
- **procserv e2e fixed-budget wait** — `epics-tools-rs/tests/procserv_e2e.rs:1145`: `toggle_into_oneshot_grants_one_more_run` waits for the procServ shutdown banner on a fixed budget and under load reads only as far as `@@@ Got a kill command` before failing (15.1 s failing versus 0.069 s passing); my branch changes zero files in that crate so it is not my defect, but it blocked my gate twice and has no owner.
- **full-workspace gate owed for the last three commits** — branch tip `aeead035`: `d907467c`, `a1ee3fac` and `aeead035` were gated at `-p epics-base-rs -p mca-rs -p optics-rs` only, which is correct for comment-only changes but leaves the pre-push `--workspace` clippy and nextest pass still owed on this branch.


Branch: `caucus/VS754JQF2H/fix-oracle-01371e1d-1` (36 commits)

## Method notes for the next round

- Reviewer panels read the checked-out tree, which stays at the merge
  base while fixers advance, so by the mid-rounds a large share of
  reviewer findings are already closed on some branch. Check
  `git log <base>..<fixer-branch> -- <cited-file>` before dispatching —
  including before ruling that a panel should do *nothing*, which is the
  ownership ruling that feels safe enough to skip the check.
- A test failing on branch A is evidence about branch A, not about
  whether the fix exists anywhere.
- Integration cost was measured before merging by accumulating
  `git merge-tree --write-tree` through `git commit-tree`, with no
  worktree touched.
- Isolated re-runs lie about flake rates. One case measured 0/25 failures
  in isolation and 4/12 under the full three-crate suite; every rate in
  this round was measured with the same full-suite instrument at both the
  candidate and the baseline commit.
- Two branches widening the same signature is more often convergence than
  duplication. Count the symbol's call sites on *base*: if the second
  branch's sites did not exist there, it is extending the seam, not
  re-fixing the defect.
