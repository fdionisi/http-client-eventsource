pub mod error;
pub mod retry_policy;

use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use anyhow::Result;
use eventsource_stream::EventStreamError;
pub use eventsource_stream::{Event as MessageEvent, EventStream};
use futures::Stream;
use futures_timer::Delay;
use http_client::{
    http::{self, header, HeaderName, HeaderValue, Request, Response, StatusCode},
    AsyncBody, HttpClient,
};
use pin_project_lite::pin_project;
use retry_policy::{RetryPolicy, DEFAULT_RETRY};

/// Events created by the [`EventSource`]
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Event {
    /// The event fired when the connection is opened
    Open,
    /// The event fired when a [`MessageEvent`] is received
    Message(MessageEvent),
}

impl From<MessageEvent> for Event {
    fn from(event: MessageEvent) -> Self {
        Event::Message(event)
    }
}
type BoxedRetry = Box<dyn RetryPolicy + Send + Unpin + 'static>;

type ResponseFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Response<AsyncBody>, http::Error>> + Send + 'a>>;
type MessageEventStream = Pin<
    Box<dyn Stream<Item = Result<MessageEvent, EventStreamError<std::io::Error>>> + Send + Sync>,
>;

/// Based on the implementation in
/// https://github.com/jpopesculian/eventsource-stream/tree/3d46f1c758f9ee4681e9da0427556d24c53f9c01
/// https://github.com/jpopesculian/reqwest-eventsource/tree/ce0b972450fd16a417e2cfbe1cc522feef26545d
pub trait EventSource {
    fn event_source(&self, request: Request<AsyncBody>) -> Result<EventSourceResponse>;
}

impl EventSource for Arc<dyn HttpClient> {
    fn event_source(&self, mut request: Request<AsyncBody>) -> Result<EventSourceResponse> {
        let headers = request.headers_mut();
        headers.append("Accept", "text/event-stream".parse()?);

        let request_clone = request.clone();

        let response = { self.send(request) };

        Ok(EventSourceResponse {
            http_client: self.clone(),
            request: request_clone,
            response: Some(response),
            stream: None,
            is_closed: false,
            delay: None,
            retry_policy: Box::new(DEFAULT_RETRY),
            last_event_id: String::new(),
            last_retry: None,
        })
    }
}

/// The ready state of an [`EventSource`]
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
#[repr(u8)]
pub enum ReadyState {
    /// The EventSource is waiting on a response from the endpoint
    Connecting = 0,
    /// The EventSource is connected
    Open = 1,
    /// The EventSource is closed and no longer emitting Events
    Closed = 2,
}

pin_project! {
    #[project = EventSourceProjection]
   pub struct EventSourceResponse<'a> {
        http_client: Arc<dyn HttpClient>,
        request: Request<AsyncBody>,
        #[pin]
        response: Option<ResponseFuture<'a>>,
        #[pin]
        stream: Option<MessageEventStream>,
        #[pin]
        delay: Option<Delay>,
        is_closed: bool,
        retry_policy: BoxedRetry,
        last_event_id: String,
        last_retry: Option<(usize, Duration)>
    }
}

impl<'a> EventSourceResponse<'a> {
    /// Close the EventSource stream and stop trying to reconnect
    pub fn close(&mut self) {
        self.is_closed = true;
    }

    /// Set the retry policy
    pub fn set_retry_policy(&mut self, policy: BoxedRetry) {
        self.retry_policy = policy
    }

    /// Get the last event id
    pub fn last_event_id(&self) -> &str {
        &self.last_event_id
    }

    /// Get the current ready state
    pub fn ready_state(&self) -> ReadyState {
        if self.is_closed {
            ReadyState::Closed
        } else if self.delay.is_some() || self.response.is_some() {
            ReadyState::Connecting
        } else {
            ReadyState::Open
        }
    }
}

impl<'a, 'b> EventSourceProjection<'a, 'b> {
    fn clear_fetch(&mut self) {
        self.response.take();
        self.stream.take();
    }

    fn retry_fetch(&mut self) -> Result<(), error::Error> {
        self.stream.take();
        let mut req = self.request.clone();
        let headers = req.headers_mut();

        headers.append(
            HeaderName::from_static("last-event-id"),
            HeaderValue::from_str(&self.last_event_id)
                .map_err(|_| error::Error::InvalidLastEventId(self.last_event_id.clone()))?,
        );
        let res_future = Box::pin(self.http_client.send(req));
        self.response.replace(res_future);
        Ok(())
    }

    fn handle_response(&mut self, res: Response<AsyncBody>) {
        self.last_retry.take();
        let mut stream = EventStream::new(res.into_body());
        stream.set_last_event_id(self.last_event_id.clone());
        self.stream.replace(Box::pin(stream));
    }

    fn handle_event(&mut self, event: &MessageEvent) {
        *self.last_event_id = event.id.clone();
        if let Some(duration) = event.retry {
            self.retry_policy.set_reconnection_time(duration)
        }
    }

    fn handle_error(&mut self, error: &error::Error) {
        self.clear_fetch();
        if let Some(retry_delay) = self.retry_policy.retry(error, *self.last_retry) {
            let retry_num = self.last_retry.map(|retry| retry.0).unwrap_or(1);
            *self.last_retry = Some((retry_num, retry_delay));
            self.delay.replace(Delay::new(retry_delay));
        } else {
            *self.is_closed = true;
        }
    }
}

impl<'a> std::fmt::Debug for EventSourceResponse<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventSourceResponse")
            .field("is_closed", &self.is_closed)
            .finish()
    }
}

fn check_response(response: Response<AsyncBody>) -> Result<Response<AsyncBody>, error::Error> {
    match response.status() {
        StatusCode::OK => {}
        status => {
            return Err((status, response).into());
        }
    }
    let content_type = if let Some(content_type) = response.headers().get(&header::CONTENT_TYPE) {
        content_type
    } else {
        return Err((HeaderValue::from_static(""), response).into());
    };
    if content_type
        .to_str()
        .map_err(|_| ())
        .and_then(|s| s.parse::<mime::Mime>().map_err(|_| ()))
        .map(|mime_type| {
            matches!(
                (mime_type.type_(), mime_type.subtype()),
                (mime::TEXT, mime::EVENT_STREAM)
            )
        })
        .unwrap_or(false)
    {
        Ok(response)
    } else {
        Err((content_type.clone(), response).into())
    }
}

impl<'a> Stream for EventSourceResponse<'a> {
    type Item = Result<Event, error::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        if *this.is_closed {
            return Poll::Ready(None);
        }

        if let Some(delay) = this.delay.as_mut().as_pin_mut() {
            match delay.poll(cx) {
                Poll::Ready(_) => {
                    this.delay.take();
                    if let Err(err) = this.retry_fetch() {
                        *this.is_closed = true;
                        return Poll::Ready(Some(Err(err)));
                    }
                }
                Poll::Pending => {
                    return Poll::Pending;
                }
            }
        }

        if let Some(response_future) = this.response.as_mut().as_pin_mut() {
            match response_future.poll(cx) {
                Poll::Ready(Ok(res)) => {
                    this.clear_fetch();
                    match check_response(res) {
                        Ok(res) => {
                            this.handle_response(res);
                            return Poll::Ready(Some(Ok(Event::Open)));
                        }
                        Err(err) => {
                            *this.is_closed = true;
                            return Poll::Ready(Some(Err(err)));
                        }
                    }
                }
                Poll::Ready(Err(err)) => {
                    let err = error::Error::EventStream(EventStreamError::Transport(err));
                    this.handle_error(&err);
                    return Poll::Ready(Some(Err(err)));
                }
                Poll::Pending => {
                    return Poll::Pending;
                }
            }
        }

        match this
            .stream
            .as_mut()
            .as_pin_mut()
            .unwrap()
            .as_mut()
            .poll_next(cx)
        {
            Poll::Ready(Some(Err(err))) => {
                let err = error::Error::Io(err);
                this.handle_error(&err);
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(Some(Ok(event))) => {
                this.handle_event(&event);
                Poll::Ready(Some(Ok(event.into())))
            }
            Poll::Ready(None) => {
                let err = error::Error::StreamEnded;
                this.handle_error(&err);
                Poll::Ready(Some(Err(err)))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
