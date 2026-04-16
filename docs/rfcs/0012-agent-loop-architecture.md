# RFC 0012: Agent Loop Architecture Overhaul

## Status: Draft

## Problem

Six architectural issues degrade agent reliability and performance:

1. **Sync I/O on async runtime** — `session.save()` calls `std::fs::write` on the tokio worker thread 3× per tool-loop iteration. Large sessions serialize megabytes of JSON, blocking the executor.

2. **Task-local session scope** — `scope_current_session` uses `tokio::task_local!`. The scope does not propagate across `tokio::spawn` boundaries, silently breaking any tool that calls `current_session_id()` from a spawned task.

3. **Event bus silent drops** — `try_send` returns `Err` on hard-cap, but every call site ignores it (`let _ = tx.send(...).await`). Under parallel tool execution the bus can saturate and lose tool output.

4. **Single-threaded command processing** — `agent_loop` blocks on `run_chat_turn` inside `while let Some(cmd) = cmd_rx.recv()`. `SetModel`, `SetContext`, `LoadSession` commands queue behind a running turn with no way to interrupt or interleave.

5. **Redundant orphan repair** — `fix_orphaned_tool_uses` scans the full message vec from the end. Called 3× per turn (post-turn, post-abort, post-load). Only the post-turn call is necessary.

6. **Unbounded evidence store** — evidence blobs accumulate on disk with no pruning. Long-running sessions leak storage indefinitely.

## Design

### 1. Async session persistence

Replace `Session::save()` with a write-behind channel:

```
Session::save()          →  Session::enqueue_save()
  std::fs::write(json)       tx.send(session.clone())
                                ↓
                           background task (spawn_blocking)
                             serialize + fs::write
                             coalesce: only latest wins
```

```rust
// core/session.rs
pub struct SessionWriter {
    tx: mpsc::Sender<Session>,
}

impl SessionWriter {
    pub fn spawn() -> Self {
        let (tx, mut rx) = mpsc::channel::<Session>(4);
        tokio::spawn(async move {
            while let Some(session) = rx.recv().await {
                // Drain any queued saves — only persist the latest.
                let mut latest = session;
                while let Ok(newer) = rx.try_recv() {
                    latest = newer;
                }
                let _ = tokio::task::spawn_blocking(move || {
                    latest.write_to_disk();
                }).await;
            }
        });
        Self { tx }
    }

    pub fn enqueue(&self, session: &Session) {
        // try_send: if channel full, a newer save is already queued.
        let _ = self.tx.try_send(session.clone());
    }
}
```

`run_turn` calls `writer.enqueue(&session)` instead of `session.save()`. Zero blocking on the async path. Coalescing means rapid mid-turn saves collapse into one disk write.

### 2. Session context via Arc instead of task-local

Replace `task_local! { CURRENT_SESSION }` with an explicit `Arc<str>` passed through the call chain:

```rust
// Before (implicit, breaks on spawn):
scope_current_session(&session.id, execute_tools(...)).await;

// After (explicit, works everywhere):
execute_tools(&tool_uses, registry, tx, cancel, caps, &session.id).await;
```

`session_id: &str` is threaded through `execute_tools` → `execute_one` → tool. Tools that need the session dir receive it as a parameter. No task-local, no silent breakage.

### 3. Event bus backpressure feedback

Add a `send_or_warn` helper that logs dropped events in debug mode:

```rust
// event_bus.rs
impl Sender {
    /// Send with diagnostic: in debug builds, log if the event was
    /// coalesced or if backpressure was applied. Never panics.
    pub async fn send_or_log(&self, event: Event) {
        if let Err(SendError(e)) = self.send(event).await {
            crate::dbg_log!("event bus closed, dropped: {:?}", 
                std::mem::discriminant(&e));
        }
    }
}
```

Replace all `let _ = tx.send(e).await` with `tx.send_or_log(e).await`. The bus itself is already well-designed (coalescing, soft/hard caps); the issue is purely that callers ignore errors.

### 4. Concurrent command handling in agent_loop

Split the loop into a `tokio::select!` that processes commands even during a turn:

```rust
async fn agent_loop(...) {
    loop {
        tokio::select! {
            // Turn is running — poll it.
            result = &mut active_turn, if active_turn.is_some() => {
                handle_turn_result(result, ...);
                active_turn = None;
            }
            // Command arrives — handle immediately.
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    AgentCommand::Chat { .. } if active_turn.is_some() => {
                        // Queue or abort-and-replace.
                    }
                    AgentCommand::Chat { cancel, .. } => {
                        active_turn = Some(spawn_turn(..., cancel));
                    }
                    AgentCommand::SetModel { .. } => {
                        // Apply immediately — takes effect next turn.
                        config.model_id = ...;
                    }
                    AgentCommand::SetContext { .. } => {
                        // Apply immediately — hot-swap.
                    }
                    AgentCommand::LoadSession { .. } => {
                        if let Some(turn) = active_turn.take() {
                            turn.cancel();
                        }
                        // Replace session.
                    }
                }
            }
        }
    }
}
```

`SetModel`, `SetContext`, `SetThinking` apply instantly. `LoadSession` cancels any active turn. `Chat` during an active turn triggers abort-and-requeue (existing behavior, now non-blocking).

The turn itself runs as a `tokio::spawn` task that communicates results back via a oneshot channel, so the select loop stays responsive.

### 5. Single-pass orphan repair

Call `fix_orphaned_tool_uses` exactly once, at the single point where it matters:

```rust
// agent.rs — after turn completes (success or error)
match result {
    Ok(_) | Err(_) => {
        fix_orphaned_tool_uses(&mut session.messages);
        writer.enqueue(&session);
    }
}
```

Remove the two redundant calls (post-abort duplicate, post-load). The post-load call moves to `LoadSession` handler only (once, not per-turn).

### 6. Evidence pruning

Add a retention policy to the evidence store:

```rust
// core/evidence.rs
impl EvidenceStore {
    /// Remove blobs older than `max_age` or exceeding `max_total_bytes`.
    /// Called once per session load and periodically during long turns.
    pub fn prune(&mut self, evidence_dir: &Path, max_age: Duration, max_total_bytes: u64) {
        // Sort by turn_index ascending (oldest first).
        // Delete until total_bytes < max_total_bytes or age < max_age.
        // Remove from self.records and unlink files.
    }
}
```

Trigger: on `LoadSession` and every N iterations in the tool loop (N=20). Default retention: 1 hour or 100 MB, whichever is smaller.

## Migration

All changes are internal to `core/agent.rs`, `core/agent/turn.rs`, `core/session.rs`, `core/evidence.rs`, and `event_bus.rs`. No public API changes. No protocol changes. No config changes.

### Ordering

1. **Async session persistence** — standalone, no dependencies
2. **Session context via Arc** — standalone, touches tool signatures
3. **Event bus send_or_log** — trivial, mechanical replacement
4. **Single-pass orphan repair** — trivial, delete 2 call sites
5. **Concurrent command handling** — depends on (1) for non-blocking save
6. **Evidence pruning** — standalone, lowest priority
