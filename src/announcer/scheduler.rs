use tracing::{debug, info, trace};

use crate::TORRENTS;
use crate::torrent::Torrent;
use tokio::time::Duration;

use super::tracker::Event;

/// Re-announce cadence while a torrent is in its simulated download phase, so
/// `downloaded` grows smoothly and progress is reported between the tracker's
/// sparse (~30 min) intervals instead of jumping at announce points.
const DL_TICK_SECS: u64 = 45;

/// Next download-phase tick for a torrent: min(DL_TICK, ETA to finish) so the
/// `completed` event fires NEAR the true crossing rather than up to a tick late.
fn dl_interval(t: &Torrent) -> u64 {
    let left = t.declared_left();
    let rate = t.dl_rate.max(1);
    let eta = left / rate; // seconds to finish at the fixed rate
    let base = DL_TICK_SECS.min(eta.max(1));
    add_jitter(base)
}

/// Add jitter (±5%) to an interval to prevent thundering herd effect.
/// Multiple torrents with similar intervals will announce at slightly different times.
fn add_jitter(interval: u64) -> u64 {
    if interval < 20 {
        // Don't add jitter to very short intervals
        return interval;
    }
    // Calculate 5% of the interval
    let jitter_range = interval / 20; // 5%
    // Random offset between -jitter_range and +jitter_range
    let offset = fastrand::u64(0..=jitter_range * 2);
    interval.saturating_sub(jitter_range).saturating_add(offset)
}

/// Floor on the scheduler's sleep so a collapsed interval (0, e.g. after a
/// failed startup announce) never busy-spins the loop / hammers the tracker.
const MIN_SLEEP_SECS: u64 = 5;

pub async fn run(wait_time: u64) {
    info!("Starting scheduler");
    loop {
        // Snapshot the torrent handles under a SHORT read lock, then drop it.
        // We must NOT hold TORRENTS.read() across the per-torrent announces below
        // (each does network I/O up to ~60s): the tokio RwLock is write-preferring,
        // so a queued add/remove writer would otherwise freeze the whole announce
        // loop AND the UI for an entire sweep. Cloning the Arc<Mutex<Torrent>>
        // handles lets us announce holding only each torrent's own mutex.
        let handles: Vec<std::sync::Arc<tokio::sync::Mutex<Torrent>>> = {
            let list = TORRENTS.read().await;
            list.iter().cloned().collect()
        };
        let next_interval = {
            // Compute minimum time until next announce across all torrents.
            // Use Option<u64> so None ("no torrents / nothing pending") is
            // distinct from Some(0) ("a torrent is overdue right now").
            let mut min_interval: Option<u64> = None;
            for m in handles.iter() {
                let mut t = m.lock().await;
                let elapsed = t.last_announce.elapsed().as_secs();
                trace!(
                    torrent = %t.name,
                    elapsed = elapsed,
                    interval = t.interval,
                    seeding = t.is_seeding(),
                    downloaded = t.declared_downloaded(),
                    uploaded = t.uploaded,
                    seeders = t.seeders,
                    leechers = t.leechers,
                    "scheduler tick"
                );

                if !t.is_seeding() {
                    // DOWNLOADING: tick on the short cadence, advance progress, and
                    // re-announce with the partial downloaded/left (or `completed`
                    // on the call that finishes the download).
                    // Compute dl_interval once so both the tick-gate and the
                    // sleep-target use the same jitter-sampled value.
                    let di = dl_interval(&t);
                    if elapsed >= di {
                        let completed_now = t.advance_download();
                        // `completed` fires on the finishing call, or is retried
                        // (is_seeding && !completed_sent) if a prior one failed.
                        let ev = if completed_now || (t.is_seeding() && !t.completed_sent) {
                            Some(Event::Completed)
                        } else {
                            None
                        };
                        debug!(torrent = %t.name, ?ev, "⤓ download tick");
                        super::tracker::announce(&mut t, ev).await;
                    }
                    let nxt = di.saturating_sub(t.last_announce.elapsed().as_secs());
                    let v = nxt.max(1);
                    min_interval = Some(min_interval.map_or(v, |m| m.min(v)));
                } else if !t.completed_sent {
                    // SEEDING but a `completed` still needs to land (it failed
                    // earlier): retry it now, then resume normal cadence.
                    debug!(torrent = %t.name, "↻ retrying completed");
                    super::tracker::announce(&mut t, Some(Event::Completed)).await;
                    super::tracker::apply_recheck(&mut t);
                    let v = t.interval.max(1);
                    min_interval = Some(min_interval.map_or(v, |m| m.min(v)));
                } else {
                    // SEEDING, completed delivered: the original behaviour.
                    if t.should_announce() {
                        debug!(torrent = %t.name, "⏰ time to announce");
                        super::tracker::announce(&mut t, None).await;
                        super::tracker::apply_recheck(&mut t);
                    }
                    let e = t.last_announce.elapsed().as_secs();
                    // Use .max(1) so an overdue/failed torrent (saturating_sub→0)
                    // contributes 1 rather than the ambiguous 0 sentinel.
                    let v = t.interval.saturating_sub(e).max(1);
                    min_interval = Some(min_interval.map_or(v, |m| m.min(v)));
                }
            }
            // None = no torrents at all → fall back to wait_time.
            match min_interval {
                None => wait_time,
                Some(v) => add_jitter(v),
            }
        };
        // Floor the sleep so a collapsed/zero interval can't busy-spin the loop
        // and flood a dead tracker (the interval clamp in tracker.rs prevents the
        // usual sources, this is the final backstop).
        let next_interval = next_interval.max(MIN_SLEEP_SECS);
        debug!("Next announce in {}s", next_interval);
        crate::json_output::write().await;
        // Persist download phase each tick so a crash/restart resumes correctly.
        let _ = crate::state::save().await;
        tokio::time::sleep(Duration::from_secs(next_interval)).await;
    }
}

// /// Build the announce query and perform it in another thread
// fn announce(event: Option<Event>) {
//     debug!("Announcing");
//     if let Some(client) = &*CLIENT.read().expect("Cannot read client") {
//         let config = CONFIG.get().expect("Cannot read configuration");
//         let list = &mut *TORRENTS.write().expect("Cannot get torrent list");
//         let mut available_download_speed: u32 = config.max_download_rate;
//         let mut available_upload_speed: u32 = config.max_upload_rate;
//         let mut next_announce = 4_294_967_295u32;
//         // send queries to trackers
//         for t in list {
//             // TODO: client.annouce(t, client);
//             let mut interval: u64 = 4_294_967_295;
//             if !t.should_announce() {
//                 next_announce = next_announce.min(t.interval.try_into().unwrap());
//                 continue;
//             }
//             // let url = &t.build_urls(event.clone(), client.key.clone())[0];
//             // let query = client.get_query();
//             // let agent = ureq::AgentBuilder::new()
//             //     .timeout(std::time::Duration::from_secs(60))
//             //     .user_agent(&client.user_agent);
//             // let mut req = agent
//             //     .build()
//             //     .get(url)
//             //     .timeout(std::time::Duration::from_secs(90));
//             // req = query
//             //     .1
//             //     .into_iter()
//             //     .fold(req, |req, header| req.set(&header.0, &header.1));
//             interval = interval.min(tracker::announce(t, event));
//             // interval = t.announce(event, req);
//             //compute the download and upload speed
//             available_upload_speed -= t.uploaded(config.min_upload_rate, available_upload_speed);
//             available_download_speed -=
//                 t.uploaded(config.min_upload_rate, available_download_speed);
//             t.uploaded += (interval as usize) * (t.next_upload_speed as usize);
//             // if t.length < t.downloaded + (t.next_download_speed as usize * interval as usize) {
//             //     //compute next interval to for an EVENT_COMPLETED
//             //     let t: u64 =
//             //         (t.length - t.downloaded).div_euclid(t.next_download_speed as usize) as u64;
//             //     ctx.run_later(Duration::from_secs(t + 5), move |this, ctx| {
//             //         this.announce(ctx, Some(Event::Completed));
//             //     });
//             // } else {
//             //     ctx.run_later(Duration::from_secs(interval), move |this, ctx| {
//             //         this.announce(ctx, None);
//             //     });
//             // }
//         }
//         // TODO: schedule next announce
//     }
// }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_jitter_short_interval() {
        // Short intervals should not have jitter
        assert_eq!(add_jitter(10), 10);
        assert_eq!(add_jitter(19), 19);
        assert_eq!(add_jitter(0), 0);
    }

    #[test]
    fn test_add_jitter_bounds() {
        // Test that jitter stays within ±5% bounds
        let interval = 1000u64;
        let min_expected = 950; // -5%
        let max_expected = 1050; // +5%

        for _ in 0..100 {
            let result = add_jitter(interval);
            assert!(
                result >= min_expected && result <= max_expected,
                "Jitter {} out of bounds [{}, {}]",
                result,
                min_expected,
                max_expected
            );
        }
    }

    #[test]
    fn test_add_jitter_typical_tracker_interval() {
        // Typical tracker interval of 1800s (30 minutes)
        let interval = 1800u64;
        let min_expected = 1710; // -5%
        let max_expected = 1890; // +5%

        for _ in 0..100 {
            let result = add_jitter(interval);
            assert!(
                result >= min_expected && result <= max_expected,
                "Jitter {} out of bounds [{}, {}]",
                result,
                min_expected,
                max_expected
            );
        }
    }
}
