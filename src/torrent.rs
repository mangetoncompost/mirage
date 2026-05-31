// https://wiki.theory.org/BitTorrentSpecification#Metainfo_File_Structure
// https://wiki.theory.org/BitTorrent_Tracker_Protocol
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use tracing::trace;

use crate::announcer::tracker::is_supported_url;
use crate::bencode::{BencodeDecoder, BencodeDecoderError, BencodeValue, encode_bencode_value};
use crate::utils::{get_sha1, percent_encoding};

/// Global upload-speed multiplier ladder, shared lock-free by the render path
/// (display) and the announce path (declared integral). The UP/DOWN arrow keys
/// walk this array and saturate at the ends; index 2 (== 1.0) is the default.
///
/// INVARIANT - coherence ceiling: the top step (8.0) is the ONLY ceiling. We do
/// NOT add a separate per-frame clamp in `speed_at`, because `integrate` has no
/// matching clamp; a clamp in one but not the other would break the
/// "declared bytes == area under the displayed curve" identity. Multiplying both
/// functions by the same scalar keeps them exactly proportional, so the identity
/// holds at every step. 8.0 means the effective peak is at most max_upload_rate*8.
pub const SPEED_STEPS: [f64; 6] = [0.25, 0.5, 1.0, 2.0, 4.0, 8.0];
const DEFAULT_STEP: usize = 2; // == 1.0
pub static SPEED_STEP_IDX: AtomicUsize = AtomicUsize::new(DEFAULT_STEP);

/// Current global speed multiplier. `Relaxed` is correct: a lone scalar with no
/// other memory published alongside it; a one-frame-late read is harmless.
#[inline]
pub fn speed_multiplier() -> f64 {
    SPEED_STEPS[SPEED_STEP_IDX
        .load(Ordering::Relaxed)
        .min(SPEED_STEPS.len() - 1)]
}

/// Walk the multiplier ladder by `delta` (+1 = Up, -1 = Down), saturating at the
/// ends. Called only from the TTY key thread (single writer). Returns the new factor.
pub fn bump_multiplier(delta: isize) -> f64 {
    let cur = SPEED_STEP_IDX.load(Ordering::Relaxed) as isize;
    let next = (cur + delta).clamp(0, SPEED_STEPS.len() as isize - 1) as usize;
    SPEED_STEP_IDX.store(next, Ordering::Relaxed);
    SPEED_STEPS[next]
}

/// Errors that can occur when parsing a Torrent struct from Bencode.
#[derive(Debug)]
pub enum TorrentError {
    BencodeError(BencodeDecoderError),
    MissingField(&'static str),
    InvalidFieldType(&'static str),
    ParseError(String), // For general parsing issues (e.g., string to u64)
    Utf8ConversionError(&'static str),
}

// Implement the Display trait for TorrentError
impl fmt::Display for TorrentError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            TorrentError::BencodeError(e) => write!(f, "Bencode decoding error: {:?}", e),
            TorrentError::MissingField(field) => write!(f, "Missing required field: {}", field),
            TorrentError::InvalidFieldType(field) => write!(f, "Invalid type for field: {}", field),
            TorrentError::ParseError(msg) => write!(f, "Parsing error: {}", msg),
            TorrentError::Utf8ConversionError(field) => {
                write!(f, "UTF-8 conversion error for field: {}", field)
            }
        }
    }
}

// Convert BencodeDecoderError to TorrentError
impl From<BencodeDecoderError> for TorrentError {
    fn from(err: BencodeDecoderError) -> Self {
        TorrentError::BencodeError(err)
    }
}

// #[derive(Debug, PartialEq, Eq, Clone)]
// pub struct Peer {
//     /// A string of length 20 which this peer uses as its id. This field will be `None` for compact peer info.
//     pub id: Option<String>,
//     /// peer's IP address either IPv6 (hexed) or IPv4 (dotted quad) or DNS name (string)
//     pub ip: String,
//     /// peer's port number
//     pub port: i64,
// }

/// Per-torrent simulated download phase. A real BitTorrent client leeches a
/// file (left>0, downloaded growing) before it can seed it; declaring upload on
/// a file you never downloaded is the classic ratio-cheat tell. We model that
/// progression. `Eq`-safe (no f64) so `Torrent`'s derive still compiles.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum DownloadState {
    /// Still leeching: `downloaded` bytes acquired so far (0 <= downloaded < length).
    Downloading { downloaded: u64 },
    /// Download finished; now seeding (declares upload).
    Seeding,
}

/// To only keep minimal torrent info in RAM. Info are ised in:
/// - the announcer (info hash, urls, name in log, sizes, downloaded, uploaded, interval, last_announce, seeders, leechers)
/// - web UI (info hash, name, size, downloaded, uploaded, seeders, leechers, is private, is a folder, path)
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Torrent {
    pub name: String,
    pub urls: Vec<String>, // aka. announce_list
    pub length: u64,
    pub private: bool,
    // pub info_hash: String,
    /// Total of fake uploaded data since the start of Mirage
    pub uploaded: u64,
    /// Last announce to the tracker
    pub last_announce: std::time::Instant,
    pub info_hash: [u8; 20],
    /// URL encoded hash thet is used to build the tracker query
    pub info_hash_urlencoded: String,
    /// Number of seeders, it is used on the web UI
    pub seeders: u16,
    /// Number of leechers, it is used on the web UI
    pub leechers: u16,
    /// It is the next upload speed that will be announced. It is also used for UI display.
    pub next_upload_speed: u32,
    /// Current interval after the last annouce
    pub interval: u64,
    pub error_count: u16,
    // pub creation_date: Option<DateTime<Local>>,
    // pub comment: Option<String>,
    // pub created_by: Option<String>,
    pub encoding: Option<String>,

    // for tracker response
    /// (optional) Minimum announce interval. If present clients must not reannounce more frequently than this.
    pub min_interval: Option<u64>,
    /// A string that the client should send back on its next announcements. If absent and a previous announce sent a tracker id, do not discard the old value; keep using it.
    pub tracker_id: Option<String>,

    /// Source file path (used for file watcher to identify torrents on removal)
    pub source_path: Option<PathBuf>,

    /// Per-torrent seed selecting the phases (and small period jitter) of the
    /// sum-of-sines upload-speed curve, so torrents fluctuate independently.
    pub speed_seed: u64,
    /// Fixed monotonic time origin for the speed curve. Captured ONCE at
    /// construction and NEVER reset (unlike `last_announce`). The curve argument
    /// is `origin.elapsed().as_secs_f64()`; the announce integral window is
    /// `[t1 - last_announce.elapsed(), t1]` with `t1 = origin.elapsed()`.
    pub origin: std::time::Instant,

    /// Current download phase + accumulated downloaded bytes. Source of truth for
    /// declared_downloaded()/declared_left()/is_seeding().
    pub dl_state: DownloadState,
    /// Fixed simulated download rate (bytes/s), drawn ONCE at construction from
    /// [min_download_rate, max_download_rate]. NOT persisted (re-drawn on load);
    /// progress is the persisted accumulator, not rate*elapsed. Guaranteed >= 1.
    pub dl_rate: u64,
    /// Wall-clock anchor for the download accumulator. advance_download() credits
    /// only the real seconds since this instant, then resets it. Reset on
    /// persistence-load so downtime is NOT credited as progress.
    pub dl_last_tick: std::time::Instant,
    /// Transient (NOT persisted; init false). Set true only after a `completed`
    /// announce gets a successful tracker response. Lets us RETRY `completed` on a
    /// failed announce while never double-sending it after success.
    pub completed_sent: bool,
    /// Optional cap on declared uploaded bytes (F2.2). When `uploaded >= target`,
    /// `can_upload()` returns false and the torrent is silently capped - the
    /// tracker never sees more than the goal. Persisted in state.json v2.
    /// `None` = no cap (unlimited).
    pub upload_target: Option<u64>,
    /// Transient (NOT persisted; init 0). Why the current announce cadence is
    /// what it is, stamped by the scheduler under the lock it already holds and
    /// surfaced by the Schedule ledger (F3.3). See [`ScheduleReason`].
    /// 0 = interval, 1 = warm-up, 2 = re-check, 3 = download-tick.
    pub schedule_reason: u8,
}

/// Cadence-reason codes stamped on [`Torrent::schedule_reason`]. Kept as a `u8`
/// on the struct (not an enum field) so it stays `Eq`/`Clone` with zero cost and
/// is trivially copied into the POD snapshot; this maps codes to labels.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScheduleReason {
    Interval = 0,
    Warmup = 1,
    Recheck = 2,
    DownloadTick = 3,
}

impl ScheduleReason {
    /// Map a stored `u8` back to a reason (unknown codes fall back to Interval).
    pub fn from_u8(v: u8) -> ScheduleReason {
        match v {
            1 => ScheduleReason::Warmup,
            2 => ScheduleReason::Recheck,
            3 => ScheduleReason::DownloadTick,
            _ => ScheduleReason::Interval,
        }
    }
    /// Short label for the ledger column.
    pub fn label(self) -> &'static str {
        match self {
            ScheduleReason::Interval => "interval",
            ScheduleReason::Warmup => "warm-up",
            ScheduleReason::Recheck => "re-check",
            ScheduleReason::DownloadTick => "dl-tick",
        }
    }
}

/// Oscillation periods (seconds) and their amplitude weights for the fake
/// upload-speed curve. INVARIANT: SPEED_WEIGHTS must sum to <= 1.0, otherwise
/// the curve can exceed [min,max] and the `.clamp()` in `speed_at` silently
/// breaks the "declared integral == area under the displayed curve" identity
/// (the bounds proof depends on the weights summing to <= 1). Components: a slow
/// mean-drift (1200s) plus three texture oscillations (90s, 23s, 7s).
const SPEED_PERIODS: [f64; 4] = [1200.0, 90.0, 23.0, 7.0];
const SPEED_WEIGHTS: [f64; 4] = [0.45, 0.25, 0.18, 0.12]; // sum == 1.0

/// Deterministic per-(torrent, component) phase in [0, TAU).
#[inline]
fn speed_phase(seed: u64, i: usize) -> f64 {
    let h = seed
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add((i as u64).wrapping_mul(0xD1B5_4A32_D192_ED03));
    // top 53 bits -> uniform f64 in [0,1) -> scale to [0, TAU)
    (h >> 11) as f64 / ((1u64 << 53) as f64) * std::f64::consts::TAU
}

/// Per-(torrent, component) effective angular frequency. The period is jittered
/// by up to +/-6% per seed/component so no two torrents share a clean harmonic
/// spectrum (defeats trivial FFT fingerprinting). Each component stays a single
/// sine, so the closed-form integral is unaffected.
#[inline]
fn speed_omega(seed: u64, i: usize) -> f64 {
    let h = seed
        .wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        .wrapping_add((i as u64).wrapping_mul(0x1656_67B1_9E37_79F9));
    let frac = (h >> 11) as f64 / ((1u64 << 53) as f64); // [0,1)
    let jitter = 1.0 + (frac - 0.5) * 0.12; // [0.94, 1.06)
    std::f64::consts::TAU / (SPEED_PERIODS[i] * jitter)
}

impl Torrent {
    /// Tells if we can announce to tracker(s) depending on the last announce
    pub fn should_announce(&self) -> bool {
        self.last_announce.elapsed().as_secs() >= self.interval
    }

    /// Tells if we can declare upload: only once we are SEEDING (finished the
    /// simulated download - you cannot upload a file you never downloaded) AND
    /// the swarm has leechers to serve. This single gate cascades: speed_at and
    /// integrate early-return 0 when !can_upload(), so a Downloading torrent
    /// declares uploaded=0 everywhere automatically.
    pub fn can_upload(&self) -> bool {
        if crate::control::is_paused() {
            return false;
        }
        if !self.is_seeding() {
            return false;
        }
        // F2.2 ratio cap: if a target was set and already met, stop uploading.
        if self.upload_target.is_some_and(|t| self.uploaded >= t) {
            return false;
        }
        (self.seeders > 0 && self.leechers > 0) || self.leechers > 1
    }

    /// Set or clear the per-torrent upload target (F2.2). Called from the engine
    /// on Cmd::SetRatioTarget, under the torrent's own Mutex (never under TORRENTS).
    pub fn set_upload_target(&mut self, target: Option<u64>) {
        self.upload_target = target;
    }

    /// Draw the per-torrent simulated download rate (bytes/s, >= 1) from config.
    /// Falls back to a sane band if CONFIG isn't set (some unit tests).
    fn pick_dl_rate() -> u64 {
        let cfg = crate::CONFIG.load();
        let lo = (cfg.min_download_rate as u64).max(1);
        let hi = (cfg.max_download_rate as u64).max(lo);
        fastrand::u64(lo..=hi)
    }

    /// Initial phase for a freshly parsed torrent (no persistence applied yet):
    /// a 0-length torrent is instantly Seeding, otherwise Downloading from 0.
    fn initial_dl_state(length: u64) -> DownloadState {
        if length == 0 {
            DownloadState::Seeding
        } else {
            DownloadState::Downloading { downloaded: 0 }
        }
    }

    /// Advance the simulated download by the real time elapsed since
    /// `dl_last_tick`, at the fixed `dl_rate`. Monotone, saturating, clamped to
    /// `length`. Returns true EXACTLY on the Downloading->Seeding transition (the
    /// call that crosses length) so the caller fires event=completed once.
    /// Idempotent once Seeding (returns false). This is the ONLY place
    /// `downloaded` is mutated.
    pub fn advance_download(&mut self) -> bool {
        if let DownloadState::Downloading { downloaded } = self.dl_state {
            let secs = self.dl_last_tick.elapsed().as_secs_f64();
            self.dl_last_tick = std::time::Instant::now();
            let gained = (secs * self.dl_rate as f64) as u64; // dl_rate >= 1
            let new = downloaded.saturating_add(gained).min(self.length);
            if new >= self.length {
                self.dl_state = DownloadState::Seeding;
                return true; // crossed the finish line on THIS call only
            }
            self.dl_state = DownloadState::Downloading { downloaded: new };
        }
        false
    }

    /// Bytes to declare as downloaded (== length once Seeding).
    #[inline]
    pub fn declared_downloaded(&self) -> u64 {
        match self.dl_state {
            DownloadState::Downloading { downloaded } => downloaded,
            DownloadState::Seeding => self.length,
        }
    }

    /// Bytes to declare as left. Never underflows (advance clamps to length).
    #[inline]
    pub fn declared_left(&self) -> u64 {
        self.length - self.declared_downloaded()
    }

    #[inline]
    pub fn is_seeding(&self) -> bool {
        matches!(self.dl_state, DownloadState::Seeding)
    }

    pub fn uploaded(&mut self, min_speed: u32, available_speed: u32) -> u32 {
        if self.can_upload() && (0 < min_speed && min_speed <= available_speed) {
            // Inclusive upper bound: min_speed..=available_speed prevents an empty
            // range panic when min_speed == available_speed.
            self.next_upload_speed = fastrand::u32(min_speed..=available_speed);
            self.next_upload_speed
        } else {
            0
        }
    }

    pub fn compute_speeds(&mut self) {
        // The frozen random pick is gone: next_upload_speed is now just the
        // current instantaneous value of the curve, kept for legacy JSON/log
        // output (the declared bytes come from `integrate`, not this field).
        let t = self.origin.elapsed().as_secs_f64();
        // Clamp explicitly before the float→u32 cast: with max_upload_rate near
        // u32::MAX and the 8× multiplier, speed_at can return ~3.4e10 which Rust
        // saturates to u32::MAX, silently pinning the display value.
        let speed = self.speed_at(t).round().min(u32::MAX as f64) as u32;
        self.next_upload_speed = speed;
        let config = crate::CONFIG.load();
        trace!(
            torrent = %self.name,
            min = config.min_upload_rate,
            max = config.max_upload_rate,
            computed_speed = speed,
            can_upload = self.can_upload(),
            seeders = self.seeders,
            leechers = self.leechers,
            "compute_speeds"
        );
    }

    /// Instantaneous fake upload rate (bytes/s) at elapsed time `t` seconds
    /// (measured from `self.origin`). Pure function of (speed_seed, t): the SAME
    /// function backs both the live dashboard display and the announce integral,
    /// so the declared total always equals the area under the displayed curve.
    pub fn speed_at(&self, t: f64) -> f64 {
        if !self.can_upload() {
            return 0.0;
        }
        let cfg = crate::CONFIG.load();
        let (min, max) = (cfg.min_upload_rate as f64, cfg.max_upload_rate as f64);
        let c = (max + min) * 0.5; // centre
        let h = (max - min) * 0.5; // half-range
        let mut s = c;
        for (i, &weight) in SPEED_WEIGHTS.iter().enumerate() {
            let omega = speed_omega(self.speed_seed, i);
            s += h * weight * (omega * t + speed_phase(self.speed_seed, i)).sin();
        }
        debug_assert!(SPEED_WEIGHTS.iter().sum::<f64>() <= 1.0 + 1e-9);
        // Scale by the global multiplier AFTER the [min,max] rounding guard. The
        // multiplier is the same scalar `integrate` applies, so display and
        // declared stay exactly proportional. It is 1.0 in non-TTY mode → identical
        // numeric behaviour to before.
        s.clamp(min, max) * speed_multiplier()
    }

    /// Closed-form integral of `speed_at` over [t0, t1], in BYTES.
    /// ∫ A·sin(ω·t + φ) dt = -(A/ω)·cos(ω·t + φ), so this is exact for any
    /// window length and any fractional endpoints - no history buffer, no
    /// per-step error. Returns 0 for empty/degenerate windows (and NaN).
    pub fn integrate(&self, t0: f64, t1: f64) -> f64 {
        if !self.can_upload() || t1 <= t0 {
            return 0.0;
        }
        let cfg = crate::CONFIG.load();
        let (min, max) = (cfg.min_upload_rate as f64, cfg.max_upload_rate as f64);
        let c = (max + min) * 0.5;
        let h = (max - min) * 0.5;
        let mut area = c * (t1 - t0); // mean contribution, >= 0
        for (i, &weight) in SPEED_WEIGHTS.iter().enumerate() {
            let omega = speed_omega(self.speed_seed, i);
            let ph = speed_phase(self.speed_seed, i);
            let amp = h * weight / omega;
            area += amp * ((omega * t0 + ph).cos() - (omega * t1 + ph).cos());
        }
        // Same global multiplier as `speed_at`, applied to the whole window. A
        // linear factor commutes with integration, so declared bytes == area
        // under the displayed (scaled) curve. The factor in force when integrate()
        // runs (announce time) scales the whole window - not applied retroactively
        // per keypress; the bounded one-window discrepancy is accepted.
        area * speed_multiplier()
    }

    // /// Load essential data from a parsed torrent using the full parsed torrent file. It reduces the RAM use to have smaller data
    // pub fn from_torrent(torrent: Torrent) -> Self {
    //     let hash_bytes = torrent.info_hash().expect("Cannot get torrent info hash");
    //     let hash = hash_bytes.encode_hex::<String>();
    //     //let hash = hash_bytes.???;
    //     let private = torrent.info.private.is_some() && torrent.info.private == Some(1);
    //     let mut t = Self {
    //         name: torrent.info.name.clone(),
    //         info_hash_urlencoded: String::with_capacity(64),
    //         length: torrent.total_size,
    //         last_announce: std::time::Instant::now(),
    //         urls: Vec::new(),
    //         info_hash: hash,
    //         private,
    //         downloaded: torrent.total_size,
    //         uploaded: 0,
    //         seeders: 0,
    //         leechers: 0,
    //         next_upload_speed: 0,
    //         next_download_speed: 0,
    //         interval: 4_294_967_295,
    //         error_count: 0,
    //     };
    //     t.urls = torrent.get_urls();
    //     t.info_hash_urlencoded = percent_encoding::percent_encode(
    //         &hash_bytes,
    //         crate::announcer::tracker::URL_ENCODE_RESERVED,
    //     )
    //     .to_string();
    //     debug!("Torrent: {:?}", t);
    //     t
    // }

    pub fn from_file(path: PathBuf) -> Result<Self, TorrentError> {
        let data = std::fs::read(&path).map_err(|e| {
            TorrentError::ParseError(format!("cannot read {}: {e}", path.display()))
        })?;
        let mut torrent = Self::from_bencode_bytes(&data)?;
        torrent.source_path = Some(path);
        Ok(torrent)
    }

    pub fn to_json(&self) -> String {
        let mut result = String::with_capacity(256);
        result.push_str("\t{\"name\": \"");
        result.push_str(&self.name.replace("\"", "\\\""));
        result.push_str("\", \"length\": ");
        result.push_str(&self.length.to_string());
        result.push_str(", \"private\": ");
        result.push_str(&self.private.to_string());
        result.push_str(", \"uploaded\": ");
        result.push_str(&self.uploaded.to_string());
        result.push_str(", \"seeders\": ");
        result.push_str(&self.seeders.to_string());
        result.push_str(", \"leechers\": ");
        result.push_str(&self.leechers.to_string());
        result.push_str(", \"next_upload_speed\": ");
        result.push_str(&self.next_upload_speed.to_string());
        result.push_str(", \"downloaded\": ");
        result.push_str(&self.declared_downloaded().to_string());
        result.push_str(", \"left\": ");
        result.push_str(&self.declared_left().to_string());
        result.push_str(", \"state\": \"");
        result.push_str(if self.is_seeding() {
            "seeding"
        } else {
            "downloading"
        });
        result.push_str("\", \"urls\": [");
        for (index, url) in self.urls.iter().enumerate() {
            if index > 0 {
                result.push_str(", ");
            }
            result.push_str(&format!("\"{url}\""));
        }
        // Close unconditionally so an empty urls list produces valid JSON.
        result.push_str("]}\n");
        result
    }

    /// Parses a raw bencoded .torrent file byte slice into a Torrent struct.
    ///
    /// This function decodes the Bencode structure, extracts relevant fields,
    /// calculates the info hash, and initializes default values for other fields.
    ///
    /// # Arguments
    /// * `bencode_data` - A byte slice containing the full bencoded .torrent file content.
    ///
    /// # Returns
    /// A `Result` which is `Ok(Torrent)` on success or `Err(TorrentError)` on failure.
    pub fn from_bencode_bytes(bencode_data: &[u8]) -> Result<Self, TorrentError> {
        let mut decoder = BencodeDecoder::new(bencode_data);
        let top_level_dict = match decoder.decode()? {
            BencodeValue::Dictionary(dict) => dict,
            _ => {
                return Err(TorrentError::InvalidFieldType(
                    "Top-level is not a dictionary",
                ));
            }
        };

        // --- Extract announce URLs ---
        let mut urls = Vec::new();
        // Try to get 'announce-list' first (multi-tracker)
        if let Some(BencodeValue::List(announce_list_bencode)) =
            top_level_dict.get(b"announce-list".as_ref())
        {
            for tier in announce_list_bencode {
                if let BencodeValue::List(tier_urls) = tier {
                    for url_bencode in tier_urls {
                        if let BencodeValue::ByteString(url_bytes) = url_bencode {
                            let url_str = std::str::from_utf8(url_bytes)
                                .map_err(|_| {
                                    TorrentError::Utf8ConversionError("announce-list URL")
                                })?
                                .to_string();
                            if !urls.contains(&url_str) && is_supported_url(&url_str) {
                                // Avoid duplicates
                                urls.push(url_str);
                            }
                        }
                    }
                }
            }
        }

        // Try to get 'announce' (single tracker), add if not already in urls
        if let Some(BencodeValue::ByteString(announce_bytes)) =
            top_level_dict.get(b"announce".as_ref())
        {
            let announce_str = std::str::from_utf8(announce_bytes)
                .map_err(|_| TorrentError::Utf8ConversionError("announce URL"))?
                .to_string();
            if !urls.contains(&announce_str) && is_supported_url(&announce_str) {
                // Avoid duplicates
                urls.push(announce_str);
            }
        }

        if urls.is_empty() {
            return Err(TorrentError::MissingField("announce or announce-list"));
        }

        // --- Extract 'info' dictionary and calculate info_hash ---
        // `info_bytes_slice` is `&BencodeValue`
        let info_bytes_slice = top_level_dict
            .get(b"info".as_ref())
            .ok_or(TorrentError::MissingField("info"))?;

        // Ensure info_bytes_slice is indeed a dictionary before proceeding
        let info_dict_map = match info_bytes_slice {
            BencodeValue::Dictionary(dict) => dict, // `dict` here is `&BTreeMap`
            _ => return Err(TorrentError::InvalidFieldType("info is not a dictionary")),
        };

        let mut encoder_buf = Vec::new();
        // Pass the reference to the info dictionary directly to the encoder.
        // `info_bytes_slice` is already `&BencodeValue`.
        encode_bencode_value(info_bytes_slice, &mut encoder_buf)?;
        let info_bencoded_raw = encoder_buf;

        let info_hash: [u8; 20] = get_sha1(&info_bencoded_raw);
        let info_hash_urlencoded = percent_encoding(&info_hash);

        // --- Decode 'info' dictionary content ---
        // `info_dict_map` is already `&BTreeMap` from the match above, so we can use it directly.

        let name_bytes = info_dict_map
            .get(b"name".as_ref())
            .ok_or(TorrentError::MissingField("info.name"))?;
        let name = match name_bytes {
            BencodeValue::ByteString(b) => std::str::from_utf8(b)
                .map_err(|_| TorrentError::Utf8ConversionError("info.name"))?
                .to_string(),
            _ => return Err(TorrentError::InvalidFieldType("info.name")),
        };

        let mut total_length: u64 = 0;
        let mut is_private = false;
        let mut encoding_option: Option<String> = None;

        // Handle 'length' for single-file torrents
        if let Some(BencodeValue::Integer(len)) = info_dict_map.get(b"length".as_ref()) {
            if *len < 0 {
                return Err(TorrentError::ParseError(
                    "info.length is negative".to_string(),
                ));
            }
            total_length = *len as u64;
        }

        // Handle 'files' for multi-file torrents
        if let Some(BencodeValue::List(files)) = info_dict_map.get(b"files".as_ref()) {
            total_length = 0; // Reset if 'files' is present, sum up
            for file_entry in files {
                if let BencodeValue::Dictionary(file_dict) = file_entry {
                    if let Some(BencodeValue::Integer(file_len)) = file_dict.get(b"length".as_ref())
                    {
                        if *file_len < 0 {
                            return Err(TorrentError::ParseError(
                                "file.length is negative".to_string(),
                            ));
                        }
                        total_length += *file_len as u64;
                    } else {
                        return Err(TorrentError::MissingField(
                            "file.length in multi-file torrent",
                        ));
                    }
                } else {
                    return Err(TorrentError::InvalidFieldType(
                        "file entry in multi-file torrent",
                    ));
                }
            }
        }

        // Handle 'private' flag
        if let Some(BencodeValue::Integer(private_val)) = info_dict_map.get(b"private".as_ref()) {
            is_private = *private_val == 1;
        }

        // Handle 'encoding'
        if let Some(BencodeValue::ByteString(encoding_bytes)) =
            top_level_dict.get(b"encoding".as_ref())
        {
            encoding_option = Some(
                std::str::from_utf8(encoding_bytes)
                    .map_err(|_| TorrentError::Utf8ConversionError("encoding"))?
                    .to_string(),
            );
        }

        Ok(Torrent {
            name,
            urls,
            length: total_length,
            private: is_private,
            uploaded: 0,                   // Default value
            last_announce: Instant::now(), // Default value
            info_hash,
            info_hash_urlencoded,
            seeders: 0,           // Default value
            leechers: 0,          // Default value
            next_upload_speed: 0, // Default value
            interval: 0,          // Default value
            error_count: 0,       // Default value
            encoding: encoding_option,
            min_interval: None, // Default value (from tracker response, not torrent file)
            tracker_id: None,   // Default value (from tracker response, not torrent file)
            source_path: None,  // Set by from_file() if loaded from disk
            // fastrand::u64(..) (full range) never panics; the per-torrent seed
            // decouples each torrent's speed curve from the others.
            speed_seed: fastrand::u64(..),
            origin: Instant::now(),
            dl_state: Self::initial_dl_state(total_length),
            dl_rate: Self::pick_dl_rate(),
            dl_last_tick: Instant::now(),
            completed_sent: false,
            upload_target: None,
            schedule_reason: 0,
        })
    }
}

// TODO: test tracker response "with d8:completei0e10:downloadedi0e10:incompletei1e8:intervali1922e12:min intervali961e5:peers6:<3A><><EFBFBD>m<EFBFBD><6D>e"
#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal Downloading torrent for download-phase tests (CONFIG-independent).
    fn dl_torrent(length: u64, rate: u64) -> Torrent {
        Torrent {
            name: "dl".into(),
            urls: vec![],
            length,
            private: false,
            uploaded: 0,
            last_announce: std::time::Instant::now(),
            info_hash: [0; 20],
            info_hash_urlencoded: String::new(),
            seeders: 4,
            leechers: 16,
            next_upload_speed: 0,
            interval: 1800,
            error_count: 0,
            encoding: None,
            min_interval: None,
            tracker_id: None,
            source_path: None,
            speed_seed: 0,
            origin: std::time::Instant::now(),
            dl_state: Torrent::initial_dl_state(length),
            dl_rate: rate.max(1),
            dl_last_tick: std::time::Instant::now(),
            completed_sent: false,
            upload_target: None,
            schedule_reason: 0,
        }
    }

    #[test]
    fn advance_download_monotone_clamps_and_completes_once() {
        let mut t = dl_torrent(1000, 400);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let done = t.advance_download(); // ~440 bytes, not done
        assert!(!done);
        assert!(
            matches!(t.dl_state, DownloadState::Downloading { downloaded } if downloaded > 0 && downloaded < 1000)
        );
        // Force completion: a long elapsed crosses length.
        t.dl_rate = 1_000_000;
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(t.advance_download()); // crosses -> Seeding, returns true ONCE
        assert!(t.is_seeding());
        assert_eq!(t.declared_downloaded(), 1000);
        assert_eq!(t.declared_left(), 0);
        assert!(!t.advance_download()); // idempotent once Seeding
    }

    #[test]
    fn zero_length_starts_seeding() {
        let mut t = dl_torrent(0, 1000);
        assert!(t.is_seeding());
        assert!(!t.advance_download());
        assert_eq!(t.declared_left(), 0);
        assert_eq!(t.declared_downloaded(), 0);
    }

    #[test]
    fn declared_left_never_underflows() {
        for d in [0u64, 500, 1000] {
            let mut t = dl_torrent(1000, 1);
            t.dl_state = if d >= 1000 {
                DownloadState::Seeding
            } else {
                DownloadState::Downloading { downloaded: d }
            };
            assert_eq!(t.declared_downloaded() + t.declared_left(), 1000);
        }
    }

    #[test]
    fn can_upload_requires_seeding() {
        let mut t = dl_torrent(1000, 1); // Downloading, 16 leechers
        assert!(!t.can_upload(), "must not upload while downloading");
        t.dl_state = DownloadState::Seeding;
        assert!(t.can_upload(), "can upload once seeding with leechers");
    }

    #[test]
    fn test_speed_curve_integral_matches_trapezoid_and_bounds() {
        // CONFIG is a global OnceCell; set it once (tolerate "already set" - we
        // read min/max back from whatever won the race).
        let cfg = crate::config::Config {
            min_upload_rate: 1_048_576,  // 1 MiB/s
            max_upload_rate: 10_485_760, // 10 MiB/s
            ..crate::config::Config::default()
        };
        crate::CONFIG.store(std::sync::Arc::new(cfg));
        let cfg = crate::CONFIG.load();
        let (min, max) = (cfg.min_upload_rate as f64, cfg.max_upload_rate as f64);

        let mut t = Torrent {
            name: String::from("curve"),
            length: 262144,
            private: false,
            uploaded: 0,
            last_announce: std::time::Instant::now(),
            info_hash: [0; 20],
            info_hash_urlencoded: String::from("01234567"),
            seeders: 4,
            leechers: 16, // can_upload() == true
            next_upload_speed: 0,
            interval: 1800,
            urls: Vec::new(),
            error_count: 0,
            encoding: None,
            min_interval: None,
            tracker_id: None,
            source_path: None,
            speed_seed: 0xDEAD_BEEF_CAFE_F00D,
            origin: std::time::Instant::now(),
            dl_state: DownloadState::Seeding,
            dl_rate: 1_048_576,
            dl_last_tick: std::time::Instant::now(),
            completed_sent: false,
            upload_target: None,
            schedule_reason: 0,
        };
        assert!(t.can_upload());

        // (a) Closed-form integral vs a fine trapezoidal numeric integral over a
        // long, fractional window crossing several oscillation periods.
        let (t0, t1) = (3.5_f64, 1803.5_f64);
        let n = 1_000_000usize;
        let dt = (t1 - t0) / n as f64;
        let mut trap = 0.5 * (t.speed_at(t0) + t.speed_at(t1));
        for k in 1..n {
            trap += t.speed_at(t0 + k as f64 * dt);
        }
        trap *= dt;
        let exact = t.integrate(t0, t1);
        let rel_err = (exact - trap).abs() / trap.max(1.0);
        assert!(
            rel_err < 1e-3,
            "exact={exact} trap={trap} rel_err={rel_err}"
        );
        assert!(exact > 0.0);

        // (b) Bounds hold for a dense sample across many periods.
        for k in 0..50_000u32 {
            let tt = k as f64 * 0.05; // 0..2500s
            let s = t.speed_at(tt);
            assert!(s >= min - 1.0 && s <= max + 1.0, "t={tt} s={s}");
        }

        // (c) Degenerate windows declare nothing.
        assert_eq!(t.integrate(10.0, 10.0), 0.0);
        assert_eq!(t.integrate(20.0, 10.0), 0.0);

        // (d) Gating: no leechers => zero speed and zero area.
        t.seeders = 0;
        t.leechers = 0;
        assert!(!t.can_upload());
        assert_eq!(t.speed_at(123.4), 0.0);
        assert_eq!(t.integrate(0.0, 1800.0), 0.0);
    }

    #[test]
    fn test_can_download_or_upload() {
        let mut t = Torrent {
            name: String::from("Test torrent"),
            length: 262144,
            private: false,
            uploaded: 0,
            last_announce: std::time::Instant::now(),
            info_hash: [0; 20],
            info_hash_urlencoded: String::from("01234567"),
            seeders: 0,
            leechers: 1,
            next_upload_speed: 0,
            interval: 1800,
            urls: Vec::with_capacity(0),
            error_count: 0,
            encoding: None,
            min_interval: None,
            tracker_id: None,
            source_path: None,
            speed_seed: 0,
            origin: std::time::Instant::now(),
            dl_state: DownloadState::Seeding,
            dl_rate: 1_048_576,
            dl_last_tick: std::time::Instant::now(),
            completed_sent: false,
            upload_target: None,
            schedule_reason: 0,
        };
        assert!(!t.can_upload());
        t.leechers = 5;
        assert!(t.can_upload());
        t.leechers = 0;
        t.seeders = 1;
        assert!(!t.can_upload());
        t.seeders = 4;
        t.leechers = 8;
        assert!(t.can_upload());
    }

    #[test]
    fn test_get_average_speeds() {
        let mut t = Torrent {
            name: String::from("Test torrent"),
            length: 262144,
            private: false,
            uploaded: 0,
            last_announce: std::time::Instant::now(),
            info_hash: [0; 20],
            info_hash_urlencoded: String::from("01234567"),
            seeders: 4,
            leechers: 16,
            next_upload_speed: 0,
            interval: 1800,
            urls: Vec::with_capacity(0),
            error_count: 0,
            encoding: None,
            min_interval: None,
            tracker_id: None,
            source_path: None,
            speed_seed: 0,
            origin: std::time::Instant::now(),
            dl_state: DownloadState::Seeding,
            dl_rate: 1_048_576,
            dl_last_tick: std::time::Instant::now(),
            completed_sent: false,
            upload_target: None,
            schedule_reason: 0,
        };
        let speed = t.uploaded(16, 64);
        assert!(speed > 0);
        t.interval = 1;
        std::thread::sleep(std::time::Duration::from_secs(2));
        assert!((16..=64).contains(&speed));
        let speed = t.uploaded(16, 64);
        assert!((16..=64).contains(&speed));
    }
}
