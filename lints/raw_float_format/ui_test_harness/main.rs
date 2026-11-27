// Built with `--test`: no float here is flagged, and the allow entry for `x`
// is not reported unused.
fn main() {
    let x = 1.5_f64;
    let _ = format!("{x}");
}

#[test]
fn assertion_text() {
    assert_eq!(format!("{}", 0.5_f64), "0.5");
}
