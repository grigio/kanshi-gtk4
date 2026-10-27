pub mod countdown;
pub mod layout_cards;
pub mod output_row;
pub mod preview;
pub mod window;

use std::sync::mpsc;
use std::time::Duration;

/// Run blocking work on a thread and deliver the result on the GTK main loop.
pub fn spawn_background<T, F, G>(work: F, done: G)
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
    G: FnOnce(T) + 'static,
{
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    let mut done = Some(done);
    glib::timeout_add_local(Duration::from_millis(40), move || match rx.try_recv() {
        Ok(value) => {
            if let Some(done) = done.take() {
                done(value);
            }
            glib::ControlFlow::Break
        }
        Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
        Err(mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
    });
}

use gtk4::glib;
