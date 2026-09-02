//! One broadcast channel of validated spots, fanned out to the telnet and
//! JSON/WebSocket servers. ARCHITECTURE §7: "Both servers are thin fan-out
//! consumers of one broadcast channel; slow clients are disconnected,
//! never back-pressure the pipeline." A `tokio::sync::broadcast` channel
//! gives us that for free: `publish` never blocks the (synchronous)
//! decode pipeline, and a subscriber that falls too far behind gets
//! `RecvError::Lagged` instead of stalling the sender -- callers should
//! treat `Lagged` as "disconnect this client," not "catch up."

use manta_spot::Spot;
use std::time::{Duration, SystemTime};
use tokio::sync::broadcast;

/// Bounds how many unconsumed spots a subscriber may lag behind before
/// it starts missing spots (and its next `recv()` returns `Lagged`).
const CHANNEL_CAPACITY: usize = 1024;

pub struct SpotBus {
    tx: broadcast::Sender<Spot>,
    epoch: SystemTime,
    sample_rate_hz: f64,
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
        }
    }

    /// Publishes one validated spot to every current subscriber. No-op
    /// (not an error) when there are zero subscribers -- a spot server
    /// with no connected clients is the common case, not a failure.
    pub fn publish(&self, spot: Spot) {
        let _ = self.tx.send(spot);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Spot> {
        self.tx.subscribe()
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

    #[test]
    fn unix_ts_for_converts_sample_count_using_sample_rate_and_epoch() {
        let epoch = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let bus = SpotBus::new(1000.0, epoch); // 1000 Hz -> 1 sample = 1 ms
                                               // 5000 samples at 1000 Hz = 5 seconds past epoch.
        assert_eq!(bus.unix_ts_for(5000), 1_700_000_005);
    }
}
