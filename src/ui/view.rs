//! Active-view + row-selection state for the TTY dashboard's 9-tab UI.
//!
//! The key reader runs on a detached OS thread (see [`super::keys`]) while the
//! frame is built on a tokio task (see [`super::mod::run`]); they never share a
//! lock, so the bridge between "user pressed a key" and "render this view" is two
//! process-global atomics. This mirrors the egui side's `View` enum so the
//! terminal and the native window stay identical.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Which of the nine tabs is active. Keep in sync with the tab strip labels in
/// `render.rs` and the egui `View` enum.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(usize)]
pub enum View {
    Dashboard = 0,
    Torrents = 1,
    Trackers = 2,
    Speeds = 3,
    Client = 4,
    Schedule = 5,
    Network = 6,
    Logs = 7,
    Config = 8,
}

impl View {
    pub fn from_index(i: usize) -> View {
        use View::*;
        [
            Dashboard, Torrents, Trackers, Speeds, Client, Schedule, Network, Logs, Config,
        ][i.min(8)]
    }
}

/// Selected tab index (0..=8).
static ACTIVE_VIEW: AtomicUsize = AtomicUsize::new(0);
/// Selected row within list-style views (Dashboard/Torrents/Trackers).
static SEL: AtomicUsize = AtomicUsize::new(0);
/// The current frame's torrent info-hashes, in display order. Published by the
/// render loop each tick so the (lock-free) key thread can resolve the selected
/// row into a `control::Cmd` target without touching the async `TORRENTS` lock.
static ROWS: Mutex<Vec<[u8; 20]>> = Mutex::new(Vec::new());

/// Render loop → publish the current row identities (called once per tick).
pub fn set_rows(hashes: Vec<[u8; 20]>) {
    if let Ok(mut g) = ROWS.lock() {
        *g = hashes;
    }
}

/// Key thread → number of selectable rows right now.
pub fn row_count() -> usize {
    ROWS.lock().map(|g| g.len()).unwrap_or(0)
}

/// Key thread → info-hash of the currently selected row, if any.
pub fn selected_hash() -> Option<[u8; 20]> {
    let g = ROWS.lock().ok()?;
    g.get(sel()).copied()
}

pub fn active_view() -> View {
    View::from_index(ACTIVE_VIEW.load(Ordering::Relaxed))
}

/// Set the active view by 0-based index (clamped to 0..=8). Resets row selection
/// so switching tabs never lands on a stale out-of-range row.
pub fn set_view(i: usize) {
    ACTIVE_VIEW.store(i.min(8), Ordering::Relaxed);
    SEL.store(0, Ordering::Relaxed);
}

/// Cycle to the next/previous tab (wraps around). `delta` is +1 or -1.
pub fn cycle_view(delta: i32) {
    let cur = ACTIVE_VIEW.load(Ordering::Relaxed) as i32;
    let next = (cur + delta).rem_euclid(9) as usize;
    set_view(next);
}

pub fn sel() -> usize {
    SEL.load(Ordering::Relaxed)
}

/// Move the selection by `delta`, clamped to `0..max` (max = row count). A `max`
/// of 0 keeps selection at 0.
pub fn bump_sel(delta: i32, max: usize) {
    if max == 0 {
        SEL.store(0, Ordering::Relaxed);
        return;
    }
    let cur = SEL.load(Ordering::Relaxed) as i32;
    let next = (cur + delta).clamp(0, max as i32 - 1) as usize;
    SEL.store(next, Ordering::Relaxed);
}

/// Clamp the stored selection into `0..max` (called by the render loop after it
/// learns the live row count, so a shrunk list never points past the end).
pub fn clamp_sel(max: usize) {
    if max == 0 {
        SEL.store(0, Ordering::Relaxed);
    } else {
        let cur = SEL.load(Ordering::Relaxed);
        if cur >= max {
            SEL.store(max - 1, Ordering::Relaxed);
        }
    }
}
