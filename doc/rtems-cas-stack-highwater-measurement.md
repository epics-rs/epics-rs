# CAS-client / CAS-event stack high-water on armv7-rtems-eabihf

On-target measurement, 2026-07-27, qemu `xilinx-zynq-a9`, RTEMS 6.0.0,
`realtime-ca-ioc` built from `caucus/58EWEJWV91/e10-residue-503b2859-1` with
`--no-default-features --features client-core,bringup-probes`, profile
`release-embedded`, custom target spec carrying `has-thread-local: true`.
Image 4,666,288 bytes.

The reading exists because the `client_roster` `StackSizeClass` change is
gated on it: the VxWorks figures cannot be carried over. `StackSizeClass::bytes()`
is `f * 0x10000 * size_of::<usize>()` (`crates/epics-base-rs/src/runtime/task.rs`),
so on a 32-bit target every class is *half* its `x86_64-wrs-vxworks` value —
Small 262,144, Medium 524,288, Big 1,048,576.

## The numbers

Source: `rtems_stack_checker_report_usage()`, called from inside the image by
`epics_rtems_boot::stats::stack_report` on the `c6-probe` thread every 60 s
(`CONFIGURE_STACK_CHECKER_ENABLED`, `rtems_config.c`). `USED` is the
lifetime high-water of the pattern fill, not an instantaneous depth.

| thread | class | declared | `AVAIL` | high-water | over-declared |
| --- | --- | --- | --- | --- | --- |
| `CAS-client` | `Big` | 1,048,576 | 1,048,560 | **24,432** | 42.9× |
| `CAS-event` | `Medium` | 524,288 | 524,272 | **3,816** | 137.4× |

Both are lifetime maxima over every `CAS-*` task the image created across the
whole session — 40 `CAS-client` and 40 `CAS-event` tasks, since the worker pool
keeps a task after its client disconnects and the reports continue to include
it. Converged: 24,424 B at 4 clients × 400 chain rounds, 24,432 B at 8 clients
× 500 rounds — +8 B for double the clients.

For scale, the deepest thread in the same image is not a CA one:

```
0x0b010003 cbMedium              0x00830820 0x0093080f 0x009306a0 1048560 202300
```

## The high-water is a function of database depth, not of client count

Read/write/monitor load alone does **not** reach the deepest `CAS-client`
path. Two loads on the same booted guest:

| load | `CAS-client` high-water |
| --- | --- |
| 32 clients × 23 channels: 5-family reads, 5 subscriptions/channel, 596 write + write-notify rounds, the refusal sweep, connect/disconnect churn | 8,960 |
| the above, plus CA writes to `RTEMS:CA:C1` and `RTEMS:CA:FAST` | **24,432** |

`RTEMS:CA:FAST → C1 → C2 → … → C8` is the C6 probe rig's nine-record `FLNK`
chain. A CA write to its head processes eight further records **inline on the
`CAS-client` task**, so the extra 15,472 B is ≈1,934 B per `FLNK` hop. A
database with a deeper chain than the 17-record C6 rig will exceed 24,432 B
proportionally; this number bounds *this* database, and a `StackSizeClass`
decision drawn from it has to carry that qualification.

## What the load actually exercised

`USED` only bounds the paths that ran, so every request shape was checked one
at a time (`doc/vxworks-e10-rig/rtemscmds-e10.py`) rather than assumed from a
silent server:

| request | served |
| --- | --- |
| `READ_NOTIFY` (15), DBR native / STS / TIME / GR / CTRL | yes — payloads 8/16/24/72/88 B |
| `EVENT_ADD` (1), DBR native / STS / TIME / GR / CTRL | yes — same five payload sizes |
| `WRITE` (4) / `WRITE_NOTIFY` (19) | yes |
| `SEARCH` (6) over the TCP circuit | yes |
| `ECHO` (23), `EVENTS_OFF` (8), `EVENTS_ON` (9), `CLEAR_CHANNEL` (12) | yes |
| `CREATE_CHAN` (18) for a missing record | yes — `CREATE_CH_FAIL` (26) |
| `READ_NOTIFY` with an unknown server id | yes — `ERROR` status 142 |
| `READ_NOTIFY` with `dcount` 4096 on a scalar | yes — clamped to 1 |
| legacy `READ` (7), every DBR family | **no** — `ERROR` status 432, `CAS: command not yet supported by blocking CA driver` |
| `READ_NOTIFY` with `dtype` 199 | circuit closed, no `ERROR` frame |

So all five response encoders ran. Legacy `READ` (7) is the pre-CA-4.11
command libca no longer emits and shares its encoders with `READ_NOTIFY`, so
its refusal does not shorten the measured path. The last two rows are
observations about the blocking driver, not about the stack, and belong to
whoever owns `crates/epics-ca-rs/src/server/`.

## Rig

`doc/vxworks-e10-rig/`:

* `build-rtems-e10.sh` — replicates `scripts/embedded-image.sh rtems ca` with
  `bringup-probes` added. `scripts/**` is another panel's this round, so the
  recipe is duplicated here rather than parameterised there.
* `boot-rtems-e10.sh` / `stop-rtems-e10.sh` — one guest, `-m 256M`, hostfwd
  `tcp 127.0.0.1:25164 → :5064`, MAC `52:54:00:12:39:10`. Records its own pid
  and kills only that pid after re-reading `/proc/<pid>/comm`; two other
  panels' `qemu-system-arm` guests were live on the box throughout and were
  never signalled.
* `rtemsload-e10.py` — the general load (channels, five DBR families, writes,
  subscriptions, the refusal sweep, connect/disconnect churn).
* `rtemschain-e10.py` — the inline `FLNK` chain writes, which is where the
  reading actually comes from.
* `rtemscmds-e10.py` — the per-command coverage check above.
