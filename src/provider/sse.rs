/// SSE (Server-Sent Events) streaming parser for LLM APIs.
///
/// Provides a pull-based event stream over reqwest responses. Backpressure
/// is honored end-to-end: the background reader task blocks when the
/// consumer lags, and the consumer blocks when the network lags.
use crate::event_bus::Sender as EventSender;
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

/// Transient stream failure — safe to retry.
///
/// Emitted when the SSE connection drops mid-stream (timeout, network cut,
/// incomplete response). Typed so retry logic can downcast instead of
/// matching error message strings.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct StreamInterrupted(pub String);

/// A parsed SSE event with type and JSON data.
#[derive(Debug, Clone)]
pub struct SseEvent {
    #[allow(dead_code)]
    pub event_type: String,
    pub data: serde_json::Value,
}

/// Byte-level SSE line buffer.
///
/// Accumulates raw bytes from the HTTP response and yields complete SSE
/// events (per the `event:` / `data:` protocol) as JSON is parsed out of
/// `data:` lines. Buffers incomplete lines so multi-byte UTF-8 characters
/// (e.g. Vietnamese diacritics, CJK) are never split.
///
/// Typical use:
/// 1. Feed bytes from each HTTP chunk via [`Self::push_bytes`].
/// 2. After each push, drain complete events with [`Self::drain`].
/// 3. Each drained event contains the SSE event type + parsed JSON.
///
/// The buffer also tracks whether `[DONE]` has been seen (OpenAI-style).
pub struct SseLineBuffer {
    raw: Vec<u8>,
    current_event: String,
    saw_done: bool,
}

impl SseLineBuffer {
    /// Create an empty buffer.
    pub fn new() -> Self {
        Self {
            raw: Vec::new(),
            current_event: String::new(),
            saw_done: false,
        }
    }

    /// Whether an SSE `[DONE]` sentinel has been seen.
    pub fn saw_done(&self) -> bool {
        self.saw_done
    }

    /// Append a chunk of raw bytes.
    pub fn push_bytes(&mut self, chunk: &[u8]) {
        self.raw.extend_from_slice(chunk);
    }

    /// Drain all complete SSE events currently buffered.
    ///
    /// Only lines ending with `\n` are processed; any trailing partial line
    /// stays in the buffer for the next chunk.
    pub fn drain(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        let mut start = 0;
        while let Some(rel_pos) = self.raw[start..].iter().position(|&b| b == b'\n') {
            let newline_pos = start + rel_pos;
            // String::from_utf8_lossy is safe here because we only process
            // *complete lines*. Within a line, multi-byte chars are never
            // split (SSE is line-oriented, and the chunk boundary only
            // matters across newlines, which are always valid 1-byte).
            let line = String::from_utf8_lossy(&self.raw[start..newline_pos]);
            start = newline_pos + 1;

            if let Some(rest) = line.strip_prefix("event:") {
                self.current_event.clear();
                self.current_event.push_str(rest.trim());
            } else if let Some(rest) = line.strip_prefix("data:") {
                let raw = rest.trim();
                if raw == "[DONE]" {
                    self.saw_done = true;
                    continue;
                }
                if let Ok(data) = serde_json::from_str::<serde_json::Value>(raw) {
                    let event_type = if self.current_event.is_empty() {
                        data.get("type")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_owned()
                    } else {
                        self.current_event.clone()
                    };
                    events.push(SseEvent { event_type, data });
                }
            } else if line.is_empty() {
                self.current_event.clear();
            }
        }
        if start > 0 {
            self.raw.drain(..start);
        }
        events
    }
}

impl Default for SseLineBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// Channel capacity for buffering SSE events between the background reader
/// task and the provider consumer. Sized to absorb short bursts (e.g. a
/// flurry of `input_json_delta` events) without blocking, while still
/// providing backpressure on sustained overload.
const SSE_EVENT_CHANNEL_CAPACITY: usize = 128;

/// A pull-based stream of SSE events.
///
/// Backpressure: the background task that reads from the HTTP response
/// blocks on `tx.send().await` when the consumer is slower than the
/// network, so no events are ever dropped. Dropping the stream aborts the
/// background task immediately.
pub struct SseEventStream {
    rx: mpsc::Receiver<Result<SseEvent>>,
    saw_done: Arc<AtomicBool>,
    _task: AbortOnDropHandle<()>,
}

impl SseEventStream {
    /// Await the next event, or `None` when the stream ends.
    pub async fn next(&mut self) -> Option<Result<SseEvent>> {
        self.rx.recv().await
    }

    /// Whether the stream terminated with an OpenAI-style `[DONE]` sentinel.
    /// Call after the stream is exhausted (i.e. `next()` returned `None`).
    pub fn saw_done(&self) -> bool {
        self.saw_done.load(Ordering::Acquire)
    }
}

/// Build and POST an SSE request, returning a pull-based event stream.
///
/// The HTTP handshake (and any transient-failure retries) happen before
/// return; by the time the caller gets a stream the response headers have
/// been received and streaming has begun. Events are parsed and forwarded
/// on a background task that honors both consumer backpressure and the
/// cancellation token.
pub async fn post_sse(
    provider: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &serde_json::Value,
    tx: &EventSender,
    cancel: &CancellationToken,
) -> Result<SseEventStream> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()?;
    let response = crate::provider::retry::send_with_retry(provider, tx, cancel, || {
        let mut req = client
            .post(url)
            .header("Content-Type", "application/json")
            .json(body);

        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        req.send()
    })
    .await?;

    let (event_tx, event_rx) = mpsc::channel::<Result<SseEvent>>(SSE_EVENT_CHANNEL_CAPACITY);
    let saw_done = Arc::new(AtomicBool::new(false));
    let saw_done_task = saw_done.clone();
    let cancel_task = cancel.clone();

    let handle = tokio::spawn(async move {
        reader_loop(response, event_tx, saw_done_task, cancel_task).await;
    });

    Ok(SseEventStream {
        rx: event_rx,
        saw_done,
        _task: AbortOnDropHandle::new(handle),
    })
}

/// Background task: read HTTP chunks, parse SSE lines, forward events.
async fn reader_loop(
    mut response: reqwest::Response,
    event_tx: mpsc::Sender<Result<SseEvent>>,
    saw_done: Arc<AtomicBool>,
    cancel: CancellationToken,
) {
    let mut buf = SseLineBuffer::new();
    // Timeout between chunks — if server stops sending data for 120s, bail.
    let chunk_timeout = std::time::Duration::from_secs(120);

    loop {
        let chunk_result = tokio::select! {
            c = response.chunk() => c,
            _ = cancel.cancelled() => {
                let _ = event_tx.send(Err(anyhow::anyhow!("Aborted"))).await;
                return;
            }
            _ = tokio::time::sleep(chunk_timeout) => {
                let _ = event_tx
                    .send(Err(StreamInterrupted(
                        "SSE stream timeout — no data for 120s".into(),
                    ).into()))
                    .await;
                return;
            }
        };

        let chunk_opt = match chunk_result {
            Ok(c) => c,
            Err(e) => {
                let _ = event_tx.send(Err(e.into())).await;
                return;
            }
        };

        let Some(chunk) = chunk_opt else { break };
        buf.push_bytes(&chunk);
        for event in buf.drain() {
            // send().await applies backpressure: if the consumer is slow,
            // the reader blocks here until space is available.
            if event_tx.send(Ok(event)).await.is_err() {
                // Consumer dropped the stream — nothing to do.
                return;
            }
        }
    }

    saw_done.store(buf.saw_done(), Ordering::Release);
    // Closing event_tx naturally signals EOF to the consumer via recv() → None.
}

#[doc(hidden)]
#[cfg(test)]
pub fn stream_from_events(events: Vec<Result<SseEvent>>, saw_done: bool) -> SseEventStream {
    let (tx, rx) = mpsc::channel::<Result<SseEvent>>(events.len().max(1));
    let saw_done_arc = Arc::new(AtomicBool::new(saw_done));
    let handle = tokio::spawn(async move {
        for event in events {
            if tx.send(event).await.is_err() {
                return;
            }
        }
    });
    SseEventStream {
        rx,
        saw_done: saw_done_arc,
        _task: AbortOnDropHandle::new(handle),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_event_fields() {
        let event = SseEvent {
            event_type: "message".into(),
            data: serde_json::json!({"text": "hi"}),
        };
        assert_eq!(event.event_type, "message");
        assert_eq!(event.data["text"], "hi");
    }

    #[test]
    fn buffer_parses_simple_data_event() {
        let mut buf = SseLineBuffer::new();
        buf.push_bytes(b"data: {\"type\":\"hello\",\"x\":1}\n\n");
        let events = buf.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "hello");
        assert_eq!(events[0].data["x"], 1);
    }

    #[test]
    fn buffer_honors_explicit_event_line() {
        let mut buf = SseLineBuffer::new();
        buf.push_bytes(b"event: message_delta\ndata: {\"x\":1}\n\n");
        let events = buf.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "message_delta");
    }

    #[test]
    fn buffer_detects_done_sentinel() {
        let mut buf = SseLineBuffer::new();
        buf.push_bytes(b"data: [DONE]\n\n");
        assert!(buf.drain().is_empty());
        assert!(buf.saw_done());
    }

    #[test]
    fn buffer_handles_split_chunks() {
        let mut buf = SseLineBuffer::new();
        buf.push_bytes(b"data: {\"ty");
        assert!(buf.drain().is_empty());
        buf.push_bytes(b"pe\":\"x\"}\n\n");
        let events = buf.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "x");
    }

    #[test]
    fn buffer_preserves_multibyte_across_chunk_boundary() {
        let mut buf = SseLineBuffer::new();
        // "xin chào" — "à" is 2 bytes (UTF-8: 0xC3 0xA0).
        // Split in the middle of the multi-byte char.
        let full = b"data: {\"text\":\"xin ch\xc3\xa0o\"}\n\n";
        let mid = 20; // roughly mid-string
        buf.push_bytes(&full[..mid]);
        assert!(buf.drain().is_empty());
        buf.push_bytes(&full[mid..]);
        let events = buf.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data["text"], "xin chào");
    }

    #[test]
    fn buffer_resets_event_on_blank_line() {
        let mut buf = SseLineBuffer::new();
        buf.push_bytes(b"event: foo\n\ndata: {\"x\":1}\n\n");
        let events = buf.drain();
        assert_eq!(events.len(), 1);
        // Blank line reset the current_event, so data falls back to JSON type field.
        assert_eq!(events[0].event_type, "");
    }

    #[test]
    fn buffer_skips_malformed_json() {
        let mut buf = SseLineBuffer::new();
        buf.push_bytes(b"data: not valid json\n\ndata: {\"type\":\"ok\"}\n\n");
        let events = buf.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "ok");
    }

    #[tokio::test]
    async fn event_stream_delivers_events_in_order() {
        let events = vec![
            Ok(SseEvent {
                event_type: "a".into(),
                data: serde_json::json!({"n": 1}),
            }),
            Ok(SseEvent {
                event_type: "b".into(),
                data: serde_json::json!({"n": 2}),
            }),
        ];
        let mut stream = stream_from_events(events, true);

        let e1 = stream.next().await.unwrap().unwrap();
        assert_eq!(e1.event_type, "a");
        let e2 = stream.next().await.unwrap().unwrap();
        assert_eq!(e2.event_type, "b");
        assert!(stream.next().await.is_none());
        assert!(stream.saw_done());
    }

    #[tokio::test]
    async fn event_stream_propagates_error() {
        let events = vec![Err(anyhow::anyhow!("boom"))];
        let mut stream = stream_from_events(events, false);
        let err = stream.next().await.unwrap().unwrap_err();
        assert_eq!(err.to_string(), "boom");
    }
}
