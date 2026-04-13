/// Stdin reader — blocking thread that reads termina events.
use crate::event::Event;
use crate::event_bus::Sender;
use termina::EventReader;
use termina::event::KeyEventKind;

/// Read terminal events in a blocking loop, sending parsed Events.
///
/// Filters out events the app never handles: key Release/Repeat, and
/// escape sequence responses (Csi, Osc, Dcs) that termina exposes but
/// luma does not use.
pub fn read_stdin_loop(reader: EventReader, tx: Sender) {
    loop {
        let Ok(raw) = reader.read(|_| true) else {
            return;
        };
        match raw {
            // Only forward key-press events (ignore Release/Repeat).
            termina::Event::Key(ref k) if k.kind != KeyEventKind::Press => continue,
            // Drop NUL char key events (Windows reports bare modifier presses this way).
            termina::Event::Key(ref k) if matches!(k.code, termina::event::KeyCode::Char('\0')) => {
                continue;
            }
            // Drop escape sequence responses — app does not use them.
            termina::Event::Csi(_) | termina::Event::Osc(_) | termina::Event::Dcs(_) => continue,
            _ => {}
        }
        if tx.blocking_send(Event::Term(raw)).is_err() {
            return;
        }
    }
}
