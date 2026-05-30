//! TTY-only blocking key reader. Spawned as a detached OS thread (never a tokio
//! task) because `crossterm::event::read()` blocks indefinitely and would
//! permanently consume a bounded tokio blocking-pool slot that can't be
//! cancelled. Arrow Up/Down walk the global speed multiplier; `q` and Ctrl+C
//! (delivered as the 0x03 key because raw mode swallowed SIGINT) request the
//! SAME shutdown the SIGINT path uses, by pinging an mpsc channel the shutdown
//! coordinator awaits.

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Spawn the reader. Returns immediately. The thread lives until it reads a quit
/// key OR `running` flips to false (set by the shutdown coordinator so a SIGINT
/// that arrived via the signal path also tears this thread down). `poll` bounds
/// the block so `running` is re-checked ~10×/s.
pub fn spawn(running: Arc<AtomicBool>, notify: tokio::sync::mpsc::UnboundedSender<()>) {
    let _ = std::thread::Builder::new()
        .name("mirage-keys".into())
        .spawn(move || {
            while running.load(Ordering::Relaxed) {
                match event::poll(Duration::from_millis(100)) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(_) => break, // stdin closed / terminal gone
                }
                let Ok(Event::Key(KeyEvent {
                    code,
                    modifiers,
                    kind,
                    ..
                })) = event::read()
                else {
                    continue;
                };
                // Release events only occur with keyboard-enhancement flags,
                // which we never enable; ignore for safety / Windows parity.
                if kind == KeyEventKind::Release {
                    continue;
                }
                use crate::control::{self, Cmd};
                use crate::ui::view::{self, View};
                let active = view::active_view();
                match (code, modifiers) {
                    // 1-9 switch the active tab.
                    (KeyCode::Char(d @ '1'..='9'), _) => {
                        view::set_view(d as usize - '1' as usize);
                    }
                    // Arrows: on the list views walk the selection; elsewhere keep
                    // the long-standing global speed bump (muscle memory).
                    (KeyCode::Up, _) => {
                        if matches!(active, View::Dashboard | View::Torrents | View::Trackers) {
                            view::bump_sel(-1, view::row_count());
                        } else if active == View::Speeds {
                            view::bump_sel(-1, 6);
                        } else {
                            crate::torrent::bump_multiplier(1);
                        }
                    }
                    (KeyCode::Down, _) => {
                        if matches!(active, View::Dashboard | View::Torrents | View::Trackers) {
                            view::bump_sel(1, view::row_count());
                        } else if active == View::Speeds {
                            view::bump_sel(1, 6);
                        } else {
                            crate::torrent::bump_multiplier(-1);
                        }
                    }
                    // Left/Right move between tabs (wraps around).
                    (KeyCode::Right, _) => view::cycle_view(1),
                    (KeyCode::Left, _) => view::cycle_view(-1),
                    // +/- edit the selected setting on the Speeds view, else
                    // nudge the global upload multiplier.
                    (KeyCode::Char('+' | '='), _) => speed_edit(active, 1),
                    (KeyCode::Char('-' | '_'), _) => speed_edit(active, -1),
                    // Toggle the help overlay.
                    (KeyCode::Char('?'), _) => view::toggle_help(),
                    // Pause / resume are GLOBAL (per-torrent pause isn't modeled
                    // in the engine): p toggles, r force-resumes.
                    (KeyCode::Char('p'), _) => {
                        control::toggle_paused();
                    }
                    (KeyCode::Char('r'), _) => {
                        control::set_paused(false);
                    }
                    // Force announce / remove act on the selected row of a list
                    // view (Dashboard/Torrents/Trackers); selected_hash skips the
                    // busy sentinel.
                    (KeyCode::Char('f'), _) => {
                        if is_list_view(active) {
                            if let Some(h) = view::selected_hash() {
                                control::send(Cmd::ForceAnnounce(h));
                            }
                        }
                    }
                    (KeyCode::Char('x'), _) => {
                        if is_list_view(active) {
                            if let Some(h) = view::selected_hash() {
                                control::send(Cmd::Remove(h));
                            }
                        }
                    }
                    (KeyCode::Char('k'), _) => {
                        if active == View::Client {
                            control::send(Cmd::ReinitClient);
                        }
                    }
                    (KeyCode::Char('s'), _) => {
                        if active == View::Config {
                            control::send(Cmd::SaveConfig);
                        }
                    }
                    // Esc closes the help overlay, else returns to the Dashboard.
                    (KeyCode::Esc, _) => {
                        if view::help_open() {
                            view::toggle_help();
                        } else {
                            view::set_view(0);
                        }
                    }
                    // Ctrl+C arrives as a KEY (0x03) because raw mode ate SIGINT.
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) | (KeyCode::Char('q'), _) => {
                        let _ = notify.send(()); // wake the shutdown coordinator
                        break;
                    }
                    _ => {}
                }
            }
        });
}

/// The views that show a selectable list of torrents (so f/x target a row).
fn is_list_view(v: crate::ui::view::View) -> bool {
    use crate::ui::view::View;
    matches!(v, View::Dashboard | View::Torrents | View::Trackers)
}

/// Left/Right editing for the Speeds view: edit the selected setting row (rates
/// / numwant / multiplier). On any other view, nudge the global multiplier.
/// Mirrors `gui/app.rs::edit`. Doubling/halving steps, clamped to sane bounds.
fn speed_edit(active: crate::ui::view::View, dir: i32) {
    use crate::ui::view::View;
    if active != View::Speeds {
        crate::torrent::bump_multiplier(if dir > 0 { 1 } else { -1 });
        return;
    }
    let mut cfg = (**crate::CONFIG.load()).clone();
    let step = |v: u32, d: i32| -> u32 {
        if d > 0 {
            v.saturating_mul(2).min(268_435_456)
        } else {
            (v / 2).max(4096)
        }
    };
    match crate::ui::view::sel() {
        0 => cfg.min_upload_rate = step(cfg.min_upload_rate, dir).min(cfg.max_upload_rate),
        1 => cfg.max_upload_rate = step(cfg.max_upload_rate, dir).max(cfg.min_upload_rate),
        2 => {
            crate::torrent::bump_multiplier(if dir > 0 { 1 } else { -1 });
            return;
        }
        3 => cfg.min_download_rate = step(cfg.min_download_rate, dir).min(cfg.max_download_rate),
        4 => cfg.max_download_rate = step(cfg.max_download_rate, dir).max(cfg.min_download_rate),
        5 => {
            let n = cfg.numwant.unwrap_or(80) as i32 + dir * 10;
            cfg.numwant = Some(n.clamp(1, 200) as u16);
        }
        _ => {}
    }
    crate::CONFIG.store(std::sync::Arc::new(cfg));
}
