//! Global, lock-light event ring buffer that feeds the dashboard's "recent
//! events" pane. Pushed to from explicit, typed `emit()` calls at the handful
//! of meaningful announce/watcher sites (NOT from a tracing layer: scraping log
//! strings is fragile and `trace!("scheduler tick")` would flood the ring).
//!
//! Uses `std::sync::Mutex` (not tokio) so the watcher's blocking notify thread
//! can push without a runtime handle. The lock is never held across `.await`.

use once_cell::sync::Lazy;
use std::collections::VecDeque;
use std::sync::Mutex;

/// Cap on retained events. ~50 fits a typical feed pane.
const CAP: usize = 50;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    ConnectOk,    // UDP connect handshake succeeded
    ConnectFail,  // resolve / connect / build failed
    AnnounceSent, // announce request dispatched
    PeersUpdated, // tracker returned seeders/leechers
    UploadTick,   // fake upload accounted this announce
    Added,        // torrent added by watcher
    Removed,      // torrent removed by watcher
    Error,        // tracker failure_reason / bad response / transport error
}

impl EventKind {
    /// A short glyph used in the feed pane. UTF-8 variant.
    pub fn glyph(self) -> &'static str {
        match self {
            EventKind::ConnectOk => "🔌",
            EventKind::ConnectFail => "✖",
            EventKind::AnnounceSent => "📡",
            EventKind::PeersUpdated => "🌱",
            EventKind::UploadTick => "⬆",
            EventKind::Added => "➕",
            EventKind::Removed => "➖",
            EventKind::Error => "⚠",
        }
    }

    /// ASCII fallback glyph for non-UTF-8 terminals.
    pub fn glyph_ascii(self) -> &'static str {
        match self {
            EventKind::ConnectOk => "+",
            EventKind::ConnectFail => "x",
            EventKind::AnnounceSent => ">",
            EventKind::PeersUpdated => "*",
            EventKind::UploadTick => "^",
            EventKind::Added => "+",
            EventKind::Removed => "-",
            EventKind::Error => "!",
        }
    }
}

#[derive(Clone)]
pub struct UiEvent {
    pub at: chrono::DateTime<chrono::Utc>,
    pub kind: EventKind,
    pub torrent: Box<str>,
    pub msg: Box<str>,
}

static EVENTS: Lazy<Mutex<VecDeque<UiEvent>>> =
    Lazy::new(|| Mutex::new(VecDeque::with_capacity(CAP)));

/// Push an event. Cheap, sync, callable from async or blocking threads.
/// The lock is poison-tolerant so a panicked emitter never wedges the feed.
pub fn emit(kind: EventKind, torrent: &str, msg: impl Into<Box<str>>) {
    let mut q = match EVENTS.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if q.len() == CAP {
        q.pop_front();
    }
    q.push_back(UiEvent {
        at: chrono::Utc::now(),
        kind,
        torrent: torrent.into(),
        msg: msg.into(),
    });
}

/// Last `n` events, oldest-first (ready to print top-to-bottom, newest at bottom).
pub fn snapshot(n: usize) -> Vec<UiEvent> {
    let q = match EVENTS.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let len = q.len();
    let start = len.saturating_sub(n);
    q.iter().skip(start).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emit_each_kind_lands_in_ring() {
        let kinds = [
            EventKind::ConnectOk,
            EventKind::ConnectFail,
            EventKind::AnnounceSent,
            EventKind::PeersUpdated,
            EventKind::UploadTick,
            EventKind::Added,
            EventKind::Removed,
            EventKind::Error,
        ];
        for k in kinds {
            emit(k, "t", "m");
        }
        // EVENTS is process-global; assert on the last N rather than total length
        // so concurrent ring tests don't observe each other's pushes.
        let snap = snapshot(kinds.len());
        assert_eq!(snap.len(), kinds.len());
        assert!(snap.iter().all(|e| &*e.torrent == "t" && &*e.msg == "m"));
    }
}
