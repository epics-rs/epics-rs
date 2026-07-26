# Test IOCs deployed on the RTEMS/QEMU box (.128), left running

**Date:** 2026-07-24
**Tree:** integration tip `ad5d77bc` (the `epics-libcom-rs` extraction, issue #55)
**Box:** `coding-agent@192.168.2.128`
**Status:** both guests running at the time of writing; this file is the record
of *where* they are, so a later panel does not have to rediscover it or, worse,
kill them looking for its own.

The box is shared. Every pid below is recorded in a pidfile and was started by
this rig; **nothing else may be killed**, and these two are killed only through
`rigpid.sh` (`rig_kill_own`), never by `pkill`.

---

## 1. What is running

| | CA IOC | QSRV (PVA) IOC |
|---|---|---|
| binary | `realtime-ca-ioc` | `realtime-pva-ioc` |
| image | `~/rtems-bringup/caioc-deploy.exe` (7,969,572 B) | `~/rtems-bringup/qsrvioc-deploy.exe` (11,079,980 B) |
| features | `--no-default-features --features client-core` | `--no-default-features --features qsrv-core,pvalink,bringup-probes` |
| guest port | TCP+UDP 5064 | TCP 5075, UDP search 5076 |
| host reachable at | `127.0.0.1:5064` | `127.0.0.1:5075` |
| guest MAC | `52:54:00:12:39:0a` | `52:54:00:12:39:0b` |
| pidfile | `~/rtems-bringup/deploy/ca.qemu.pid` | `~/rtems-bringup/deploy/qsrv.qemu.pid` |
| console log | `~/rtems-bringup/deploy/ca.log` | `~/rtems-bringup/deploy/qsrv.log` |
| records served | 3 | 27 records + 2 QSRV groups |

Boot rig: `~/rtems-bringup/deploy/boot-deploy.sh <image> <hostport> <guestport>
<tag> <mac-last-byte>`. It `rig_kill_own`s a previous run of *itself* by
recorded pid, starts one `setsid` qemu, records the pid, waits for
`dhcp BOUND`, and **exits without killing** — leaving the guest up is the point.

## 2. Why the host port equals the guest port

The first attempt forwarded `127.0.0.1:15164 -> guest 5064` and `caget` failed
with *"Virtual circuit disconnect", Context "127.0.0.1:5064"*. That is not a
transport fault: a CA name-server SEARCH reply carries **the server's own TCP
port**, so `libca` resolves the PV through `127.0.0.1:15164` and then dials
`127.0.0.1:5064` — a port SLIRP was not forwarding. PVA search replies carry
the server port the same way.

So a hostfwd whose host port differs from the guest port resolves names and
then cannot connect. Both guests are therefore forwarded **port-to-port**
(`hostfwd=tcp:127.0.0.1:5064-:5064`, `…:5075-:5075`), which is also the shape a
client expects from a real IOC. Both ports were confirmed free (`ss -lnt`)
before binding; the only pre-existing EPICS listener on the box is a
`caRepeater` on UDP 5065, which neither guest touches.

## 3. Build wiring

Built through the wired scripts' spec plumbing (`doc/rtems-tls-spec-deviation.md`):
`--target "$(rtems-tls-spec.sh)" -Zjson-target-spec` +
`CARGO_TARGET_ARMV7_RTEMS_EABIHF_TLS_LINKER=arm-rtems6-gcc`, `set -e -o pipefail`.

`build-ca.sh` and `build-qsrv.sh` both `cd $HOME/epics-rs`, which is another
panel's checkout (branch `box-measure2`, with uncommitted heapattr-probe edits).
Switching it would have destroyed that panel's work, so the two scripts were
copied to `build-ca-deploy.sh` / `build-qsrv-deploy.sh` with exactly two edits
each — `cd $HOME/wedge-wt` and the staged output name. `diff` against the
originals shows those three lines and nothing else. `~/wedge-wt` is this
panel's own worktree, moved to branch `box-deploy` = `ad5d77bc`.

Both builds exit 0, and `epics-libcom-rs v0.24.3` appears in the compile list of
both, so the extracted crate is in both images.

These are dev images; for measured release/strip/LTO sizes on this target (and
on vxworks), see `doc/vxworks-port.md` §5.5.

## 4. Smoke results

Captured before the binaries were renamed to `realtime-ca-ioc` /
`realtime-pva-ioc`, so the `RTEMS:MSG` values below still read the old names —
that field carries the string the running image printed, and rewriting it here
would make this section report a run that never happened.

Client tools: pvxs and EPICS base are installed on the box but not on `PATH` —
`~/rtems-bringup/pvxs-build/pvxs/bin/linux-x86_64/` and
`~/rtems-bringup/pvxs-build/base/bin/linux-x86_64/`.

CA, with `EPICS_CA_NAME_SERVERS=127.0.0.1:5064`, `EPICS_CA_AUTO_ADDR_LIST=NO`:

```
$ caget-rs RTEMS:AO RTEMS:LO RTEMS:MSG          # exit 0
RTEMS:AO                       1.5
RTEMS:LO                       7
RTEMS:MSG                      rtems-ca-ioc
$ caput-rs RTEMS:AO 2.75                        # exit 0   Old 1.5 -> New 2.75
$ caget-rs RTEMS:AO                             # exit 0   2.75  (readback)
$ camonitor-rs RTEMS:AO                         # 2 updates: initial + the put
RTEMS:AO   <undefined> 1.5 UDF NO_ALARM
RTEMS:AO   2014-04-14 07:33:26.475331 2.75
$ caget RTEMS:AO RTEMS:LO RTEMS:MSG             # C base caget, exit 0, same values
```

The guest clock reads 2014 — the RTEMS clock is not NTP-synced on this board;
unrelated to the deployment, and it is why the monitor timestamp looks wrong.

PVA, with `EPICS_PVA_NAME_SERVERS=127.0.0.1:5075`, `EPICS_PVA_AUTO_ADDR_LIST=NO`:

```
$ pvxinfo RTEMS:PVA:AO      # exit 0, struct "epics:nt/NTScalar:1.0"
$ pvxget  RTEMS:PVA:AO RTEMS:PVA:LO RTEMS:PVA:MSG    # exit 0
    value double = 1.5 / int32_t = 7 / string = "rtems-pva-ioc"
$ pvxinfo RTEMS:PVA:GRP     # exit 0, struct "rtems:demo/Group:1.0"
$ pvxget  RTEMS:PVA:GRP     # exit 0
    setpoint.value double = 1.5   count int32_t = 7
    message string = "rtems-pva-ioc"   record._options.atomic bool = true
$ pvxmonitor RTEMS:PVA:V0   # 121 updates in 12 s ~= the record's SCAN .1 second
```

`pvxinfo` is what proves the group id, not `pvxget`: the default Delta format
never prints the top-level struct id.

## 5. Known non-serving records on the QSRV guest

`build-qsrv.sh` carries `bringup-probes`, so three stage-5 probe records are
loaded whose links name a host-side upstream IOC that is **not** deployed:

```
STAGE5 seq=23 record RTEMS:PVA:DOWN  VAL=Ok("0") SEVR=Ok("3") STAT=Ok("17")
STAGE5 seq=23 record RTEMS:PVA:DOWN2 VAL=Ok("0") SEVR=Ok("3") STAT=Ok("17")
STAGE5 seq=23 record RTEMS:PVA:UPLNK VAL=Ok("0") SEVR=Ok("3") STAT=Ok("17")
```

`SEVR=3 STAT=17` is INVALID/LINK — the expected state of a `pva://` link with
no upstream. The other 24 records and both groups serve. Deploying an upstream
IOC named `UPSTREAM:*` would clear them; that is a separate rig, not part of
this deployment.

## 6. How to stop them

```
cd ~/rtems-bringup/deploy
. ~/rtems-bringup/rigpid.sh
rig_pidfile "$PWD/ca.qemu.pid";   rig_kill_own
rig_pidfile "$PWD/qsrv.qemu.pid"; rig_kill_own
```

Never `pkill qemu-system-arm` and never `pkill -f` — other panels run guests on
this box, and `pkill -f` additionally matches the ssh command line running the
script.
