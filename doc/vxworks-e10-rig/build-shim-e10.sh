#!/bin/bash
# build-shim-e10.sh — compile heapresidue.c to the object every E10 image links.
#
# Round 1 built this object by hand and did not record the line, which is a hole
# in the rig's provenance: the source is committed but the recipe was not, so
# the object could not be reproduced from the repository alone.  This script is
# that recipe.
#
# TWO THINGS THE SDK FORCES:
#
#  * `-I$WIND_CC_SYSROOT/usr/h/public` and nothing else.  Adding `usr/h` or
#    pre-including <vxWorks.h> pulls in the kernel header tree, which stops with
#    "VSB configuration not associated properly" — an RTP object does not have,
#    and does not need, a VSB.
#
#  * `-Wno-implicit-int`.  The SDK's own <stdio.h> declares `vfprintf` and
#    friends with a `_Va_list` argument whose typedef lives in <yvals.h>, which
#    that header does not reach on this include path.  clang 18 makes implicit
#    int an error, so the SDK header fails to compile against the SDK compiler.
#    None of the affected prototypes is called here — this file calls `printf`.
#
# CARGO DOES NOT SEE THIS OBJECT.  It is passed as `-Clink-arg`, which is not a
# tracked input, so a rebuilt shim alone leaves cargo believing every image is
# up to date and `build-e10.sh` silently ships the OLD object.  Touch the bin
# source (or `cargo clean -p`) after running this, or the change is not in the
# image you then boot.
set -e -o pipefail
source "$HOME/vx-rig-e10/env.sh"
export PATH=$WIND_SDK_CCBASE_PATH:$PATH
R=$HOME/vx-rig-e10

clang --target=x86_64-wrs-vxworks -c -O2 -Wno-implicit-int \
    -I"$WIND_CC_SYSROOT/usr/h/public" \
    -o "$R/heapresidue.o" "$R/heapresidue.c"

# The nesting flags must NOT be thread-local: an image whose allocator wrapper
# touches TLS dies with signal 11 before `main`, because the C runtime's own
# startup allocates before the RTP's TLS base register is set.  Guard it here so
# the fatal build cannot be produced silently.
if readelfpentium -S "$R/heapresidue.o" | grep -q '\.tbss'; then
    echo "FATAL: heapresidue.o has a .tbss section — a __thread cell is back."
    echo "       An image linking this object dies at rtpSp with signal 11."
    exit 1
fi
ls -l "$R/heapresidue.o"
