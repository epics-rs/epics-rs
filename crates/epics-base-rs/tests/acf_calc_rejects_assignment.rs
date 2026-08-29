//! A7 — an ACF whose `RULE(...) { CALC(...) }` assigns into an argument is
//! rejected outright, and the IOC keeps the rule set it was running.
//!
//! C `asAsgRuleCalc` (`asLibRoutines.c:1407-1416`) checks the `stores` bitmap
//! `calcArgUsage` returns:
//!
//! ```c
//! calcArgUsage(pasgrule->rpcl, &pasgrule->inpUsed, &stores);
//! /* Until someone proves stores are not dangerous, don't allow them */
//! if (stores) { … status = S_asLib_badCalc;
//!     errlogPrintf("Assignment operator used in CALC expression '%s'\n", calc); }
//! ```
//!
//! and `asLib.y:294-299` raises that status as `yyerror("")`, which aborts the
//! parse — the whole file, not the one rule. That severity is deliberate:
//! dropping a single bad rule and loading the rest would install a policy the
//! operator did not write, and a weaker one.
//!
//! The port compiled the expression and threw the `stores` bitmap away, so a
//! storing CALC loaded and evaluated to a constant no matter what the INP
//! links read — an unconditional grant to every client in the group.
//!
//! On the literal spelling: `CALC("A:=1")` is refused by BOTH C and this port
//! before the stores check is ever reached, because `:=` pops its operand and
//! leaves the stack empty — `postfix.c:499` `runtime_depth != 1` →
//! `CALC_ERR_INCOMPLETE`. The store expressions that reach the check are the
//! ones that also leave a result, `A:=1;1` and `B:=2;A+B` among them, and
//! those are what these cases use.

use epics_base_rs::server::access_security::{AccessLevel, AcfCell, new_acf_cell, parse_acf};

/// Every storing expression the compiler accepts must be refused, whichever
/// argument it stores into and whether or not it also reads one.
#[test]
fn an_acf_rule_calc_that_stores_is_refused() {
    for expr in ["A:=1;1", "B:=2;A+B", "A:=A+1;A", "A:=1;B:=2;A+B"] {
        let acf = format!(r#"ASG(G) {{ INPA("gate") RULE(1, WRITE) {{ CALC("{expr}") }} }}"#);
        let err = parse_acf(&acf)
            .err()
            .unwrap_or_else(|| panic!("CALC(\"{expr}\") stores, so C refuses the file"));
        let msg = err.to_string();
        assert!(
            msg.contains("assignment operator"),
            "CALC(\"{expr}\"): the rejection must name the reason C names \
             (asLibRoutines.c:1423), got {msg:?}"
        );
    }
}

/// A CALC that only reads its arguments is untouched by the check.
#[test]
fn an_acf_rule_calc_that_only_reads_still_parses() {
    for expr in ["A", "A=1", "A>0&&B<10", "A?1:0"] {
        let acf =
            format!(r#"ASG(G) {{ INPA("gate") INPB("lim") RULE(1, WRITE) {{ CALC("{expr}") }} }}"#);
        parse_acf(&acf)
            .unwrap_or_else(|e| panic!("CALC(\"{expr}\") reads only; C accepts it ({e})"));
    }
}

/// The consequence the severity is about: a live cell holding a strict policy
/// must still hold it after an operator tries to load a file with a storing
/// CALC. `asInit` maps the parse error and returns before it stores, exactly
/// as C's `yyerror` aborts before `asInitialize` swaps `pasbase`.
#[test]
fn a_rejected_acf_leaves_the_running_rule_set_in_place() {
    let strict = parse_acf("ASG(DEFAULT) { RULE(1, READ) }").unwrap();
    let cell: AcfCell = new_acf_cell(Some(strict));
    assert_eq!(
        cell.load_full().unwrap().check_access("DEFAULT", "h", "u"),
        AccessLevel::Read
    );

    // The operator edits the file and reloads. The reload path is
    // `parse_acf(...)?` followed by `cell.store(...)`, so a refused parse
    // never reaches the store.
    let reload = parse_acf(r#"ASG(DEFAULT) { INPA("gate") RULE(1, WRITE) { CALC("A:=1;1") } }"#);
    assert!(reload.is_err(), "the storing CALC refuses the whole file");

    assert_eq!(
        cell.load_full().unwrap().check_access("DEFAULT", "h", "u"),
        AccessLevel::Read,
        "the previous, stricter rule set survives the refused reload — the \
         unconditional WRITE in the refused file must not become live"
    );
}
