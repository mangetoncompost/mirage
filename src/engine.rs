//! Engine startup shared by the CLI entry (`run_cli` in main.rs) and the native
//! GUI entry (`gui::run`). `start()` is the GUI variant: it runs the full
//! announce engine on a background tokio runtime WITHOUT the terminal dashboard
//! or the ctrl_c→exit handler (the GUI window owns the process lifecycle).

use std::path::PathBuf;
use tracing::info;

use crate::config::{self, Config};
use crate::{CONFIG, STARTED, announcer, directory, state, watcher};

/// Resolve the config path (CLI arg or XDG) and load it (or defaults).
pub async fn load_config(config_path: Option<PathBuf>) -> Config {
    match config_path {
        Some(path) => {
            info!("Loading configuration from {}", path.display());
            Config::load_from_file(&path).await
        }
        None => {
            info!("Loading default configuration");
            Config::default()
        }
    }
}

/// Start the announce engine for the GUI: store config, init client, load
/// torrents, announce STARTED, spawn the watcher, then run the scheduler in the
/// foreground (of the background runtime). Returns if there are no torrents
/// (the GUI stays up; the watcher path can add some later).
pub async fn start(config: Config) {
    CONFIG.store(std::sync::Arc::new(config.clone()));
    let _ = STARTED.set(chrono::offset::Utc::now());

    if let Some(refresh_every) = config::init_client(&config).await {
        tokio::spawn(crate::run_key_renewer(refresh_every));
    }

    directory::prepare_torrent_folder(config.torrent_dir.clone()).await;
    let count = directory::load_torrents(config.torrent_dir.clone()).await;

    let wait_time = if count == 0 {
        info!("No torrent yet — GUI up, watching for .torrent files");
        1
    } else {
        announcer::tracker::announce_started().await
    };

    let watch_dir = CONFIG.load().torrent_dir.clone();
    tokio::spawn(async move {
        watcher::watch_directory(watch_dir).await;
    });

    // GUI ↔ engine command channel + a ~1s snapshot publisher (lock-free reads
    // for the UI). Both are harmless no-ops if no GUI is attached.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<crate::control::Cmd>();
    let _ = crate::control::CMD.set(tx);
    tokio::spawn(process_commands(rx));
    tokio::spawn(async {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tick.tick().await;
            crate::control::build_and_publish().await;
        }
    });

    // Best-effort STOPPED + state save when the process is asked to quit
    // (the GUI window close triggers exit; we also catch SIGINT here).
    tokio::spawn(async {
        let _ = tokio::signal::ctrl_c().await;
        announcer::tracker::announce_stopped().await;
        let _ = state::save().await;
        std::process::exit(0);
    });

    crate::run_announcer(wait_time).await;
}

/// Apply GUI-issued commands to the running engine. Each handler takes only the
/// locks it needs, briefly, and never holds one across a network await beyond
/// what `announce` already does.
async fn process_commands(mut rx: tokio::sync::mpsc::UnboundedReceiver<crate::control::Cmd>) {
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
