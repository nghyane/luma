/// WebSearch tool — client-side web search via external APIs.
///
/// Used as fallback when the provider has no built-in search.
/// Supports Exa, Tavily, and SearXNG backends.
use crate::core::tool::{Tool, ToolExecution};
use crate::core::types::ToolSchema;
use anyhow::{Result, bail};
use std::future::Future;
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const SEARCH_TIMEOUT_SECS: u64 = 15;

/// Search backend configuration.
#[derive(Clone)]
pub enum SearchBackend {
    Exa {
        api_key: String,
    },
    Tavily {
        api_key: String,
    },
    SearXNG {
        base_url: String,
    },
    /// Kiro's built-in `web_search` MCP tool. Routes the query through
    /// `AmazonCodeWhispererStreamingService.InvokeMCP` and costs 0
    /// credits (probed live). Auth + profile ARN are resolved from the
    /// auth pool at call time so expired tokens auto-refresh.
    Kiro,
}

/// Client-side web search tool.
pub struct WebSearchTool {
    backend: SearchBackend,
}

impl WebSearchTool {
    /// Create a new web search tool with the given backend.
    pub fn new(backend: SearchBackend) -> Self {
        Self { backend }
    }
}

impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "WebSearch"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "WebSearch".into(),
            description: concat!(
                "Search the web for current information.\n",
                "- Returns titles, URLs, and relevant excerpts for top results.\n",
                "- Use for documentation, API references, error messages, latest versions.\n",
                "- Do not use for questions answerable from codebase alone.",
            )
            .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query"
                    },
                    "max_results": {
                        "type": "number",
                        "description": "Maximum results to return (default 5)"
                    }
                },
                "required": ["query"]
            }),
            streamable_arg: None,
        }
    }

    fn execute(
        &self,
        args: serde_json::Value,
        output_tx: mpsc::Sender<String>,
        cancel: CancellationToken,
        _caps: crate::core::tool::ModelCaps,
    ) -> Pin<Box<dyn Future<Output = Result<ToolExecution>> + Send + '_>> {
        Box::pin(async move {
            let query = args["query"].as_str().unwrap_or("");
            if query.is_empty() {
                bail!("missing query");
            }
            let max = args["max_results"].as_u64().unwrap_or(5) as usize;

            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(SEARCH_TIMEOUT_SECS))
                .build()?;

            let results = tokio::select! {
                biased;
                _ = cancel.cancelled() => bail!("aborted"),
                r = search(&self.backend, &client, query, max) => r?,
            };

            if results.is_empty() {
                return Ok(ToolExecution {
                    result: "No results found.".into(),
                    artifact: None,
                });
            }

            // Stream structured output for UI
            for r in &results {
                let mut entry = format!("{}\n{}\n", r.title, r.url);
                if !r.snippet.is_empty() {
                    entry.push_str(&r.snippet);
                    entry.push('\n');
                }
                entry.push('\n');
                let _ = output_tx.send(entry).await;
            }

            // Model-facing result: numbered list
            let mut output = String::new();
            for (i, r) in results.iter().enumerate() {
                output.push_str(&format!("{}. {}\n   {}\n", i + 1, r.title, r.url));
                if !r.snippet.is_empty() {
                    output.push_str(&format!("   {}\n", r.snippet));
                }
                output.push('\n');
            }
            Ok(ToolExecution {
                result: output.into(),
                artifact: None,
            })
        })
    }
}

struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// Dispatch to the appropriate backend.
async fn search(
    backend: &SearchBackend,
    client: &reqwest::Client,
    query: &str,
    max: usize,
) -> Result<Vec<SearchResult>> {
    match backend {
        SearchBackend::Exa { api_key } => search_exa(client, api_key, query, max).await,
        SearchBackend::Tavily { api_key } => search_tavily(client, api_key, query, max).await,
        SearchBackend::SearXNG { base_url } => search_searxng(client, base_url, query, max).await,
        SearchBackend::Kiro => search_kiro(client, query, max).await,
    }
}

/// Exa: type "auto" (neural + keyword), highlights for relevant excerpts.
async fn search_exa(
    client: &reqwest::Client,
    api_key: &str,
    query: &str,
    max: usize,
) -> Result<Vec<SearchResult>> {
    let resp = client
        .post("https://api.exa.ai/search")
        .header("x-api-key", api_key)
        .json(&serde_json::json!({
            "query": query,
            "num_results": max,
            "type": "auto",
            "contents": {
                "highlights": { "query": query }
            }
        }))
        .send()
        .await?;

    let body: serde_json::Value = resp.json().await?;
    let results = body["results"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|r| {
                    let highlights = r["highlights"]
                        .as_array()
                        .map(|h| {
                            h.iter()
                                .filter_map(|v| v.as_str())
                                .collect::<Vec<_>>()
                                .join(" ")
                        })
                        .unwrap_or_default();
                    SearchResult {
                        title: r["title"].as_str().unwrap_or("Untitled").to_owned(),
                        url: r["url"].as_str().unwrap_or("").to_owned(),
                        snippet: highlights,
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(results)
}

/// Tavily: include_answer for direct answers, content for snippets.
async fn search_tavily(
    client: &reqwest::Client,
    api_key: &str,
    query: &str,
    max: usize,
) -> Result<Vec<SearchResult>> {
    let resp = client
        .post("https://api.tavily.com/search")
        .json(&serde_json::json!({
            "api_key": api_key,
            "query": query,
            "max_results": max,
            "include_answer": true,
        }))
        .send()
        .await?;

    let body: serde_json::Value = resp.json().await?;

    let mut results: Vec<SearchResult> = Vec::new();

    // Prepend Tavily's direct answer if available
    if let Some(answer) = body["answer"].as_str()
        && !answer.is_empty()
    {
        results.push(SearchResult {
            title: "Direct Answer".to_owned(),
            url: String::new(),
            snippet: answer.to_owned(),
        });
    }

    if let Some(arr) = body["results"].as_array() {
        for r in arr {
            results.push(SearchResult {
                title: r["title"].as_str().unwrap_or("").to_owned(),
                url: r["url"].as_str().unwrap_or("").to_owned(),
                snippet: r["content"].as_str().unwrap_or("").to_owned(),
            });
        }
    }

    Ok(results)
}

/// SearXNG: JSON format, take top N results.
async fn search_searxng(
    client: &reqwest::Client,
    base_url: &str,
    query: &str,
    max: usize,
) -> Result<Vec<SearchResult>> {
    let url = format!("{}/search", base_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .query(&[("q", query), ("format", "json"), ("pageno", "1")])
        .send()
        .await?;

    let body: serde_json::Value = resp.json().await?;
    let results = body["results"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .take(max)
                .map(|r| SearchResult {
                    title: r["title"].as_str().unwrap_or("").to_owned(),
                    url: r["url"].as_str().unwrap_or("").to_owned(),
                    snippet: r["content"].as_str().unwrap_or("").to_owned(),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(results)
}

/// Kiro: `InvokeMCP` → web_search MCP tool on `q.us-east-1.amazonaws.com`.
///
/// Wire form (probed live, content-type `application/x-amz-json-1.0`):
///
/// ```text
/// POST /   X-Amz-Target: AmazonCodeWhispererStreamingService.InvokeMCP
/// { "profileArn": "...", "jsonrpc": "2.0", "id": "<turn>",
///   "method": "tools/call",
///   "params": { "name": "web_search", "arguments": { "query": "..." } } }
/// ```
///
/// Response is a JSON-RPC envelope. `result.content[0].text` contains a
/// JSON-encoded `{"results": [...]}` array with `title / url / snippet /
/// publishedDate / domain`. Queries longer than 200 chars are rejected
/// by the server — truncate at the byte boundary, it's a hard cap.
async fn search_kiro(
    client: &reqwest::Client,
    query: &str,
    max: usize,
) -> Result<Vec<SearchResult>> {
    // Kiro's web_search MCP tool caps queries at 200 chars. Truncate on
    // a UTF-8 char boundary so we don't ship a sliced codepoint.
    let trimmed = if query.chars().count() > 200 {
        query.chars().take(200).collect::<String>()
    } else {
        query.to_owned()
    };

    let cred = crate::config::auth::resolve(crate::config::auth::AuthVendor::Kiro).await?;
    let profile_arn = cred.profile_arn.as_deref().ok_or_else(|| {
        anyhow::anyhow!("Kiro credential has no profile_arn; re-run `luma login kiro`")
    })?;

    let body = serde_json::json!({
        "profileArn": profile_arn,
        "jsonrpc": "2.0",
        "id": "1",
        "method": "tools/call",
        "params": {
            "name": "web_search",
            "arguments": { "query": trimmed },
        },
    });

    let resp = client
        .post("https://q.us-east-1.amazonaws.com/")
        .header("Authorization", format!("Bearer {}", cred.token))
        .header("Content-Type", "application/x-amz-json-1.0")
        .header(
            "X-Amz-Target",
            "AmazonCodeWhispererStreamingService.InvokeMCP",
        )
        .header(
            "User-Agent",
            "aws-sdk-rust/1.3.14 ua/2.1 api/codewhispererstreaming/0.1.14474 \
             os/macos lang/rust/1.92.0 app/AmazonQ-For-CLI",
        )
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let snippet: String = text.chars().take(300).collect();
        bail!("Kiro web_search HTTP {status}: {snippet}");
    }

    let envelope: serde_json::Value = resp.json().await?;
    if let Some(err) = envelope.get("error") {
        bail!(
            "Kiro web_search error: {}",
            err.get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown")
        );
    }
    let text = envelope["result"]["content"][0]["text"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Kiro web_search: missing result.content[0].text"))?;
    let parsed: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| anyhow::anyhow!("Kiro web_search: bad inner JSON: {e}"))?;

    let items = parsed["results"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .take(max)
                .map(|r| SearchResult {
                    title: r["title"].as_str().unwrap_or("").to_owned(),
                    url: r["url"].as_str().unwrap_or("").to_owned(),
                    snippet: r["snippet"].as_str().unwrap_or("").to_owned(),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(items)
}
