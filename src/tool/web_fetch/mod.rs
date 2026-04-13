/// WebFetch tool — fetch a web page, convert HTML to markdown, clip output.
mod bm25;
mod html;
mod ranking;

use crate::core::tool::{Tool, ToolExecution};
use crate::core::types::ToolSchema;
use anyhow::{Result, bail};
use std::future::Future;
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const MAX_OUTPUT_BYTES: usize = 262_144;
const FETCH_TIMEOUT_SECS: u64 = 30;
const USER_AGENT: &str = "Mozilla/5.0 (compatible; Luma/1.0)";

/// Fetch and read a web page as markdown.
pub struct WebFetchTool;

impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "WebFetch"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "WebFetch".into(),
            description: concat!(
                "Fetch and read a web page, returning content as markdown.\n",
                "- Extracts main content from <main>, <article>, role=\"main\".\n",
                "- Strips nav, footer, sidebar, scripts, styles.\n",
                "- When objective is given, returns only the most relevant excerpts.\n",
                "- Output clipped to 256KB.\n",
                "- Call in parallel for multiple unrelated URLs.\n",
                "- Use for reading documentation, blog posts, API references.",
            )
            .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL to fetch"
                    },
                    "objective": {
                        "type": "string",
                        "description": "Optional reading objective — returns ranked excerpts instead of full page"
                    }
                },
                "required": ["url"]
            }),
            streamable_arg: None,
        }
    }

    fn execute(
        &self,
        args: serde_json::Value,
        _output_tx: mpsc::Sender<String>,
        cancel: CancellationToken,
        _caps: crate::core::tool::ModelCaps,
    ) -> Pin<Box<dyn Future<Output = Result<ToolExecution>> + Send + '_>> {
        Box::pin(async move {
            let url = args["url"].as_str().unwrap_or("");
            if url.is_empty() {
                bail!("missing url");
            }
            let objective = args.get("objective").and_then(|v| v.as_str()).unwrap_or("");

            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT_SECS))
                .user_agent(USER_AGENT)
                .redirect(reqwest::redirect::Policy::limited(10))
                .build()?;

            let resp = tokio::select! {
                biased;
                _ = cancel.cancelled() => bail!("aborted"),
                r = client.get(url).send() => r?,
            };

            if !resp.status().is_success() {
                bail!("HTTP {} from {}", resp.status(), url);
            }

            let content_type = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();

            let body = tokio::select! {
                biased;
                _ = cancel.cancelled() => bail!("aborted"),
                r = resp.text() => r?,
            };

            let markdown = html::convert_to_markdown(&body, &content_type);

            if !objective.is_empty() {
                Ok(ToolExecution {
                    result: (ranking::rank_excerpts(&markdown, objective)).into(),
                    artifact: None,
                })
            } else {
                Ok(ToolExecution {
                    result: (clip_text(&markdown, MAX_OUTPUT_BYTES)).into(),
                    artifact: None,
                })
            }
        })
    }
}

/// Clip text to a maximum byte size, avoiding mid-character cuts.
fn clip_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_text_noop() {
        assert_eq!(clip_text("hello", 100), "hello");
    }

    #[test]
    fn clip_text_truncates() {
        assert_eq!(clip_text("hello world", 5), "hello");
    }

    #[test]
    fn clip_text_multibyte() {
        assert_eq!(clip_text("hello 世界", 7), "hello ");
    }
}
