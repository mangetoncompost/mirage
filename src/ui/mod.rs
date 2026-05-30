//! Live "super-shell" dashboard for Mirage.
//!
//! On an interactive stdout we paint a full-screen dashboard (header with
//! client/peer/key/uptime, one row per torrent with seeders/leechers/upload
//! speed/total/countdown bar, a scrolling recent-events feed, and a totals
//! footer), redrawn ~2.5×/s independently of the (possibly half-hourly)
//! announce schedule. When stdout is piped/redirected — or `MIRAGE_NO_UI` is
//! set — we fall back to the existing `tracing` logs unchanged.

pub mod draw;
mod events;
mod history;
pub mod keys;
pub mod overlay;
mod render;
mod snapshot;
pub mod view;

pub use events::{EventKind, emit};

use std::io::IsTerminal;
use std::time::Duration;

use snapshot::{Frame, snapshot_client, snapshot_torrents};

/// Use the live dashboard only on an interactive stdout, unless opted out via
/// `MIRAGE_NO_UI` (which forces classic log mode even on a TTY).
pub fn should_use_tui() -> bool {
    if std::env::var_os("MIRAGE_NO_UI").is_some() {
        return false;
    }
    std::io::stdout().is_terminal()
}

/// Install a panic hook that restores the terminal before the default hook
/// prints the panic message — so a render-task (or any) panic never strands the
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
        // per tick under the lock-light history ring — same discipline as
        // set_rows above, and never touched from build_frame or the key thread.
        let total_up: u64 = rows.iter().map(|r| r.uploaded).sum();
        let row_peak: u64 = rows.iter().map(|r| r.up_speed as u64).max().unwrap_or(0);
        let secs = (now - started).num_seconds().max(0);
        history::push_sample(secs, total_up, row_peak);

        // Resolve the active overlay (Help / Palette / Detail / None) here so
        // build_frame stays pure — it never reads the overlay atomics itself.
        let active_overlay = overlay::active();

        // Milestone celebration (F1.3): ratio = total_up / total_seeding_length.
        // Only seeding torrents contribute to the denominator — downloading ones
        // haven't finished yet and their partial length would deflate the ratio.
        let total_seeding_len: u64 = rows
            .iter()
            .filter(|r| !r.downloading && !r.busy)
            .map(|r| r.length)
            .sum();
        let celebrate = overlay::check_milestone(total_up, total_seeding_len, spinner as u64);
        if celebrate {
            crate::ui::emit(crate::ui::EventKind::Milestone, "session", overlay::celebration_label());
        }
        let celebrate_label = overlay::celebration_label();

        let frame = Frame {
            client: snapshot_client().await,
            started,
            now,
            rows,
            feed: events::snapshot(feed_lines),
            feed_cap: feed_lines,
            term_h: h as usize,
            spinner,
            up_history: history::samples(),
            frame_peak_speed: history::session_peak(),
            celebrate: overlay::celebrating(spinner as u64),
            celebrate_label,
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
        }
        #[cfg(not(unix))]
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            _ = tick.tick() => {
                render_once(spinner).await;
                spinner = spinner.wrapping_add(1);
            }
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
