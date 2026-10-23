//! lemon's report on snl.lem, as the C build of the sequencer wrote it,
//! against the report of the port's lemon: same states, same actions, same
//! state numbers, so the same parse and error-recovery behaviour.

use epics_base_rs::reference::{ReferenceTree, reference_path};

#[test]
fn grammar_report_matches_snl_out() {
    let path = reference_path(
        ReferenceTree::Modules,
        "sequencer/src/snc/O.linux-x86_64/snl.out",
    );
    let want = std::fs::read_to_string(&path).expect("read snl.out");
    let got = epics_snc_rs::grammar_report();
    if got != want {
        let line = got
            .lines()
            .zip(want.lines())
            .position(|(g, w)| g != w)
            .unwrap_or_else(|| got.lines().count().min(want.lines().count()));
        panic!(
            "report differs from {} at line {}:\n  got:  {:?}\n  want: {:?}",
            path.display(),
            line + 1,
            got.lines().nth(line),
            want.lines().nth(line),
        );
    }
}
