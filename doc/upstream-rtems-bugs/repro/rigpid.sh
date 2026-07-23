#!/bin/bash
# Shared rig-pid helpers.  Source with:  . "$HOME/rtems-bringup/rigpid.sh"
#
# WHY THIS EXISTS: the box is SHARED.  Several panels run their own qemu
# guests at the same time.  `pkill -x qemu-system-arm` kills EVERY panel's
# guest, not just this rig's, and `pkill -f <pattern>` additionally matches
# the ssh command line that is running the script and so can kill the shell
# itself.  Every rig therefore records the pids it started and kills only
# those, after re-checking that the pid still names a qemu-system-arm
# process (pids are reused).
#
#   rig_pidfile <path>   declare where this rig records its pids
#   rig_track <pid>...   record pids this rig started
#   rig_kill_own         TERM (then KILL) only the recorded pids

RIG_PIDFILE=""

rig_pidfile() {
  RIG_PIDFILE=$1
}

rig_track() {
  for _p in "$@"; do
    [ -n "$_p" ] && echo "$_p" >> "$RIG_PIDFILE"
  done
}

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
