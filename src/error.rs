use std::fmt::{self, Debug};

use eventsource_stream::EventStreamError;
use http_client::{
    http::{self, HeaderValue, Response, StatusCode},
    AsyncBody,
};

/// Error raised when a [`RequestBuilder`] cannot be cloned. See [`RequestBuilder::try_clone`] for
/// more information
#[derive(Debug, Clone, Copy)]
pub struct CannotCloneRequestError;

impl fmt::Display for CannotCloneRequestError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("expected a cloneable request")
    }
}

impl std::error::Error for CannotCloneRequestError {}

#[derive(Debug, Clone)]
pub struct ResponseErrorPayload {
    pub body: String,
    pub headers: http::HeaderMap,
}

impl From<Response<AsyncBody>> for ResponseErrorPayload {
    fn from(response: Response<AsyncBody>) -> Self {
        Self {
            body: String::new(),
            headers: response.headers().clone(),
        }
    }
}

/// Error raised by the EventSource stream fetching and parsing
#[derive(Debug)]
pub enum Error {
    Io(EventStreamError<std::io::Error>),
    EventStream(EventStreamError<http::Error>),
    // The `Content-Type` returned by the server is invalid
    InvalidContentType(HeaderValue, ResponseErrorPayload),
    /// The status code returned by the server is invalid
    InvalidStatusCode(StatusCode, ResponseErrorPayload),
    /// The `Last-Event-ID` cannot be formed into a Header to be submitted to the server
    InvalidLastEventId(String),
    /// The stream ended
    StreamEnded,
}

impl From<(HeaderValue, Response<AsyncBody>)> for Error {
    fn from((header, response): (HeaderValue, Response<AsyncBody>)) -> Self {
        Self::InvalidContentType(header, response.into())
    }
}

impl From<(StatusCode, Response<AsyncBody>)> for Error {
    fn from((status, response): (StatusCode, Response<AsyncBody>)) -> Self {
        Self::InvalidStatusCode(status, response.into())
    }
}

impl From<String> for Error {
    fn from(value: String) -> Self {
        Self::InvalidLastEventId(value)
    }
}

impl From<EventStreamError<http::Error>> for Error {
    fn from(err: EventStreamError<http::Error>) -> Self {
        Self::EventStream(err)
    }
}
