//! Runtime control layer between the async engine and the dashboard UI.
//!
//! The native window is a real embedded terminal running the dashboard in a PTY
//! child, so there is no in-process GUI snapshot to publish — the TTY renderer
//! reads engine state directly (see `ui::snapshot`). What remains here is the
//! lock-free global pause flag (checked in `Torrent::can_upload`) and the
//! command channel the dashboard's keys use to mutate the running engine.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc::UnboundedSender;

/// Global pause: when true, no upload is declared and announces are skipped.
/// Checked in `Torrent::can_upload` (so speed_at/integrate return 0). Lock-free,
/// flipped by the dashboard's `p` key.
static PAUSED: AtomicBool = AtomicBool::new(false);

/// Channel the dashboard uses to request structural mutations; the engine's
/// `process_commands` task drains it. Set once at engine startup.
pub static CMD: std::sync::OnceLock<UnboundedSender<Cmd>> = std::sync::OnceLock::new();

#[inline]
pub fn is_paused() -> bool {
    PAUSED.load(Ordering::Relaxed)
}
#[inline]
pub fn set_paused(p: bool) {
    PAUSED.store(p, Ordering::Relaxed);
}
#[inline]
pub fn toggle_paused() -> bool {
    let n = !is_paused();
    set_paused(n);
    n
}

/// Send a command to the engine (no-op if the engine isn't running, e.g. tests).
pub fn send(cmd: Cmd) {
    if let Some(tx) = CMD.get() {
        let _ = tx.send(cmd);
    }
}

/// Structural mutations applied inside the engine loop (where locks are held
/// safely), never on the UI thread.
#[derive(Debug, Clone)]
pub enum Cmd {
    /// Add a torrent from a .torrent file path (copied into the watch dir).
    #[allow(dead_code)]
    Add(PathBuf),
    /// Remove a torrent by info hash.
    Remove([u8; 20]),
    /// Force the next announce for a torrent (resets its countdown).
    ForceAnnounce([u8; 20]),
    /// Re-init the emulated client (after a client-profile change).
    ReinitClient,
    /// Persist the current config to config.toml.
    SaveConfig,
    /// Export a snapshot of the live session to a timestamped JSON file (and,
    /// best-effort, the system clipboard via OSC-52). Applied in the engine so
    /// the per-torrent locks are taken on the engine task, never the UI thread.
    #[allow(dead_code)]
    ExportSnapshot,
    /// Set (or clear, with `None`) a per-torrent uploaded-bytes goal. Once
    /// `uploaded` reaches the target the torrent stops declaring upload — a
    /// hard ratio cap the tracker never sees exceeded.
    #[allow(dead_code)]
    SetRatioTarget([u8; 20], Option<u64>),
}
