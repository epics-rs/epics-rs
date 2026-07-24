#!/bin/bash
# Live test of the converted prio/ kill path, run with NO other guest holding
# the 5064 hostfwd, plus proof that the retired pkill patterns matched a decoy.
cd $HOME/rtems-bringup
rm -f prio/prio.qemu.pid

echo "=== decoy: a qemu this rig did NOT start, paused (-S), carrying the exact"
echo "===        command line the OLD prio pattern matched"
setsid qemu-system-arm -M xilinx-zynq-a9 -m 256M -S -nographic -serial none -monitor none \
  -kernel /home/coding-agent/rtems-bringup/prio/caioc-instr.exe >/tmp/decoy-prio.log 2>&1 </dev/null &
DPR=$!
setsid qemu-system-arm -M xilinx-zynq-a9 -m 256M -S -nographic -serial none -monitor none \
  -kernel /home/coding-agent/rtems-bringup/pi/caioc-base.exe >/tmp/decoy-pi.log 2>&1 </dev/null &
DPI=$!
sleep 2
echo "DECOY-prio=$DPR  DECOY-pi=$DPI"
echo "--- retired pattern 'prio/caioc-instr.exe' would have matched pids:"
pgrep -f 'prio/caioc-instr.exe' | tr '\n' ' '; echo
echo "--- retired pattern 'kernel /home/coding-agent/rtems-bringup/pi/' would have matched pids:"
pgrep -f 'kernel /home/coding-agent/rtems-bringup/pi/' | tr '\n' ' '; echo
echo "    (this shell: $$ ; the ssh/bash running the killer is itself a -f match risk)"

echo
echo "=== boot our own prio guest (alone on 5064) ==="
prio/boot-instr.sh
sleep 10
MINE=$(cat prio/prio.qemu.pid)
echo "--- recorded pid=$MINE comm=$(cat /proc/$MINE/comm 2>/dev/null || echo GONE)"
echo "--- all qemu-system-arm ---"; ps -eo pid,comm | grep qemu-system-arm
echo "--- guest actually booted? (last console line) ---"; tail -1 prio/after.log

echo
echo "=== prio/stop-mine.sh ==="
prio/stop-mine.sh; echo "exit=$?"
sleep 1
echo "--- mine gone? $(cat /proc/$MINE/comm 2>/dev/null || echo GONE)"
echo "--- decoys survived? prio=$(cat /proc/$DPR/comm 2>/dev/null || echo GONE) pi=$(cat /proc/$DPI/comm 2>/dev/null || echo GONE)"
echo "--- pidfile removed? ---"; ls prio/prio.qemu.pid 2>&1
echo "--- all qemu-system-arm after stop ---"; ps -eo pid,comm | grep qemu-system-arm

echo
echo "=== second stop is a no-op ==="; prio/stop-mine.sh; echo "exit=$?"
echo "=== clean up decoys by own pid ==="
kill $DPR $DPI 2>/dev/null; sleep 2; kill -9 $DPR $DPI 2>/dev/null
ps -eo pid,comm | grep qemu-system-arm || echo "NO qemu remains"
