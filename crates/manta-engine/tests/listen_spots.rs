//! `listen()`'s new `on_spot` callback: a real `manta-spot::Validator`
//! run over the streamed event sequence. Uses a raw-complex-IQ in-memory
//! source (not `AudioIqSource`) for isolation and speed -- this test
//! doesn't need real audio hardware semantics to exercise `on_spot`. The
//! near-DC Hilbert leakage this choice originally sidestepped (issue #21)
//! is fixed by MAN-273 (see `listen_audio.rs`'s doc comment).

use manta_engine::{listen, PipelineConfig, Spot};
use manta_testkit::scene::{render_scene, SignalSpec};
use num_complex::Complex32;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

struct FixedFreqSource {
    samples: Vec<Complex32>,
    cursor: usize,
    fs: f64,
    center_freq_hz: f64,
}

impl manta_input::IqSource for FixedFreqSource {
    fn sample_rate(&self) -> f64 {
        self.fs
    }
    fn center_freq_hz(&self) -> f64 {
        self.center_freq_hz
    }
    fn read(&mut self, buf: &mut [Complex32]) -> anyhow::Result<usize> {
        let n = buf.len().min(self.samples.len() - self.cursor);
        buf[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
        self.cursor += n;
        Ok(n)
    }
}

#[test]
fn listen_emits_a_spot_via_on_spot() {
    let sig = SignalSpec {
        dit_units: 1.0,
        element_gap_units: 1.0,
        text: "CQ CQ DE K5ARH K5ARH K".into(),
        loop_text: true,
        wpm: 20.0,
        offset_hz: 12_340.0,
        snr_2500_db: 20.0,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
        weight: 3.0,
        char_gap_units: 3.0,
        word_gap_units: 7.0,
        rise_ms: 5.0,
    };
    let (samples, _texts) =
        render_scene(std::slice::from_ref(&sig), 96_000.0, 30.0, Some(1)).unwrap();
    let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
        samples,
        cursor: 0,
        fs: 96_000.0,
        center_freq_hz: 14_000_000.0,
    });

    let stop = Arc::new(AtomicBool::new(false));
    let mut spots: Vec<Spot> = Vec::new();
    listen(
        src,
        &PipelineConfig::default(),
        stop,
        |_ev| {},
        |spot| spots.push(spot.clone()),
    )
    .unwrap();

    assert!(
        spots.iter().any(|s| s.callsign == "K5ARH"),
        "expected a K5ARH spot from on_spot, got: {spots:?}"
    );
}

// ---- MAN-318: the callsign lookup step through `listen_with_observers` ----

use manta_engine::{listen_with_observers, ListenObservers, LiveRecord, LiveStateSink};
use manta_spot::{CallsignLookup, Existence, LookupStatus, Reason, Verdict};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// Answers every `check` with `status`; `settled` reports `Exists` for every
/// pending call on its `exists_on`-th call after the first `check` (never
/// when `None`), and `settle_all` reports `Unknown` for any call still
/// pending. `batches` counts `on_tracks` callbacks (`listen_k5arh`).
struct FakeLookup {
    status: LookupStatus,
    exists_on: Option<usize>,
    checks: AtomicUsize,
    settled_calls: AtomicUsize,
    /// `settled_calls` and `batches` when `check` was first called.
    first_check: Mutex<Option<(usize, usize)>>,
    batches: AtomicUsize,
    pending: Mutex<Vec<String>>,
}

impl FakeLookup {
    fn new(status: LookupStatus, exists_on: Option<usize>) -> Arc<Self> {
        Arc::new(Self {
            status,
            exists_on,
            checks: AtomicUsize::new(0),
            settled_calls: AtomicUsize::new(0),
            first_check: Mutex::new(None),
            batches: AtomicUsize::new(0),
            pending: Mutex::new(Vec::new()),
        })
    }
}

impl CallsignLookup for FakeLookup {
    fn check(&self, call: &str) -> LookupStatus {
        self.checks.fetch_add(1, Ordering::SeqCst);
        self.first_check.lock().unwrap().get_or_insert((
            self.settled_calls.load(Ordering::SeqCst),
            self.batches.load(Ordering::SeqCst),
        ));
        if self.status == LookupStatus::Pending {
            let mut pending = self.pending.lock().unwrap();
            if !pending.iter().any(|c| c == call) {
                pending.push(call.to_string());
            }
        }
        self.status
    }
    fn settled(&self) -> Vec<(String, Existence)> {
        let n = self.settled_calls.fetch_add(1, Ordering::SeqCst) + 1;
        let since = match *self.first_check.lock().unwrap() {
            Some((at, _)) => n - at,
            None => return Vec::new(),
        };
        let mut pending = self.pending.lock().unwrap();
        if pending.is_empty() || self.exists_on.is_none_or(|on| since < on) {
            return Vec::new();
        }
        std::mem::take(&mut *pending)
            .into_iter()
            .map(|c| (c, Existence::Exists))
            .collect()
    }
    fn settle_all(&self) -> Vec<(String, Existence)> {
        std::mem::take(&mut *self.pending.lock().unwrap())
            .into_iter()
            .map(|c| (c, Existence::Unknown))
            .collect()
    }
}

fn k5arh_source() -> Box<dyn manta_input::IqSource> {
    let sig = SignalSpec {
        dit_units: 1.0,
        element_gap_units: 1.0,
        text: "CQ CQ DE K5ARH K5ARH K".into(),
        loop_text: true,
        wpm: 20.0,
        offset_hz: 12_340.0,
        snr_2500_db: 20.0,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
        weight: 3.0,
        char_gap_units: 3.0,
        word_gap_units: 7.0,
        rise_ms: 5.0,
    };
    let (samples, _texts) =
        render_scene(std::slice::from_ref(&sig), 96_000.0, 30.0, Some(1)).unwrap();
    Box::new(FixedFreqSource {
        samples,
        cursor: 0,
        fs: 96_000.0,
        center_freq_hz: 14_000_000.0,
    })
}

/// What `on_spot` saw, with the `on_tracks` callback count at each spot
/// (also counted into `lookup.batches`, when given).
fn listen_k5arh(observers: ListenObservers, lookup: Option<&FakeLookup>) -> Vec<(Spot, usize)> {
    let mut spots = Vec::new();
    let tracks = std::cell::Cell::new(0usize);
    listen_with_observers(
        k5arh_source(),
        &PipelineConfig::default(),
        Arc::new(AtomicBool::new(false)),
        observers,
        |_ev| {},
        |spot| spots.push((spot.clone(), tracks.get())),
        |_n| {
            tracks.set(tracks.get() + 1);
            if let Some(l) = lookup {
                l.batches.fetch_add(1, Ordering::SeqCst);
            }
        },
    )
    .unwrap();
    spots
}

fn with_lookup(lookup: &Arc<FakeLookup>) -> ListenObservers {
    ListenObservers {
        callsign_lookup: Some(lookup.clone()),
        ..Default::default()
    }
}

#[test]
fn without_a_lookup_spots_are_unchanged() {
    let plain: Vec<Spot> = listen_k5arh(ListenObservers::default(), None)
        .into_iter()
        .map(|(s, _)| s)
        .collect();
    assert!(!plain.is_empty());
    let exists = FakeLookup::new(LookupStatus::Known(Existence::Exists), None);
    let looked_up: Vec<Spot> = listen_k5arh(with_lookup(&exists), None)
        .into_iter()
        .map(|(s, _)| s)
        .collect();
    assert_eq!(looked_up, plain);
    assert!(exists.checks.load(Ordering::SeqCst) > 0);
}

#[test]
fn a_not_found_call_never_reaches_on_spot() {
    let lookup = FakeLookup::new(LookupStatus::Known(Existence::NotFound), None);
    let spots = listen_k5arh(with_lookup(&lookup), None);
    assert!(spots.is_empty(), "{spots:?}");
    assert!(lookup.checks.load(Ordering::SeqCst) > 0);
}

#[test]
fn a_held_spot_reaches_on_spot_on_a_later_batch() {
    let lookup = FakeLookup::new(LookupStatus::Pending, Some(5));
    let spots = listen_k5arh(with_lookup(&lookup), Some(&lookup));
    let k5arh: Vec<_> = spots
        .iter()
        .filter(|(s, _)| s.callsign == "K5ARH")
        .collect();
    assert_eq!(k5arh.len(), 1, "{spots:?}");
    // `settled` is asked once per batch and `on_tracks` fires once per
    // batch, so the fifth `settled` call after the hold is at least four
    // batches on.
    let (_, batches_at_hold) = lookup.first_check.lock().unwrap().expect("a check");
    assert!(
        k5arh[0].1 >= batches_at_hold + 4,
        "held at batch {batches_at_hold}, released at {}",
        k5arh[0].1
    );
}

#[test]
fn a_spot_still_held_at_end_of_stream_is_settled_before_listen_returns() {
    let lookup = FakeLookup::new(LookupStatus::Pending, None);
    let spots = listen_k5arh(with_lookup(&lookup), None);
    assert_eq!(
        spots.iter().filter(|(s, _)| s.callsign == "K5ARH").count(),
        1,
        "{spots:?}"
    );
}

struct Recorder(Mutex<Vec<LiveRecord>>);

impl LiveStateSink for Recorder {
    fn wants_records(&self) -> bool {
        true
    }
    fn publish(&self, record: LiveRecord) {
        self.0.lock().unwrap().push(record);
    }
}

#[test]
fn the_live_stream_sees_lookup_pending_then_spotted() {
    let recorder = Arc::new(Recorder(Mutex::new(Vec::new())));
    let lookup = FakeLookup::new(LookupStatus::Pending, Some(3));
    let spots = listen_k5arh(
        ListenObservers {
            callsign_lookup: Some(lookup),
            live_state: Some(recorder.clone()),
            ..Default::default()
        },
        None,
    );
    assert_eq!(
        spots.iter().filter(|(s, _)| s.callsign == "K5ARH").count(),
        1,
        "{spots:?}"
    );
    let records = recorder.0.lock().unwrap();
    let k5arh: Vec<(Verdict, Option<Reason>)> = records
        .iter()
        .filter_map(|r| match r {
            LiveRecord::Decision(d)
                if d.call == "K5ARH" && d.reason != Some(Reason::Repetition) =>
            {
                Some((d.verdict, d.reason))
            }
            _ => None,
        })
        .collect();
    let pending = k5arh
        .iter()
        .position(|d| *d == (Verdict::Deferred, Some(Reason::LookupPending)))
        .expect("a lookupPending decision");
    let spotted = k5arh
        .iter()
        .position(|d| *d == (Verdict::Spotted, None))
        .expect("a spotted decision");
    assert!(pending < spotted, "{k5arh:?}");
}
