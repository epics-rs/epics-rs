pub mod data;
pub mod db_access;
pub mod drivers;
pub mod math;
pub mod records;
pub mod seq_runner;
pub mod snl;

/// Path to the bundled database template directory.
pub const OPTICS_DB_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/db");
