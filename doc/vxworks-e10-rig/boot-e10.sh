#!/bin/bash
# boot-e10.sh <image-basename> <seconds> — boot ONE VxWorks guest, load the RTP
# from the rig ftpd, run it for <seconds>, capture the console, stop.
#
# RIG DISCIPLINE.  This records its own qemu pid in $R/qemu.pid and kills only
# that pid, after re-reading /proc/<pid>/comm.  gv100 also runs another arm's
# long-lived RTEMS guests (qemu-system-arm) which must survive; ~/rtems-bringup/
# rigpid.sh is UNSAFE here because its rig_is_qemu hardcodes qemu-system-arm and
# would silently skip a VxWorks guest.  No pkill/killall of any kind is run.
#
# Resource block for this panel, never another's: hostfwd 21534/25064/25075,
# ftpd 2131, passive 60010-60015, console socket /tmp/vxcon-e10.sock.
set -e -o pipefail
R=$HOME/vx-rig-e10
SDK=$HOME/wrsdk-vxworks7-qemu-1.17.0
KERNEL=$SDK/vxsdk/bsps/itl_generic_3_0_0_5/vxWorks
IMG=$1
SECS=${2:-1400}
# Guest RAM. The E11 abort is a function of it — 1024M dies, 1280M walls
# cleanly — so it is a parameter, not a constant. `-m` is the ONLY thing that
# changes between those arms.
RAM=${VXRAM:-1024M}
CON=/tmp/vxcon-e10.sock
LOG=$R/logs/console-$IMG.log
FTPLOG=$R/logs/ftpd-$IMG.log

[ -f "$R/ftp/root/$IMG.vxe" ] || { echo "no image $R/ftp/root/$IMG.vxe"; exit 1; }
rm -f "$CON" "$R/con.in"
: > "$LOG"

python3 "$R/ftpd-e10.py" "$R/ftp/root" > "$FTPLOG" 2>&1 &
FTPPID=$!
echo $FTPPID > "$R/ftpd.pid"
sleep 1

GUESTFWD="guestfwd=tcp:10.0.2.100:21-cmd:python3 /tmp/pybridge.py 2131"
for p in 60010 60011 60012 60013 60014 60015; do
    GUESTFWD="$GUESTFWD,guestfwd=tcp:10.0.2.100:$p-cmd:python3 /tmp/pybridge.py $p"
done

qemu-system-x86_64 -m "$RAM" -kernel "$KERNEL" \
  -net nic \
  -net "user,hostfwd=tcp:127.0.0.1:21534-:1534,hostfwd=tcp:127.0.0.1:25064-:5064,hostfwd=tcp:127.0.0.1:25075-:5075,$GUESTFWD" \
  -display none -monitor none \
  -chardev "socket,id=vcon,path=$CON,server=on,wait=off" -serial chardev:vcon \
  -append "bootline:fs(0,0)host:vxWorks h=10.0.2.100 e=10.0.2.15 u=target pw=vxTarget o=gei0" \
  > "$R/logs/qemu-$IMG.log" 2>&1 &
QPID=$!
echo $QPID > "$R/qemu.pid"
echo "qemu pid=$QPID image=$IMG ram=$RAM secs=$SECS log=$LOG"

for _ in $(seq 30); do [ -S "$CON" ] && break; sleep 1; done
mkfifo "$R/con.in"
sleep 100000 > "$R/con.in" &
HOLDER=$!
echo $HOLDER > "$R/holder.pid"
nc -U "$CON" < "$R/con.in" > "$LOG" 2>&1 &
NCPID=$!
echo $NCPID > "$R/nc.pid"

sleep 25                       # kernel shell prompt
echo "" > "$R/con.in"
sleep 2
# VXNOLAUNCH=1 leaves the guest at the kernel shell with no RTP started. The
# 768M/832M diagnosis needs the kernel's free pool as it stands BEFORE the
# loader touches it — once the RTP has loaded (or failed and been destroyed)
# memShow no longer answers "was there room for the image".
if [ "${VXNOLAUNCH:-0}" = 1 ]; then
    echo "no-launch: kernel shell only"
else
    echo "rtpSp \"/host.host/$IMG.vxe\"" > "$R/con.in"
    echo "launched; running ${SECS}s"
fi
sleep "$SECS"

echo "=== stopping ==="
"$R/stop-e10.sh"
echo "=== console bytes: $(wc -c < "$LOG") ==="
