//! Lock-safe snapshotting of the shared global state (TORRENTS / CLIENT /
//! STARTED) into plain-old-data structs the renderer can use without holding
//! any lock during the stdout write.
//!
//! DEADLOCK HAZARD (verified against the tree): the scheduler holds the inner
//! per-torrent `Mutex` across a network announce - `m.lock().await` in
//! scheduler.rs stays held through `tracker::announce(&mut t, ..).await`, which
//! does HTTP/UDP I/O lasting seconds. `announce_started`, `announce_stopped`
//! and the watcher do the same. The outer `TORRENTS.read()` is held alongside.
//!
//! Therefore the renderer:
//!   1. takes `TORRENTS.read().await` - read-vs-read does not contend, but a
//!      queued watcher WRITE (add/remove) can briefly delay this read; that is
//!      bounded because no announce holds `TORRENTS.read()` (see scheduler.rs);
//!   2. uses `try_lock()` on each inner `Mutex`, NEVER `lock().await` - a torrent
//!      mid-announce yields a "busy" placeholder row for that frame;
//!   3. copies each torrent into a POD struct and drops every lock before any
//!      stdout write.
//!
//! `CLIENT` is snapshotted via `try_read()` for the same reason: a queued
//! CLIENT writer (key-renewer or `k` re-init) must not block the render loop.

use std::collections::HashSet;

use crate::torrent::WireCapture;
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
    pub length: u64,   // total torrent size (bytes) - ratio denominator + detail card
    #[allow(dead_code)]
    pub left: u64, // declared bytes left (0 when seeding) - future detail
    #[allow(dead_code)]
    pub interval: u64, // current announce interval (s) - for the Schedule ledger / detail card
    pub secs_to_announce: u64, // interval - elapsed, saturating
    pub error_count: u16,
    pub busy: bool,          // true => mid-announce, try_lock failed (placeholder row)
    pub downloading: bool,   // true => still in the simulated download phase
    pub schedule_reason: u8, // why the current cadence (see torrent::ScheduleReason) - F3.3 ledger
    pub dl_percent: u8,      // 0..=100 download progress
    #[allow(dead_code)]
    pub downloaded: u64, // declared downloaded bytes (display interpolation)
    pub urls: Vec<String>,   // announce URL(s) for the trackers view
    /// Last announce wire snapshot (HTTP or UDP). None until the first announce.
    pub last_wire: Option<WireCapture>,
}

/// One aggregated tracker row for the Trackers tab in rollup mode. Summed over
/// every torrent that announces to the same host. Counters are widened past the
/// per-torrent `u16` so a busy private tracker never overflows the sum.
#[derive(Clone)]
pub struct TrackerAgg {
    pub host: String,
    pub torrents: u32,
    pub up_speed: u64, // summed instantaneous upload (bytes/s)
    pub uploaded: u64, // summed total uploaded (bytes)
    pub seeders: u64,
    pub leechers: u64,
    pub errors: u32,
}

/// Group the snapshotted rows by tracker host into rollups, sorted by summed
/// uploaded bytes descending (the operator's usual "who did I credit most"
/// question). Busy placeholder rows carry no URL and are skipped, matching the
/// per-torrent Trackers view. Pure over the rows: no locks, no I/O. Resolved in
/// `render_once` and stored on `Frame` so `build_frame` only reads the result.
pub fn aggregate_trackers(rows: &[TorrentView]) -> Vec<TrackerAgg> {
    use std::collections::HashMap;
    let mut by_host: HashMap<String, TrackerAgg> = HashMap::new();
    for tv in rows.iter().filter(|tv| !tv.busy) {
        let url = match tv.urls.first() {
            Some(u) => u.as_str(),
            None => continue,
        };
        // Same host-extraction chain as build_trk so the grouping matches what
        // the per-torrent view shows for each row's host.
        let host = url
            .split("://")
            .nth(1)
            .unwrap_or(url)
            .split('/')
            .next()
            .unwrap_or(url)
            .to_string();
        let agg = by_host.entry(host.clone()).or_insert_with(|| TrackerAgg {
            host,
            torrents: 0,
            up_speed: 0,
            uploaded: 0,
            seeders: 0,
            leechers: 0,
            errors: 0,
        });
        agg.torrents += 1;
        agg.up_speed += tv.up_speed as u64;
        agg.uploaded += tv.uploaded;
        agg.seeders += tv.seeders as u64;
        agg.leechers += tv.leechers as u64;
        agg.errors += tv.error_count as u32;
    }
    let mut out: Vec<TrackerAgg> = by_host.into_values().collect();
    // Stable, deterministic order: uploaded desc, then host asc to break ties so
    // the list never reshuffles frame-to-frame for equal totals.
    out.sort_by(|a, b| {
        b.uploaded
            .cmp(&a.uploaded)
            .then_with(|| a.host.cmp(&b.host))
    });
    out
}

/// Severity of a plausibility finding. Ordered so the worst can be picked out.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum PlausibilityLevel {
    Ok,
    Suspect,
    Implausible,
}

/// One finding from the plausibility linter. `subject` is a torrent name or
/// "session" for the global checks. POD: no locks, computed once per tick.
#[derive(Clone)]
pub struct PlausibilityFlag {
    pub level: PlausibilityLevel,
    pub subject: String,
    pub reason: String,
}

// Linter thresholds (v1, hard-coded). Tuned from how private trackers flag
// fakes: implausibly large totals relative to torrent size, instantaneous speed
// above a believable home line, and a swarm-vs-speed mismatch. See the audit
// notes; these can move to config later without changing the call sites.
//
// A seeding torrent whose declared upload exceeds this multiple of its own size
// is suspect (you rarely upload 10x a torrent's bytes to one swarm) and very
// implausible past the higher multiple.
const RATIO_SUSPECT_X: u64 = 10;
const RATIO_IMPLAUSIBLE_X: u64 = 25;
// Instantaneous speed above this fraction of the configured home-line cap, to a
// near-empty swarm, is a classic tell. We treat "near-empty" as <= this many
// leechers.
const LONELY_SWARM_LEECHERS: u16 = 1;
// Fraction of max_upload_rate considered "fast" for the lonely-swarm check, in
// percent (so no float const).
const FAST_FRACTION_PCT: u64 = 50;

/// Run the plausibility linter over the snapshotted rows. Pure: arithmetic over
/// the POD rows plus the configured upload cap. Returns findings worst-first;
/// an all-OK session yields a single `Ok` summary flag so the overlay is never
/// blank. Resolved in `render_once`, stored on `Frame`, read by `build_frame`.
pub fn lint_plausibility(rows: &[TorrentView], max_upload_rate: u32) -> Vec<PlausibilityFlag> {
    let mut flags: Vec<PlausibilityFlag> = Vec::new();
    let cap = max_upload_rate as u64;
    let fast = cap.saturating_mul(FAST_FRACTION_PCT) / 100;

    for tv in rows.iter().filter(|tv| !tv.busy) {
        // Upload total far past the torrent's own size (seeding torrents only;
        // a downloading torrent has not declared a full length yet).
        if !tv.downloading && tv.length > 0 {
            let mult = tv.uploaded / tv.length;
            if mult >= RATIO_IMPLAUSIBLE_X {
                flags.push(PlausibilityFlag {
                    level: PlausibilityLevel::Implausible,
                    subject: tv.name.clone(),
                    reason: format!("uploaded {mult}x the torrent size"),
                });
            } else if mult >= RATIO_SUSPECT_X {
                flags.push(PlausibilityFlag {
                    level: PlausibilityLevel::Suspect,
                    subject: tv.name.clone(),
                    reason: format!("uploaded {mult}x the torrent size"),
                });
            }
        }
        // Fast upload to a near-empty swarm: physically implausible.
        if cap > 0 && tv.up_speed as u64 >= fast && tv.leechers <= LONELY_SWARM_LEECHERS {
            flags.push(PlausibilityFlag {
                level: PlausibilityLevel::Suspect,
                subject: tv.name.clone(),
                reason: format!(
                    "{} L upload near line speed with {} leecher(s)",
                    crate::utils::format_bytes(tv.up_speed),
                    tv.leechers
                ),
            });
        }
    }

    // Worst findings first, then by subject for a stable order.
    flags.sort_by(|a, b| {
        b.level
            .cmp(&a.level)
            .then_with(|| a.subject.cmp(&b.subject))
    });

    if flags.is_empty() {
        flags.push(PlausibilityFlag {
            level: PlausibilityLevel::Ok,
            subject: String::from("session"),
            reason: String::from("nothing looks implausible"),
        });
    }
    flags
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
    /// Per-host tracker rollups for the Trackers tab in aggregated mode. Always
    /// computed (cheap) so a `g` toggle shows them without a tick of lag.
    pub tracker_aggs: Vec<TrackerAgg>,
    /// Whether the Trackers tab should render the aggregated rollups instead of
    /// per-torrent rows. Snapshotted from `view::trk_aggregated()` each tick.
    pub trk_aggregated: bool,
    /// Plausibility linter findings, worst-first. Always computed (cheap) so the
    /// `!` overlay opens without a tick of lag. Read only by `build_frame`.
    pub plausibility: Vec<PlausibilityFlag>,
    pub feed: Vec<UiEvent>,
    /// How many rows the feed pane should occupy. The dashboard pads the pane
    /// with blank rows up to this so the box always bottom-anchors to the window
    /// (no empty terminal rows below the board).
    pub feed_cap: usize,
    /// Terminal height in rows. Every view pads its body up to this (minus the
    /// footer) so the box fills the whole window - no blank rows below it.
    pub term_h: usize,
    pub spinner: usize,
    /// `(seconds_since_started, cumulative_uploaded_bytes)` samples, oldest
    /// first - copied from the lock-light history ring by `render_once`. Feeds
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
    /// Projected seconds until the next ratio milestone, computed in
    /// `render_once` from the average credited rate over the history window.
    /// `None` when there is nothing seeding, the rate is non-positive, or the
    /// highest milestone is already reached. Rendered on the ratio tab.
    pub eta_next_milestone_secs: Option<u64>,
    /// Label for the next milestone the ETA targets (e.g. "2.0×"). Empty when
    /// `eta_next_milestone_secs` is `None`.
    pub next_milestone_label: String,
    /// The set of multi-selected torrent hashes (F2.1). POD copy snapshotted
    /// once per tick; `build_frame` reads only this copy - never MARKED directly.
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
                // ticks (does NOT mutate dl_state - advance happens only in the
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
                // Display-only projection of uploaded bytes between the sparse
                // announces (which are the ONLY place t.uploaded is mutated). We
                // add the integral of the speed curve over the window since the
                // last announce, the same closed form the next announce will
                // declare, so the displayed total climbs live instead of sitting
                // flat for ~30 min. The tracker still receives the value computed
                // at announce time; this never mutates t.uploaded.
                let projected_uploaded = {
                    let t1 = t.origin.elapsed().as_secs_f64();
                    let t0 = (t1 - t.last_announce.elapsed().as_secs_f64()).max(0.0);
                    let gained = t.integrate(t0, t1).round().max(0.0) as u64;
                    t.uploaded.saturating_add(gained)
                };
                rows.push(TorrentView {
                    name: t.name.clone(),
                    info_hash: t.info_hash,
                    seeders: t.seeders,
                    leechers: t.leechers,
                    // Instantaneous speed off the same curve that backs the
                    // declared integral - 4 sins, synchronous, no .await. It is 0
                    // while downloading (can_upload() is false), which is correct.
                    up_speed: t
                        .speed_at(t.origin.elapsed().as_secs_f64())
                        .round()
                        .min(u32::MAX as f64) as u32,
                    uploaded: projected_uploaded,
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
                    last_wire: t.last_wire.clone(),
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
                last_wire: None,
            }),
        }
    } // outer read lock dropped here
    rows
}

pub async fn snapshot_client() -> Option<ClientView> {
    // Use try_read so a queued CLIENT writer (key-renewer timer or `k` re-init)
    // does not block the render loop. On contention we simply show no client info
    // for this frame - same "busy placeholder" discipline as snapshot_torrents.
    crate::CLIENT.try_read().ok()?.as_ref().map(|c| ClientView {
        name: c.name.clone(),
        peer_id: c.peer_id.clone(),
        key: c.key,
        user_agent: c.user_agent.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Minimal seeding-torrent row for the pure aggregation/linter helpers.
    fn row(
        name: &str,
        url: &str,
        up_speed: u32,
        uploaded: u64,
        length: u64,
        leechers: u16,
    ) -> TorrentView {
        TorrentView {
            name: name.to_string(),
            info_hash: [0u8; 20],
            seeders: 3,
            leechers,
            up_speed,
            uploaded,
            length,
            left: 0,
            interval: 1800,
            secs_to_announce: 900,
            error_count: 0,
            busy: false,
            downloading: false,
            schedule_reason: 0,
            dl_percent: 100,
            downloaded: length,
            urls: if url.is_empty() {
                Vec::new()
            } else {
                vec![url.to_string()]
            },
            last_wire: None,
        }
    }

    #[test]
    fn aggregate_groups_by_host_and_sums() {
        let rows = vec![
            row("a", "http://t1.example/announce", 100, 1000, 500, 5),
            row("b", "http://t1.example/announce", 200, 3000, 500, 7),
            row("c", "udp://t2.example:80/announce", 50, 200, 500, 1),
        ];
        let aggs = aggregate_trackers(&rows);
        assert_eq!(aggs.len(), 2);
        // Sorted by uploaded desc: t1 (4000) before t2 (200).
        assert_eq!(aggs[0].host, "t1.example");
        assert_eq!(aggs[0].torrents, 2);
        assert_eq!(aggs[0].uploaded, 4000);
        assert_eq!(aggs[0].up_speed, 300);
        assert_eq!(aggs[0].seeders, 6);
        assert_eq!(aggs[0].leechers, 12);
        assert_eq!(aggs[1].host, "t2.example:80");
    }

    #[test]
    fn aggregate_skips_busy_and_urlless_rows() {
        let mut busy = row("busy", "", 0, 0, 0, 0);
        busy.busy = true;
        let rows = vec![busy, row("real", "http://t.example/a", 10, 10, 100, 2)];
        let aggs = aggregate_trackers(&rows);
        assert_eq!(aggs.len(), 1);
        assert_eq!(aggs[0].host, "t.example");
    }

    #[test]
    fn lint_flags_oversized_upload() {
        // uploaded 30x size => implausible.
        let rows = vec![row("huge", "http://t/a", 0, 3000, 100, 5)];
        let flags = lint_plausibility(&rows, 2_097_152);
        assert_eq!(flags[0].level, PlausibilityLevel::Implausible);
    }

    #[test]
    fn lint_flags_fast_upload_to_empty_swarm() {
        // Near line speed (1.5 MiB/s on a 2 MiB/s cap) to a single leecher.
        let rows = vec![row("lonely", "http://t/a", 1_572_864, 100, 100_000, 1)];
        let flags = lint_plausibility(&rows, 2_097_152);
        assert!(flags.iter().any(|f| f.level == PlausibilityLevel::Suspect));
    }

    #[test]
    fn lint_clean_session_yields_ok_summary() {
        let rows = vec![row("fine", "http://t/a", 1000, 150, 100, 20)];
        let flags = lint_plausibility(&rows, 2_097_152);
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].level, PlausibilityLevel::Ok);
    }
}
