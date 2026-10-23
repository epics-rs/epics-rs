//! Stamps the date into `SEQ_RELEASE` as the C build's seq_release.pl does
//! into seq_release.h: Perl's scalar `localtime` on the builder, taken when
//! the generator first runs and kept until it changes.
fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    let now = chrono::Local::now().format("%a %b %e %H:%M:%S %Y");
    println!("cargo::rustc-env=EPICS_SNC_COMPILED={now}");
}
