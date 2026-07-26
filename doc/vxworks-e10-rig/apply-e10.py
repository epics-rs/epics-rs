#!/usr/bin/env python3
"""apply-e10.py — the E10 rig mutations, anchored so a drifted tree fails loudly.

    apply-e10.py <tree> probe        # report call + rig cadence (both IOCs)
    apply-e10.py <tree> perattempt   # DialPool::dial -> one transient thread
    apply-e10.py <tree> revert       # restore every .e10-orig backup

Every replacement is anchored on an exact string and asserts the anchor was
found exactly once.  A tree whose source moved therefore stops the rig instead
of producing a number from an image that is not the one described.
"""

import sys
import os
import shutil

CA_BIN = "crates/epics-ca-rs/src/bin/realtime-ca-ioc.rs"
PVA_BIN = "crates/epics-bridge-rs/src/bin/realtime-pva-ioc.rs"
PVA_NS = "crates/epics-pva-rs/src/client_native/search_engine.rs"
BLOCKING_IO = "crates/epics-libcom-rs/src/runtime/blocking_io.rs"


def edit(tree, rel, subs):
    path = os.path.join(tree, rel)
    with open(path) as f:
        src = f.read()
    for anchor, replacement in subs:
        n = src.count(anchor)
        assert n == 1, f"{rel}: anchor occurs {n} times, expected 1:\n{anchor}"
        src = src.replace(anchor, replacement)
    backup = path + ".e10-orig"
    if not os.path.exists(backup):
        shutil.copy2(path, backup)
    with open(path, "w") as f:
        f.write(src)
    print(f"patched {rel}")


EXTERN_DECL = """    // E10 RIG: the `-Wl,--wrap` live-block heap accounting shim linked into
    // this image (`heapresidue.c`).  Not production code — the rig applies it
    // with `apply-e10.py probe` and reverts it from the `.e10-orig` backup.
    // Plain comments, not doc comments: rustdoc generates nothing for an
    // extern block and warns.
    #[cfg(feature = "bringup-probes")]
    unsafe extern "C" {
        fn heapresidue_report(seq: u32, detail: i32);
    }

"""

REPORT_CALL = """
        // E10 RIG: live-block heap accounting, sampled on the same line group
        // as the attempt count the residue is priced per.  The per-size and
        // per-site tables ride the census cadence (every 6th pass); the totals
        // go out every pass.
        //
        // SAFETY: `heapresidue_report` reads its own static tables through
        // relaxed atomics and printf, takes no pointer from us, and is linked
        // into this image by the rig's `-C link-arg`.
        unsafe {
            heapresidue_report(seq, i32::from(seq.is_multiple_of(6)));
        }
"""


def probe(tree):
    ca_anchor = """             queued={dial_queued} dialing={dial_dialing} \\
             MEM_FREE={mem_free} MEM_USED={mem_used}",
        );
"""
    edit(
        tree,
        CA_BIN,
        [
            # 1. declare the shim, ABOVE `c6_report`'s own doc comment: inserted
            #    between the doc comment and the `#[cfg]` it belongs to, the
            #    extern block would swallow those docs (`unused doc comment`).
            (
                "    /// C6 PROBE: one console report — the link registry, the shared",
                EXTERN_DECL
                + "    /// C6 PROBE: one console report — the link registry, the shared",
            ),
            # 2. call it from the 10 s report
            (ca_anchor, ca_anchor + REPORT_CALL),
            # 3. rig cadence: EPICS_CA_CONN_TMO=5 so 1260 s yields ~250 dial
            #    attempts instead of the shipped 30 s cadence's ~40.  Changes
            #    how often an attempt happens, never what one attempt allocates.
            (
                """            ("EPICS_CA_AUTO_ADDR_LIST", "NO"),
        ] {""",
                """            ("EPICS_CA_AUTO_ADDR_LIST", "NO"),
            ("EPICS_CA_CONN_TMO", "5"),
        ] {""",
            ),
        ],
    )

    pva_anchor = """            "STAGE5 seq={seq} dialpool workers={dial_workers} attempts={dial_attempts} \\
             MEM_FREE={mem_free} MEM_USED={mem_used}",
        );
"""
    edit(
        tree,
        PVA_BIN,
        [
            (
                "    /// STAGE-5 PROBE: one console report — the link registry, the ONE",
                EXTERN_DECL
                + "    /// STAGE-5 PROBE: one console report — the link registry, the ONE",
            ),
            (pva_anchor, pva_anchor + REPORT_CALL),
        ],
    )

    # PVA rig cadence, the counterpart of EPICS_CA_CONN_TMO=5 above: there is no
    # environment variable for it, so the constant is edited.
    edit(
        tree,
        PVA_NS,
        [
            (
                "    const RECONNECT_INTERVAL: Duration = Duration::from_secs(10);",
                "    const RECONNECT_INTERVAL: Duration = Duration::from_secs(5); // E10 RIG: was 10",
            )
        ],
    )


PERATTEMPT = '''    pub fn dial(
        &'static self,
        target: SocketAddr,
    ) -> io::Result<oneshot::Receiver<io::Result<TcpStream>>> {
        // E10 RIG — THE PER-ATTEMPT ARM.  The pre-`9daff491` shape: one
        // transient thread created per attempt and left to exit, with the same
        // name stem, the same band, the same `StackSizeClass::Small` and the
        // same `oneshot` reply channel as the pooled path, and the same
        // single-finalizer rule for the socket it opens.
        //
        // Mutating `DialPool::dial` rather than each caller is what makes ONE
        // mutation cover both halves of E10: `CA_DIAL_POOL` and
        // `PVA_DIAL_POOL` are two instances of this type.
        //
        // `workers` is incremented and never returned, so `worker_count()`
        // equals the number of attempts in this arm and stays <= 4 in the
        // pooled one — the console proof that the mutation is live, on the
        // line the attempt count is already printed on.
        let (reply, rx) = oneshot::channel();
        let index = {
            let mut q = self.lock();
            q.workers += 1;
            q.workers
        };
        let name_prefix = self.name_prefix;
        if let Err(e) = spawn_dedicated_thread(
            format!("{name_prefix} {index}"),
            self.priority,
            StackSizeClass::Small,
            move || {
                let dialed = (!reply.is_closed()).then(|| TcpStream::connect(target));
                if let Some(dialed) = dialed {
                    let _ = reply.send(dialed);
                }
            },
        ) {
            self.lock().workers -= 1;
            return Err(e);
        }
        Ok(rx)
    }

    /// Unreachable in the per-attempt arm; kept so the diff is one function.
    #[allow(dead_code)]
'''


def perattempt(tree):
    # The pooled `dial` body, verbatim, is replaced wholesale; the marker for
    # its end is the doc comment of `worker_loop`, which the replacement keeps.
    anchor = """    pub fn dial(
        &'static self,
        target: SocketAddr,
    ) -> io::Result<oneshot::Receiver<io::Result<TcpStream>>> {
        let (reply, rx) = oneshot::channel();
        let req = DialRequest { target, reply };

        let mut q = self.lock();
        // Each queued request already claims one available worker, so this
        // request is covered only if the available ones outnumber the queue.
        if q.pending.len() + q.busy < q.workers || q.workers >= MAX_DIAL_WORKERS {
            q.pending.push_back(req);
            drop(q);
            self.work.notify_one();
            return Ok(rx);
        }

        // Create the worker *before* queueing, so a spawn failure leaves the
        // pool exactly as it found it and the caller keeps its error.
        let index = q.workers;
        q.workers += 1;
        drop(q);
        if let Err(e) = spawn_dedicated_thread(
            format!("{} {index}", self.name_prefix),
            self.priority,
            StackSizeClass::Small,
            move || self.worker_loop(),
        ) {
            self.lock().workers -= 1;
            return Err(e);
        }
        self.lock().pending.push_back(req);
        self.work.notify_one();
        Ok(rx)
    }

"""
    edit(tree, BLOCKING_IO, [(anchor, PERATTEMPT)])


def revert(tree):
    n = 0
    for root, _dirs, files in os.walk(os.path.join(tree, "crates")):
        for f in files:
            if f.endswith(".e10-orig"):
                backup = os.path.join(root, f)
                target = backup[: -len(".e10-orig")]
                shutil.copy2(backup, target)
                os.remove(backup)
                print(f"reverted {os.path.relpath(target, tree)}")
                n += 1
    print(f"reverted {n} file(s)")


if __name__ == "__main__":
    tree, mode = sys.argv[1], sys.argv[2]
    {"probe": probe, "perattempt": perattempt, "revert": revert}[mode](tree)
