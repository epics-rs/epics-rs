//! The three `EPICS_BUILD_*` environment parameters.
//!
//! C's Makefile passes these to `bldEnvData.pl` on the command line
//! (`-c $(COMPILER_CLASS)`, `-s $(OS_CLASS)`, `-t $(T_A)`), so in C they name
//! the toolchain and target that built libCom — they are not in `CONFIG_ENV`.
//! The Rust analogue names the toolchain and target that built *this* crate,
//! derived from the compile-time `cfg`s, and it lands in the same
//! [`ENV_PARAM_LIST`](super::env_table::ENV_PARAM_LIST) row `bldEnvData.pl` puts
//! it in. On `x86_64-unknown-linux-gnu` all three reproduce the C values on this
//! machine (`gcc` / `Linux` / `linux-x86_64`).

/// C `COMPILER_CLASS` — the C-ABI toolchain class of the target, which is what
/// `configure/os/CONFIG.*` names. A `*-gnu` / `*-musl` target is gcc-class, an
/// `*-msvc` target is msvc-class, and everything else (Apple, `*-none`) is
/// clang-class.
pub const COMPILER_CLASS: &str = if cfg!(target_env = "msvc") {
    "msvc"
} else if cfg!(any(target_env = "gnu", target_env = "musl")) {
    "gcc"
} else {
    "clang"
};

/// C `OS_CLASS` — the EPICS spelling of the target OS
/// (`configure/os/CONFIG.Common.<os>`).
pub const OS_CLASS: &str = if cfg!(target_os = "linux") {
    "Linux"
} else if cfg!(target_os = "macos") {
    "Darwin"
} else if cfg!(target_os = "windows") {
    "WIN32"
} else if cfg!(target_os = "freebsd") {
    "freebsd"
} else {
    "Unknown"
};

/// C `T_A` — the EPICS target-architecture name (`configure/CONFIG_SITE`'s
/// `CROSS_COMPILER_TARGET_ARCHS` vocabulary), derived from the Rust target
/// triple.
pub const TARGET_ARCH: &str = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    "linux-x86_64"
} else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
    "linux-aarch64"
} else if cfg!(all(target_os = "linux", target_arch = "arm")) {
    "linux-arm"
} else if cfg!(all(target_os = "linux", target_arch = "x86")) {
    "linux-x86"
} else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    "darwin-aarch64"
} else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
    "darwin-x86"
} else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
    "windows-x64"
} else if cfg!(all(target_os = "windows", target_arch = "x86")) {
    "win32-x86"
} else {
    "unknown"
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `EPICS_BUILD_*` row must carry a value — C warns and substitutes
    /// `""` when a parameter has none, and an empty one would print as
    /// "is undefined" where C prints the build triple.
    #[test]
    fn build_params_are_never_empty() {
        assert!(!COMPILER_CLASS.is_empty());
        assert!(!OS_CLASS.is_empty());
        assert!(!TARGET_ARCH.is_empty());
    }

    /// On the reference platform the three must be byte-identical to the C
    /// `envData.c` this port is diffed against.
    #[test]
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    fn matches_c_env_data_on_the_reference_platform() {
        assert_eq!(COMPILER_CLASS, "gcc");
        assert_eq!(OS_CLASS, "Linux");
        assert_eq!(TARGET_ARCH, "linux-x86_64");
    }
}
