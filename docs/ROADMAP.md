# epics-rs Roadmap

**Status**: Draft, 2026-05
**Scope**: Strategic direction for epics-rs after the v0.13.0 protocol-parity baseline.

## Positioning

**epics-rs is a Rust-native EPICS stack for fast simulation today and production-grade Linux IOC infrastructure next.**

The post-v0.13.0 strategy is to *deepen* on tier-1 desktop/server operating systems rather than *broaden* across embedded RTOSs. The current project strength remains a Cargo-native path to complete simulated EPICS IOCs; the roadmap moves that stack toward production Linux operation. Production targets are Linux (vanilla and PREEMPT_RT); macOS and Windows are first-class developer targets. RTEMS, VxWorks, and bare-metal microcontroller targets are out of scope; pvxs already serves those well, and epics-rs and pvxs are positioned as complementary rather than competing.

> **RTEMS 6 caveat on that recommendation.** pvxs as shipped does not work on RTEMS 6 until the RTEMS-5-era `kqueue` avoidance at its `src/evhelper.cpp:183` is removed; with that line present libevent falls back to its `poll` backend, which never blocks on this BSP, and the IOC burns a core instead of serving. Remove the line and it serves normally — measurement and root cause in `doc/rtems-scope-b-session-handoff.md` §5.3.

Rationale:

- Rust toolchain on RTEMS is tier-3, out-of-tree, and not production-ready. Waiting on tier-2 promotion is an external blocker we cannot drive.
- Embedded/microcontroller demand for direct EPICS nodes is hypothetical; the current production pattern is host-side IOC + dumb device.
- Designing a runtime abstraction layer to keep all targets open imposes per-call overhead, API surface loss, and maintenance cost on the dominant Linux server use case. We pay the price every day to support a use case that may never materialize.
- Specializing on Linux unlocks io_uring, AF_XDP, eBPF, NUMA-aware scheduling, PREEMPT_RT, systemd, and cgroups v2 as first-class capabilities — none of which translate cleanly through an RTOS abstraction.

## Supported targets

| Tier | Targets | Guarantees |
|------|---------|------------|
| 1 (production) | `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` (vanilla and PREEMPT_RT) | CI target, performance baseline tracked, all features |
| 2 (developer) | `x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc` | CI target, all features, no RT/perf guarantees |
| 3 (community) | Other Linux architectures, FreeBSD | Best-effort, no CI |
| Out of scope | RTEMS, VxWorks, no_std/embedded | Use pvxs — on RTEMS 6 see the caveat under [Positioning](#positioning) |

Tier-2 targets receive all features and bug fixes; the tier distinction is about performance and real-time guarantees, not feature parity. macOS and Windows users get every cross-platform improvement (buffer tuning, allocation reduction, raw-frame forwarding, runtime tuning, `tokio-console`, OpenTelemetry tracing) automatically. Linux-only capabilities (PREEMPT_RT, io_uring, AF_XDP, eBPF probes, systemd, cgroups v2) are compile-time gated via `#[cfg(target_os = "linux")]` and feature flags, so they have zero footprint on tier-2 builds.

This matrix is the support policy target. Until Phase 0 is complete, CI and benchmark coverage may lag the policy; after Phase 0, pull requests that compromise tier-1 capabilities to widen target coverage will be rejected.

## Phase 0 — Support policy and baselines

Make the roadmap measurable before the Linux RT and high-performance networking work starts.

### Scope

- Align README positioning with this roadmap: simulation/prototyping remains the current value proposition; production Linux IOC operation is the next strategic target.
- Expand CI toward the support matrix: Linux x86_64, Linux aarch64, macOS, and Windows MSVC. Cross builds are acceptable for targets where hosted CI is expensive, but tier-1 Linux must run tests.
- Establish benchmark jobs and checked-in baseline reports for CA, PVA, and gateway paths. Existing CA micro/end-to-end Criterion benches are a start, not the full standard suite.
- Add load profiles for single IOC, gateway fan-out, and search burst. Record p50 / p99 / p99.99 latency, throughput, dropped packet/event counts, allocations, and RSS.
- Add co-located client/server UDP regression tests for PVA search/beacon behavior so future socket-option tuning does not reintroduce endpoint ownership bugs.

### Acceptance criteria

- CI matrix covers every tier-1 and tier-2 target at least at build level; tier-1 Linux runs tests.
- `cargo bench` / load-test commands are documented and reproducible from a clean checkout.
- Baseline reports identify which results are mandatory merge gates and which are advisory trend data.

## Phase 1 — Linux real-time support

Bring tier-1 Linux targets to deterministic-latency operation suitable for accelerator IOC deployments. PREEMPT_RT is mainline since kernel 6.12; the substrate exists, the work is integration.

### Scope

- `epics-rt` feature flag (default off) that enables RT-aware behavior across the workspace.
- `mlockall(MCL_CURRENT | MCL_FUTURE)` invoked from a public initialization entry point. Must be called before tokio runtime starts, which requires an RT-aware `#[epics_main]` / runtime-builder path.
- `SCHED_FIFO` / `SCHED_DEADLINE` thread classes for monitor and reactor threads, configurable per crate.
- Priority-inheritance mutexes on RT-critical paths. Use direct `pthread_mutexattr_setprotocol(PTHREAD_PRIO_INHERIT)` wrappers where needed, or prove the path avoids blocking locks.
- CPU affinity / pinning via `tokio::runtime::Builder::on_thread_start` calling `sched_setaffinity`.
- Allocation-free hot paths in PVA monitor encoder, CA `event_callback` dispatch, and gateway fan-out. Use thread-local arenas (`bumpalo`) or `Bytes` reuse pools; no `Vec::new()` per frame.
- Pre-faulted stack size for RT threads.

### Out of scope for Phase 1

- Hard sub-microsecond determinism. PREEMPT_RT delivers single-digit-microsecond worst-case latency on tuned hardware; epics-rs aims for jitter under 1 ms end-to-end on a typical accelerator IOC, matching what RTEMS provides today.
- Real-time scheduling for the entire workspace. Only marked-RT tasks get RT class; control-plane tasks remain `SCHED_OTHER`.

### Acceptance criteria

- PVA monitor end-to-end latency on PREEMPT_RT shows 99.99th-percentile jitter within target. Numbers established by Phase 1 baseline.
- No allocation in the steady-state monitor send path, verified by `dhat` or `heaptrack`.
- Documentation: deployment recipe (kernel config, BIOS, isolcpus, irqaffinity, RT throttling) in `docs/deployment/linux-rt.md`.

### Estimated effort

4–6 person-weeks.

## Phase 2 — High-performance I/O for CA and PVA

Apply kernel-level networking optimizations to both protocols at once. Optimizations are protocol-agnostic at the OS interface; each has different deployment cost and effect surface.

### Sprint 0 — Free wins (week 1)

Low-risk socket and runtime tuning; opt-in flags or audit-and-tune. These changes do not require the full benchmark gate, but network-visible changes require targeted regression tests.

- Audit and configure `SO_RCVBUF` / `SO_SNDBUF` for gateway TCP sockets and UDP responders. Default to OS maximum on gateway role.
- `TCP_NODELAY` audit on PVA ops and CA `event_callback` paths. Coalesce small writes via `writev` where Nagle was masking the issue.
- `SO_REUSEPORT` on UDP search responders for multi-worker fan-in, only where endpoint ownership is explicit. Co-located client/server and search/beacon sockets must be regression-tested so kernel load-balancing cannot route packets to the wrong role.
- `SO_BUSY_POLL` opt-in (`EPICS_PVA_BUSY_POLL_US`, `EPICS_CA_BUSY_POLL_US`). Trades CPU for tail-latency. Off by default.
- Tokio runtime configuration: explicit `worker_threads`, tuned `event_interval`, bounded `max_blocking_threads` to prevent silent leaks.
- Apply the Phase 0 benchmark/load-test harness to the tuned socket/runtime defaults and record before/after deltas.

### Sprint 1 — Measured wins (weeks 2–4)

Items that require a baseline first. Target effects are documented; numbers come from measurement.

- PVA gateway raw-frame forwarding: keep the current default-on path and validate it under production-like fan-out. `EPICS_PVA_GW_RAW_FRAMES=NO` remains the rollback switch if a regression is found.
- PVA monitor encoder allocation audit. Move per-frame `Vec` allocations to per-channel reusable buffers. Validate with `dhat-rs`.
- CA `event_callback` writev coalescing where prelude and payload currently issue two writes.
- Adaptive `epoll`/`mio` event count per loop based on backlog depth.

### Sprint 2 — io_uring backend (single hot path)

Tokio-uring is a separate runtime; it cannot replace the workspace-wide tokio runtime. The realistic shape is a hybrid: control-plane tasks run on regular tokio, while a designated hot-path task (initially the PVA monitor sender) runs on a `tokio_uring::start` thread and communicates via lock-free queues.

- Feature-flagged `epics-pva-rs/io-uring`. Off by default until the supported Linux kernel floor is selected in Phase 0.
- Target operations: `IORING_OP_SEND_ZC` (zero-copy send), `IORING_OP_SEND` with `IORING_RECVSEND_BUNDLE` for batched fan-out, and multi-shot recv (`IORING_OP_RECV_MULTISHOT`) for the search responder.
- Effect surface: throughput improvement scales with PVA monitor fan-out width; gateway with 1k+ subscribers expected to benefit most. With raw-frame forwarding default-on, io_uring's marginal gain narrows — measure both together to decide priority.
- Estimated effort: 2–4 person-weeks.

### Sprint 3 — AF_XDP for UDP search and beacon (per-deployment)

Kernel-bypass UDP via XDP/eBPF. Bypasses the BSD socket layer entirely; UDP-only; requires `CAP_NET_ADMIN` and NIC driver support (`igb`, `ixgbe`, `mlx5`, etc.).

- Out-of-tree feature crate `epics-net-xdp` providing an alternate transport for PVA and CA search responders and beacon receivers.
- Use case: large-site gateways handling thousands of concurrent client search bursts where a vanilla UDP socket drops packets.
- Default off and not built into the standard binary; requires explicit opt-in plus deployment infrastructure.
- Measurement gate: Sprint 0 baseline must show packet drop or CPU saturation on the UDP path before this is undertaken.
- Estimated effort: 4–6 person-weeks.

### Cross-cutting requirements

- All optimizations must be measured before and after. No "optimization" lands without a delta on the standard benchmark suite, unless the change is correctness-only and carries a targeted regression result.
- All optimizations must be opt-in or runtime-detectable. The default build runs unmodified across tier-1 and tier-2 targets.
- AF_XDP and io_uring code paths must compile-out cleanly when the feature flag is off.

## Phase 3 — Observability and operations (parallel)

Deepen Linux-native operational integration. Items below are mostly additive; can be done in parallel with Phases 1 and 2.

- `tokio-console` integration for live task and lock visualization.
- `tracing` + OpenTelemetry exporter as an opt-in feature for distributed tracing across IOC and gateway.
- `eBPF` USDT probes for in-kernel observability of PVA channel state, monitor delivery, and ACF decisions. Deliver as `bpftrace` recipes in `docs/observability/`.
- systemd integration: socket activation, journal-aware logging, watchdog ping.
- cgroups v2 resource limit awareness: respect memory limits in cache sizing, react to CPU pressure.
- Linux audit subsystem integration for ACF security events.

## Cross-cutting principles

1. **Measure first.** No optimization lands without a benchmark delta or a targeted regression result explaining why the change is correctness-only. Phase 0 baselines are the prerequisite for Phase 2 performance work.
2. **Opt-in over rewrite.** Where a kernel feature requires a recent Linux version (PREEMPT_RT, io_uring multi-shot, AF_XDP), gate behind a feature flag. The default build runs on tier-1 and tier-2 unchanged.
3. **Tier-1 first.** Every change is validated on Linux. macOS and Windows are kept green via CI but do not gate Linux work.
4. **No abstraction layers we don't need.** We deliberately do not add a runtime trait or proto/transport split solely to preserve RTOS optionality. If a refactor has standalone value (testing speed, fuzzing surface, modularity), that is the justification — not future portability.
5. **Honest tier policy.** Out-of-scope targets stay out of scope. Users requesting RTEMS support are directed to pvxs; this is documented in README. Tell them the one thing that otherwise costs a day: on RTEMS 6 pvxs as shipped does not work until the RTEMS `kqueue` avoidance at its `src/evhelper.cpp:183` is removed (`doc/rtems-scope-b-session-handoff.md` §5.3).

## Non-goals

- RTEMS, VxWorks, or any RTOS target.
- `no_std` or embedded microcontroller support for protocol crates.
- Runtime abstraction layer over Tokio.
- Drop-in compatibility with EPICS Base C builds for fields beyond the IOC protocol surface.

## Open questions

These are intentionally not answered in this document. Each blocks a specific phase.

- **PREEMPT_RT minimum kernel version.** 6.12 is the mainline merge; what is the realistic deployment baseline at accelerator sites in 2026? This determines whether Phase 1 targets 6.12 or earlier RT-patch kernels.
- **io_uring minimum kernel.** The chosen operation set determines the floor: core io_uring networking, zero-copy send, multi-shot recv, and recv/send bundle support do not all arrive at the same kernel version. Which subset is worth supporting first?
- **AF_XDP deployment cost.** What is the operational overhead at sites where XDP requires interaction with vendor NIC firmware? Decide whether to invest before measurement justifies it.
- **macOS production status.** Today macOS is a developer target. Should it be promoted to tier-1 production for the (small) macOS IOC user base, or kept tier-2 indefinitely?
