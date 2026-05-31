//! Persistence of each torrent's simulated download phase, keyed by info_hash,
//! so a restart resumes (completed torrents seed immediately, partially
//! downloaded ones continue from where they left off - never re-downloading
//! from scratch, which would itself be a tell).
//!
//! Hand-rolled JSON (the project already hand-rolls bencode + json_output, and
//! pulls no serde crate). Atomic write: temp + fsync + rename. Robust to a
//! missing/corrupt file, removed/new torrents, and file-size changes.
//!
//! **Schema versions:**
//! - v1 - `{version:1, torrents:[{info_hash, length, downloaded, seeding}]}`
//! - v2 - adds optional `upload_target` per torrent (F2.2 ratio cap)
//!
//! The parser accepts both versions: v1 files default `upload_target` to None,
//! so upgrading from v1 → v2 is transparent. v1 files are NOT re-written as v2
//! until the next save (which always writes v2).

use std::collections::HashMap;
use std::path::PathBuf;
use tracing::warn;

use crate::TORRENTS;
use crate::torrent::{DownloadState, Torrent};

/// One persisted entry (keyed by 40-hex info_hash in the map).
pub struct Entry {
    pub length: u64,
    pub downloaded: u64,
    pub seeding: bool,
    /// F2.2 ratio cap. None when not set or when loaded from a v1 file.
    pub upload_target: Option<u64>,
}

/// 40-char lowercase hex of a 20-byte info hash (stable across file moves).
fn hex40(h: &[u8; 20]) -> String {
    let mut s = String::with_capacity(40);
    for b in h {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Resolve the state file path (XDG state dir on Unix, torrent_dir sidecar elsewhere).
fn state_path() -> PathBuf {
    #[cfg(unix)]
    if let Ok(p) = xdg::BaseDirectories::with_prefix("Mirage").place_state_file("state.json") {
        return p;
    }
    let dir = crate::CONFIG.load().torrent_dir.clone();
    dir.join(".mirage_state.json")
}

/// Atomically persist all torrents' download state (temp + fsync + rename).
/// Uses try_lock to skip torrents mid-announce (they keep their last persisted
/// value; the next save catches them). Never partially overwrites on a crash.
pub async fn save() -> std::io::Result<()> {
    let path = state_path();
    let tmp = path.with_extension("json.tmp");
    let mut s = String::from("{\"version\":2,\"torrents\":[\n");
    {
        let list = TORRENTS.read().await;
        let mut first = true;
        for m in list.iter() {
            if let Ok(t) = m.try_lock() {
                if !first {
                    s.push_str(",\n");
                }
                first = false;
                s.push_str(&format!(
                    "{{\"info_hash\":\"{}\",\"length\":{},\"downloaded\":{},\"seeding\":{}",
                    hex40(&t.info_hash),
                    t.length,
                    t.declared_downloaded(),
                    t.is_seeding()
                ));
                if let Some(target) = t.upload_target {
                    s.push_str(&format!(",\"upload_target\":{target}"));
                }
                s.push('}');
            }
        }
    }
    s.push_str("\n]}");
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    tokio::fs::write(&tmp, s.as_bytes()).await?;
    tokio::fs::File::open(&tmp).await?.sync_all().await?;
    tokio::fs::rename(&tmp, &path).await
}

/// Parse the state file into a map keyed by 40-hex info_hash. Returns an empty
/// map on a missing file, and an empty map + warning on a corrupt one (so all
/// torrents fall back to their constructor default).
pub fn load_dict() -> HashMap<String, Entry> {
    let raw = match std::fs::read_to_string(state_path()) {
        Ok(r) => r,
        Err(_) => return HashMap::new(), // missing -> defaults
    };
    match parse_state(&raw) {
        Some(d) => d,
        None => {
            warn!("Mirage state file is corrupt, ignoring it");
            HashMap::new()
        }
    }
}

/// Apply one persisted entry (looked up by this torrent's info_hash) to a
/// freshly parsed torrent, with full validation. Resets dl_last_tick so downtime
/// is NOT credited as download progress. No-op if the torrent isn't in the dict.
pub fn apply(t: &mut Torrent, dict: &HashMap<String, Entry>) {
    let Some(e) = dict.get(&hex40(&t.info_hash)) else {
        return; // unmatched -> keep constructor default (Downloading, or 0-len Seeding)
    };
    if e.length != t.length {
        // File changed under this info_hash: don't trust a stale `downloaded`
        // against a different size - restart the download.
        t.dl_state = if t.length == 0 {
            DownloadState::Seeding
        } else {
            DownloadState::Downloading { downloaded: 0 }
        };
        t.completed_sent = false;
    } else if e.seeding {
        t.dl_state = DownloadState::Seeding;
        t.completed_sent = true; // completed in a prior session; don't re-fire it
    } else {
        let d = e.downloaded.min(t.length); // clamp overshoot
        t.dl_state = if d >= t.length {
            DownloadState::Seeding
        } else {
            DownloadState::Downloading { downloaded: d }
        };
        t.completed_sent = false;
    }
    // Restore upload target if set (F2.2 ratio cap).
    t.upload_target = e.upload_target;
    t.dl_last_tick = std::time::Instant::now();
}

/// Minimal tolerant parser for the flat, machine-generated state JSON. Any
/// malformed object rejects the whole file (-> None). Accepts v1 and v2.
fn parse_state(raw: &str) -> Option<HashMap<String, Entry>> {
    // Accept version 1 (no upload_target) and version 2 (with optional upload_target).
    let is_v1 = raw.contains("\"version\":1");
    let is_v2 = raw.contains("\"version\":2");
    if !is_v1 && !is_v2 {
        return None;
    }
    const KEY: &str = "\"info_hash\":\"";
    let mut map = HashMap::new();
    let mut rest = raw;
    while let Some(start) = rest.find(KEY) {
        // `after` starts right after the opening quote of the hash value.
        let after = &rest[start + KEY.len()..];
        let end = after.find('"')?;
        let hash = &after[..end];
        if hash.len() != 40 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        // `obj_tail` starts right after the hash closing quote.
        let obj_tail = &after[end..];
        // The object ends at the first `}` in obj_tail (this is the machine-
        // generated flat format: no nested objects).
        let obj_end = obj_tail.find('}').map(|i| i + 1).unwrap_or(obj_tail.len());
        let obj = &obj_tail[..obj_end];

        let length = extract_u64(obj, "\"length\":")?;
        let downloaded = extract_u64(obj, "\"downloaded\":")?;
        let seeding = extract_bool(obj, "\"seeding\":")?;
        // upload_target is optional even in v2 (not all torrents have a cap).
        let upload_target = if is_v2 {
            extract_u64_opt(obj, "\"upload_target\":")
        } else {
            None
        };
        map.insert(
            hash.to_string(),
            Entry {
                length,
                downloaded,
                seeding,
                upload_target,
            },
        );
        // Advance past the matched KEY so the next iteration finds the next object.
        rest = &rest[start + KEY.len()..];
    }
    Some(map)
}

/// Extract the integer value following `key` in `s` (stops at the first non-digit).
fn extract_u64(s: &str, key: &str) -> Option<u64> {
    let i = s.find(key)? + key.len();
    let digits: String = s[i..].chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<u64>().ok()
}

/// Like `extract_u64` but returns None gracefully if the key is absent.
fn extract_u64_opt(s: &str, key: &str) -> Option<u64> {
    extract_u64(s, key)
}

/// Extract the bool value following `key` in `s`.
fn extract_bool(s: &str, key: &str) -> Option<bool> {
    let i = s.find(key)? + key.len();
    let tail = s[i..].trim_start();
    if tail.starts_with("true") {
        Some(true)
    } else if tail.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(length: u64, downloaded: u64, seeding: bool) -> Entry {
        Entry {
            length,
            downloaded,
            seeding,
            upload_target: None,
        }
    }

    #[test]
    fn parse_roundtrip_and_corrupt() {
        let good_v1 = "{\"version\":1,\"torrents\":[\n\
            {\"info_hash\":\"aabbccddeeff00112233445566778899aabbccdd\",\"length\":1000,\"downloaded\":400,\"seeding\":false},\n\
            {\"info_hash\":\"00112233445566778899aabbccddeeff00112233\",\"length\":2000,\"downloaded\":2000,\"seeding\":true}\n]}";
        let d = parse_state(good_v1).expect("should parse v1");
        assert_eq!(d.len(), 2);
        let a = d.get("aabbccddeeff00112233445566778899aabbccdd").unwrap();
        assert_eq!((a.length, a.downloaded, a.seeding), (1000, 400, false));
        assert_eq!(a.upload_target, None); // v1 has no target
        let b = d.get("00112233445566778899aabbccddeeff00112233").unwrap();
        assert!(b.seeding);

        // v2 with upload_target
        let good_v2 = "{\"version\":2,\"torrents\":[\n\
            {\"info_hash\":\"aabbccddeeff00112233445566778899aabbccdd\",\"length\":1000,\"downloaded\":400,\"seeding\":false,\"upload_target\":5000000}\n]}";
        let d2 = parse_state(good_v2).expect("should parse v2");
        let a2 = d2.get("aabbccddeeff00112233445566778899aabbccdd").unwrap();
        assert_eq!(a2.upload_target, Some(5_000_000));

        // unknown version -> None
        assert!(parse_state("{\"version\":3,\"torrents\":[]}").is_none());
        assert!(parse_state("not json at all").is_none());
        // missing field -> None
        assert!(
            parse_state(
                "{\"version\":1,\"torrents\":[{\"info_hash\":\"aabbccddeeff00112233445566778899aabbccdd\",\"length\":10}]}"
            )
            .is_none()
        );
    }

    #[test]
    fn apply_validates() {
        use crate::torrent::DownloadState;
        let mut t = crate::torrent::Torrent {
            name: "x".into(),
            urls: vec![],
            length: 1000,
            private: false,
            uploaded: 0,
            last_announce: std::time::Instant::now(),
            info_hash: [0xAB; 20],
            info_hash_urlencoded: String::new(),
            seeders: 0,
            leechers: 0,
            next_upload_speed: 0,
            interval: 0,
            error_count: 0,
            encoding: None,
            min_interval: None,
            tracker_id: None,
            source_path: None,
            speed_seed: 0,
            origin: std::time::Instant::now(),
            dl_state: DownloadState::Downloading { downloaded: 0 },
            dl_rate: 1,
            dl_last_tick: std::time::Instant::now(),
            completed_sent: false,
            upload_target: None,
            schedule_reason: 0,
            last_wire: None,
        };
        let key = hex40(&t.info_hash);

        // matching length, partial -> resumes
        let mut dict = HashMap::new();
        dict.insert(key.clone(), entry(1000, 500, false));
        apply(&mut t, &dict);
        assert_eq!(t.dl_state, DownloadState::Downloading { downloaded: 500 });

        // seeding -> Seeding + completed_sent
        let mut dict = HashMap::new();
        dict.insert(key.clone(), entry(1000, 1000, true));
        apply(&mut t, &dict);
        assert_eq!(t.dl_state, DownloadState::Seeding);
        assert!(t.completed_sent);

        // upload_target restored
        let mut dict = HashMap::new();
        dict.insert(
            key.clone(),
            Entry {
                length: 1000,
                downloaded: 1000,
                seeding: true,
                upload_target: Some(123456),
            },
        );
        apply(&mut t, &dict);
        assert_eq!(t.upload_target, Some(123456));

        // length mismatch -> restart download
        t.dl_state = DownloadState::Downloading { downloaded: 999 };
        let mut dict = HashMap::new();
        dict.insert(key.clone(), entry(42, 10, false));
        apply(&mut t, &dict);
        assert_eq!(t.dl_state, DownloadState::Downloading { downloaded: 0 });

        // overshoot -> clamped to Seeding
        let mut dict = HashMap::new();
        dict.insert(key.clone(), entry(1000, 99999, false));
        apply(&mut t, &dict);
        assert_eq!(t.dl_state, DownloadState::Seeding);

        // unmatched -> unchanged
        let before = t.dl_state;
        apply(&mut t, &HashMap::new());
        assert_eq!(t.dl_state, before);
    }
}
