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
pub(crate) async fn process_commands(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<crate::control::Cmd>,
) {
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
                // Clone handles under a short read lock, then lock the inner
                // mutex off the outer lock (mirrors scheduler.rs pattern).
                let handles: Vec<_> = crate::TORRENTS.read().await.clone();
                for m in handles.iter() {
                    let mut t = m.lock().await;
                    if t.info_hash == hash {
                        // For downloading torrents the scheduler gates on
                        // last_announce.elapsed() >= dl_interval (~45 s). Setting
                        // last_announce far enough back forces that gate to open.
                        let dl_interval_secs = 50u64;
                        t.last_announce = std::time::Instant::now()
                            - std::time::Duration::from_secs(dl_interval_secs);
                        // For seeding torrents interval==0 + old last_announce makes
                        // should_announce() immediately true.
                        t.interval = 0;
                        info!("ForceAnnounce: {}", t.name);
                        break;
                    }
                }
            }
            Cmd::ReinitClient => {
                let cfg = (**CONFIG.load()).clone();
                if let Some(n) = config::init_client(&cfg).await {
                    // Aborts the previous renewer before spawning - no task leak.
                    crate::spawn_key_renewer(n);
                }
                info!("client re-initialised");
            }
            Cmd::SaveConfig => {
                save_config_toml().await;
            }
            Cmd::ExportSnapshot => {
                export_snapshot().await;
            }
            Cmd::SetRatioTarget(hash, target) => {
                let handles: Vec<_> = crate::TORRENTS.read().await.clone();
                for m in handles.iter() {
                    let mut t = m.lock().await;
                    if t.info_hash == hash {
                        t.set_upload_target(target);
                        let msg = match target {
                            Some(b) => format!("cap → {}", crate::utils::format_bytes_u64(b)),
                            None => "cap cleared".to_string(),
                        };
                        crate::ui::emit(crate::ui::EventKind::ConnectOk, &t.name, msg);
                        break;
                    }
                }
            }
        }
    }
}

/// Announce STOPPED for the torrent then drop it from the list.
async fn announce_then_remove(hash: [u8; 20]) {
    // Clone handles under a short read lock so we don't hold TORRENTS.read()
    // across the multi-second network announce (mirrors scheduler.rs pattern).
    let handles: Vec<_> = crate::TORRENTS.read().await.clone();
    for m in handles.iter() {
        let mut t = m.lock().await;
        if t.info_hash == hash {
            announcer::tracker::announce(&mut t, Some(announcer::tracker::Event::Stopped)).await;
            break;
        }
    }
    let mut list = crate::TORRENTS.write().await;
    list.retain(|m| {
        if let Ok(t) = m.try_lock() {
            t.info_hash != hash // drop the target; keep everything else
        } else {
            true // can't acquire lock → not the target (target was locked above); keep it
        }
    });
    info!("torrent removed");
}

/// Serialize the live CONFIG to the XDG config.toml using toml::to_string so
/// that client names and paths with quotes/backslashes/newlines are correctly
/// escaped and the file always round-trips without corrupting on the next load.
async fn save_config_toml() {
    let c = CONFIG.load();
    let mut tbl = toml::Table::new();
    tbl.insert("client".into(), toml::Value::String(c.client.clone()));
    tbl.insert("port".into(), toml::Value::Integer(c.port as i64));
    tbl.insert(
        "min_upload_rate".into(),
        toml::Value::Integer(c.min_upload_rate as i64),
    );
    tbl.insert(
        "max_upload_rate".into(),
        toml::Value::Integer(c.max_upload_rate as i64),
    );
    tbl.insert(
        "min_download_rate".into(),
        toml::Value::Integer(c.min_download_rate as i64),
    );
    tbl.insert(
        "max_download_rate".into(),
        toml::Value::Integer(c.max_download_rate as i64),
    );
    tbl.insert(
        "numwant".into(),
        toml::Value::Integer(c.numwant.unwrap_or(80) as i64),
    );
    tbl.insert("use_pid_file".into(), toml::Value::Boolean(c.use_pid_file));
    tbl.insert(
        "torrent_dir".into(),
        toml::Value::String(c.torrent_dir.display().to_string()),
    );
    if let Some(ref p) = c.output_stats {
        tbl.insert(
            "output_stats".into(),
            toml::Value::String(p.display().to_string()),
        );
    }
    let toml_str = toml::to_string(&tbl).unwrap_or_else(|e| {
        tracing::warn!("SaveConfig: serialize failed: {e}");
        String::new()
    });
    if toml_str.is_empty() {
        return;
    }
    if let Some(path) = crate::get_config_from_xdg() {
        if let Err(e) = tokio::fs::write(&path, toml_str).await {
            tracing::warn!("SaveConfig: {e}");
            crate::ui::emit(
                crate::ui::EventKind::Error,
                "config",
                format!("save failed: {e}"),
            );
        } else {
            info!("config saved to {}", path.display());
            crate::ui::emit(
                crate::ui::EventKind::ConnectOk,
                "config",
                format!("saved to {}", path.display()),
            );
        }
    }
}

/// Serialize the live session to a timestamped JSON file and queue an OSC-52
/// clipboard payload so the user can paste the path (F2.3).
///
/// Uses the same manual JSON builder as [`crate::json_output`] and the same
/// temp+rename write safety as [`crate::state`]. The clipboard gets the file
/// path (not the full JSON - OSC-52 has terminal-dependent size limits).
async fn export_snapshot() {
    let started = match crate::STARTED.get() {
        Some(s) => *s,
        None => return,
    };
    let mut json = String::with_capacity(4096);
    json.push_str("{\"started\":\"");
    json.push_str(&started.to_rfc3339());
    json.push_str("\",\"client\":\"");
    if let Some(cl) = &*crate::CLIENT.read().await {
        json.push_str(&cl.name);
    }
    json.push_str("\",\"torrents\":[\n");
    let handles: Vec<_> = crate::TORRENTS.read().await.clone();
    let mut total_up: u64 = 0;
    let mut first = true;
    for m in &handles {
        let t = m.lock().await;
        if !first {
            json.push(',');
        }
        first = false;
        total_up += t.uploaded;
        json.push_str(&t.to_json());
    }
    json.push_str("\n],\"total_uploaded\":");
    json.push_str(&total_up.to_string());
    json.push('}');

    // Resolve the export directory: XDG state dir (same as state.json) on Unix,
    // falling back to the torrent_dir. Never write to cwd - that breaks when
    // launched from Mirage.app (cwd = $HOME, fine) but is surprising otherwise.
    #[cfg(unix)]
    let export_dir = xdg::BaseDirectories::with_prefix("Mirage")
        .get_state_home()
        .unwrap_or_else(|| crate::CONFIG.load().torrent_dir.clone());
    #[cfg(not(unix))]
    let export_dir = crate::CONFIG.load().torrent_dir.clone();

    if let Err(e) = tokio::fs::create_dir_all(&export_dir).await {
        tracing::warn!("ExportSnapshot: mkdir {}: {e}", export_dir.display());
    }

    let now_str = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let filename = export_dir.join(format!("mirage-{now_str}.json"));
    let tmp = filename.with_extension("json.tmp");

    let written = if let Err(e) = tokio::fs::write(&tmp, json.as_bytes()).await {
        tracing::warn!("ExportSnapshot: write {}: {e}", tmp.display());
        false
    } else {
        match tokio::fs::rename(&tmp, &filename).await {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(
                    "ExportSnapshot: rename {} → {}: {e}",
                    tmp.display(),
                    filename.display()
                );
                // Clean up the orphaned .tmp so it does not accumulate on disk.
                let _ = tokio::fs::remove_file(&tmp).await;
                false
            }
        }
    };

    if written {
        let b64 = crate::utils::base64_encode(filename.to_string_lossy().as_bytes());
        crate::ui::draw::queue_clipboard(b64);
        crate::ui::emit(
            crate::ui::EventKind::Exported,
            "export",
            format!("→ {}", filename.display()),
        );
        info!("snapshot exported to {}", filename.display());
    } else {
        crate::ui::emit(
            crate::ui::EventKind::Error,
            "export",
            "write failed - check logs",
        );
    }
}
