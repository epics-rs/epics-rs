//! The boundary cases of the production-slice rule, one test per boundary.
//!
//! The rule this crate replaced was positional — everything before the first
//! `\n#[cfg(test)]`. `positionally` below is that rule, kept here as the thing
//! being disproved: every test that names it fails if `production` goes back
//! to it.

use source_guard::{Comments, production_str as production};

/// The rule eleven guards carried, verbatim.
fn positionally(src: &str) -> &str {
    match src.find("\n#[cfg(test)]") {
        Some(i) => &src[..i],
        None => src,
    }
}

/// Production code written *below* a test item is still production code.
///
/// This is the defect the rule change closes, and it is not hypothetical: a
/// `#[cfg(test)] mod` landed near the top of `epics-ca-rs`'s
/// `client/transport.rs` and cut that guard's slice from 1944 code lines to
/// 277. It failed loudly only because the anchors went missing with it — the
/// same edit one item lower would have left the guard green over a seventh of
/// its subject.
#[test]
fn a_test_item_above_production_code_leaves_the_covered_set_unchanged() {
    // A file always opens with something; the positional rule looks for a
    // `\n` before the attribute, so an item at byte 0 is invisible to it too.
    const HEAD: &str = "use crate::seam;\n";
    const TEST_MOD: &str = "#[cfg(test)]\nmod t {\n    fn helper() {}\n}\n";
    const PROD: &str = "pub async fn read_loop() {\n    seam::sleep_until(d).await;\n}\n";

    let above = production(&format!("{HEAD}{TEST_MOD}{PROD}"), Comments::Keep);
    let below = production(&format!("{HEAD}{PROD}{TEST_MOD}"), Comments::Keep);

    let code = |s: &str| {
        s.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(
        code(&above),
        code(&below),
        "the covered set must not depend on where a test item sits"
    );
    assert!(above.contains("read_loop"), "{above:?}");

    // The rule being replaced: same file, same code, and the slice is empty.
    assert!(
        !positionally(&format!("{HEAD}{TEST_MOD}{PROD}")).contains("read_loop"),
        "if the positional rule can see this, the test proves nothing"
    );
}

/// A `#[cfg(test)]` free function is test code too, and a guard that stops at
/// the first one loses everything after it. `epics-ca-rs`'s transport has two,
/// one of which names a banned spelling.
#[test]
fn a_cfg_test_free_function_is_excluded_without_ending_the_slice() {
    let src = "\
fn write_loop() {}
#[cfg(test)]
async fn drain_or_event(
    n: usize,
) -> usize {
    tokio::time::timeout(n).await
}
fn read_loop() {}
";
    let prod = production(src, Comments::Keep);
    assert!(prod.contains("fn write_loop") && prod.contains("fn read_loop"));
    assert_eq!(prod.matches("tokio::time").count(), 0, "{prod}");
    assert!(
        !positionally(src).contains("fn read_loop"),
        "the positional rule stops at the helper"
    );
}

/// `#[cfg(any(target_os = "rtems", test))]` ships on RTEMS. Seven items in
/// `epics-libcom-rs`'s task seam carry it, inside the file its own thread
/// census guards; a rule that matched the substring `test` would drop them.
#[test]
fn a_predicate_is_read_not_matched() {
    let kept = [
        "#[cfg(any(target_os = \"rtems\", test))]",
        "#[cfg(not(test))]",
        "#[cfg(feature = \"latest\")]",
    ];
    for attr in kept {
        let src = format!("{attr}\nfn ships() {{\n    marker();\n}}\n");
        assert!(
            production(&src, Comments::Keep).contains("marker()"),
            "`{attr}` is production and must stay in the slice"
        );
    }
    let dropped = [
        "#[cfg(test)]",
        "#[cfg(all(test, unix, target_env = \"gnu\"))]",
        "#[cfg(all(test, tokio_backend))]",
        "#[cfg(any(test, test))]",
    ];
    for attr in dropped {
        let src = format!("{attr}\nfn only_in_tests() {{\n    marker();\n}}\n");
        assert!(
            !production(&src, Comments::Keep).contains("marker()"),
            "`{attr}` is test-only and must leave the slice"
        );
    }
}

/// Line numbers survive, because a guard that reports an offending line has to
/// name the line the reader will open. Truncation gave that for free; removing
/// an item from the middle does not, so excluded lines are blanked.
#[test]
fn excluded_lines_are_blanked_so_numbering_holds() {
    let src = "\
fn a() {}
#[cfg(test)]
mod t {
    fn helper() {}
}
fn b() {}
";
    let prod = production(src, Comments::Keep);
    assert_eq!(prod.lines().count(), src.lines().count());
    assert_eq!(
        prod.lines().nth(5),
        Some("fn b() {}"),
        "line 6 of the slice must still be line 6 of the file"
    );
    assert_eq!(prod.lines().nth(2), Some(""));
}

/// One comment rule, and it reads literals. The two rules in the tree could
/// not: a whole-line rule left `x(); // tokio::spawn` in the slice, and a
/// truncate-at-`//` rule cut `"https://…"` in half.
#[test]
fn comment_stripping_keeps_literals_and_lifetimes() {
    let src = "\
/// Doc naming tokio::spawn in prose.
fn f<'a>(s: &'a str) -> &'a str {
    let url = \"https://example.invalid/x\";
    let sep = '/';
    let raw = r\"C:\\\\a//b\";
    call(); // tokio::spawn
    /* block
       tokio::spawn */
    s
}
";
    let code = production(src, Comments::Strip);
    assert_eq!(code.matches("tokio::spawn").count(), 0, "{code}");
    assert!(code.contains("https://example.invalid/x"), "{code}");
    assert!(code.contains("r\"C:\\\\a//b\""), "{code}");
    assert!(code.contains("&'a str"), "{code}");
    assert!(code.contains("'/'"), "{code}");
    assert_eq!(
        code.lines().count(),
        src.lines().count(),
        "stripping keeps line structure"
    );
    assert!(
        production(src, Comments::Keep).contains("Doc naming"),
        "Keep leaves prose alone"
    );
}

/// An item the rule cannot close stays in the slice, so an unparsable shape
/// raises a false alarm rather than silencing the guard.
#[test]
fn an_unclosable_test_item_is_left_in() {
    let src = "#[cfg(test)]\nmod t {\n    fn never_closed() {\n";
    assert!(production(src, Comments::Keep).contains("never_closed"));
}

/// A test module nested inside another module is test code at any depth.
///
/// The column-0 spelling of the rule made this a boundary, and two guards in
/// `epics-bridge-rs`'s `realtime-pva-ioc.rs` worked around it by hand: they
/// truncated at `"\n    #[cfg(test)]"`, four spaces and all, because the
/// module they had to exclude sits inside `mod ioc`. The other five guards in
/// that same file used the column-0 needle and swept the nested test module in
/// as production.
#[test]
fn a_nested_test_module_is_excluded_at_its_own_indentation() {
    const SRC: &str = "\
pub mod ioc {
    pub fn build() {}

    #[cfg(test)]
    mod tests {
        #[test]
        fn t() {
            let _ = \"}\";
        }
    }

    pub fn teardown() {}
}
";
    let prod = production(SRC, Comments::Keep);

    assert!(prod.contains("pub fn build()"));
    assert!(
        prod.contains("pub fn teardown()"),
        "the nested item's closing brace is the one at its own indentation, \
         not the first `}}` after it: {prod}"
    );
    assert!(!prod.contains("fn t()"), "{prod}");
    // And the trailing module brace survives, so the slice still parses as the
    // file a reader opens.
    assert!(prod.trim_end().ends_with('}'));
}

/// A `#[cfg(test)]` on a field or a variant covers one line, not the rest of
/// the enclosing item.
///
/// Without this the closing-brace search would run past the member and stop at
/// the `}` that ends the struct, deleting every field below it.
#[test]
fn a_test_only_member_does_not_swallow_its_container() {
    const SRC: &str = "\
struct Port {
    #[cfg(test)]
    injected: bool,
    real: Socket,
}

enum Phase {
    #[cfg(test)]
    Rigged,
    Live,
}
";
    let prod = production(SRC, Comments::Keep);

    assert!(!prod.contains("injected"), "{prod}");
    assert!(prod.contains("real: Socket"), "{prod}");
    assert!(!prod.contains("Rigged"), "{prod}");
    assert!(prod.contains("Live"), "{prod}");
}

/// A test-only item's doc comment goes out with the item.
///
/// Under `Comments::Keep` a left-behind doc comment is production text, and
/// it is the text most likely to quote the needle its guard forbids: the four
/// test-only helpers at the foot of `epics-pva-rs`'s `server_native/tcp.rs`
/// carry eleven lines of prose naming `#[cfg(test)]` and `tokio` between them.
#[test]
fn a_test_item_takes_its_documentation_with_it() {
    const SRC: &str = "\
pub fn ship() {}

/// A fixture whose prose names tokio::spawn( on purpose.
/// Second line of the same comment.
#[allow(dead_code)]
#[cfg(test)]
fn fixture() {}

pub fn ship2() {}
";
    let prod = production(SRC, Comments::Keep);

    assert!(
        prod.contains("pub fn ship()") && prod.contains("pub fn ship2()"),
        "{prod}"
    );
    assert!(!prod.contains("tokio::spawn("), "{prod}");
    assert!(!prod.contains("Second line"), "{prod}");
    assert!(!prod.contains("allow(dead_code)"), "{prod}");
    // Still line-for-line with the file a reader opens.
    assert_eq!(prod.lines().count(), SRC.lines().count());
}

/// The back-walk stops at the preceding item, and at an inner doc comment.
///
/// `//!` documents the module it is written in, not the item below it, so it
/// is production text even when a test item follows.
#[test]
fn the_back_walk_stops_at_real_code_and_at_an_inner_doc() {
    const SRC: &str = "\
//! Module docs.
#[cfg(test)]
mod t {
    fn helper() {}
}
";
    let prod = production(SRC, Comments::Keep);
    assert!(prod.contains("//! Module docs."), "{prod}");
    assert!(!prod.contains("helper"), "{prod}");

    const TIGHT: &str = "\
pub fn ship() {}
#[cfg(test)]
fn fixture() {}
";
    let prod = production(TIGHT, Comments::Keep);
    assert!(prod.contains("pub fn ship()"), "{prod}");
    assert!(!prod.contains("fixture"), "{prod}");
}
