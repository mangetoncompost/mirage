//! Generic overlay system for the super-shell dashboard.
//!
//! An overlay replaces the per-view body for one frame. The active overlay is
//! resolved in `render_once` (from the process-global atomics below) and passed
//! into `build_frame` as a plain enum value — `build_frame` stays pure and only
//! *renders* the resolved state, never reads atomics itself.
//!
//! Currently defined overlays:
//! - `None`    — normal view body
//! - `Help`    — the `?` help card (migrated from the old `help_open()` bool)
//! - `Palette` — the fuzzy command palette (F3.1; INFRA-C wires its keys)
//! - `Detail`  — per-torrent info + wire sub-view (F3.2)

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

/// Which overlay (if any) is currently open. Resolved in `render_once`, passed
/// to `build_frame` as a `Overlay` value — never read inside `build_frame`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Overlay {
    None,
    Help,
    Palette,
    /// Detail card for a specific torrent (identified by a compact 20-byte
    /// info-hash stored separately in `DETAIL_HASH`).
    Detail,
}

// --- process-global overlay state (one atomic per overlay type) ---------------

/// Help overlay open?
static HELP: AtomicBool = AtomicBool::new(false);
/// Palette overlay open?
static PALETTE: AtomicBool = AtomicBool::new(false);
/// Detail overlay open?
static DETAIL: AtomicBool = AtomicBool::new(false);
/// Sub-tab index inside the Detail overlay (0=info, 1=wire).
pub static DETAIL_SUB: AtomicU8 = AtomicU8::new(0);

// DETAIL_HASH: the info-hash of the torrent whose detail card is open.
// A Mutex<Option<[u8;20]>> — same pattern as ROWS in view.rs.
use std::sync::Mutex;
pub static DETAIL_HASH: Mutex<Option<[u8; 20]>> = Mutex::new(None);

/// Resolve the active overlay for this frame. Priority: Help > Palette > Detail.
/// Called once per tick from `render_once`; result is passed into `build_frame`.
pub fn active() -> Overlay {
    if HELP.load(Ordering::Relaxed) {
        Overlay::Help
    } else if PALETTE.load(Ordering::Relaxed) {
        Overlay::Palette
    } else if DETAIL.load(Ordering::Relaxed) {
        Overlay::Detail
    } else {
        Overlay::None
    }
}

// --- Help --------------------------------------------------------------------

pub fn help_open() -> bool {
    HELP.load(Ordering::Relaxed)
}
pub fn toggle_help() {
    HELP.fetch_xor(true, Ordering::Relaxed);
}

// --- Palette -----------------------------------------------------------------

pub fn palette_open() -> bool {
    PALETTE.load(Ordering::Relaxed)
}
pub fn open_palette() {
    PALETTE.store(true, Ordering::Relaxed);
}
pub fn close_palette() {
    PALETTE.store(false, Ordering::Relaxed);
    // Clear the search buffer on close.
    if let Ok(mut g) = PALETTE_BUF.lock() {
        g.clear();
    }
    PALETTE_SEL.store(0, Ordering::Relaxed);
}

// --- INFRA-C: palette capture buffer -----------------------------------------

/// The user's in-progress search query inside the palette overlay.
pub static PALETTE_BUF: Mutex<String> = Mutex::new(String::new());
/// Selected item index within the filtered palette list.
pub static PALETTE_SEL: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Type a character into the palette buffer.
pub fn palette_push(ch: char) {
    if let Ok(mut g) = PALETTE_BUF.lock() {
        g.push(ch);
    }
    PALETTE_SEL.store(0, Ordering::Relaxed);
}
/// Backspace in the palette buffer.
pub fn palette_pop() {
    if let Ok(mut g) = PALETTE_BUF.lock() {
        g.pop();
    }
    PALETTE_SEL.store(0, Ordering::Relaxed);
}
/// Copy the current palette buffer (for rendering).
pub fn palette_query() -> String {
    PALETTE_BUF.lock().map(|g| g.clone()).unwrap_or_default()
}
/// Bump the palette selection by delta within [0, max).
pub fn palette_bump_sel(delta: i32, max: usize) {
    if max == 0 {
        PALETTE_SEL.store(0, Ordering::Relaxed);
        return;
    }
    let cur = PALETTE_SEL.load(Ordering::Relaxed) as i32;
    let next = (cur + delta).rem_euclid(max as i32) as usize;
    PALETTE_SEL.store(next, Ordering::Relaxed);
}

// --- Detail ------------------------------------------------------------------

pub fn detail_open() -> bool {
    DETAIL.load(Ordering::Relaxed)
}
pub fn open_detail(hash: [u8; 20]) {
    if let Ok(mut g) = DETAIL_HASH.lock() {
        *g = Some(hash);
    }
    DETAIL_SUB.store(0, Ordering::Relaxed);
    DETAIL.store(true, Ordering::Relaxed);
}
pub fn close_detail() {
    DETAIL.store(false, Ordering::Relaxed);
    if let Ok(mut g) = DETAIL_HASH.lock() {
        *g = None;
    }
}
pub fn detail_hash() -> Option<[u8; 20]> {
    *DETAIL_HASH.lock().ok()?
}
#[allow(dead_code)]
pub fn cycle_detail_sub() {
    let cur = DETAIL_SUB.load(Ordering::Relaxed);
    DETAIL_SUB.store(if cur == 0 { 1 } else { 0 }, Ordering::Relaxed);
}

// --- Milestone celebration (F1.3) --------------------------------------------
// Ratio milestones (uploaded / total_length, stored as units of 0.5, so "2.0"
// is stored as 4): 1.0, 1.5, 2.0, 3.0, 5.0, 10.0.
// LAST_MILESTONE holds the highest milestone (×10) already celebrated, so each
// fires exactly once. CELEBRATE_UNTIL_TICK is the render-tick after which the
// flash stops.
static LAST_MILESTONE: AtomicU64 = AtomicU64::new(0);
static CELEBRATE_UNTIL_TICK: AtomicU64 = AtomicU64::new(0);
static CELEBRATE_LABEL: Mutex<String> = Mutex::new(String::new());

/// The milestones in tenths (10 = 1.0×, 15 = 1.5×, …).
const MILESTONES_TENTHS: &[u64] = &[10, 15, 20, 30, 50, 100];

/// Check if the global ratio crossed a new milestone. `total_up` is the summed
/// uploaded bytes, `total_len` is the summed torrent lengths (only seeding).
/// Must be called from `render_once` once per tick. Returns true if a new
/// milestone was just crossed (caller should emit a Milestone event too).
pub fn check_milestone(total_up: u64, total_len: u64, tick: u64) -> bool {
    if total_len == 0 {
        return false;
    }
    let ratio_tenths = (total_up * 10) / total_len;
    let crossed = MILESTONES_TENTHS
        .iter()
        .rev()
        .find(|&&m| ratio_tenths >= m && m > LAST_MILESTONE.load(Ordering::Relaxed));
    if let Some(&m) = crossed {
        LAST_MILESTONE.store(m, Ordering::Relaxed);
        CELEBRATE_UNTIL_TICK.store(tick + 10, Ordering::Relaxed); // ~4 s of flash
        if let Ok(mut g) = CELEBRATE_LABEL.lock() {
            *g = format!("ratio {:.1}× !", m as f64 / 10.0);
        }
        true
    } else {
        false
    }
}

/// True if we are currently in a celebration window.
pub fn celebrating(tick: u64) -> bool {
    tick < CELEBRATE_UNTIL_TICK.load(Ordering::Relaxed)
}

/// The celebration label (e.g. "ratio 2.0× !").
pub fn celebration_label() -> String {
    CELEBRATE_LABEL
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default()
}
