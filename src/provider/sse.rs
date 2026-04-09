/// SSE (Server-Sent Events) streaming parser for LLM APIs.
use crate::event::Event;
use anyhow::{Result, bail};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Transient stream failure — safe to retry.
///
/// Emitted when the SSE connection drops mid-stream (timeout, network cut,
/// incomplete response). Typed so retry logic can downcast instead of
/// matching error message strings.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct StreamInterrupted(pub String);

/// A parsed SSE event with type and JSON data.
pub struct SseEvent {
    #[allow(dead_code)]
    pub event_type: String,
    pub data: serde_json::Value,
}

/// SSE stream completion metadata.
pub struct SseOutcome {
    pub saw_done: bool,
}

/// Build and send an SSE POST request, then stream events via callback.
pub async fn post_sse(
    provider: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &serde_json::Value,
    tx: &mpsc::Sender<Event>,
    cancel: &CancellationToken,
    mut on_event: impl FnMut(SseEvent),
) -> Result<SseOutcome> {
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

    let mut raw_buf: Vec<u8> = Vec::new();
    let mut current_event = String::new();
    let mut response = response;
    // Timeout between chunks — if server stops sending data for 120s, bail.
    let chunk_timeout = std::time::Duration::from_secs(120);

    let mut saw_done = false;
    loop {
        let chunk = tokio::select! {
            c = response.chunk() => c?,
            _ = cancel.cancelled() => { bail!("Aborted"); }
            _ = tokio::time::sleep(chunk_timeout) => { return Err(StreamInterrupted("SSE stream timeout — no data for 120s".into()).into()); }
        };
        let Some(chunk) = chunk else {
            break;
        };
        raw_buf.extend_from_slice(&chunk);

        // Process only complete lines (ending with \n) to avoid splitting
        // multi-byte UTF-8 characters (e.g. Vietnamese diacritics) across chunks.
        let mut start = 0;
        while let Some(rel_pos) = raw_buf[start..].iter().position(|&b| b == b'\n') {
            let newline_pos = start + rel_pos;
            let line = String::from_utf8_lossy(&raw_buf[start..newline_pos]);
            start = newline_pos + 1;

            if let Some(rest) = line.strip_prefix("event:") {
                current_event.clear();
                current_event.push_str(rest.trim());
            } else if let Some(rest) = line.strip_prefix("data:") {
                let raw = rest.trim();
                if raw == "[DONE]" {
                    saw_done = true;
                    continue;
                }
                if let Ok(data) = serde_json::from_str::<serde_json::Value>(raw) {
                    let event_type = if current_event.is_empty() {
                        data.get("type")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_owned()
                    } else {
                        current_event.clone()
                    };
                    on_event(SseEvent { event_type, data });
                }
            } else if line.is_empty() {
                current_event.clear();
            }
        }
        if start > 0 {
            raw_buf.drain(..start);
        }
    }

    Ok(SseOutcome { saw_done })
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
}
