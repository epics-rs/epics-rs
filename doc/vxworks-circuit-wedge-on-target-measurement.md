# The circuit wedge, measured on real VxWorks 7 (row E9)

Status: MEASURED 2026-07-26 on the bring-up box (`192.168.2.128`), QEMU
`itl_generic_3_0_0_5`, Wind River `wrsdk-vxworks7-qemu-1.17.0`.
Subject: the `CircuitDeathGuard` fix, `0366e2a7` (`fix(ca-client): retire a
circuit the instant its pump dies, not at next CreateChannel`).

This is the VxWorks counterpart of
[rtems-circuit-wedge-on-target-measurement.md](rtems-circuit-wedge-on-target-measurement.md).
It exists because the RTEMS run proved the fix against *libbsd*, and the two
numbers it turns on — the connect ladder and the descriptor floor — are
properties of the target's own TCP stack rather than of the fix, so neither
could be assumed to carry over. Measured here, the ladder constant does
coincide with the RTEMS one and the readings around it do not; §6 says which.

## 1. What was measured, and against what

Two `realtime-ca-ioc` images built from the same tree, the same rustc and the
same SDK, differing by **exactly the one commit under test**:

| image | branch | tip | wedge fix |
| --- | --- | --- | --- |
| `wedge-ctl.vxe` | `e9/control-no-wedge-fix` | `5dbcadd0` | no |
| `wedge-fix.vxe` | `caucus/58EWEJWV91/e9-wedge-0f4213ca-1` | `d4085caa` | yes |

```
$ git diff --stat d4085caa 5dbcadd0
 crates/epics-ca-rs/src/client/transport.rs | 476 ++++-------------------------
 1 file changed, 66 insertions(+), 410 deletions(-)
```

One file, and it is the file `0366e2a7` touched. The patch that makes the
control image is committed alongside this document as
[vx-circuit-wedge-control.patch](vx-circuit-wedge-control.patch).

Both `--no-default-features --features client-core,bringup-probes`,
`C6_NAME_SERVERS=10.0.2.2:45064`.

### 1.1 Three substitutions against the RTEMS protocol, and why

**(a) The control commit.** The RTEMS run's pair was box branch
`419b59d5d7c` versus `12c18624c71`. Neither is usable here:

* `12c18624c71` is not a revision in this repository at all — it was a
  box-local branch commit on the RTEMS rig, and `git rev-parse` on it fails.
* `419b59d5d7c` (`caucus/F5819ABWS0/box-measure-83b3e6e7-1`) predates the
  VxWorks port entirely: 4 of its files carry a `target_os = "vxworks"` arm
  against 27 at `e971e26a`, and its libc pin is the RTEMS-era rev, so `std`
  itself does not build for `x86_64-wrs-vxworks` from that tree.

The control was therefore reconstructed at the *current* tip instead: HEAD with
`0366e2a7` reverted (`f7727168`), which preserves the property the protocol
actually depends on — the two images differ by exactly the commit under test —
while dropping the property that cannot be had, namely that the pair be the
same two commits the RTEMS run used. The revert conflicted in one region, the
`circuit_retirement_tests` module the fix itself added and `17072fac` later
touched; it was resolved to the parent-of side, i.e. the fix's own tests go
away with the fix.

**(b) Target-spec identity is void.** The RTEMS pair had to hold the custom
`has-thread-local` JSON spec constant. `x86_64-wrs-vxworks` is a *builtin*
rustc triple with no spec file, so the clause has nothing to bind to. Identity
here is the toolchain and the SDK, both held constant across the two builds:

```
rustc 1.99.0-nightly (87e5904f5 2026-07-20)
commit-hash: 87e5904f5eb6398af6b22eac2802c78934260c48
LLVM version: 22.1.8

wrsdk-vxworks7-qemu-1.17.0   version 26.03  bsp itl_generic_3_0_0_5
arch i86  compiler llvm  data_model LP64  processor_model SMP
```

**(c) Topology.** The RTEMS run used two guests on a QEMU hub and cut a hub
port. The VxWorks rig on this box is a single guest on SLIRP; §2 states what
replaced it and §4 settles the hazard that substitution carried.

## 2. Topology and the cut

One VxWorks guest; the CA peer is a **host process**, `softioc-rs`, on host
port 45064. SLIRP aliases the host at `10.0.2.2`, so the guest reaches it at
`10.0.2.2:45064` with no hostfwd in the path — this is an outbound flow.

* guest `10.0.2.15` — the image under test: CA client, four CP input links onto
  `UPSTREAM:AI/AO/FAST/OTHER`, name service over TCP at `10.0.2.2:45064`
* host `10.0.2.2:45064` — `softioc-rs --db upstream-e9.db --port 45064`
* `-monitor unix:/tmp/vxmon-e9a.sock`, `id=n1` on the user netdev

**The cut is `set_link n1 off` on the QEMU monitor** — the guest's own link,
not anything done to the host process. Phase walk
(`~/vx-rig-e9/run-wedge-e9.sh`, §6):

1. boot, `rtpSp` the image, wait for `link pv=UPSTREAM:OTHER connected=true`;
2. +70 s so at least one full `FDCENSUS` lands on the established state;
3. `pre-cut`, +20 s, then **POWER CUT** `set_link n1 off`;
4. held 480 s, a mark each 60 s;
5. `set_link n1 on`, +150 s in 30 s marks.

Choosing the link rather than a host-side `iptables` DROP is what makes §4
answerable: an `iptables` rule leaves the SLIRP hub alive and able to originate
frames, whereas with the link down QEMU delivers no frame of any kind to the
guest. A forged RST is structurally impossible here, not merely unobserved.

## 3. Result

### 3.1 The VxWorks redial ladder — the new number

`DIALPROBE` times every dial from submit to resolve. Fix image, verbatim, the
whole run:

```
DIALPROBE submit  n=1 target=10.0.2.2:45064 workers=1 queued=1 dialing=0
DIALPROBE resolve n=1 target=10.0.2.2:45064 elapsed_ms=50 outcome=connected
DIALPROBE submit  n=2 target=10.0.2.2:45064 workers=1 queued=1 dialing=0
DIALPROBE resolve n=2 target=10.0.2.2:45064 elapsed_ms=16 outcome=connected
--- POWER CUT (set_link n1 off), 14:27:04 ---
DIALPROBE submit  n=3 target=10.0.2.2:45064 workers=1 queued=1 dialing=0
DIALPROBE resolve n=3 target=10.0.2.2:45064 elapsed_ms=74950 outcome=error:host unreachable (os error 65)
DIALPROBE submit  n=4 target=10.0.2.2:45064 workers=1 queued=1 dialing=0
DIALPROBE resolve n=4 target=10.0.2.2:45064 elapsed_ms=74916 outcome=error:host unreachable (os error 65)
DIALPROBE submit  n=5 target=10.0.2.2:45064 workers=1 queued=1 dialing=0
DIALPROBE resolve n=5 target=10.0.2.2:45064 elapsed_ms=75000 outcome=error:host unreachable (os error 65)
DIALPROBE submit  n=6 target=10.0.2.2:45064 workers=1 queued=1 dialing=0
DIALPROBE resolve n=6 target=10.0.2.2:45064 elapsed_ms=75000 outcome=error:host unreachable (os error 65)
--- RESTORE (set_link n1 on), 14:35:06 ---
DIALPROBE submit  n=7 target=10.0.2.2:45064 workers=1 queued=1 dialing=0
DIALPROBE resolve n=7 target=10.0.2.2:45064 elapsed_ms=0 outcome=connected
DIALPROBE submit  n=8 target=10.0.2.2:45064 workers=1 queued=1 dialing=0
DIALPROBE resolve n=8 target=10.0.2.2:45064 elapsed_ms=16 outcome=connected
```

(The `submit`/`resolve` lines are printed without the column padding shown
here; the padding is added in this document only so the two halves line up.
Every field value is as captured.)

The control image ran the same ladder:

```
--- POWER CUT (set_link n1 off), 14:40:27 ---
DIALPROBE resolve n=3 target=10.0.2.2:45064 elapsed_ms=74900 outcome=error:timed out (os error 60)
DIALPROBE resolve n=4 target=10.0.2.2:45064 elapsed_ms=75000 outcome=error:host unreachable (os error 65)
DIALPROBE resolve n=5 target=10.0.2.2:45064 elapsed_ms=75000 outcome=error:host unreachable (os error 65)
DIALPROBE resolve n=6 target=10.0.2.2:45064 elapsed_ms=75000 outcome=error:host unreachable (os error 65)
```

**The measured VxWorks 7 connect ceiling is 74900–75000 ms — 75.0 s, −100/+0 ms
over eight rungs across the two images.** The RTEMS number was 74997–75010 ms.
Both stacks are BSD-derived and both land on the same `TCPTV_KEEP_INIT` = 75 s
connect ceiling; what the measurement shows is that the *constant* carries over
and the properties around it do not:

| | RTEMS (libbsd) | VxWorks 7 |
| --- | --- | --- |
| ceiling | 74997–75010 ms | 74900–75000 ms |
| spread | 13 ms over 4 rungs | 100 ms over 8 rungs |
| errno on ladder exhaustion | `Connection timed out (os error 116)` = `ETIMEDOUT`, 4 of 4 | `host unreachable (os error 65)` = `EHOSTUNREACH`, 7 of 8; `timed out (os error 60)` = `ETIMEDOUT`, 1 of 8 |
| `Instant` resolution | 1 s (quantized) | sub-second (`elapsed_ms=16`, `elapsed_ms=33`, `elapsed_ms=50`) |

The errno numbering is BSD's, not Linux's — `ETIMEDOUT` is 60 and
`EHOSTUNREACH` 65 here against 110 and 113 on Linux. **The errno is not stable
across attempts.** The two legs ran the identical topology and the identical
cut, and the one `ETIMEDOUT` rung is the control leg's *first* post-cut
attempt while its other three and all four of the fix leg's report
`EHOSTUNREACH`. So a caller on this target must not key on either value: what
is reproducible is that the dial fails at 75 s, not which of the two errnos
carries it. (Inference, offered as such and not measured: BSD's retransmit
timer reports `t_softerror` in preference to `ETIMEDOUT`, and taking the
guest's own link down makes ARP for the gateway fail, which is what records
`EHOSTUNREACH` as that soft error — an attempt that still had a valid ARP entry
when it started would fall through to `ETIMEDOUT`. Nothing in these runs probes
the ARP table, so that is where the explanation stops.)

The gap between one rung resolving and the next submitting is three probe
windows, ~30 s (`seq=29,30,31` then submit; `seq=40,41,42` then submit) — that
is the CA client's own retry backoff, and it is the same in both images. Both
images run exactly four rungs in a 480 s outage: 4 × 75 s + 4 × 30 s = 420 s.

### 3.2 The descriptor floor during the outage

`FDPROBE` prints `FD_CNT` every ~10 s. The pre-connection baseline for this
image is **5** descriptors (3 chardev, the CA TCP listener, the CA UDP port).
Established, it is 7: two TCP circuits to `10.0.2.2:45064`. Each in-flight dial
socket adds one.

| | pre-cut | outage floor (between dials) | outage peak (dial in flight) | after restore |
| --- | --- | --- | --- | --- |
| control (no fix) | 7 | **6** | 7 | 7 |
| fixed | 7 | **5** | 6 | 7 |

Raw `seq:FD_CNT`, fixed image, the whole outage (cut at `seq=12`, restore at
`seq=59`):

```
 1:7   2:7   3:7   4:7   5:7   6:7   7:7   8:7   9:7  10:7  11:7  12:7
13:7  14:7  15:7  16:7  17:7  18:6  19:6  20:6  21:6  22:6  23:6  24:6
25:6  26:6  27:6  28:6  29:5  30:5  31:5  32:6  33:6  34:6  35:6  36:6
37:6  38:6  39:6  40:5  41:5  42:5  43:6  44:6  45:6  46:6  47:6  48:6
49:6  50:5  51:5  52:5  53:6  54:6  55:6  56:6  57:6  58:6  59:6  60:5
61:5  62:5  63:7  64:7  65:7  66:7  67:7  68:7  69:7  70:7  71:7  72:7
73:7  74:7  75:7  76:7
```

Raw `seq:FD_CNT`, control image (cut at `seq=10`, restore at `seq=57`):

```
 1:7   2:7   3:7   4:7   5:7   6:7   7:7   8:7   9:7  10:7  11:7  12:7
13:7  14:7  15:7  16:7  17:7  18:7  19:7  20:7  21:7  22:7  23:7  24:6
25:6  26:6  27:7  28:7  29:7  30:7  31:7  32:7  33:7  34:7  35:6  36:6
37:6  38:7  39:7  40:7  41:7  42:7  43:7  44:7  45:6  46:6  47:6  48:7
49:7  50:7  51:7  52:7  53:7  54:7  55:6  56:6  57:6  58:7  59:7  60:7
61:7  62:7  63:7  64:7  65:7  66:7  67:7  68:7  69:7  70:7  71:7  72:7
73:7  74:7
```

### 3.3 Which descriptor, and for how long

`FDCENSUS` names it. Pre-cut both images hold the same two established
circuits. Fixed image, the last pre-cut census verbatim:

```
FDCENSUS begin tag=c6-12
FDCENSUS tag=c6-12 fd=0 kind=chardev mode=020666 rdev=0
FDCENSUS tag=c6-12 fd=1 kind=chardev mode=020666 rdev=0
FDCENSUS tag=c6-12 fd=2 kind=chardev mode=020666 rdev=0
FDCENSUS tag=c6-12 fd=3 kind=tcp so_type=1 listening=1 local=0.0.0.0:5064 peer=none(errno=57) mode=0140666
FDCENSUS tag=c6-12 fd=4 kind=udp so_type=2 listening=-1 local=0.0.0.0:5064 peer=none(errno=57) mode=0140666
FDCENSUS tag=c6-12 fd=5 kind=tcp so_type=1 listening=0 local=10.0.2.15:53951 peer=10.0.2.2:45064 mode=0140666
FDCENSUS tag=c6-12 fd=6 kind=tcp so_type=1 listening=0 local=10.0.2.15:53397 peer=10.0.2.2:45064 mode=0140666
FDCENSUS end tag=c6-12 open=7 max=1000
```

**fixed** — all eight outage censuses:

```
--- POWER CUT ---
FDCENSUS tag=c6-18 fd=5 kind=tcp so_type=1 listening=0 local=10.0.2.15:53951 peer=10.0.2.2:45064 mode=0140666
FDCENSUS end tag=c6-18 open=6 max=1000
FDCENSUS tag=c6-24 fd=5 kind=tcp so_type=1 listening=0 local=10.0.2.15:55817 peer=10.0.2.2:45064 mode=0140666
FDCENSUS end tag=c6-24 open=6 max=1000
FDCENSUS begin tag=c6-30
FDCENSUS end tag=c6-30 open=5 max=1000
FDCENSUS tag=c6-36 fd=5 kind=tcp so_type=1 listening=0 local=10.0.2.15:57885 peer=10.0.2.2:45064 mode=0140666
FDCENSUS end tag=c6-36 open=6 max=1000
FDCENSUS begin tag=c6-42
FDCENSUS end tag=c6-42 open=5 max=1000
FDCENSUS tag=c6-48 fd=5 kind=tcp so_type=1 listening=0 local=10.0.2.15:51538 peer=10.0.2.2:45064 mode=0140666
FDCENSUS end tag=c6-48 open=6 max=1000
FDCENSUS tag=c6-54 fd=5 kind=tcp so_type=1 listening=0 local=10.0.2.15:57696 peer=10.0.2.2:45064 mode=0140666
FDCENSUS end tag=c6-54 open=6 max=1000
FDCENSUS begin tag=c6-60
FDCENSUS end tag=c6-60 open=5 max=1000
--- RESTORE ---
FDCENSUS tag=c6-66 fd=5 kind=tcp so_type=1 listening=0 local=10.0.2.15:54235 peer=10.0.2.2:45064 mode=0140666
FDCENSUS tag=c6-66 fd=6 kind=tcp so_type=1 listening=0 local=10.0.2.15:53952 peer=10.0.2.2:45064 mode=0140666
FDCENSUS end tag=c6-66 open=7 max=1000
```

Three of the eight — `c6-30`, `c6-42`, `c6-60` — carry **no non-listener TCP
line at all** and read `open=5`, the pre-connection baseline: at those instants
the process held nothing whatever for the dead peer. That is only possible when
every circuit has been retired.

Two of the eight need reading precisely rather than being lumped in with the
rest. `fd=6` (`local=…:53397`), the data circuit, is gone from the *first*
post-cut census onward — never censused again. `fd=5` at `c6-18` is still
`local=…:53951`, the original name-service socket: it survives one census past
the data circuit and is gone by `c6-24`, where `fd=5` already carries a fresh
port. Every `fd=5` line from `c6-24` on is an in-flight dial socket, and its
port rotates on every attempt — `55817` → `57885` → `51538` → `57696` — so each
is a fresh SYN rather than a retained socket. None of the eight ever shows
`peer=none(errno=57)`.

**One VxWorks-specific reading note.** On RTEMS the zombie censused as
`peer=none mode=0140000`, and it was the `mode` that distinguished it: a live
socket read `0140666`. That discriminator does not work here. Both readings
below are from this run:

* a dead VxWorks socket keeps `mode=0140666`. `mode` never drops to `0140000`
  in either image, so it separates nothing.
* VxWorks `getpeername` fills in the destination for a socket still in
  `SYN_SENT`: every in-flight dial socket in both images censuses
  `peer=10.0.2.2:45064`, where the RTEMS ones censused `peer=none`.

The consequence is that on VxWorks `peer=none(errno=57)` — `ENOTCONN` — is a
*sharper* discriminator than it was on RTEMS, not a blunter one: it appears on
a socket that was connected and is no longer, and on nothing else. It occurs
**7 times in the control image and 0 times in the fix image**, on non-listener
descriptors — one per census for the whole outage, and never once in the fix. Alongside it, the local port is the second reading: a held circuit
keeps its original ephemeral port across censuses, a redial rotates it.

**control** — the wedge. `fd=6`, local port `56754`, is held
`peer=none(errno=57)` across **seven consecutive censuses**, `c6-18` through
`c6-54` — 360 s of the 480 s outage, and it was already dead before `c6-18`
— while `fd=5` beside it rotates through a fresh dial port every attempt
(`54840` → `65228` → `58605` → `56949`). The zombie is released only after
the link returns:

```
--- POWER CUT ---
FDCENSUS tag=c6-18 fd=5 kind=tcp so_type=1 listening=0 local=10.0.2.15:54840 peer=10.0.2.2:45064 mode=0140666
FDCENSUS tag=c6-18 fd=6 kind=tcp so_type=1 listening=0 local=10.0.2.15:56754 peer=none(errno=57) mode=0140666
FDCENSUS end tag=c6-18 open=7 max=1000
FDCENSUS tag=c6-24 fd=6 kind=tcp so_type=1 listening=0 local=10.0.2.15:56754 peer=none(errno=57) mode=0140666
FDCENSUS end tag=c6-24 open=6 max=1000
FDCENSUS tag=c6-30 fd=5 kind=tcp so_type=1 listening=0 local=10.0.2.15:65228 peer=10.0.2.2:45064 mode=0140666
FDCENSUS tag=c6-30 fd=6 kind=tcp so_type=1 listening=0 local=10.0.2.15:56754 peer=none(errno=57) mode=0140666
FDCENSUS end tag=c6-30 open=7 max=1000
FDCENSUS tag=c6-36 fd=6 kind=tcp so_type=1 listening=0 local=10.0.2.15:56754 peer=none(errno=57) mode=0140666
FDCENSUS end tag=c6-36 open=6 max=1000
FDCENSUS tag=c6-42 fd=5 kind=tcp so_type=1 listening=0 local=10.0.2.15:58605 peer=10.0.2.2:45064 mode=0140666
FDCENSUS tag=c6-42 fd=6 kind=tcp so_type=1 listening=0 local=10.0.2.15:56754 peer=none(errno=57) mode=0140666
FDCENSUS end tag=c6-42 open=7 max=1000
FDCENSUS tag=c6-48 fd=5 kind=tcp so_type=1 listening=0 local=10.0.2.15:56949 peer=10.0.2.2:45064 mode=0140666
FDCENSUS tag=c6-48 fd=6 kind=tcp so_type=1 listening=0 local=10.0.2.15:56754 peer=none(errno=57) mode=0140666
FDCENSUS end tag=c6-48 open=7 max=1000
FDCENSUS tag=c6-54 fd=5 kind=tcp so_type=1 listening=0 local=10.0.2.15:56949 peer=10.0.2.2:45064 mode=0140666
FDCENSUS tag=c6-54 fd=6 kind=tcp so_type=1 listening=0 local=10.0.2.15:56754 peer=none(errno=57) mode=0140666
FDCENSUS end tag=c6-54 open=7 max=1000
--- RESTORE ---
FDCENSUS tag=c6-60 fd=5 kind=tcp so_type=1 listening=0 local=10.0.2.15:51562 peer=10.0.2.2:45064 mode=0140666
FDCENSUS tag=c6-60 fd=6 kind=tcp so_type=1 listening=0 local=10.0.2.15:55541 peer=10.0.2.2:45064 mode=0140666
FDCENSUS end tag=c6-60 open=7 max=1000
```

**Retirement latency, fixed image.** The cut is at 14:27:04, latest probe
`seq=12`. `C6 seq=15` still reads `circuits=Some(1)` with all four links
`connected=true`; `seq=16` reads `connected=false` on all four; `seq=18` reads
`circuits=Some(0)` and `FD_CNT` drops 7 → 6. At ~10 s per seq that puts the
data circuit's retirement at **cut + 60…70 s**, which is the CA
inactivity/echo watchdog declaring the peer dead — the guard fires when the
pump exits and adds nothing to it. The RTEMS run measured cut + 62…71 s for
the same transition.

### 3.4 Recovery

Fixed image, after `set_link n1 on` at 14:35:06: `n=7` connects in 0 ms, `n=8`
in 16 ms, and the run ends with

```
C6 seq=75 links=4 circuits=Some(1)
C6 seq=75 dialpool workers=1 attempts=8 queued=0 dialing=0 MEM_FREE=-1 MEM_USED=17047552
C6 seq=75 link pv=UPSTREAM:AI connected=true
C6 seq=75 link pv=UPSTREAM:AO connected=true
C6 seq=75 link pv=UPSTREAM:FAST connected=true
C6 seq=75 link pv=UPSTREAM:OTHER connected=true
C6 seq=75 record RTEMS:CA:DOWN VAL=Ok("787") SEVR=Ok("0") STAT=Ok("0")
C6 seq=75 record RTEMS:CA:DOWN2 VAL=Ok("787") SEVR=Ok("0") STAT=Ok("0")
C6 seq=75 record RTEMS:CA:UPLNK VAL=Ok("0") SEVR=Ok("3") STAT=Ok("17")
C6 seq=75 record RTEMS:CA:FAST VAL=Ok("7875") SEVR=Ok("0") STAT=Ok("0")
C6 seq=75 record RTEMS:CA:C8 VAL=Ok("7883") SEVR=Ok("0") STAT=Ok("0")
C6 seq=75 record RTEMS:CA:OTHER VAL=Ok("787") SEVR=Ok("0") STAT=Ok("0")
```

(`MEM_FREE=-1` is the VxWorks stats backend reporting the free/max figures as
unavailable, per [vxworks-port.md](vxworks-port.md); `RTEMS:CA:*` are the probe
database's record names, unchanged from the RTEMS rig. `RTEMS:CA:UPLNK` is
`SEVR=3 STAT=17` in every sample of both runs, before the cut included — it is
an output link the probe DB never drives, not an outage artefact.)

Control image: identical in shape. `n=7` connects in 0 ms, `n=8` in
16 ms, `FD_CNT` returns to 7 at `seq=58` and stays there through `seq=74`, and
`c6-60`/`c6-66`/`c6-72` all show two established circuits on fresh ports
(`51562`, `55541`). The wedge costs a descriptor for the duration of the
outage; it does not stop the client recovering when the peer comes back.

## 4. Attribution: this is a blackhole, not a forged RST

The hazard the RTEMS rig hit was libslirp forging RSTs into guest↔guest flows
it does not own. That rig's answer was a `tcpdump -e` source-MAC check after
the fact. This rig answers it two ways, and neither is a MAC check:

**Structurally.** The cut is `set_link n1 off` on the netdev the guest's only
NIC is attached to. QEMU stops delivering frames on that link in both
directions. There is no path by which libslirp, or anything else, can put a
frame in front of the guest's TCP while the link is down — a forged RST is not
merely absent, it is unrepresentable.

**Empirically, from the outcome strings.** A TCP that receives an RST for its
SYN aborts the connect *at once* — that is the property that matters here, and
it holds whatever errno the target renders. All eight post-cut dials across the
two images instead ran **74900–75000 ms** and ended in `host unreachable`
(7 of 8) or `timed out` (1 of 8): the SYN-retransmit ladder ran to its ceiling
with nothing coming back. For scale, the same images' pre-cut dials over the
same path resolve in 16–50 ms, so the 75 s is the ladder rather than the rig
being slow. Zero of the sixteen `DIALPROBE resolve` lines in the two runs
reports a refusal or a reset.

## 5. A defect this run had to fix before it could measure anything

The first boot of the fix image produced **no circuits at all**. Verbatim from
the console:

```
circuit pump threads failed to start error=no protocol option (os error 42)
```

on every dial, immediately after `outcome=connected`, with
`circuits=Some(0)` and all four `ca://` links `connected=false`.

Root cause: `drive_socket_blocking` in
`crates/epics-libcom-rs/src/runtime/blocking_io.rs` propagated the result of
`set_write_timeout` (`SO_SNDTIMEO`). VxWorks 7's socket stack does not
implement that option and returns `ENOPROTOOPT` (errno 42) on an otherwise-good
connected socket, so every CA client circuit aborted the instant its dial
succeeded. `SO_RCVTIMEO` on the same socket is accepted.

This is the same rule `crates/epics-pva-rs/src/server_native/blocking.rs`
already applied to the PVA server's accepted sockets; the fix moves it into the
seam both blocking drivers share, so `SO_RCVTIMEO` stays fatal and
`SO_SNDTIMEO` becomes best-effort in one place. The cost is stated rather than
hidden: on a target that refuses the option, `write_frame_deadline` loses the
timeout tick it regains control on, so a peer that never reads parks the writer
pump in `write` instead of tripping the deadline. That is a stall under
backpressure; the fatal version was no connection at all.

Committed as `d4085caa`, and cherry-picked onto the control branch as
`5dbcadd0` so that the A/B differential stays exactly the one commit under
test.

## 6. Verdict

**The pass criterion is met.** It was: the control image holds the fd across
the cut while the fix image retires the circuit. Measured:

* **control holds it.** `fd=6`, local port `56754`, censused
  `peer=none(errno=57)` at `c6-18`, `c6-24`, `c6-30`, `c6-36`, `c6-42`,
  `c6-48`, `c6-54` — seven consecutive censuses, the same descriptor and the
  same port every time — and released only after the link came back. It is out
  of the redial path for that entire span.
* **fix retires it.** Zero `peer=none(errno=57)` non-listener lines in the
  whole run. Three of eight outage censuses read `open=5`, the pre-connection
  baseline, holding nothing at all for the dead peer; the other five hold one
  socket whose port rotates on every attempt.
* **the offset is exactly one descriptor**, in every inter-dial gap, for the
  whole 480 s: control floor 6 against fix floor 5, control peak 7 against fix
  peak 6. That one descriptor is the wedge.
* recovery is unaffected: both images return to two established circuits and
  four `connected=true` links within 30 s of `set_link n1 on`.

The `CircuitDeathGuard` fix is therefore confirmed on VxWorks 7 as well as on
RTEMS, against a different TCP stack and a different cut.

**The new number.** The VxWorks 7 redial ladder is **75.0 s per rung
(74900–75000 ms measured, eight rungs)**, with a ~30 s client backoff between
rungs, so a 480 s outage costs four rungs. It coincides with the RTEMS
`TCPTV_KEEP_INIT` of 75.00 s — the constant transfers, which could not be known
before measuring it, and the RTEMS document's number should not be cited for
this target on the strength of that coincidence. Two things do *not* transfer:
the errno (BSD numbering, and not stable across attempts — §3.1) and the census
signature of a dead socket (`peer=none(errno=57)` with `mode` unchanged at
`0140666`, not RTEMS's `mode=0140000` — §3.3).

**One defect was found and fixed on the way** (§5): `SO_SNDTIMEO` is
unimplemented on VxWorks 7 and `drive_socket_blocking` propagated its
`ENOPROTOOPT` fatally, so before `d4085caa` a CA client on this target
established **no circuits at all**. Nothing in this row could have been
measured without that fix, and it is a defect in the shipped port, not in the
rig.

## 7. Reproduction

The box is not backed up. Every script the run used is reproduced here in
full, as it stood at the time of the measurement.

### 7.1 `~/vx-rig-e9/build-vx-ca.sh`

```bash
#!/bin/bash
# build-vx-ca.sh - build one realtime-ca-ioc VxWorks measurement image (RTP).
#
# The VxWorks sibling of ~/rtems-bringup/build-wedge.sh.  Three differences,
# all forced by the target rather than chosen:
#
#   * --target x86_64-wrs-vxworks, a BUILTIN rustc triple, so there is no
#     $SPECFLAG and no generated JSON spec: the has-thread-local deviation the
#     RTEMS script exists to carry has no counterpart here (doc/vxworks-port.md
#     §1).
#   * the libc patch is CONFIG-level, derived from the manifest pin by
#     scripts/libc-std-patch.sh, because -Zbuild-std resolves std's own libc
#     from rust-src's lock and a manifest [patch] never reaches it.  With
#     rust-src on 0.2.185 and the fork on 0.2.188 that helper prints the ALIAS
#     shape (patch.crates-io.libc-std), which is a different key and so does
#     not collide with the entry below.
#   * the box-global ~/.cargo/config.toml carries an RTEMS libc PATH patch that
#     would otherwise win over the manifest pin, so this tree carries its own
#     .cargo/config.toml.  It must be a PATH entry too: cargo merges config
#     [patch] tables key by key, so a git+rev entry lands on top of the
#     global's path and cargo refuses the pair -- measured, "dependency (libc)
#     specification is ambiguous. Only one of `git` or `path` is allowed".
#     $LIBC_PIN is a checkout of the manifest's own rev, so the CONTENT is the
#     manifest pin either way.
#
#   $1 = output .vxe name staged into $FTPROOT
#   $2 = C6_NAME_SERVERS value compiled into the image
#   $3 = git ref to build (default: leave the tree as it is)
set -e -o pipefail

TREE=$HOME/vx-rig-e9/tree
LIBC_PIN=$HOME/vx-rig-e9/libc-pin
FTPROOT=$HOME/vx-rig-e9/ftp/root
S=$HOME/wrsdk-vxworks7-qemu-1.17.0
TARGET=x86_64-wrs-vxworks

export WIND_SDK_HOME=$S WIND_HOME=$S WIND_BASE=$S/vxsdk
export WIND_CC_SYSROOT=$S/vxsdk/sysroot WIND_SDK_CC_SYSROOT=$S/vxsdk/sysroot
export WRSD_LICENSE_FILE=$S/license
export CONFIG_SITE=$S/vxsdk/sysroot/usr/mk/config.site
export WIND_SDK_CCBASE_PATH=$S/compilers/llvm-18.1.8.2/LINUX64/bin
export PATH=$HOME/.cargo/bin:$S/vxsdk/host/x86_64-linux/bin:$PATH
export LD_LIBRARY_PATH=$S/vxsdk/host/x86_64-linux/lib:${LD_LIBRARY_PATH:-}
export CARGO_HOME=$HOME/.cargo
export CARGO_TARGET_DIR=$HOME/vx-rig-e9/target
export RUSTUP_TOOLCHAIN=nightly
unset RUSTC_BOOTSTRAP RUSTFLAGS RUSTC

mkdir -p "$FTPROOT"
cd "$TREE"
[ -n "$3" ] && git checkout -q "$3"

LIBC_REV=$(sed -n 's/^libc *= *{.*rev *= *"\([^"]*\)".*/\1/p' Cargo.toml)
[ -n "$LIBC_REV" ] || { echo "no libc git pin in Cargo.toml"; exit 1; }
[ "$(git -C "$LIBC_PIN" rev-parse HEAD)" = "$LIBC_REV" ] || {
    echo "$LIBC_PIN is not at the manifest rev $LIBC_REV"; exit 1; }
mkdir -p .cargo
cat > .cargo/config.toml <<EOF
[target.$TARGET]
linker = "wr-c++"

[patch.crates-io]
libc = { path = "$LIBC_PIN" }
EOF

# The std-graph half of the same pin: a version-relabelled clone of that rev,
# applied as an alias entry so the workspace graph keeps the path entry above.
mapfile -t CFG < <(./scripts/libc-std-patch.sh nightly)
[ "${#CFG[@]}" -gt 0 ] || { echo "libc-std-patch.sh printed no patch lines"; exit 1; }
CFGARGS=()
for line in "${CFG[@]}"; do CFGARGS+=(--config "$line"); done

export C6_NAME_SERVERS="$2"
echo "=== building $1 from $(git rev-parse --short HEAD) ($(git log -1 --format=%s | cut -c1-60)) ==="
echo "=== C6_NAME_SERVERS=$C6_NAME_SERVERS  target=$TARGET  no SPECFLAG ==="
echo "=== patch lines: ${CFG[*]} ==="
cp Cargo.lock /tmp/e9-lock-snapshot.$$
cargo +nightly build --release --target "$TARGET" \
    -Zbuild-std=std,panic_abort "${CFGARGS[@]}" \
    --no-default-features --features client-core,bringup-probes \
    -j4 -p epics-ca-rs --bin realtime-ca-ioc 2>&1 | tail -25
cp /tmp/e9-lock-snapshot.$$ Cargo.lock; rm -f /tmp/e9-lock-snapshot.$$

# rustc's vxworks target appends the .vxe suffix itself.
cp "$CARGO_TARGET_DIR/$TARGET/release/realtime-ca-ioc.vxe" "$FTPROOT/$1"
echo "staged $FTPROOT/$1  $(stat -c %s "$FTPROOT/$1") bytes  from $(git rev-parse HEAD)"
strings "$FTPROOT/$1" | grep -F "$2" | head -2
```

### 7.2 `~/vx-rig-e9/rig-e9.sh` — kill safety on a shared box

`gv100` is shared. `~/rtems-bringup/rigpid.sh` cannot be used here: its
`rig_is_qemu` hardcodes `comm == "qemu-system-arm"`, so a VxWorks guest is
silently skipped by `rig_kill_own` and accumulates while the caller believes it
was cleaned up. Note also that `/proc/<pid>/comm` truncates at 15 characters,
so the string to match is `qemu-system-x86`, not `qemu-system-x86_64` — matching
the untruncated name makes the guard refuse to kill the rig's own guests.

```bash
#!/bin/bash
# rig-e9.sh - SOURCED.  Pid-scoped process bookkeeping for the E9 rig.
#
# WHY THIS EXISTS RATHER THAN ~/rtems-bringup/rigpid.sh: that file's
# `rig_is_qemu` hardcodes `comm == "qemu-system-arm"`, so a VxWorks guest
# (`qemu-system-x86_64`) is silently SKIPPED by rig_kill_own and accumulates
# forever while the caller believes it was cleaned up.  gv100 is shared: two
# long-running RTEMS arm guests belong to another arm and must survive, so a
# blanket pkill/killall against qemu is never run here.  Every kill below is
# by a pid THIS rig recorded, and re-checks /proc/<pid>/comm first.
E9=$HOME/vx-rig-e9
PIDDIR=$E9/pids
mkdir -p "$PIDDIR"

rig_track() {  # rig_track <slot> <pid>
    echo "$2" > "$PIDDIR/$1.pid"
}

rig_comm() { cat "/proc/$1/comm" 2>/dev/null; }

# Kill one recorded pid, but only if it is still the process we recorded.
# <want-comm> is matched against /proc/<pid>/comm; "" means accept any.
rig_kill_slot() {  # rig_kill_slot <slot> <want-comm>
    local slot="$1"
    local want="$2"
    local f="$PIDDIR/$slot.pid"
    local pid comm
    [ -f "$f" ] || return 0
    pid=$(cat "$f")
    [ -n "$pid" ] || { rm -f "$f"; return 0; }
    comm=$(rig_comm "$pid")
    if [ -z "$comm" ]; then
        echo "rig: slot=$slot pid=$pid already gone"; rm -f "$f"; return 0
    fi
    if [ -n "$want" ] && [ "$comm" != "$want" ]; then
        echo "rig: REFUSING to kill slot=$slot pid=$pid comm=$comm (recorded as $want) - pid reused"
        rm -f "$f"; return 0
    fi
    kill "$pid" 2>/dev/null
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        sleep 1; [ -z "$(rig_comm "$pid")" ] && break
    done
    if [ -n "$(rig_comm "$pid")" ]; then kill -9 "$pid" 2>/dev/null; sleep 1; fi
    echo "rig: killed slot=$slot pid=$pid comm=$comm"
    rm -f "$f"
}

# The guest's own slots, torn down on every (re)boot.  The qemu slot carries
# the x86_64 comm so a stale pidfile can never reach the arm guests.
rig_kill_guest() {
    rig_kill_slot qemu   qemu-system-x86   # /proc/<pid>/comm truncates at 15 chars
    rig_kill_slot conbr  nc
    rig_kill_slot holder sleep
    rig_kill_slot ftpd   python3
}

# Everything, including the host-side upstream CA server.  The upstream is
# deliberately NOT in rig_kill_guest: it is the peer the guest dials, it
# outlives a guest reboot, and folding it in once already killed it out from
# under a run that had just rebooted the guest.
rig_kill_own() {
    rig_kill_guest
    rig_kill_slot ioc    softioc-rs
}

rig_show() {
    echo "--- rig-e9 processes ---"
    for f in "$PIDDIR"/*.pid; do
        [ -e "$f" ] || continue
        local pid; pid=$(cat "$f")
        printf "%-8s pid=%-8s comm=%s\n" "$(basename "$f" .pid)" "$pid" "$(rig_comm "$pid")"
    done
    echo "--- other panels' qemu (MUST SURVIVE) ---"
    pgrep -a qemu-system-arm || echo "  (none)"
}
```

### 7.3 `~/vx-rig-e9/boot-e9.sh`

```bash
#!/bin/bash
# boot-e9.sh <image.vxe> <tag> - bring up ONE VxWorks guest for the E9 rig and
# leave it at the kernel shell with the RTP not yet started.
#
# The qemu form is doc/vxworks-port.md §6 verbatim, with three changes:
#   * E9's own host-port block (41534/45075 hostfwd, ftpd 2151, pasv
#     60030-60035, console /tmp/vxcon-e9a.sock) so three panels can boot at once;
#   * `-monitor unix:...` instead of `-monitor none`, because the cut is
#     `set_link` and the monitor is how it is reached;
#   * `id=n1` on the user netdev, so `set_link` has a name to take down.
# Everything else -- legacy `-net nic -net user`, `o=gei0`, serial OFF stdio on
# a chardev socket, and the EOF-propagating python FTP bridge -- is unchanged,
# because each of those cost a debugging round already.
set -u
E9=$HOME/vx-rig-e9
IMG="$1"; TAG="$2"
SDK=$HOME/wrsdk-vxworks7-qemu-1.17.0
KERNEL=$SDK/vxsdk/bsps/itl_generic_3_0_0_5/vxWorks
FTPROOT=$E9/ftp/root
CON=$E9/$TAG-con.in
LOG=$E9/$TAG-console.log
FTPLOG=$E9/$TAG-ftpd.log
CONSOCK=/tmp/vxcon-e9a.sock
MONSOCK=/tmp/vxmon-e9a.sock
. "$E9/rig-e9.sh"

[ -f "$FTPROOT/$IMG" ] || { echo "no image $FTPROOT/$IMG"; exit 1; }

rig_kill_guest
rm -f "$CON" "$CONSOCK" "$MONSOCK"
mkfifo "$CON"
: > "$LOG"

# ftpd: E9's own port and passive range, masquerading as the guest-visible
# 10.0.2.100 that the guestfwd entries below terminate.
setsid python3 - "$FTPROOT" <<'PY' > "$FTPLOG" 2>&1 &
import logging, sys
from pyftpdlib.authorizers import DummyAuthorizer
from pyftpdlib.handlers import FTPHandler
from pyftpdlib.servers import FTPServer
auth = DummyAuthorizer(); auth.add_user("target","vxTarget",sys.argv[1],perm="elr")
h = FTPHandler; h.authorizer = auth
h.masquerade_address = "10.0.2.100"
h.passive_ports = range(60030, 60036)
logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
FTPServer(("127.0.0.1", 2151), h).serve_forever()
PY
rig_track ftpd $!
sleep 2

# The FIFO holder: a one-shot writer closing con.in would EOF qemu's console.
setsid sh -c "exec sleep 86400 > $CON" < /dev/null > /dev/null 2>&1 &
rig_track holder $!

GF="guestfwd=tcp:10.0.2.100:21-cmd:python3 /tmp/pybridge.py 2151"
for p in 60030 60031 60032 60033 60034 60035; do
    GF="$GF,guestfwd=tcp:10.0.2.100:$p-cmd:python3 /tmp/pybridge.py $p"
done

setsid qemu-system-x86_64 -m 1024M -kernel "$KERNEL" \
    -net nic -net "user,id=n1,hostfwd=tcp:127.0.0.1:41534-:1534,hostfwd=tcp:127.0.0.1:45075-:5075,$GF" \
    -display none -monitor "unix:$MONSOCK,server,nowait" \
    -chardev "socket,id=vcon,path=$CONSOCK,server=on,wait=off" -serial chardev:vcon \
    -append "bootline:fs(0,0)host:vxWorks h=10.0.2.100 e=10.0.2.15 u=target pw=vxTarget o=gei0" \
    > "$E9/$TAG-qemu.log" 2>&1 < /dev/null &
rig_track qemu $!
sleep 3

# Console bridge: stdin from the FIFO, stdout to the log.
setsid nc -U "$CONSOCK" < "$CON" > "$LOG" 2>&1 &
rig_track conbr $!

echo "booting $IMG tag=$TAG ..."
for i in $(seq 60); do
    sleep 1
    grep -q '^->' "$LOG" 2>/dev/null && { echo "kernel shell up after ${i}s"; break; }
done
echo "=== console so far ($(wc -c < "$LOG") bytes) ==="
tail -c 1200 "$LOG"
rig_show
```

### 7.4 `~/vx-rig-e9/upstream-e9.sh` and `upstream-e9.db`

```bash
#!/bin/bash
# upstream-e9.sh - the host-side CA server the guest's ca:// links resolve
# through, on E9's own host port 45064.  SLIRP puts the host at 10.0.2.2, so
# the guest reaches it at 10.0.2.2:45064 with no hostfwd involved -- this is an
# OUTBOUND flow, and the cut is `set_link n1 off` on the monitor, not anything
# done to this process.
set -u
E9=$HOME/vx-rig-e9
. "$E9/rig-e9.sh"
rig_kill_slot ioc softioc-rs
cd "$E9"
setsid ./target/release/softioc-rs --db upstream-e9.db --port 45064 \
    > "$E9/upstream.log" 2>&1 < /dev/null &
rig_track ioc $!
sleep 4
echo "upstream pid=$(cat $E9/pids/ioc.pid) comm=$(rig_comm "$(cat $E9/pids/ioc.pid)")"
tail -3 "$E9/upstream.log"
```

```
# The four PVs the C6 probe database links to (realtime-ca-ioc.rs C6_PROBE_DB).
# Values move so a CP link's monitor has something to deliver and the console's
# `connected=true` is a live reading rather than a one-shot initial update.
record(calc, "UPSTREAM:AI")    { field(CALC, "VAL+1") field(SCAN, "1 second")  field(PREC, "3") }
record(ao,   "UPSTREAM:AO")    { field(VAL,  "0")     field(PREC, "3") }
record(calc, "UPSTREAM:FAST")  { field(CALC, "VAL+1") field(SCAN, ".1 second") field(PREC, "3") }
record(calc, "UPSTREAM:OTHER") { field(CALC, "VAL+1") field(SCAN, "1 second")  field(PREC, "3") }
```

### 7.5 `~/vx-rig-e9/mon-e9.py`

```python
# mon-e9.py <unix-monitor-socket> <qemu monitor command>
# One command, print the reply.  Used for `info network` and `set_link`.
import socket, sys, time
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sys.argv[1])
s.settimeout(1.0)
banner = b""
t0 = time.time()
while time.time() - t0 < 1.5:
    try:
        d = s.recv(65536)
        if not d: break
        banner += d
    except socket.timeout:
        break
s.sendall((" ".join(sys.argv[2:]) + "\n").encode())
out = b""
t0 = time.time()
while time.time() - t0 < 2.0:
    try:
        d = s.recv(65536)
        if not d: break
        out += d
    except socket.timeout:
        break
sys.stdout.write(out.decode(errors="replace"))
```

### 7.6 `~/vx-rig-e9/run-wedge-e9.sh` and `ab-e9.sh`

```bash
#!/bin/bash
# run-wedge-e9.sh <tag> <outage-seconds> <restore-seconds>
#
# The phase walk of the E9 A/B, on a guest that is ALREADY running the image
# under test with its circuits established.  Topology substitution vs the RTEMS
# topology B (doc/rtems-circuit-wedge-on-target-measurement.md §2): the peer is
# a host CA server reached through SLIRP at 10.0.2.2:45064 rather than a second
# guest on a hub, and the POWER CUT is `set_link n1 off` on the SLIRP netdev
# rather than `set_link h2 off` on a hub port.  The cut is at the LINK, so no
# frame of any kind can reach the guest while it is down -- which is what makes
# a forged RST structurally impossible here rather than merely unobserved.
set -u
E9=$HOME/vx-rig-e9
TAG="$1"; OUTAGE="$2"; RESTORE="$3"
LOG=$E9/$TAG-console.log
PH=$E9/$TAG-phases.txt
MON=/tmp/vxmon-e9a.sock

mark() {  # mark <phase>
    local seq
    seq=$(grep -ao 'FDPROBE seq=[0-9]*' "$LOG" | tail -1)
    printf '%s  %-28s  %s\n' "$(date -u +%H:%M:%S)" "$1" "${seq:-<none>}" | tee -a "$PH"
}

: > "$PH"
mark "pre-cut"
sleep 20
mark "pre-cut+20"

python3 "$E9/mon-e9.py" "$MON" "set_link n1 off" > "$E9/$TAG-cut.txt" 2>&1
mark "POWER CUT (set_link n1 off)"

n=0
while [ "$n" -lt "$OUTAGE" ]; do
    sleep 60; n=$((n+60))
    mark "outage +${n}s"
done

python3 "$E9/mon-e9.py" "$MON" "set_link n1 on" > "$E9/$TAG-restore.txt" 2>&1
mark "RESTORE (set_link n1 on)"

n=0
while [ "$n" -lt "$RESTORE" ]; do
    sleep 30; n=$((n+30))
    mark "restore +${n}s"
done
echo "--- phases ---"; cat "$PH"
```

```bash
#!/bin/bash
# ab-e9.sh <image.vxe> <tag> <outage-s> <restore-s>
# One whole leg of the E9 A/B: boot the guest on the image under test, start
# the RTP, wait for the four ca:// links to report connected, then walk the
# phases.  Both legs run this same script so the only difference between them
# is the image.
set -u
E9=$HOME/vx-rig-e9
IMG="$1"; TAG="$2"; OUTAGE="$3"; RESTORE="$4"
LOG=$E9/$TAG-console.log

"$E9/boot-e9.sh" "$IMG" "$TAG" > "$E9/$TAG-boot.log" 2>&1
"$E9/upstream-e9.sh" > "$E9/$TAG-upstream.log" 2>&1

echo "rtpSp \"/host.host/$IMG\"" > "$E9/$TAG-con.in"
for i in $(seq 180); do
    sleep 1
    grep -aq 'link pv=UPSTREAM:OTHER connected=true' "$LOG" && { echo "links up after ${i}s"; break; }
done
grep -aq 'link pv=UPSTREAM:OTHER connected=true' "$LOG" || {
    echo "LINKS NEVER CONNECTED - aborting this leg"; tail -c 1500 "$LOG"; exit 1; }

# Let the pre-cut state settle onto at least one full FDCENSUS (every 6th probe,
# ~60 s) so the outage censuses have a baseline of the same kind to be read
# against.
sleep 70
"$E9/run-wedge-e9.sh" "$TAG" "$OUTAGE" "$RESTORE"
```

### 7.7 Running it

```
cd ~/vx-rig-e9
./build-vx-ca.sh wedge-fix.vxe 10.0.2.2:45064 caucus/58EWEJWV91/e9-wedge-0f4213ca-1
./build-vx-ca.sh wedge-ctl.vxe 10.0.2.2:45064 e9/control-no-wedge-fix
./ab-e9.sh wedge-fix.vxe lad 480 150
./ab-e9.sh wedge-ctl.vxe ctl 480 150
```

Artefacts on the box under `~/vx-rig-e9/`: `lad-console.log`,
`lad-phases.txt`, `lad-cut.txt`, `lad-restore.txt`, and the `ctl-*`
counterparts.
