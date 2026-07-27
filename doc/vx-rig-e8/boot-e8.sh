#!/bin/bash
# E8 rig: boot the VxWorks 7 guest for the CA WorkerPool banding measurement.
#
# The working qemu form of doc/vxworks-port.md section 6, moved onto the E8
# port block: host 31534/35064/35075, ftpd 2141, passive 60020-60025, console
# socket /tmp/vxcon-e8.sock.  Every port here is this panel's alone -- gv100
# runs three measurement panels plus two protected RTEMS guests, and booting on
# a shared port makes qemu refuse to start.
#
# RIG DISCIPLINE: records ONLY its own qemu pid in $D/qemu.pid.  Never
# pkill/killall against qemu on this box: pids 3308690 and 3309544 are another
# arm's long-running RTEMS guests and must survive.  Stop with ./stop-e8.sh,
# which re-checks /proc/<pid>/comm before killing.
set -u
SDK=$HOME/wrsdk-vxworks7-qemu-1.17.0
KERNEL=$SDK/vxsdk/bsps/itl_generic_3_0_0_5/vxWorks
D=$HOME/vx-rig-e8
MEM=${1:-1024M}
LOG=$D/console.log
FIFO=$D/con.in
SOCK=/tmp/vxcon-e8.sock

[ -f "$KERNEL" ] || { echo "no kernel at $KERNEL"; exit 1; }
mkdir -p "$D/ftp/root"

# The FTP bridge must propagate EOF or rtpSpawn blocks forever (section 6):
# /tmp/pybridge.py exits the instant the host socket closes; a cmd:nc bridge
# transfers every byte and never closes, so netDrv never sees end-of-file.
GF=""
GF="$GF,guestfwd=tcp:10.0.2.100:21-cmd:python3 /tmp/pybridge.py 2141"
for p in 60020 60021 60022 60023 60024 60025; do
    GF="$GF,guestfwd=tcp:10.0.2.100:$p-cmd:python3 /tmp/pybridge.py $p"
done

rm -f "$SOCK" "$FIFO"
mkfifo "$FIFO"
: > "$LOG"

# Serial goes to a unix-socket chardev, not stdio: -serial stdio collides with
# the cmd: chardevs above.
setsid qemu-system-x86_64 -m "$MEM" -kernel "$KERNEL" \
    -net nic \
    -net "user,hostfwd=tcp:127.0.0.1:31534-:1534,hostfwd=tcp:127.0.0.1:35064-:5064,hostfwd=tcp:127.0.0.1:35075-:5075$GF" \
    -display none -monitor none \
    -chardev "socket,id=vcon,path=$SOCK,server=on,wait=off" -serial chardev:vcon \
    -append "bootline:fs(0,0)host:vxWorks h=10.0.2.100 e=10.0.2.15 u=target pw=vxTarget o=gei0" \
    < /dev/null > "$D/qemu.out" 2>&1 &
echo $! > "$D/qemu.pid"
sleep 2

# Holder keeps the FIFO open for write, so a one-shot `echo > con.in` closing
# its end does not deliver EOF to the console bridge.
setsid sh -c "exec sleep 86400 > $FIFO" < /dev/null > /dev/null 2>&1 &
echo $! > "$D/holder.pid"
setsid sh -c "exec nc -U $SOCK < $FIFO > $LOG 2>&1" < /dev/null > /dev/null 2>&1 &
echo $! > "$D/conbridge.pid"

echo "qemu=$(cat "$D/qemu.pid") holder=$(cat "$D/holder.pid") con=$(cat "$D/conbridge.pid") mem=$MEM"
echo "console: $LOG   drive: echo 'rtpSp \"/host.host/realtime-ca-ioc.vxe\"' > $FIFO"
