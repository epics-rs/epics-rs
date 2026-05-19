# epics-bridge-rs Deferred/Remaining Punchlist — 2026-05-19

Driver-managed punchlist. **Worker contract:**

1. Take exactly the next unchecked `[ ]` item the driver hands you. Do NOT freelance to other items.
2. Before editing the cited line, run the **Anchor / Sites / Same defect / Distinct** audit per the global rule (see ~/.claude/CLAUDE.md "Fixes from reported defects"). Mandatory report header before edits.
3. NEVER emit: `TODO`, `FIXME`, `unimplemented!()`, `#[allow(...)]` (to silence), `// later`, "next session", "out of scope" (unless user-scoped this round), "scope이 크다", "위험합니다", "다음에", "defer".
4. Root cause fix at source. Comment-only "fix" = rejected. Type/API closure preferred over local patches when the global rule's "Invariant-driven fixes" section applies.
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

- [ ] **BR-R4** — QSRV ACF adapter collapses method/authority/roles and field ASL.
  - Spec: `doc/pvxs-functional-security-review-2026-05-18.md:153-176`

- [ ] **BR-R21** — PVA gateway READ/MONITOR upstream authorization uses the shared cache client.
  - Spec: `doc/pvxs-functional-security-review-2026-05-18.md:604-632`

### Priority 3 — Field addressing & PUT/scan processing

- [ ] **BR-R11** — pvalink OUT writes do not preserve `field`, `proc`, `block`, or deferred option semantics from the DB link.
  - Spec: `doc/pvxs-functional-security-review-2026-05-18.md:327-357`

### Priority 4 — DB pvalink / group syntax/options

- [ ] **BR-R10** — DB JSON pvalink options are reduced to only the PV name.
  - Spec: `doc/pvxs-functional-security-review-2026-05-18.md:299-326`

- [x] **BR-R27** — pvalink cache key drops per-link `field` and option state.
  - Spec: `doc/pvxs-functional-security-review-2026-05-18.md:766-794`
  - Done: worker B, commit `285a24e1` on `caucus/HJB9ABPH/backend`; regression `br_r27_pvalink_cache_separates_per_link_options`. Cross-crate: all three pvalink files (`link.rs`, `registry.rs`, `integration.rs`). Upstream parity: pvxs/ioc/pvalink.h:65 (per-link pvaLinkConfig); pvxs/ioc/pvalink.h:116 (channels_key_t); pvxs/ioc/pvalink_lset.cpp:49-65,99-100 (makeRequest + channel lookup key); pvxs/ioc/pvalink_link.cpp:91 (root = lchan->root[fieldName]).

### Priority 5 — Gateway typed forwarding / decoded monitor fallback

- [x] **BR-R6** — PVA gateway reserializes downstream PUTs through string `pvput`.
  - Spec: `doc/pvxs-functional-security-review-2026-05-18.md:201-225`
  - Done: worker B, commit `4b7c87eb` on `caucus/HJB9ABPH/backend`; regression `br_r6_gateway_typed_put_passthrough`. Both `put_value` and `put_value_checked` in `source.rs` switched from `pvfield_to_pvput_string` + `pvput` to `pvput_pv_field`. Dead helpers `pvfield_to_pvput_string` and `scalar_to_string` removed. Upstream parity: pvxs/src/clientget.cpp:305 (`to_wire_valid(R, temp)` — no string round-trip).

- [x] **BR-R41** — PVA gateway decoded monitor fallback emits only the initial snapshot.
  - Spec: `doc/pvxs-functional-security-review-2026-05-18.md:1120-1144`
  - Done: worker B, commit `e314bb8a` on `caucus/HJB9ABPH/backend`; regression `br_r41_typed_subscribe_delivers_updates`. `tx_inner` moved into `pvmonitor_raw_frames_handle` callback closure; `!outcome.was_first` guard prevents duplicate broadcast of initial snapshot. `MonitorEventOutcome.decoded: bool` replaced by `value: Option<PvField>` read under write lock. Upstream parity: pvxs/src/client.cpp — `MonitorImpl::notify()` delivers every monitor event to every subscriber after first `TypeChange`.

### Priority 6 — Method/authority/roles, upstream identity, resource caps

- [x] **BR-R7** — PVA gateway upstream credential pool is unbounded and keyed by client-controlled identity.
  - Spec: `doc/pvxs-functional-security-review-2026-05-18.md:226-249`
  - Done: worker B, commit `52308402` on `caucus/HJB9ABPH/backend`; regression `br_r7_gateway_credential_pool_bounded`. Both `upstream_pool` (PvaClient) and `upstream_caches` (ChannelCache) replaced with `BoundedPool<K, V>` (LRU eviction, default cap 256). `set_max_upstream_identities(&self, n)` updates both pool caps atomically. Upstream parity: pvxs has no pva2pva gateway source in checked-out copy; wire-compatible expectation applied per spec §BR-R7.

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
- Done: 8 (BR-R4, BR-R21, BR-R11, BR-R10, BR-R27, BR-R6, BR-R41, BR-R7) — tracked on main via doc tracker commits
- In progress: 0
- Blocked: 0
