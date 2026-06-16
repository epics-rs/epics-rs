# Workspace C-parity review — 2026-06-16 (round 4: commit-history-classified families)

Methodology note: rounds 1–3 were static C-vs-Rust source sweeps (now
exhausted/closed). This round classifies the **commit history** since
`v0.15.0` (2411 commits, 1755 `fix`/`feat`) into the *recurring* structural
families — the empirically-proven bug families, the same basis the
`examples/regression-ioc` harness was built on — and cross-references each
family's coverage status to find which still have **open siblings**. A
targeted code hunt (4 parallel read-only agents) confirmed the open siblings
at file:line.

## Parity philosophy (scope filter — unchanged)

Only OUTPUT FORM must match C/pvxs (wire/DBR/PVA encodings, on-disk dataset
shape, observable record-field values incl. DBE masks). Internal design may
differ if observably equivalent. A divergence counts ONLY if observably
different AND reachable.

## Recurring-family classification (commit-history theme frequency vs coverage)

| Recurring family | history commits | coverage status |
|---|---|---|
| disconnect / teardown | 41 | trap-write `AfterWrite` on cancel/supersede/teardown **FIXED (R4-4, 02053a3d)** |
| String / type-drop | 34 | common-field FIXED (49942183), P-array FIXED; compress INP-link **FIXED (R4-3, f388beab)** |
| timestamp / utag | 33 | pinned (regr H), utag FIXED (regr Q) — closed |
| init / load order | 30 | pinned (regr O), common-field-load FIXED — closed |
| flow-control credit | 30 | audited sound (pipeline credit + dropped-monitor owner) — closed |
| link locality / PP | 28 | non-local Db-link family closed — closed |
| numeric convert | 26 | ADP-56 signed off; QSRV String→int radix **FIXED (R4-2, 90167988)** |
| monitor DBE-mask | 24 | pinned (regr I/M), SCAL-1/2 closed — closed |
| menu / enum label | 16 | enum *serving* pinned (regr E/F/R); db-load label resolution **FIXED (R4-1, e7e20583)**; runtime DBR_STRING-write label resolution **FIXED (R4-5, 86137c83)** |
| alarm / severity | 16 | pinned (regr G/N) — closed |
| array / waveform | 15 | P-family FIXED — closed |

7 of 11 families fully covered; the 4 with open siblings (R4-1..R4-4 below) are now **all FIXED** (R4-1 e7e20583, R4-2 90167988, R4-3 f388beab, R4-4 02053a3d; plus the R4-5 write-path sibling 86137c83).

## Open Findings

### R4-1: db-load menu-label resolution uses a global table, not the field's own menu — 12 menus drop, sel `SELM` mis-maps
Severity: High — **FIXED (e7e20583)**

Root cause (structural): `db_loader::apply_fields` (`crates/epics-base-rs/src/server/db_loader/mod.rs:972-979`) parses a field-list menu field's `.db` value with `EpicsValue::parse(desc.dbf_type, value_str)`, whose only label path is the hand-maintained global `resolve_menu_string` (`crates/epics-base-rs/src/types/value.rs:1075-1142`). It ignores the record's own complete, correctly-ordered `menu_field_choices` table (`record_trait.rs:339`, per-record overrides). A menu label is **menu-specific**, so a single global table is fundamentally wrong: it (a) drops labels it doesn't know → the record load errors out, and (b) mis-maps shared labels to the wrong index.

Confirmed drops (`field(<field>, "<Label>")` fails the record load), each with the canonical labels living in an *existing* record constant the loader ignores:
- ai/ao `LINR` (menuConvert, 15 labels: `NO CONVERSION`/`SLOPE`/`LINEAR`/typeK… ) — `ai.rs:137`, `ao.rs:258`
- compress `ALG` (compressALG, 6) — `compress.rs:351`, `COMPRESS_ALG_CHOICES`
- compress `BALG` (bufferingALG, 2) — `compress.rs:376`, `COMPRESS_BALG_CHOICES`
- histogram `CMD` (histogramCMD, 4: Read/Clear/Start/Stop) — `histogram.rs:181`
- ao `OIF` (aoOIF, 2: Full/Incremental) — `ao.rs:308`, `AO_OIF_CHOICES`
- swait `OOPT` missing `Never` (7th synApps choice) — `swait.rs:121`
- swait `DOPT` (Use VAL/Use DOL) — `swait.rs:128`, `SWAIT_DOPT_CHOICES`
- scalcout `OOPT` missing `Never` — `scalcout.rs:182`
- transform `COPT` (Conditional/Always) — `transform.rs:109`
- transform `IVLA` (Ignore error/Do Nothing) — `transform.rs:115`
- sseq `WAIT1..WAITA` (sseqWAIT, 12: NoWait/Wait/After1..AfterA) — `sseq.rs:870`

Confirmed mis-map (worse — silent wrong value, no error):
- sel `SELM "Specified"` → `resolve_menu_string` returns **1** (from the menuFanout `All/Specified/Mask` block, `value.rs:1111`), but selSELM order is `Specified(0)/High Signal(1)/Low Signal(2)/Median Signal(3)` (`sel.rs:11` `SELM_CHOICES`). So `field(SELM,"Specified")` loads SELM=1 ("High Signal"). `sel.rs:125`.

C ref: dbStaticLib resolves a menu field's `.db` value against that field's own `menuFtype`/record menu (`/Users/stevek/codes/epics-base/modules/database/src/ioc/dbStatic/dbStaticLib.c` `dbPutString`/`dbGetMenuIndexFromString`), never a cross-menu global table.

Structural fix (done, e7e20583): `apply_fields`, when `record.menu_field_choices(field).or_else(shared_menu_choices)` is `Some(choices)`, resolves the value against the field's own menu — exact label first then a numeric index (matching C order; `dbGetMenuIndexFromString`'s `strcmp` precedes `epicsParseUInt16`) via the new shared `resolve_menu_field_string` (`record/menu_choices.rs`) — instead of `EpicsValue::parse`'s global table. An unknown choice errors (`S_db_badChoice`) instead of mis-mapping. No record edits were needed: every affected record's `menu_field_choices` was already complete. Pinned by `db_loader::tests::db_load_menu_labels_resolve_against_field_menu`.

### R4-2: QSRV PVA String→integer PUT is base-10-only; pvxs uses base-0 (hex/octal)
Severity: Medium — **FIXED (90167988)**
Fix: the two `scalar_to_i64`/`scalar_to_u64` String branches route through the CA path's existing base-0 parsers (`EpicsValue::parse_int`/`parse_uint`, now public). Empty/garbage/out-of-range still reject (pvxs parity); only the accepted set widens to hex/octal. Pinned by `convert::tests::string_to_numeric_put_accepts_c_radix`.
Rust: `crates/epics-bridge-rs/src/convert.rs:229-237` (`scalar_to_i64`), `:264-272` (`scalar_to_u64`) — a `ScalarValue::String` is parsed `s.trim().parse::<i64>()/<u64>()` (decimal-only), rejecting `0x`/octal. Reached from `crates/epics-bridge-rs/src/qsrv/channel.rs:646-651`.
C ref: pvxs `src/util.cpp:786-817` `parseTo<int64_t/uint64_t>` use `std::stoll/stoull(s,&idx,0)` — base-0 (auto hex/octal).
Reachability: `pvput PV "0x1F"` into a numeric field → pvxs writes 31; Rust QSRV returns `PutRejected`. The convert.rs comment already claims pvxs parity, and the CA sibling (`value.rs:1210` `parse_int`/`parse_uint`) already does C-radix — the bridge path is inconsistent with both.

### R4-3: compress `INP`-link delivery drops a non-Double linked source
Severity: Medium — **FIXED (f388beab)**
Fix: `put_field_internal` (the single ReadDbLink-delivery owner) coerces the delivered value to the target field's `dbf_type` (from `field_list`), mirroring C `dbGetLink(DBF_<target>)`. sseq's `put_field_internal` override is the distinct owner for its own targets. Pinned by `compress::pbuf_tests::input_link_coerces_long_source_into_double_buffer`.
Rust: input-link read delivery `crates/epics-base-rs/src/server/database/processing.rs:2730,2785` (`let _ = instance.record.put_field_internal(target, value)` — error discarded); `put_field_internal` (`record_trait.rs:896-908`) coerces only `EnumWithChoices`, not to the target field's DBF type; compress `VAL` arm accepts only Double/DoubleArray (`compress.rs:517-536`).
C ref: `compressRecord.c:342` `dbGetLink(&prec->inp, DBF_DOUBLE, …)` — C requests DBF_DOUBLE, so the link layer converts any numeric/string source to double before the record sees it.
Reachability: a compress record with `field(INP,"SRC.VAL")` where SRC is DBF_LONG (longin/calc) or waveform FTVL=LONG delivers `EpicsValue::Long(Array)` → VAL arm `_ => Err(TypeMismatch)` → discarded → buffer never advances, VAL stays zero. CA/PVA/OUT-link write paths are SAFE (they coerce via `field_io.rs:643-667` / `:101-127`); sseq's two `ReadDbLink` targets are covered by its own override. Structural fix: `put_field_internal` should coerce to the target field's `dbf_type` from `field_list()` (same pattern `put_pv_inner` uses), closing every `ReadDbLink` target by construction.

### R4-4: trapped put-callback `AfterWrite` finalizer skipped on abort/supersede/teardown (3 sites, one family)
Severity: Medium — **FIXED (02053a3d)**
Invariant (MUST): once `asTrapWrite`/`BeforeWrite` is dispatched for a trapped put, exactly one `AfterWrite` MUST follow on every exit path (completion, async-abort, supersede, teardown).
Rust bypassing paths (no RAII finalizer — `AfterWrite` is a plain await-sequenced statement an `abort()` cuts):
1. PVA QSRV blocking put — `crates/epics-bridge-rs/src/qsrv/trap_write.rs:112-118` (Before at :112, `write().await` parks at `channel.rs:701 rx.await`, After at :118); the PUT-EXEC task's `AbortOnDrop` (`epics-pva-rs/src/server_native/tcp.rs:1053`) cuts it on DestroyChannel/teardown. Group path `qsrv/group.rs:1189` same shape.
2. CA server supersede — `crates/epics-ca-rs/src/server/tcp.rs:330` `supersede_put_notify` aborts prev + sends only `ECA_PUTCBINPROG`, never the superseded `AfterWrite`. C fires `asTrapWriteAfter` before that reply (`camessage.c:1697-1701`).
3. CA server connection teardown — `crates/epics-ca-rs/src/server/tcp.rs:3429-3479` dispatches `AfterWrite` only after `rx.await`; connection-drop abort (`tcp.rs:880`) cuts the parked task. C fires it from the disconnect branch (`camessage.c:1619-1620`).
C ref: EPICS base `asTrapWriteAfter` on every cancel branch (`camessage.c:1400/1620/1700`); pvxs `SecurityLogger` RAII destructor (`ioc/securitylogger.h:28-29`).
Observable: a caPutLog/trap-write listener records `BeforeWrite` with no paired `AfterWrite` on supersede/teardown of a slow (motor/async) put — a security-audit-trail divergence vs a real C IOC. Structural fix: a scope/Drop guard owning the Before/After pair (RAII equivalent of `SecurityLogger`), one helper covering all three sites.
Fix (done): new `TrapWriteGuard` + `TrapWriteFields` in `epics-base-rs::server::access_security` — `begin()` dispatches `BeforeWrite`, `complete(status)` dispatches the normal-path `AfterWrite` and disarms, and `Drop` dispatches a cancel `AfterWrite` if still armed (the abort/supersede/teardown paths). `AfterWrite` therefore fires exactly once on every exit path by construction. The bridge `put_with_trap` holds the guard across `write().await`; the CA server moves it into the async completion task so a task `abort()` (supersede via `supersede_put_notify`, teardown via the `write_notify_tasks` drain) runs the guard's `Drop`. The three explicit Before/After dispatch sites are removed for the single owner. Pinned by `access_security::tests::trap_write_guard_{complete_fires_one_after_and_disarms_drop,drop_without_complete_fires_cancel_after}`, `qsrv::trap_write::tests::after_fires_when_write_future_cancelled`, and `server::tcp::put_notify_supersede_tests::supersede_fires_cancel_after_write_via_guard_drop`.

### R4-5: runtime `DBR_STRING` write to a `DBF_MENU` field mis-maps the label (write-path sibling of R4-1)
Severity: Medium — **FIXED (86137c83)**
Root cause (same family as R4-1): the write coercion (`field_io.rs` `put_pv_inner` :120, `put_pv_and_post_with_origin` :342, `put_record_field_from_ca_inner` :660) coerced a type-mismatched value with `EpicsValue::convert_to(target)`, whose `String→Short/Enum` path (`value.rs:932/937`) consults the field-blind global `resolve_menu_string`. A raw `ca_put(DBR_STRING,"Specified")` to `sel.SELM` thus stored 1, not 0 — the same mis-map as db-load, on the runtime path.
C ref: `dbConvert.c` `putStringMenu` (:1206-1226) resolves a DBR_STRING write against the field's own menu (`strcmp` over `papChoiceValue`, then `epicsParseUInt16(dbConvertBase)`).
Reachability: narrow but valid — standard clients (`caput-rs`, `caput`, pyepics) resolve enum labels client-side and send an index, so this is reached only by a client that deliberately sends `DBR_STRING` with a label. Still observably divergent vs C, which resolves it correctly.
Fix (done): the three sites share `coerce_write_value`, which resolves a String write to a menu field against the field's own menu (`menu_field_choices`/`shared_menu_choices` → `resolve_menu_field_string`) before falling through to `convert_to`. Pinned by `field_io::tests::write_path_menu_label_resolves_against_field_menu`.

## Audited clean this round (no finding)
- Runtime record-field type-drop via CA/PVA/OUT-link: PREVENTED by write-path coercion (`field_io.rs:643-667`, `:101-127`); the ~93 typed-only `put_field` arms are unreachable with a mismatched type via those vectors. Only the INPUT-link sub-path (R4-3) lacks coercion. (The menu-*label* sub-case of this coercion — distinct from type-drop — was the R4-5 sibling, now fixed.)
- PVA monitor pipeline-credit accounting, CA base dropped-monitor counter, bridge connection/pause-vote state machines: single-owner accounting sound, finalizers cover exits.
- CA String→numeric radix (`value.rs:1163-1259`), integer narrowing, DBF_CHAR signedness, old-DBR promotions: match C `dbConvert`/pvxs `copyOut`. (R4-2 is the *bridge* path only.)

## Round 5 Open Findings (record-processing field-output, 2026-06-16)

Fresh-angle audit of `epics-base-rs/src/server/records/*` `process()` vs C
`std/rec/*Record.c`, on the OBSERVABLE axes: processed field values, alarm
severity/status, DBE monitor masks, UDF/INVALID. R5-1..R5-3 verified at the
C+Rust source by me; R5-4..R5-14 reported by sub-agents (C-cited, pending
independent re-verification before fix).

### R5-1 (CRITICAL, VERIFIED): `sub`/`aSub` subroutine never runs on the normal process path
`SubRecord::process`/`ASubRecord::process` are empty (`sub_record.rs:148`, `asub_record.rs:213`); they rely on `RecordInstance::subroutine`, which is invoked ONLY in `process_local()` (`record_instance.rs:1903`). The main engine `process_record_with_links_inner` calls `instance.record.process()` directly (`processing.rs:1619`) and bypasses `process_local` (its own comment, `processing.rs:1629-1632`). So sub/aSub run their SNAM only via the by-name/QSRV-group path (`processing.rs:322`), never on SCAN / event / CA-put-to-PP-INP / FLNK. C runs `do_sub()` every `process()` (`subRecord.c:147`, `aSubRecord.c:223`). Observable: VAL/VALA..VALU and OUTA..OUTU never update. Fix needs a design call (subroutine ownership: move onto the record, or invoke `instance.subroutine` from the main engine for sub/aSub). Cascading once it runs: `SubroutineFn` return-status (`subRecord.c:430` SOFT_ALARM, `aSubRecord.c:223` `val=status`) and sub HIHI/HIGH/LOLO/LOW/HYST/MDEL/ADEL limit-alarm fields are absent.

### R5-2 (HIGH, VERIFIED, FIXED e4ae8906): calc/calcout `VAL` token in CALC/OCAL always reads 0
`calc.rs:674`/`calcout.rs` build `NumericInputs::with_vars(vars)` (`prev_val: 0.0`, `engine/mod.rs:100`) and never assign `inputs.prev_val = self.val`; the engine `FetchVal` reads `prev_val` (`engine/numeric.rs:37`). C `calcPerform.c:73-74` `FETCH_VAL: *++ptop = *presult` reads the previous VAL. Trigger: `CALC="VAL+1"` → C 1,2,3…; Rust 1,1,1…. Same `with_vars(prev_val=0)` pattern in swait/scalcout/transform (synApps crate, out of base scope — note as sibling).
**FIXED:** seed `prev_val` before each eval — calc.rs cached-RPCL + fallback paths seed `self.val`; calcout.rs CALC seeds `self.val`, OCAL seeds `self.oval` (C `presult = &oval`, calcoutRecord.c:621). Regression tests assert `VAL+1`/OCAL `VAL+1` count up. swait/scalcout/transform sibling in the synApps crate remains OPEN (out of base-crate scope).

### R5-3 (HIGH, VERIFIED, FIXED 207f5484): fanout/seq SHFT default is 0; C dbd `initial("-1")`
`fanout.rs:93`/`seq.rs:173` default `shft: 0`; C `fanoutRecord.dbd.pod:133`/`seqRecord.dbd.pod:287` `field(SHFT,DBF_SHORT){ initial("-1") }` (POD: "If not set, SHFT is -1 so bits shift left by 1"). Rust db_loader applies no dbd `initial()`, so the struct `Default` is the only source. Trigger: `SELM=Mask`, SHFT omitted → C `seln<<1` (SELN=1 → LNK1), Rust `seln>>0` (SELN=1 → LNK0): wrong forward links fire.
**FIXED:** fanout/seq `Default` now `shft: -1`. `select_link_indices_ex` already implements the signed shift (`shft>=0 ? seln>>shft : seln<<-shft`, mod.rs:392-396), so only the default was wrong. mbbi/mbbo/mbbi_direct/mbbo_direct SHFT have NO dbd `initial()` (default 0) → their `shft: 0` already matches, not changed. Tests assert the -1 default + SELN=1 selects slot 1 under Mask.
**STILL OPEN (larger family):** ANY field whose Rust `Default` ≠ its C dbd `initial()` (db_loader ignores dbd initials) — a separate audit, tracked as **R5-15** below.

### R5-4..R5-14 (REPORTED by sub-agent, C-cited, pending re-verify)
- R5-4 (MED): ao closed-loop Incremental DOL adds to current VAL not PVAL (`processing.rs:1398` vs `aoRecord.c:442` `val=pval` then `+=`).
- R5-5 (MED): ao constant DOL + Incremental loses the increment, behaves Full (`ao.rs:455-457`).
- R5-6 (MED): calcout ODLY>0 posts VAL/OVAL monitors + FLNK on the delaying cycle AND again on the delayed cycle (`calcout.rs:1035` vs `calcoutRecord.c:276-282`).
- R5-7 (MED): sel `Specified` mode fetches ALL inputs, not only INP[SELN] (`sel.rs:663` vs `selRecord.c:411`): extra input monitors + spurious SEVR from non-selected broken links.
- R5-8 (MED): sel runs `do_sel` unconditionally; C gates on fetch success (`sel.rs:323` vs `selRecord.c:114`).
- R5-9 (MED): seq never writes DOLn read-back into DOn nor posts DOn (`links.rs:1415` vs `seqRecord.c:256-268`).
- R5-10 (MED): dfanout OUT-link write failure does not raise LINK_ALARM/MAJOR (`links.rs:1362` vs `dfanoutRecord.c:311`).
- R5-11 (LOW): ai/ao/calc/calcout aux-field posts on alarm-transition cycle use a fixed `aux_mask` rather than VAL's `monitor_mask` (over/under-notify under non-default MDEL/ADEL).
- R5-12 (LOW): ai writes `LALM=NaN` / drifts AFVL on a UDF cycle; C `aiRecord.c:319-323` sets `afvl=0`, returns early, leaves LALM.
- R5-13 (LOW): waveform/aai/aao ignore MPST/APST "On Change" and never compute HASH (default "Always" matches).
- R5-14 (LOW): seq reads SELL→SELN in SELM=All (C skips, `seqRecord.c:148`); dfanout OMSL/IVOA served as SHORT not ENUM; seq DOLn not coerced to DBR_DOUBLE.

### R5-15 (MED, STRUCTURAL, OPEN): db_loader applies no dbd `initial()`; any Rust `Default` ≠ C dbd `initial()` diverges
Surfaced by R5-3 (SHFT). The Rust port hand-codes each record's `Default` impl and never parses the C `.dbd.pod` `field(...){ initial("...") }` directives, so any field whose hand-coded default differs from its C dbd initial() ships the wrong post-load value when the `.db` file omits that field. R5-3 was one instance (SHFT). Scope of the audit: enumerate every `initial("...")` in `epics-base/modules/database/src/std/rec/*.dbd.pod` whose value is non-zero / non-empty-string, and compare against the corresponding Rust record `Default`. Structural options: (a) correct each diverging Default by hand (cheap per-field, but the dual-source drift remains for future records), or (b) drive defaults from a single dbd-initial table so Default and dbd cannot diverge. Confirm scope with the user before a table-driven rewrite (large). Only observable when a client/db omits the field AND the field is externally readable or alters processing.

### R5 audited clean (sub-agent verified, no divergence)
ai convert (RVAL+ROFF/ASLO+AOFF/LINR/ESLO+EOFF/SMOO all quadrants/HIHI-LOLO order+HYST+LALM/AFTC); ao convert (drive clamp/OROC/linearization/raw round/IVOA/RVAL+RBV masks); calc numeric domain (div/sqrt/log/mod/nint/atan2/NaN→UDF, OOPT 6 modes, DOPT, IVOA/IVOV, LA..LU); compress (all per-ALG arithmetic, ILIL/IHIL, PBUF tail, FIFO/LIFO+lin, RES); sel selection (High/Low/Median incl. even/odd index, all-NaN, out-of-range SOFT, HYST+LALM); fanout/dfanout/seq (SELM=All/Specified/Mask ordering+range, dfanout IVOA/checkAlarms/MDEL/ADEL/DOL, fanout Passive FLNK gate); waveform (UDF, NORD, subArray clamps, default MPST/APST).

## Review Log
- Round 4 (2026-06-16): commit-history classification (11 recurring families) + 4-agent code hunt. 4 families have open siblings → R4-1 (menu-label, 12 menus + mis-map; the dominant one, structural), R4-2 (QSRV radix), R4-3 (compress INP coercion), R4-4 (trap-write AfterWrite, 3 sites). The classification and the independent code hunt converged on the same open set.
- Round 5 (2026-06-16): fresh-angle record-processing field-output audit (alarm order, monitor masks, value edge cases, UDF). Found the port is NOT converged on record behaviour — R5-1 (sub/aSub subroutine inert on the normal process path, CRITICAL) + R5-2 (calc/calcout VAL token reads 0) + R5-3 (fanout/seq SHFT default) verified at source; R5-4..R5-14 reported by sub-agents pending re-verify. Contrast with the R4-4 finalizer-skip sweep, which found that family isolated — the record-output axis is where live divergences remain.
- Round 4 fixes (2026-06-16): R4-1 db-load menu-label resolution FIXED (e7e20583). The [Fixes from reported defects] enumeration of the same root (field-blind `resolve_menu_string`) surfaced a runtime-write sibling on the `field_io` coercion path → filed + FIXED as R4-5 (86137c83). Both reuse the shared `resolve_menu_field_string` helper. R4-2 QSRV String→int base-0 radix FIXED (90167988, shared `EpicsValue::parse_int`/`parse_uint`). R4-3 compress INP-link coercion FIXED (f388beab, `put_field_internal` coerces to the target field type). R4-4 trap-write AfterWrite-on-every-exit-path FIXED (02053a3d) — verifying the C reference (`camessage.c:1400/1620/1700` all call `asTrapWriteAfter`) confirmed the divergence and dictated the structural fix: a `TrapWriteGuard` RAII pair (BeforeWrite on `begin`, AfterWrite on `complete`-or-`Drop`) replacing the three explicit dispatch sites across epics-base-rs/epics-bridge-rs/epics-ca-rs. **All round-4 findings (R4-1..R4-5) now closed.**
