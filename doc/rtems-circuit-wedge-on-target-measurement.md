# The circuit wedge, measured on real RTEMS (topology B, power cut)

Status: MEASURED 2026-07-23 on the RTEMS/QEMU box (`192.168.2.128`).
Subject: the `CircuitDeathGuard` fix, merge `d0534d95`
(`fix(ca-client): retire a circuit the instant its pump dies, not at next
CreateChannel`, `0366e2a7`).

The fix landed with host-only (tokio) tests. The defect it closes was
*discovered* on the target and can only be *disproved* there, because the
leak it removes is a target file descriptor held across a link outage. This
run is the on-target A/B.

**Naming note (2026-07-25).** The target IOC binaries were later renamed —
`rtems-ca-ioc` → `realtime-ca-ioc`, `rtems-pva-ioc` → `realtime-pva-ioc`.
Every old name below is left exactly as captured, because this file is a
record of the tree as it stood, not a description of it as it stands.

## 1. What was measured, and against what

Two `rtems-ca-ioc` images, built from the same tree, the same nightly and the
same custom target spec (`has-thread-local: true`, per
[rtems-tls-spec-deviation.md](rtems-tls-spec-deviation.md)), differing by
**exactly the one commit under test**:

| image | tree | wedge fix |
| --- | --- | --- |
| `ctl-down.exe` | box `box-measure2` @ `419b59d5d7c` | no |
| `wedge-down.exe` | box `box-wedge` @ `12c18624c71` (= `box-measure2` + `0366e2a7`) | yes |

Both `--no-default-features --features client-core,bringup-probes`,
`C6_NAME_SERVERS=10.0.2.15:5064`. Keeping the target spec identical matters:
the spec deviation was adopted the same day, and an A/B that also flipped the
spec could not tell the fix from the TLS model.

## 2. Topology and the cut

Topology B, two QEMU guests on one hub, per the topology-B rig conventions
recorded on the box in `~/rtems-bringup/topoB/README.txt`:

* guest1 `upstream-ca.exe` — serves `UPSTREAM:AI/AO/FAST/OTHER`, `10.0.2.15`
* guest2 the image under test — CA client, four CP input links onto those PVs,
  name service over TCP at `10.0.2.15:5064`, `10.0.2.16`
* `-serial null -serial mon:stdio`, monitor on telnet, `-nic hubport` /
  `-nic socket,connect=`, distinct MAC per guest per rig

Phase walk (`~/rtems-bringup/topoB/run-wedge.sh`, own-pid kills via
`rigpid.sh` — the box is shared):

1. boot both, wait for `dhcp BOUND` on both consoles;
2. `set_link n1 off` **immediately** — take libslirp off the hub before the
   first CA dial (§4);
3. +60 s, +120 s marks: the client resolves and establishes;
4. **POWER CUT** — `set_link h2 off`, held 480 s, a mark each 60 s;
5. `set_link h2 on`, +150 s, final mark.

The cut is `set_link h2 off` on the *monitor*, not a hostfwd probe: a hostfwd
"success" is a SLIRP false positive and hostfwd is dead behind a hub port
anyway. Everything below is read off the serial consoles and the filter-dump
pcap.

## 3. Result

### 3.1 The descriptor floor during the outage

`FDPROBE` prints `FD_CNT` every ~10 s. The pre-connection baseline for this
image is **8** descriptors (3 chardev, 2 non-socket, the UDP beacon socket,
the CA listener, the CA UDP port). Each established circuit and each in-flight
dial socket adds one.

Both runs establish two circuits (`FD_CNT=10`) and, during the outage, hold at
most one dial in flight at a time (`dialpool workers=1`). So the *floor*
between dial attempts is the number of descriptors the client is holding for
nothing:

| | pre-cut | outage floor (between dials) | outage peak (dial in flight) | after restore |
| --- | --- | --- | --- | --- |
| control (no fix) | 10 | **9** | 10 | 10 |
| fixed | 10 | **8** | 9 | 10 |

Raw `seq:FD_CNT`, guest2, the four inter-dial gaps of the outage:

```
control  … 30:10 31:9  32:9  33:9  34:10 …  41:9  42:9  43:9  44:10 …
                52:9  53:9  54:9  55:10 …  62:9  63:9  64:9  65:10 …
fixed    … 31:9  32:8  33:8  34:8  35:9  …  42:8  43:8  44:8  45:9  …
                53:8  54:8  55:8  56:9  …  63:8  64:8  65:8  66:10 …
```

The two series are the same shape offset by exactly **one descriptor**, in
every gap, for the whole 480 s. That one descriptor is the wedge.

### 3.2 Which descriptor, and for how long

`FDCENSUS` names it. Pre-cut, both runs hold two established circuits.
After the cut:

**control** — the name-service circuit is retired; the data circuit is not:

```
c6-18  fd=47 local=10.0.2.16:19467 peer=10.0.2.15:5064 mode=0140666
c6-18  fd=48 local=10.0.2.16:32596 peer=10.0.2.15:5064 mode=0140666
--- POWER CUT ---
c6-24  fd=48 local=10.0.2.16:32596 peer=none  mode=0140000   <-- zombie
c6-24  fd=51 local=10.0.2.16:62187 peer=none  mode=0140666   (dial in flight)
c6-30  fd=48 … peer=none mode=0140000
c6-36  fd=48 … peer=none mode=0140000
c6-42  fd=48 … peer=none mode=0140000
c6-48  fd=48 … peer=none mode=0140000
c6-54  fd=48 … peer=none mode=0140000
c6-60  fd=48 … peer=none mode=0140000
c6-66  fd=48 … peer=none mode=0140000
--- set_link h2 on ---
c6-72  (fd=48 gone; two fresh circuits established)
```

`fd=48` is held `peer=none, mode=0140000` across **8 consecutive censuses**,
~480 s — the entire outage — and is released only after power returns. It is
out of the redial path for the whole time, exactly the signature the fix
describes.

**fixed** — both circuits are gone by the first post-cut census, and the only
TCP descriptor left is the rotating in-flight dial socket, a fresh ephemeral
port per attempt and `mode=0140666` (a live SYN, not a zombie):

```
c6-18  fd=46 local=10.0.2.16:57574 peer=10.0.2.15:5064 mode=0140666
c6-18  fd=47 local=10.0.2.16:59023 peer=10.0.2.15:5064 mode=0140666
--- POWER CUT ---
c6-24  fd=50 local=10.0.2.16:37010 peer=none mode=0140666
c6-30  fd=50 local=10.0.2.16:37010 peer=none mode=0140666
c6-36  fd=51 local=10.0.2.16:37946 peer=none mode=0140666
c6-48  fd=52 local=10.0.2.16:35718 peer=none mode=0140666
c6-60  fd=53 local=10.0.2.16:10999 peer=none mode=0140666
--- set_link h2 on ---
c6-66  fd=54 local=10.0.2.16:29823 peer=10.0.2.15:5064 mode=0140666
c6-66  fd=55 local=10.0.2.16:47849 peer=10.0.2.15:5064 mode=0140666
```

Zero circuits survive the cut. `c6-42` and `c6-54` carry no non-listener TCP
line at all — the census landed in a gap between dial attempts, which is only
possible when nothing is being held.

**Retirement latency, fixed image.** The cut is at 23:09:23, when the latest
guest2 probe is `seq=17`. `FD_CNT` goes 10 → 9 at `seq=24`; `seq=23` (the
latest at the 23:10:24 mark) still reads 10. At ~10 s per seq that puts the
retirement of *both* circuits inside a single probe window, **cut + 62…71 s**.
That interval is the CA inactivity/echo watchdog deciding the peer is gone —
i.e. the guard fires as soon as the pump exits, and adds nothing.

### 3.3 The 75 s redial ladder

`DIALPROBE` times every dial from submit to resolve. After the cut, every
attempt runs to the BSD `TCPTV_KEEP_INIT` connect ceiling:

| rung | control `elapsed_ms` | fixed `elapsed_ms` |
| --- | --- | --- |
| 1 | 74998 | 74997 |
| 2 | 75000 | 75001 |
| 3 | 75010 | 75000 |
| 4 | 75010 | 75010 |
| restore | 15602 → connected | 3202 → connected |
| restore +1 | 6 → connected | 6 → connected |

Four rungs at **74997–75010 ms** in both images (`error:Connection timed out
(os error 116)`), i.e. 75.00 s ± 10 ms — the ladder is a kernel timer and the
fix does not perturb it. The first rung after `set_link h2 on` connects, the
next connects in 6 ms, and both images end with two circuits and all four
links `connected=true` (`C6 seq=80 link pv=UPSTREAM:* connected=true`).

**What this does and does not prove.** The four ladder rungs are the
name-service circuit's. A CA data circuit is created only from a resolved
search, and the search is itself blocked on the down name-service circuit, so
no second dial can be in flight during a total outage — `dialpool workers=1,
queued=0` in both images throughout. What the fix demonstrably changes is that
the dead data circuit is *retired and its socket freed* (§3.2), which is what
puts it back on the redial path; the redial itself lands on the first rung
after connectivity returns. The claim proven here is the retirement, measured
as a descriptor, not a second concurrent dial.

## 4. Attribution: this is the power cut, not a forged RST

libslirp forges RSTs (src MAC `52:55:0a:00:02:02`) into guest↔guest TCP flows
it does not own, so before believing any dial abort the pcap must be read with
`tcpdump -e` and the source MAC checked. Both runs were dumped
(`wedgefix.pcap`, `wedgectl.pcap`, filter-dump on the hub port).

**fixed run** — 8 frames from `52:55:0a:00:02:02`, all of them DHCP replies at
23:06:13 and 23:06:16, i.e. before `set_link n1 off` at 23:06:21. **Zero
forged RSTs.** The two RST frames in the capture both carry the downstream
guest's own MAC `52:54:00:12:35:0b`. The measurement is clean.

**control run** — 8 DHCP replies plus **3 forged RSTs** at 23:09:01.62–.66,
`52:55:0a:00:02:02` forging `10.0.2.16.37726 > 10.0.2.15.5064 [R]`, one second
before `set_link n1 off` at 23:09:02. This is a race in the rig, not in the
client: one dial (`DIALPROBE n=3`, resolved in 6 ms) landed inside the slirp
window. It does **not** touch the finding — the forged RSTs hit local port
`37726`, whereas the zombie is local port `32596`, and they occur at 23:09:01,
**three minutes before** the power cut at 23:12:03. The wedged circuit is a
victim of `set_link h2 off` alone.

## 5. Verdict

The `CircuitDeathGuard` fix is confirmed on real RTEMS. Without it, a CA data
circuit killed by a power cut after establishment leaks one descriptor
(`peer=none, mode=0140000`) for the entire outage; with it, both circuits are
retired within one ~10 s probe window of the CA watchdog declaring the peer
dead, the descriptor floor returns to the pre-connection baseline of 8, and
recovery on power restore is unchanged.

## 6. Reproduction

On the box:

```
# images (both through the has-thread-local spec)
~/rtems-bringup/build-measure.sh topoB/ctl-down.exe   10.0.2.15:5064   # control
~/rtems-bringup/build-wedge.sh   topoB/wedge-down.exe 10.0.2.15:5064   # fixed

# rig: tag image monport sockport mac-suffix outage
cd ~/rtems-bringup/topoB
./run-wedge.sh wedgefix wedge-down.exe 4451 8021 35 480
./run-wedge.sh wedgectl ctl-down.exe   4452 8022 36 480
```

Artefacts on the box under `~/rtems-bringup/topoB/`:
`wedgefix-{g1,g2}.log`, `wedgefix-phases.txt`, `wedgefix.pcap`, and the
`wedgectl-*` counterparts. The pre-fix run of 2026-07-23 that first showed the
wedge is `fd3-*` in the same directory (`fd=47`, local port `60319`, held for
660 s).

Never blanket-`pkill` on that box — `run-wedge.sh` records its own two pids and
kills only those, per
[rtems-measurement-rig-shared-box-kill-safety.md](rtems-measurement-rig-shared-box-kill-safety.md).
