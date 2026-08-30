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
    IncludeCycle {
        chain: Vec<String>,
    },
    IncludeDepthExceeded(usize),
    UndefinedMacro {
        key: String,
        source: String,
        line: usize,
    },
    /// A macro whose value resolves back into itself. Distinct from
    /// [`Self::UndefinedMacro`] because the two faults have different
    /// causes and different fixes: one name is missing from the
    /// substitution set, the other is defined in terms of itself.
    RecursiveMacro {
        key: String,
        source: String,
        line: usize,
    },
    /// A `$(`/`${` whose closing delimiter never arrived. Distinct from
    /// the two above because macLib names no macro for it — it copies
    /// the reference and the whole rest of the line through verbatim
    /// (`macCore.c:862-875`) — so what a `.req` author is shown is the
    /// text that was passed through, not a key to go and define.
    UnterminatedMacro {
        reference: String,
        source: String,
        line: usize,
    },
    CorruptSaveFile {
        path: String,
        message: String,
    },
    /// The set's member list came out empty. Refused rather than carried,
    /// because a set with no members still rotates and rewrites the files
    /// holding the values it was configured to protect.
    EmptySaveSet {
        name: String,
        reason: String,
    },
    PvNotFound(String),
    Ca(CaError),
}

impl fmt::Display for AutosaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::RequestFile { path, message } => {
                write!(f, "request file error in '{path}': {message}")
            }
            Self::IncludeCycle { chain } => {
                write!(f, "include cycle detected: {}", chain.join(" -> "))
            }
            Self::IncludeDepthExceeded(depth) => {
                write!(f, "include depth exceeded maximum of {depth}")
            }
            Self::UndefinedMacro { key, source, line } => {
                write!(f, "undefined macro '{key}' in {source} at line {line}")
            }
            Self::RecursiveMacro { key, source, line } => {
                write!(f, "recursive macro '{key}' in {source} at line {line}")
            }
            Self::UnterminatedMacro {
                reference,
                source,
                line,
            } => {
                write!(
                    f,
                    "unterminated macro reference '{reference}' in {source} at line {line}"
                )
            }
            Self::CorruptSaveFile { path, message } => {
                write!(f, "corrupt save file '{path}': {message}")
            }
            Self::EmptySaveSet { name, reason } => {
                write!(f, "save set '{name}' has no PVs to save: {reason}")
            }
            Self::PvNotFound(name) => write!(f, "PV not found: {name}"),
            Self::Ca(e) => write!(f, "CA error: {e}"),
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
