//! Persistence of each torrent's simulated download phase, keyed by info_hash,
//! so a restart resumes (completed torrents seed immediately, partially
//! downloaded ones continue from where they left off — never re-downloading
//! from scratch, which would itself be a tell).
//!
//! Hand-rolled JSON (the project already hand-rolls bencode + json_output, and
//! pulls no serde crate). Atomic write: temp + fsync + rename. Robust to a
//! missing/corrupt file, removed/new torrents, and file-size changes.

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
}

/// 40-char lowercase hex of a 20-byte info hash (stable across file moves).
fn hex40(h: &[u8; 20]) -> String {
    let mut s = String::with_capacity(40);
    for b in h {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Resolve the state file path (XDG state dir, fallback to a torrent_dir sidecar).
fn state_path() -> PathBuf {
    if let Ok(p) = xdg::BaseDirectories::with_prefix("RatioUp").place_state_file("state.json") {
        return p;
    }
    let dir = crate::CONFIG
        .get()
        .map(|c| c.torrent_dir.clone())
        .unwrap_or_else(|| PathBuf::from("."));
    dir.join(".ratioup_state.json")
}

/// Atomically persist all torrents' download state (temp + fsync + rename).
/// Uses try_lock to skip torrents mid-announce (they keep their last persisted
/// value; the next save catches them). Never partially overwrites on a crash.
pub async fn save() -> std::io::Result<()> {
    let path = state_path();
    let tmp = path.with_extension("json.tmp");
    let mut s = String::from("{\"version\":1,\"torrents\":[\n");
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
                    "{{\"info_hash\":\"{}\",\"length\":{},\"downloaded\":{},\"seeding\":{}}}",
                    hex40(&t.info_hash),
                    t.length,
                    t.declared_downloaded(),
                    t.is_seeding()
                ));
            }
        }
    }
    s.push_str("\n]}");
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    tokio::fs::write(&tmp, s.as_bytes()).await?;
    // fsync the temp file before rename for crash atomicity.
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
            warn!("RatioUp state file is corrupt, ignoring it");
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
        // against a different size — restart the download.
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
    t.dl_last_tick = std::time::Instant::now();
}

/// Minimal tolerant parser for the flat, machine-generated state JSON. Any
/// malformed object or unexpected version rejects the whole file (-> None).
fn parse_state(raw: &str) -> Option<HashMap<String, Entry>> {
    // version must be 1
    if !raw.contains("\"version\":1") {
        return None;
    }
    let mut map = HashMap::new();
    // Each torrent object looks like {"info_hash":"..","length":N,"downloaded":N,"seeding":bool}
    let mut rest = raw;
    while let Some(start) = rest.find("\"info_hash\":\"") {
        let after = &rest[start + "\"info_hash\":\"".len()..];
        let end = after.find('"')?;
        let hash = &after[..end];
        if hash.len() != 40 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let obj_tail = &after[end..];
        let length = extract_u64(obj_tail, "\"length\":")?;
        let downloaded = extract_u64(obj_tail, "\"downloaded\":")?;
        let seeding = extract_bool(obj_tail, "\"seeding\":")?;
        map.insert(
            hash.to_string(),
            Entry {
                length,
                downloaded,
                seeding,
            },
        );
        // advance past this object
        rest = obj_tail;
    }
    Some(map)
}

/// Extract the integer value following `key` in `s` (stops at the first non-digit).
fn extract_u64(s: &str, key: &str) -> Option<u64> {
    let i = s.find(key)? + key.len();
    let digits: String = s[i..].chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<u64>().ok()
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
        }
    }

    #[test]
    fn parse_roundtrip_and_corrupt() {
        let good = "{\"version\":1,\"torrents\":[\n\
            {\"info_hash\":\"aabbccddeeff00112233445566778899aabbccdd\",\"length\":1000,\"downloaded\":400,\"seeding\":false},\n\
            {\"info_hash\":\"00112233445566778899aabbccddeeff00112233\",\"length\":2000,\"downloaded\":2000,\"seeding\":true}\n]}";
        let d = parse_state(good).expect("should parse");
        assert_eq!(d.len(), 2);
        let a = d.get("aabbccddeeff00112233445566778899aabbccdd").unwrap();
        assert_eq!((a.length, a.downloaded, a.seeding), (1000, 400, false));
        let b = d.get("00112233445566778899aabbccddeeff00112233").unwrap();
        assert!(b.seeding);

        // corrupt / wrong version -> None
        assert!(parse_state("{\"version\":2,\"torrents\":[]}").is_none());
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
        // Build a minimal torrent via the test-friendly fields. We can't call
        // from_bencode_bytes here, so construct directly with a known info_hash.
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
