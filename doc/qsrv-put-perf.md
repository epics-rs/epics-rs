# QSRV PUT path: server CPU per put vs pvxs

Measurement backing the `perf/qsrv-put-op-path` branch. Goal, set by the
project owner: the Rust qsrv must beat pvxs QSRV2 on PUT — lower server
CPU per put AND higher puts/s, measured the same day on the same host.

## Method

- Server: `benchsrv` (scratch bin) — the native PVA server +
  `BridgeProvider`/`QsrvPvStore` over a 2-record `bench.db`, runtime
  shape selectable (`--workers 0` = current_thread, `N` =
  `multi_thread(N)`).
- Client: `putbench` — one `PvaClient`, 100 warmup puts, then 20,000
  sequential `pvput`s of a scalar to `BENCH:AO` (each put is a fresh
  INIT/EXEC/DESTROY op, matching `pvput` CLI behaviour).
- Server CPU: `/proc/<pid>/stat` utime+stime delta across the timed
  window (100 Hz ticks), µs/put = ticks × 10,000 / 20,000. PID taken
  with `pgrep -x` (exact name — `-f` matches the measuring shell).
- Control: pvxs `softIocPVX` on the same host, same db, same client:
  **108 µs/put, 3,649 puts/s** (measured 2026-08-08).

## Runtime shape sweep (2026-08-09, 96-core host, all branch perf work in)

| workers | server CPU µs/put (runs) | puts/s |
|---|---|---|
| 0 (current_thread) | 92.5, 93.0, 97.5 | 3,787–3,935 |
| 1 | 96.5, 97.0 | 3,858–3,884 |
| 2 | 115.0, 116.0 | 3,521–3,734 |
| 4 | 125.5, 126.5 | 3,515–3,706 |
| 96 (tokio default here) | 132.0, 134.5 | 3,568–3,774 |

The cliff is between 1 and 2 workers: the serving work for one client is
a single runnable task, and every idle sibling worker adds wake/steal
churn as the task migrates. A bare `#[tokio::main]` (or
`new_multi_thread()` without `worker_threads`) sizes the pool to the
host's CPU count, so the same binary that wins at 1 worker loses to
pvxs at the host default.

## Default chosen for the server bins

`worker_threads = 1`, multi-thread flavor — the reactor shape pvxs and
pva2pva serve from (one event loop). Applied to `qsrv_rs`,
`dual_ioc_rs`, `pva_gateway_rs`, `softioc-rs`, `ca_gateway_rs`,
`dual_gateway_rs`.

Why not `current_thread` (2–4 µs/put cheaper): the flavor is
load-bearing. `epics_base_rs::runtime::task::block_on_sync` refuses a
current-thread runtime (`NotBlockable::CurrentThreadRuntime`) because
parking its only thread halts the tasks that would wake it, and asyn's
`block_on_reply` loses its `block_in_place` scheduling courtesy. One
multi-thread worker keeps every flavor-branched bridge on its normal
path; `block_in_place` on a 1-worker pool is sound (tokio spawns a
replacement worker for the blocked one).

Not fixed here, deliberately: client CLIs and short-lived tools (no
serving hot path), `ca-soak*` load generators (want the parallelism),
`procserv_rs` (console supervisor), `realtime-*` bins (exec backend, no
tokio), the `oracle*` parity harness.
