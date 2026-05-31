//! Live "super-shell" dashboard for Mirage.
//!
//! On an interactive stdout we paint a full-screen dashboard (header with
//! client/peer/key/uptime, one row per torrent with seeders/leechers/upload
//! speed/total/countdown bar, a scrolling recent-events feed, and a totals
//! footer), redrawn ~2.5×/s independently of the (possibly half-hourly)
//! announce schedule. When stdout is piped/redirected - or `MIRAGE_NO_UI` is
//! set - we fall back to the existing `tracing` logs unchanged.

pub mod draw;
mod events;
mod history;
pub mod keys;
mod notify;
pub mod overlay;
mod render;
mod snapshot;
pub mod view;

pub use events::{EventKind, emit};

use std::io::IsTerminal;
use std::time::Duration;

use snapshot::{Frame, snapshot_client, snapshot_torrents};

/// Wake signal from the key thread to the render loop. The render loop ticks at
/// ~400ms; without this, every keypress (tab switch, selection, action) would
/// wait up to a full tick before its effect appears. The key thread calls
/// [`request_redraw`] after each handled key; the loop's `select!` awaits it and
/// repaints immediately. A `Notify` coalesces: many keys before the next poll
/// collapse into one wakeup, so a key storm never queues redundant frames.
static REDRAW: once_cell::sync::Lazy<tokio::sync::Notify> =
    once_cell::sync::Lazy::new(tokio::sync::Notify::new);

/// Ask the render loop to repaint now (called from the key thread). Cheap and
/// lock-free; safe to call from the non-async key thread.
pub fn request_redraw() {
    REDRAW.notify_one();
}

/// Use the live dashboard only on an interactive stdout, unless opted out via
/// `MIRAGE_NO_UI` (which forces classic log mode even on a TTY).
pub fn should_use_tui() -> bool {
    if std::env::var_os("MIRAGE_NO_UI").is_some() {
        return false;
    }
    std::io::stdout().is_terminal()
}

/// Install a panic hook that restores the terminal before the default hook
/// prints the panic message - so a render-task (or any) panic never strands the
/// user in the alternate screen with a hidden cursor. MUST be called BEFORE
/// `draw::TermGuard::enter()`.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        draw::restore();
        default(info);
    }));
}

/// The render loop. Spawned as a sibling tokio task; the announcer stays the
/// foreground driver. Reads the shared global state through lock-safe snapshots
/// (see [`snapshot`]) and never holds a lock across the stdout write.
pub async fn run(mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let mut spinner = 0usize;
    let mut tick = tokio::time::interval(Duration::from_millis(400));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Build one frame from a fresh snapshot and paint it. Returns nothing; pure
    // except the single stdout write in draw::paint.
    async fn render_once(spinner: usize) {
        let (w, h) = term_size();
        let rows = snapshot_torrents().await;
        // Publish row identities for the key thread, then clamp the (atomic)
        // selection to the live count so a shrunk torrent list never leaves the
        // cursor past the end.
        view::set_rows(rows.iter().map(|r| r.info_hash).collect());
        let active = view::active_view();
        // Clamp the selection only for list views (Dashboard/Torrents/Trackers); on
        // the Speeds tab SEL is a settings-row index (0..=5) unrelated to torrent count.
        use view::View;
        match active {
            View::Dashboard | View::Torrents | View::Trackers => view::clamp_sel(rows.len()),
            View::Speeds => view::clamp_sel(6),
            // Ratio, Schedule, Network, Logs, Config, Client have no selectable list.
            _ => {}
        }
        let sel = view::sel();
        let feed_lines = render::feed_capacity(h, rows.len(), 12);
        let started = *crate::STARTED
            .get()
            .expect("STARTED set in main before UI spawn");
        let now = chrono::Utc::now();

        // Time-series sample for the history widgets: summed upload + summed
        // instantaneous speed across the (already-snapshotted) rows. Pushed once
        // per tick under the lock-light history ring - same discipline as
        // set_rows above, and never touched from build_frame or the key thread.
        let total_up: u64 = rows.iter().map(|r| r.uploaded).sum();
        let row_peak: u64 = rows.iter().map(|r| r.up_speed as u64).max().unwrap_or(0);
        let secs = (now - started).num_seconds().max(0);
        history::push_sample(secs, total_up, row_peak);

        // Resolve the active overlay (Help / Palette / Detail / None) here so
        // build_frame stays pure - it never reads the overlay atomics itself.
        let active_overlay = overlay::active();

        // Milestone celebration (F1.3): ratio = total_up / total_seeding_length.
        // Only seeding torrents contribute to the denominator - downloading ones
        // haven't finished yet and their partial length would deflate the ratio.
        let total_seeding_len: u64 = rows
            .iter()
            .filter(|r| !r.downloading && !r.busy)
            .map(|r| r.length)
            .sum();
        let celebrate = overlay::check_milestone(total_up, total_seeding_len, spinner as u64);
        if celebrate {
            crate::ui::emit(
                crate::ui::EventKind::Milestone,
                "session",
                overlay::celebration_label(),
            );
            // Discreet desktop/terminal notification (off unless opted in). Fired
            // here so it inherits check_milestone's once-per-milestone debounce.
            notify::milestone(&overlay::celebration_label());
        }
        let celebrate_label = overlay::celebration_label();

        // Project an ETA to the next ratio milestone (ratio tab footer). The
        // average credited rate is taken from the endpoints of the history ring
        // (the same cumulative-upload series the graph draws), which smooths the
        // sparse per-announce jumps. Bail to None on the same guards as the
        // milestone check: nothing seeding, no positive rate, or the top
        // milestone already reached.
        let (eta_next_milestone_secs, next_milestone_label) = {
            let samples = history::samples();
            let avg_rate = match (samples.first(), samples.last()) {
                (Some(&(t0, u0)), Some(&(t1, u1))) if t1 > t0 && u1 > u0 => {
                    (u1 - u0) as f64 / (t1 - t0) as f64
                }
                _ => 0.0,
            };
            if total_seeding_len == 0 || avg_rate <= 0.0 {
                (None, String::new())
            } else {
                let ratio_tenths = ((total_up as u128 * 10) / total_seeding_len as u128)
                    .min(u64::MAX as u128) as u64;
                match overlay::next_milestone_tenths(ratio_tenths) {
                    Some(m) => {
                        let target = (m as u128 * total_seeding_len as u128 / 10) as u64;
                        let remaining = target.saturating_sub(total_up);
                        let secs = (remaining as f64 / avg_rate).round().max(0.0) as u64;
                        (Some(secs), format!("{:.1}×", m as f64 / 10.0))
                    }
                    None => (None, String::new()),
                }
            }
        };

        // Tracker rollups for the `g`-toggled aggregated view. Computed from the
        // rows before they move into the frame; pure, no locks.
        let tracker_aggs = snapshot::aggregate_trackers(&rows);
        let trk_aggregated = view::trk_aggregated();
        // Plausibility linter (`!` overlay). Pure over the rows + the configured
        // upload cap; resolved here so build_frame only reads the result.
        let plausibility = snapshot::lint_plausibility(&rows, crate::CONFIG.load().max_upload_rate);

        let frame = Frame {
            client: snapshot_client().await,
            started,
            now,
            rows,
            tracker_aggs,
            trk_aggregated,
            plausibility,
            feed: events::snapshot(feed_lines),
            feed_cap: feed_lines,
            term_h: h as usize,
            spinner,
            up_history: history::samples(),
            frame_peak_speed: history::session_peak(),
            celebrate: overlay::celebrating(spinner as u64),
            celebrate_label,
            eta_next_milestone_secs,
            next_milestone_label,
            marked: view::marked_set(),
        };
        let s = render::build_frame(&frame, w, active, sel, active_overlay); // pure, no locks/IO
        draw::paint(&s);
    }

    // SIGWINCH (window resize) wakes an immediate repaint so the box reflows at
    // once instead of waiting up to one 400ms tick. SIGCONT (resumed after a
    // Ctrl-Z suspend) re-enters the alt screen + raw mode the shell may have
    // dropped, then repaints. (SIGCONT = 18 on Linux/macOS; tokio has no named
    // constructor for it, so build it from the raw number.)
    #[cfg(unix)]
    let (mut sigwinch, mut sigcont) = {
        use tokio::signal::unix::{SignalKind, signal};
        (
            signal(SignalKind::window_change()).expect("SIGWINCH handler"),
            signal(SignalKind::from_raw(18)).expect("SIGCONT handler"),
        )
    };

    loop {
        #[cfg(unix)]
        tokio::select! {
            // biased: shutdown always wins over a ready tick so we never paint
            // onto the restored normal screen after the coordinator calls restore().
            biased;
            _ = shutdown.changed() => break,
            _ = tick.tick() => {
                render_once(spinner).await;
                spinner = spinner.wrapping_add(1);
            }
            _ = sigwinch.recv() => { render_once(spinner).await; }
            _ = sigcont.recv() => {
                // Guard: don't re-enter the alt screen after shutdown was signalled.
                if !*shutdown.borrow() {
                    draw::reenter();
                    render_once(spinner).await;
                }
            }
            // Keypress wake: repaint at once so action feedback is immediate
            // instead of waiting up to one ~400ms tick. The spinner is not
            // advanced here so its cadence stays time-based, not key-based.
            _ = REDRAW.notified() => { render_once(spinner).await; }
        }
        #[cfg(not(unix))]
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            _ = tick.tick() => {
                render_once(spinner).await;
                spinner = spinner.wrapping_add(1);
            }
            _ = REDRAW.notified() => { render_once(spinner).await; }
        }
    }
    draw::restore();
}

/// Best-effort terminal size. `crossterm::terminal::size()` is authoritative on
/// a real terminal, but inside some pseudo-terminals (e.g. `script`) it can
/// report an implausibly small size; treat anything below a sane floor as
/// unknown and fall back to `$COLUMNS`/`$LINES`, then to 80×24.
fn term_size() -> (u16, u16) {
    let env_dim = |name: &str| -> Option<u16> {
        std::env::var(name)
            .ok()
            .and_then(|v| v.trim().parse::<u16>().ok())
    };
    let (mut w, mut h) = crossterm::terminal::size().unwrap_or((0, 0));
    // Only substitute a fallback when crossterm returned the (0,0) sentinel (unknown
    // size). A small-but-nonzero real size (e.g. the user dragged the window to 6
    // rows) should be trusted so render.rs's "terminal too small" guard fires correctly
    // and the frame is not painted at 80×24 into a 6-row window.
    if w == 0 {
        w = env_dim("COLUMNS").filter(|&c| c >= 40).unwrap_or(80);
    }
    if h == 0 {
        h = env_dim("LINES").filter(|&l| l >= 10).unwrap_or(24);
    }
    (w, h)
}
