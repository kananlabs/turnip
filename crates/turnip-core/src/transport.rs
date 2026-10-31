//! HTTP transport: a shared client, retry policy for transient failures, and
//! an incremental SSE parser. All network I/O in the core goes through here.

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::{CoreError, Result};

/// A request failure, classified for retry decisions.
#[derive(Debug)]
pub struct HttpError {
    pub message: String,
    pub status: Option<u16>,
}

impl HttpError {
    pub fn new(message: impl Into<String>, status: Option<u16>) -> Self {
        HttpError {
            message: message.into(),
            status,
        }
    }

    /// Transient failures get retried: network errors and 408/425/429/5xx.
    pub fn retryable(&self) -> bool {
        match self.status {
            None => true,
            Some(status) => matches!(status, 408 | 425 | 429 | 500..=599),
        }
    }
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(status) => write!(f, "HTTP {status}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// Pumped while transport waits; returning false aborts. Implemented for
/// closures so callers keep their ergonomics.
pub trait Tick {
    fn tick(&mut self) -> bool;
}

impl<F: FnMut() -> bool> Tick for F {
    fn tick(&mut self) -> bool {
        self()
    }
}

/// Observes retries while a streaming request is established. Also a `Tick`.
pub trait RetryObserver: Tick {
    /// 1-based attempt that just failed, total attempts, delay about to be
    /// slept, and the HTTP status (None for transport errors).
    fn on_retry(&mut self, attempt: u32, max_attempts: u32, delay_ms: u64, status: Option<u16>);
}

/// The outcome of a failed streaming request.
#[derive(Debug)]
pub enum StreamRequestError {
    Http(HttpError),
    /// The `tick` callback aborted between attempts.
    Aborted,
}

impl From<StreamRequestError> for HttpError {
    fn from(error: StreamRequestError) -> HttpError {
        match error {
            StreamRequestError::Http(error) => error,
            StreamRequestError::Aborted => HttpError::new("aborted", None),
        }
    }
}

/// Retry/backoff policy, resolved from configuration.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub retry_base_ms: u64,
    pub idle_timeout_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            max_retries: 4,
            retry_base_ms: 1000,
            idle_timeout_ms: 600_000,
        }
    }
}

/// One complete SSE message: an optional `event:` name and joined `data:`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseMessage {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental SSE parser: feed raw bytes, receive complete messages.
#[derive(Debug, Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    event: Option<String>,
    data: String,
}

impl SseParser {
    pub fn new() -> Self {
        SseParser::default()
    }

    /// Feed bytes and drain every complete message they complete.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<SseMessage> {
        let mut messages = Vec::new();
        self.buffer.extend_from_slice(bytes);
        while let Some(newline) = self.buffer.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=newline).collect();
            line.pop(); // '\n'
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.handle_line(&line, &mut messages);
        }
        messages
    }

    fn handle_line(&mut self, line: &[u8], messages: &mut Vec<SseMessage>) {
        if line.is_empty() {
            // A blank line dispatches the pending field set.
            if !self.data.is_empty() {
                self.data.pop(); // trailing '\n' between data lines
                messages.push(SseMessage {
                    event: self.event.take(),
                    data: std::mem::take(&mut self.data),
                });
            } else {
                self.event = None;
            }
            return;
        }
        let (field, value) = match line.iter().position(|b| *b == b':') {
            Some(0) => return, // a comment line
            Some(colon) => (&line[..colon], &line[colon + 1..]),
            None => (line, &line[..0]),
        };
        let value = value.strip_prefix(b" ").unwrap_or(value);
        match field {
            b"event" => self.event = Some(String::from_utf8_lossy(value).into_owned()),
            b"data" => {
                self.data.push_str(&String::from_utf8_lossy(value));
                self.data.push('\n');
            }
            _ => {} // id:, retry:, and extensions are ignored
        }
    }
}

/// Shared HTTP client plus retry policy.
pub struct HttpTransport {
    client: reqwest::blocking::Client,
    pub policy: RetryPolicy,
}

impl HttpTransport {
    pub fn new(policy: RetryPolicy) -> Self {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            // A coarse ceiling so a hung non-streaming request cannot block
            // forever; streaming reads use their own idle logic below.
            .timeout(Duration::from_secs(1800))
            .user_agent(concat!("turnip/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client builds");
        HttpTransport { client, policy }
    }

    /// One non-streaming request (model listings, probes). A `timeout`
    /// overrides the client default.
    pub fn request(
        &self,
        method: &str,
        url: &str,
        headers: &BTreeMap<String, String>,
        body: Option<&serde_json::Value>,
        timeout: Option<Duration>,
    ) -> std::result::Result<String, HttpError> {
        let method: reqwest::Method = method
            .parse()
            .map_err(|e| HttpError::new(format!("invalid method: {e}"), None))?;
        let mut request = self.client.request(method, url);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        let response = request
            .send()
            .map_err(|e| HttpError::new(e.to_string(), None))?;
        let status = response.status();
        let text = response
            .text()
            .map_err(|e| HttpError::new(e.to_string(), None))?;
        if !status.is_success() {
            return Err(HttpError::new(
                crate::provider::api_error_message(&text),
                Some(status.as_u16()),
            ));
        }
        Ok(text)
    }

    /// POST an SSE stream with retries on transient failures.
    ///
    /// `observer.tick` is polled while waiting (both between retries and
    /// while a stream is quiet); returning false aborts with
    /// [`StreamRequestError::Aborted`]. `observer.on_retry` observes each
    /// retry.
    pub fn post_sse(
        &self,
        url: &str,
        headers: &BTreeMap<String, String>,
        body: &serde_json::Value,
        observer: &mut dyn RetryObserver,
    ) -> std::result::Result<SseReader, StreamRequestError> {
        let max_attempts = self.policy.max_retries.saturating_add(1);
        let mut attempt = 1;
        loop {
            let mut request = self.client.post(url);
            for (name, value) in headers {
                request = request.header(name, value);
            }
            let request = request.json(body);
            let failure = match request.send() {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        return Ok(SseReader::spawn(response));
                    }
                    let text = response.text().unwrap_or_default();
                    HttpError::new(
                        crate::provider::api_error_message(&text),
                        Some(status.as_u16()),
                    )
                }
                Err(error) => HttpError::new(error.to_string(), None),
            };
            if attempt >= max_attempts || !failure.retryable() {
                return Err(StreamRequestError::Http(failure));
            }
            // Exponential backoff with a quarter-band jitter.
            let backoff = self
                .policy
                .retry_base_ms
                .saturating_mul(1u64 << (attempt - 1).min(10));
            let noise = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_millis() as u64)
                .unwrap_or(0)
                % (backoff / 4).max(1);
            let delay_ms = backoff.saturating_sub(noise).max(1);
            observer.on_retry(attempt, max_attempts, delay_ms, failure.status);
            if !retry_sleep(delay_ms, observer) {
                return Err(StreamRequestError::Aborted);
            }
            attempt += 1;
        }
    }
}

/// Sleep with tick pumping; returns false when the caller aborted.
fn retry_sleep(ms: u64, tick: &mut dyn Tick) -> bool {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline {
        if !tick.tick() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    tick.tick()
}

/// Reads a streaming response on a background thread so the caller can poll
/// with timeouts (keeping Ctrl+C responsive on every platform).
pub struct SseReader {
    receiver: mpsc::Receiver<std::io::Result<Vec<u8>>>,
}

impl SseReader {
    fn spawn(response: reqwest::blocking::Response) -> SseReader {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = response;
            let mut chunk = [0u8; 16 * 1024];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        if sender.send(Ok(chunk[..n].to_vec())).is_err() {
                            break; // caller went away; drop the transfer
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        break;
                    }
                }
            }
        });
        SseReader { receiver }
    }

    /// Poll for the next chunk. `tick` runs about every 50 ms while waiting;
    /// returning false aborts (the reader thread dies with the channel).
    /// Returns Ok(None) at end of stream, or Err on abort or read failure.
    pub fn next_chunk(
        &self,
        tick: &mut dyn Tick,
        idle_timeout_ms: u64,
    ) -> Result<Option<Vec<u8>>> {
        let idle = Instant::now();
        loop {
            match self.receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(chunk) => {
                    return match chunk {
                        Ok(bytes) => Ok(Some(bytes)),
                        Err(error) => Err(CoreError::Message(error.to_string())),
                    };
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if idle.elapsed() >= Duration::from_millis(idle_timeout_ms) {
                        return Err(CoreError::message("stream stalled: no data received"));
                    }
                    if !tick.tick() {
                        return Err(CoreError::Aborted);
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_parses_events_and_data() {
        let mut parser = SseParser::new();
        let messages = parser.feed(b"event: delta\ndata: {\"a\":1}\n\ndata: solo\n\n");
        assert_eq!(
            messages,
            vec![
                SseMessage {
                    event: Some("delta".into()),
                    data: "{\"a\":1}".into()
                },
                SseMessage {
                    event: None,
                    data: "solo".into()
                },
            ]
        );
    }

    #[test]
    fn sse_handles_crlf_and_comments() {
        let mut parser = SseParser::new();
        let messages = parser.feed(b": keep-alive\r\ndata: one\r\ndata: two\r\n\r\n");
        assert_eq!(
            messages,
            vec![SseMessage {
                event: None,
                data: "one\ntwo".into()
            }]
        );
    }

    #[test]
    fn sse_splits_partial_chunks() {
        let mut parser = SseParser::new();
        assert!(parser.feed(b"data: he").is_empty());
        assert_eq!(parser.feed(b"llo\n\n").len(), 1);
    }

    #[test]
    fn retryability_classification() {
        assert!(HttpError::new("x", Some(429)).retryable());
        assert!(HttpError::new("x", Some(503)).retryable());
        assert!(HttpError::new("x", None).retryable());
        assert!(!HttpError::new("x", Some(400)).retryable());
        assert!(!HttpError::new("x", Some(401)).retryable());
    }
}
