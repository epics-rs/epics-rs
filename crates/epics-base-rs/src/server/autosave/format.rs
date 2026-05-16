pub const VERSION: &str = "autosave-rs V1.0";
pub const END_MARKER: &str = "<END>";
pub const ARRAY_MARKER: &str = "@array@";
pub const SAV_EXT: &str = "sav";
pub const SAVB_EXT: &str = "savB";
pub const MAX_INCLUDE_DEPTH: usize = 10;

/// Save-file format mode.
///
/// Selects the on-disk `.sav` encoding written by autosave. Reading
/// is mode-agnostic — the reader accepts both the native `[v,v,v]`
/// array form and the C `@array@ { ... }` form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompatMode {
    /// autosave-rs native format: `save/restore`-style header with
    /// the autosave-rs banner, arrays as `[v,v,v]`, strings quoted.
    #[default]
    Native,
    /// C-autosave wire-compatible format: a C IOC (`restore.c`,
    /// `asVerify`) can read the produced `.sav` file. Header carries
    /// the `save/restore` banner, scalars are plain printf form,
    /// arrays use `@array@ { "v" "v" ... }`.
    CRead,
}
