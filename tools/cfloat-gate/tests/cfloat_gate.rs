//! The live gate over this workspace; see the crate docs for the rule.

/// The shipped crates and the examples.
const GROUPS: &[&str] = &["crates", "examples"];

/// `(file, placeholder, argument, reason)`: sites whose argument is not a
/// float. Matched exactly; a stale entry fails the gate.
const EXEMPT: &[(&str, &str, &str, &str)] = &[
    (
        "crates/epics-base-rs/src/server/database/breakpoint.rs",
        "{:<28.28}",
        "rec",
        "C %-28.28s of a record name",
    ),
    (
        "crates/epics-base-rs/src/server/database/breakpoint.rs",
        "{:<28.28}",
        "name",
        "C %-28.28s of a record name",
    ),
    (
        "crates/epics-base-rs/src/server/iocsh/commands.rs",
        "{field:>4.4}",
        "field",
        "C %4.4s of a field name",
    ),
    (
        "crates/epics-base-rs/src/server/recgbl.rs",
        "{ft_name:.12}",
        "ft_name",
        "C %.12s of a DBF_ name",
    ),
    (
        "crates/epics-libcom-rs/src/runtime/task.rs",
        "{:>16.16}",
        "self.name",
        "C %16.16s of a thread name",
    ),
    (
        "crates/epics-libcom-rs/src/runtime/task.rs",
        "{:>8.8}",
        "if self.is_suspended() { \"SUSPEND\" } else { \"OK\" }",
        "C %8.8s of the thread state",
    ),
    (
        "crates/epics-libcom-rs/src/cfmt.rs",
        "{a:.p$}",
        "a",
        "CFloat's own digits, of a finite magnitude",
    ),
    (
        "crates/epics-libcom-rs/src/cfmt.rs",
        "{a:.p$e}",
        "a",
        "CFloat's own digits, of a finite magnitude",
    ),
    (
        "crates/epics-libcom-rs/src/cfmt.rs",
        "{a:.prec$e}",
        "a",
        "CFloat's own digits, of a finite magnitude",
    ),
];

#[test]
fn every_float_placeholder_formats_a_cfloat() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root");
    cfloat_gate::check(root, &cfloat_gate::crates_in(root, GROUPS), EXEMPT);
}
