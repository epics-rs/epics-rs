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
| disconnect / teardown | 41 | mostly closed; **OPEN:** trap-write `AfterWrite` (R4-4, 3 sites) |
| String / type-drop | 34 | common-field FIXED (49942183), P-array FIXED; **OPEN:** compress INP-link (R4-3) |
| timestamp / utag | 33 | pinned (regr H), utag FIXED (regr Q) — closed |
| init / load order | 30 | pinned (regr O), common-field-load FIXED — closed |
| flow-control credit | 30 | audited sound (pipeline credit + dropped-monitor owner) — closed |
| link locality / PP | 28 | non-local Db-link family closed — closed |
| numeric convert | 26 | ADP-56 signed off; **OPEN:** QSRV String→int radix (R4-2) |
| monitor DBE-mask | 24 | pinned (regr I/M), SCAL-1/2 closed — closed |
| menu / enum label | 16 | enum *serving* pinned (regr E/F/R); **OPEN: db-load label resolution (R4-1, 12 menus + a mis-map)** |
| alarm / severity | 16 | pinned (regr G/N) — closed |
| array / waveform | 15 | P-family FIXED — closed |

7 of 11 families fully covered; **4 have open siblings (R4-1..R4-4 below).**

## Open Findings

### R4-1: db-load menu-label resolution uses a global table, not the field's own menu — 12 menus drop, sel `SELM` mis-maps
Severity: High — fix (structural)

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

Structural fix (planned): in `apply_fields`, when `record.menu_field_choices(field)` is `Some(choices)`, resolve the value as numeric-index-first then exact-label against `choices` (menu-specific), instead of `EpicsValue::parse`'s global table. Complete each affected record's `menu_field_choices` to cover all its menu fields (wiring the existing label constants). Closes all 12 drops + the sel mis-map + prevents future gaps by construction. One finding → one commit.

### R4-2: QSRV PVA String→integer PUT is base-10-only; pvxs uses base-0 (hex/octal)
Severity: Medium — fix
Rust: `crates/epics-bridge-rs/src/convert.rs:229-237` (`scalar_to_i64`), `:264-272` (`scalar_to_u64`) — a `ScalarValue::String` is parsed `s.trim().parse::<i64>()/<u64>()` (decimal-only), rejecting `0x`/octal. Reached from `crates/epics-bridge-rs/src/qsrv/channel.rs:646-651`.
C ref: pvxs `src/util.cpp:786-817` `parseTo<int64_t/uint64_t>` use `std::stoll/stoull(s,&idx,0)` — base-0 (auto hex/octal).
Reachability: `pvput PV "0x1F"` into a numeric field → pvxs writes 31; Rust QSRV returns `PutRejected`. The convert.rs comment already claims pvxs parity, and the CA sibling (`value.rs:1210` `parse_int`/`parse_uint`) already does C-radix — the bridge path is inconsistent with both.

### R4-3: compress `INP`-link delivery drops a non-Double linked source
Severity: Medium — fix
Rust: input-link read delivery `crates/epics-base-rs/src/server/database/processing.rs:2730,2785` (`let _ = instance.record.put_field_internal(target, value)` — error discarded); `put_field_internal` (`record_trait.rs:896-908`) coerces only `EnumWithChoices`, not to the target field's DBF type; compress `VAL` arm accepts only Double/DoubleArray (`compress.rs:517-536`).
C ref: `compressRecord.c:342` `dbGetLink(&prec->inp, DBF_DOUBLE, …)` — C requests DBF_DOUBLE, so the link layer converts any numeric/string source to double before the record sees it.
Reachability: a compress record with `field(INP,"SRC.VAL")` where SRC is DBF_LONG (longin/calc) or waveform FTVL=LONG delivers `EpicsValue::Long(Array)` → VAL arm `_ => Err(TypeMismatch)` → discarded → buffer never advances, VAL stays zero. CA/PVA/OUT-link write paths are SAFE (they coerce via `field_io.rs:643-667` / `:101-127`); sseq's two `ReadDbLink` targets are covered by its own override. Structural fix: `put_field_internal` should coerce to the target field's `dbf_type` from `field_list()` (same pattern `put_pv_inner` uses), closing every `ReadDbLink` target by construction.

### R4-4: trapped put-callback `AfterWrite` finalizer skipped on abort/supersede/teardown (3 sites, one family)
Severity: Medium — fix
Invariant (MUST): once `asTrapWrite`/`BeforeWrite` is dispatched for a trapped put, exactly one `AfterWrite` MUST follow on every exit path (completion, async-abort, supersede, teardown).
Rust bypassing paths (no RAII finalizer — `AfterWrite` is a plain await-sequenced statement an `abort()` cuts):
1. PVA QSRV blocking put — `crates/epics-bridge-rs/src/qsrv/trap_write.rs:112-118` (Before at :112, `write().await` parks at `channel.rs:701 rx.await`, After at :118); the PUT-EXEC task's `AbortOnDrop` (`epics-pva-rs/src/server_native/tcp.rs:1053`) cuts it on DestroyChannel/teardown. Group path `qsrv/group.rs:1189` same shape.
2. CA server supersede — `crates/epics-ca-rs/src/server/tcp.rs:330` `supersede_put_notify` aborts prev + sends only `ECA_PUTCBINPROG`, never the superseded `AfterWrite`. C fires `asTrapWriteAfter` before that reply (`camessage.c:1697-1701`).
3. CA server connection teardown — `crates/epics-ca-rs/src/server/tcp.rs:3429-3479` dispatches `AfterWrite` only after `rx.await`; connection-drop abort (`tcp.rs:880`) cuts the parked task. C fires it from the disconnect branch (`camessage.c:1619-1620`).
C ref: EPICS base `asTrapWriteAfter` on every cancel branch (`camessage.c:1400/1620/1700`); pvxs `SecurityLogger` RAII destructor (`ioc/securitylogger.h:28-29`).
Observable: a caPutLog/trap-write listener records `BeforeWrite` with no paired `AfterWrite` on supersede/teardown of a slow (motor/async) put — a security-audit-trail divergence vs a real C IOC. Structural fix: a scope/Drop guard owning the Before/After pair (RAII equivalent of `SecurityLogger`), one helper covering all three sites.

## Audited clean this round (no finding)
- Runtime record-field type-drop via CA/PVA/OUT-link: PREVENTED by write-path coercion (`field_io.rs:643-667`, `:101-127`); the ~93 typed-only `put_field` arms are unreachable with a mismatched type via those vectors. Only the INPUT-link sub-path (R4-3) lacks coercion.
- PVA monitor pipeline-credit accounting, CA base dropped-monitor counter, bridge connection/pause-vote state machines: single-owner accounting sound, finalizers cover exits.
- CA String→numeric radix (`value.rs:1163-1259`), integer narrowing, DBF_CHAR signedness, old-DBR promotions: match C `dbConvert`/pvxs `copyOut`. (R4-2 is the *bridge* path only.)

## Review Log
- Round 4 (2026-06-16): commit-history classification (11 recurring families) + 4-agent code hunt. 4 families have open siblings → R4-1 (menu-label, 12 menus + mis-map; the dominant one, structural), R4-2 (QSRV radix), R4-3 (compress INP coercion), R4-4 (trap-write AfterWrite, 3 sites). The classification and the independent code hunt converged on the same open set.
