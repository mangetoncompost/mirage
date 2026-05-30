//! Live "super-shell" dashboard for RatioUp.
//!
//! On an interactive stdout we paint a full-screen dashboard (header with
//! client/peer/key/uptime, one row per torrent with seeders/leechers/upload
//! speed/total/countdown bar, a scrolling recent-events feed, and a totals
//! footer), redrawn ~2.5×/s independently of the (possibly half-hourly)
//! announce schedule. When stdout is piped/redirected — or `RATIOUP_NO_UI` is
//! set — we fall back to the existing `tracing` logs unchanged.

pub mod draw;
mod events;
pub mod keys;
mod render;
mod snapshot;
pub mod view;

pub use events::{EventKind, emit};

use std::io::IsTerminal;
use std::time::Duration;

use snapshot::{Frame, snapshot_client, snapshot_torrents};

/// Use the live dashboard only on an interactive stdout, unless opted out via
/// `RATIOUP_NO_UI` (which forces classic log mode even on a TTY).
pub fn should_use_tui() -> bool {
    if std::env::var_os("RATIOUP_NO_UI").is_some() {
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

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let (w, h) = term_size();
                let rows = snapshot_torrents().await;
                // Publish row identities for the key thread, then clamp the
                // (atomic) selection to the live count so a shrunk torrent list
                // never leaves the cursor past the end.
                view::set_rows(rows.iter().map(|r| r.info_hash).collect());
                view::clamp_sel(rows.len());
                let active = view::active_view();
                let sel = view::sel();
                let feed_lines = render::feed_capacity(h, rows.len(), 12);
                let frame = Frame {
                    client: snapshot_client().await,
                    started: *crate::STARTED.get().expect("STARTED set in main before UI spawn"),
                    now: chrono::Utc::now(),
                    rows,
                    feed: events::snapshot(feed_lines),
                    spinner,
                };
                let s = render::build_frame(&frame, w, active, sel); // pure, no locks/IO
                draw::paint(&s);
                spinner = spinner.wrapping_add(1);
            }
            _ = shutdown.changed() => break,
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
        std::env::var(name).ok().and_then(|v| v.trim().parse::<u16>().ok())
    };
    let (mut w, mut h) = crossterm::terminal::size().unwrap_or((0, 0));
    if w < 40 {
        w = env_dim("COLUMNS").filter(|&c| c >= 40).unwrap_or(80);
    }
    if h < 10 {
        h = env_dim("LINES").filter(|&l| l >= 10).unwrap_or(24);
    }
    (w, h)
}
