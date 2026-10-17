use asyn_rs::error::AsynError;

/// Error type for areaDetector operations.
#[derive(Debug, thiserror::Error)]
pub enum ADError {
    #[error("invalid dimensions: {0}")]
    InvalidDimensions(String),

    #[error("buffer size mismatch: expected {expected}, actual {actual}")]
    BufferSizeMismatch { expected: usize, actual: usize },

    #[error("data type mismatch: expected {expected:?}, actual {actual:?}")]
    DataTypeMismatch {
        expected: super::ndarray::NDDataType,
        actual: super::ndarray::NDDataType,
    },

    #[error("pool exhausted: {0} bytes requested, {1} bytes limit")]
    PoolExhausted(usize, usize),

    #[error("unsupported conversion: {0}")]
    UnsupportedConversion(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("asyn error: {0}")]
    Asyn(#[from] AsynError),

    /// A failure C reports itself: `lines` are what it prints, in order, and
    /// `message` is what it leaves in `NDFileWriteMessage`.
    #[error("{message}")]
    Reported { lines: Vec<String>, message: String },

    /// A write after which the writer closed its own file and C drops the
    /// capture with no `closeFileBase` (NDFileHDF5.cpp:1519-1557).
    #[error("{0}")]
    Aborted(Box<ADError>),
}

impl ADError {
    /// The console lines C prints for this failure; none unless `Reported`.
    pub fn lines(&self) -> &[String] {
        match self {
            Self::Reported { lines, .. } => lines,
            Self::Aborted(e) => e.lines(),
            _ => &[],
        }
    }

    pub fn is_aborted(&self) -> bool {
        matches!(self, Self::Aborted(_))
    }

    /// The same failure with its lines dropped, once they have been printed.
    pub fn without_lines(self) -> Self {
        match self {
            Self::Reported { message, .. } => Self::Reported {
                lines: Vec::new(),
                message,
            },
            Self::Aborted(e) => Self::Aborted(Box::new(e.without_lines())),
            e => e,
        }
    }
}

pub type ADResult<T> = Result<T, ADError>;
