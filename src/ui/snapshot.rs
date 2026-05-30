//! Lock-safe snapshotting of the shared global state (TORRENTS / CLIENT /
//! STARTED) into plain-old-data structs the renderer can use without holding
//! any lock during the stdout write.
//!
//! DEADLOCK HAZARD (verified against the tree): the scheduler holds the inner
//! per-torrent `Mutex` across a network announce — `m.lock().await` in
//! scheduler.rs stays held through `tracker::announce(&mut t, ..).await`, which
//! does HTTP/UDP I/O lasting seconds. `announce_started`, `announce_stopped`
//! and the watcher do the same. The outer `TORRENTS.read()` is held alongside.
//!
//! Therefore the renderer:
//!   1. takes `TORRENTS.read().await` — read-vs-read does not contend, but a
//!      queued watcher WRITE (add/remove) can briefly delay this read; that is
//!      bounded because no announce holds `TORRENTS.read()` (see scheduler.rs);
//!   2. uses `try_lock()` on each inner `Mutex`, NEVER `lock().await` — a torrent
//!      mid-announce yields a "busy" placeholder row for that frame;
//!   3. copies each torrent into a POD struct and drops every lock before any
//!      stdout write.
//!
//! `CLIENT` is snapshotted via `try_read()` for the same reason: a queued
//! CLIENT writer (key-renewer or `k` re-init) must not block the render loop.

use std::collections::HashSet;

use crate::ui::events::UiEvent;

/// POD copy of one torrent for one frame. No borrows, no locks.
#[derive(Clone)]
pub struct TorrentView {
    pub name: String,
    pub info_hash: [u8; 20], // for resolving SEL -> control::Cmd target
    pub seeders: u16,
    pub leechers: u16,
    pub up_speed: u32, // bytes/s (next_upload_speed)
    pub uploaded: u64, // total bytes
    pub length: u64,   // total torrent size (bytes) — ratio denominator + detail card
    #[allow(dead_code)]
    pub left: u64, // declared bytes left (0 when seeding) — future detail
    #[allow(dead_code)]
    pub interval: u64, // current announce interval (s) — for the Schedule ledger / detail card
    pub secs_to_announce: u64, // interval - elapsed, saturating
    pub error_count: u16,
    pub busy: bool,          // true => mid-announce, try_lock failed (placeholder row)
    pub downloading: bool,   // true => still in the simulated download phase
    pub schedule_reason: u8, // why the current cadence (see torrent::ScheduleReason) — F3.3 ledger
    pub dl_percent: u8,      // 0..=100 download progress
    #[allow(dead_code)]
    pub downloaded: u64, // declared downloaded bytes (display interpolation)
    pub urls: Vec<String>,   // announce URL(s) for the trackers view
}

#[derive(Clone)]
pub struct ClientView {
    pub name: String,
    pub peer_id: String,
    pub key: u32, // fake_torrent_client::Client.key is u32
    #[allow(dead_code)]
    pub user_agent: String,
}

/// Everything render.rs needs for one frame. Pure data.
pub struct Frame {
    pub client: Option<ClientView>,
    pub started: chrono::DateTime<chrono::Utc>,
    pub now: chrono::DateTime<chrono::Utc>,
    pub rows: Vec<TorrentView>,
    pub feed: Vec<UiEvent>,
    /// How many rows the feed pane should occupy. The dashboard pads the pane
    /// with blank rows up to this so the box always bottom-anchors to the window
    /// (no empty terminal rows below the board).
    pub feed_cap: usize,
    /// Terminal height in rows. Every view pads its body up to this (minus the
    /// footer) so the box fills the whole window — no blank rows below it.
    pub term_h: usize,
    pub spinner: usize,
    /// `(seconds_since_started, cumulative_uploaded_bytes)` samples, oldest
    /// first — copied from the lock-light history ring by `render_once`. Feeds
    /// the cumulative-upload graph. Empty until the first tick records a sample.
    pub up_history: Vec<(i64, u64)>,
    /// Stable per-session peak of the fastest single-torrent upload speed
    /// (bytes/s), used as the per-row meter bars' denominator so they don't
    /// rescale (visually "jump") when the fastest torrent stops.
    pub frame_peak_speed: u64,
    /// Whether a ratio milestone was just crossed this tick (F1.3 flash).
    pub celebrate: bool,
    /// Label for the celebration footer line (e.g. "ratio 2.0× !").
    pub celebrate_label: String,
    /// The set of multi-selected torrent hashes (F2.1). POD copy snapshotted
    /// once per tick; `build_frame` reads only this copy — never MARKED directly.
    pub marked: HashSet<[u8; 20]>,
}

pub async fn snapshot_torrents() -> Vec<TorrentView> {
    let list = crate::TORRENTS.read().await; // read lock; compatible with announcer
    let mut rows = Vec::with_capacity(list.len());
    for m in list.iter() {
        match m.try_lock() {
            // NEVER .await here
            Ok(t) => {
                let elapsed = t.last_announce.elapsed().as_secs();
                // Display-only projection of download progress between sparse
                // ticks (does NOT mutate dl_state — advance happens only in the
                // scheduler). u128 percent math is safe for huge u64 lengths.
                let projected = if t.is_seeding() {
                    t.length
                } else {
                    let gained = (t.dl_last_tick.elapsed().as_secs_f64() * t.dl_rate as f64) as u64;
                    t.declared_downloaded().saturating_add(gained).min(t.length)
                };
                let dl_percent = if t.length == 0 {
                    100u8
                } else {
                    (projected as u128 * 100 / t.length as u128) as u8
                };
                rows.push(TorrentView {
                    name: t.name.clone(),
                    info_hash: t.info_hash,
                    seeders: t.seeders,
                    leechers: t.leechers,
                    // Instantaneous speed off the same curve that backs the
                    // declared integral — 4 sins, synchronous, no .await. It is 0
                    // while downloading (can_upload() is false), which is correct.
                    up_speed: t
                        .speed_at(t.origin.elapsed().as_secs_f64())
                        .round()
                        .min(u32::MAX as f64) as u32,
                    uploaded: t.uploaded,
                    length: t.length,
                    left: t.declared_left(),
                    interval: t.interval,
                    secs_to_announce: t.interval.saturating_sub(elapsed),
                    error_count: t.error_count,
                    busy: false,
                    downloading: !t.is_seeding(),
                    schedule_reason: t.schedule_reason,
                    dl_percent,
                    downloaded: projected,
                    urls: t.urls.clone(),
                });
            }
            Err(_) => rows.push(TorrentView {
                name: String::from("(announcing…)"),
                info_hash: [0u8; 20],
                seeders: 0,
                leechers: 0,
                up_speed: 0,
                uploaded: 0,
                length: 0,
                left: 0,
                interval: 0,
                secs_to_announce: 0,
                error_count: 0,
                busy: true,
                downloading: false,
                schedule_reason: 0,
                dl_percent: 0,
                downloaded: 0,
                urls: Vec::new(),
            }),
        }
    } // outer read lock dropped here
    rows
}

pub async fn snapshot_client() -> Option<ClientView> {
    // Use try_read so a queued CLIENT writer (key-renewer timer or `k` re-init)
    // does not block the render loop. On contention we simply show no client info
    // for this frame — same "busy placeholder" discipline as snapshot_torrents.
    crate::CLIENT.try_read().ok()?.as_ref().map(|c| ClientView {
        name: c.name.clone(),
        peer_id: c.peer_id.clone(),
        key: c.key,
        user_agent: c.user_agent.clone(),
    })
}
