use thiserror::Error;
use tokio::task::JoinError;
use tokio::time::error::Elapsed;


// Taken from agourlay/dlm library
#[derive(Error, Debug)]
pub enum TauError {
    #[error("connection closed")]
    ConnectionClosed,
    #[error("connection timeout")]
    ConnectionTimeout,
    #[error("response body error")]
    ResponseBodyError,
    #[error("deadline elapsed timeout")]
    DeadLineElapsedTimeout,
    #[error("response status not success - {message:?}")]
    ResponseStatusNotSuccess { message: String },
    #[error("URL decode error - {message:?}")]
    UrlDecodeError { message: String },
    #[error("standard I/O error - {e}")]
    StdIoError { e: std::io::Error },
    #[error("task error - {e}")]
    TaskError { e: JoinError },
    #[error("other error - {message:?}")]
    Other { message: String },
}

const CONNECTION_CLOSED: &str = "connection closed before message completed";
const CONNECTION_TIMEOUT: &str = "error trying to connect: operation timed out";
const BODY_ERROR: &str = "error reading a body from connection";

impl std::convert::From<reqwest::Error> for TauError {
    fn from(e: reqwest::Error) -> Self {
        //TODO use Reqwest's types instead of guessing from strings https://github.com/seanmonstar/reqwest/issues/757
        let e_string = e.to_string();
        if e_string.contains(BODY_ERROR) {
            TauError::ResponseBodyError
        } else if e_string.contains(CONNECTION_CLOSED) {
            TauError::ConnectionClosed
        } else if e_string.contains(CONNECTION_TIMEOUT) {
            TauError::ConnectionTimeout
        } else {
            TauError::Other { message: e_string }
        }
    }
}

impl std::convert::From<std::io::Error> for TauError {
    fn from(e: std::io::Error) -> Self {
        TauError::StdIoError { e }
    }
}

impl std::convert::From<Elapsed> for TauError {
    fn from(_: Elapsed) -> Self {
        TauError::DeadLineElapsedTimeout
    }
}

impl std::convert::From<JoinError> for TauError {
    fn from(e: JoinError) -> Self {
        TauError::TaskError { e }
    }
}
