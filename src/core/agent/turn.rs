/// Turn execution — auth, provider, tool loop, summaries, mid-turn save.
use super::AgentConfig;
use crate::core::provider::{ESCALATED_MAX_TOKENS, Provider, StopReason, StreamResponse};
use crate::core::registry::Registry;
use crate::core::session::Session;
use crate::core::types::{ContentBlock, Message, Role, ToolResultBody};
use crate::event::Event;
use crate::event_bus::Sender as EventSender;
use crate::provider::retry::ProviderRateLimited;
use anyhow::Result;
use tokio::sync::mpsc;

/// Fallback cap when evidence ingestion fails (I/O error on the blob).
///
/// Normal oversized results spill to the evidence store (see
/// `core::evidence` and `maybe_promote_to_evidence`). If the blob write
/// fails, this cap bounds the inline copy so a runaway tool can't balloon
/// the transcript. Dead path in practice — kept for defense in depth.
const SAFETY_FALLBACK_CAP: usize = 32_000;

const STREAM_RETRIES: u8 = 4;
const STREAM_RETRY_DELAY_SECS: u64 = 3;

/// Max outer retries for auth (401) + pool failover (429) combined. Bounds
/// runaway loops when several accounts are sequentially unhealthy.
const MAX_AUTH_RETRIES: u8 = 5;

/// Max consecutive stream errors before the turn gives up. Mirrors Claude
/// Code's approach of yielding errors as messages — but caps runaway loops.
const MAX_STREAM_ERROR_RECOVERY: u8 = 2;
const MAX_OUTPUT_RECOVERY: u8 = 3;

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
    writer: &crate::core::session::SessionWriter,
) -> Result<()> {
    use crate::auth::domain::AuthFailure;
    use crate::auth::repo::SqliteAuthRepository;
    use crate::auth::service::AuthService;
    use crate::config::auth;
    use crate::provider::binding::GatewayId;

    let gateway = GatewayId::from_source(&config.source);
    let provider_kind = gateway.auth_vendor();

    let caps = crate::core::tool::ModelCaps {
        vision: config.capabilities.iter().any(|c| c == "vision"),
    };

    let mut auth_cred = auth::resolve(provider_kind).await?;
    for attempt in 0..MAX_AUTH_RETRIES {
        let provider = build_provider(config, &auth_cred, &session.id);
        let outcome = run_turn(RunTurnCtx {
            session,
            provider: &*provider,
            model_id: &config.model_id,
            registry,
            tx,
            cancel: cancel.clone(),
            caps,
            writer,
        })
        .await;
        let err = match outcome {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };

        // 429 — rate-limited account. Mark cooldown and fail over to the
        // next healthy account in the same provider.
        if let Some(rl) = err.downcast_ref::<ProviderRateLimited>() {
            let label = rl.label.clone();
            let retry_after = rl.retry_after_secs;
            let Some(key) = auth_cred.account_key.as_ref() else {
                return Err(err);
            };
            let _ = AuthService::new(SqliteAuthRepository::with_default_path())
                .mark_rate_limited(key, retry_after);
            tx.send_or_log(Event::ToolOutput {
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

        // 401 / 403 — auth rejected by the server (typed by the HTTP
        // layer). OAuth accounts treat 403 as account-specific access
        // loss: mark this account `needs_relogin` and fail over to the
        // next healthy account immediately. 401 still gets a
        // force-refresh attempt on the same account. API keys surface an
        // actionable error and leave the pool untouched.
        if let Some(unauth) = err.downcast_ref::<crate::provider::retry::ProviderUnauthorized>() {
            if auth_cred.is_oauth && unauth.status == 403 {
                let dead_label = auth_cred.label.clone();
                let Some(key) = auth_cred.account_key.as_ref() else {
                    return Err(err);
                };
                let _ = AuthService::new(SqliteAuthRepository::with_default_path())
                    .mark_auth_failed(key, AuthFailure::Revoked);
                tx.send_or_log(Event::ToolOutput {
                    name: String::new(),
                    chunk: format!(
                        "{} account {} rejected access (403), switching…",
                        provider_kind.as_str(),
                        dead_label
                    ),
                })
                .await;
                if attempt + 1 == MAX_AUTH_RETRIES {
                    return Err(err);
                }
                auth_cred = auth::resolve(provider_kind).await?;
                continue;
            }

            if attempt + 1 == MAX_AUTH_RETRIES {
                return Err(err);
            }

            if !auth_cred.is_oauth {
                anyhow::bail!(
                    "{} key rejected ({}): {}. Run `luma login` to replace it.",
                    unauth.provider,
                    unauth.status,
                    unauth.detail
                );
            }
            tx.send_or_log(Event::ToolOutput {
                name: String::new(),
                chunk: "token rejected, refreshing…".into(),
            })
            .await;
            if matches!(
                provider_kind,
                crate::config::auth::AuthVendor::OpenAI | crate::config::auth::AuthVendor::Kiro
            ) {
                let Some(key) = auth_cred.account_key.as_ref() else {
                    return Err(err);
                };
                auth_cred = AuthService::new(SqliteAuthRepository::with_default_path())
                    .refresh_credential(key)
                    .await?;
            } else {
                auth_cred = auth::force_refresh(provider_kind).await?;
            }
            continue;
        }

        return Err(err);
    }
    anyhow::bail!("exhausted auth retries")
}

fn build_provider(
    config: &AgentConfig,
    auth: &crate::config::auth::Credential,
    session_id: &str,
) -> Box<dyn Provider> {
    use crate::provider::binding;
    let binding = binding::resolve(&config.source, &config.model_id);
    binding::build_provider(&binding, auth, session_id, config.thinking)
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
    model_id: &'a str,
    schemas: &'a [crate::core::types::ToolSchema],
    server_schemas: &'a [serde_json::Value],
    resolve_image: &'a crate::core::provider::ImageResolver,
    tx: &'a EventSender,
    cancel: &'a tokio_util::sync::CancellationToken,
}

/// Owned inputs for one turn. Bundles mutable session state and fixed
/// collaborators so the turn entrypoint stays cohesive and clippy-clean.
struct RunTurnCtx<'a> {
    session: &'a mut Session,
    provider: &'a dyn Provider,
    model_id: &'a str,
    registry: &'a Registry,
    tx: &'a EventSender,
    cancel: tokio_util::sync::CancellationToken,
    caps: crate::core::tool::ModelCaps,
    writer: &'a crate::core::session::SessionWriter,
}

/// Stream with automatic retry on transient network failures.
///
/// On a retryable failure, notifies the UI via `ProviderRetry` event and
/// re-sends the request. The caller's messages are immutable here —
/// only the caller (`run_turn`) mutates session state.
async fn stream_with_retry(
    ctx: &TurnCtx<'_>,
    messages: &[Message],
    provider_state: Option<crate::core::provider_state::ProviderRequestState<'_>>,
    max_tokens_override: Option<u32>,
    tool_use_tx: Option<tokio::sync::mpsc::Sender<crate::core::types::ContentBlock>>,
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
            ctx.tx
                .send_or_log(Event::ProviderRetry {
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
            provider_state,
            max_tokens_override,
            tx: ctx.tx.clone(),
            cancel: ctx.cancel.clone(),
            tool_use_tx: tool_use_tx.clone(),
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
async fn run_turn(ctx: RunTurnCtx<'_>) -> Result<()> {
    let RunTurnCtx {
        session,
        provider,
        model_id,
        registry,
        tx,
        cancel,
        caps,
        writer,
    } = ctx;
    let schemas = registry.schemas();
    let server_schemas = provider.server_tool_schemas(registry.server_capabilities());
    let resolve_image = crate::core::session::image_resolver(&session.id);
    let provider_state_kind = provider.session_state_kind();
    if let Some(kind) = provider_state_kind {
        session.ensure_provider_state(kind);
    }
    let ctx = TurnCtx {
        provider,
        model_id,
        schemas: &schemas,
        server_schemas: &server_schemas,
        resolve_image: &*resolve_image,
        tx,
        cancel: &cancel,
    };

    let mut output_recovery_count: u8 = 0;
    let mut stream_error_count: u8 = 0;
    let mut codex_turn_state: Option<String> = None;

    loop {
        if cancel.is_cancelled() {
            anyhow::bail!("Aborted");
        }

        // Create channel for streaming tool execution. Provider sends
        // completed ToolUse blocks here mid-stream so we can start
        // executing tools before the full response arrives.
        let (tu_tx, mut tu_rx) = tokio::sync::mpsc::channel::<crate::core::types::ContentBlock>(16);

        // First attempt: provider default max_tokens.
        let routing = provider.tool_result_image_routing();
        let routed = crate::core::provider::route_tool_result_images(&session.messages, routing);

        // Run stream and early tool execution concurrently. The stream
        // sends ToolUse blocks via tu_tx as they arrive; we collect them
        // and start executing as soon as the stream finishes (tu_tx drops
        // → tu_rx closes). Tools that arrived mid-stream are already
        // queued and execute immediately, overlapping with any post-stream
        // bookkeeping.
        let provider_state = provider_state_kind
            .and_then(|kind| session.request_state_for_turn(kind, codex_turn_state.as_deref()));
        let stream_future = stream_with_retry(&ctx, &routed, provider_state, None, Some(tu_tx));

        // Collect tool_use blocks that arrive mid-stream.
        let mut early_tool_uses: Vec<ToolUseRef> = Vec::new();
        let collect_future = async {
            while let Some(block) = tu_rx.recv().await {
                if let ContentBlock::ToolUse { id, name, input } = block {
                    early_tool_uses.push(ToolUseRef { id, name, input });
                }
            }
        };

        // Race: stream produces the response, channel collects tool blocks.
        // Stream finishing drops tu_tx → collect_future ends.
        let mut result = {
            let (stream_result, _) = tokio::join!(stream_future, collect_future);
            match stream_result {
                Ok(r) => r,
                Err(e) => {
                    if cancel.is_cancelled() {
                        anyhow::bail!("Aborted");
                    }
                    stream_error_count += 1;
                    if stream_error_count > MAX_STREAM_ERROR_RECOVERY {
                        return Err(e);
                    }
                    let msg = e.to_string();
                    tx
                        .send_or_log(Event::ToolOutput {
                            name: String::new(),
                            chunk: format!("stream error (recovery {stream_error_count}/{MAX_STREAM_ERROR_RECOVERY}): {msg}"),
                        })
                        .await;
                    session
                        .messages
                        .push(Message::system(format!("[API error — retrying: {msg}]")));
                    writer.enqueue(session);
                    continue;
                }
            }
        };

        // Escalate once if the first call hit max_tokens before finishing,
        // but only if the provider actually honors an override. For providers
        // that ignore `max_tokens_override` (e.g. Codex), retrying with the
        // same cap would waste a request; surface the failure directly.
        if result.stop_reason == StopReason::MaxTokens && provider.supports_max_tokens_override() {
            crate::dbg_log!("max_tokens hit — escalating to {ESCALATED_MAX_TOKENS} and retrying");
            tx.send_or_log(Event::ToolOutput {
                name: String::new(),
                chunk: format!(
                    "output limit hit, escalating max_tokens to {ESCALATED_MAX_TOKENS}…"
                ),
            })
            .await;
            match stream_with_retry(
                &ctx,
                &routed,
                provider_state_kind.and_then(|kind| {
                    session.request_state_for_turn(kind, codex_turn_state.as_deref())
                }),
                Some(ESCALATED_MAX_TOKENS),
                None,
            )
            .await
            {
                Ok(r) => result = r,
                Err(e) => {
                    if cancel.is_cancelled() {
                        anyhow::bail!("Aborted");
                    }
                    stream_error_count += 1;
                    if stream_error_count > MAX_STREAM_ERROR_RECOVERY {
                        return Err(e);
                    }
                    let msg = e.to_string();
                    session.messages.push(Message::system(format!(
                        "[API error on escalation — retrying: {msg}]"
                    )));
                    writer.enqueue(session);
                    continue;
                }
            }
        }

        // Successful stream resets the error counter.
        stream_error_count = 0;

        let context_usage_emitted = result.context_usage_emitted;
        let StreamResponse {
            message: response,
            usage,
            stop_reason,
            provider_state,
            ..
        } = result;
        if let Some(provider_state) = provider_state {
            let crate::core::provider_state::ProviderStateUpdate::Codex(update) = &provider_state;
            if let Some(turn_state) = update.turn_state.clone() {
                codex_turn_state = Some(turn_state);
            }
            session.apply_provider_state(provider_state);
        }

        // Snapshot current context window — replaces previous turn, not cumulative.
        session.usage.input_tokens = usage.input_tokens;
        session.usage.output_tokens = usage.output_tokens;
        session.usage.cache_read = usage.cache_read.unwrap_or(0);
        session.usage.cache_write = usage.cache_write.unwrap_or(0);

        session.messages.push(response.clone());
        // Mid-turn save: persist after each assistant message.
        writer.enqueue(session);

        // Context-usage fallback for providers that don't report tokens
        // or emit ContextUsage themselves. Matches Kiro CLI's algorithm:
        // chars = messages (text + tool_use input + tool_result) + tool specs
        // tokens = (chars / 4 + 5) / 10 * 10
        if usage.input_tokens == 0 && usage.output_tokens == 0 && !context_usage_emitted {
            let est_chars = crate::provider::estimate_context_chars(&session.messages, ctx.schemas);
            let ctx_window = crate::config::models::context_window(ctx.model_id);
            let est_tokens = ((est_chars / 4 + 5) / 10 * 10) as u64;
            let pct = ((est_tokens as f64 / ctx_window as f64) * 100.0).clamp(0.0, 100.0) as f32;
            ctx.tx
                .send_or_log(crate::event::Event::ContextUsage(pct))
                .await;
        }

        if cancel.is_cancelled() {
            anyhow::bail!("Aborted");
        }

        // Still MaxTokens after (potentially) escalating → inject a
        // recovery nudge so the model resumes where it left off, mirroring
        // Claude Code's max_output_tokens recovery loop.
        if stop_reason == StopReason::MaxTokens {
            output_recovery_count += 1;
            if output_recovery_count > MAX_OUTPUT_RECOVERY {
                anyhow::bail!(
                    "output token limit hit {} times. Start a new session or switch to a model with larger output capacity.",
                    output_recovery_count
                );
            }
            tx.send_or_log(Event::ToolOutput {
                name: String::new(),
                chunk: format!(
                    "output limit hit, resuming… (recovery {}/{})",
                    output_recovery_count, MAX_OUTPUT_RECOVERY
                ),
            })
            .await;
            session.messages.push(Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "Output token limit hit. Resume directly — no apology, no recap. \
                           Pick up mid-thought if that is where the cut happened. \
                           Break remaining work into smaller pieces."
                        .to_owned(),
                }],
                origin: None,
            });
            writer.enqueue(session);
            continue;
        }

        // Use early-collected tool_use blocks if available (arrived
        // mid-stream via channel), otherwise fall back to extracting
        // from the completed response.
        let tool_uses: Vec<ToolUseRef> = if !early_tool_uses.is_empty() {
            early_tool_uses
        } else {
            response
                .tool_uses()
                .map(|(id, name, input)| ToolUseRef {
                    id: id.to_owned(),
                    name: name.to_owned(),
                    input: input.clone(),
                })
                .collect()
        };
        if tool_uses.is_empty() {
            return Ok(());
        }

        let tool_results =
            execute_tools(&tool_uses, registry, tx, cancel.clone(), caps, &session.id).await;
        let aborted = cancel.is_cancelled();

        let turn_index = session.messages.len().saturating_sub(1);
        let evidence_dir = crate::core::session::session_evidence_dir(&session.id);

        // Push all tool_result blocks as a single user message — even on
        // abort, so the model sees what happened on replay. Results above
        // `EVIDENCE_PROMOTION_THRESHOLD` spill to the evidence store and
        // keep only a summary inline.
        let mut result_blocks: Vec<ContentBlock> = Vec::with_capacity(tool_results.len());
        for (id, body) in tool_results {
            let (content, evidence_id) = maybe_promote_to_evidence(
                session,
                &evidence_dir,
                turn_index,
                &tool_uses,
                &id,
                body,
            );
            result_blocks.push(ContentBlock::ToolResult {
                tool_use_id: id,
                content,
                is_error: false,
                evidence_id,
            });
        }
        session.messages.push(Message {
            role: Role::User,
            content: result_blocks,
            origin: None,
        });
        // Mid-turn save: persist after tool results.
        writer.enqueue(session);

        if aborted {
            anyhow::bail!("Aborted");
        }
    }
}

/// If `text` exceeds the evidence threshold, persist it as evidence and
/// return `(summary, Some(id))`; otherwise return `(text, None)`.
///
/// A failed blob write falls back to inline truncation so the turn keeps
/// progressing — losing disk space is worse than losing a debuggable
/// artifact. `SAFETY_FALLBACK_CAP` bounds the inline copy so a pathological
/// runaway tool can't blow up the transcript either way.
fn maybe_promote_to_evidence(
    session: &mut Session,
    evidence_dir: &std::path::Path,
    turn_index: usize,
    tool_uses: &[ToolUseRef],
    tool_use_id: &str,
    body: ToolResultBody,
) -> (ToolResultBody, Option<String>) {
    use crate::core::evidence::{EVIDENCE_PROMOTION_THRESHOLD, classify};
    use crate::core::types::ToolResultItem;

    // Evidence promotion operates on the textual portion only. Image
    // items (and any other non-text item types) ride through unchanged:
    // they have independent token costs and cannot be summarized into
    // a text preview sensibly.
    let (mut text, images): (String, Vec<ToolResultItem>) = match body {
        ToolResultBody::Text(s) => (s, Vec::new()),
        ToolResultBody::Items(items) => {
            let mut text = String::new();
            let mut imgs = Vec::new();
            for item in items {
                match item {
                    ToolResultItem::Text { text: t } => {
                        if !text.is_empty() && !t.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(&t);
                    }
                    img @ ToolResultItem::Image { .. } => imgs.push(img),
                }
            }
            (text, imgs)
        }
    };

    // Rebuild a body from `(text, images)` — single spot so every early
    // return keeps image items attached.
    let rebuild = |text: String, images: Vec<ToolResultItem>| -> ToolResultBody {
        if images.is_empty() {
            ToolResultBody::Text(text)
        } else {
            let mut items = Vec::with_capacity(1 + images.len());
            if !text.is_empty() {
                items.push(ToolResultItem::Text { text });
            }
            items.extend(images);
            ToolResultBody::Items(items)
        }
    };

    if text.len() < EVIDENCE_PROMOTION_THRESHOLD {
        return (rebuild(text, images), None);
    }
    let Some(tu) = tool_uses.iter().find(|t| t.id == tool_use_id) else {
        return (rebuild(text, images), None);
    };
    // A Read call pulling an `artifact://…` URI is the agent re-reading
    // a resource that already lives outside the transcript (either a
    // stored evidence blob or a discovered skill). Promoting the
    // returned content again would:
    //
    //   * duplicate the blob on disk (for `artifact://ev/`), and
    //   * hide the content the agent explicitly asked for by replacing
    //     it with yet another summary — the agent then loops,
    //     pull-reading into opaque summaries.
    //
    // Keep the content inline for this call. Cache cost is bounded
    // (one turn per explicit pull), and the bytes the agent asked for
    // are the bytes it receives.
    if tu.name.eq_ignore_ascii_case("read")
        && tu
            .input
            .get("path")
            .and_then(|v| v.as_str())
            .is_some_and(|p| p.starts_with("artifact://"))
    {
        return (rebuild(text, images), None);
    }
    let Some(draft) = classify(&tu.name, &tu.input, &text) else {
        return (rebuild(text, images), None);
    };
    let summary_template = draft.summary.clone();
    let preview = draft.preview.clone();
    match session
        .evidence
        .ingest(evidence_dir, turn_index, tool_use_id, draft)
    {
        Ok(id) => {
            let header = summary_template.replace("{id}", &id);
            let content = if preview.is_empty() {
                header
            } else {
                // Marker tells the model the inline block is a head
                // preview only, names the exact byte count of the full
                // blob, and points at the URI to fetch the rest. Wording
                // is deliberate: "preview only" + total size nudges the
                // model to re-read when the preview is insufficient,
                // instead of treating the snippet as the full answer.
                let total_bytes = text.len();
                format!(
                    "{header}\n\n{preview}\n\n[preview only — {total_bytes} bytes total, read artifact://ev/{id} for full content]"
                )
            };
            (rebuild(content, images), Some(id))
        }
        Err(e) => {
            crate::dbg_log!("evidence ingest failed for {tool_use_id}: {e}");
            if text.len() > SAFETY_FALLBACK_CAP {
                text.truncate(SAFETY_FALLBACK_CAP);
                text.push_str(crate::core::tool::TRUNCATION_MARKER);
            }
            (rebuild(text, images), None)
        }
    }
}

/// Check if a Read tool call targets a skill. Returns the skill name.
fn skill_name_from_read(tool_name: &str, args: &serde_json::Value) -> Option<String> {
    if !tool_name.eq_ignore_ascii_case("read") {
        return None;
    }
    let path = args.get("path")?.as_str()?;
    crate::config::skills::parse_skill_read_path(path)
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
    caps: crate::core::tool::ModelCaps,
) -> (String, ToolResultBody) {
    // Provider decoders tag malformed tool-input JSON with a synthetic
    // `_parse_error` field instead of feeding partial/empty input to the
    // tool (which then fails with a cryptic "missing path argument"
    // that doesn't tell the model what went wrong). Short-circuit here
    // with a targeted error so the model sees the decode failure and
    // can retry with a fresh tool call.
    if let Some(err) = tu.input.get("_parse_error").and_then(|v| v.as_str()) {
        let msg = format!(
            "tool_input decode failed: {err}. The provider streamed tool \
             arguments that did not parse as JSON. Retry the tool call \
             — do not assume the previous arguments reached the tool."
        );
        tx.send_or_log(Event::ToolEnd {
            name: tu.name.clone(),
            summary: msg.clone(),
        })
        .await;
        return (tu.id.clone(), msg.into());
    }

    let skill = skill_name_from_read(&tu.name, &tu.input);

    let result: ToolResultBody = match registry.get(&tu.name) {
        Some(tool) => {
            if let Some(name) = &skill {
                tx.send_or_log(Event::SkillStart(name.clone())).await;
            }

            let summary = format_tool_summary(&tu.name, &tu.input);
            tx.send_or_log(Event::ToolStart {
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

            let res = tool
                .execute(tu.input.clone(), output_tx, cancel, caps)
                .await;
            fwd_handle.await.ok();

            match res {
                Ok(exec) => {
                    if let Some(artifact) = exec.artifact {
                        tx.send_or_log(Event::ToolArtifact {
                            name: tu.name.clone(),
                            artifact: Box::new(artifact),
                        })
                        .await;
                    }
                    let end_summary = format_tool_result(&tu.name, &exec.result.as_text());
                    tx.send_or_log(Event::ToolEnd {
                        name: tu.name.clone(),
                        summary: end_summary,
                    })
                    .await;
                    if let Some(name) = &skill {
                        tx.send_or_log(Event::SkillEnd(format!("loaded {name}")))
                            .await;
                    }
                    exec.result
                }
                Err(e) => {
                    let msg = format!("Error: {e}");
                    tx.send_or_log(Event::ToolEnd {
                        name: tu.name.clone(),
                        summary: msg.clone(),
                    })
                    .await;
                    if let Some(name) = &skill {
                        tx.send_or_log(Event::SkillEnd(format!("failed to load {name}")))
                            .await;
                    }
                    msg.into()
                }
            }
        }
        None => format!("Unknown tool: {}", tu.name).into(),
    };
    (tu.id.clone(), result)
}

/// Execute tool calls — concurrent when multiple, preserving order.
/// Session scope is established internally so callers don't need to wrap.
pub async fn execute_tools(
    tool_uses: &[ToolUseRef],
    registry: &Registry,
    tx: &EventSender,
    cancel: tokio_util::sync::CancellationToken,
    caps: crate::core::tool::ModelCaps,
    session_id: &str,
) -> Vec<(String, ToolResultBody)> {
    crate::core::session::scope_current_session(session_id, async {
        if tool_uses.len() == 1 {
            return vec![execute_one(&tool_uses[0], registry, tx, cancel, caps).await];
        }
        let futures: Vec<_> = tool_uses
            .iter()
            .map(|tu| execute_one(tu, registry, tx, cancel.clone(), caps))
            .collect();
        futures::future::join_all(futures).await
    })
    .await
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
            _caps: crate::core::tool::ModelCaps,
        ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<ToolExecution>> + Send + '_>>
        {
            let counter = self.counter;
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                Ok(ToolExecution {
                    result: format!("done_{}", counter.load(Ordering::SeqCst)).into(),
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
        let results =
            execute_tools(&calls, &registry, &tx, cancel, Default::default(), "test").await;
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

    #[tokio::test]
    async fn parse_error_input_short_circuits_without_running_tool() {
        // Bug history: Anthropic decoder silently replaced unparseable
        // tool-input buffers with `{}`, so Edit ran with no `path` and
        // bailed "missing path argument" — useless to the model. Now
        // the decoder tags the failure with `_parse_error`; this test
        // locks in that the turn loop honors the tag and returns a
        // targeted error instead of invoking the tool.
        static CALLED: AtomicUsize = AtomicUsize::new(0);
        CALLED.store(0, Ordering::SeqCst);

        let mut registry = Registry::new();
        registry.register(Box::new(SlowTool { counter: &CALLED }));

        let (tx, _rx) = crate::event_bus::channel();
        let cancel = CancellationToken::new();

        let calls = vec![ToolUseRef {
            id: "tc_bad".into(),
            name: "slow".into(),
            input: serde_json::json!({
                "_parse_error": "EOF while parsing a string",
                "_raw_buffer": r#"{"path":"/tmp"#,
            }),
        }];

        let results =
            execute_tools(&calls, &registry, &tx, cancel, Default::default(), "test").await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "tc_bad");
        let body = results[0].1.as_text();
        assert!(
            body.contains("tool_input decode failed"),
            "unexpected body: {body}"
        );
        assert_eq!(
            CALLED.load(Ordering::SeqCst),
            0,
            "tool must not run when input failed to decode",
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

    #[test]
    fn short_result_stays_inline() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = Session::new();
        let tool_uses = vec![ToolUseRef {
            id: "tc_1".into(),
            name: "Read".into(),
            input: serde_json::json!({"path": "/tmp/x.rs"}),
        }];
        let (content, evidence_id) = maybe_promote_to_evidence(
            &mut session,
            tmp.path(),
            0,
            &tool_uses,
            "tc_1",
            "short".into(),
        );
        assert_eq!(content.as_text(), "short");
        assert!(evidence_id.is_none());
        assert!(session.evidence.records.is_empty());
    }

    #[test]
    fn oversized_result_promotes_to_evidence() {
        use crate::core::evidence::EVIDENCE_PROMOTION_THRESHOLD;

        let tmp = tempfile::tempdir().unwrap();
        let mut session = Session::new();
        let tool_uses = vec![ToolUseRef {
            id: "tc_1".into(),
            name: "Read".into(),
            input: serde_json::json!({"path": "/tmp/big.rs"}),
        }];
        // Use readable multi-line content so the preview has something
        // real to splice; a flat "x" repeat collapses to a single line
        // and defeats the line-boundary trim in `head_preview`.
        let line = "fn main() { println!(\"hello\"); }\n";
        let repeats = EVIDENCE_PROMOTION_THRESHOLD.div_ceil(line.len()) + 1;
        let big: String = line.repeat(repeats);
        let (content, evidence_id) = maybe_promote_to_evidence(
            &mut session,
            tmp.path(),
            2,
            &tool_uses,
            "tc_1",
            big.clone().into(),
        );
        let id = evidence_id.expect("promoted");
        let content_text = content.as_text();
        assert!(
            content_text.len() < big.len(),
            "inline content must be shorter than blob"
        );
        assert!(content_text.contains("/tmp/big.rs"), "header preserved");
        assert!(
            content_text.contains(&format!("artifact://ev/{id}")),
            "pull URI advertised so the agent can fetch the tail"
        );
        assert!(
            content_text.contains("fn main()"),
            "head preview is spliced so the model can reason this turn"
        );
        assert_eq!(session.evidence.records.len(), 1);
        let rec = &session.evidence.records[0];
        assert_eq!(rec.id, id);
        assert_eq!(rec.turn_index, 2);
        assert_eq!(rec.tool_use_id, "tc_1");
        let blob = std::fs::read_to_string(tmp.path().join(format!("{id}.txt"))).unwrap();
        assert_eq!(blob, big);
    }

    #[test]
    fn artifact_uri_read_stays_inline_even_when_oversized() {
        // An agent re-reading artifact://ev/{id} gets the stored
        // evidence back verbatim. Promoting that content again would
        // just duplicate the blob and loop the agent through a second
        // summary — the very thing the explicit pull was trying to
        // avoid. Guard against the regression.
        use crate::core::evidence::EVIDENCE_PROMOTION_THRESHOLD;

        let tmp = tempfile::tempdir().unwrap();
        let mut session = Session::new();
        let tool_uses = vec![ToolUseRef {
            id: "tc_pull".into(),
            name: "Read".into(),
            input: serde_json::json!({"path": "artifact://ev/ev_abc"}),
        }];
        let big = "y".repeat(EVIDENCE_PROMOTION_THRESHOLD + 1);
        let (content, evidence_id) = maybe_promote_to_evidence(
            &mut session,
            tmp.path(),
            3,
            &tool_uses,
            "tc_pull",
            big.clone().into(),
        );
        assert!(evidence_id.is_none(), "pull must not re-promote");
        assert_eq!(content.as_text(), big, "content must be returned verbatim");
        assert!(
            session.evidence.records.is_empty(),
            "no new evidence record from a pull"
        );
    }

    #[test]
    fn artifact_skill_read_stays_inline_even_when_oversized() {
        use crate::core::evidence::EVIDENCE_PROMOTION_THRESHOLD;

        let tmp = tempfile::tempdir().unwrap();
        let mut session = Session::new();
        let tool_uses = vec![ToolUseRef {
            id: "tc_skill".into(),
            name: "Read".into(),
            input: serde_json::json!({"path": "artifact://skill/test-skill"}),
        }];
        let big = "z".repeat(EVIDENCE_PROMOTION_THRESHOLD + 1);
        let (content, evidence_id) = maybe_promote_to_evidence(
            &mut session,
            tmp.path(),
            1,
            &tool_uses,
            "tc_skill",
            big.clone().into(),
        );
        assert!(evidence_id.is_none(), "skill pull must not promote");
        assert_eq!(content.as_text(), big);
    }
}
