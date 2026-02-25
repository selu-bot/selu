use thiserror::Error;

#[derive(Debug, Error)]
pub enum PapError {
    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Unauthorized")]
    Unauthorized,

    #[error("Forbidden: {0}")]
    Forbidden(String),

    #[error("Invalid input: {0}")]
    InvalidInput(String),

    #[error("Database error: {0}")]
    Database(String),

    #[error("Capability error: {0}")]
    Capability(String),

    #[error("LLM error: {0}")]
    Llm(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

pub type PapResult<T> = Result<T, PapError>;
