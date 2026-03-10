use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub offset: usize,
    pub line: usize,
    pub column: usize,
}

impl std::fmt::Display for Span {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}, column {}", self.line, self.column)
    }
}

#[derive(Debug, Error)]
pub enum CompileError {
    #[error("{span}: {message}")]
    Syntax { span: Span, message: String },

    #[error("{span}: {message}")]
    Semantic { span: Span, message: String },

    #[error("{0}")]
    Other(String),
}

impl CompileError {
    pub fn syntax(span: Span, message: impl Into<String>) -> Self {
        Self::Syntax {
            span,
            message: message.into(),
        }
    }

    pub fn semantic(span: Span, message: impl Into<String>) -> Self {
        Self::Semantic {
            span,
            message: message.into(),
        }
    }
}

pub type CompileResult<T> = Result<T, CompileError>;
