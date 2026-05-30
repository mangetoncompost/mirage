#![allow(non_snake_case)]

use arc_swap::ArcSwap;
use fake_torrent_client::Client;
use once_cell::sync::Lazy;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, OnceCell, RwLock};
use tokio::time::Duration;
use tracing::{self, error, info, warn};
use utils::format_bytes;

pub(crate) use crate::announcer::scheduler::run as run_announcer;
use crate::config::Config;
use crate::torrent::Torrent;

mod announcer;
pub mod bencode;
mod config;
mod control;
mod directory;
mod engine;
mod gui;
pub mod json_output;
mod state;
pub mod torrent;
mod transmission;
mod ui;
mod utils;
mod watcher;

static STARTED: OnceCell<chrono::DateTime<chrono::Utc>> = OnceCell::const_new();
/// Live, lock-free swappable config: the native GUI can edit it at runtime and
/// the announce hot path reads it with `CONFIG.load()`. Defaults until set in main.
static CONFIG: Lazy<ArcSwap<Config>> = Lazy::new(|| ArcSwap::from_pointee(Config::default()));
static CLIENT: RwLock<Option<Client>> = RwLock::const_new(None);
static TORRENTS: RwLock<Vec<Mutex<Torrent>>> = RwLock::const_new(Vec::new()); // TODO: replace with mutex

async fn run_key_renewer(refresh_every: u16) {
    loop {
        if let Some(client) = &mut *CLIENT.write().await {
            client.generate_key();
            // generate_key() can leave key == 0 (lib bug); keep it non-zero.
            config::ensure_client_key(client);
        }
        // std::thread::sleep(Duration::from_secs(u64::from(refresh_every)));
        tokio::time::sleep(Duration::from_secs(u64::from(refresh_every))).await;
    }
}

/// Parse CLI args. Only a config file can be there.
pub(crate) fn parse_cli_args() -> Option<PathBuf> {
    let mut args = std::env::args().skip(1); // Skip the program name

    // Manually parse arguments
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-c" | "--config" => {
                if let Some(path_str) = args.next() {
                    return Some(PathBuf::from(path_str));
                } else {
                    tracing::error!("Missing value for -c/--config");
                }
            }
            // GUI flag is handled in main(); ignore it here silently.
            "--gui" => {}
            // Handle other arguments or positional arguments here
            other_arg => {
                tracing::error!("Warning: Unknown argument: {}, Ignoring", other_arg);
            }
        }
    }
    None
}

pub(crate) fn get_config_from_xdg() -> Option<PathBuf> {
    let xdg = xdg::BaseDirectories::with_prefix("RatioUp");
    match xdg.place_config_file("config.toml") {
        Ok(path) => return Some(path),
        Err(e) => tracing::error!("Cannot create config file: {e}"),
    }
    None
}

/// Entry point: a native macOS GUI window when launched with `--gui` or from the
/// .app bundle; otherwise the original CLI/TTY behavior, byte-for-byte.
fn main() {
    let want_gui = std::env::args().any(|a| a == "--gui") || launched_from_bundle();
    if want_gui {
        gui::run();
    } else {
        run_cli();
    }
}

/// True when the executable lives inside an .app bundle (…/Contents/MacOS/).
fn launched_from_bundle() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.ends_with("Contents/MacOS")))
        .unwrap_or(false)
}

#[tokio::main]
async fn run_cli() {
    //configure logger
    let log_level = std::env::var("RUST_LOG")
        .unwrap_or_else(|_| "trace".to_string());
    let level = match log_level.to_lowercase().as_str() {
        "error" => tracing::Level::ERROR,
        "warn"  => tracing::Level::WARN,
        "info"  => tracing::Level::INFO,
        "debug" => tracing::Level::DEBUG,
        _       => tracing::Level::TRACE,
    };
    // Live dashboard on an interactive TTY; classic tracing logs otherwise.
    // In TUI mode we deliberately do NOT init the fmt subscriber so the tracing
    // macros become no-ops and cannot corrupt the alternate screen — the event
    // ring carries the signal in-UI instead.
    let tui = ui::should_use_tui();
    let _term_guard = if tui {
        ui::install_panic_hook(); // BEFORE enter()
        Some(ui::draw::TermGuard::enter())
    } else {
        tracing_subscriber::fmt()
            .with_max_level(level)
            .with_level(true)
            .with_target(true)
            .with_file(true)
            .with_line_number(true)
            .with_thread_ids(true)
            .with_thread_names(true)
            .init();
        None
    };

    // get config path if possible
    let mut config_path: Option<PathBuf> = parse_cli_args();
    if config_path.is_none() {
        config_path = get_config_from_xdg();
    }

    // load config from file or default
    let config = if let Some(path) = config_path {
        tracing::info!("Loading configuration from {}", path.display());
        Config::load_from_file(&path).await
    } else {
        tracing::info!("Loading default configuration");
        Config::default()
    };

    info!(
        "Upload bandwidth: \u{2191} {} - {}",
        format_bytes(config.min_upload_rate),
        format_bytes(config.max_upload_rate)
    );

    CONFIG.store(Arc::new(config.clone()));
    if let Err(e) = STARTED.set(chrono::offset::Utc::now()) {
        error!("Cannot set start time: {e}");
        return;
    }

    // schedule client refresh key if applicable
    if let Some(refresh_every) = config::init_client(&config).await {
        tokio::spawn(run_key_renewer(refresh_every));
    }

    directory::prepare_torrent_folder(config.torrent_dir.clone()).await;
    let count = directory::load_torrents(config.torrent_dir).await;
    if count == 0 {
        info!("No torrent, exiting");
        return;
    }
    let mut pid_file: Option<PathBuf> = None;
    if config.use_pid_file {
        // Create PID file
        pid_file = write_pid_file().await;
    }
    let wait_time = announcer::tracker::announce_started().await;

    // Start file watcher for dynamic torrent management
    let watch_dir = CONFIG.load().torrent_dir.clone();
    tokio::spawn(async move {
        watcher::watch_directory(watch_dir).await;
    });

    // Live dashboard render task (TTY only) + shutdown signal it listens on.
    let (sd_tx, sd_rx) = tokio::sync::watch::channel(false);
    // Key thread -> shutdown coordinator wake-up (TTY only). In raw mode Ctrl+C
    // arrives as a key, not SIGINT, so the key thread pings this channel.
    let (key_tx, mut key_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    // Lets the key thread leave its poll loop when shutdown comes from SIGINT.
    let key_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));

    if tui {
        tokio::spawn(ui::run(sd_rx));
        // TTY ONLY: never spawned in non-TTY mode, so SPEED_STEP_IDX stays at the
        // default (x1.0) and behaviour is bit-identical to before there.
        ui::keys::spawn(key_running.clone(), key_tx);
    }

    tokio::spawn(async move {
        // Graceful exit on Ctrl+C: real SIGINT (non-TTY, or external `kill -INT`)
        // OR the q / Ctrl+C-as-key the dashboard reads under raw mode.
        tokio::select! {
            r = tokio::signal::ctrl_c() => { let _ = r; }
            _ = key_rx.recv() => {}
        }
        // Let the key thread leave its poll loop (exit(0) would kill it anyway).
        key_running.store(false, std::sync::atomic::Ordering::Relaxed);
        // Tear down the dashboard first so logs/exit messages land on the real
        // screen. exit(0) skips Drop, so restore() must be called explicitly;
        // it is idempotent, now also disables raw mode, and is safe even if the
        // TUI never started.
        let _ = sd_tx.send(true);
        ui::draw::restore();
        info!("Exiting...");
        announcer::tracker::announce_stopped().await;
        // Persist final download phase so the next launch resumes correctly.
        let _ = state::save().await;
        if config.use_pid_file && pid_file.is_some() {
            remove_pid_file(pid_file).await;
        }
        std::process::exit(0);
    });

    run_announcer(wait_time).await;
}

async fn write_pid_file() -> Option<PathBuf> {
    match xdg::BaseDirectories::new().place_runtime_file("ratio_up.pid") {
        Ok(file) => {
            match tokio::fs::write(file.clone(), std::process::id().to_string().as_bytes()).await {
                Ok(_) => Some(file),
                Err(e) => {
                    warn!("Cannot create PID file: {e}");
                    None
                }
            }
        }
        Err(e) => {
            warn!("Cannot create PID file: {e}");
            None
        }
    }
}

async fn remove_pid_file(pid_file: Option<PathBuf>) {
    if let Some(path) = pid_file {
        let _ = tokio::fs::remove_file(path).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    /// Test if it creates the torrent directory and do not panic when it exists
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_torrent_directory() {
        let mut dir = env::temp_dir();
        dir.push("ratioup-test-torrents-dir");
        if dir.is_dir() {
            let _ = std::fs::remove_dir(dir.clone());
        }
        directory::prepare_torrent_folder(dir.clone()).await;
        assert!(dir.is_dir());
        directory::prepare_torrent_folder(dir).await;
    }
}
