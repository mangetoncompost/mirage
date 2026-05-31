//! Discreet desktop notifications for high-signal events (currently ratio
//! milestones). Off by default: nothing is emitted unless `notify_milestones`
//! is set in the config. There is deliberately no terminal bell, no sound, and
//! no notification on the high-frequency Error path - only the debounced
//! milestone crossing reaches here.
//!
//! Two best-effort channels, both silent on failure:
//!   1. OSC 9 queued through `draw::queue_notification`, drained on the next
//!      paint. This is the most widely supported terminal-notification escape
//!      (iTerm2, Ghostty, WezTerm, Windows Terminal); terminals that do not
//!      parse it just ignore the bytes, so a minimized-but-focused terminal
//!      still surfaces the notification natively where supported.
//!   2. A platform shell-out (`osascript` on macOS, `notify-send` on Linux) so
//!      the notification reaches the OS notification center even when the
//!      terminal itself does not handle OSC 9. Spawned detached; we never wait
//!      on it and never surface its failure.

/// Notify on a ratio-milestone crossing. `label` is the celebration text
/// (e.g. "ratio 2.0× !"). No-op unless `notify_milestones` is enabled.
pub fn milestone(label: &str) {
    if !crate::CONFIG.load().notify_milestones {
        return;
    }
    let body = format!("Mirage: {label}");
    // Terminal channel: queue for the next paint (drained as OSC 9).
    super::draw::queue_notification(body.clone());
    // OS channel: best-effort, detached shell-out.
    os_notify(&body);
}

/// Fire a detached OS notification. Best-effort: any spawn error is dropped.
/// The body is passed as a single argument (not interpolated into a shell
/// string) so a torrent-derived label cannot inject extra commands.
#[cfg(target_os = "macos")]
fn os_notify(body: &str) {
    // osascript -e 'display notification "<body>" with title "Mirage"'.
    // The body is embedded in the AppleScript string; strip quotes and
    // backslashes so it cannot terminate or escape the string literal.
    let safe: String = body.chars().filter(|&ch| ch != '"' && ch != '\\').collect();
    let script = format!("display notification \"{safe}\" with title \"Mirage\"");
    let _ = tokio::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(target_os = "linux")]
fn os_notify(body: &str) {
    // notify-send "Mirage" "<body>" - body is a separate argv entry, no shell.
    let _ = tokio::process::Command::new("notify-send")
        .arg("Mirage")
        .arg(body)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Other platforms: the OSC 9 terminal channel is the only path.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn os_notify(_body: &str) {}
