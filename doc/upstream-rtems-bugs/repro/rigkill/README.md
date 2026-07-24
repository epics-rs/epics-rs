# Rig kill-safety: the last two `pkill -f` guest killers, converted

The measurement box is **shared** — several panels run their own qemu guests at
the same time. `rigpid.sh` (this directory's parent, `repro/rigpid.sh`) is the
own-pid discipline every rig is supposed to use: record each pid you start,
kill only those, and re-read `/proc/PID/comm` first because pids are recycled.

Two rigs were still killing by command-line pattern. Both live in private rig
directories, so the practical blast radius was small — but the pattern is wrong
for two reasons that do not depend on who owns the directory:

* another panel booting an image out of the same path matches the pattern, and
* `pkill -f` matches *command lines*, including the ssh/bash command line that
  is running the killer itself.

| file | was | now |
|---|---|---|
| `~/rtems-bringup/pi/stop.sh` | `pkill -f "kernel /home/coding-agent/rtems-bringup/pi/"` | `rig_kill_own` over `pi/pi.qemu.pid` |
| `~/rtems-bringup/prio/stop-mine.sh` | `pkill -f "prio/caioc-instr.exe"` | `rig_kill_own` over `prio/prio.qemu.pid` |

The pid recording was added to the boot scripts that start those guests:
`pi/boot.sh`, `prio/boot-before.sh`, `prio/boot-instr.sh` (all three now source
`rigpid.sh`, call `rig_pidfile`, and `rig_track` the pid they start; each also
echoes it so a console reader can pair the boot with the pidfile).

The copies here are the exact files installed on the box (`pi-*.sh` /
`prio-*.sh` are `pi/`- and `prio/`-relative).

## Live test — 2026-07-24 on the box

`killtest-prio.sh` is the prio half verbatim; the pi half was the same
sequence. Both were run with the box otherwise idle. Recorded results:

**Decoys.** Two `qemu-system-arm` processes this rig did **not** start, held
alive with `-S` (start paused) and carrying the exact command lines the retired
patterns matched. `pgrep` confirms the retired patterns would have killed them:

```
--- retired pattern 'prio/caioc-instr.exe' would have matched pids: 3202041
--- retired pattern 'kernel /home/coding-agent/rtems-bringup/pi/' would have matched pids: 3202042
    (this shell: 3202039 ; the ssh/bash running the killer is itself a -f match risk)
```

**prio.** Booted `caioc-instr.exe` (reached `cgem0: sending ARP announce`), pid
3202070 recorded in `prio/prio.qemu.pid`:

```
=== prio/stop-mine.sh ===
rig: TERM own qemu pid 3202070
--- mine gone? GONE
--- decoys survived? prio=qemu-system-arm pi=qemu-system-arm
--- pidfile removed? ls: cannot access 'prio/prio.qemu.pid': No such file or directory
=== second stop is a no-op === exit=0
```

**pi.** Booted `caioc-probe-rton.exe`, pid 3201229 recorded in
`pi/pi.qemu.pid`; `pi/stop.sh` printed `rig: TERM own qemu pid 3201229`, both
decoys (3201198, 3201199) survived, pidfile removed, second stop a no-op.

A first attempt at the pi negative control was **discarded**: the decoy image
(`pi/caioc-base.exe`) panics at boot (`EXIT STATUS NOT ZERO`, exit code 101) and
died on its own before the kill ran, so it proved nothing. That is why the
decoys above are held paused with `-S`.

## Scope note — the host-side client killers are a different thing

`caload.sh`, `caload2.sh`, `caload3.sh`, `worst.sh`, `isolate2.sh` still run
`pkill -f camonitor`, and `pvastorm.sh` runs `pkill -f pvxmonitor`. Those
**cannot touch a guest**: `camonitor`/`pvxmonitor` are host processes and no
process inside a qemu guest appears in the host process table, so no pattern
aimed at them can reach a guest.

That is a statement about blast *radius*, not about safety. `pkill -f
camonitor` still matches **another panel's** camonitor on this shared box. They
are out of this conversion's scope only because they kill no guest; a panel that
needs concurrent CA/PVA client load should record its own client pids the same
way rather than rely on those scripts.

After this conversion, `grep -rn pkill ~/rtems-bringup/*.sh
~/rtems-bringup/pi/*.sh ~/rtems-bringup/prio/*.sh ~/rtems-bringup/topoB/*.sh`
finds **no live guest-killing `pkill`** — every remaining hit is either a
comment explaining why not to, or one of the host-side client killers above.
