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
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
    style::ResetColor,
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};
use std::io::{Write, stdout};

/// Enter the alternate screen, raw mode, hide the cursor, and DISABLE bracketed
/// paste. Without the last one, pasting text into the window under raw mode
/// streams raw bytes the key thread reads as commands (a pasted 'q' would quit,
/// 'x' would remove a torrent) — disabling it makes a paste a no-op.
///
/// We clear AFTER entering the alt screen. `EnterAlternateScreen` preserves the
/// cursor's row, so without `Clear(All)`+home the first frame paints wherever the
/// cursor sat (mid-screen, pushing the box down). And macOS Terminal.app keeps
/// the *main* screen's scrollback reachable even inside the alt screen, so we
/// also `Clear(Purge)` (ESC[3J) to wipe that scrollback — otherwise you can
/// scroll up and see the shell/clear output behind the dashboard.
fn enter_screen() {
    let mut o = stdout();
    let _ = enable_raw_mode();
    let _ = execute!(
        o,
        EnterAlternateScreen,
        Clear(ClearType::Purge),
        Clear(ClearType::All),
        MoveTo(0, 0),
        Hide,
        DisableBracketedPaste,
    );
}

/// RAII guard: `enter()` on construction, `restore()` on Drop (normal-return path).
pub struct TermGuard;

impl TermGuard {
    pub fn enter() -> Self {
        // Raw mode turns arrows into KeyCode::Up/Down and delivers Ctrl+C as the
        // 0x03 key (no SIGINT) — the key thread reads both. Undone in restore().
        enter_screen();
        TermGuard
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// Re-establish the alt screen + raw mode after returning from a Ctrl-Z suspend
/// (SIGCONT): the shell may have restored the normal screen and cooked mode.
pub fn reenter() {
    enter_screen();
}

/// Idempotent terminal restoration. Safe to call multiple times and even if the
/// TUI never started (it just emits harmless show-cursor / reset / leave-alt).
pub fn restore() {
    let mut o = stdout();
    // Disable raw mode FIRST; idempotent and safe even if it was never enabled
    // (non-TTY restore() calls, double-calls from Drop + explicit path + panic).
    let _ = disable_raw_mode();
    let _ = execute!(
        o,
        EnableBracketedPaste,
        ResetColor,
        Show,
        LeaveAlternateScreen
    );
    let _ = o.flush();
}

/// One buffered write per frame. `frame` already contains per-line clear-to-EOL,
/// line breaks, and a trailing clear-below; we only home the cursor, write the
/// whole string, and flush — a single syscall's worth of I/O.
pub fn paint(frame: &str) {
    let mut o = stdout();
    let _ = execute!(o, MoveTo(0, 0));
    let _ = o.write_all(frame.as_bytes());
    let _ = o.flush();
}
