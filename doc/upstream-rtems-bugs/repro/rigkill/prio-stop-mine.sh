#!/bin/bash
# Stop ONLY the qemu guests this rig started.
#
# Was: `pkill -f "prio/caioc-instr.exe"`.  Keeping the pattern in a file kept
# it off the ssh command line, but it still matched by command line: any other
# panel booting the same image path would be killed, and the pattern is a
# substring match, not an identity.  boot-before.sh / boot-instr.sh now record
# the pid they start in prio/prio.qemu.pid; rig_kill_own TERMs (then KILLs)
# exactly those, after re-reading /proc/PID/comm to confirm the pid still names
# a qemu-system-arm (pids are recycled).
#
# Scope note: this reaches GUESTS only.  The host-side client killers in the
# load rigs (caload*.sh / worst.sh / isolate2.sh `pkill -f camonitor`,
# pvastorm.sh `pkill -f pvxmonitor`) cannot touch a qemu guest at all --
# camonitor/pvxmonitor are host processes, and no guest process appears in the
# host process table.  Different blast radius, not a safe one: those patterns
# still match another panel's host-side client.  Out of this conversion's
# scope because they kill no guest.
. "$HOME/rtems-bringup/rigpid.sh"
rig_pidfile "$HOME/rtems-bringup/prio/prio.qemu.pid"
rig_kill_own
