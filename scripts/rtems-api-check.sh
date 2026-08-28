#!/usr/bin/env bash
#
# rtems-api-check.sh - the integrity gate for
# `crates/epics-rtems-boot/csrc/tests/rtems-api/`.
#
# That directory records the RTEMS 6 and rtems-libbsd declarations that
# `csrc/rtems_init.c` names, so a runner with no cross toolchain can still
# compile that file with -Werror (see scripts/csrc-check.sh). A record is only
# worth compiling against while it still says what the real headers say, and
# there are exactly two ways for it to stop doing so:
#
#   1. Someone hand-writes a declaration into it to make a compile error go
#      away. Then CI is green against a fiction.
#   2. RTEMS changes a prototype. Then CI is green against last year's API.
#
# Pass 1 (STRUCTURE) closes the first and needs no toolchain, so it runs on
# every push: every line of the record must either sit inside an `@rtems-api`
# block - which pass 2 can check - or be a comment, a blank, an #include, an
# include guard, or an explicitly justified `@rtems-api-local` line. There is
# nowhere to put an unmarked declaration.
#
# Pass 2 (FIDELITY) closes the second and needs an installed BSP, so it runs
# wherever RTEMS_BSP_PREFIX is set - the bring-up box, an image build - and is
# skipped, loudly, where it is not: each `@rtems-api <header>` block must
# appear in `<header>` as a contiguous run of byte-identical lines.
#
# Neither pass is a substitute for the on-target boot, which stays the
# acceptance for the shim.

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

API=crates/epics-rtems-boot/csrc/tests/rtems-api
BSP=${RTEMS_BSP:-xilinx_zynq_a9_qemu}
TOOL_TARGET=arm-rtems6

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

mapfile -t RECORDS < <(find "$API" -name '*.h' | sort)
if [ ${#RECORDS[@]} -eq 0 ]; then
    echo "error: no recorded headers under $API." >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Pass 1: structure.
# ---------------------------------------------------------------------------
echo "==> pass 1: every declaration in the record is marked"
structure_failed=0
for rec in "${RECORDS[@]}"; do
    awk -v rec="$rec" '
        /^\/\* @rtems-api [^ ]+ \*\/$/ {
            if (inblock) {
                printf "%s:%d: block opened while one was already open\n", rec, FNR
                bad = 1
            }
            inblock = 1
            next
        }
        /^\/\* @rtems-api-end \*\/$/ {
            if (!inblock) {
                printf "%s:%d: @rtems-api-end with no open block\n", rec, FNR
                bad = 1
            }
            inblock = 0
            next
        }
        inblock { next }

        /^[[:space:]]*$/ { next }

        # Outside a marked block only these may appear.
        /^[[:space:]]*\/\*/  { next }   # comment opener
        /^[[:space:]]*\*/    { next }   # comment continuation or closer
        /^[[:space:]]*\/\// { next }
        /^#include[[:space:]]/                        { next }
        /^#(ifndef|define)[[:space:]]+EPICS_RS_RECORDED_/ { next }
        /^#endif[[:space:]]+\/\* EPICS_RS_RECORDED_/      { next }

        { printf "%s:%d: unmarked declaration: %s\n", rec, FNR, $0; bad = 1 }
        END {
            if (inblock) { print rec ": a block was never closed by @rtems-api-end"; bad = 1 }
            exit bad ? 1 : 0
        }
    ' "$rec" || structure_failed=1
done
if [ "$structure_failed" -ne 0 ]; then
    cat >&2 <<'EOF'
error: the record contains a declaration no `@rtems-api` marker covers, so
       nothing can prove it against a real header. Put it under a marker
       naming the RTEMS header it was copied from, or - if it is genuinely
       ours - under an `@rtems-api-local:` comment saying why.
EOF
    exit 1
fi

# ---------------------------------------------------------------------------
# Pass 2: fidelity.
# ---------------------------------------------------------------------------
if [ -z "${RTEMS_BSP_PREFIX:-}" ]; then
    echo "==> pass 2: SKIPPED - RTEMS_BSP_PREFIX is unset, so no real headers"
    echo "    to compare against. Run this where a BSP is installed; a runner"
    echo "    cannot have one (see scripts/csrc-check.sh)."
    exit 0
fi

# Two roots because the record spans two trees: the BSP install holds
# <rtems.h> and the libbsd headers, and the toolchain sysroot holds newlib's
# <sys/socket.h>, which is where PF_ROUTE is defined.
ROOTS=(
    "$RTEMS_BSP_PREFIX/$TOOL_TARGET/$BSP/lib/include"
    "$RTEMS_BSP_PREFIX/$TOOL_TARGET/include"
)
for root in "${ROOTS[@]}"; do
    if [ ! -d "$root" ]; then
        echo "error: $root is not a directory." >&2
        echo "       RTEMS_BSP_PREFIX=$RTEMS_BSP_PREFIX RTEMS_BSP=$BSP" >&2
        exit 1
    fi
done

echo "==> pass 2: every recorded block is verbatim in $RTEMS_BSP_PREFIX ($BSP)"

# Is the block in $2 a contiguous run of whole lines somewhere in file $1?
contains_block() {
    awk -v bf="$2" '
        BEGIN { n = 0; while ((getline line < bf) > 0) block[++n] = line }
        { hdr[NR] = $0 }
        END {
            for (i = 1; i + n - 1 <= NR; i++) {
                ok = 1
                for (j = 1; j <= n; j++)
                    if (hdr[i + j - 1] != block[j]) { ok = 0; break }
                if (ok) exit 0
            }
            exit 1
        }
    ' "$1"
}

checked=0
failed=0
for rec in "${RECORDS[@]}"; do
    hdr=
    blk="$work/block"
    : > "$blk"
    while IFS= read -r line || [ -n "$line" ]; do
        if [[ $line =~ ^/\*\ @rtems-api\ ([^\ ]+)\ \*/$ ]]; then
            hdr=${BASH_REMATCH[1]}
            : > "$blk"
            continue
        fi
        [ -n "$hdr" ] || continue
        if [ "$line" = "/* @rtems-api-end */" ]; then
            checked=$((checked + 1))
            found=
            for root in "${ROOTS[@]}"; do
                [ -f "$root/$hdr" ] || continue
                found=$root
                if contains_block "$root/$hdr" "$blk"; then
                    hdr=
                    break
                fi
            done
            if [ -n "$hdr" ]; then
                failed=$((failed + 1))
                if [ -z "$found" ]; then
                    echo "MISSING HEADER  $hdr  (recorded in ${rec#"$API"/})" >&2
                else
                    echo "DRIFTED         $hdr  (recorded in ${rec#"$API"/})" >&2
                    sed 's/^/                | /' "$blk" >&2
                fi
                hdr=
            fi
            continue
        fi
        printf '%s\n' "$line" >> "$blk"
    done < "$rec"
done

echo "==> rtems-api-check: $checked recorded blocks, $failed not found verbatim"
if [ "$failed" -ne 0 ]; then
    cat >&2 <<'EOF'
error: the record no longer matches the installed headers. Either RTEMS
       changed the API - in which case re-record the block and fix
       rtems_init.c to match - or the block was written by hand and never
       came from a header at all.
EOF
    exit 1
fi
