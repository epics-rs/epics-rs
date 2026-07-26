#!/bin/bash
# build-e10.sh <ca|pva> <outname>  — build one E10 image with the --wrap shim.
set -e -o pipefail
source $HOME/vx-rig-e10/env.sh
R=$HOME/vx-rig-e10
LIBC_PATCH=$(ls -d $R/target/libc-std-patch/*-0.2.185)
OBJ=$R/heapresidue.o

WRAP="-Clink-arg=$OBJ"
for s in malloc free calloc realloc memalign posix_memalign aligned_alloc; do
    WRAP="$WRAP -Clink-arg=-Wl,--wrap=$s"
done
export RUSTFLAGS="$WRAP"

case "$1" in
  ca)  PKG=epics-ca-rs;     BIN=realtime-ca-ioc;  FEATS=client-core,bringup-probes ;;
  pva) PKG=epics-bridge-rs; BIN=realtime-pva-ioc; FEATS=qsrv-core,pvalink,bringup-probes ;;
  *) echo "usage: $0 <ca|pva> <outname>"; exit 1 ;;
esac

cd $R/tree
cargo +nightly build --release -j4 \
    --target x86_64-wrs-vxworks -Zbuild-std=std,panic_abort \
    --config "patch.crates-io.libc-std.package=\"libc\"" \
    --config "patch.crates-io.libc-std.path=\"$LIBC_PATCH\"" \
    --config "profile.release.debug=\"line-tables-only\"" \
    -p $PKG --bin $BIN --no-default-features --features $FEATS

SRC=$R/target/x86_64-wrs-vxworks/release/$BIN.vxe
cp $SRC $R/$2-unstripped.vxe
cp $SRC $R/ftp/root/$2.vxe
ls -l $R/$2-unstripped.vxe $R/ftp/root/$2.vxe
