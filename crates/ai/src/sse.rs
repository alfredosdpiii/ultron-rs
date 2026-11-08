//! Server-sent event decoding shared by the Anthropic client (`iterateSseMessages`) and the OpenAI SDK
//! (`_iterSSEMessages`): lines end at LF, CRLF or CR; `event:`/`data:` fields; a blank line ends an event. Also the
//! OpenAI SDK's stream of JSON events the Chat Completions and Responses providers read.

use crate::http::{AbortSignal, Failure, HttpResponse, Read, Sdk, is_aborted, read_chunk};
use jsrt::json_parse::parse_json;
use serde_json::{Value, json};

/// The earliest frame boundary in `buffer` (its offset and length): at each offset, the first of `separators` that
/// starts there. The providers that split frames themselves differ only in which blank lines end one.
pub(crate) fn frame_boundary(buffer: &[u8], separators: &[&str]) -> Option<(usize, usize)> {
    (0..buffer.len()).find_map(|at| {
        separators
            .iter()
            .find(|sep| buffer[at..].starts_with(sep.as_bytes()))
            .map(|sep| (at, sep.len()))
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct ServerSentEvent {
    pub event: Option<String>,
    pub data: String,
    /// Every non-blank line of the event, comments included.
    pub raw: Vec<String>,
}

#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
    raw: Vec<String>,
}

impl SseDecoder {
    /// Decode the complete lines in `chunk`. A CR at the end of the buffered bytes waits for the next chunk, so a CRLF
    /// split across chunks stays one line break (upstream's Anthropic decoder reads it as two).
    pub fn push(&mut self, chunk: &[u8]) -> Vec<ServerSentEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        let mut start = 0;
        let mut index = 0;
        while index < self.buffer.len() {
            let byte = self.buffer[index];
            if byte == b'\n' || byte == b'\r' {
                if byte == b'\r' && index + 1 == self.buffer.len() {
                    break;
                }
                let line = String::from_utf8_lossy(&self.buffer[start..index]).into_owned();
                let next = if byte == b'\r' && self.buffer[index + 1] == b'\n' {
                    index + 2
                } else {
                    index + 1
                };
                events.extend(self.line(line));
                start = next;
                index = next;
            } else {
                index += 1;
            }
        }
        self.buffer.drain(..start);
        events
    }

    /// End of body: the unterminated last line is decoded; `flush_trailing` also emits an event that no blank line
    /// closed (the Anthropic decoder does, the OpenAI SDK does not).
    pub fn finish(&mut self, flush_trailing: bool) -> Vec<ServerSentEvent> {
        let mut events = Vec::new();
        if !self.buffer.is_empty() {
            let rest = std::mem::take(&mut self.buffer);
            let text = String::from_utf8_lossy(&rest);
            let line = text.strip_suffix('\r').unwrap_or(&text).to_string();
            events.extend(self.line(line));
        }
        if flush_trailing {
            events.extend(self.flush());
        }
        events
    }

    fn flush(&mut self) -> Option<ServerSentEvent> {
        if self.event.is_none() && self.data.is_empty() {
            return None;
        }
        Some(ServerSentEvent {
            event: self.event.take(),
            data: std::mem::take(&mut self.data).join("\n"),
            raw: std::mem::take(&mut self.raw),
        })
    }

    fn line(&mut self, line: String) -> Option<ServerSentEvent> {
        if line.is_empty() {
            return self.flush();
        }
        self.raw.push(line.clone());
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.find(':') {
            Some(i) => (&line[..i], &line[i + 1..]),
            None => (line.as_str(), ""),
        };
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
        None
    }
}

/// The OpenAI SDK stream: JSON per `data:` event until `[DONE]`; a body with `error` throws an API error. An abort ends
/// the stream quietly, as the SDK does.
pub(crate) async fn read_openai_stream(
    response: &mut HttpResponse,
    signal: Option<&AbortSignal>,
    mut handle: impl FnMut(&Value) -> Result<(), Failure>,
) -> Result<(), Failure> {
    let headers = response.headers.clone();
    let mut decoder = SseDecoder::default();
    let mut done = false;
    let mut process = |events: Vec<ServerSentEvent>, done: &mut bool| -> Result<(), Failure> {
        for sse in events {
            if *done {
                continue;
            }
            if sse.data.starts_with("[DONE]") {
                *done = true;
                continue;
            }
            if sse.event.as_deref().is_some_and(|e| e.starts_with("thread.")) {
                continue;
            }
            let data = parse_json(&sse.data).map_err(|e| Failure::plain(e.to_string()))?;
            if let Some(error) = data.get("error").filter(|e| jsrt::value::truthy(Some(e))) {
                return Err(Failure::api(
                    Sdk::OpenAi,
                    None,
                    Some(headers.clone()),
                    Some(json!({ "error": error })),
                    None,
                ));
            }
            handle(&data)?;
        }
        Ok(())
    };
    loop {
        match read_chunk(response.body.as_mut(), signal).await {
            Read::Chunk(bytes) => {
                process(decoder.push(&bytes), &mut done)?;
                if is_aborted(signal) {
                    return Ok(());
                }
            }
            Read::End => break,
            Read::Aborted => return Ok(()),
            Read::Failed(error) => return Err(Failure::plain(error)),
        }
    }
    process(decoder.finish(false), &mut done)
}
