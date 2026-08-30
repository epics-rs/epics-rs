//! **C pushes the scope BEFORE it parses the definition list, so a scoped
//! definition can reference the ones before it.**
//!
//! `macCore.c:827-850` interleaves the push with the parse: `macPushScope`
//! runs first, then the `while (*r == ',')` loop translates each definition's
//! value and `macPutValue`s it into the scope that is already on the stack. So
//! definition N sees definitions 1..N-1 and nothing after them, and each value
//! is resolved once, where it is written — not re-resolved at reference time.
//!
//! This port staged the definitions in a `Vec` and pushed the whole frame
//! afterwards, which cannot express any of that: every definition saw the same
//! enclosing scope, so a forward reference silently read the OUTER value.
//!
//! Measured on `softIoc` built from `~/work/epics-base` (`R7.0.10`), with an
//! outer `A=outer` supplied to `dbLoadRecords`. The value is read back out of a
//! numeric field, because a `.db` that loads cleanly prints nothing at all:
//!
//! ```text
//! ERROR: Can't set 'Q1.PREC' to 'q[plain]'    : No digits to convert
//! ERROR: Can't set 'Q2.PREC' to 'q[2]'        : No digits to convert
//! ERROR: Can't set 'Q3.PREC' to 'q[1]'        : No digits to convert
//! ERROR: Can't set 'Q4.PREC' to 'q[1-tail]'   : No digits to convert
//! ERROR: Can't set 'Q5.PREC' to 'q[outer]'    : No digits to convert
//! ERROR: Can't set 'Q6.PREC' to 'q[outer]'    : No digits to convert
//! ```
//!
//! Rows 3 and 4 are the ones this port got wrong: it wrote `outer` and
//! `outer-tail`.

use std::collections::HashMap;

use epics_base_rs::server::db_loader::{MacroExpandOptions, expand_macros};

/// One case per boundary of "which definitions are visible from a definition",
/// not one per story.
#[test]
fn a_scoped_definition_sees_the_ones_declared_before_it() {
    let macros = HashMap::from([("A".to_string(), "outer".to_string())]);
    for (raw, want) in [
        // No forward reference at all — the baseline the old staging got right.
        ("$(B,B=plain)", "plain"),
        ("$(B,A=1,B=2)", "2"),
        // FORWARD: `B` is declared after `A`, so `A` is already in the scope
        // C pushed, and `$(A)` is the scoped `1` — not the outer `outer`.
        ("$(B,A=1,B=$(A))", "1"),
        // Same, with the reference embedded rather than the whole value, so a
        // fix that special-cased a bare `$(A)` cannot pass.
        ("$(B,A=1,B=$(A)-tail)", "1-tail"),
        // REVERSE: `B` is declared FIRST, so `A` is not in the scope yet and
        // the outer one wins. This is also what proves the value is resolved
        // where it is written: if it were kept raw and re-resolved at
        // reference time, the later `A=1` would have won and this would read
        // `1`.
        ("$(B,B=$(A),A=1)", "outer"),
        // SELF: the outer `A` is visible from inside `A`'s own definition,
        // because `macPutValue` has not run for it yet.
        ("$(A,A=$(A))", "outer"),
    ] {
        let got = expand_macros(raw, &macros, MacroExpandOptions::default());
        assert_eq!(got.text, want, "expanding {raw:?}");
        assert!(!got.errored(), "{raw:?} must not report a fault");
    }
}

/// The scope is popped on the way out — a definition must not leak past the
/// reference that declared it, in either direction.
#[test]
fn a_scoped_definition_does_not_outlive_its_reference() {
    let macros = HashMap::from([("A".to_string(), "outer".to_string())]);
    let got = expand_macros(
        "$(B,A=1,B=$(A))|$(A)",
        &macros,
        MacroExpandOptions::default(),
    );
    assert_eq!(got.text, "1|outer", "the scoped A must not survive the `)`");
    assert!(!got.errored());
}
