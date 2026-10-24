# rt-probe

PREEMPT_RT measurement harness for epics-rs. Each subcommand boots an
in-process IOC (`IocBuilder` database with a `.1 second` calc record
`RT:SCAN` and an ao `RT:AO`, plus `CaServer` / `PvaServer`), measures one
thing, and prints the distribution:

```
n= min= mean= p50= p90= p99= p99.9= max= (us)
```

It ports no C module; it is the rig behind the SCHED_FIFO and
priority-inheritance work. Absolute numbers from a QEMU/KVM guest are not
bare-metal numbers; compare runs against each other.

Linux and the tokio backend only. On another OS the binary prints a message
and exits 2; built with `EPICS_RS_BUILD_EXEC_BACKEND=thread` it refuses to
run, because that backend has no CA/PVA server.

## Subcommands

| Command | Measures | Options (default) |
|---------|----------|-------------------|
| `scan-jitter` | deviation of `RT:SCAN` timestamps from the 100 ms period | `--samples 300` |
| `scan-decomp` | the same chain split into scan leg / CA hop / full chain | `--samples 2000` |
| `serve` + `watch` | `scan-decomp` split into an IOC process and a client process, each under its own `chrt` | `--ca-port 5164`, `watch --samples 2000` |
| `ca-latency` | put → CA monitor delivery | `--samples 500 --hogs 0` |
| `pva-latency` | put → PVA monitor delivery | `--samples 500 --hogs 0` |
| `rt-policy` | whether an IOC thread gets SCHED_FIFO under `EPICS_ALLOW_POSIX_THREAD_PRIORITY_SCHEDULING` (alias `EPICS_RS_ALLOW_RT_PRIORITY`): `enter_ioc_thread`'s verdict and the kernel's `sched_getscheduler` | none |
| `pi-proof` | three-priority inversion on the record gate (`PvDatabase::lock_record`) | `--samples 200 --low 10 --med 30 --high 50 --cpu 6` |

`--hogs N` adds N background CPU-hog threads. `serve` never binds 5064, so
it does not answer for the site's CA namespace. `pi-proof` pins its three
SCHED_FIFO threads to `--cpu`, which is not 0 on purpose: a runaway FIFO
band on the boot CPU starves the housekeeping threads there.

## Run

```sh
cargo run --release -p rt-probe -- scan-jitter --samples 300
cargo run --release -p rt-probe -- ca-latency --samples 500 --hogs 4

EPICS_ALLOW_POSIX_THREAD_PRIORITY_SCHEDULING=NO cargo run --release -p rt-probe -- rt-policy
```

Output of `scan-jitter --samples 20` and of `rt-policy` with the switch unset:

```
scan-jitter |dev from 100ms|: n=20 min=4.4 mean=55.8 p50=54.7 p90=96.1 p99=126.6 p99.9=126.6 max=126.6 (us)
rt-policy: enter_ioc_thread(CaServerLow) verdict=Realtime; kernel policy=SCHED_FIFO prio=20
```

The SCHED_FIFO arms (`rt-policy` with the switch on, `pi-proof`) need
CAP_SYS_NICE or root.

Split rig, one process per scheduling class:

```sh
chrt -f 80 target/release/rt-probe serve --ca-port 5164 &
chrt -f 70 target/release/rt-probe watch --ca-port 5164 --samples 2000
```

## Features

- `linux-rt` — forwards to `epics-base-rs/linux-rt`, which makes
  `PriorityInheritanceMutex` a `pthread_mutex_t` with `PTHREAD_PRIO_INHERIT`.
  `pi-proof` prints `PI mutex active in this build = true|false`; run it once
  with and once without the feature for the PI-on / PI-off arms:

  ```sh
  cargo run --release -p rt-probe --features linux-rt -- pi-proof
  cargo run --release -p rt-probe -- pi-proof
  ```

## Not covered

No RTEMS build, and no C counterpart to compare against in the
same binary.
