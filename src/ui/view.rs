//! Active-view + row-selection state for the TTY dashboard's 10-tab UI.
//!
//! The key reader runs on a detached OS thread (see [`super::keys`]) while the
//! frame is built on a tokio task (see [`super::mod::run`]); they never share a
//! lock, so the bridge between "user pressed a key" and "render this view" is a
//! set of process-global atomics.
//!
//! Overlay state (Help, Palette, Detail) has been moved to [`super::overlay`]
//! so it can be reused independently of the tab index.

use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use once_cell::sync::Lazy;

/// Which of the ten tabs is active. Keep in sync with `TAB_LABELS` in
/// `render.rs`.
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
    Ratio = 9, // F1.1 cumulative-upload graph (10th tab, key `0`)
}

impl View {
    pub fn from_index(i: usize) -> View {
        use View::*;
        [
            Dashboard, Torrents, Trackers, Speeds, Client, Schedule, Network, Logs, Config, Ratio,
        ][i.min(9)]
    }
}

/// Selected tab index (0..=9).
static ACTIVE_VIEW: AtomicUsize = AtomicUsize::new(0);
/// Selected row within list-style views (Dashboard/Torrents/Trackers).
static SEL: AtomicUsize = AtomicUsize::new(0);
/// Multi-selected torrent info-hashes (F2.1). Empty = no batch selection.
/// Cleared on tab switch (set_view) so a stale set never targets dead torrents.
static MARKED: Lazy<Mutex<HashSet<[u8; 20]>>> = Lazy::new(|| Mutex::new(HashSet::new()));

/// Delegates to the unified overlay system in [`super::overlay`].
pub fn help_open() -> bool {
    super::overlay::help_open()
}
pub fn toggle_help() {
    super::overlay::toggle_help();
}

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

/// Key thread → info-hash of the currently selected row, if any. Skips the
/// all-zero sentinel a busy (mid-announce) row publishes, so a command never
/// targets a placeholder.
pub fn selected_hash() -> Option<[u8; 20]> {
    let g = ROWS.lock().ok()?;
    g.get(sel()).copied().filter(|h| *h != [0u8; 20])
}

pub fn active_view() -> View {
    View::from_index(ACTIVE_VIEW.load(Ordering::Relaxed))
}

/// Set the active view by 0-based index (clamped to 0..=9). Resets row selection
/// so switching tabs never lands on a stale out-of-range row. Also closes any
/// open overlay so leaving a tab doesn't strand the user in a stale overlay.
pub fn set_view(i: usize) {
    ACTIVE_VIEW.store(i.min(9), Ordering::Relaxed);
    SEL.store(0, Ordering::Relaxed);
    // Close the detail overlay on tab switch (palette/help the key handler closes).
    super::overlay::close_detail();
    // Clear multi-selection so a stale set never targets dead torrents after a tab switch.
    if let Ok(mut g) = MARKED.lock() {
        g.clear();
    }
}

// --- Multi-select helpers (F2.1) ---------------------------------------------

/// Toggle the mark on a torrent. Returns the new mark state.
pub fn toggle_mark(hash: [u8; 20]) -> bool {
    if let Ok(mut g) = MARKED.lock() {
        if g.contains(&hash) {
            g.remove(&hash);
            false
        } else {
            g.insert(hash);
            true
        }
    } else {
        false
    }
}

/// Mark all currently visible torrents (from ROWS).
pub fn mark_all() {
    let rows = ROWS.lock().ok().map(|g| g.clone()).unwrap_or_default();
    if let Ok(mut g) = MARKED.lock() {
        for h in rows.iter().filter(|h| **h != [0u8; 20]) {
            g.insert(*h);
        }
    }
}

/// Clear all marks.
pub fn clear_marks() {
    if let Ok(mut g) = MARKED.lock() {
        g.clear();
    }
}

/// Returns the set of currently marked hashes (POD copy for the snapshot).
pub fn marked_set() -> HashSet<[u8; 20]> {
    MARKED.lock().map(|g| g.clone()).unwrap_or_default()
}

/// Whether a given hash is marked.
pub fn is_marked(hash: &[u8; 20]) -> bool {
    MARKED.lock().map(|g| g.contains(hash)).unwrap_or(false)
}

/// Returns the marked hashes as a vec, or falls back to `selected_hash()` if
/// the marked set is empty. The key handler uses this for f/x batch actions.
pub fn marked_or_selected() -> Vec<[u8; 20]> {
    let g = MARKED.lock().ok();
    let marks: Vec<_> = g.as_ref().map(|s| s.iter().copied().collect()).unwrap_or_default();
    if marks.is_empty() {
        selected_hash().into_iter().collect()
    } else {
        marks
    }
}

/// Cycle to the next/previous tab (wraps around). `delta` is +1 or -1.
pub fn cycle_view(delta: i32) {
    let cur = ACTIVE_VIEW.load(Ordering::Relaxed) as i32;
    let next = (cur + delta).rem_euclid(10) as usize;
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
