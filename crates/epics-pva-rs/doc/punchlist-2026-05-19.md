# epics-pva-rs Deferred/Remaining Punchlist — 2026-05-19

Driver-managed punchlist. **Worker contract:**

1. Take exactly the next unchecked `[ ]` item the driver hands you. Do NOT freelance to other items.
2. Before editing the cited line, run the **Anchor / Sites / Same defect / Distinct** audit per the global rule (see ~/.claude/CLAUDE.md "Fixes from reported defects"). Mandatory report header before edits.
3. NEVER emit: `TODO`, `FIXME`, `unimplemented!()`, `#[allow(...)]` (to silence), `// later`, "next session", "out of scope" (unless user-scoped this round), "scope이 크다", "위험합니다", "다음에", "defer".
4. **Upstream parity is the bar — no inventing semantics.** Every design decision (values, edge cases, where behaviour applies, defaults) MUST be grounded in the upstream C++ reference:
   - pvxs:       `/Users/stevek/codes/pvxs`
   - epics-base: `/Users/stevek/codes/epics-base`
   Use `rg` in those trees BEFORE making decisions. Commit message and end-of-task report MUST include an **Upstream parity** section listing `pvxs:file:line` and/or `epics-base:file:line` your implementation mirrors for each behaviour. If you cannot find the upstream reference for a sub-behaviour, STOP and report — do NOT invent.
5. Root cause fix at source. Comment-only "fix" = rejected. Type/API closure preferred over local patches when the global rule's "Invariant-driven fixes" section applies.
5. After edits: `cargo fmt --all` → `cargo clippy -p epics-pva-rs --all-targets -- -D warnings` → `cargo nextest run -p epics-pva-rs`. If your change crosses crate boundaries, escalate to `--workspace`. Doctest changes → `cargo test --doc -p epics-pva-rs`.
6. Add a regression test that fails on main and passes after your fix. Name it `pva_rN_<short>`.
7. End-of-task report in the format mandated by global rules: Tested / Failed / UNFIXED / Fixed.
8. Commit per item (one item = one commit). No bundled commits. No `git push` without explicit user confirmation. No `Co-Authored-By` lines.

Driver (main session) verifies after each item:
- `rg "(TODO|FIXME|unimplemented!|#\[allow|// later)"` over your diff → must be zero new hits.
- Banned phrases in your panel output → rejection + correction.
- All three cargo commands recorded as passing.
- Regression test exists and exercises the cited defect.

When all items checked, run full-workspace `cargo clippy --workspace --all-targets -- -D warnings` + `cargo nextest run --workspace` + `cargo test --doc --workspace` before reporting done.

---

## Items

- [x] **PVA-R2** (MEDIUM, architectural) — `PvaClientBuilder::tcp_timeout()` is stored but not applied. Plumb `tcp_timeout` through `ConnectionPool::get_or_connect` + `ServerConn::connect` + spawned heartbeat task. Multi-layer signature change.
  - Spec: `doc/critical-review-2026-05-18.md:68-102`
  - Deferral note: `doc/critical-review-2026-05-18.md:1254-1257`
  - Done: worker A, commit `479f77c0` on `caucus/HJB9ABPH/worker`; regression `pva_r2_tcp_timeout_applied`. Upstream parity: pvxs clientconn.cpp:73-74,163-165; config.cpp:149,211-226,373-391.

- [ ] **PVA-R3** (MEDIUM, architectural) — Nested Variant values lose the stream type-cache. Thread `&mut TypeCache` through `decode_pv_field` family + all op-response decoders + reader flattening.
  - Spec: `doc/critical-review-2026-05-18.md:103-147`
  - Deferral note: `doc/critical-review-2026-05-18.md:1258-1260`

- [x] **PVA-R4** (MEDIUM, architectural) — TCP name servers as persistent search peers (not direct-connect fallbacks). Client-side persistent name-server connection in `SearchEngine`. Server-side TCP SEARCH already cleared via R11.
  - Spec: `doc/critical-review-2026-05-18.md:148-183`
  - Deferral note: `doc/critical-review-2026-05-18.md:1261-1267`
  - Done: worker A, commit `7dfe5de6` on `caucus/HJB9ABPH/worker` (commit subject typo says `BR-R4`, content is PVA-R4); regression `pva_r4_tcp_nameserver_persistent_peer`. `SearchEngine::spawn` now takes `name_servers: Vec<SocketAddr>`, spawns persistent `ns_task` per entry with full PVA TCP handshake + bidirectional SEARCH relay, reconnects every 10s; `Channel` NS fallback path removed. Upstream parity: pvxs `tcpNSCheckInterval`; client.cpp:828-846 (port-0 fixup).

- [x] **PVA-R6** (MEDIUM, architectural) — `SharedPV` drops the newest update when a subscriber queue is full. Implement squash-to-tail semantics via `Mutex<VecDeque>+Notify` or `tokio::sync::watch` (tokio::mpsc has no sender-side drop-oldest).
  - Spec: `doc/critical-review-2026-05-18.md:259-294`
  - Deferral note: `doc/critical-review-2026-05-18.md:1268-1271`
  - Done: worker A, commit `c4bb773a` on `caucus/HJB9ABPH/worker`; regression `pva_r6_squash_to_tail`. New `MonitorOutbox`/`MonitorInbox` types with `Mutex<VecDeque>+Notify`; queue default 64 → 4. Upstream parity: pvxs servermon.cpp:66 (default queue limit), :283-286 (squash-to-tail).

- [ ] **PVA-R14** (MEDIUM, architectural) — Decouple server source calls from per-connection read loop. Operation-state-machine restructure so source futures don't head-of-line-block the socket parser.
  - Spec: `doc/critical-review-2026-05-18.md:513-556`
  - Deferral note: `doc/critical-review-2026-05-18.md:1272-1275`

- [ ] **TLS-NAMESERVER** (MEDIUM, architectural) — TLS via name-server (mixed-mode listener). TCP accept loop peeks the first byte and dispatches to plain handshake or `TlsAcceptor`. Refactor of `server_native/tcp.rs:460-590`.
  - Spec/Deferral note: `doc/critical-review-2026-05-18.md:1290-1298`

---

## Driver state

- Total open: 3 (was 6)
- Done: 3 (PVA-R2, PVA-R6, PVA-R4)
- In progress: 1 (TLS-NAMESERVER, worker A)
- Blocked: 0
- Verified pre-existing main failure: `critical1_audit_layer_records_put` (tests/pva_gateway.rs:557) — separate from this round; not introduced by any worker.
