//! Error type for the git engine.
//!
//! A thin `{ message }` wrapper around failures from the in-process gix-backed
//! git layer, so callers carry a human-readable description regardless of the
//! underlying cause.

/// A git operation failed. `message` holds a human-readable description.
#[derive(Debug)]
pub struct GitError {
    pub message: String,
}

impl GitError {
    /// Build an error from an already-formatted message.
    pub fn message(message: impl Into<String>) -> Self {
        GitError {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for GitError {}
