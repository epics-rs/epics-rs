#!/bin/bash
# Stop ONLY the qemu guests this rig started.
#
# Was: `pkill -f "kernel /home/coding-agent/rtems-bringup/pi/"`.  Two ways that
# is wrong on a SHARED box: another panel booting an image out of this path
# would be killed too, and `pkill -f` matches command lines -- including the
# ssh/bash command line that is running the killer itself.  boot.sh now records
# each pid it starts in pi/pi.qemu.pid; rig_kill_own TERMs (then KILLs) exactly
# those, and only after re-reading /proc/PID/comm to confirm the pid still
# names a qemu-system-arm (pids are recycled).
#
# Scope note: this reaches GUESTS only.  The host-side client killers in the
# load rigs (caload.sh / caload2.sh / caload3.sh / worst.sh / isolate2.sh
# `pkill -f camonitor`, pvastorm.sh `pkill -f pvxmonitor`) cannot touch a qemu
# guest at all -- camonitor/pvxmonitor are host processes and no guest process
# is visible to the host process table.  They are a different blast radius,
# not a safe one: `pkill -f camonitor` still matches ANOTHER panel's camonitor
# on this shared box.  They are out of this conversion's scope because they
# kill no guest; if a panel needs concurrent CA client load, it must record its
# own client pids the same way rather than rely on those scripts.
. "$HOME/rtems-bringup/rigpid.sh"
rig_pidfile "$HOME/rtems-bringup/pi/pi.qemu.pid"
rig_kill_own
