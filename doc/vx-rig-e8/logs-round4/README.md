# E8 round-4 on-target captures

`x86_64-wrs-vxworks`, QEMU guest `-m 1024M`, cold boot, image built by
`build-e8.sh plain` from the tree carrying `248a038c` (`Arc<Snapshot>`) and
`0aee28fc` (`FrameBuf` / `encode_dbr_into`). Evidence for §17.1–§17.2 of
`doc/vxworks-ca-worker-pool-on-target-measurement.md`.

* `console-arcframe-all3-survive.log` — the guest console across
  `stackload.py 4 130 all3 10 1`, the workload that aborted the RTP with
  `memory allocation of 1048576 bytes failed` / `signal 6` on the previous
  image (see `../logs-round3/console-stackload2-1024M-abort.log`). Carries the
  `MEM_USED` trajectory (43,278,336 → 215,539,712 peak → 214,425,600 across
  the 130 s hold → 204,988,416 after the clients left) and the new
  `MONPROBE seq=N COLLAPSED=` line beside it.

* `console-arcframe-all3-postmortem.log` — the same log continued through
  `rtpShow` (`STATE_NORMAL`, 27 tasks, RTP `0xffff800008c42000`) and
  `edrShow` (one record, the cold-boot `INFO/BOOT`). The post-mortem is here
  because a starved low-priority census reporter can make a live IOC look
  dead; `STATE_NORMAL` plus an empty exception log is what rules that out.

The client-side transcript for the run is quoted verbatim in §17.2.
