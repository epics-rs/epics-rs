//! Stamps the date into `SEQ_RELEASE` as the C build's seq_release.pl does
//! into seq_release.h: Perl's scalar `localtime` on the builder, taken when
//! the generator first runs and kept until it changes. Writes that header
//! into `$OUT_DIR/include`, the `$(SNCSEQ)/include` a program's
//! `#include <seq_release.h>` reaches for `MAGIC`.

/// C `MAGIC`: the table layout snc-rs generates and epics-seq-rs checks,
/// 2.2.9's, where seq_release.pl spells the release.
const MAGIC: u32 = 2_002_009;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    let now = chrono::Local::now().format("%a %b %e %H:%M:%S %Y");
    println!("cargo::rustc-env=EPICS_SNC_COMPILED={now}");
    println!("cargo::rustc-env=EPICS_SNC_MAGIC={MAGIC}");
    let version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let include =
        std::path::Path::new(&std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("include");
    std::fs::create_dir_all(&include).expect("create include dir");
    std::fs::write(
        include.join("seq_release.h"),
        format!(
            "#define SEQ_RELEASE \"Sequencer release {version}, compiled {now}\"\n\
             #define MAGIC {MAGIC}\n"
        ),
    )
    .expect("write seq_release.h");
}
