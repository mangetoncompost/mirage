//! Terminal I/O for the dashboard: entering/leaving the alternate screen,
//! hiding/showing the cursor, and painting a pre-built frame string.
//!
//! We do NOT enable raw mode: we only render, never read keystrokes, so
//! `tokio::signal::ctrl_c()` keeps working and line discipline stays sane on a
//! crash. The hot per-frame path writes the raw ANSI embedded in the frame
//! string; crossterm `execute!` is used only for the rare enter/restore moves.

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    execute,
    style::ResetColor,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io::{Write, stdout};

/// RAII guard: `enter()` on construction, `restore()` on Drop (normal-return path).
pub struct TermGuard;

impl TermGuard {
    pub fn enter() -> Self {
        let mut o = stdout();
        let _ = execute!(o, EnterAlternateScreen, Hide);
        TermGuard
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// Idempotent terminal restoration. Safe to call multiple times and even if the
/// TUI never started (it just emits harmless show-cursor / reset / leave-alt).
pub fn restore() {
    let mut o = stdout();
    let _ = execute!(o, ResetColor, Show, LeaveAlternateScreen);
    let _ = o.flush();
}

/// One buffered write per frame. `frame` already contains per-line clear-to-EOL
/// + line breaks and a trailing clear-below; we only home the cursor, write the
/// whole string, and flush — a single syscall's worth of I/O.
pub fn paint(frame: &str) {
    let mut o = stdout();
    let _ = execute!(o, MoveTo(0, 0));
    let _ = o.write_all(frame.as_bytes());
    let _ = o.flush();
}
