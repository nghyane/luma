/// Stdin reader — blocking thread that reads crossterm events.
use crate::event::Event;
use crossterm::event::{self as ct, KeyEventKind};
use tokio::sync::mpsc;

/// Read terminal events in a blocking loop, sending parsed Events.
pub fn read_stdin_loop(tx: mpsc::Sender<Event>) {
    while let Ok(raw) = ct::read() {
        // Only forward key-press events (ignore Release/Repeat).
        if let ct::Event::Key(ref k) = raw
            && k.kind != KeyEventKind::Press
        {
            continue;
        }

        // blocking_send: input thread is dedicated, so blocking until the
        // event loop drains is fine. Prevents mouse/key events from being
        // silently dropped when the channel is full during heavy streaming.
        if tx.blocking_send(Event::Term(raw)).is_err() {
            return;
        }
    }
}
