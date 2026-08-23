#![allow(
    clippy::approx_constant,
    clippy::collapsible_if,
    clippy::manual_is_multiple_of,
    clippy::needless_range_loop,
    clippy::new_without_default,
    clippy::too_many_arguments
)]

/// Filesystem root of the `ad-core-rs` crate, holding the `db/` templates and
/// `ioc/commonPlugins.cmd` that AD startup scripts reach through `$(ADCORE)`.
///
/// `env!` is evaluated here, inside the crate that owns the assets, so the path
/// is correct whether `ad-core-rs` is a sibling path dependency or a registry
/// checkout under a version-suffixed directory (`ad-core-rs-0.22.1`). A
/// consumer must never rebuild this from its own `CARGO_MANIFEST_DIR`.
pub const AD_CORE_DIR: &str = env!("CARGO_MANIFEST_DIR");

pub mod attributes;
pub mod codec;
pub mod color;
pub mod color_layout;
pub mod convert;
pub mod driver;
pub mod error;
pub mod finalize;
pub mod ndarray;
pub mod ndarray_handle;
pub mod ndarray_pool;
pub mod params;
pub mod pixel_cast;
pub mod plugin;
pub mod roi;
pub mod runtime;
pub mod timestamp;

#[cfg(feature = "ioc")]
pub mod ioc;
