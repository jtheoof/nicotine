//! Requests to the config panel from outside its iced runtime: the tray
//! icon's menu (on the tray's own thread) and the daemon's `show` IPC
//! command (sent when Nicotine is launched again while running).
//!
//! The panel installs the channel when it starts and drains it through an
//! iced subscription, so requests run through its normal update logic
//! (e.g. a tray "toggle previews" is persisted exactly like the checkbox).

use iced::futures::channel::mpsc;
use iced::futures::stream::{self, BoxStream, StreamExt};
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiRequest {
    /// Show the panel window, opening it if it's closed.
    Open,
    /// Flip preview-window visibility.
    TogglePreviews,
    /// Quit Nicotine.
    Quit,
}

static SENDER: OnceLock<mpsc::UnboundedSender<UiRequest>> = OnceLock::new();
static RECEIVER: Mutex<Option<mpsc::UnboundedReceiver<UiRequest>>> = Mutex::new(None);

/// Create the channel. Called once by the panel before its runtime starts.
pub fn install() {
    let (tx, rx) = mpsc::unbounded();
    if SENDER.set(tx).is_ok() {
        *RECEIVER.lock().unwrap() = Some(rx);
    }
}

/// Send a request to the panel. Returns false when no panel is running in
/// this process to receive it.
pub fn request(req: UiRequest) -> bool {
    SENDER
        .get()
        .is_some_and(|tx| tx.unbounded_send(req).is_ok())
}

/// The panel's side of the channel, for `Subscription::run`. The receiver
/// can only be taken once; later calls get an empty stream.
pub fn requests() -> BoxStream<'static, UiRequest> {
    match RECEIVER.lock().unwrap().take() {
        Some(rx) => rx.boxed(),
        None => stream::empty().boxed(),
    }
}
