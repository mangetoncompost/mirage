//! Terminal I/O for the dashboard: entering/leaving the alternate screen,
//! hiding/showing the cursor, and painting a pre-built frame string.
//!
//! Raw mode is enabled in `enter()` so the key thread (see `ui::keys`) can read
//! arrow keys and Ctrl+C (which arrives as the 0x03 key, not SIGINT). It is
//! disabled first thing in `restore()`, which runs on normal exit (Drop), the
//! panic hook, and the explicit shutdown path. The hot per-frame path writes the
//! raw ANSI embedded in the frame string; crossterm `execute!` is used only for
//! the rare enter/restore moves.

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    execute,
    style::ResetColor,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use std::io::{Write, stdout};

/// RAII guard: `enter()` on construction, `restore()` on Drop (normal-return path).
pub struct TermGuard;

impl TermGuard {
    pub fn enter() -> Self {
        let mut o = stdout();
        // Raw mode turns arrows into KeyCode::Up/Down and delivers Ctrl+C as the
        // 0x03 key (no SIGINT) — the key thread reads both. Disabled in restore().
        let _ = enable_raw_mode();
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
    // Disable raw mode FIRST; idempotent and safe even if it was never enabled
    // (non-TTY restore() calls, double-calls from Drop + explicit path + panic).
    let _ = disable_raw_mode();
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
