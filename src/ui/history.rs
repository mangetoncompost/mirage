//! Lock-light time-series store feeding the dashboard's history widgets
//! (the cumulative-upload graph and the per-row speed meter's stable scale).
//!
//! Same discipline as [`super::view::ROWS`]: a `std::sync::Mutex` touched once
//! per render tick from `render_once` (never from the key thread, never from
//! `build_frame`). `build_frame` stays pure — it only ever sees a POD copy of
//! the samples that `render_once` already pulled into the `Frame`.
//!
//! Sampling is keyed on whole seconds since `STARTED` (a `OnceCell`, so the
//! origin is fixed for the process), which keeps the X axis stable and
//! deterministic regardless of the exact tick the sample landed on.

use once_cell::sync::Lazy;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Retained samples. At one sample per ~400 ms tick this is ~4 min of raw
/// history; past that we downsample (drop every other sample on insert) so a
/// multi-hour session keeps a bounded buffer while still spanning the whole run.
const CAP: usize = 600;

/// `(seconds_since_started, cumulative_uploaded_bytes)` samples, oldest first.
static UPLOADED_HIST: Lazy<Mutex<VecDeque<(i64, u64)>>> =
    Lazy::new(|| Mutex::new(VecDeque::with_capacity(CAP)));

/// Highest instantaneous *single-torrent* upload speed (bytes/s) seen this
/// session. Used as a stable denominator for the per-row speed meter so the bars
/// don't rescale (and visually "jump") every time the fastest torrent pauses or
/// finishes. A per-row scale (not the summed total) keeps a busy row's bar full
/// even when there are many torrents.
static SESSION_PEAK_SPEED: AtomicU64 = AtomicU64::new(0);

/// Push one tick's sample. Called once per render tick from `render_once`.
/// `secs` is whole seconds since `STARTED`; `total_up` is the summed uploaded
/// bytes across all torrents (for the graph); `row_peak` is this frame's fastest
/// single-torrent upload speed (for the meter scale).
pub fn push_sample(secs: i64, total_up: u64, row_peak: u64) {
    // Stable peak for the meter scale (monotonic; never shrinks within a session).
    SESSION_PEAK_SPEED.fetch_max(row_peak, Ordering::Relaxed);

    let mut q = match UPLOADED_HIST.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };

    // Coalesce: at most one sample per whole second so SIGWINCH/SIGCONT repaints
    // (which fire an extra render_once on the same second) don't double-record.
    if let Some(&(last_s, _)) = q.back()
        && last_s == secs
    {
        q.pop_back();
    }
    q.push_back((secs, total_up));

    // When full, halve the resolution in place: keep every other sample. The
    // series still spans the whole session, just coarser — and because each
    // sample carries its own `secs`, the graph's X axis stays correct regardless
    // of how many downsampling passes have happened.
    if q.len() > CAP {
        let kept: VecDeque<(i64, u64)> =
            q.iter().enumerate().filter(|(i, _)| i % 2 == 0).map(|(_, s)| *s).collect();
        *q = kept;
    }
}

/// POD copy of the current samples for one frame (oldest first). Copied into the
/// `Frame` by `render_once`; `build_frame` reads only this copy.
pub fn samples() -> Vec<(i64, u64)> {
    let q = match UPLOADED_HIST.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    q.iter().copied().collect()
}

/// Stable denominator (bytes/s) for the per-row speed meter. Never zero so
/// callers can divide unconditionally.
pub fn session_peak() -> u64 {
    SESSION_PEAK_SPEED.load(Ordering::Relaxed).max(1)
}
