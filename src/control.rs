//! Runtime control layer shared between the async engine (scheduler/announcer)
//! and the native GUI. The GUI reads a lock-free published [`Snapshot`] each
//! frame and sends mutations as [`Cmd`]s; the scheduler drains the commands and
//! publishes the snapshot while it already holds the torrent locks (so the UI
//! never touches a lock the announcer holds across network I/O).

use arc_swap::ArcSwap;
use once_cell::sync::Lazy;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc::UnboundedSender;

/// Global pause: when true, no upload is declared and announces are skipped.
/// Checked in `Torrent::can_upload` (so speed_at/integrate return 0) and in the
/// scheduler. Lock-free, flipped by the GUI.
static PAUSED: AtomicBool = AtomicBool::new(false);

/// Latest immutable snapshot of engine state, published by the scheduler and
/// read by the GUI with a single atomic load — no locking, no contention.
pub static SNAPSHOT: Lazy<ArcSwap<Snapshot>> = Lazy::new(|| ArcSwap::from_pointee(Snapshot::default()));

/// Channel the GUI uses to request structural mutations; the scheduler drains it.
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

/// Structural mutations applied inside the scheduler loop (where locks are held
/// safely), never on the UI thread.
#[derive(Debug, Clone)]
pub enum Cmd {
    /// Add a torrent from a .torrent file path (copied into the watch dir).
    Add(PathBuf),
    /// Remove a torrent by info hash.
    Remove([u8; 20]),
    /// Force the next announce for a torrent (resets its countdown).
    ForceAnnounce([u8; 20]),
    /// Pause a single torrent.
    PauseTorrent([u8; 20]),
    /// Resume a single torrent.
    ResumeTorrent([u8; 20]),
    /// Re-init the emulated client (after a client-profile change).
    ReinitClient,
    /// Persist the current config to config.toml.
    SaveConfig,
}

/// POD copy of one torrent for one GUI frame. No borrows, no locks.
#[derive(Clone, Default)]
pub struct TorrentView {
    pub name: String,
    pub info_hash: [u8; 20],
    pub length: u64,
    pub seeders: u16,
    pub leechers: u16,
    pub up_speed: u32,
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
    pub interval: u64,
    pub secs_to_announce: u64,
    pub error_count: u16,
    pub downloading: bool,
    pub dl_percent: u8,
    pub paused: bool,
}

#[derive(Clone, Default)]
pub struct ClientView {
    pub name: String,
    pub peer_id: String,
    pub key: String,
    pub user_agent: String,
}

/// Everything the GUI renders for one frame. Published by the scheduler.
#[derive(Clone, Default)]
pub struct Snapshot {
    pub client: Option<ClientView>,
    pub rows: Vec<TorrentView>,
    pub total_uploaded: u64,
    pub total_up_speed: u64,
    pub multiplier: f64,
    pub paused: bool,
    pub error_count: u32,
}
