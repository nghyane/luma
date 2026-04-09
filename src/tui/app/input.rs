/// Stdin reader — blocking thread that reads termina events.
use crate::event::Event;
use termina::event::KeyEventKind;
use termina::EventReader;
use tokio::sync::mpsc;

/// Read terminal events in a blocking loop, sending parsed Events.
///
/// Filters out events the app never handles: key Release/Repeat, and
/// escape sequence responses (Csi, Osc, Dcs) that termina exposes but
/// luma does not use.
pub fn read_stdin_loop(reader: EventReader, tx: mpsc::Sender<Event>) {
    loop {
        let Ok(raw) = reader.read(|_| true) else {
            return;
        };
        match raw {
            // Only forward key-press events (ignore Release/Repeat).
            termina::Event::Key(ref k) if k.kind != KeyEventKind::Press => continue,
            // Drop escape sequence responses — app does not use them.
            termina::Event::Csi(_) | termina::Event::Osc(_) | termina::Event::Dcs(_) => continue,
            _ => {}
        }
        if tx.blocking_send(Event::Term(raw)).is_err() {
            return;
        }
    }
}
