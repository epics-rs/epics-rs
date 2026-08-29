//! `AdIoc::new()` publishes the search paths an st.cmd resolves `$(ADCORE)/...`
//! against. Every one of them must name a directory that actually exists.
// `ad_plugins_rs::ioc` mounts on the tokio-backend-only IOC runners, so
// this file is gated with the module it exercises.
#![cfg(tokio_backend)]
#![cfg(feature = "ioc")]

use std::collections::HashMap;
use std::path::Path;

use ad_plugins_rs::ioc::AdIoc;

/// Each environment variable `AdIoc::new()` introduces is a directory an IOC
/// reads assets out of, so each must resolve — under a sibling path-dependency
/// checkout and under a version-suffixed registry checkout alike.
///
/// Unfixed, `AdIoc::new()` built four paths out of *its own* `CARGO_MANIFEST_DIR`:
/// `../ad-core-rs`, `../calc`, `../busy` and `../autosave`. The last three name
/// crates this workspace does not contain in either mode, which is what fails
/// here. `../ad-core-rs` happens to resolve in a sibling checkout but not in the
/// registry, where the directory is `ad-core-rs-0.22.1`; the `AD_CORE_DIR`
/// assertion below pins it to the one dir that is right in both.
#[test]
fn ad_ioc_publishes_only_paths_that_exist() {
    for var in ["ADCORE", "CALC", "BUSY", "AUTOSAVE"] {
        // SAFETY: nextest gives each test its own process, and this runs before
        // the IOC spawns any thread.
        unsafe { std::env::remove_var(var) };
    }

    let before: HashMap<String, String> = std::env::vars().collect();
    let _ioc = AdIoc::new();
    let published: Vec<(String, String)> = std::env::vars()
        .filter(|(name, _)| !before.contains_key(name))
        .collect();

    for (name, value) in &published {
        assert!(
            Path::new(value).is_dir(),
            "AdIoc::new() set {name}={value}, which is not an existing directory"
        );
    }

    let adcore = published
        .iter()
        .find(|(name, _)| name == "ADCORE")
        .map(|(_, value)| value.as_str())
        .expect("AdIoc::new() must publish ADCORE for `< $(ADCORE)/ioc/commonPlugins.cmd`");

    assert_eq!(
        adcore,
        ad_core_rs::AD_CORE_DIR,
        "ADCORE must be the directory ad-core-rs reports for itself, not a path \
         reconstructed from ad-plugins-rs's manifest dir"
    );
    assert!(
        Path::new(adcore)
            .join("db")
            .join("ADBase.template")
            .is_file(),
        "every AD db template does `include \"ADBase.template\"` from $(ADCORE)/db"
    );
    assert!(
        Path::new(adcore)
            .join("ioc")
            .join("commonPlugins.cmd")
            .is_file(),
        "st.cmd aborts before iocInit() if `< $(ADCORE)/ioc/commonPlugins.cmd` is missing"
    );
}
