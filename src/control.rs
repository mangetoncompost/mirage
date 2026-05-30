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

/// The egui context, registered by the GUI so the engine can wake a repaint
/// immediately after publishing a fresh snapshot (instead of waiting the 1s tick).
pub static EGUI: std::sync::OnceLock<egui::Context> = std::sync::OnceLock::new();

/// Publish a new snapshot and wake the GUI to repaint (no-op without a GUI).
pub fn publish(snap: Snapshot) {
    SNAPSHOT.store(std::sync::Arc::new(snap));
    if let Some(ctx) = EGUI.get() {
        ctx.request_repaint();
    }
}

/// Build a fresh snapshot from live engine state (read lock + per-torrent
/// try_lock → POD, never holds a lock across .await), then publish it. Cheap;
/// safe to call ~1/s from a GUI publisher task. No-op cost when no GUI.
pub async fn build_and_publish() {
    let client = crate::CLIENT.read().await.as_ref().map(|c| ClientView {
        name: c.name.clone(),
        peer_id: c.peer_id.clone(),
        key: crate::config::KEY_HEX.load().as_str().to_string(),
        user_agent: c.user_agent.clone(),
    });
    let mut rows = Vec::new();
    let mut total_uploaded = 0u64;
    let mut total_up_speed = 0u64;
    let mut error_count = 0u32;
    {
        let list = crate::TORRENTS.read().await;
        for m in list.iter() {
            if let Ok(t) = m.try_lock() {
                let elapsed = t.last_announce.elapsed().as_secs();
                let up = t.speed_at(t.origin.elapsed().as_secs_f64()).round() as u32;
                let dl_percent = if t.length == 0 {
                    100
                } else {
                    (t.declared_downloaded() as u128 * 100 / t.length as u128) as u8
                };
                total_uploaded += t.uploaded;
                total_up_speed += up as u64;
                if t.error_count > 0 {
                    error_count += 1;
                }
                rows.push(TorrentView {
                    name: t.name.clone(),
                    info_hash: t.info_hash,
                    length: t.length,
                    seeders: t.seeders,
                    leechers: t.leechers,
                    up_speed: up,
                    uploaded: t.uploaded,
                    downloaded: t.declared_downloaded(),
                    left: t.declared_left(),
                    interval: t.interval,
                    secs_to_announce: t.interval.saturating_sub(elapsed),
                    error_count: t.error_count,
                    downloading: !t.is_seeding(),
                    dl_percent,
                    paused: false, // per-torrent pause not yet modeled in Torrent
                });
            }
        }
    }
    publish(Snapshot {
        client,
        rows,
        total_uploaded,
        total_up_speed,
        multiplier: crate::torrent::speed_multiplier(),
        paused: is_paused(),
        error_count,
    });
}

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
/// Some fields are part of the published data model for future views.
#[allow(dead_code)]
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
#[allow(dead_code)]
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
