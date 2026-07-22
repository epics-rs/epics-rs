# Where the non-stack per-connection heap actually goes (C IOC, RTEMS 6)

Target: EPICS base 7.0.10 C IOC, BSP `xilinx_zynq_a9_qemu`,
`CONFIGURE_MAXIMUM_FILE_DESCRIPTORS 150` (this panel's deviation; stock is 64),
256 MB guest.  Driver: `~/rtems-cside/ceiling3.py`, one CA TCP connection with
one `CA_PROTO_CREATE_CHAN` each, marks at 0 / 53 / 100 / ceiling.

## Correction to my earlier report

I previously reported the non-stack residual as "7,945 B per connection at 53
connections but 42,269 B at 139", and named libbsd mbuf/zone growth as the
candidate.  **That comparison was invalid**: those two figures came from two
different builds and two different boots (stock fd=64 vs my fd=150 build), not
from one run at two connection counts.

Measured properly, in a single boot with four marks, and reproduced across two
independent boots (identical to 0.1 MiB at every mark):

| connections | heap free | incremental per connection |
|------------:|----------:|---------------------------:|
| 0           | 219.7 MiB | -                          |
| 53          | 138.0 MiB | 1,616,383 B                |
| 100         |  65.6 MiB | 1,615,251 B                |
| 139         |   5.6 MiB | 1,613,183 B                |

The incremental cost is **flat** -- very slightly decreasing -- not growing.
There is no nonlinearity to explain.  The residual above the two thread stacks
(1,048,576 + 524,288 = 1,572,864 B) is ~42.4 KB at *every* connection count,
including 53.

## The named candidate is disproved

`netstat -m` on target is byte-identical at 100 and at 139 connections:

```
297/213/510 mbufs in use (current/cache/total)
263/113/376/1024 mbuf clusters in use (current/cache/total/max)
0/0/0 requests for mbufs denied (mbufs/clusters/mbuf+clusters)
0/0/0 requests for mbufs delayed (mbufs/clusters/mbuf+clusters)
```

No mbuf growth, no denials, no delays.  Diffing every UMA zone (`vmstat -z`)
between 53 and 139 connections (86 additional connections):

| zone       | item size | used@0 | used@53 | used@100 | used@139 | bytes/conn |
|------------|----------:|-------:|--------:|---------:|---------:|-----------:|
| tcpcb      |       712 |      1 |      54 |      101 |      141 |      720.3 |
| socket     |       488 |      8 |      61 |      108 |      148 |      493.7 |
| tcp_inpcb  |       328 |      1 |      54 |      101 |      141 |      331.8 |
| Files      |        64 |      9 |      62 |      109 |      148 |       64.0 |
| SLEEPQUEUE |        44 |     18 |      72 |      119 |      158 |       44.0 |

Every other zone is unchanged.  **All libbsd zones together are 1,654 B per
connection** -- 3.9 % of the residual, 0.1 % of the total.

## What the residual actually is

Measured on target (`casizes`, an iocsh command in my app that prints
`sizeof` from rsrv's own `server.h`):

```
CASIZES sizeof(struct client)=184 MAX_TCP=16384 sizeof(channel_in_use)=56
        sizeof(event_ext)=48 per-client-min=32952
```

rsrv gives every client both a send and a receive buffer of `MAX_TCP`
(`caProto.h:67`, `1024*16`), at `caservertask.c:1284` and `:1287`.

Budget for one connection, against the measured 1,615,251 B:

| item                                    | bytes     | share |
|-----------------------------------------|----------:|------:|
| CAS-client thread stack                 | 1,048,576 | 64.9 %|
| CAS-event thread stack                  |   524,288 | 32.5 %|
| rsrv send buffer (MAX_TCP)              |    16,384 |  1.0 %|
| rsrv recv buffer (MAX_TCP)              |    16,384 |  1.0 %|
| libbsd zones (tcpcb/socket/inpcb/...)   |     1,636 |  0.1 %|
| `struct client`                         |       184 |       |
| `struct channel_in_use` (1 channel)     |        56 |       |
| accounted                               | 1,607,508 | 99.5 %|
| unattributed (allocator headers, slab   |     7,743 |  0.5 %|
| rounding, free-list block reservation)  |           |       |

So of the ~42.4 KB above the stacks, **32,768 B (77 %) is rsrv's own pair of
16 KB CA buffers**, 1,636 B is libbsd, and the rest is allocator overhead.

`casr 4` confirms the free lists reserve in blocks and retain on close:

| mark        | free-list bytes held | clients | small buffers |
|-------------|---------------------:|--------:|--------------:|
| 0           |                1,288 |       7 |             0 |
| 139         |              185,464 |       4 |            10 |
| after close |            4,773,576 |     143 |           288 |

which is the mechanism behind the earlier observation that a second ceiling
round on the same boot reuses memory rather than leaking: at 139 connections
the heap is 5.6 MiB free, and after closing all of them it is 5.7 MiB free --
the 227 MiB is not returned to the heap, it is parked on rsrv's free lists.

## Operational consequence

At fd=150 the C IOC is at 5.6 MiB free when it hits its 139-connection fd
ceiling, so it is roughly 3 connections from being memory-bound as well.  That
part of the earlier statement stands.  But the lever is not zone tuning and not
the fd cap: **97.4 % of the per-connection cost is the two thread stacks.**
Raising `CONFIGURE_MAXIMUM_FILE_DESCRIPTORS` without reducing per-client stack
reservation buys about three more connections and then the guest is out of
memory.  The only large lever is the 1 MiB + 512 KiB stack pair -- which,
per the earlier `rt stackuse` measurement, is using 2,024 B and 380 B
respectively on this target.
