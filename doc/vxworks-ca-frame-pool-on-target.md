# The connection frame pool on target: does cross-delivery buffer reuse move the CA wall?

`8375ca36` gave each CA connection one send buffer (`FramePool`) whose lend
and return are enforced by `PooledFrame`'s `Drop` in the single drain owner,
replacing a fresh `Vec` per delivery. That is the port of C's
`cas_copy_in_header` reuse of the client's send buffer across deliveries.

The change adds one `std::sync::Mutex` per connection. On this platform a
`pthread` mutex allocates its semaphore lazily at first lock, and semaphore
exhaustion is reported as `EINVAL`, so "one more mutex per client" is not free
the way it is on a hosted target: it is a new claim on the same RTP object
arena the admission wall is already pressing against. Host tests cannot see
that. So the pool was measured against the wall directly.

## Method

Ten boots of `realtime-ca-ioc.vxe` as an RTP on `qemu-system-x86_64`, guest RAM
fixed at 1024M, `client` and `event` roster classes both `Medium`
(2,097,152 B declared per set) — the same configuration as the `Medium/Medium`
row of [the declared-stack sweep](vxworks-ca-admission-wall-vs-declared-stack.md),
whose wall is 58 sets. Five boots with the pool applied, five without,
alternating so a slow drift on the shared box could not land entirely in one
arm (`doc/vx-rig-e8/arms.sh`).

The pool was applied to the rig tree by anchor rather than by `git apply`
(`doc/vx-rig-e8/rigpool.py`): that tree is at `43ff13c7`, predating
`871b2de6`, and carries other rounds' uncommitted probe edits, so a patch
would either reject or drag those edits into the measurement. Each file is
backed up before the edit and restored from the backup afterwards; the restored
files were diffed byte-for-byte against their backups at the end of the
campaign.

Each run ramps CA clients until four consecutive admission failures, then
records the served count, the first failure verbatim, and the image's own
`POOLPROBE` line.

## Result: the wall does not move

Served clients, every run — `doc/vx-rig-e8/logs-pool-ab/arms.tsv` and the nine
`phaseramp-*.log` transcripts beside it:

| run | pool | served | `POOLPROBE seq=1` | first failure |
|---|---|---:|---|---|
| `scMedium` | no | 58 | `SETS=58 WORKERS=116 REFUSED=4` | EAGAIN refusal |
| `noPool2` | no | 58 | `SETS=58 WORKERS=116 REFUSED=4` | EAGAIN refusal |
| `nopoolM3` | no | 58 | `SETS=59 WORKERS=118 REFUSED=0` | 20 s client timeout |
| `nopoolM4` | no | 58 | `SETS=58 WORKERS=116 REFUSED=4` | EAGAIN refusal |
| `nopoolM5` | no | 58 | `SETS=58 WORKERS=116 REFUSED=4` | EAGAIN refusal |
| `poolMedium` | yes | 58 | `SETS=59 WORKERS=118 REFUSED=0` | 20 s client timeout |
| `poolMedium2` | yes | 58 | `SETS=58 WORKERS=116 REFUSED=4` | EAGAIN refusal |
| `poolM3` | yes | 58 | `SETS=58 WORKERS=116 REFUSED=4` | EAGAIN refusal |
| `poolM4` | yes | 58 | `SETS=58 WORKERS=116 REFUSED=4` | EAGAIN refusal |
| `poolM5` | yes | 58 | `SETS=58 WORKERS=116 REFUSED=4` | EAGAIN refusal |

58 served in 10 of 10 runs, both arms. The per-connection mutex costs no
admitted client at this wall, and the retained buffer costs none either.

The one run per arm that reports `SETS=59` and a client timeout instead of a
refusal is the admission gate letting one set past itself; that is a property
of the gate, not of the pool — it occurs once in five runs *with* the pool and
once in five runs *without* it, with a byte-identical failure signature. It is
written up separately in
[the admission gate finding](vxworks-ca-admission-gate-is-not-a-ceiling.md).

## What this does not measure

- **Semaphore headroom below the wall.** The wall is unmoved at 58 clients, and
  at 58 clients the pool's mutexes are 58 of the arena's objects. This says
  nothing about a build that raises the client ceiling: the pool's semaphore
  cost is linear in connections and would arrive alongside everything else
  that is.
- **Steady-state allocation.** The A/B measures the admission ceiling, not
  delivery throughput or heap churn. `MEM_USED` is not comparable between the
  two arms here because the faulting runs sampled it with clients still held
  and the refusing runs sampled it after the ramp had released them.
- **Classes other than `Medium/Medium`.** One row of the sweep, at one guest
  RAM size.

## Reproducing

```
scp doc/vx-rig-e8/rigpool.py doc/vx-rig-e8/arms.sh coding-agent@<box>:~/vx-rig-e8/
ssh coding-agent@<box> 'cd ~/vx-rig-e8 && ./arms.sh'   # ~35 min, six boots
```

`arms.sh` leaves the rig tree un-pooled. It boots through `boot-e8.sh` and
stops through `stop-e8.sh`, both of which act on recorded pids only — the box
is shared and carries long-lived `qemu-system-arm` guests that must survive.
