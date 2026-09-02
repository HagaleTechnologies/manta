//! One broadcast channel of validated spots, fanned out to the telnet and
//! JSON/WebSocket servers. ARCHITECTURE §7: "Both servers are thin fan-out
//! consumers of one broadcast channel; slow clients are disconnected,
//! never back-pressure the pipeline." A `tokio::sync::broadcast` channel
//! gives us that for free: `publish` never blocks the (synchronous)
//! decode pipeline, and a subscriber that falls too far behind gets
//! `RecvError::Lagged` instead of stalling the sender -- callers should
//! treat `Lagged` as "disconnect this client," not "catch up."

use manta_spot::Spot;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use tokio::sync::broadcast;

/// Bounds how many unconsumed spots a subscriber may lag behind before
/// it starts missing spots (and its next `recv()` returns `Lagged`).
const CHANNEL_CAPACITY: usize = 1024;

/// How many of the most recently published spots `recent()` can return --
/// backs the telnet server's `sh/dx` command (MAN-12 scope, per the
/// 2026-09-02 ticket clarification: real command grammar, not just
/// "don't disconnect").
const RECENT_HISTORY_CAP: usize = 50;

pub struct SpotBus {
    tx: broadcast::Sender<Spot>,
    epoch: SystemTime,
    sample_rate_hz: f64,
    recent: Mutex<VecDeque<Spot>>,
    occurrence_counts: Mutex<HashMap<String, u32>>,
}

impl SpotBus {
    /// `epoch` is the wall-clock instant corresponding to `sample_ts == 0`
    /// in the decode pipeline (session start); `sample_rate_hz` converts a
    /// spot's sample-count timestamp to elapsed seconds since `epoch`.
    pub fn new(sample_rate_hz: f64, epoch: SystemTime) -> Self {
        let (tx, _rx) = broadcast::channel(CHANNEL_CAPACITY);
        Self {
            tx,
            epoch,
            sample_rate_hz,
            recent: Mutex::new(VecDeque::with_capacity(RECENT_HISTORY_CAP)),
            occurrence_counts: Mutex::new(HashMap::new()),
        }
    }

    /// Publishes one validated spot to every current subscriber. No-op
    /// (not an error) when there are zero subscribers -- a spot server
    /// with no connected clients is the common case, not a failure.
    pub fn publish(&self, spot: Spot) {
        {
            let mut recent = self.recent.lock().expect("recent lock poisoned");
            if recent.len() == RECENT_HISTORY_CAP {
                recent.pop_front();
            }
            recent.push_back(spot.clone());
        }
        *self
            .occurrence_counts
            .lock()
            .expect("occurrence_counts lock poisoned")
            .entry(spot.callsign.clone())
            .or_insert(0) += 1;
        let _ = self.tx.send(spot);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Spot> {
        self.tx.subscribe()
    }

    /// The last `n` published spots, oldest first (`sh/dx` backing store).
    pub fn recent(&self, n: usize) -> Vec<Spot> {
        let recent = self.recent.lock().expect("recent lock poisoned");
        let skip = recent.len().saturating_sub(n);
        recent.iter().skip(skip).cloned().collect()
    }

    /// How many times a callsign has been published on this bus so far --
    /// backs the telnet server's `set dx filter unique > n` command.
    pub fn occurrence_count(&self, callsign: &str) -> u32 {
        self.occurrence_counts
            .lock()
            .expect("occurrence_counts lock poisoned")
            .get(callsign)
            .copied()
            .unwrap_or(0)
    }

    /// Converts a spot's `sample_ts` (samples since session start) to a
    /// Unix wall-clock timestamp, using this bus's `epoch`/`sample_rate_hz`.
    pub fn unix_ts_for(&self, sample_ts: u64) -> i64 {
        let elapsed = Duration::from_secs_f64(sample_ts as f64 / self.sample_rate_hz);
        (self.epoch + elapsed)
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("epoch predates the Unix epoch")
            .as_secs() as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use manta_spot::SpotType;

    fn sample_spot(sample_ts: u64) -> Spot {
        Spot {
            callsign: "K5ARH".to_string(),
            freq_hz: 14_027_100.0,
            snr_db: 20.0,
            wpm: 25.0,
            spot_type: SpotType::De,
            confidence: 0.9,
            track_id: 1,
            sample_ts,
        }
    }

    #[tokio::test]
    async fn a_subscriber_receives_a_published_spot() {
        let bus = SpotBus::new(96_000.0, SystemTime::now());
        let mut rx = bus.subscribe();

        bus.publish(sample_spot(0));

        let received = rx.recv().await.unwrap();
        assert_eq!(received.callsign, "K5ARH");
    }

    #[tokio::test]
    async fn publishing_with_no_subscribers_does_not_panic_or_block() {
        let bus = SpotBus::new(96_000.0, SystemTime::now());
        bus.publish(sample_spot(0));
    }

    #[tokio::test]
    async fn each_subscriber_gets_its_own_copy() {
        let bus = SpotBus::new(96_000.0, SystemTime::now());
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        bus.publish(sample_spot(0));

        assert_eq!(rx1.recv().await.unwrap().callsign, "K5ARH");
        assert_eq!(rx2.recv().await.unwrap().callsign, "K5ARH");
    }

    #[tokio::test]
    async fn a_lagging_subscriber_is_told_to_disconnect_instead_of_stalling_the_sender() {
        let bus = SpotBus::new(96_000.0, SystemTime::now());
        let mut rx = bus.subscribe();

        // Publish well past the channel capacity without the subscriber
        // ever draining -- this must not block `publish`.
        for i in 0..(CHANNEL_CAPACITY as u64 + 10) {
            bus.publish(sample_spot(i));
        }

        match rx.recv().await {
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            other => panic!("expected Lagged, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn recent_returns_the_last_n_published_spots_oldest_first() {
        let bus = SpotBus::new(96_000.0, SystemTime::now());
        for i in 0..5u64 {
            let mut spot = sample_spot(i);
            spot.track_id = i as u32;
            bus.publish(spot);
        }

        let recent = bus.recent(3);
        assert_eq!(recent.len(), 3);
        assert_eq!(
            recent.iter().map(|s| s.track_id).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
    }

    #[tokio::test]
    async fn recent_caps_at_the_available_history() {
        let bus = SpotBus::new(96_000.0, SystemTime::now());
        bus.publish(sample_spot(0));
        bus.publish(sample_spot(1));

        assert_eq!(bus.recent(10).len(), 2);
    }

    #[tokio::test]
    async fn occurrence_count_tracks_how_many_times_a_callsign_has_been_published() {
        let bus = SpotBus::new(96_000.0, SystemTime::now());
        assert_eq!(bus.occurrence_count("K5ARH"), 0);

        bus.publish(sample_spot(0));
        assert_eq!(bus.occurrence_count("K5ARH"), 1);

        bus.publish(sample_spot(1));
        assert_eq!(bus.occurrence_count("K5ARH"), 2);
        assert_eq!(bus.occurrence_count("JA1ABC"), 0);
    }

    #[test]
    fn unix_ts_for_converts_sample_count_using_sample_rate_and_epoch() {
        let epoch = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let bus = SpotBus::new(1000.0, epoch); // 1000 Hz -> 1 sample = 1 ms
                                               // 5000 samples at 1000 Hz = 5 seconds past epoch.
        assert_eq!(bus.unix_ts_for(5000), 1_700_000_005);
    }
}
