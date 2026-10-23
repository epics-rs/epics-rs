use std::fmt;

use crate::error::CaError;

/// Result type for autosave operations.
pub type AutosaveResult<T> = Result<T, AutosaveError>;

/// Errors that can occur during autosave operations.
#[derive(Debug)]
pub enum AutosaveError {
    Io(std::io::Error),
    RequestFile {
        path: String,
        message: String,
    },
    /// C `readReqFile` could not open the top-level request file, and has
    /// said so on the console.
    ReqFileUnopened(String),
    IncludeCycle {
        chain: Vec<String>,
    },
    IncludeDepthExceeded(usize),
    CorruptSaveFile {
        path: String,
        message: String,
    },
    PvNotFound(String),
    Ca(CaError),
    /// C `create_data_set` refusing a method the request file's list
    /// already has: `bit` is that method's `save_method` bit.
    AlreadyInMode {
        request_file: String,
        bit: u32,
    },
}

impl fmt::Display for AutosaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::RequestFile { path, message } => {
                write!(f, "request file error in '{path}': {message}")
            }
            Self::ReqFileUnopened(file) => write!(f, "{file}: Can't open .req file"),
            Self::IncludeCycle { chain } => {
                write!(f, "include cycle detected: {}", chain.join(" -> "))
            }
            Self::IncludeDepthExceeded(depth) => {
                write!(f, "include depth exceeded maximum of {depth}")
            }
            Self::CorruptSaveFile { path, message } => {
                write!(f, "corrupt save file '{path}': {message}")
            }
            Self::PvNotFound(name) => write!(f, "PV not found: {name}"),
            Self::Ca(e) => write!(f, "CA error: {e}"),
            // ERRLOG's "%s:%s " prefix with C's __FILE__ (save_restore.c:2373).
            Self::AlreadyInMode { request_file, bit } => write!(
                f,
                "../save_restore.c:create_data_set '{request_file}' already in {bit:x} mode"
            ),
        }
    }
}

impl std::error::Error for AutosaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Ca(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for AutosaveError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<CaError> for AutosaveError {
    fn from(e: CaError) -> Self {
        Self::Ca(e)
    }
}
