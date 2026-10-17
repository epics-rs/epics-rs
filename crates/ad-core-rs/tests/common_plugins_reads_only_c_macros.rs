//! `iocBoot/commonPlugins.cmd` is loaded by every areaDetector IOC, so a
//! macro it reads that C's EXAMPLE_commonPlugins.cmd does not is one the IOCs
//! never define: macLib prints "<name> is undefined" at boot.

use std::path::Path;

/// The macros C's EXAMPLE_commonPlugins.cmd reads: its header list, then the
/// module paths (`$(ADCORE)`, and `$(AUTOSAVE)` in its autosave block).
const C_MACROS: &[&str] = &[
    "PREFIX",
    "PORT",
    "QSIZE",
    "XSIZE",
    "YSIZE",
    "NCHANS",
    "CBUFFS",
    "MAX_THREADS",
    "ADCORE",
    "AUTOSAVE",
];

#[test]
fn common_plugins_reads_only_the_macros_c_does() {
    let path = Path::new(ad_core_rs::AD_CORE_DIR).join("iocBoot/commonPlugins.cmd");
    let text = std::fs::read_to_string(&path).unwrap();
    let mut extra: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .flat_map(|l| l.split("$(").skip(1))
        .map(|m| {
            m.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .next()
                .unwrap()
        })
        .filter(|m| !C_MACROS.contains(m))
        .collect();
    extra.dedup();
    assert!(extra.is_empty(), "{} reads {extra:?}", path.display());
}
