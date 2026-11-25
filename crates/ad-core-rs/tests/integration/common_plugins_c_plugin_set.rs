//! commonPlugins.cmd loads the plugins C's EXAMPLE_commonPlugins.cmd does: no
//! NDFileMagick (commented out, :37-38) and one ROIStat port with 8 ROIs
//! (:50-60).

use std::path::Path;

fn loads() -> Vec<String> {
    let path = Path::new(ad_core_rs::AD_CORE_DIR).join("iocBoot/commonPlugins.cmd");
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .map(String::from)
        .collect()
}

#[test]
fn common_plugins_loads_no_magick_plugin() {
    let magick: Vec<String> = loads()
        .into_iter()
        .filter(|l| l.contains("Magick"))
        .collect();
    assert!(magick.is_empty(), "C comments these out: {magick:?}");
}

#[test]
fn common_plugins_roi_stat_has_c_8_rois() {
    let lines = loads();
    let configure: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with("NDROIStatConfigure("))
        .collect();
    assert_eq!(configure.len(), 1);
    let max_rois = configure[0].split(',').nth(5).unwrap().trim();
    assert_eq!(max_rois, "8", "{}", configure[0]);
    let n = lines
        .iter()
        .filter(|l| l.contains("\"NDROIStatN.template\""))
        .count();
    assert_eq!(n, 8);
}

/// The `maxThreads` C's EXAMPLE_commonPlugins.cmd passes each port; leaving it
/// off runs the port with MaxThreads_RBV 1 where C has 5.
#[test]
fn common_plugins_passes_c_max_threads() {
    const C: &[(&str, &str)] = &[
        ("ROI1", "$(MAX_THREADS=5)"),
        ("ROI2", "$(MAX_THREADS=5)"),
        ("ROI3", "$(MAX_THREADS=5)"),
        ("ROI4", "$(MAX_THREADS=5)"),
        ("ROISTAT1", "$(MAX_THREADS=5)"),
        ("STATS1", "$(MAX_THREADS=5)"),
        ("STATS2", "$(MAX_THREADS=5)"),
        ("STATS3", "$(MAX_THREADS=5)"),
        ("STATS4", "$(MAX_THREADS=5)"),
        ("STATS5", "$(MAX_THREADS=5)"),
        ("TRANS1", "$(MAX_THREADS=5)"),
        ("OVER1", "$(MAX_THREADS=5)"),
        ("CC1", "$(MAX_THREADS=5)"),
        ("CC2", "$(MAX_THREADS=5)"),
        ("FFT1", "5"),
        ("CODEC1", "5"),
        ("CODEC2", "5"),
        ("BADPIX1", "5"),
    ];
    let lines = loads();
    for (port, max_threads) in C {
        let line = lines
            .iter()
            .find(|l| l.contains("Configure(") && l.contains(&format!("(\"{port}\",")))
            .unwrap_or_else(|| panic!("no configure for {port}"));
        // ROIStat and Overlay take maxThreads one later, after their count.
        let idx =
            if line.starts_with("NDROIStatConfigure(") || line.starts_with("NDOverlayConfigure(") {
                10
            } else {
                9
            };
        let args: Vec<&str> = line
            .strip_suffix(')')
            .unwrap()
            .split(',')
            .map(str::trim)
            .collect();
        assert_eq!(args.get(idx).copied(), Some(*max_threads), "{line}");
    }
}
