//! Engine command processing for the dashboard.
//!
//! The dashboard's per-torrent keys (force announce, remove, re-init client,
//! save config) and the REPL send [`crate::control::Cmd`]s; `run_cli` wires the
//! channel and spawns [`process_commands`] to apply them against the running
//! engine. (The native window runs this same CLI path inside a PTY child, so its
//! keys flow here too.)

use tracing::info;

use crate::config;
use crate::{CONFIG, announcer};

/// Apply UI-issued commands to the running engine. Each handler takes only the
/// locks it needs, briefly, and never holds one across a network await beyond
/// what `announce` already does. Used by both the GUI `start()` and the CLI/TTY
/// `run_cli()` (the dashboard's per-torrent keys send these commands).
pub(crate) async fn process_commands(mut rx: tokio::sync::mpsc::UnboundedReceiver<crate::control::Cmd>) {
    use crate::control::Cmd;
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Cmd::Add(path) => {
                // Copy into the watch dir; the file watcher ingests + announces it.
                let dir = CONFIG.load().torrent_dir.clone();
                if let Some(name) = path.file_name() {
                    let dest = dir.join(name);
                    if let Err(e) = tokio::fs::copy(&path, &dest).await {
                        tracing::warn!("Add: copy {} failed: {e}", path.display());
                    } else {
                        info!("Add: {} → watch dir", path.display());
                    }
                }
            }
            Cmd::Remove(hash) => {
                announce_then_remove(hash).await;
            }
            Cmd::ForceAnnounce(hash) => {
                let list = crate::TORRENTS.read().await;
                for m in list.iter() {
                    let mut t = m.lock().await;
                    if t.info_hash == hash {
                        // Make it due now so the scheduler announces on the next wake.
                        t.interval = 0;
                        t.last_announce =
                            std::time::Instant::now() - std::time::Duration::from_secs(1);
                        info!("ForceAnnounce: {}", t.name);
                        break;
                    }
                }
            }
            Cmd::PauseTorrent(hash) | Cmd::ResumeTorrent(hash) => {
                // Per-torrent pause is modeled by the global pause for now; a real
                // per-torrent flag is a follow-up. Log the intent.
                tracing::info!("per-torrent pause/resume requested ({:?}) — use global pause", hash);
            }
            Cmd::ReinitClient => {
                let cfg = (**CONFIG.load()).clone();
                if let Some(n) = config::init_client(&cfg).await {
                    tokio::spawn(crate::run_key_renewer(n));
                }
                info!("client re-initialised");
            }
            Cmd::SaveConfig => {
                save_config_toml().await;
            }
        }
    }
}

/// Announce STOPPED for the torrent then drop it from the list.
async fn announce_then_remove(hash: [u8; 20]) {
    {
        let list = crate::TORRENTS.read().await;
        for m in list.iter() {
            let mut t = m.lock().await;
            if t.info_hash == hash {
                announcer::tracker::announce(&mut t, Some(announcer::tracker::Event::Stopped)).await;
                break;
            }
        }
    }
    let mut list = crate::TORRENTS.write().await;
    list.retain(|m| {
        if let Ok(t) = m.try_lock() {
            t.info_hash != hash
        } else {
            true
        }
    });
    info!("torrent removed");
}

/// Serialize the live CONFIG to the XDG config.toml.
async fn save_config_toml() {
    let c = CONFIG.load();
    let toml = format!(
        "client = \"{}\"\nport = {}\nmin_upload_rate = {}\nmax_upload_rate = {}\nmin_download_rate = {}\nmax_download_rate = {}\nnumwant = {}\ntorrent_dir = \"{}\"\n",
        c.client,
        c.port,
        c.min_upload_rate,
        c.max_upload_rate,
        c.min_download_rate,
        c.max_download_rate,
        c.numwant.unwrap_or(80),
        c.torrent_dir.display(),
    );
    if let Some(path) = crate::get_config_from_xdg() {
        if let Err(e) = tokio::fs::write(&path, toml).await {
            tracing::warn!("SaveConfig: {e}");
        } else {
            info!("config saved to {}", path.display());
        }
    }
}
