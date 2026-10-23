//! NDCodec_settings.req saves the codec settings C's does, including the
//! zlib and LZ4-HDF5 ones ADCore added with those codecs (ADCore ec0069d7).
#![cfg(feature = "ioc")]

use std::path::Path;

use epics_base_rs::server::autosave::macros::MacroContext;
use epics_base_rs::server::autosave::request::load_request_file_with_search_paths;

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — ran and passed on the
// exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p
// ad-core-rs --all-features`); it builds no CA server, and its reactor is
// `#[tokio::test]`'s own, which the backend does not remove.
#[tokio::test]
async fn nd_codec_settings_saves_c_set() {
    let paths = [Path::new(ad_core_rs::AD_CORE_DIR).join("db")];
    let macros = MacroContext::from_map(
        [("P", "13SIM1:"), ("R", "Codec1:")]
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .into(),
    );
    let list = load_request_file_with_search_paths("NDCodec_settings.req", &paths, &macros)
        .await
        .unwrap();
    let own: Vec<&str> = list
        .entries
        .iter()
        .filter_map(|e| e.pv_name.strip_prefix("13SIM1:Codec1:"))
        .collect();
    let c = [
        "Mode",
        "Compressor",
        "JPEGQuality",
        "ZlibCLevel",
        "BloscCompressor",
        "BloscCLevel",
        "BloscShuffle",
        "BloscNumThreads",
        "LZ4HDF5BlockSize",
    ];
    // Then NDPluginBase_settings.req's, from NDArrayPort on.
    assert_eq!(own[..=c.len()], [&c[..], &["NDArrayPort"]].concat());
}
