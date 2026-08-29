# epics-bridge-rs Deferred/Remaining Punchlist — 2026-05-19

Driver-managed punchlist. **Worker contract:**

1. Take exactly the next unchecked `[ ]` item the driver hands you. Do NOT freelance to other items.
2. Before editing the cited line, run the **Anchor / Sites / Same defect / Distinct** audit per the global rule (see ~/.claude/CLAUDE.md "Fixes from reported defects"). Mandatory report header before edits.
3. NEVER emit: `TODO`, `FIXME`, `unimplemented!()`, `#[allow(...)]` (to silence), `// later`, "next session", "out of scope" (unless user-scoped this round), "scope이 크다", "위험합니다", "다음에", "defer".
4. **Upstream parity is the bar — no inventing semantics.** Every design decision (values, edge cases, where behaviour applies, defaults) MUST be grounded in the upstream C++ reference:
   - pvxs:       `$PVXS_HOME`
   - epics-base: `$EPICS_BASE`
   Use `rg` in those trees BEFORE making decisions. Commit message and end-of-task report MUST include an **Upstream parity** section listing `pvxs:file:line` and/or `epics-base:file:line` your implementation mirrors for each behaviour. If you cannot find the upstream reference for a sub-behaviour, STOP and report — do NOT invent.
5. Root cause fix at source. Comment-only "fix" = rejected. Type/API closure preferred over local patches when the global rule's "Invariant-driven fixes" section applies.
5. After edits: `cargo fmt --all` → `cargo clippy -p epics-bridge-rs --all-targets -- -D warnings` → `cargo nextest run -p epics-bridge-rs`. If your change crosses crate boundaries, escalate to `--workspace`. Doctest changes → `cargo test --doc -p epics-bridge-rs`.
6. Add a regression test that fails on main and passes after your fix. Name it `br_rN_<short>`.
7. End-of-task report in the format mandated by global rules: Tested / Failed / UNFIXED / Fixed.
8. Commit per item (one item = one commit). No bundled commits. No `git push` without explicit user confirmation. No `Co-Authored-By` lines.

Driver (main session) verifies after each item:
- `rg "(TODO|FIXME|unimplemented!|#\[allow|// later)"` over your diff → must be zero new hits.
- Banned phrases in your panel output → rejection + correction.
- All three cargo commands recorded as passing.
- Regression test exists and exercises the cited defect.

When all items checked, run full-workspace `cargo clippy --workspace --all-targets -- -D warnings` + `cargo nextest run --workspace` + `cargo test --doc --workspace` before reporting done.

---

## C reference pins

Every C/C++ citation in this file resolves at the tree and revision below,
not at whatever the env-var checkout holds today — those checkouts run ahead
of their pins, so a citation checked against one can be graded wrong while
being right, or graded right after drifting into a neighbouring construct.
The resolve-by-symbol rule, the shared-basename rule and the verification of
each pin are in `c-reference-pins.md`.

| tree | pinned revision | cited here |
| --- | --- | --- |
| `pvxs` | `1.5.1-42-gb568e93` | `clientconn.cpp`, `credentials.cpp`, `dataencode.cpp`, `groupconfigprocessor.cpp`, `groupsource.cpp`, `securityclient.cpp`, `servermon.cpp`, and the `ioc/pvalink*.cpp` set |
| `epics-base` | `R7.0.10` | `asLibRoutines.c`, `asLib_lex.l`, `dbLock.c` |

Every `pvalink*.cpp` exists in both `pvxs` and `pva2pva`, so the basename
alone resolves in the wrong file without failing. The pvalink items here mean
the pvxs copies under `ioc/` — `pvaGetDBFtype` and `pvaGetElements` land
exactly at `ioc/pvalink_lset.cpp:199,242` at the pin, and pva2pva's `pdbApp`
copy has no `ioc::DBManyLock` at all.

Rust `*.rs` citations are in-repo and carry no pin: they resolve at the
current worktree, not at the commit this review was written on. Where the
reviewed code has since been fixed, moved or replaced, the line names the
construct that now carries the behaviour and the sentence says so.


## Items (ordered by review-doc priority block)

### Priority 1 — ACF/identity/RPC protection

- [x] **BR-R4** — QSRV ACF adapter collapses method/authority/roles and field ASL.
  - Spec: `crates/epics-bridge-rs/doc/pvxs-functional-security-review-2026-05-18.md:153-176`
  - Done: worker B, commit `db9a79e4` on `caucus/HJB9ABPH/backend` (+ doc tracker commit `2a58c9d9` on main); regression `br_r4_acf_method_authority_roles_field_asl`. Cross-crate: also touched `epics-base-rs/src/server/access_security.rs` (quoted-string support in `read_brace_list`). Upstream parity: pvxs credentials.cpp:31-45; securityclient.cpp:25-30,42-46; epics-base asLibRoutines.c:1006; asLib_lex.l.

- [x] **BR-R21** — PVA gateway READ/MONITOR upstream authorization uses the shared cache client.
  - Spec: `crates/epics-bridge-rs/doc/pvxs-functional-security-review-2026-05-18.md:604-632`
  - Done: worker B, commit `6f9d26c6` on `caucus/HJB9ABPH/backend`; regression `br_r21_gateway_monitor_credential_scoping`. New field `upstream_caches` + `upstream_cache_for` in `GatewayChannelSource`; extracted `subscribe_raw_inner`/`subscribe_inner` helpers; overrode `get_value_checked`, `subscribe_checked`, `subscribe_raw_checked` to route through per-credential cache. Upstream parity: pvxs security-review spec "the gateway MUST NOT silently conflate per-client upstream authorization into a single shared-client authorization" (doc:604-632); mirrors `upstream_client_for`/PUT/RPC/PROCESS path (PG-G10).

### Priority 3 — Field addressing & PUT/scan processing

- [x] **BR-R11** — pvalink OUT writes do not preserve `field`, `proc`, `block`, or deferred option semantics from the DB link.
  - Spec: `crates/epics-bridge-rs/doc/pvxs-functional-security-review-2026-05-18.md:327-357`
  - Done: worker B, commit `a198da48` on `caucus/HJB9ABPH/backend`; regression `br_r11_pvalink_out_options_preserved`. Cross-crate: also touched `epics-pva-rs` (added `op_put_field_with_request`, `op_put_value_raw`, `pvput_field_with_request`, `pvput_pv_field_with_request`). Upstream parity: pvxs ioc/pvalink_channel.cpp:28-47 (putReq template), :138 (field targeting), :220-263 (process computation), :223 (block), :268 (PUT); ioc/pvalink_lset.cpp:650-653 (defer/wait).

### Priority 4 — DB pvalink / group syntax/options

- [x] **BR-R10** — DB JSON pvalink options are reduced to only the PV name.
  - Spec: `crates/epics-bridge-rs/doc/pvxs-functional-security-review-2026-05-18.md:299-326`
  - Done: worker B, commit `dd80cdcd` on `caucus/HJB9ABPH/backend`; regressions `br_r10_json_pva_options_preserved_in_parsed_link`, `br_r10_json_pva_bare_pv_unchanged`, `br_r10_db_json_pvalink_options_preserved`. Cross-crate: epics-base-rs `extract_pv_and_opts_from_subobject` encodes all JSON keys as `?k=v&…` query string in `ParsedLink::Pva`; epics-bridge-rs adds `strip_query`, `lazy_register_inp_opts`/`lazy_register_out_opts`, and `install_pvalink_resolver` pre-scanner. Upstream parity: pvxs:ioc/pvalink_jlif.cpp:24-41 (JSON keys), :69-196 (per-key handlers), ioc/pvalink.cpp:~250 (jlink config lifetime).

- [x] **BR-R27** — pvalink cache key drops per-link `field` and option state.
  - Spec: `crates/epics-bridge-rs/doc/pvxs-functional-security-review-2026-05-18.md:766-794`
  - Done: worker B, commit `285a24e1` on `caucus/HJB9ABPH/backend` (+ doc tracker `686db100` on main); regression `br_r27_pvalink_cache_separates_per_link_options`. `RegistryKey` now `(pv_name, pipeline, queue_size, direction)` matching pvxs `channels_key_t`; `link_options`/`out_link_options` re-keyed by full query-bearing link string; `ScanTarget.field` added; `read_with_field` / `try_read_cached_with_field` for per-link field at read time.

### Priority 5 — Gateway typed forwarding / decoded monitor fallback

- [x] **BR-R6** — PVA gateway reserializes downstream PUTs through string `pvput`.
  - Spec: `crates/epics-bridge-rs/doc/pvxs-functional-security-review-2026-05-18.md:201-225`
  - Done: worker B, commit `4b7c87eb` on `caucus/HJB9ABPH/backend` (+ doc tracker `2eabe05f`); regression `br_r6_gateway_typed_put_passthrough`. Gateway source.rs typed pass-through replaces string `pvput` round-trip. 51m+ debug cycle (context compacted) but landed clean.

- [x] **BR-R41** — PVA gateway decoded monitor fallback emits only the initial snapshot.
  - Spec: `crates/epics-bridge-rs/doc/pvxs-functional-security-review-2026-05-18.md:1120-1144`
  - Done: worker B, commit `e314bb8a` on `caucus/HJB9ABPH/backend` (+ doc tracker `fcd4ff0e`); regression `br_r41_typed_subscribe_delivers_updates`. Three root causes fixed: (1) `tx_inner` dropped before `pvmonitor_raw_frames_handle` closure captured it; (2) `MonitorEventOutcome.value` re-acquisition race (now `Option<PvField>` read under write lock); (3) initial-event duplicate broadcast guarded by `!outcome.was_first`. 1h33m debug cycle (context compacted twice — contract drift cost).

### Priority 6 — Method/authority/roles, upstream identity, resource caps

- [x] **BR-R7** — PVA gateway upstream credential pool is unbounded and keyed by client-controlled identity.
  - Spec: `crates/epics-bridge-rs/doc/pvxs-functional-security-review-2026-05-18.md:226-249`
  - Done: worker B, commit `52308402` on `caucus/HJB9ABPH/backend` (+ doc tracker `ecf055a7`); regression `br_r7_gateway_credential_pool_bounded`. `upstream_pool` + `upstream_caches` HashMap→BoundedPool with LRU eviction, cap 256; `set_max_upstream_identities(&self, n)` config knob via `Arc<Mutex<...>>` interior mutability. Single-file source.rs (+172/-17), 10m. Clean contract compliance.

- [x] **BR-R8** — PVA gateway does not preserve downstream auth method/authority upstream.
  - Spec: `crates/epics-bridge-rs/doc/pvxs-functional-security-review-2026-05-18.md:250-273`
  - Done: Track A, commit `273d9a1d`; regression `br_r8_x509_downstream_recorded_as_asserted_identity` + `br_r8_ca_downstream_records_ca_method`. New `AssertedIdentity` on `epics-pva-rs` `PvaClientBuilder`/`PvaClient`; gateway `upstream_client_for` records downstream method+authority. Upstream parity: pvxs `clientconn.cpp:217-305` (CA wire carries no x509/authority — gateway converts identity to an explicit CA-style assertion).

### Priority 7 — Descriptor/value type shape

- [x] **BR-R12** — QSRV NTScalar/NTScalarArray metadata shape differs from pvxs.
  - Spec: `crates/epics-bridge-rs/doc/pvxs-functional-security-review-2026-05-18.md:358-387`
  - Done: Track B, commit `ad6331ed`; regression `br_r12_array_metadata_shape_matches_pvxs` + `br_r12_string_value_omits_numeric_metadata`. Limits typed by value scalar type, `display.form` an `enum_t` struct, full `valueAlarm` field set, NTScalarArray emits `control`/`valueAlarm`. Single-file `qsrv/pvif.rs`.

- [x] **BR-R13** — Unsigned 64-bit EPICS fields are not represented through QSRV.
  - Spec: `crates/epics-bridge-rs/doc/pvxs-functional-security-review-2026-05-18.md:388-415`
  - Done: Track B, commit `21ec21dc`; regressions `br_r13_uint64_field_maps_to_pva_ulong`, `br_r13_uint64_array_qsrv_descriptor_uses_ulong`, `br_r13_waveform_ftvl_uint64_storage_and_field_type`, `br_r13_waveform_new_from_uint64_dbf_type`. Added `DbFieldType::UInt64` + `EpicsValue::UInt64`/`UInt64Array`; waveform FTVL 7/8; QSRV `DBF_UINT64`→PVA `ulong`. Cross-crate (epics-base-rs, epics-ca-rs, epics-pva-rs).

### Priority 8 — Atomic semantics (shared multi-record lock)

- [x] **BR-R15** — QSRV atomic group PUT is not DBManyLock-equivalent.
  - Spec: `crates/epics-bridge-rs/doc/pvxs-functional-security-review-2026-05-18.md:444-469`
  - Done: Track C, commit `b093d49e`; regression `br_r15_atomic_group_excludes_direct_member_write` + `br_r15_atomic_put_blocks_on_member_record_gates`. Uses the unified `epics-base-rs` `record_lock` registry (`PvDatabase::lock_record` / `lock_records`). Upstream parity: pvxs `groupconfigprocessor.cpp:1165`, `groupsource.cpp:621,645`; epics-base `dbScanLock`.

- [x] **BR-R18** — pvalink `atomic` scan-on-update lacks a multi-record lock epoch.
  - Spec: `crates/epics-bridge-rs/doc/pvxs-functional-security-review-2026-05-18.md:524-548`
  - Done: Track D, commit `eff1848f`; regression `br_r18_atomic_scan_holds_multi_record_lock_epoch`. pvalink atomic scan holds the unified `record_lock` epoch (`PvDatabase::lock_records`) across the atomic target loop, released at the atomic→non-atomic boundary. Upstream parity: pvxs `ioc/pvalink_channel.cpp:409,422-427`; epics-base `dbLock.c:384,448`.

### Priority 9 — Group monitor metadata / archive events (residuals)

- [x] **BR-R29-RESIDUAL** — Group default `+trigger` SelfOnly variant exists but wire BitSet narrowing for SelfOnly is the residual gap.
  - Spec: `crates/epics-bridge-rs/doc/pvxs-functional-security-review-2026-05-18.md:819-844` and Cleared note `:58`
  - Done: Track C, commit `ee250057`; regressions `br_r29_diff_changed_bitset_marks_only_changed_leaves`, `br_r29_partial_monitor_payload_narrows_changed_bitset`, `br_r29_pure_self_trigger_predicate`. New `ChannelSource::monitor_emits_partial`, `encode::diff_changed_bitset`, `build_monitor_payload_partial`. Upstream parity: pvxs `groupsource.cpp:268,279,280`, `dataencode.cpp:414-437`.

- [x] **BR-R33-RESIDUAL** — Per-op queueSize negotiation is the residual gap (group GET/MONITOR carries root options already).
  - Spec: `crates/epics-bridge-rs/doc/pvxs-functional-security-review-2026-05-18.md:920-944` and Cleared note `:59`
  - Done: Track C, commit `16309573`; regression `br_r33_group_monitor_stamps_negotiated_queue_size`. `negotiated_queue_size(pvRequest)` threaded through `GroupMonitor::with_queue_size`. Upstream parity: pvxs `servermon.cpp:66,313,533-544`.

### Priority 10 — pvalink value conversion / metadata hooks

- [x] **BR-R24** — pvalink DB link metadata hooks are mostly absent.
  - Spec: `crates/epics-bridge-rs/doc/pvxs-functional-security-review-2026-05-18.md:686-712`
  - Done: Track D, commit `142aa474`; regressions `br_r24_link_metadata_surfaces_remote_display_control_valuealarm`, `br_r24_link_metadata_none_when_disconnected_and_enum_maps_to_dbf_enum`. New `LinkDbfType` + `LinkMetadata` + `LinkSet::link_metadata` hook in epics-base-rs; `PvaLinkResolver` impl. Upstream parity: pvxs `ioc/pvalink_lset.cpp:199,242,444,462,480,505,522,706`.

### Priority 12 — Gateway monitor fanout

- [x] **BR-R14** — PVA gateway monitor fanout is not pvRequest-transparent.
  - Spec: `crates/epics-bridge-rs/doc/pvxs-functional-security-review-2026-05-18.md:416-443`
  - Done: Track A, commit `15cc8be4`; regression `br_r14_pipeline_monitor_rejected_by_gateway` + `br_r14_field_projection_is_not_event_affecting`. New `MonitorOptions`; gateway rejects pipeline/queueSize monitor options (event-affecting), keeps field projection transparent. Upstream parity: pvxs `servermon.cpp:521-555`.

---

## Driver state

- Total open: 0 ✓ ALL ITEMS CLEARED
- Done: 17 (BR-R4, BR-R21, BR-R11, BR-R10, BR-R27, BR-R6, BR-R41, BR-R7, BR-R8, BR-R12, BR-R13, BR-R14, BR-R15, BR-R18, BR-R24, BR-R29-RESIDUAL, BR-R33-RESIDUAL)
- In progress: 0
- Blocked: 0
- Final 9 items (BR-R8, R12, R13, R14, R15, R18, R24, R29-RESIDUAL, R33-RESIDUAL) done in parallel on 4 worktree tracks A/B/C/D, integrated onto branch `integration/punchlist-2026-05-19`.
- **Integration note:** Tracks C and D independently added an `epics-base-rs` `record_lock` multi-record lock; merged into one unified registry (`RecordLockRegistry` / `lock_record` + `lock_records`) so QSRV atomic group PUT and pvalink atomic scan share one gate set and mutually exclude.
- **Verification (branch `integration/punchlist-2026-05-19`):** `cargo fmt --all` applied; `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo nextest run --workspace --no-fail-fast` 4631 run, **4631 passed, 0 failed**; `cargo test --doc --workspace` 0 failed.
- **Pre-existing `pva_gateway` failures — now FIXED:** the 6 `epics-bridge-rs::pva_gateway` integration tests that failed on the `main` baseline (`critical1_audit_layer_records_put`, `critical1_read_only_gateway_rejects_put`, `gateway_control_prefix_cache_size`, `gateway_get_forwards_upstream_value`, `gateway_monitor_fans_out_to_two_clients`, `multi_tenant_gateway_routes_to_correct_upstream`) were two real defects plus an unrealistic test fixture:
  1. **epics-pva-rs `ops_v2`** — the default *pipelined* monitor pvRequest forced `field(value)` instead of an empty `field {}` (= whole structure). `field(value)` against a bare-scalar PV is rejected by `request_to_mask`/pvxs `request2mask` as `EmptyMask`, and against a structure it silently narrowed the gateway's upstream monitor to the `value` leaf only.
  2. **epics-bridge-rs `channel_cache`** — the gateway's upstream monitor task fed only the raw broadcast (`tx_raw`); the decoded `subscribe` path (used for pipelined / projected / filtered downstream monitors) attached to a typed broadcast nothing fed, so such monitors saw only their initial snapshot. Fixed by feeding the typed broadcast from the already-decoded `state.latest`.
  3. The `spawn_upstream` test fixture served a bare top-level `Scalar` PV; real IOC/QSRV PVs are NTScalar structures (pvxs rejects `field(value)` on a bare scalar identically). Corrected to an `epics:nt/NTScalar:1.0` fixture.
