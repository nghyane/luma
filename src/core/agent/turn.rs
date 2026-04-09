/// Turn execution — auth, provider, tool loop, summaries, mid-turn save.
use super::AgentConfig;
use crate::core::provider::Provider;
use crate::core::registry::Registry;
use crate::core::session::Session;
use crate::core::types::{Message, ToolCall};
use crate::event::Event;
use anyhow::Result;
use tokio::sync::mpsc;

const MAX_ITERATIONS: usize = 50;
const MAX_RESULT_LEN: usize = 32_000;
const STREAM_RETRIES: u8 = 2;
const STREAM_RETRY_DELAY_SECS: u64 = 2;

/// Run a chat turn: resolve auth → build provider → run tool loop.
/// Retries once on 401.
pub async fn run_chat_turn(
    session: &mut Session,
    config: &AgentConfig,
    registry: &Registry,
    tx: &mpsc::Sender<Event>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    use crate::config::auth::{self, AuthProvider};

    let provider_kind = match config.source.as_str() {
        "anthropic" => AuthProvider::Anthropic,
        _ => AuthProvider::OpenAI,
    };

    let auth = auth::resolve(provider_kind).await?;
    let provider = build_provider(config, &auth, &session.id);

    match run_turn(session, &*provider, registry, tx, cancel.clone()).await {
        Ok(()) => Ok(()),
        Err(e) if e.to_string().contains("401") || e.to_string().contains("Unauthorized") => {
            let _ = tx
                .send(Event::ToolOutput {
                    name: String::new(),
                    chunk: "Token expired, refreshing...".into(),
                })
                .await;
            auth::clear_cached(provider_kind);
            let auth = auth::resolve(provider_kind).await?;
            let provider = build_provider(config, &auth, &session.id);
            run_turn(session, &*provider, registry, tx, cancel).await
        }
        Err(e) => Err(e),
    }
}

fn build_provider(
    config: &AgentConfig,
    auth: &crate::config::auth::Credential,
    session_id: &str,
) -> Box<dyn Provider> {
    use crate::provider::claude::ClaudeProvider;
    use crate::provider::codex::CodexProvider;
    use crate::provider::openai::OpenAIProvider;

    match config.source.as_str() {
        "anthropic" => {
            let mut p = ClaudeProvider::new(&config.model_id, &auth.token, auth.is_oauth);
            p.set_thinking(config.thinking);
            Box::new(p)
        }
        "codex" => {
            let mut p = CodexProvider::new(
                &config.model_id,
                &auth.token,
                auth.account_id.clone(),
                session_id,
            );
            p.set_thinking(config.thinking);
            Box::new(p)
        }
        _ => {
            let mut p = OpenAIProvider::new(&config.model_id, &auth.token);
            p.set_thinking(config.thinking);
            Box::new(p)
        }
    }
}

/// Whether an error is a transient stream failure worth retrying.
fn is_stream_retryable(err: &anyhow::Error) -> bool {
    // Typed: providers emit StreamInterrupted for recoverable failures.
    if err.downcast_ref::<crate::provider::sse::StreamInterrupted>().is_some() {
        return true;
    }
    // Reqwest transport errors (connection reset, broken pipe, etc.)
    if let Some(re) = err.downcast_ref::<reqwest::Error>() {
        return re.is_connect() || re.is_timeout() || re.is_request();
    }
    false
}

/// Stream with automatic retry on transient network failures.
///
/// On a retryable failure, notifies the UI via ProviderRetry event and
/// re-sends the request. The caller's messages are immutable here —
/// only the caller (run_turn) mutates session state.
async fn stream_with_retry(
    messages: &[Message],
    provider: &dyn Provider,
    schemas: &[crate::core::types::ToolSchema],
    server_schemas: &[serde_json::Value],
    resolve_image: &crate::core::provider::ImageResolver,
    tx: &mpsc::Sender<Event>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(Message, crate::core::types::Usage)> {
    let mut last_err: Option<anyhow::Error> = None;

    for attempt in 0..=STREAM_RETRIES {
        if cancel.is_cancelled() {
            anyhow::bail!("Aborted");
        }

        if attempt > 0 {
            if let Some(ref e) = last_err {
                crate::dbg_log!("stream retry attempt {attempt}: {e}");
            }
            let _ = tx
                .send(Event::ProviderRetry {
                    provider: provider.name().to_owned(),
                    delay_secs: STREAM_RETRY_DELAY_SECS,
                    attempt,
                    max_attempts: STREAM_RETRIES + 1,
                })
                .await;
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(STREAM_RETRY_DELAY_SECS)) => {}
                _ = cancel.cancelled() => anyhow::bail!("Aborted"),
            }
        }

        match provider
            .stream(
                messages,
                schemas,
                server_schemas,
                resolve_image,
                tx.clone(),
                cancel.clone(),
            )
            .await
        {
            Ok(result) => return Ok(result),
            Err(e) => {
                if !is_stream_retryable(&e) || attempt == STREAM_RETRIES {
                    return Err(e);
                }
                last_err = Some(e);
            }
        }
    }

    // Unreachable — loop returns or breaks above.
    anyhow::bail!("stream failed after retries")
}

/// Run one turn: provider call → tool execution loop.
async fn run_turn(
    session: &mut Session,
    provider: &dyn Provider,
    registry: &Registry,
    tx: &mpsc::Sender<Event>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let schemas = registry.schemas();
    let server_schemas = provider.server_tool_schemas(registry.server_capabilities());
    let resolve_image = crate::core::session::image_resolver(&session.id);

    for _ in 0..MAX_ITERATIONS {
        if cancel.is_cancelled() {
            anyhow::bail!("Aborted");
        }

        let (response, usage) = stream_with_retry(
            &session.messages,
            provider,
            &schemas,
            &server_schemas,
            &*resolve_image,
            tx,
            &cancel,
        )
        .await?;

        session.usage.input_tokens += usage.input_tokens;
        session.usage.output_tokens += usage.output_tokens;
        session.usage.cache_read += usage.cache_read.unwrap_or(0);
        session.usage.cache_write += usage.cache_write.unwrap_or(0);

        session.messages.push(response.clone());
        // Mid-turn save: persist after each assistant message.
        session.save();

        if cancel.is_cancelled() {
            anyhow::bail!("Aborted");
        }

        let tool_calls = match &response.tool_calls {
            Some(tcs) if !tcs.is_empty() => tcs.clone(),
            _ => return Ok(()),
        };

        let tool_results = execute_tools(&tool_calls, registry, tx, cancel.clone()).await;
        let aborted = cancel.is_cancelled();

        // Always push tool results — even on abort, so LLM sees what happened
        for (tc_id, result) in tool_results {
            let mut truncated = result;
            if truncated.len() > MAX_RESULT_LEN {
                truncated.truncate(MAX_RESULT_LEN);
                truncated.push_str("\n[truncated]");
            }
            session.messages.push(Message::tool(tc_id, truncated));
        }
        // Mid-turn save: persist after tool results.
        session.save();

        if aborted {
            anyhow::bail!("Aborted");
        }
    }
    Ok(())
}

/// Check if a read tool call targets a SKILL.md file.
fn skill_name_from_read(tool_name: &str, args: &serde_json::Value) -> Option<String> {
    if tool_name != "read" {
        return None;
    }
    let path = args.get("path")?.as_str()?;
    if !path.ends_with("SKILL.md") {
        return None;
    }
    // Extract skill name from parent directory
    std::path::Path::new(path)
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
}

/// Execute a single tool call, streaming output events.
async fn execute_one(
    tc: &ToolCall,
    registry: &Registry,
    tx: &mpsc::Sender<Event>,
    cancel: tokio_util::sync::CancellationToken,
) -> (String, String) {
    let args: serde_json::Value = serde_json::from_str(&tc.function.arguments)
        .unwrap_or(serde_json::Value::Object(Default::default()));

    let skill = skill_name_from_read(&tc.function.name, &args);

    let result = match registry.get(&tc.function.name) {
        Some(tool) => {
            if let Some(name) = &skill {
                let _ = tx.send(Event::SkillStart(name.clone())).await;
            }

            let summary = format_tool_summary(&tc.function.name, &args);
            let _ = tx
                .send(Event::ToolStart {
                    name: tc.function.name.clone(),
                    summary,
                })
                .await;

            let (output_tx, mut output_rx) = mpsc::channel::<String>(256);
            let tx_fwd = tx.clone();
            let tool_name = tc.function.name.clone();
            let fwd_handle = tokio::spawn(async move {
                while let Some(chunk) = output_rx.recv().await {
                    let _ = tx_fwd
                        .send(Event::ToolOutput {
                            name: tool_name.clone(),
                            chunk,
                        })
                        .await;
                }
            });

            let res = tool.execute(args, output_tx, cancel).await;
            fwd_handle.await.ok();

            match res {
                Ok(r) => {
                    let end_summary = format_tool_result(&tc.function.name, &r);
                    let _ = tx
                        .send(Event::ToolEnd {
                            name: tc.function.name.clone(),
                            summary: end_summary,
                        })
                        .await;
                    if let Some(name) = &skill {
                        let _ = tx.send(Event::SkillEnd(format!("loaded {name}"))).await;
                    }
                    r
                }
                Err(e) => {
                    let msg = format!("Error: {e}");
                    let _ = tx
                        .send(Event::ToolEnd {
                            name: tc.function.name.clone(),
                            summary: msg.clone(),
                        })
                        .await;
                    if let Some(name) = &skill {
                        let _ = tx
                            .send(Event::SkillEnd(format!("failed to load {name}")))
                            .await;
                    }
                    msg
                }
            }
        }
        None => format!("Unknown tool: {}", tc.function.name),
    };
    (tc.id.clone(), result)
}

/// Execute tool calls — concurrent when multiple, preserving order.
pub async fn execute_tools(
    tool_calls: &[ToolCall],
    registry: &Registry,
    tx: &mpsc::Sender<Event>,
    cancel: tokio_util::sync::CancellationToken,
) -> Vec<(String, String)> {
    if tool_calls.len() == 1 {
        return vec![execute_one(&tool_calls[0], registry, tx, cancel).await];
    }
    let futures: Vec<_> = tool_calls
        .iter()
        .map(|tc| execute_one(tc, registry, tx, cancel.clone()))
        .collect();
    futures::future::join_all(futures).await
}

use super::summary::{format_tool_result, format_tool_summary};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::registry::Registry;
    use crate::core::tool::Tool;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio_util::sync::CancellationToken;

    struct SlowTool {
        counter: &'static AtomicUsize,
    }

    impl Tool for SlowTool {
        fn name(&self) -> &str {
            "slow"
        }
        fn schema(&self) -> crate::core::types::ToolSchema {
            crate::core::types::ToolSchema {
                name: "slow".into(),
                description: "test".into(),
                parameters: serde_json::json!({}),
            }
        }
        fn execute(
            &self,
            _args: serde_json::Value,
            _output_tx: mpsc::Sender<String>,
            _cancel: CancellationToken,
        ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send + '_>>
        {
            let counter = self.counter;
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                Ok(format!("done_{}", counter.load(Ordering::SeqCst)))
            })
        }
    }

    #[tokio::test]
    async fn parallel_tool_execution() {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        COUNTER.store(0, Ordering::SeqCst);

        let mut registry = Registry::new();
        registry.register(Box::new(SlowTool { counter: &COUNTER }));

        let (tx, _rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();

        let calls = vec![
            ToolCall {
                id: "tc_1".into(),
                r#type: "function".into(),
                function: crate::core::types::ToolCallFunction {
                    name: "slow".into(),
                    arguments: "{}".into(),
                },
            },
            ToolCall {
                id: "tc_2".into(),
                r#type: "function".into(),
                function: crate::core::types::ToolCallFunction {
                    name: "slow".into(),
                    arguments: "{}".into(),
                },
            },
        ];

        let start = std::time::Instant::now();
        let results = execute_tools(&calls, &registry, &tx, cancel).await;
        let elapsed = start.elapsed();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "tc_1");
        assert_eq!(results[1].0, "tc_2");
        assert!(
            elapsed.as_millis() < 100,
            "took {}ms, expected parallel",
            elapsed.as_millis()
        );
    }

    #[test]
    fn stream_interrupted_is_retryable() {
        let err: anyhow::Error =
            crate::provider::sse::StreamInterrupted("timeout".into()).into();
        assert!(is_stream_retryable(&err));
    }

    #[test]
    fn auth_error_is_not_retryable() {
        let err = anyhow::anyhow!("401 Unauthorized");
        assert!(!is_stream_retryable(&err));
    }

    #[test]
    fn abort_is_not_retryable() {
        let err = anyhow::anyhow!("Aborted");
        assert!(!is_stream_retryable(&err));
    }
}
