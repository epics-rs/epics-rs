#!/usr/bin/env bash
# Compatibility smoke: run REAL EPICS artifacts through the C `softIoc` and the
# port's `softioc-rs` with identical argv, and report where the two diverge.
#
# The question is not "are the bytes the same" — it is "does an existing site
# artifact still work". So the primary verdict is the IOC's own report of what
# it loaded (`dbl`, `dbgrep`), not console text; console text is secondary and
# normalised. Nothing here is fixed by this script: it reports.
#
# Usage:
#   scripts/compat-smoke.sh [--limit N] [--only db|acf|cmd] [--verbose]
#                           [--s-limit N]   # -S pass: 0 skips, <0 unlimited
#                           [--diffs DIR]   # keep every stderr diff for triage
#
# Requires EPICS_BASE and EPICS_MODULES to point at the C reference trees.
set -uo pipefail

BASE=${EPICS_BASE:-/home/stevek/work/epics-base}
MODULES=${EPICS_MODULES:-/home/stevek/work/epics-modules}
C_IOC=$BASE/bin/linux-x86_64/softIoc
R_IOC=${R_IOC:-$PWD/target/debug/softioc-rs}

# Never 5064. A test that binds the site default steals a real IOC's port and
# makes every later measurement a split-brain result.
export EPICS_CA_SERVER_PORT=15164 EPICS_CAS_SERVER_PORT=15164
export EPICS_PVA_SERVER_PORT=15175 EPICS_PVAS_SERVER_PORT=15175
export EPICS_CA_AUTO_ADDR_LIST=NO EPICS_CA_ADDR_LIST=127.0.0.1
export TERM=

LIMIT=0 ONLY=all VERBOSE=0 TMO=8 STMO=3 SLIMIT=12 DIFFS=  # SLIMIT: 0 skips pass 2
while [ $# -gt 0 ]; do
    case $1 in
        --limit) LIMIT=$2; shift 2 ;;
        --only) ONLY=$2; shift 2 ;;
        --verbose) VERBOSE=1; shift ;;
        --timeout) TMO=$2; shift 2 ;;
        --s-limit) SLIMIT=$2; shift 2 ;;
        --diffs) DIFFS=$2; shift 2 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

WORK=$(mktemp -d "${TMPDIR:-/tmp}/compat-smoke.XXXXXX") || exit 1
trap 'rm -rf "$WORK"' EXIT

# ---------------------------------------------------------------- preflight
# Two different hazards, and only one of them is fatal.
#
# Fatal: something already owns a port this run needs. Then two IOCs answer at
# once and every comparison after it is split-brain.
#
# Not fatal: another IOC exists on the box at all. This machine runs several
# panels in parallel and a sibling's A/B is none of this run's business — it is
# named, not obeyed, because refusing on it means this harness can never run
# while anyone else is working. `pgrep -x` matches the process NAME, so a
# `clippy-driver` line that merely mentions `softioc_rs` in a path is not a hit.
preflight() {
    local bad=0
    for b in "$C_IOC" "$R_IOC"; do
        [ -x "$b" ] || { echo "PREFLIGHT FAIL: not executable: $b" >&2; bad=1; }
    done
    for p in "$EPICS_CAS_SERVER_PORT" "$EPICS_PVAS_SERVER_PORT"; do
        if ss -lntu 2>/dev/null | grep -qE "[:.]$p\b"; then
            echo "PREFLIGHT FAIL: port $p is already bound:" >&2
            ss -lntup 2>/dev/null | grep -E "[:.]$p\b" >&2
            bad=1
        fi
    done
    local stray
    stray=$( { pgrep -x softIoc; pgrep -x softioc-rs; } 2>/dev/null | tr '\n' ' ')
    [ -n "$stray" ] && echo "PREFLIGHT NOTE: other IOC pids on this box: $stray (not on this run's ports)" >&2
    [ $bad -eq 0 ] || exit 1
}

# Re-checked after the sweep: a sibling that grabbed a port mid-run would have
# invalidated everything measured after that moment, and the run must say so
# rather than publish the numbers.
postflight() {
    for p in "$EPICS_CAS_SERVER_PORT" "$EPICS_PVAS_SERVER_PORT"; do
        if ss -lntup 2>/dev/null | grep -E "[:.]$p\b" | grep -qv 'softIoc\|softioc-rs'; then
            echo "POSTFLIGHT WARN: port $p was taken by a non-IOC process during the run;" >&2
            echo "                 treat these results as unverified." >&2
        fi
    done
}

# ------------------------------------------------------------------- corpus
# Real artifacts only. Nothing here is authored by the harness; every path is
# a file that shipped with epics-base or an epics-modules module.
corpus() {
    if [ "$ONLY" = all ] || [ "$ONLY" = db ]; then
        # Record databases: dbLoadRecords over the real record/field grammar.
        find "$BASE/modules" -name '*.db' -type f ! -name '._*' 2>/dev/null |
            sort | sed 's|^|db\t|'
        find "$BASE/modules" \( -name '*.substitutions' -o -name '*.template' \) \
            -type f ! -name '._*' 2>/dev/null | sort | sed 's|^|tpl\t|'
    fi
    if [ "$ONLY" = all ] || [ "$ONLY" = acf ]; then
        # Access-security files: the boundary this round moved.
        find "$BASE" "$MODULES" -name '*.acf' -type f ! -name '._*' 2>/dev/null |
            sort | sed 's|^|acf\t|'
    fi
    if [ "$ONLY" = all ] || [ "$ONLY" = cmd ]; then
        # Real startup scripts: iocsh vocabulary, macro handling, and the
        # lines that run AFTER iocInit.
        # `._*` are macOS AppleDouble resource forks that rode into these
        # checkouts. They are binary, not startup scripts, and feeding them to
        # an IOC measures blob-handling rather than compatibility.
        find "$BASE/modules" "$MODULES" -name '*.cmd' -type f ! -name '._*' \
            2>/dev/null | sort | sed 's|^|cmd\t|'
    fi
}

# ------------------------------------------------------------------ running
# One case, one binary. Builds the script the case needs, runs the binary with
# the script's own directory as cwd (a real st.cmd resolves `../../dbd/...`
# relative to itself), and captures stdout, stderr and status separately.
#
# Every case ends with `dbl` so the IOC reports the PV set it actually built —
# that report, not the console chatter, is what a site cares about.
run_one() {
    local kind=$1 art=$2 bin=$3 out=$4 mode=$5
    local script=$WORK/st.cmd cwd=$PWD extra=()
    case $kind in
        db)  printf 'dbLoadRecords("%s")\niocInit\ndbl\n' "$art" > "$script" ;;
        tpl) printf 'dbLoadTemplate("%s")\niocInit\ndbl\n' "$art" > "$script" ;;
        acf) printf 'dbLoadRecords("%s/modules/database/test/std/rec/aiTest.db")\nasSetFilename("%s")\niocInit\ndbl\nasdbdump\n' \
                 "$BASE" "$art" > "$script" ;;
        cmd) # Run the site's own script verbatim, then ask what it built.
             cat "$art" > "$script"
             printf '\ndbl\n' >> "$script"
             cwd=$(dirname "$art") ;;
    esac
    local t=$TMO
    # `-S` never returns on its own: the C IOC's healthy answer is to sit in
    # `epicsThreadSleep` forever, so every -S run costs its whole timeout.
    # That is why pass 2 is sampled and given a short one.
    [ "$mode" = S ] && { extra+=(-S); t=$STMO; }
    ( cd "$cwd" && timeout -k 2 "$t" "$bin" "${extra[@]}" "$script" ) \
        > "$out.out" 2> "$out.err" < /dev/null
    echo $? > "$out.rc"
}

# Strip what two DIFFERENT IOC implementations are entitled to differ on: the
# banner, the build id, the port announcements, the record-count line, the
# prompt, ANSI, and any path that names this run's temp directory.
#
# `cas WARNING:` is NOT in that list and MUST NOT be added to it. It is the
# line RSRV writes when the port it was configured with is already held, which
# is the one signal that separates a run poisoned by a leaked IOC from a clean
# one. Deleting it makes the two produce byte-identical text and "no collision
# this time" unfalsifiable; leaving the assigned port NUMBER in it is almost as
# bad, because the number differs per run and the collision then hides inside
# the stderr bucket as an ordinary parity divergence. So the block stays in the
# compared text with only the number elided, and `collision_check` below turns
# every occurrence into a loud, counted, run-voiding flag.
normalise() {
    sed -E \
        -e 's/\x1b\[[0-9;]*m//g' \
        -e '/^#{4,}/d' \
        -e '/^## /d' \
        -e '/^Starting iocInit/d' \
        -e '/^iocRun: All initialization complete/d' \
        -e '/records, .* with device support/d' \
        -e '/^CA server: /d' \
        -e '/^PVA server/d' \
        -e 's/^(cas WARNING: Using dynamically assigned (TCP|UDP) port )[0-9]+/\1<dynamic>/' \
        -e 's/^epics> //' \
        -e "s|$WORK|<work>|g" \
        -e '/^[[:space:]]*$/d' \
        "$1"
}

# The PV set the IOC built, from `dbl`. A `dbl` line is a bare record name; the
# console chatter around it is dropped by taking only lines that look like one.
pv_set() {
    # `]` first and `-` last: inside an ERE bracket expression a backslash is
    # a literal backslash, so the obvious `[...\[\]-]` closes the class at the
    # first `]` and silently matches nothing. That spelling made every PV set
    # empty and turned "no PV differences" into a false zero.
    grep -aoE '^[]A-Za-z0-9_:.<>;[-]+$' "$1" 2>/dev/null | sort -u
}

# A case whose IOC could not take its own port did not measure that IOC, so
# this runs BEFORE any verdict is formed and its count gates the exit status.
# `preflight`/`postflight` bracket the run; this catches a port taken and
# released inside it, which neither of them can see.
collision_check() {
    local errn=$1 kind=$2 art=$3 tag=$4
    grep -q '^cas WARNING:' "$errn" || return 0
    printf 'PORT-COLLISION\t%s\t%s\t%s\n' "$kind" "$art" "$tag" >&2
    COLLISIONS=$((COLLISIONS + 1))
}

# ----------------------------------------------------------------- verdicts
preflight
[ -n "$DIFFS" ] && { mkdir -p "$DIFFS" || exit 1; }

# `nonempty` guards against a false zero: if every case built no records at
# all, "PV sets identical" would be two empty files agreeing, which proves
# nothing. The summary reports how many comparisons had records in them.
total=0 same=0 pvdiff=0 errdiff=0 rcdiff=0 nonempty=0 COLLISIONS=0
declare -a FLAGGED=()

while IFS=$'\t' read -r kind art; do
    [ -n "${art:-}" ] || continue
    total=$((total + 1))
    [ "$LIMIT" -gt 0 ] && [ "$total" -gt "$LIMIT" ] && { total=$((total - 1)); break; }

    c=$WORK/c r=$WORK/r
    run_one "$kind" "$art" "$C_IOC" "$c" plain
    run_one "$kind" "$art" "$R_IOC" "$r" plain

    crc=$(cat "$c.rc") rrc=$(cat "$r.rc")
    pv_set "$c.out" > "$c.pv"; pv_set "$r.out" > "$r.pv"
    normalise "$c.err" > "$c.errn"; normalise "$r.err" > "$r.errn"

    [ -s "$c.pv" ] && nonempty=$((nonempty + 1))

    collision_check "$c.errn" "$kind" "$art" C
    collision_check "$r.errn" "$kind" "$art" R

    why=()
    cmp -s "$c.pv" "$r.pv" || { why+=("PV-SET"); pvdiff=$((pvdiff + 1)); }
    [ "$crc" = "$rrc" ] || { why+=("EXIT($crc/$rrc)"); rcdiff=$((rcdiff + 1)); }
    if ! cmp -s "$c.errn" "$r.errn"; then
        why+=("STDERR"); errdiff=$((errdiff + 1))
        # A stderr difference is not a verdict on its own — it has to be read.
        # Keeping the normalised pair (not just the diff) is what lets a later
        # pass bucket by shape and tell a reworded message from a missing one.
        if [ -n "$DIFFS" ]; then
            slug=$(printf '%s' "${kind}_${art}" | tr -c 'A-Za-z0-9._-' '_')
            cp "$c.errn" "$DIFFS/$slug.c"
            cp "$r.errn" "$DIFFS/$slug.r"
            # stdout too: iocsh echoes each line it dispatches, so the count of
            # echoed lines is how far the script actually got. A stderr
            # difference only matters if one side stopped running the script.
            cp "$c.out" "$DIFFS/$slug.cout"
            cp "$r.out" "$DIFFS/$slug.rout"
            cp "$c.rc" "$DIFFS/$slug.crc"
            cp "$r.rc" "$DIFFS/$slug.rrc"
        fi
    fi

    if [ ${#why[@]} -eq 0 ]; then
        same=$((same + 1))
        [ $VERBOSE -eq 1 ] && echo "SAME  $kind ${art#"$BASE"/}"
    else
        FLAGGED+=("$kind|$art|${why[*]}")
        echo "DIFF  $kind ${art#"$BASE"/}  [${why[*]}]"
        if [ "${why[*]}" != "${why[*]#*PV-SET}" ]; then
            echo "      PV set only C built:    $(comm -23 "$c.pv" "$r.pv" | tr '\n' ' ')"
            echo "      PV set only port built: $(comm -13 "$c.pv" "$r.pv" | tr '\n' ' ')"
        fi
        if [ $VERBOSE -eq 1 ]; then
            diff -u "$c.errn" "$r.errn" | sed -n '3,12p' | sed 's/^/      /'
        fi
    fi
done < <(corpus)

echo
postflight
echo "cases=$total  identical=$same  pv-set=$pvdiff  exit=$rcdiff  stderr=$errdiff"
echo "of those, $nonempty case(s) actually built records — the PV comparison"
echo "is over that many non-empty sets, not over $total pairs of empty ones."
echo "port collisions=$COLLISIONS  (a non-zero count voids every number above)"

# Pass 2: only the flagged cases, under -S, where nothing reads the script's
# tail and the process is expected to stay up. A `-S` run never exits on its
# own, so its verdict is "still alive at the timeout" plus whether it listens.
if [ ${#FLAGGED[@]} -gt 0 ] && [ "$SLIMIT" -ne 0 ]; then
    echo
    n=${#FLAGGED[@]}
    [ "$SLIMIT" -gt 0 ] && [ "$n" -gt "$SLIMIT" ] && n=$SLIMIT
    echo "--- pass 2: -S on $n of ${#FLAGGED[@]} flagged case(s), ${STMO}s each ---"
    for f in "${FLAGGED[@]:0:$n}"; do
        IFS='|' read -r kind art _ <<< "$f"
        for pair in "C:$C_IOC" "R:$R_IOC"; do
            tag=${pair%%:*} bin=${pair#*:}
            run_one "$kind" "$art" "$bin" "$WORK/s$tag" S
            rc=$(cat "$WORK/s$tag.rc")
            # 124 from `timeout` means it was still running: the C answer for
            # a healthy -S IOC. Anything else is an exit the other side may not
            # have taken.
            normalise "$WORK/s$tag.err" > "$WORK/s$tag.errn"
            collision_check "$WORK/s$tag.errn" "$kind" "$art" "$tag-S"
            alive=$([ "$rc" = 124 ] && echo alive || echo "exited($rc)")
            echo "  $tag $alive  $kind ${art#"$BASE"/}"
        done
    done
fi

# Last, so pass 2's boots are counted too. A held port means the numbers above
# describe a contaminated run, and a harness that reports them anyway is the
# defect this exists to prevent.
if [ "$COLLISIONS" -gt 0 ]; then
    printf 'PORT-COLLISION: %d run(s) met a held port. This corpus run is VOID.\n' \
        "$COLLISIONS" >&2
    exit 3
fi
