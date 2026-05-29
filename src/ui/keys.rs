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
        .name("ratioup-keys".into())
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
                match (code, modifiers) {
                    (KeyCode::Up, _) => {
                        crate::torrent::bump_multiplier(1);
                    }
                    (KeyCode::Down, _) => {
                        crate::torrent::bump_multiplier(-1);
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
