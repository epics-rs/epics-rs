#!/usr/bin/env python3
"""E11 heap decomposition: the MINIMAL mutation set for the 1024M abort.

`apply-e10.py probe` is the wrong tool here.  It bundles the E10 dial cadence
(`EPICS_CA_CONN_TMO` = 5) with the shim hook, and the defect under measurement
is on `main`: an image that redials four times more often than the shipped one
is not the program that dies.  This applies exactly two things:

  1. the `heapresidue_report` extern and its call, so the shim can speak;
  2. the probe loop's 10 s sleep dropped to 1 s, because the connection ramp
     that kills the IOC takes under two seconds at the driver's full rate and a
     10 s sample would land either side of it.  The driver is slowed to ~1
     connection/s to match, so the pair gives one heap sample per connection.

Nothing else is touched — no cadence, no dial shape, no timer.  Backups are
`.e11-orig`, a different suffix from `apply-e10.py`'s, so the two mutation sets
can never revert each other's files.
"""
import os
import shutil
import sys

CA_BIN = "crates/epics-ca-rs/src/bin/realtime-ca-ioc.rs"

EXTERN_DECL = """    // E11 RIG: the `-Wl,--wrap` live-block heap accounting shim linked into
    // this image (`heapresidue.c`).  Not production code — the rig applies it
    // with `apply-e11.py probe` and reverts it from the `.e11-orig` backup.
    #[cfg(feature = "bringup-probes")]
    unsafe extern "C" {
        fn heapresidue_report(seq: u32, detail: i32);
    }

"""

REPORT_CALL = """
        // E11 RIG: live-block heap accounting, one FULL sample per probe pass.
        // detail=1 unconditionally, unlike the E10 rig's every-6th-pass census:
        // the ramp that kills the IOC is 41 connections long and the question is
        // which site grows per connection, so the per-size and per-site tables
        // have to land at the same 1 s cadence the driver dials at.
        //
        // SAFETY: `heapresidue_report` reads its own static tables through
        // relaxed atomics and printf, takes no pointer from us, and is linked
        // into this image by the rig's `-C link-arg`.
        unsafe {
            heapresidue_report(seq, 1);
        }
"""

CA_ANCHOR = """             queued={dial_queued} dialing={dial_dialing} \\
             MEM_FREE={mem_free} MEM_USED={mem_used}",
        );
"""

SLEEP_FROM = "                        thread::sleep(std::time::Duration::from_secs(10));"
SLEEP_TO = (
    "                        thread::sleep(std::time::Duration::from_secs(1));"
    "  // E11 RIG cadence"
)


def edit(tree, rel, subs):
    p = os.path.join(tree, rel)
    bak = p + ".e11-orig"
    if not os.path.exists(bak):
        shutil.copy2(p, bak)
    s = open(p).read()
    for a, b in subs:
        assert a in s, "anchor missing in %s: %r" % (rel, a[:60])
        s = s.replace(a, b, 1)
    open(p, "w").write(s)
    print("patched " + rel)


def probe(tree):
    edit(
        tree,
        CA_BIN,
        [
            (
                "    /// C6 PROBE: one console report — the link registry, the shared",
                EXTERN_DECL
                + "    /// C6 PROBE: one console report — the link registry, the shared",
            ),
            (CA_ANCHOR, CA_ANCHOR + REPORT_CALL),
            (SLEEP_FROM, SLEEP_TO),
        ],
    )


def revert(tree):
    n = 0
    for root, _dirs, files in os.walk(os.path.join(tree, "crates")):
        for f in files:
            if f.endswith(".e11-orig"):
                backup = os.path.join(root, f)
                target = backup[: -len(".e11-orig")]
                shutil.copy2(backup, target)
                os.remove(backup)
                print("reverted " + os.path.relpath(target, tree))
                n += 1
    print("reverted %d file(s)" % n)


if __name__ == "__main__":
    tree, mode = sys.argv[1], sys.argv[2]
    {"probe": probe, "revert": revert}[mode](tree)
