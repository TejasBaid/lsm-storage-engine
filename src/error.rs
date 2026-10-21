use std::io;
use std::sync::Arc;

/// Errors returned by the storage engine.
///
/// `Error` is cheaply cloneable so that a sticky background error can be
/// reported to every subsequent caller.
#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    /// An operating-system level I/O failure.
    #[error("I/O error: {0}")]
    Io(#[source] Arc<io::Error>),

    /// On-disk data failed validation (bad checksum, truncated structure,
    /// impossible field value, ...).
    #[error("corruption: {0}")]
    Corruption(String),

    /// The caller supplied an invalid argument or option.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// The database is shutting down and the operation was abandoned.
    #[error("database is shutting down")]
    ShuttingDown,
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(Arc::new(e))
    }
}

impl Error {
    pub(crate) fn corruption(msg: impl Into<String>) -> Self {
        Error::Corruption(msg.into())
    }

    /// Returns true if this error indicates on-disk corruption.
    pub fn is_corruption(&self) -> bool {
        matches!(self, Error::Corruption(_))
    }
}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;
