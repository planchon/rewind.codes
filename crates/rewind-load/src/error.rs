//! What stops the load from running at all. What goes wrong during the load (a refused start,
//! a run that ends otherwise than expected) is counted in the report instead.

/// A setup that failed: the server cannot be reached, a key or the action set cannot be made.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct Error(String);

impl Error {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl From<rewind_api::Error> for Error {
    fn from(error: rewind_api::Error) -> Self {
        Self(error.to_string())
    }
}
