# epics-bridge-rs Deferred/Remaining Punchlist — 2026-05-19

Driver-managed punchlist. **Worker contract:**

1. Take exactly the next unchecked `[ ]` item the driver hands you. Do NOT freelance to other items.
2. Before editing the cited line, run the **Anchor / Sites / Same defect / Distinct** audit per the global rule (see ~/.claude/CLAUDE.md "Fixes from reported defects"). Mandatory report header before edits.
3. NEVER emit: `TODO`, `FIXME`, `unimplemented!()`, `#[allow(...)]` (to silence), `// later`, "next session", "out of scope" (unless user-scoped this round), "scope이 크다", "위험합니다", "다음에", "defer".
4. **Upstream parity is the bar — no inventing semantics.** Every design decision (values, edge cases, where behaviour applies, defaults) MUST be grounded in the upstream C++ reference:
   - pvxs:       `/Users/stevek/codes/pvxs`
   - epics-base: `/Users/stevek/codes/epics-base`
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

## Items (ordered by review-doc priority block)

### Priority 1 — ACF/identity/RPC protection

- [x] **BR-R4** — QSRV ACF adapter collapses method/authority/roles and field ASL.
  - Spec: `doc/pvxs-functional-security-review-2026-05-18.md:153-176`
  - Done: worker B, commit `db9a79e4` on `caucus/HJB9ABPH/backend` (+ doc tracker commit `2a58c9d9` on main); regression `br_r4_acf_method_authority_roles_field_asl`. Cross-crate: also touched `epics-base-rs/src/server/access_security.rs` (quoted-string support in `read_brace_list`). Upstream parity: pvxs credentials.cpp:31-45; securityclient.cpp:25,42-45; epics-base asLibRoutines.c:1006; asLib_lex.l.

- [x] **BR-R21** — PVA gateway READ/MONITOR upstream authorization uses the shared cache client.
  - Spec: `doc/pvxs-functional-security-review-2026-05-18.md:604-632`
  - Done: worker B, commit `6f9d26c6` on `caucus/HJB9ABPH/backend`; regression `br_r21_gateway_monitor_credential_scoping`. New field `upstream_caches` + `upstream_cache_for` in `GatewayChannelSource`; extracted `subscribe_raw_inner`/`subscribe_inner` helpers; overrode `get_value_checked`, `subscribe_checked`, `subscribe_raw_checked` to route through per-credential cache. Upstream parity: pvxs security-review spec "the gateway MUST NOT silently conflate per-client upstream authorization into a single shared-client authorization" (doc:604-632); mirrors `upstream_client_for`/PUT/RPC/PROCESS path (PG-G10).

### Priority 3 — Field addressing & PUT/scan processing

- [x] **BR-R11** — pvalink OUT writes do not preserve `field`, `proc`, `block`, or deferred option semantics from the DB link.
  - Spec: `doc/pvxs-functional-security-review-2026-05-18.md:327-357`
  - Done: worker B, commit `a198da48` on `caucus/HJB9ABPH/backend`; regression `br_r11_pvalink_out_options_preserved`. Cross-crate: also touched `epics-pva-rs` (added `op_put_field_with_request`, `op_put_value_raw`, `pvput_field_with_request`, `pvput_pv_field_with_request`). Upstream parity: pvxs pvalink_channel.cpp:28-47 (putReq template), :138 (field targeting), :220-263 (process computation), :223 (block), :268 (PUT); pvalink_lset.cpp:647 (defer/wait).

### Priority 4 — DB pvalink / group syntax/options

- [x] **BR-R10** — DB JSON pvalink options are reduced to only the PV name.
  - Spec: `doc/pvxs-functional-security-review-2026-05-18.md:299-326`
  - Done: worker B, commit `dd80cdcd` on `caucus/HJB9ABPH/backend`; regressions `br_r10_json_pva_options_preserved_in_parsed_link`, `br_r10_json_pva_bare_pv_unchanged`, `br_r10_db_json_pvalink_options_preserved`. Cross-crate: epics-base-rs `extract_pv_and_opts_from_subobject` encodes all JSON keys as `?k=v&…` query string in `ParsedLink::Pva`; epics-bridge-rs adds `strip_query`, `lazy_register_inp_opts`/`lazy_register_out_opts`, and `install_pvalink_resolver` pre-scanner. Upstream parity: pvxs:ioc/pvalink_jlif.cpp:24-41 (JSON keys), :69-196 (per-key handlers), ioc/pvalink.cpp:~250 (jlink config lifetime).

- [x] **BR-R27** — pvalink cache key drops per-link `field` and option state.
  - Spec: `doc/pvxs-functional-security-review-2026-05-18.md:766-794`
  - Done: worker B, commit `285a24e1` on `caucus/HJB9ABPH/backend` (+ doc tracker `686db100` on main); regression `br_r27_pvalink_cache_separates_per_link_options`. `RegistryKey` now `(pv_name, pipeline, queue_size, direction)` matching pvxs `channels_key_t`; `link_options`/`out_link_options` re-keyed by full query-bearing link string; `ScanTarget.field` added; `read_with_field` / `try_read_cached_with_field` for per-link field at read time.

### Priority 5 — Gateway typed forwarding / decoded monitor fallback

- [x] **BR-R6** — PVA gateway reserializes downstream PUTs through string `pvput`.
  - Spec: `doc/pvxs-functional-security-review-2026-05-18.md:201-225`
  - Done: worker B, commit `4b7c87eb` on `caucus/HJB9ABPH/backend` (+ doc tracker `2eabe05f`); regression `br_r6_gateway_typed_put_passthrough`. Gateway source.rs typed pass-through replaces string `pvput` round-trip. 51m+ debug cycle (context compacted) but landed clean.

- [x] **BR-R41** — PVA gateway decoded monitor fallback emits only the initial snapshot.
  - Spec: `doc/pvxs-functional-security-review-2026-05-18.md:1120-1144`
  - Done: worker B, commit `e314bb8a` on `caucus/HJB9ABPH/backend` (+ doc tracker `fcd4ff0e`); regression `br_r41_typed_subscribe_delivers_updates`. Three root causes fixed: (1) `tx_inner` dropped before `pvmonitor_raw_frames_handle` closure captured it; (2) `MonitorEventOutcome.value` re-acquisition race (now `Option<PvField>` read under write lock); (3) initial-event duplicate broadcast guarded by `!outcome.was_first`. 1h33m debug cycle (context compacted twice — contract drift cost).

### Priority 6 — Method/authority/roles, upstream identity, resource caps

- [x] **BR-R7** — PVA gateway upstream credential pool is unbounded and keyed by client-controlled identity.
  - Spec: `doc/pvxs-functional-security-review-2026-05-18.md:226-249`
  - Done: worker B, commit `52308402` on `caucus/HJB9ABPH/backend` (+ doc tracker `ecf055a7`); regression `br_r7_gateway_credential_pool_bounded`. `upstream_pool` + `upstream_caches` HashMap→BoundedPool with LRU eviction, cap 256; `set_max_upstream_identities(&self, n)` config knob via `Arc<Mutex<...>>` interior mutability. Single-file source.rs (+172/-17), 10m. Clean contract compliance.

- [ ] **BR-R8** — PVA gateway does not preserve downstream auth method/authority upstream.
  - Spec: `doc/pvxs-functional-security-review-2026-05-18.md:250-273`

### Priority 7 — Descriptor/value type shape

- [ ] **BR-R12** — QSRV NTScalar/NTScalarArray metadata shape differs from pvxs.
  - Spec: `doc/pvxs-functional-security-review-2026-05-18.md:358-387`

- [ ] **BR-R13** — Unsigned 64-bit EPICS fields are not represented through QSRV.
  - Spec: `doc/pvxs-functional-security-review-2026-05-18.md:388-415`

### Priority 8 — Atomic semantics (shared multi-record lock)

- [ ] **BR-R15** — QSRV atomic group PUT is not DBManyLock-equivalent.
  - Spec: `doc/pvxs-functional-security-review-2026-05-18.md:444-469`

- [ ] **BR-R18** — pvalink `atomic` scan-on-update lacks a multi-record lock epoch.
  - Spec: `doc/pvxs-functional-security-review-2026-05-18.md:524-548`

### Priority 9 — Group monitor metadata / archive events (residuals)

- [ ] **BR-R29-RESIDUAL** — Group default `+trigger` SelfOnly variant exists but wire BitSet narrowing for SelfOnly is the residual gap.
  - Spec: `doc/pvxs-functional-security-review-2026-05-18.md:819-844` and Cleared note `:58`

- [ ] **BR-R33-RESIDUAL** — Per-op queueSize negotiation is the residual gap (group GET/MONITOR carries root options already).
  - Spec: `doc/pvxs-functional-security-review-2026-05-18.md:920-944` and Cleared note `:59`

### Priority 10 — pvalink value conversion / metadata hooks

- [ ] **BR-R24** — pvalink DB link metadata hooks are mostly absent.
  - Spec: `doc/pvxs-functional-security-review-2026-05-18.md:686-712`

### Priority 12 — Gateway monitor fanout

- [ ] **BR-R14** — PVA gateway monitor fanout is not pvRequest-transparent.
  - Spec: `doc/pvxs-functional-security-review-2026-05-18.md:416-443`

---

## Driver state

- Total open: 8 (was 17)
- Done: 8 (BR-R4, BR-R21, BR-R11, BR-R10, BR-R27, BR-R6, BR-R41, BR-R7)
- In progress: 1 (BR-R8, worker B)
- Blocked: 0
