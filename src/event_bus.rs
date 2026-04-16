/// Coalescing event bus — unbounded-in-count, bounded-in-bytes, lossless.
///
/// Motivation: streaming LLM providers produce bursts of small `Token` /
/// `Thinking` / `ToolInput` events faster than the TUI can render. A
/// standard bounded `mpsc` forces a choice between:
///
/// - **Drop on full** (`try_send`) — loses user-visible output.
/// - **Block on full** (`send().await`) — stalls the whole provider loop,
///   cascading latency end-to-end.
///
/// This bus avoids both by exploiting a property of streaming events:
/// consecutive `Token` / `Thinking` / `ToolInput` deltas can be **merged**
/// into a single larger event without losing information. The UI cares
/// about final concatenated content, not the exact chunk boundaries.
///
/// ## Algorithm
///
/// - Queue is an unbounded `VecDeque<Event>` behind a mutex.
/// - Before pushing, the sender tries to merge the new event into the
///   queue's tail via [`Event::try_merge`]. Mergeable events collapse
///   into one entry; unmergeable events append.
/// - A soft cap on **total bytes across mergeable events** applies
///   backpressure only when the consumer is so slow that accumulated
///   content exceeds the limit. This prevents unbounded memory growth.
/// - A hard cap on **queue length** caps unmergeable bursts (a flood of
///   tool starts/ends is implausible but technically possible).
///
/// In normal operation with a responsive UI the queue stays tiny (1–4
/// entries) regardless of producer rate, because merging keeps pace with
/// arrival. Backpressure kicks in only under sustained UI stalls.
///
/// ## Correctness
///
/// - Lossless: every byte of content ever sent reaches the consumer.
/// - Ordered: unmergeable events preserve their relative order. Mergeable
///   events are combined in-place at the tail.
/// - Dropping the [`Sender`] signals EOF to the [`Receiver`]: `recv()`
///   returns `None` once the queue drains.
use crate::event::Event;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

/// Default soft cap on coalesced content bytes. ~4 MiB is enough to buffer
/// seconds of streaming output without meaningful memory pressure.
pub const DEFAULT_SOFT_CAP_BYTES: usize = 4 * 1024 * 1024;

/// Default hard cap on queue length (unmergeable event count).
pub const DEFAULT_HARD_CAP_EVENTS: usize = 4096;

struct Shared {
    queue: Mutex<VecDeque<Event>>,
    /// Cumulative byte size of the string payloads inside the queue.
    /// Used for soft-cap backpressure; approximate (counts `String::len`).
    bytes: AtomicUsize,
    soft_cap_bytes: usize,
    hard_cap_events: usize,
    /// Signalled when an event is pushed (consumer wake).
    data_ready: Notify,
    /// Signalled when an event is popped (producer wake).
    space_ready: Notify,
    /// `true` once every `Sender` has been dropped.
    closed: AtomicBool,
}

/// Sender half. Cheap to clone.
#[derive(Clone)]
pub struct Sender {
    shared: Arc<Shared>,
}

/// Receiver half. Not cloneable — single consumer.
pub struct Receiver {
    shared: Arc<Shared>,
}

/// Create a new coalescing event channel with default capacity.
pub fn channel() -> (Sender, Receiver) {
    channel_with(DEFAULT_SOFT_CAP_BYTES, DEFAULT_HARD_CAP_EVENTS)
}

/// Create a channel with custom caps. Primarily for testing.
pub fn channel_with(soft_cap_bytes: usize, hard_cap_events: usize) -> (Sender, Receiver) {
    let shared = Arc::new(Shared {
        queue: Mutex::new(VecDeque::new()),
        bytes: AtomicUsize::new(0),
        soft_cap_bytes,
        hard_cap_events,
        data_ready: Notify::new(),
        space_ready: Notify::new(),
        closed: AtomicBool::new(false),
    });
    (
        Sender {
            shared: shared.clone(),
        },
        Receiver { shared },
    )
}

/// Error returned when a send fails because the receiver has been dropped.
/// The original event is preserved inside so callers can retry or drop.
#[derive(Debug)]
pub struct SendError(pub Event);

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "event channel closed while sending {:?}", self.0)
    }
}

impl std::error::Error for SendError {}

impl Sender {
    /// Send an event.
    ///
    /// - If the event can be merged into the queue tail, it is merged
    ///   in-place without allocating a new queue entry.
    /// - Otherwise, the event is appended.
    /// - If the queue's accumulated byte size exceeds the soft cap, or
    ///   the queue length exceeds the hard cap, the sender awaits space.
    ///
    /// Returns `Err` if the receiver has been dropped.
    pub async fn send(&self, mut event: Event) -> Result<(), SendError> {
        loop {
            // Acquire the space notification *before* checking state so we
            // never miss a wake between check and wait.
            let wait = self.shared.space_ready.notified();
            tokio::pin!(wait);

            let outcome = self.try_push(event);
            match outcome {
                PushOutcome::Done => return Ok(()),
                PushOutcome::DoneOverCap => {
                    // Data was accepted but we're over the soft cap — wait
                    // until the consumer drains something.
                    wait.await;
                    return Ok(());
                }
                PushOutcome::Closed(e) => return Err(SendError(e)),
                PushOutcome::Full(e) => {
                    // Hard cap reached, unmergeable — wait and retry.
                    event = e;
                    wait.await;
                }
            }
        }
    }

    /// Send with diagnostic logging. Never panics, never silently drops.
    /// All agent/provider call sites should use this instead of bare `send`.
    pub async fn send_or_log(&self, event: Event) {
        if let Err(SendError(e)) = self.send(event).await {
            crate::dbg_log!("event bus closed, dropped {:?}", std::mem::discriminant(&e));
        }
    }

    /// Core push logic. Acquires the lock for the minimum window, releases
    /// before any `.await` the caller does.
    fn try_push(&self, event: Event) -> PushOutcome {
        let mut q = self.shared.queue.lock().unwrap();
        if is_closed(&self.shared) {
            return PushOutcome::Closed(event);
        }

        // Try to merge into the tail.
        if let Some(tail) = q.back_mut() {
            let added = content_size(&event);
            match tail.try_merge(event) {
                Ok(()) => {
                    self.shared.bytes.fetch_add(added, Ordering::AcqRel);
                    self.shared.data_ready.notify_one();
                    return if self.shared.bytes.load(Ordering::Acquire) > self.shared.soft_cap_bytes
                    {
                        PushOutcome::DoneOverCap
                    } else {
                        PushOutcome::Done
                    };
                }
                Err(returned) => {
                    // Fall through to append path.
                    if q.len() >= self.shared.hard_cap_events {
                        return PushOutcome::Full(returned);
                    }
                    let added = content_size(&returned);
                    q.push_back(returned);
                    self.shared.bytes.fetch_add(added, Ordering::AcqRel);
                    self.shared.data_ready.notify_one();
                    return PushOutcome::Done;
                }
            }
        }

        // Empty queue.
        if q.len() >= self.shared.hard_cap_events {
            return PushOutcome::Full(event);
        }
        let added = content_size(&event);
        q.push_back(event);
        self.shared.bytes.fetch_add(added, Ordering::AcqRel);
        self.shared.data_ready.notify_one();
        PushOutcome::Done
    }

    /// Non-blocking send, primarily for call sites that are themselves
    /// synchronous (e.g. inside a blocking stdin reader thread). Returns
    /// the event back on backpressure so the caller can drop or retry.
    pub fn try_send(&self, event: Event) -> Result<(), Event> {
        let mut q = self.shared.queue.lock().unwrap();
        if is_closed(&self.shared) {
            return Err(event);
        }
        if let Some(tail) = q.back_mut() {
            let added = content_size(&event);
            match tail.try_merge(event) {
                Ok(()) => {
                    self.shared.bytes.fetch_add(added, Ordering::AcqRel);
                    self.shared.data_ready.notify_one();
                    return Ok(());
                }
                Err(returned) => {
                    if q.len() >= self.shared.hard_cap_events {
                        return Err(returned);
                    }
                    let added = content_size(&returned);
                    q.push_back(returned);
                    self.shared.bytes.fetch_add(added, Ordering::AcqRel);
                    self.shared.data_ready.notify_one();
                    return Ok(());
                }
            }
        }
        if q.len() >= self.shared.hard_cap_events {
            return Err(event);
        }
        let added = content_size(&event);
        q.push_back(event);
        self.shared.bytes.fetch_add(added, Ordering::AcqRel);
        self.shared.data_ready.notify_one();
        Ok(())
    }

    /// Blocking send for call sites outside the tokio runtime (e.g. the
    /// stdin reader running on `spawn_blocking`). Soft-cap backpressure
    /// does not apply to callers here — they come from tiny, bounded
    /// sources like keyboard input — so the method simply retries on hard
    /// cap by parking briefly. Returns `Err` if the receiver is dropped.
    pub fn blocking_send(&self, mut event: Event) -> Result<(), SendError> {
        loop {
            match self.try_send(event) {
                Ok(()) => return Ok(()),
                Err(returned) => {
                    if is_closed(&self.shared) {
                        return Err(SendError(returned));
                    }
                    event = returned;
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
        }
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        // If this is the last sender, signal EOF.
        // Arc strong count == 2 means: this sender + the receiver. After
        // drop there will be only the receiver.
        if Arc::strong_count(&self.shared) == 2 {
            self.shared.closed.store(true, Ordering::Release);
            self.shared.data_ready.notify_waiters();
        }
    }
}

impl Receiver {
    /// Await the next event. Returns `None` once all senders have been
    /// dropped and the queue is empty.
    pub async fn recv(&mut self) -> Option<Event> {
        loop {
            let wait = self.shared.data_ready.notified();
            tokio::pin!(wait);

            match self.pop_or_check() {
                PopOutcome::Event(e) => return Some(e),
                PopOutcome::Closed => return None,
                PopOutcome::Empty => {
                    wait.await;
                }
            }
        }
    }

    /// Non-blocking drain of a single event if available.
    pub fn try_recv(&mut self) -> Option<Event> {
        match self.pop_or_check() {
            PopOutcome::Event(e) => Some(e),
            _ => None,
        }
    }

    fn pop_or_check(&self) -> PopOutcome {
        let mut q = self.shared.queue.lock().unwrap();
        if let Some(event) = q.pop_front() {
            let size = content_size(&event);
            self.shared.bytes.fetch_sub(size, Ordering::AcqRel);
            self.shared.space_ready.notify_waiters();
            return PopOutcome::Event(event);
        }
        if is_closed(&self.shared) {
            PopOutcome::Closed
        } else {
            PopOutcome::Empty
        }
    }
}

enum PopOutcome {
    Event(Event),
    Empty,
    Closed,
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Release);
        // Wake any blocked senders so they can observe closure.
        self.shared.space_ready.notify_waiters();
    }
}

fn is_closed(shared: &Shared) -> bool {
    shared.closed.load(Ordering::Acquire)
}

/// Outcome of a single push attempt. Constructed inside a non-async
/// critical section so the mutex guard never crosses `.await`.
enum PushOutcome {
    /// Pushed or merged successfully; queue is within caps.
    Done,
    /// Pushed or merged, but byte cap exceeded — caller should await
    /// consumer drain before returning success.
    DoneOverCap,
    /// Receiver has been dropped; caller should return error.
    Closed(Event),
    /// Queue is at hard cap and the event is not mergeable; caller
    /// should await space then retry.
    Full(Event),
}

/// Byte cost of an event for the soft-cap accounting. Only counts the
/// payload that grows with streaming (strings inside merge-eligible
/// variants); other events are accounted as zero since they arrive in
/// bounded counts.
fn content_size(event: &Event) -> usize {
    match event {
        Event::Token(s) | Event::Thinking(s) => s.len(),
        Event::ToolInput { chunk, .. } | Event::ToolOutput { chunk, .. } => chunk.len(),
        Event::ToolArtifact { artifact, .. } => {
            artifact.raw_input.as_ref().map_or(0, String::len)
                + artifact.error.as_ref().map_or(0, String::len)
        }
        _ => 0,
    }
}

impl Event {
    /// Attempt to merge `other` into `self` in place.
    ///
    /// Returns `Ok(())` on success (caller should discard `other` — the
    /// merged state lives in `self`) or `Err(other)` if the two events
    /// are not of a mergeable pair.
    ///
    /// Mergeable pairs:
    /// - `Token + Token` → string concatenation
    /// - `Thinking + Thinking` → string concatenation
    /// - `ToolInput{name=a} + ToolInput{name=a}` → concat chunks (names must match)
    /// - `ToolOutput{name=a} + ToolOutput{name=a}` → concat chunks
    /// - `Usage + Usage` → replace with latest
    ///
    /// All other pairs (including same-variant with different names, and
    /// any lifecycle events) are rejected so the consumer sees each one.
    pub fn try_merge(&mut self, other: Event) -> Result<(), Event> {
        match (self, other) {
            (Event::Token(a), Event::Token(b)) => {
                a.push_str(&b);
                Ok(())
            }
            (Event::Thinking(a), Event::Thinking(b)) => {
                a.push_str(&b);
                Ok(())
            }
            (
                Event::ToolInput {
                    name: a_name,
                    chunk: a_chunk,
                },
                Event::ToolInput {
                    name: b_name,
                    chunk: b_chunk,
                },
            ) if *a_name == b_name => {
                a_chunk.push_str(&b_chunk);
                Ok(())
            }
            (
                Event::ToolOutput {
                    name: a_name,
                    chunk: a_chunk,
                },
                Event::ToolOutput {
                    name: b_name,
                    chunk: b_chunk,
                },
            ) if *a_name == b_name => {
                a_chunk.push_str(&b_chunk);
                Ok(())
            }
            (slot @ Event::Usage(_), other @ Event::Usage(_)) => {
                *slot = other;
                Ok(())
            }
            (slot @ Event::ContextUsage(_), other @ Event::ContextUsage(_)) => {
                *slot = other;
                Ok(())
            }
            // Reconstruct `other` with all its fields and return.
            (_, other) => Err(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::Usage;

    #[test]
    fn merge_token_concatenates() {
        let mut a = Event::Token("hel".into());
        assert!(a.try_merge(Event::Token("lo".into())).is_ok());
        match a {
            Event::Token(s) => assert_eq!(s, "hello"),
            _ => panic!("expected Token"),
        }
    }

    #[test]
    fn merge_thinking_concatenates() {
        let mut a = Event::Thinking("abc".into());
        assert!(a.try_merge(Event::Thinking("def".into())).is_ok());
        match a {
            Event::Thinking(s) => assert_eq!(s, "abcdef"),
            _ => panic!("expected Thinking"),
        }
    }

    #[test]
    fn merge_tool_input_same_name() {
        let mut a = Event::ToolInput {
            name: "Write".into(),
            chunk: "hello ".into(),
        };
        assert!(
            a.try_merge(Event::ToolInput {
                name: "Write".into(),
                chunk: "world".into()
            })
            .is_ok()
        );
        match a {
            Event::ToolInput { chunk, .. } => assert_eq!(chunk, "hello world"),
            _ => panic!("expected ToolInput"),
        }
    }

    #[test]
    fn merge_tool_input_different_names_fails() {
        let mut a = Event::ToolInput {
            name: "Write".into(),
            chunk: "x".into(),
        };
        let b = Event::ToolInput {
            name: "Edit".into(),
            chunk: "y".into(),
        };
        let err = a.try_merge(b).unwrap_err();
        match err {
            Event::ToolInput { name, .. } => assert_eq!(name, "Edit"),
            _ => panic!("expected ToolInput returned"),
        }
    }

    #[test]
    fn merge_token_with_thinking_fails() {
        let mut a = Event::Token("a".into());
        let err = a.try_merge(Event::Thinking("b".into())).unwrap_err();
        assert!(matches!(err, Event::Thinking(_)));
    }

    #[test]
    fn merge_usage_replaces() {
        let mut a = Event::Usage(Usage {
            input_tokens: 100,
            output_tokens: 10,
            cache_read: None,
            cache_write: None,
        });
        assert!(
            a.try_merge(Event::Usage(Usage {
                input_tokens: 100,
                output_tokens: 25,
                cache_read: None,
                cache_write: None,
            }))
            .is_ok()
        );
        match a {
            Event::Usage(u) => assert_eq!(u.output_tokens, 25),
            _ => panic!("expected Usage"),
        }
    }

    #[test]
    fn merge_lifecycle_events_fails() {
        let mut a = Event::ToolStart {
            name: "Write".into(),
            summary: "a".into(),
        };
        let b = Event::ToolStart {
            name: "Write".into(),
            summary: "b".into(),
        };
        assert!(a.try_merge(b).is_err());
    }

    #[tokio::test]
    async fn channel_basic_send_recv() {
        let (tx, mut rx) = channel();
        tx.send(Event::Token("hi".into())).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert!(matches!(evt, Event::Token(ref s) if s == "hi"));
    }

    #[tokio::test]
    async fn channel_coalesces_tokens() {
        let (tx, mut rx) = channel();
        // Send many tokens before the receiver wakes up — they should all
        // merge into one event.
        for i in 0..100 {
            tx.send(Event::Token(i.to_string())).await.unwrap();
        }
        let first = rx.recv().await.unwrap();
        let combined: String = (0..100).map(|i| i.to_string()).collect();
        match first {
            Event::Token(s) => assert_eq!(s, combined),
            _ => panic!("expected Token"),
        }
        // Nothing else should be queued.
        assert!(rx.try_recv().is_none());
    }

    #[tokio::test]
    async fn channel_preserves_unmergeable_order() {
        let (tx, mut rx) = channel();
        tx.send(Event::Token("hi".into())).await.unwrap();
        tx.send(Event::ToolStart {
            name: "Write".into(),
            summary: "a".into(),
        })
        .await
        .unwrap();
        tx.send(Event::Token("bye".into())).await.unwrap();

        assert!(matches!(rx.recv().await, Some(Event::Token(_))));
        assert!(matches!(rx.recv().await, Some(Event::ToolStart { .. })));
        assert!(matches!(rx.recv().await, Some(Event::Token(_))));
    }

    #[tokio::test]
    async fn channel_closes_on_sender_drop() {
        let (tx, mut rx) = channel();
        tx.send(Event::Token("one".into())).await.unwrap();
        drop(tx);
        assert!(matches!(rx.recv().await, Some(Event::Token(_))));
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn channel_send_fails_after_receiver_drop() {
        let (tx, rx) = channel();
        drop(rx);
        let err = tx.send(Event::Token("x".into())).await.unwrap_err();
        assert!(matches!(err.0, Event::Token(_)));
    }

    #[tokio::test]
    async fn channel_soft_cap_backpressures_on_content() {
        // 256-byte cap, 128 hard events.
        let (tx, mut rx) = channel_with(256, 128);

        // First send: fits.
        tx.send(Event::Token("a".repeat(200))).await.unwrap();

        // Second send: merges to 400 bytes, over the cap. Start a sender
        // task that will block until we consume.
        let tx2 = tx.clone();
        let send_fut = tokio::spawn(async move {
            tx2.send(Event::Token("b".repeat(200))).await.unwrap();
        });

        // Give the task a moment to reach the await.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!send_fut.is_finished(), "sender should be blocked");

        // Drain — this wakes the sender.
        let _ = rx.recv().await.unwrap();
        send_fut.await.unwrap();
    }

    #[tokio::test]
    async fn channel_hard_cap_blocks_unmergeable_flood() {
        let (tx, mut rx) = channel_with(usize::MAX, 2);

        // Fill with unmergeable events.
        tx.send(Event::ToolStart {
            name: "A".into(),
            summary: "".into(),
        })
        .await
        .unwrap();
        tx.send(Event::ToolStart {
            name: "B".into(),
            summary: "".into(),
        })
        .await
        .unwrap();

        // Third should block.
        let tx2 = tx.clone();
        let send_fut = tokio::spawn(async move {
            tx2.send(Event::ToolStart {
                name: "C".into(),
                summary: "".into(),
            })
            .await
            .unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!send_fut.is_finished());

        rx.recv().await.unwrap();
        send_fut.await.unwrap();
    }

    #[tokio::test]
    async fn content_size_accounts_only_streaming_payload() {
        assert_eq!(content_size(&Event::Token("12345".into())), 5);
        assert_eq!(content_size(&Event::Thinking("ab".into())), 2);
        assert_eq!(
            content_size(&Event::ToolInput {
                name: "Write".into(),
                chunk: "xyz".into()
            }),
            3
        );
        assert_eq!(content_size(&Event::AgentDone), 0);
        assert_eq!(
            content_size(&Event::ToolStart {
                name: "Write".into(),
                summary: "big summary".into()
            }),
            0
        );
    }
}
