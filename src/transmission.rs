//! Faithful, self-contained Transmission client emulation for `client = "auto"`.
//!
//! At startup we detect the locally installed Transmission version (macOS) and
//! synthesize a `fake_torrent_client::Client` whose wire bytes match a real
//! Transmission of that version — peer_id prefix `-TR<m><n><p>0-`, User-Agent
//! `Transmission/<ver>`, the captured query template & header set, a constant
//! 8-hex-uppercase session key, and `numwant=0` on stop. When Transmission is
//! updated, Mirage follows automatically on the next launch.
//!
//! Ground truth: a captured announce from Transmission 4.1.1 —
//!   peer_id=-TR4110-bf0dd2puquxi, User-Agent: Transmission/4.1.1,
//!   key=5D8BA306 (8 hex UPPERCASE, constant), numwant 80 started / 0 stopped,
//!   headers: User-Agent, Accept: */*, Accept-Encoding: deflate, gzip.

use std::path::Path;
use tracing::{info, warn};

use fake_torrent_client::clients::ClientVersion;

/// Standard macOS app-bundle location. The Homebrew cask also symlinks the app
/// into /Applications, so this single path covers the common cases. MacPorts /
/// Setapp / relocated installs are not covered → safe fallback applies.
const MAC_PLIST: &str = "/Applications/Transmission.app/Contents/Info.plist";

/// Nearest built-in fallback when detection fails. 4.0.6 is the newest TR
/// profile the bundled crate knows and matches the captured 4.x wire format.
pub const FALLBACK_VERSION: Version = Version {
    major: 4,
    minor: 0,
    patch: 6,
};

/// EXACT 4.1.1-capture query template (param order: info_hash, peer_id, port,
/// uploaded, downloaded, left, numwant, key, compact, supportcrypto, event).
/// `{key}` is substituted with 8 uppercase hex in build_url. No ipv6/ipv4 token,
/// so build_url has nothing to strip.
pub const TR_QUERY: &str = "info_hash={infohash}&peer_id={peerid}&port={port}\
&uploaded={uploaded}&downloaded={downloaded}&left={left}\
&numwant={numwant}&key={key}&compact=1&supportcrypto=1&event={event}";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

impl Version {
    /// Version string for the User-Agent, e.g. "4.1.1".
    pub fn full_string(&self) -> String {
        format!("{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Detect the installed Transmission version on macOS, or None if absent/unparseable.
pub fn detect_macos() -> Option<Version> {
    let path = Path::new(MAC_PLIST);
    if !path.exists() {
        return None;
    }
    // plist::Value::from_file handles BOTH binary and XML plists.
    let value = plist::Value::from_file(path).ok()?;
    let dict = value.as_dictionary()?;
    let raw = dict.get("CFBundleShortVersionString")?.as_string()?;
    parse_version(raw)
}

/// Parse "4.1.1" -> (4,1,1); "4.0" -> (4,0,0); "3.00" -> (3,0,0); strips any
/// build/suffix after the first space/'-'/'+'. major is mandatory; missing
/// minor/patch default to 0.
pub fn parse_version(raw: &str) -> Option<Version> {
    let core = raw.split([' ', '-', '+']).next().unwrap_or(raw);
    let mut it = core.split('.');
    let major: u16 = it.next()?.trim().parse().ok()?;
    let minor: u16 = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let patch: u16 = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    Some(Version {
        major,
        minor,
        patch,
    })
}

/// Resolve to a concrete version: detected, else logged fallback.
pub fn resolve_version() -> Version {
    match detect_macos() {
        Some(v) => {
            info!("client=auto: detected Transmission {}", v.full_string());
            v
        }
        None => {
            warn!(
                "client=auto: Transmission not found/parseable at {MAC_PLIST}; falling back to {}",
                FALLBACK_VERSION.full_string()
            );
            FALLBACK_VERSION
        }
    }
}

/// Build the `-TR<m><n><p>0-` peer-id prefix. Returns None if any component is
/// >= 10: the azureus encoding is fixed single-digit-per-component, Transmission
/// > has never shipped a two-digit component, and emitting a longer prefix would
/// > be a detectable fingerprint. Caller falls back to a crate profile prefix.
pub fn peer_id_prefix(v: &Version) -> Option<String> {
    if v.major >= 10 || v.minor >= 10 || v.patch >= 10 {
        return None;
    }
    Some(format!("-TR{}{}{}0-", v.major, v.minor, v.patch))
}

/// 12 random chars from [0-9a-z] (capture suffix example: bf0dd2puquxi).
fn random_peer_suffix() -> String {
    const POOL: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    (0..12)
        .map(|_| POOL[fastrand::usize(..POOL.len())] as char)
        .collect()
}

/// Build a faithful Transmission Client for `v` with the given single-digit-safe
/// peer-id prefix. No build()/generate_key()/generate_peer_id() — every wire
/// field is set directly, so the crate's stale defaults/typos are never read.
fn synthesize(v: &Version, prefix: &str) -> fake_torrent_client::Client {
    let mut c = fake_torrent_client::Client::default();
    c.name = format!("transmission-{}", v.full_string());
    c.peer_id = format!("{prefix}{}", random_peer_suffix()); // -TR4110-xxxxxxxxxxxx
    c.user_agent = format!("Transmission/{}", v.full_string()); // Transmission/4.1.1
    c.query = TR_QUERY.to_owned();

    // Header set EXACTLY as captured (order via get_query: UA, Accept, Accept-Encoding).
    c.accept = "*/*".to_owned();
    c.accept_encoding = "deflate, gzip".to_owned();
    c.accept_language = String::new(); // empty => no Accept-Language header
    c.connection = None; // get_query never emits Connection anyway

    c.num_want = 80; // started/periodic
    c.num_want_on_stop = 0; // stopped (capture confirms numwant=0)
    c.key_refresh_every = None; // Transmission key_refresh = Never => no renewer

    // 8 hex UPPERCASE, constant per session: store a non-zero u32; build_url
    // formats it {:08X}, UDP serializes the same u32 big-endian. Non-zero avoids
    // the crate's key==0 fingerprint.
    c.key = fastrand::u32(1..=u32::MAX);
    c
}

/// Entry point for client="auto". Detect → synthesize; on a multi-digit version
/// component, fall back to the nearest crate profile (a real, internally
/// consistent Transmission) rather than emit a malformed peer-id prefix.
pub fn build_auto_client() -> fake_torrent_client::Client {
    let v = resolve_version();
    match peer_id_prefix(&v) {
        Some(prefix) => synthesize(&v, &prefix),
        None => {
            warn!(
                "client=auto: version {} has a multi-digit component; using nearest crate profile instead of a malformed peer-id prefix",
                v.full_string()
            );
            fallback_from_profile(&v)
        }
    }
}

/// Nearest crate Transmission profile by major version (multi-digit edge case only).
fn nearest_profile(v: &Version) -> ClientVersion {
    match v.major {
        0..=2 => ClientVersion::Transmission_2_94,
        3 => ClientVersion::Transmission_3_00,
        _ => ClientVersion::Transmission_4_0_6, // 4.x and newer
    }
}

/// build() the nearest profile (authentic peer_prefix/UA for that version), then
/// override the wire template to the captured one and force a non-zero key.
fn fallback_from_profile(v: &Version) -> fake_torrent_client::Client {
    let mut c = fake_torrent_client::Client::default();
    c.build(nearest_profile(v));
    c.generate_peer_id(); // crate's authentic -TRxxxx- + checksum tail
    c.query = TR_QUERY.to_owned(); // drop the &ipv6={ipv6} token
    c.key = fastrand::u32(1..=u32::MAX); // bypass the key==0 decimal-parse bug
    c.key_refresh_every = None; // Never
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions() {
        assert_eq!(
            parse_version("4.1.1"),
            Some(Version {
                major: 4,
                minor: 1,
                patch: 1
            })
        );
        assert_eq!(
            parse_version("4.0"),
            Some(Version {
                major: 4,
                minor: 0,
                patch: 0
            })
        );
        assert_eq!(
            parse_version("3.00"),
            Some(Version {
                major: 3,
                minor: 0,
                patch: 0
            })
        );
        assert_eq!(
            parse_version("4.1.1-beta2"),
            Some(Version {
                major: 4,
                minor: 1,
                patch: 1
            })
        );
        assert_eq!(parse_version("garbage"), None);
    }

    #[test]
    fn peer_id_prefix_matches_capture_rule() {
        // Ground-truth capture: Transmission 4.1.1 -> -TR4110-
        let v = Version {
            major: 4,
            minor: 1,
            patch: 1,
        };
        assert_eq!(peer_id_prefix(&v).as_deref(), Some("-TR4110-"));
        assert_eq!(
            peer_id_prefix(&Version {
                major: 3,
                minor: 0,
                patch: 0
            })
            .as_deref(),
            Some("-TR3000-")
        );
        assert_eq!(
            peer_id_prefix(&Version {
                major: 4,
                minor: 0,
                patch: 6
            })
            .as_deref(),
            Some("-TR4060-")
        );
        // Multi-digit component -> None (caller uses crate fallback).
        assert_eq!(
            peer_id_prefix(&Version {
                major: 4,
                minor: 10,
                patch: 0
            }),
            None
        );
    }

    #[test]
    fn synthesized_client_is_faithful() {
        let v = Version {
            major: 4,
            minor: 1,
            patch: 1,
        };
        let c = synthesize(&v, "-TR4110-");
        assert!(c.peer_id.starts_with("-TR4110-"));
        assert_eq!(c.peer_id.len(), 20); // 8 prefix + 12 suffix
        assert_eq!(c.user_agent, "Transmission/4.1.1");
        assert_eq!(c.num_want, 80);
        assert_eq!(c.num_want_on_stop, 0);
        assert_eq!(c.key_refresh_every, None);
        assert_ne!(c.key, 0);
        // key renders as 8 uppercase hex
        let hex = format!("{:08X}", c.key);
        assert_eq!(hex.len(), 8);
        assert!(
            hex.chars()
                .all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_lowercase())
        );
    }
}
