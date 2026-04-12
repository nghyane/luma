/// Turn execution — auth, provider, tool loop, summaries, mid-turn save.
use super::AgentConfig;
use crate::core::provider::{Provider, StopReason, StreamResponse};
use crate::core::registry::Registry;
use crate::core::session::Session;
use crate::core::types::{ContentBlock, Message, Role};
use crate::event::Event;
use crate::event_bus::Sender as EventSender;
use crate::provider::claude::ESCALATED_MAX_TOKENS;
use crate::provider::retry::ProviderRateLimited;
use anyhow::Result;
use tokio::sync::mpsc;

const MAX_ITERATIONS: usize = 50;

/// Final safety cap on a single tool_result before it is written into the
/// transcript. Tools are expected to produce outputs already bounded by
/// their own limits (e.g. `bash` does head+tail truncation at 32K); this
/// cap only catches tools that forgot to bound, to keep the transcript
/// from ballooning on a runaway result.
///
/// When the evidence store (see `docs/rfcs/evidence-backed-handoff.md`)
/// lands, this cap is superseded by evidence ingestion and this site will
/// instead create an `EvidenceRecord`.
const AGENT_RESULT_SAFETY_CAP: usize = 32_000;

const STREAM_RETRIES: u8 = 2;
const STREAM_RETRY_DELAY_SECS: u64 = 2;

/// Max outer retries for auth (401) + pool failover (429) combined. Bounds
/// runaway loops when several accounts are sequentially unhealthy.
const MAX_AUTH_RETRIES: u8 = 5;

/// Run a chat turn: resolve auth → build provider → run tool loop.
///
/// Handles two kinds of cross-request retries at this level:
///
/// * **401** — token rejected by the server. Force-refresh the current
///   account's OAuth tokens and retry once.
/// * **429** — account is rate-limited. Mark it on cooldown in the pool
///   and resolve a *different* account for the same provider, then
///   rebuild the provider and retry. This is transparent to the user
///   unless every account for the provider is cooling, in which case a
///   clear "all accounts cooling" error surfaces.
pub async fn run_chat_turn(
    session: &mut Session,
    config: &AgentConfig,
    registry: &Registry,
    tx: &EventSender,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    use crate::config::auth::{self, AuthProvider};

    let provider_kind = match config.source.as_str() {
        "anthropic" => AuthProvider::Anthropic,
        _ => AuthProvider::OpenAI,
    };

    let mut auth_cred = auth::resolve(provider_kind).await?;
    for attempt in 0..MAX_AUTH_RETRIES {
        let provider = build_provider(config, &auth_cred, &session.id);
        let outcome = run_turn(session, &*provider, registry, tx, cancel.clone()).await;
        let err = match outcome {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };

        // 429 — rate-limited account. Mark cooldown and fail over to the
        // next healthy account in the same provider.
        if let Some(rl) = err.downcast_ref::<ProviderRateLimited>() {
            let label = rl.label.clone();
            let retry_after = rl.retry_after_secs;
            auth::mark_rate_limited(&label, retry_after);
            let _ = tx
                .send(Event::ToolOutput {
                    name: String::new(),
                    chunk: format!(
                        "{} account {} rate limited, switching…",
                        provider_kind.as_str(),
                        label
                    ),
                })
                .await;
            if attempt + 1 == MAX_AUTH_RETRIES {
                return Err(err);
            }
            auth_cred = auth::resolve(provider_kind).await?;
            continue;
        }

        // 401 — stale / revoked token. Force a refresh and retry once.
        if is_auth_error(provider_kind.as_str(), &err) {
            let _ = tx
                .send(Event::ToolOutput {
                    name: String::new(),
                    chunk: "token rejected, refreshing…".into(),
                })
                .await;
            auth_cred = auth::force_refresh(provider_kind).await?;
            continue;
        }

        return Err(err);
    }
    anyhow::bail!("exhausted auth retries")
}

fn is_auth_error(provider: &str, err: &anyhow::Error) -> bool {
    let msg = err.to_string();
    crate::provider::retry::classify_auth_failure(provider, reqwest::StatusCode::UNAUTHORIZED, &msg)
        .is_some()
        || msg.contains("401")
        || msg.contains("Unauthorized")
        || msg.contains("unauthorized")
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
            let mut p =
                ClaudeProvider::new(&config.model_id, &auth.token, auth.is_oauth, &auth.label);
            p.set_thinking(p.thinking_capabilities().coerce(config.thinking));
            Box::new(p)
        }
        "codex" => {
            let mut p = CodexProvider::new(
                &config.model_id,
                &auth.token,
                auth.account_id.clone(),
                session_id,
                &auth.label,
            );
            p.set_thinking(p.thinking_capabilities().coerce(config.thinking));
            Box::new(p)
        }
        _ => {
            let mut p = OpenAIProvider::new(&config.model_id, &auth.token, &auth.label);
            p.set_thinking(p.thinking_capabilities().coerce(config.thinking));
            Box::new(p)
        }
    }
}

/// Whether an error is a transient stream failure worth retrying.
fn is_stream_retryable(err: &anyhow::Error) -> bool {
    // Typed: providers emit StreamInterrupted for recoverable failures.
    if err
        .downcast_ref::<crate::provider::sse::StreamInterrupted>()
        .is_some()
    {
        return true;
    }
    // Reqwest transport errors (connection reset, broken pipe, etc.)
    if let Some(re) = err.downcast_ref::<reqwest::Error>() {
        return re.is_connect() || re.is_timeout() || re.is_request();
    }
    false
}

/// Shared context for turn execution — fixed across iterations and retries.
struct TurnCtx<'a> {
    provider: &'a dyn Provider,
    schemas: &'a [crate::core::types::ToolSchema],
    server_schemas: &'a [serde_json::Value],
    resolve_image: &'a crate::core::provider::ImageResolver,
    tx: &'a EventSender,
    cancel: &'a tokio_util::sync::CancellationToken,
}

/// Stream with automatic retry on transient network failures.
///
/// On a retryable failure, notifies the UI via `ProviderRetry` event and
/// re-sends the request. The caller's messages are immutable here —
/// only the caller (`run_turn`) mutates session state.
async fn stream_with_retry(
    ctx: &TurnCtx<'_>,
    messages: &[Message],
    max_tokens_override: Option<u32>,
) -> Result<StreamResponse> {
    use crate::core::provider::StreamRequest;

    let mut last_err: Option<anyhow::Error> = None;

    for attempt in 0..=STREAM_RETRIES {
        if ctx.cancel.is_cancelled() {
            anyhow::bail!("Aborted");
        }

        if attempt > 0 {
            if let Some(ref e) = last_err {
                crate::dbg_log!("stream retry attempt {attempt}: {e}");
            }
            let _ = ctx
                .tx
                .send(Event::ProviderRetry {
                    provider: ctx.provider.name().to_owned(),
                    delay_secs: STREAM_RETRY_DELAY_SECS,
                    attempt,
                    max_attempts: STREAM_RETRIES + 1,
                })
                .await;
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(STREAM_RETRY_DELAY_SECS)) => {}
                _ = ctx.cancel.cancelled() => anyhow::bail!("Aborted"),
            }
        }

        let req = StreamRequest {
            messages,
            tools: ctx.schemas,
            server_tools: ctx.server_schemas,
            resolve_image: ctx.resolve_image,
            max_tokens_override,
            tx: ctx.tx.clone(),
            cancel: ctx.cancel.clone(),
        };
        match ctx.provider.stream(req).await {
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
///
/// Per-request escalation on `max_tokens`: if a stream finishes with
/// `stop_reason = MaxTokens` using the provider default, the same request is
/// retried once with [`ESCALATED_MAX_TOKENS`]. Mirrors claude-code's
/// `max_output_tokens_escalate` path.
async fn run_turn(
    session: &mut Session,
    provider: &dyn Provider,
    registry: &Registry,
    tx: &EventSender,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let schemas = registry.schemas();
    let server_schemas = provider.server_tool_schemas(registry.server_capabilities());
    let resolve_image = crate::core::session::image_resolver(&session.id);
    let ctx = TurnCtx {
        provider,
        schemas: &schemas,
        server_schemas: &server_schemas,
        resolve_image: &*resolve_image,
        tx,
        cancel: &cancel,
    };

    for _ in 0..MAX_ITERATIONS {
        if cancel.is_cancelled() {
            anyhow::bail!("Aborted");
        }

        // First attempt: provider default max_tokens.
        let mut result = stream_with_retry(&ctx, &session.messages, None).await?;

        // Escalate once if the first call hit max_tokens before finishing,
        // but only if the provider actually honors an override. For providers
        // that ignore `max_tokens_override` (e.g. Codex), retrying with the
        // same cap would waste a request; surface the failure directly.
        if result.stop_reason == StopReason::MaxTokens && provider.supports_max_tokens_override() {
            crate::dbg_log!("max_tokens hit — escalating to {ESCALATED_MAX_TOKENS} and retrying");
            let _ = tx
                .send(Event::ProviderRetry {
                    provider: provider.name().to_owned(),
                    delay_secs: 0,
                    attempt: 1,
                    max_attempts: 2,
                })
                .await;
            result = stream_with_retry(&ctx, &session.messages, Some(ESCALATED_MAX_TOKENS)).await?;
        }

        let StreamResponse {
            message: response,
            usage,
            stop_reason,
        } = result;

        // Snapshot current context window — replaces previous turn, not cumulative.
        session.usage.input_tokens = usage.input_tokens;
        session.usage.output_tokens = usage.output_tokens;
        session.usage.cache_read = usage.cache_read.unwrap_or(0);
        session.usage.cache_write = usage.cache_write.unwrap_or(0);

        session.messages.push(response.clone());
        // Mid-turn save: persist after each assistant message.
        session.save();

        if cancel.is_cancelled() {
            anyhow::bail!("Aborted");
        }

        // Still MaxTokens after (potentially) escalating → turn is cut off.
        // Message differs depending on whether escalation actually ran.
        if stop_reason == StopReason::MaxTokens {
            if provider.supports_max_tokens_override() {
                anyhow::bail!(
                    "output token limit hit even at {ESCALATED_MAX_TOKENS} tokens. \
                     Try /compact or switch model."
                );
            }
            anyhow::bail!(
                "{} hit its output token limit. Try /compact or switch model.",
                provider.name()
            );
        }

        // Collect tool_use blocks in document order — required so that
        // tool_result blocks on the next user message line up 1:1.
        let tool_uses: Vec<ToolUseRef> = response
            .tool_uses()
            .map(|(id, name, input)| ToolUseRef {
                id: id.to_owned(),
                name: name.to_owned(),
                input: input.clone(),
            })
            .collect();
        if tool_uses.is_empty() {
            return Ok(());
        }

        let tool_results = execute_tools(&tool_uses, registry, tx, cancel.clone()).await;
        let aborted = cancel.is_cancelled();

        // Push all tool_result blocks as a single user message — even on
        // abort, so the model sees what happened on replay.
        let mut result_blocks: Vec<ContentBlock> = Vec::with_capacity(tool_results.len());
        for (id, mut text) in tool_results {
            if text.len() > AGENT_RESULT_SAFETY_CAP {
                text.truncate(AGENT_RESULT_SAFETY_CAP);
                text.push_str(crate::core::tool::TRUNCATION_MARKER);
            }
            result_blocks.push(ContentBlock::ToolResult {
                tool_use_id: id,
                content: text,
                is_error: false,
            });
        }
        session.messages.push(Message {
            role: Role::User,
            content: result_blocks,
            origin: None,
        });
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

/// Owned reference to a single tool_use request being executed. Held across
/// the async tool boundary so `execute_tools` can borrow nothing from the
/// session while the tool runs.
#[derive(Clone)]
pub struct ToolUseRef {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

/// Execute a single tool call, streaming output events.
async fn execute_one(
    tu: &ToolUseRef,
    registry: &Registry,
    tx: &EventSender,
    cancel: tokio_util::sync::CancellationToken,
) -> (String, String) {
    let skill = skill_name_from_read(&tu.name, &tu.input);

    let result = match registry.get(&tu.name) {
        Some(tool) => {
            if let Some(name) = &skill {
                let _ = tx.send(Event::SkillStart(name.clone())).await;
            }

            let summary = format_tool_summary(&tu.name, &tu.input);
            let _ = tx
                .send(Event::ToolStart {
                    name: tu.name.clone(),
                    summary,
                })
                .await;

            let (output_tx, mut output_rx) = mpsc::channel::<String>(256);
            let tx_fwd = tx.clone();
            let tool_name = tu.name.clone();
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

            let res = tool.execute(tu.input.clone(), output_tx, cancel).await;
            fwd_handle.await.ok();

            match res {
                Ok(exec) => {
                    if let Some(artifact) = exec.artifact {
                        let _ = tx
                            .send(Event::ToolArtifact {
                                name: tu.name.clone(),
                                artifact: Box::new(artifact),
                            })
                            .await;
                    }
                    let end_summary = format_tool_result(&tu.name, &exec.result);
                    let _ = tx
                        .send(Event::ToolEnd {
                            name: tu.name.clone(),
                            summary: end_summary,
                        })
                        .await;
                    if let Some(name) = &skill {
                        let _ = tx.send(Event::SkillEnd(format!("loaded {name}"))).await;
                    }
                    exec.result
                }
                Err(e) => {
                    let msg = format!("Error: {e}");
                    let _ = tx
                        .send(Event::ToolEnd {
                            name: tu.name.clone(),
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
        None => format!("Unknown tool: {}", tu.name),
    };
    (tu.id.clone(), result)
}

/// Execute tool calls — concurrent when multiple, preserving order.
pub async fn execute_tools(
    tool_uses: &[ToolUseRef],
    registry: &Registry,
    tx: &EventSender,
    cancel: tokio_util::sync::CancellationToken,
) -> Vec<(String, String)> {
    if tool_uses.len() == 1 {
        return vec![execute_one(&tool_uses[0], registry, tx, cancel).await];
    }
    let futures: Vec<_> = tool_uses
        .iter()
        .map(|tu| execute_one(tu, registry, tx, cancel.clone()))
        .collect();
    futures::future::join_all(futures).await
}

use super::summary::{format_tool_result, format_tool_summary};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::registry::Registry;
    use crate::core::tool::{Tool, ToolExecution};
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
                streamable_arg: None,
            }
        }
        fn execute(
            &self,
            _args: serde_json::Value,
            _output_tx: mpsc::Sender<String>,
            _cancel: CancellationToken,
        ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<ToolExecution>> + Send + '_>>
        {
            let counter = self.counter;
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                Ok(ToolExecution {
                    result: format!("done_{}", counter.load(Ordering::SeqCst)),
                    artifact: None,
                })
            })
        }
    }

    #[tokio::test]
    async fn parallel_tool_execution() {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        COUNTER.store(0, Ordering::SeqCst);

        let mut registry = Registry::new();
        registry.register(Box::new(SlowTool { counter: &COUNTER }));

        let (tx, _rx) = crate::event_bus::channel();
        let cancel = CancellationToken::new();

        let calls = vec![
            ToolUseRef {
                id: "tc_1".into(),
                name: "slow".into(),
                input: serde_json::json!({}),
            },
            ToolUseRef {
                id: "tc_2".into(),
                name: "slow".into(),
                input: serde_json::json!({}),
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
        let err: anyhow::Error = crate::provider::sse::StreamInterrupted("timeout".into()).into();
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
