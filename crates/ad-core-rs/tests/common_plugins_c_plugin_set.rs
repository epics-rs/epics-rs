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
