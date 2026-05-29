use std::time::Duration;

use crate::bencode::{BencodeDecoder, BencodeValue};
use crate::torrent::Torrent;
use crate::ui::{EventKind, emit};
use crate::{CLIENT, CONFIG, TORRENTS};
use fake_torrent_client::Client;
use reqwest::Client as ReqwestClient;
use tracing::{debug, error, info, trace, warn};
use url::{Host, Url};

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
    // /// Must be sent to the tracker when the client becomes a seeder. Must not be
    // /// present if the client started as a seeder.
    // Completed,
    /// Must be sent to tracker if the client is shutting down gracefully.
    Stopped,
}

/// After a STARTED announce, a real client doesn't wait the tracker's full
/// interval (often 30 min) before its first stats update — it reports progress
/// fairly soon. We mirror that: once the STARTED response tells us the swarm has
/// peers to "upload" to, schedule the *next* announce after a short randomized
/// warm-up delay instead of the full interval, so fake upload starts promptly.
/// The tracker's real interval is restored on that next announce. We never go
/// below the tracker's `min interval` if it sent one.
const WARMUP_MIN_SECS: u64 = 30;
const WARMUP_MAX_SECS: u64 = 90;

/// Clamp `t.interval` to a short warm-up window if the torrent can now upload,
/// so the scheduler re-announces soon after STARTED. Returns the effective wait.
pub fn apply_warmup(t: &mut Torrent) -> u64 {
    if t.can_upload() {
        let mut warmup = fastrand::u64(WARMUP_MIN_SECS..=WARMUP_MAX_SECS);
        // Honor the tracker's minimum announce interval, if any.
        if let Some(min) = t.min_interval {
            warmup = warmup.max(min);
        }
        // Only shorten — never lengthen past what the tracker asked for.
        if warmup < t.interval {
            t.interval = warmup;
        }
    }
    t.interval
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
        for url in torrent.urls.clone() {
            debug!("\t{}", url);
            if url.to_lowercase().starts_with("udp://") {
                crate::announcer::udp::announce_udp(&url, torrent, client, event).await;
            } else {
                announce_http(&url, torrent, client, event).await;
            }
        }
        info!(
            "Anounced: interval={}, event={:?}, downloaded=0, uploaded={}, seeders={}, leechers={}, torrent={}",
            torrent.interval,
            event,
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

    let reqwest_client = ReqwestClient::builder()
        .user_agent(&client.user_agent)
        .timeout(Duration::from_secs(60)) // Timeout pour la connexion et la lecture
        .build()
        .expect("Failed to build reqwest client");

    let (url_template, headers_to_set) = client.get_query();
    let mut full_url = String::from(url);
    full_url.push(if full_url.contains('?') { '&' } else { '?' });
    full_url.push_str(&url_template);
    let (built_url, uploaded) = build_url(url, torrent, event, client.key.clone().to_string()).await;
    info!("Announce HTTP URL {built_url}");

    let mut request_builder = reqwest_client.get(&built_url);

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
                    torrent.error_count += 1;
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
                                torrent.error_count += 1;
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
                                    torrent.interval = *interval as u64;
                                }

                                // (optional) Minimum announce interval. If present clients must not reannounce more frequently than this.
                                if let Some(BencodeValue::Integer(mi)) =
                                    dict.get(b"min interval".as_ref())
                                {
                                    torrent.min_interval = Some(*mi as u64);
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

                                // number of peers with the entire file, i.e. seeders (integer)
                                if let Some(BencodeValue::Integer(value)) =
                                    dict.get(b"complete".as_ref())
                                {
                                    torrent.seeders = *value as u16;
                                }

                                // number of leechers (integer)
                                if let Some(BencodeValue::Integer(value)) =
                                    dict.get(b"incomplete".as_ref())
                                {
                                    torrent.leechers = *value as u16;
                                }

                                // b"peers" not handled

                                // Accumulate the fake uploaded bytes we just
                                // declared to the tracker (mirrors the UDP path,
                                // which does the same on a successful announce).
                                torrent.uploaded += uploaded;

                                // Reset last_announce and error_count on successful response
                                torrent.last_announce = std::time::Instant::now();
                                torrent.error_count = 0;
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
                            torrent.error_count += 1;
                        }
                    }
                }
                Err(e) => {
                    error!("Bad response with HTTP status {status}: {:?}", e);
                    emit(EventKind::Error, &torrent.name, format!("decode (HTTP {status})"));
                    torrent.error_count += 1;
                }
            }
        }
        Err(err) => {
            error!("Cannot announce: {:?}", err);
            emit(EventKind::Error, &torrent.name, format!("HTTP fail: {err}"));
            torrent.error_count += 1;
        }
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
    event: Option<Event>,
    key: String,
) -> (String, u64) {
    info!("Torrent {:?}: {}", event, torrent.name);
    // Declared upload = exact integral of the time-varying speed curve over the
    // window since the last announce (the area under the curve the dashboard has
    // been showing). STARTED declares 0, like a real client. The window is
    // derived from last_announce, so it is idempotent across the per-URL loop
    // (re-integrates the same [t0,t1] until last_announce resets on success).
    let uploaded: u64 = if event == Some(Event::Started) {
        0
    } else {
        let t1 = torrent.origin.elapsed().as_secs_f64();
        let t0 = (t1 - torrent.last_announce.elapsed().as_secs_f64()).max(0.0);
        torrent.integrate(t0, t1).round().max(0.0) as u64
    };

    //build URL list
    let client = (*CLIENT.read().await).clone().unwrap();
    let mut port = 55555u16;
    let mut numwant = 80u16;
    if let Some(config) = CONFIG.get() {
        port = config.port;
        if let Some(nw) = config.numwant {
            numwant = nw;
        }
    }
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
                // Event::Completed => "completed",
                Event::Stopped => "stopped",
            },
        ),
        None => result
            .replace("&event={event}", "")
            .replace("event={event}&", "")
            .replace("event={event}", ""),
    };

    let result = result
        .replace("{infohash}", &torrent.info_hash_urlencoded)
        .replace("{key}", &key)
        .replace("{uploaded}", uploaded.to_string().as_str())
        .replace("{downloaded}", "0")
        .replace("{peerid}", &client.peer_id)
        .replace("{port}", &port.to_string())
        .replace("{numwant}", &numwant.to_string())
        .replace("ipv6={ipv6}", "")
        .replace("{left}", "0");
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
        assert!(periodic.contains("key={key}&numwant={numwant}"), "{periodic}");

        // event at the very end of the query.
        let q_end = "uploaded={uploaded}&numwant={numwant}&event={event}";
        let periodic_end = substitute_event(q_end, None);
        assert!(!periodic_end.contains("event="), "{periodic_end}");
        assert!(periodic_end.ends_with("numwant={numwant}"), "{periodic_end}");

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
