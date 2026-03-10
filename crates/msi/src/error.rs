use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum MsiError {
    #[error("I/O error for '{path}': {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("substitution parse error at {file}:{line}: {message}")]
    SubstParse {
        file: String,
        line: usize,
        message: String,
    },

    #[error("include depth limit ({max_depth}) exceeded at '{path}'")]
    IncludeDepth { path: PathBuf, max_depth: usize },

    #[error("include file not found: '{name}' (searched: {searched:?})")]
    IncludeNotFound {
        name: String,
        searched: Vec<PathBuf>,
    },

    #[error("invalid directive at {file}:{line}: {message}")]
    InvalidDirective {
        file: String,
        line: usize,
        message: String,
    },
}
