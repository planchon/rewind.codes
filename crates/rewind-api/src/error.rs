//! The error type of `rewind-api`.

use crate::problem::Problem;

/// Everything that can go wrong in this crate: calling the API, or checking what it sends.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The server answered with an error (`application/problem+json`).
    #[error("{0}")]
    Problem(Box<Problem>),
    /// The request did not complete: the server could not be reached, or the connection broke.
    #[error("cannot reach the Rewind server at {url}: {message}")]
    Transport { url: String, message: String },
    /// The server answered something this client does not understand.
    #[error("unexpected response from the Rewind server: {0}")]
    Decode(String),
    /// The client cannot be built (an invalid base URL, for example).
    #[error("invalid client configuration: {0}")]
    Config(String),
    /// A value does not follow the API's rules (an idempotency key, a duration, a secret).
    #[error("invalid {what}: {message}")]
    Invalid { what: &'static str, message: String },
    /// A webhook's signature does not prove that Rewind sent it.
    #[error("webhook signature refused: {0}")]
    Signature(String),
}

impl From<Problem> for Error {
    fn from(problem: Problem) -> Self {
        Error::Problem(Box::new(problem))
    }
}

impl Error {
    pub(crate) fn invalid(what: &'static str, message: impl Into<String>) -> Self {
        Error::Invalid {
            what,
            message: message.into(),
        }
    }

    /// The problem the server answered with, if that is what failed.
    pub fn problem(&self) -> Option<&Problem> {
        match self {
            Error::Problem(problem) => Some(problem.as_ref()),
            _ => None,
        }
    }
}
