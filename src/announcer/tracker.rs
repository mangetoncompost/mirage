use std::time::Duration;

use crate::bencode::{BencodeDecoder, BencodeValue};
use crate::torrent::Torrent;
use crate::ui::{EventKind, emit};
use crate::{CLIENT, CONFIG, TORRENTS};
use fake_torrent_client::Client;
use once_cell::sync::Lazy;
use reqwest::Client as ReqwestClient;
use tracing::{debug, error, info, trace, warn};
use url::{Host, Url};

/// Shared HTTP client - built once at first use and reused across all announces.
/// The User-Agent is NOT baked in here; it is injected per-request via the headers
/// returned by `client.get_query()`, so changing the emulated client profile with `k`
/// is reflected immediately without needing to rebuild the pool.
static HTTP_CLIENT: Lazy<ReqwestClient> = Lazy::new(|| {
    ReqwestClient::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .expect("Failed to build reqwest client")
});

/// Sane bounds for a tracker-supplied announce interval (seconds). The value is
/// a fully untrusted i64 from the tracker response: a negative number would cast
/// to ~u64::MAX (wedging the torrent's announce forever) and 0 would busy-spin
/// the scheduler. Clamp to [30s, 24h]; out-of-range / non-positive → 30 min.
pub const MIN_INTERVAL: u64 = 30;
pub const MAX_INTERVAL: u64 = 86_400;
const DEFAULT_INTERVAL: u64 = 1_800;

/// Clamp a tracker-supplied interval into the safe range.
pub fn clamp_interval(raw: i64) -> u64 {
    if raw <= 0 {
        DEFAULT_INTERVAL
    } else {
        (raw as u64).clamp(MIN_INTERVAL, MAX_INTERVAL)
    }
}

// pub fn print_request_error(code: u16) {
//     match code {
//         100 => error!("100 Invalid request, not a GET"),
//         101 => error!("101 Info hash is missing"),
//         102 => error!("102 Peer ID is missing"),
//         103 => error!("103 Port is missing"),
//         150 => error!("150 Info hash is not 20 bytes long"),
//         151 => error!("151 Invalid peer ID"),
//         152 => error!("152 Invalid numwant: requested more peers than allowed by tracker"),
//         // Sent only by trackers that do not automatically include new hashes into the database.
//         200 => error!("200 info_hash not found in the database"),
//         500 => error!("500 Client sent an eventless request before the specified time"),
//         900 => error!("500 Generic error"),
//         _ => warn!("Unknown error code: {code}"),
//     }
// }

/// The optional announce event.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event {
    /// The first request to tracker must include this value.
    Started = 2,
    /// Must be sent when the client becomes a seeder (finished downloading).
    /// Must NOT be present if the client started as a seeder.
    Completed = 1,
    /// Must be sent to tracker if the client is shutting down gracefully.
    Stopped = 3,
}

/// Extract a peer count from a `complete`/`incomplete` value that may be either
/// a bencode Integer (the textbook form) OR a ByteString of ASCII digits (some
/// non-conformant private trackers). Returns None if the key is absent/garbage.
fn bencode_count(v: Option<&BencodeValue>) -> Option<u64> {
    match v {
        Some(BencodeValue::Integer(n)) => Some((*n).max(0) as u64),
        Some(BencodeValue::ByteString(b)) => std::str::from_utf8(b)
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .map(|n| n.max(0) as u64),
        _ => None,
    }
}

/// Count peers from a compact ByteString (fixed-size records of `record_len`
/// bytes) or a non-compact List of peer dictionaries. `record_len` is 6 for
/// IPv4 (`peers`) and 18 for IPv6 (`peers6`). Used as a fallback leecher signal
/// for trackers that omit `incomplete`.
fn count_peers(v: Option<&BencodeValue>, record_len: usize) -> u64 {
    match v {
        Some(BencodeValue::ByteString(b)) if !b.is_empty() && b.len() % record_len == 0 => {
            (b.len() / record_len) as u64
        }
        Some(BencodeValue::List(list)) => list.len() as u64,
        _ => 0,
    }
}

/// After a STARTED announce, a real client doesn't wait the tracker's full
/// interval (often 30 min) before its first stats update - it reports progress
/// fairly soon. We mirror that: once the STARTED response tells us the swarm has
/// peers to "upload" to, schedule the *next* announce after a short randomized
/// warm-up delay instead of the full interval, so fake upload starts promptly.
/// The tracker's real interval is restored on that next announce. We never go
/// below the tracker's `min interval` if it sent one.
const WARMUP_MIN_SECS: u64 = 30;
const WARMUP_MAX_SECS: u64 = 90;

/// When the torrent can't upload yet (no leechers) but is alive on the tracker
/// (has seeders), re-check sooner than the full interval so a leecher that
/// appears is noticed quickly instead of up to ~1h later.
const RECHECK_NO_LEECHER_MIN_SECS: u64 = 300; // 5 min
const RECHECK_NO_LEECHER_MAX_SECS: u64 = 600; // 10 min

/// Shorten `t.interval` to `[lo, hi]` (randomized), but never below the
/// tracker's `min interval` and never LONGER than what the tracker asked for.
fn shorten_interval(t: &mut Torrent, lo: u64, hi: u64) -> u64 {
    let mut delay = fastrand::u64(lo..=hi);
    if let Some(min) = t.min_interval {
        delay = delay.max(min);
    }
    if delay < t.interval {
        t.interval = delay;
    }
    t.interval
}

/// Post-STARTED warm-up: if the torrent CAN upload (has leechers), schedule the
/// next announce in 30-90s so fake upload starts promptly instead of waiting the
/// tracker's full interval. Called ONCE after a STARTED announce. If it can't
/// upload yet, fall through to the no-leecher re-check below. Returns the wait.
pub fn apply_warmup(t: &mut Torrent) -> u64 {
    if t.can_upload() {
        // Stamp the cadence reason at the real decision point (not in the
        // scheduler branch), so a row freshly STARTED or retried is labelled
        // correctly in the Schedule ledger (F3.3).
        t.schedule_reason = crate::torrent::ScheduleReason::Warmup as u8;
        shorten_interval(t, WARMUP_MIN_SECS, WARMUP_MAX_SECS)
    } else {
        apply_recheck(t)
    }
}

/// Periodic re-check pacing: if the torrent still can't upload (0 leechers) but
/// the swarm is alive (has seeders), poll again in 5-10 min instead of waiting
/// the tracker's full interval (up to ~1h) - so a leecher that appears is caught
/// quickly. If it CAN upload, leave the tracker's interval as-is (normal cadence;
/// we must NOT keep hammering at warm-up speed, which risks a ban). Returns wait.
pub fn apply_recheck(t: &mut Torrent) -> u64 {
    if !t.can_upload() && (t.seeders > 0 || t.leechers > 0) {
        t.schedule_reason = crate::torrent::ScheduleReason::Recheck as u8;
        shorten_interval(t, RECHECK_NO_LEECHER_MIN_SECS, RECHECK_NO_LEECHER_MAX_SECS)
    } else {
        // Normal tracker rhythm: uploading fine, or no swarm to chase.
        t.schedule_reason = crate::torrent::ScheduleReason::Interval as u8;
        t.interval
    }
}

pub async fn announce_started() -> u64 {
    info!("Announcing torrent(s) with STARTED event");
    let list = TORRENTS.read().await;
    let mut wait_time = u64::MAX;
    for m in list.iter() {
        let mut t = m.lock().await;
        announce(&mut t, Some(Event::Started)).await;
        let wait = apply_warmup(&mut t);
        wait_time = wait_time.min(wait);
        info!("Time: {}", wait_time);
    }
    wait_time
}

pub async fn announce_stopped() {
    info!("Announcing torrent(s) with STOPPED event");
    let list = TORRENTS.read().await;
    let mut total_uploaded: u64 = 0;

    for m in list.iter() {
        let mut t = m.lock().await;
        announce(&mut t, Some(Event::Stopped)).await;
        total_uploaded += t.uploaded;
        info!(
            "Torrent \"{}\": uploaded={}, seeders={}, leechers={}, errors={}",
            t.name,
            crate::utils::format_bytes_u64(t.uploaded),
            t.seeders,
            t.leechers,
            t.error_count
        );
    }

    info!(
        "Session total: {} torrents, uploaded={}",
        list.len(),
        crate::utils::format_bytes_u64(total_uploaded)
    );
}

/// Check if the tracker URL is supported.
/// Supports HTTP, HTTPS, and UDP schemes.
/// Rejects .local TLDs (mDNS).
pub fn is_supported_url(url_str: &str) -> bool {
    let parsed_url = match Url::parse(url_str) {
        Ok(url) => url,
        Err(e) => {
            error!("Unable to parse URL: {url_str} {e}");
            return false;
        }
    };

    let host = match parsed_url.host() {
        Some(h) => h,
        None => {
            error!("No host in tracker URL: {url_str}");
            return false;
        }
    };

    // Check supported schemes
    let scheme = parsed_url.scheme();
    if scheme != "http" && scheme != "https" && scheme != "udp" {
        warn!("Unsupported tracker scheme: {}", scheme);
        return false;
    }

    match host {
        Host::Domain(domain_str) => {
            // For ".local", a simple split is sufficient, as ".local" is not a "public" TLD managed by the public
            // suffix list, but a pseudo-TLD for mDNS.
            let parts: Vec<&str> = domain_str.split('.').collect();
            if let Some(tld_candidate) = parts.last() {
                *tld_candidate != "local"
            } else {
                // no dot in domain, ex: "localhost" or just "myserver"
                warn!("Skipping, no dot in domain: {url_str}");
                false
            }
        }
        // IP addresses are supported
        Host::Ipv4(_) | Host::Ipv6(_) => true,
    }
}

/// Sends an announce request to the tracker with the specified parameters.
///
/// This may be used by a torrent to request peers to download from and to
/// report statistics to the tracker.
///
/// # Important
///
/// The tracker may not be contacted more often than the minimum interval
/// returned in the first announce response.
pub async fn announce(torrent: &mut Torrent, event: Option<Event>) {
    trace!(
        torrent = %torrent.name,
        event = ?event,
        uploaded = torrent.uploaded,
        seeders = torrent.seeders,
        leechers = torrent.leechers,
        interval = torrent.interval,
        elapsed = torrent.last_announce.elapsed().as_secs(),
        "→ announce called"
    );
    torrent.compute_speeds();
    trace!(
        torrent = %torrent.name,
        next_upload_speed = torrent.next_upload_speed,
        can_upload = torrent.can_upload(),
        "speeds computed"
    );
    if let Some(client) = &*CLIENT.read().await {
        debug!(
            torrent = %torrent.name,
            peer_id = %client.peer_id,
            user_agent = %client.user_agent,
            urls = torrent.urls.len(),
            "announcing with client"
        );
        emit(
            EventKind::AnnounceSent,
            &torrent.name,
            format!(
                "{} url(s){}",
                torrent.urls.len(),
                match event {
                    Some(e) => format!(" [{e:?}]"),
                    None => String::new(),
                }
            ),
        );
        // Announce to every tracker in the announce-list, but AGGREGATE the
        // peer counts instead of letting each response clobber the previous one.
        // A torrent with two trackers where the 2nd returns incomplete=0 would
        // otherwise wipe a valid leecher count from the 1st (→ no upload). We
        // keep the max seen across all trackers (a peer counted by any tracker
        // is real), then write it back once the loop is done.
        // Snapshot the upload window ONCE before the per-URL loop. After the first
        // successful announce resets torrent.last_announce, subsequent trackers
        // would see elapsed≈0 and declare uploaded=0. We pass the pre-computed
        // value into each announce call so every tracker gets the same honest delta.
        let pre_loop_uploaded: u64 =
            if matches!(event, Some(Event::Started) | Some(Event::Completed)) {
                0
            } else {
                let t1 = torrent.origin.elapsed().as_secs_f64();
                let t0 = (t1 - torrent.last_announce.elapsed().as_secs_f64()).max(0.0);
                torrent.integrate(t0, t1).round().max(0.0) as u64
            };
        let mut max_seeders: u16 = 0;
        let mut max_leechers: u16 = 0;
        for url in torrent.urls.clone() {
            debug!("\t{}", url);
            if url.to_lowercase().starts_with("udp://") {
                crate::announcer::udp::announce_udp(
                    &url,
                    torrent,
                    client,
                    event,
                    pre_loop_uploaded,
                )
                .await;
            } else {
                announce_http(&url, torrent, client, event, pre_loop_uploaded).await;
            }
            max_seeders = max_seeders.max(torrent.seeders);
            max_leechers = max_leechers.max(torrent.leechers);
        }
        torrent.seeders = max_seeders;
        torrent.leechers = max_leechers;
        info!(
            "Anounced: interval={}, event={:?}, downloaded={}, left={}, uploaded={}, seeders={}, leechers={}, torrent={}",
            torrent.interval,
            event,
            torrent.declared_downloaded(),
            torrent.declared_left(),
            torrent.uploaded,
            torrent.seeders,
            torrent.leechers,
            torrent.name
        );
    }
}

// /// Check which torrents need to be announced and call the announce fuction when applicable
// pub fn check_and_announce() {
//     let list = TORRENTS.read().expect("Cannot get torrent list");
//     for m in list.iter() {
//         let mut t = m.lock().unwrap();
//         if t.shound_announce() {
//             announce(&mut t, None);
//         }
//     }
// }

async fn announce_http(
    url: &str,
    torrent: &mut Torrent,
    client: &Client,
    event: Option<Event>,
    pre_loop_uploaded: u64,
) -> u64 {
    // announce parameters are built up in the query string, see:
    // https://www.bittorrent.org/beps/bep_0003.html trackers section
    // let mut query = vec![
    //     ("port", params.port.to_string()),
    //     ("downloaded", params.downloaded.to_string()),
    //     ("uploaded", params.uploaded.to_string()),
    //     ("left", params.left.to_string()),
    //     // Indicates that client accepts a compact response (each peer takes
    //     // up only 6 bytes where the first four bytes constitute the IP
    //     // address and the last 2 the port number, in Network Byte Order).
    //     // The is always true to save network traffic (many trackers don't
    //     // consider this and send compact lists anyway).
    //     ("compact", "1".to_string()),
    // ];
    // if let Some(peer_count) = params.peer_count {
    //     query.push(("numwant", peer_count.to_string()));
    // }
    // if let Some(ip) = &params.ip {
    //     query.push(("ip", ip.to_string()));
    // }

    // hack:
    // reqwest uses serde_urlencoded which doesn't support encoding a raw
    // byte array into a percent encoded string. However, the tracker
    // expects the url encoded form of the raw info hash, so we need to be
    // able to map the raw bytes to its url encoded form. The peer id is
    // also stored as a raw byte array. Using `String::from_utf8_lossy`
    // would cause information loss.
    //
    // We do this using the separate percent_encoding crate, and by
    // "hard-coding" the info hash and the peer id into the url string. This
    // is the only way in which reqwest doesn't url encode again the custom
    // url encoded info hash. All other methods, such as mutating the query
    // parameters on the `Url` object, or by serializing the info hash with
    // `serde_bytes` do not work: they throw an error due to expecting valid
    // utf8.
    //
    // However, this is decidedly _not_ great: we're relying on an
    // undocumented edge case of a third party library (reqwest) that may
    // very well break in a future update.
    // let url = format!(
    //     "{url}\
    //     ?info_hash={info_hash}\
    //     &peer_id={peer_id}",
    //     url = url,
    //     info_hash = percent_encoding::percent_encode(&params.info_hash, URL_ENCODE_RESERVED),
    //     peer_id = percent_encoding::percent_encode(&params.peer_id, URL_ENCODE_RESERVED),
    // );

    // headers_to_set carries the client's User-Agent/Accept/Accept-Encoding.
    // (The query template is built inside build_url, not from here.)
    let (_url_template, headers_to_set) = client.get_query();
    // Inject the key as 8 uppercase hex (constant per session), like a real
    // Transmission - not the decimal `client.key.to_string()` used before.
    let key_hex = crate::config::KEY_HEX.load().as_str().to_string();
    let (built_url, uploaded) =
        build_url(url, torrent, client, event, key_hex, pre_loop_uploaded).await;
    info!("Announce HTTP URL {built_url}");

    let mut request_builder = HTTP_CLIENT.get(&built_url);

    for (name, value) in headers_to_set {
        request_builder = request_builder.header(&name, &value);
    }

    match request_builder.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            info!(
                "\tTime since last announce: {}s \t interval: {}",
                torrent.last_announce.elapsed().as_secs(),
                torrent.interval
            );

            // read response body
            let bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    error!("Failed to read response bytes: {:?}", e);
                    emit(EventKind::Error, &torrent.name, format!("body read: {e}"));
                    torrent.error_count = torrent.error_count.saturating_add(1);
                    return torrent.interval; // return current interval
                }
            };
            let bytes_vec = bytes.to_vec(); //convert Bytes to Vec<u8>

            // we start to check if the tracker has returned an error message, if yes, we will reannounce later
            trace!(
                status = status,
                bytes = bytes_vec.len(),
                body = %String::from_utf8_lossy(&bytes_vec),
                "raw tracker response"
            );

            // Bencode decoding
            let mut decoder = BencodeDecoder::new(&bytes_vec);
            match decoder.decode() {
                Ok(bv) => {
                    match bv {
                        BencodeValue::Dictionary(dict) => {
                            if let Some(BencodeValue::ByteString(msg)) =
                                dict.get(b"failure reason".as_ref())
                            {
                                // If present, then no other keys may be present. The value is a human-readable error message as to why the request failed
                                error!("Cannot announce: {:?}", std::str::from_utf8(msg));
                                emit(
                                    EventKind::Error,
                                    &torrent.name,
                                    format!("tracker: {}", String::from_utf8_lossy(msg)),
                                );
                                torrent.error_count = torrent.error_count.saturating_add(1);
                            } else {
                                // Check for warning message (response still gets processed normally)
                                if let Some(BencodeValue::ByteString(msg)) =
                                    dict.get(b"warning message".as_ref())
                                {
                                    warn!("Announce with warning: {:?}", std::str::from_utf8(msg));
                                }

                                // Process response fields
                                // Interval in seconds that the client should wait between sending regular requests to the tracker
                                if let Some(BencodeValue::Integer(interval)) =
                                    dict.get(b"interval".as_ref())
                                {
                                    // Clamp the tracker-supplied (fully untrusted, i64) interval:
                                    // a negative value casts to ~u64::MAX and would wedge this
                                    // torrent's announce forever; 0 would busy-spin the scheduler.
                                    torrent.interval = clamp_interval(*interval);
                                }

                                // (optional) Minimum announce interval. If present clients must not reannounce more frequently than this.
                                if let Some(BencodeValue::Integer(mi)) =
                                    dict.get(b"min interval".as_ref())
                                {
                                    torrent.min_interval = Some(clamp_interval(*mi));
                                }

                                // A string that the client should send back on its next announcements. If absent and
                                // a previous announce sent a tracker id, do not discard the old value; keep using it.
                                if let Some(BencodeValue::ByteString(tid)) =
                                    dict.get(b"tracker_id".as_ref())
                                {
                                    match std::str::from_utf8(tid) {
                                        Ok(tracker_id) => {
                                            torrent.tracker_id = Some(tracker_id.to_string())
                                        }
                                        Err(e) => error!("Unable to decode tracker_id: {:?}", e),
                                    }
                                }

                                // Robustly extract seeders/leechers. Many (esp.
                                // private) trackers diverge from the textbook
                                // `complete`/`incomplete` integers:
                                //   - they OMIT incomplete (or complete) and only
                                //     ship a `peers` list/blob → we must COUNT it;
                                //   - they send the counts as STRINGS not integers;
                                //   - they ship IPv6 peers in `peers6`.
                                // Mirage used to read only the integer keys and
                                // ignore `peers`, so such trackers showed L:0 and
                                // never uploaded. We now take the MAX of the
                                // declared count and the actual peer count.
                                let complete = bencode_count(dict.get(b"complete".as_ref()));
                                let incomplete = bencode_count(dict.get(b"incomplete".as_ref()));
                                let peer_count = count_peers(dict.get(b"peers".as_ref()), 6)
                                    .saturating_add(count_peers(dict.get(b"peers6".as_ref()), 18));

                                if let Some(s) = complete {
                                    torrent.seeders = s.min(u16::MAX as u64) as u16;
                                }
                                // Leechers = max(declared incomplete, peers we can
                                // see). If incomplete is absent, the peer count is
                                // the only signal; if present, peers can only
                                // confirm/raise it (compact peer lists are usually
                                // leechers from the tracker's perspective).
                                let declared_leechers = incomplete.unwrap_or(0);
                                let leechers = declared_leechers.max(peer_count);
                                if incomplete.is_some() || peer_count > 0 {
                                    torrent.leechers = leechers.min(u16::MAX as u64) as u16;
                                }

                                // Accumulate the fake uploaded bytes we just
                                // declared to the tracker (mirrors the UDP path,
                                // which does the same on a successful announce).
                                torrent.uploaded += uploaded;

                                // Reset last_announce and error_count on successful response
                                torrent.last_announce = std::time::Instant::now();
                                torrent.error_count = 0;
                                // A `completed` is delivered exactly once SUCCESSFULLY:
                                // mark it only on success so a failed completed is retried.
                                if event == Some(Event::Completed) {
                                    torrent.completed_sent = true;
                                }
                                if uploaded > 0 {
                                    emit(
                                        EventKind::UploadTick,
                                        &torrent.name,
                                        format!("+{}", crate::utils::format_bytes_u64(uploaded)),
                                    );
                                }
                                emit(
                                    EventKind::PeersUpdated,
                                    &torrent.name,
                                    format!(
                                        "S:{} L:{} int:{}s",
                                        torrent.seeders, torrent.leechers, torrent.interval
                                    ),
                                );
                            }
                        }
                        _ => {
                            error!("Response is not a dictionary");
                            emit(EventKind::Error, &torrent.name, "not a dictionary");
                            torrent.error_count = torrent.error_count.saturating_add(1);
                        }
                    }
                }
                Err(e) => {
                    error!("Bad response with HTTP status {status}: {:?}", e);
                    emit(
                        EventKind::Error,
                        &torrent.name,
                        format!("decode (HTTP {status})"),
                    );
                    torrent.error_count = torrent.error_count.saturating_add(1);
                }
            }
        }
        Err(err) => {
            error!("Cannot announce: {:?}", err);
            emit(EventKind::Error, &torrent.name, format!("HTTP fail: {err}"));
            torrent.error_count = torrent.error_count.saturating_add(1);
        }
    }
    // On error (or a tracker that returned no interval) torrent.interval stays 0,
    // which would make the scheduler retry at the 5 s floor every loop. Apply a
    // conservative backoff so a failing tracker is retried at most every ~60 s.
    if torrent.interval == 0 && torrent.error_count > 0 {
        torrent.interval = 60;
    }
    if let Some(min) = torrent.min_interval
        && min > torrent.interval
    {
        return min;
    }
    torrent.interval
}

/// Build the HTTP announce URLs for the listed trackers in the torrent file.
/// It prepares the annonce query by replacing variables (port, numwant, ...) with the computed values.
/// Returns the built URL and the `uploaded` byte count it declares to the tracker
/// (so the caller can accumulate the same value into `torrent.uploaded`).
pub async fn build_url(
    url: &str,
    torrent: &mut Torrent,
    client: &Client,
    event: Option<Event>,
    key: String,
    // Pre-computed upload delta for this announce round (snapshotted by the
    // caller before the per-URL loop so every tracker gets the same window).
    uploaded: u64,
) -> (String, u64) {
    info!("Torrent {:?}: {}", event, torrent.name);
    // `uploaded` is the pre-computed delta passed in by announce() before the
    // per-URL loop - see the comment there. This function no longer recomputes
    // it from last_announce so that every tracker in the list receives the
    // same declared window.

    // The caller (announce → announce_http) already holds the CLIENT read guard
    // and passes the borrow down. We must NOT re-acquire CLIENT.read() here: the
    // tokio RwLock is write-preferring, so a key-renewer/ReinitClient writer that
    // queues between the outer read (in announce) and a read here would deadlock
    // the announce task forever.
    let cfg = CONFIG.load();
    let port = cfg.port;
    // numwant = 0 on STOPPED (like a real Transmission), else the config override
    // or the client profile's num_want (80 for Transmission). Deriving from the
    // profile means future client profiles auto-track.
    let numwant: u16 = if event == Some(Event::Stopped) {
        client.num_want_on_stop
    } else {
        cfg.numwant.unwrap_or(client.num_want)
    };
    let mut result = String::from(url);
    result.push(if result.contains('?') { '&' } else { '?' });
    result.push_str(&client.query);

    // The `event` parameter must carry a real value (started/stopped/completed)
    // or be OMITTED entirely. An empty `event=` is rejected by many trackers
    // (e.g. Ubuntu's returns HTTP 400 "invalid event"), so for periodic
    // announces (event = None) we strip the whole `event={event}` token rather
    // than leaving it blank.
    let result = match event {
        Some(e) => result.replace(
            "{event}",
            match e {
                Event::Started => "started",
                Event::Completed => "completed",
                Event::Stopped => "stopped",
            },
        ),
        None => result
            .replace("&event={event}", "")
            .replace("event={event}&", "")
            .replace("event={event}", ""),
    };

    // Declare the simulated download progress: downloaded grows, left shrinks
    // until the download phase finishes (then downloaded==length, left==0).
    let result = result
        .replace("{infohash}", &torrent.info_hash_urlencoded)
        .replace("{key}", &key)
        .replace("{uploaded}", uploaded.to_string().as_str())
        .replace("{downloaded}", &torrent.declared_downloaded().to_string())
        .replace("{peerid}", &client.peer_id)
        .replace("{port}", &port.to_string())
        .replace("{numwant}", &numwant.to_string())
        // Strip the ipv6 param robustly (we don't announce an IPv6 address):
        // the param with a leading or trailing separator, then the bare token,
        // like the {event} strip above. A leftover {ipv6} would otherwise ship a
        // literal placeholder to the tracker if the client template changes.
        .replace("&ipv6={ipv6}", "")
        .replace("ipv6={ipv6}&", "")
        .replace("ipv6={ipv6}", "")
        .replace("{ipv6}", "")
        .replace("{left}", &torrent.declared_left().to_string());
    // info!(
    //     "\tUploaded: {}",
    //     byte_unit::Byte::from_u128(uploaded as u128)
    //         .unwrap()
    //         .get_appropriate_unit(byte_unit::UnitType::Decimal)
    //         .to_string()
    // );
    info!("\tAnnonce at: {}", url);
    (result, uploaded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bencode::BencodeDecoder;

    #[test]
    fn interval_clamp_rejects_hostile_values() {
        // Negative would cast to ~u64::MAX (permanent wedge) → default.
        assert_eq!(clamp_interval(-1), DEFAULT_INTERVAL);
        // Zero would busy-spin the scheduler → default.
        assert_eq!(clamp_interval(0), DEFAULT_INTERVAL);
        // Too small → floored; too large → capped.
        assert_eq!(clamp_interval(1), MIN_INTERVAL);
        assert_eq!(clamp_interval(10_000_000), MAX_INTERVAL);
        // A normal value passes through.
        assert_eq!(clamp_interval(1800), 1800);
    }

    /// Decode a bencode dict and extract (seeders, leechers) the way announce_http
    /// now does, so we can assert the robust parsing across real-world tracker
    /// response shapes (missing incomplete, peers list, string ints, ipv6).
    fn parse_counts(body: &[u8]) -> (u64, u64) {
        let mut dec = BencodeDecoder::new(body);
        let dict = match dec.decode().unwrap() {
            BencodeValue::Dictionary(d) => d,
            _ => panic!("not a dict"),
        };
        let complete = bencode_count(dict.get(b"complete".as_ref())).unwrap_or(0);
        let incomplete = bencode_count(dict.get(b"incomplete".as_ref()));
        let peer_count = count_peers(dict.get(b"peers".as_ref()), 6)
            .saturating_add(count_peers(dict.get(b"peers6".as_ref()), 18));
        let leechers = incomplete.unwrap_or(0).max(peer_count);
        (complete, leechers)
    }

    #[test]
    fn test_leecher_parsing_robust_across_tracker_shapes() {
        // Textbook: complete + incomplete integers.
        assert_eq!(
            parse_counts(b"d8:completei247e10:incompletei3e8:intervali1800ee"),
            (247, 3)
        );

        // Private tracker that OMITS incomplete and only ships a compact peers
        // blob (3 IPv4 peers = 18 bytes). Must count the peers as leechers.
        let mut body = Vec::new();
        body.extend_from_slice(b"d8:intervali1800e5:peers18:");
        body.extend_from_slice(&[0u8; 18]);
        body.push(b'e');
        assert_eq!(parse_counts(&body), (0, 3));

        // peers as a non-compact LIST of 2 dicts, no incomplete.
        // ("10.0.0.1" is 8 chars → 8:10.0.0.1)
        let listbody = b"d8:completei247e8:intervali1800e5:peersld2:ip8:10.0.0.14:porti1eed2:ip8:10.0.0.24:porti2eeee";
        assert_eq!(parse_counts(listbody), (247, 2));

        // counts sent as STRINGS instead of integers (non-conformant tracker).
        // ("incomplete" is 10 chars → 10:incomplete)
        assert_eq!(parse_counts(b"d8:complete3:24710:incomplete1:3e"), (247, 3));

        // IPv6 peers6 (18 bytes = 1 peer), empty peers.
        let mut v6 = Vec::new();
        v6.extend_from_slice(b"d8:completei247e5:peers0:6:peers618:");
        v6.extend_from_slice(&[0u8; 18]);
        v6.push(b'e');
        assert_eq!(parse_counts(&v6), (247, 1));

        // declared incomplete wins when larger than visible peers.
        let mut mix = Vec::new();
        mix.extend_from_slice(b"d10:incompletei50e5:peers6:");
        mix.extend_from_slice(&[0u8; 6]);
        mix.push(b'e');
        assert_eq!(parse_counts(&mix), (0, 50));
    }

    /// Reproduces the `{event}` substitution rule build_url applies, on the real
    /// query template shipped by fake-torrent-client. A periodic announce
    /// (event = None) must NOT leave an empty `event=` (trackers reject it);
    /// it must be omitted entirely.
    fn substitute_event(query: &str, event: Option<Event>) -> String {
        match event {
            Some(e) => query.replace(
                "{event}",
                match e {
                    Event::Started => "started",
                    Event::Completed => "completed",
                    Event::Stopped => "stopped",
                },
            ),
            None => query
                .replace("&event={event}", "")
                .replace("event={event}&", "")
                .replace("event={event}", ""),
        }
    }

    #[test]
    fn test_event_param_omitted_when_none() {
        // The exact template fake-torrent-client uses (event in the middle).
        let q = "uploaded={uploaded}&key={key}&event={event}&numwant={numwant}&compact=1";
        let started = substitute_event(q, Some(Event::Started));
        assert!(started.contains("event=started"), "started: {started}");

        let periodic = substitute_event(q, None);
        // No empty event= must remain, and the rest of the query stays intact.
        assert!(!periodic.contains("event="), "must omit event=: {periodic}");
        assert!(
            periodic.contains("key={key}&numwant={numwant}"),
            "{periodic}"
        );

        // event at the very end of the query.
        let q_end = "uploaded={uploaded}&numwant={numwant}&event={event}";
        let periodic_end = substitute_event(q_end, None);
        assert!(!periodic_end.contains("event="), "{periodic_end}");
        assert!(
            periodic_end.ends_with("numwant={numwant}"),
            "{periodic_end}"
        );

        let stopped = substitute_event(q, Some(Event::Stopped));
        assert!(stopped.contains("event=stopped"), "{stopped}");
    }

    #[test]
    pub fn test_supported_url() {
        // HTTP and HTTPS
        assert!(is_supported_url("http://localhost/?param=test"));
        assert!(is_supported_url("https://localhost/?param=test"));
        assert!(is_supported_url("http://another-host/?param=test"));
        assert!(is_supported_url("http://some-host.tld/?param=test"));
        assert!(is_supported_url("https://some-host.tld/?param=test"));

        // UDP is now supported
        assert!(is_supported_url("udp://tracker.example.com:1337/announce"));
        assert!(is_supported_url("udp://udp-host.tld:6969/announce"));

        // .local TLD should be rejected
        assert!(!is_supported_url("http://myserver.local/announce"));
        assert!(!is_supported_url("udp://tracker.local:6969/announce"));

        // IP addresses are supported
        assert!(is_supported_url("http://192.168.1.1:8080/announce"));
        assert!(is_supported_url("udp://192.168.1.1:6969/announce"));

        // Unsupported schemes
        assert!(!is_supported_url("wss://tracker.example.com/announce"));
    }
}
