//! Lock-safe snapshotting of the shared global state (TORRENTS / CLIENT /
//! STARTED) into plain-old-data structs the renderer can use without holding
//! any lock during the stdout write.
//!
//! DEADLOCK HAZARD (verified against the tree): the scheduler holds the inner
//! per-torrent `Mutex` across a network announce — `m.lock().await` in
//! scheduler.rs stays held through `tracker::announce(&mut t, ..).await`, which
//! does HTTP/UDP I/O lasting seconds. `announce_started`, `announce_stopped`
//! and the watcher do the same. The outer `TORRENTS.read()` is held alongside.
//!
//! Therefore the renderer:
//!   1. takes `TORRENTS.read().await` — read-vs-read never blocks, the announce
//!      paths also hold only `read()`, so no outer contention;
//!   2. uses `try_lock()` on each inner `Mutex`, NEVER `lock().await` — a torrent
//!      mid-announce yields a "busy" placeholder row for that frame;
//!   3. copies each torrent into a POD struct and drops every lock before any
//!      stdout write.

use crate::ui::events::UiEvent;

/// POD copy of one torrent for one frame. No borrows, no locks.
#[derive(Clone)]
pub struct TorrentView {
    pub name: String,
    pub seeders: u16,
    pub leechers: u16,
    pub up_speed: u32,         // bytes/s (next_upload_speed)
    pub uploaded: u64,         // total bytes
    pub interval: u64,         // current announce interval (s)
    pub secs_to_announce: u64, // interval - elapsed, saturating
    pub error_count: u16,
    pub busy: bool, // true => mid-announce, try_lock failed (placeholder row)
}

#[derive(Clone)]
pub struct ClientView {
    pub name: String,
    pub peer_id: String,
    pub key: u32, // fake_torrent_client::Client.key is u32
    #[allow(dead_code)]
    pub user_agent: String,
}

/// Everything render.rs needs for one frame. Pure data.
pub struct Frame {
    pub client: Option<ClientView>,
    pub started: chrono::DateTime<chrono::Utc>,
    pub now: chrono::DateTime<chrono::Utc>,
    pub rows: Vec<TorrentView>,
    pub feed: Vec<UiEvent>,
    pub spinner: usize,
}

pub async fn snapshot_torrents() -> Vec<TorrentView> {
    let list = crate::TORRENTS.read().await; // read lock; compatible with announcer
    let mut rows = Vec::with_capacity(list.len());
    for m in list.iter() {
        match m.try_lock() {
            // NEVER .await here
            Ok(t) => {
                let elapsed = t.last_announce.elapsed().as_secs();
                rows.push(TorrentView {
                    name: t.name.clone(),
                    seeders: t.seeders,
                    leechers: t.leechers,
                    up_speed: t.next_upload_speed,
                    uploaded: t.uploaded,
                    interval: t.interval,
                    secs_to_announce: t.interval.saturating_sub(elapsed),
                    error_count: t.error_count,
                    busy: false,
                });
            }
            Err(_) => rows.push(TorrentView {
                name: String::from("(announcing…)"),
                seeders: 0,
                leechers: 0,
                up_speed: 0,
                uploaded: 0,
                interval: 0,
                secs_to_announce: 0,
                error_count: 0,
                busy: true,
            }),
        }
    } // outer read lock dropped here
    rows
}

pub async fn snapshot_client() -> Option<ClientView> {
    crate::CLIENT.read().await.as_ref().map(|c| ClientView {
        name: c.name.clone(),
        peer_id: c.peer_id.clone(),
        key: c.key,
        user_agent: c.user_agent.clone(),
    })
}
