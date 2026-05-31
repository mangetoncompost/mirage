use arc_swap::ArcSwap;
use once_cell::sync::Lazy;
use std::str::FromStr;

use std::path::PathBuf;
use toml::Value;
use tracing::{error, info, warn};

use crate::transmission;

// use crate::json_output;

/// Uppercase-hex session key for the HTTP announce path, set in init_client and
/// read in build_url. ArcSwap so a runtime client re-init (from the GUI) can
/// re-store it. The UDP path uses client.key (u32); both serialize the SAME key.
pub static KEY_HEX: Lazy<ArcSwap<String>> = Lazy::new(|| ArcSwap::from_pointee(String::new()));

/// Visible, user-facing default folder for `.torrent` files: `~/Downloads/Mirage`.
/// Absolute, so it resolves to the same place no matter the working directory the
/// process was launched from (terminal cwd, Finder/`Mirage.app`, etc.). Falls back
/// to a bare relative `Mirage` only if `$HOME` is unset, which never happens on a
/// normal desktop session.
pub fn default_torrent_dir() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join("Downloads").join("Mirage"),
        None => PathBuf::from("Mirage"),
    }
}

/// Resolve a configured path against the config file's directory when it is
/// relative. A relative path in a config file means "next to the config", never
/// "next to wherever the process happened to start". Absolute paths pass through
/// unchanged. This is what keeps `cargo run` (cwd = repo) and `Mirage.app`
/// (cwd = home) reading the exact same folder.
fn anchor_to_config(config_path: &std::path::Path, value: PathBuf) -> PathBuf {
    if value.is_absolute() {
        return value;
    }
    match config_path.parent() {
        Some(dir) => dir.join(value),
        None => value,
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// torrent port
    pub port: u16,
    pub min_upload_rate: u32, //in byte
    pub max_upload_rate: u32, //in byte
    /// Simulated download speed band (bytes/s), used by the fake download phase.
    pub min_download_rate: u32,
    pub max_download_rate: u32,

    pub use_pid_file: bool,

    // /// when announcing on HTTPS tracker, do we check the SSL certificate
    // pub check_https_certs: bool,
    /// To set the number of peers we want
    pub numwant: Option<u16>,
    // pub simultaneous_seed: u16, //useful ?
    pub client: String,
    /// Directory scanned for `.torrent` files. Resolved to an absolute path
    /// (default `~/Downloads/Mirage`) so it does not depend on the launch cwd.
    pub torrent_dir: PathBuf,
    // pub key_refresh_every: u16,
    /// Output file path for the JSON file.
    /// You may want somethink like `/var/www/mirage.json` to expose it on your web server.
    pub output_stats: Option<PathBuf>,
    /// Emit a discreet desktop notification (terminal OSC 9 when the dashboard is
    /// active, else a best-effort `osascript`/`notify-send` shell-out) on a ratio
    /// milestone. Off by default: nothing is ever sent unless this is true. No
    /// terminal bell is used.
    pub notify_milestones: bool,
    /// Swarm-proportional upload cap: per-leecher upload budget in KiB/s. When
    /// `Some(k)`, each torrent's fake speed scales with its leecher count toward
    /// the configured `max_upload_rate` (declaring near-line-speed to an almost
    /// empty swarm is a classic ratio-faking tell). `None` (the default) keeps
    /// the original behaviour: the curve ignores swarm size entirely.
    pub per_leecher_kib_s: Option<u32>,
}
impl Default for Config {
    fn default() -> Self {
        Config {
            // The port number that the client is listening on. Ports reserved for BitTorrent are typically 6881-6889. Clients may choose to give up if it cannot establish
            // a port within this range. Here ports are random between 49152 and 65534
            port: fastrand::u16(49152..65534),
            min_upload_rate: 8192,         //8*1024
            max_upload_rate: 2097152,      //2048*1024
            min_download_rate: 8192,       // == .env MIN_DOWNLOAD_RATE hint
            max_download_rate: 16_777_216, // == .env MAX_DOWNLOAD_RATE hint (16 MiB/s)
            // check_https_certs: false,
            use_pid_file: false,
            numwant: None,
            torrent_dir: std::env::var("TORRENT_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| default_torrent_dir()),
            //client: fake_torrent_client::Client::from(fake_torrent_client::clients::ClientVersion::Qbittorrent_4_4_2),
            // key_refresh_every: 0,
            // "auto" detects & faithfully emulates the locally installed
            // Transmission version (falls back to a built-in profile if absent).
            client: String::from("auto"),
            output_stats: None,
            notify_milestones: false,
            per_leecher_kib_s: None,
        }
    }
}
impl Config {
    pub async fn load_from_file(path: &PathBuf) -> Config {
        let result: tokio::io::Result<String> = tokio::fs::read_to_string(path).await;
        let mut config = Config::default();
        match result {
            Ok(content) => {
                let config_value: Value = match toml::from_str(&content) {
                    Ok(val) => val,
                    Err(e) => {
                        error!("Cannot load config file: {e}");
                        return config;
                    }
                };

                let root_table = match config_value {
                    Value::Table(table) => table,
                    _ => {
                        error!("Invalid type in config file");
                        return config;
                    }
                };

                if let Some(client) = root_table.get("client") {
                    if let Some(client) = client.as_str() {
                        config.client = String::from(client);
                    } else {
                        error!("Client is not a string");
                    }
                }

                if let Some(port) = root_table.get("port") {
                    if let Some(port) = port.as_integer() {
                        if !(1..=65535).contains(&port) {
                            error!("Invalid port");
                        } else {
                            config.port = port as u16;
                        }
                    } else {
                        error!("port is not an integer");
                    }
                };

                if let Some(numwant) = root_table.get("numwant") {
                    if let Some(numwant) = numwant.as_integer() {
                        if !(1..=65535).contains(&numwant) {
                            error!("Invalid numwant");
                        } else {
                            config.numwant = Some(numwant as u16);
                        }
                    } else {
                        error!("numwant is not an integer");
                    }
                };

                if let Some(pid) = root_table.get("use_pid_file") {
                    if let Some(v) = pid.as_bool() {
                        config.use_pid_file = v;
                    } else {
                        error!("use_pid_file is not a boolean");
                    }
                    // Note: the redundant bool::from_str fallback has been removed -
                    // as_bool() is authoritative for TOML boolean values.
                }

                if let Some(n) = root_table.get("notify_milestones") {
                    if let Some(v) = n.as_bool() {
                        config.notify_milestones = v;
                    } else {
                        error!("notify_milestones is not a boolean");
                    }
                }

                if let Some(plk) = root_table.get("per_leecher_kib_s") {
                    if let Some(v) = plk.as_integer() {
                        if (1..=i64::from(u32::MAX)).contains(&v) {
                            config.per_leecher_kib_s = Some(v as u32);
                        } else {
                            error!("Invalid per_leecher_kib_s (expected 1..=u32::MAX)");
                        }
                    } else {
                        error!("per_leecher_kib_s is not an integer");
                    }
                }

                // Rate fields: validate range [0, u32::MAX] and fall through on
                // bad input (don't return early - later valid fields would be lost).
                let parse_rate = |label: &str, v: i64| -> Option<u32> {
                    if (0..=(u32::MAX as i64)).contains(&v) {
                        Some(v as u32)
                    } else {
                        error!("{label} out of range [0, {}]: {v}", u32::MAX);
                        None
                    }
                };
                if let Some(speed) = root_table.get("min_upload_rate") {
                    match speed.as_integer() {
                        Some(v) => {
                            if let Some(r) = parse_rate("min_upload_rate", v) {
                                config.min_upload_rate = r;
                            }
                        }
                        None => error!("min_upload_rate is not an integer"),
                    }
                }
                if let Some(speed) = root_table.get("max_upload_rate") {
                    match speed.as_integer() {
                        Some(v) => {
                            if let Some(r) = parse_rate("max_upload_rate", v) {
                                config.max_upload_rate = r;
                            }
                        }
                        None => error!("max_upload_rate is not an integer"),
                    }
                }
                if let Some(speed) = root_table.get("min_download_rate") {
                    match speed.as_integer() {
                        Some(v) => {
                            if let Some(r) = parse_rate("min_download_rate", v) {
                                config.min_download_rate = r;
                            }
                        }
                        None => error!("min_download_rate is not an integer"),
                    }
                }
                if let Some(speed) = root_table.get("max_download_rate") {
                    match speed.as_integer() {
                        Some(v) => {
                            if let Some(r) = parse_rate("max_download_rate", v) {
                                config.max_download_rate = r;
                            }
                        }
                        None => error!("max_download_rate is not an integer"),
                    }
                }

                if let Some(dir) = root_table.get("torrent_dir") {
                    if let Some(dir) = dir.as_str() {
                        config.torrent_dir = PathBuf::from(dir);
                    } else {
                        error!("Invalid torrent_dir");
                    }
                }

                if let Some(value) = root_table.get("output_stats") {
                    if let Some(path) = value.as_str() {
                        config.output_stats = Some(PathBuf::from(path));
                    } else {
                        error!("Invalid output_stats");
                    }
                }
            }
            Err(e) => {
                error!("Could not read config file: {} {e}", path.display());
                info!("Using default configuration");
            }
        };

        if !config.speeds_ok() {
            warn!(
                "Min upload rate ({}) is greater than max upload rate ({}), switching values",
                config.min_upload_rate, config.max_upload_rate
            );
            std::mem::swap(&mut config.min_upload_rate, &mut config.max_upload_rate);
        }
        if config.min_download_rate > config.max_download_rate {
            warn!(
                "Min download rate ({}) is greater than max download rate ({}), switching values",
                config.min_download_rate, config.max_download_rate
            );
            std::mem::swap(&mut config.min_download_rate, &mut config.max_download_rate);
        }

        // Make file-relative paths absolute, anchored to the config's own
        // directory. Without this, a relative `torrent_dir` resolves against the
        // process working directory, which differs between a terminal launch and
        // Mirage.app (cwd = home), so the two would read different folders.
        config.torrent_dir = anchor_to_config(path, config.torrent_dir);
        config.output_stats = config.output_stats.map(|p| anchor_to_config(path, p));

        config
    }

    fn speeds_ok(&self) -> bool {
        self.min_upload_rate <= self.max_upload_rate
    }
}

/// Init the client from the configuration and returns the interval to refresh client key if applicable
pub async fn init_client(config: &Config) -> Option<u16> {
    let client = if config.client.eq_ignore_ascii_case("auto") {
        // Faithful, self-contained emulation of the locally installed
        // Transmission version (or nearest fallback). Follows updates on relaunch.
        transmission::build_auto_client()
    } else {
        // Legacy path: explicit crate profile by exact enum name.
        let mut c = fake_torrent_client::Client::default();
        match fake_torrent_client::clients::ClientVersion::from_str(&config.client) {
            Ok(selected) => {
                c.build(selected);
            }
            Err(e) => {
                error!(
                    "Client {} does not exist, using default one: {e}",
                    config.client
                );
            }
        }
        // Work around a fake-torrent-client (0.9.11) bug: generate_key() builds a
        // hex string but parses it as DECIMAL u32, which fails for any key with
        // a-f, leaving client.key == 0. A constant key=0 is a detectable
        // fingerprint, so synthesize a real random non-zero key when that happens.
        ensure_client_key(&mut c);
        c
    };

    // Preformat the uppercase-hex key ONCE from the final u32 so the HTTP path
    // (hex) and the UDP path (key.to_be_bytes()) serialize the SAME key, constant
    // for the whole session. Real Transmission sends 8 hex uppercase, not decimal.
    KEY_HEX.store(std::sync::Arc::new(format!("{:08X}", client.key)));

    info!(
        "Client {} (key: {}, peer ID: {})",
        client.name,
        KEY_HEX.load().as_str(),
        client.peer_id
    );
    let key_interval = client.key_refresh_every;
    let mut guard = crate::CLIENT.write().await;
    *guard = Some(client);
    key_interval
}

/// Ensure the client carries a non-zero tracker key. fake-torrent-client's
/// `generate_key()` can leave `key == 0` (it parses a hex string as decimal),
/// and a constant zero key is a fingerprint. Generate a random non-zero u32.
pub fn ensure_client_key(client: &mut fake_torrent_client::Client) {
    if client.key == 0 {
        // non-zero: 1..=u32::MAX
        client.key = fastrand::u32(1..=u32::MAX);
    }
}

#[cfg(test)]
mod tests {
    use crate::config::{Config, anchor_to_config, default_torrent_dir};
    use std::path::{Path, PathBuf};

    #[test]
    fn test_speed_ok() {
        let mut cfg = Config::default();
        assert!(cfg.speeds_ok());

        cfg.min_upload_rate = 8192;
        cfg.max_upload_rate = 8192;
        assert!(cfg.speeds_ok());

        cfg.min_upload_rate = 8192;
        cfg.max_upload_rate = 4096;
        assert!(!cfg.speeds_ok());
    }

    #[test]
    fn relative_torrent_dir_anchors_to_config_dir() {
        let cfg = Path::new("/home/u/.config/Mirage/config.toml");
        // A relative value resolves next to the config file, not the cwd.
        assert_eq!(
            anchor_to_config(cfg, PathBuf::from("torrents")),
            PathBuf::from("/home/u/.config/Mirage/torrents")
        );
    }

    #[test]
    fn absolute_torrent_dir_passes_through() {
        let cfg = Path::new("/home/u/.config/Mirage/config.toml");
        let abs = PathBuf::from("/data/torrents");
        assert_eq!(anchor_to_config(cfg, abs.clone()), abs);
    }

    #[test]
    fn default_torrent_dir_is_absolute_under_home() {
        // The visible default must be absolute so it is cwd-independent.
        // Guarded on HOME being set, which it is in the test runner.
        if std::env::var_os("HOME").is_some() {
            let dir = default_torrent_dir();
            assert!(dir.is_absolute());
            assert!(dir.ends_with("Downloads/Mirage"));
        }
    }
}
