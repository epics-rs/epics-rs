# Measurement rigs on the shared box — removing every blanket `pkill` of qemu

**Eight rig scripts on `coding-agent@192.168.2.128` killed *every* panel's qemu
guest, not their own. All eight are converted; a live two-guest test shows the
new path kills the tracked guest and leaves the untracked one running.**

The box is shared: several panels boot `qemu-system-arm` guests at the same
time. A `tls-spec` panel was running guests out of `~/rtems-bringup/tlsdtor/`
while this change was made. Six scripts opened *and* closed with
`pkill -x qemu-system-arm`, which matches by executable name and therefore
terminates every guest on the box; two more used `pkill -f "kernel <image>.exe"`,
which additionally matches any command line containing that text — including the
`ssh` command line of the shell running the script.

None of these scripts are tracked in this repository (`git ls-files` finds no
`run-*.sh` / `boot-*.sh`), so they were fixed in place on the box. This document
is the record: the diffs as applied, and the console proof that the new path is
selective.

Taken **2026-07-24** on `coding-agent@192.168.2.128`.

---

## 1. The eight sites

Anchor: `grep -rn --include="*.sh" -E "pkill|killall" ~/rtems-bringup/ ~/rtems-cside/`
(excluding the vendored `libbsd/` FreeBSD source tree, which contains FreeBSD's
own `bin/pkill` sources and is not a rig).

| script | site | what it killed |
|---|---|---|
| `run-diff.sh` | `:12`, `:27` | **every** guest on the box |
| `run-blackhole.sh` | `:17`, `:31` | **every** guest on the box |
| `run-diff-pva.sh` | `:8`, `:23` | **every** guest on the box |
| `run-dialpool.sh` | `:16`, `:35` | **every** guest on the box |
| `topoB/run-fd.sh` | `:11`, `:94` | **every** guest on the box |
| `topoB/run-fd2.sh` | `:15` | **every** guest on the box (its *tail* was already pid-scoped, and carried a comment saying so — the startup kill was still blanket) |
| `boot-ca.sh` | `:2`, `:3` | any panel's guest booted from `caioc.exe` / `pvaioc.exe`, by `pkill -f` |
| `boot-pva.sh` | `:2`, `:3` | any panel's guest booted from `pvaioc.exe` / `caioc.exe` / `pvaprobe.exe`, by `pkill -f` |

Not changed, and why:

* `pi/stop.sh` — `pkill -f "kernel /home/coding-agent/rtems-bringup/pi/"`, and
  `prio/stop-mine.sh` — `pkill -f "prio/caioc-instr.exe"`. Both patterns are
  scoped to a private per-rig directory, so they cannot reach another panel's
  guest unless that panel boots an image out of that same directory. They remain
  `pkill -f`, which is against the box rule, but they are not the shared-box
  hazard this change is about; converting them needs their boot scripts to
  record pids and neither boot script was in scope here. **UNFIXED.**
* `caload.sh:93`, `caload2.sh:67`, `caload3.sh:50`, `worst.sh:10,25`,
  `isolate2.sh:35,43` — `pkill -f camonitor`; `pvastorm.sh:22` —
  `pkill -f pvxmonitor`. These kill *host-side CA/PVA client* processes, not
  guests. They are the same defect shape (a blanket kill on a shared box) but a
  different blast radius, and no guest is at risk. **UNFIXED.**
* `~/rtems-cside/boot-cioc.sh` already killed only `$D/qemu.pid`. No change.

## 2. The replacement

New shared helper `~/rtems-bringup/rigpid.sh`. Each rig declares a pidfile,
records every pid it starts, and kills only those — after re-checking through
`/proc/<pid>/comm` that the pid still names a `qemu-system-arm` process, because
pids are reused:

```bash
rig_is_qemu() {
  [ -r "/proc/$1/comm" ] || return 1
  [ "$(cat "/proc/$1/comm" 2>/dev/null)" = "qemu-system-arm" ]
}

rig_kill_own() {
  [ -n "$RIG_PIDFILE" ] || { echo "rig: rig_pidfile not set" >&2; return 1; }
  [ -f "$RIG_PIDFILE" ] || return 0
  while read -r _p; do
    [ -n "$_p" ] || continue
    if rig_is_qemu "$_p"; then
      kill "$_p" 2>/dev/null && echo "rig: TERM own qemu pid $_p"
    fi
  done < "$RIG_PIDFILE"
  sleep 2
  while read -r _p; do
    [ -n "$_p" ] || continue
    if rig_is_qemu "$_p"; then
      kill -9 "$_p" 2>/dev/null && echo "rig: KILL own qemu pid $_p"
    fi
  done < "$RIG_PIDFILE"
  rm -f "$RIG_PIDFILE"
}
```

The startup call is what makes this a drop-in replacement rather than a
weakening: a leftover guest from a *previous run of the same rig* is in that
rig's own pidfile, so it is still cleaned up. What is no longer cleaned up is
somebody else's guest — which was never this script's to clean.

## 3. The diffs as applied

Backups are on the box as `<script>.bak-blanketpkill`. Representative diff
(`run-diff.sh`); the other five `pkill -x` conversions are the same three edits:

```diff
 rm -f dp2-pooled.log dp2-perattempt.log DONE-DIFF
-pkill -x qemu-system-arm 2>/dev/null
-sleep 2
+# The box is SHARED: kill only the guests THIS rig started (recorded in
+# dp2.qemu.pid).  A blanket `pkill -x qemu-system-arm` would take down
+# every other panel's guest too.
+. "$HOME/rtems-bringup/rigpid.sh"
+rig_pidfile "$PWD/dp2.qemu.pid"
+rig_kill_own
 qemu-system-arm -M xilinx-zynq-a9 -m 256M -no-reboot -nographic \
   -serial null -serial mon:stdio -monitor telnet:127.0.0.1:4453,server,nowait \
   -nic user,model=cadence_gem,mac=52:54:00:12:34:62 \
   -kernel dialpool-ca.exe </dev/null > dp2-pooled.log 2>&1 &
 A=$!
+rig_track $A
@@
 B=$!
+rig_track $B
 sleep "$RUN_SECS"
-kill $A $B 2>/dev/null
-sleep 2
-pkill -x qemu-system-arm 2>/dev/null
+rig_kill_own
 touch DONE-DIFF
```

`topoB/run-fd.sh` and `topoB/run-fd2.sh` additionally had their mid-script
DHCP-failure exit converted:

```diff
-  echo "VERDICT: DHCP-FAIL" >> fd-phases.txt; kill $G1 $G2 2>/dev/null; touch DONE-FD; exit 1
+  echo "VERDICT: DHCP-FAIL" >> fd-phases.txt; rig_kill_own; touch DONE-FD; exit 1
```

`boot-ca.sh` (and identically `boot-pva.sh`), whose kill was `pkill -f`:

```diff
 #!/bin/bash
-pkill -f "kernel caioc.exe" 2>/dev/null
-pkill -f "kernel pvaioc.exe" 2>/dev/null
-sleep 2
+# The box is SHARED. `pkill -f "kernel caioc.exe"` kills any OTHER panel
+# booted from the same image, and the -f pattern can match a shell command
+# line too. Kill only the guest THIS script recorded.
+. "$HOME/rtems-bringup/rigpid.sh"
+rig_pidfile "$HOME/rtems-bringup/bootca.qemu.pid"
+rig_kill_own
 cd $HOME/rtems-bringup
 rm -f ca.log
 setsid qemu-system-arm ... -kernel caioc.exe > ca.log 2>&1 &
+rig_track $!
 disown
 exit 0
```

## 4. Verification

**Syntax.** `bash -n` on the helper and all eight scripts:

```
OK rigpid.sh
OK run-diff.sh
OK run-blackhole.sh
OK run-diff-pva.sh
OK run-dialpool.sh
OK topoB/run-fd.sh
OK topoB/run-fd2.sh
SYNTAX OK   (boot-ca.sh)
SYNTAX OK   (boot-pva.sh)
```

**No blanket kill survives.** Re-running the anchor for `pkill -x
qemu-system-arm` over `~/rtems-bringup/` and `~/rtems-cside/` (excluding the
`.bak-blanketpkill` backups and `libbsd/`) returns **no matches**. The
patcher also reported `leftover_blanket=0` for each of the six files.

**Live selectivity test.** Two real guests booted from `dialpool-ca.exe`, only
the first recorded in the rig's pidfile; `rig_kill_own` then run. Console:

```
MINE=3025799 OTHER=3025800
-- before: alive? --
pid 3025799 comm=qemu-system-arm
pid 3025800 comm=qemu-system-arm
-- pidfile contents --
3025799
-- rig_kill_own --
rig: TERM own qemu pid 3025799
-- after --
pid 3025799 comm=GONE
pid 3025800 comm=qemu-system-arm
-- cleanup: kill the OTHER guest by its own pid --
pid 3025800 comm=GONE
```

The untracked guest survives `rig_kill_own` and is then killed explicitly by its
own pid. Sample size: one test, two guests, one rig.

## 5. Limits

* **The eight scripts were not re-run end to end.** What is proven is the kill
  path (§4, live) and that each script still parses (`bash -n`). No rig was
  driven through a full measurement run after the edit, so a mistake in the
  `rig_track` placement inside a rig's own control flow would not have been
  caught by this verification.
* **`rig_is_qemu` closes the pid-reuse window but does not eliminate it.** A
  recorded pid that has been reused *by another qemu-system-arm* would still be
  killed. Nothing on this box makes that likely within a rig's lifetime, and it
  was not tested.
* **`pkill -f` remains in eight further scripts** (§1, "not changed"): two guest
  killers scoped to a private rig directory, six host-side client killers.
* The `.bak-blanketpkill` backups are on the box only; this repository holds no
  copy of the pre-change scripts.
