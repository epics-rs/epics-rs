pub const VERSION: &str = "autosave-rs V1.0";
pub const END_MARKER: &str = "<END>";
pub const ARRAY_MARKER: &str = "@array@";
pub const SAV_EXT: &str = "sav";
pub const SAVB_EXT: &str = "savB";
pub const MAX_INCLUDE_DEPTH: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompatMode {
    /// autosave-rs native format
    Native,
    /// C autosave file reading support (Level B)
    CRead,
}
