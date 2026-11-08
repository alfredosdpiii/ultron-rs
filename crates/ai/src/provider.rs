//! The skeleton every streaming provider shares: the message being streamed and its events ([`Run`]), the ending
//! of `stream` (`done`, or the failure as an `error` event), the stop-reason check after the last event, reading a
//! response body chunk by chunk against the abort signal, and one retry loop.

use std::future::Future;
use std::time::Duration;

use crate::http::{AbortSignal, Body, Read, cancelled, is_aborted, read_chunk};
use crate::types::{AssistantMessage, AssistantMessageEvent, EventSink, StopReason};

/// The fetch `AbortError` a body read rejects with once the request is aborted.
pub(crate) const OPERATION_ABORTED: &str = "This operation was aborted";
/// What the providers throw when the signal fired before or after the stream (`throw new Error("Request was
/// aborted")`).
pub(crate) const REQUEST_WAS_ABORTED: &str = "Request was aborted";
/// The retry helpers' abort (`retryProviderRequest`, the Google and Bedrock SDK retries).
pub(crate) const REQUEST_ABORTED: &str = "Request aborted";
pub(crate) const UNKNOWN_ERROR: &str = "An unknown error occurred";
/// `AbortSignal.timeout`'s reason (a `TimeoutError` DOMException) when a request outlives its limit.
pub(crate) const OPERATION_TIMED_OUT: &str = "The operation was aborted due to timeout";

/// A provider's failure type, as the shared readers build one.
pub(crate) trait ProviderError: Sized {
    fn plain(message: String) -> Self;

    /// A body read cut off by the abort.
    fn aborted() -> Self {
        Self::plain(OPERATION_ABORTED.into())
    }
}

/// One assistant message being streamed: the message so far, where its events go, and the provider's own state.
pub(crate) struct Run<'s, 'e, S> {
    pub output: AssistantMessage,
    sink: &'s mut EventSink<'e>,
    pub state: S,
}

impl<'s, 'e, S> Run<'s, 'e, S> {
    pub fn new(output: AssistantMessage, sink: &'s mut EventSink<'e>, state: S) -> Self {
        Run { output, sink, state }
    }

    /// Send the event `event` builds from a snapshot of the message.
    pub fn emit(&mut self, event: impl FnOnce(AssistantMessage) -> AssistantMessageEvent) {
        let event = event(self.output.clone());
        (self.sink)(&event);
    }

    pub fn start(&mut self) {
        self.emit(|partial| AssistantMessageEvent::Start { partial });
    }

    /// The end of `stream`: `done` after a successful drive; else the message stops as aborted (the signal fired) or
    /// errored with `error` as its error message, and ends with an `error` event.
    pub fn end(mut self, result: Result<(), String>, signal: Option<&AbortSignal>) -> AssistantMessage {
        match result {
            Ok(()) => {
                let reason = self.output.stop_reason;
                self.emit(|message| AssistantMessageEvent::Done { reason, message });
            }
            Err(error) => {
                self.output.stop_reason = if is_aborted(signal) {
                    StopReason::Aborted
                } else {
                    StopReason::Error
                };
                self.output.error_message = Some(error);
                let reason = self.output.stop_reason;
                self.emit(|error| AssistantMessageEvent::Error { reason, error });
            }
        }
        self.output
    }

    /// The check after the last event: an abort, a stream that never set a stop reason (`pending` is the message),
    /// or an error stop reason (its error message, else a generic one) fails the stream.
    pub fn stopped(&self, signal: Option<&AbortSignal>, pending: &str) -> Result<(), String> {
        if is_aborted(signal) {
            return Err(REQUEST_WAS_ABORTED.into());
        }
        match self.output.stop_reason {
            StopReason::Pending => Err(pending.into()),
            StopReason::Aborted | StopReason::Error => Err(self
                .output
                .error_message
                .clone()
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| UNKNOWN_ERROR.into())),
            _ => Ok(()),
        }
    }
}

/// The next chunk of `body`, `None` at its end. A signal that fired before or during the read fails with
/// [`ProviderError::aborted`].
pub(crate) async fn next_chunk<E: ProviderError>(
    body: &mut dyn Body,
    signal: Option<&AbortSignal>,
) -> Result<Option<Vec<u8>>, E> {
    if is_aborted(signal) {
        return Err(E::aborted());
    }
    match read_chunk(body, signal).await {
        Read::Chunk(bytes) => Ok(Some(bytes)),
        Read::End => Ok(None),
        Read::Aborted => Err(E::aborted()),
        Read::Failed(error) => Err(E::plain(error)),
    }
}

/// One retry loop: `send` makes attempt `n` (from 0); a failure goes to `classify` with `n`, which returns the delay in
/// milliseconds before the next attempt, or the error to fail with. The wait ends with `aborted()` when `signal`
/// fires.
pub(crate) async fn retry<T, E, Fut>(
    signal: Option<&AbortSignal>,
    aborted: fn() -> E,
    mut send: impl FnMut(u32) -> Fut,
    mut classify: impl FnMut(E, u32) -> Result<f64, E>,
) -> Result<T, E>
where
    Fut: Future<Output = Result<T, E>>,
{
    let mut attempt = 0;
    loop {
        let error = match send(attempt).await {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        let delay_ms = classify(error, attempt)?;
        tokio::select! {
            biased;
            () = cancelled(signal) => return Err(aborted()),
            () = tokio::time::sleep(delay(delay_ms)) => {}
        }
        attempt += 1;
    }
}

/// A delay in milliseconds as a sleep: negative and NaN are none, one too long to represent is a day.
fn delay(ms: f64) -> Duration {
    Duration::try_from_secs_f64(ms.max(0.0) / 1000.0).unwrap_or(Duration::from_secs(86_400))
}
