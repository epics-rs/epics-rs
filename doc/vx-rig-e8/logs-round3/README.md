# E8 round-3 captures

Kept because gv100 is not backed up and every number in §§15–18 of
`doc/vxworks-ca-worker-pool-on-target-measurement.md` is quoted from these
files.

| file | what it is |
| --- | --- |
| `stackload5.log` | driver side of the widest stack arm: 12 reply shapes × 48, 4 connections, 380 s hold, `HOLD ok=192 fail=0` |
| `chainprobe.log` | the FLNK depth boundary, every link read: `L15 = 1215.0`, `L16..L18 = 0.0` |
| `phaseramp-wall-mtx2.log` | cold ramp to the wall: 43 ramp + 5 monitor = 48, `EAGAIN` refusal verbatim |
| `console-stackload5-1024M-ctrl.log` | guest console, distilled — the `STACKUSE` census showing 65,912 / 7,120 B |
| `console-wallmtx2-1024M-cold.log` | guest console, distilled — the `MTXPROBE semMCreate=NULL` / `lock rc=22` pair and the `CAS-client 48` panic |
| `console-stackload2-1024M-abort.log` | guest console, distilled — the monitored-1 MiB-array abort, `memory allocation of 1048576 bytes failed` → signal 6 |

The three console files are filtered to the `MTXPROBE` / `STACKUSE` /
`POOLPROBE` / allocation / panic / depth-limit / `MEM_USED` lines. The full
transcripts are a 115200-baud console dump dominated by the 10 s reporter
and are not worth versioning; the filter keeps every line the doc quotes.
