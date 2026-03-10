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
}

pub type ADResult<T> = Result<T, ADError>;
