//! Errors used across the turnip core library.

/// A recoverable, user-presentable error. Library APIs return this instead of
/// panicking or leaking transport details; messages are safe to print verbatim.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// A user-actionable message, already fit for display.
    #[error("{0}")]
    Message(String),
    /// The operation was cancelled through an [`crate::provider::AbortSignal`].
    #[error("aborted")]
    Aborted,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl CoreError {
    pub fn message(msg: impl Into<String>) -> Self {
        CoreError::Message(msg.into())
    }
}

pub type Result<T> = std::result::Result<T, CoreError>;
