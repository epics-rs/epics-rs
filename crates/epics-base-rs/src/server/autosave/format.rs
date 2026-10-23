pub const END_MARKER: &str = "<END>";
pub const ARRAY_MARKER: &str = "@array@";
pub const SAV_EXT: &str = "sav";
pub const SAVB_EXT: &str = "savB";
pub const MAX_INCLUDE_DEPTH: usize = 10;
/// `save_restore.h` `BUF_SIZE`: C's line buffer for reading request and
/// save files, which also bounds a long string's value.
pub const BUF_SIZE: usize = 200;
