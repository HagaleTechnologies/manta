//! MAN-198: the write-only track trace the loss funnel reads. A
//! `TrackManager` with a trace set records every candidate birth,
//! CANDIDATE -> ACTIVE confirmation and close here, plus a census of the
//! rises it refused to spawn; with none set it records nothing and decodes
//! exactly as before.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// Why a rising channel started no candidate track: the spawn condition it
/// failed. Source masks are tested first (the input, not the detector,
/// refused the rise), then the tracker's own conditions in its scan order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SpawnVeto {
    TrustedBand,
    AnalyticGuard,
    DigitalWindow,
    MirrorImage,
    Warmup,
    StaticCrash,
    OwnedByTrack,
    /// MAN-112: the rise's two-hop phase position sat more than
    /// `CENTROID_OUTLIER_CH` from the channel next to a live track: a
    /// carrier's skirt, not a second carrier.
    SidelobePosition,
    CoolingDown,
    NotLocalPeak,
    Rtty,
    CoModulated,
}

impl SpawnVeto {
    /// The camelCase label the funnel's `detail` carries.
    pub fn label(self) -> &'static str {
        match self {
            SpawnVeto::TrustedBand => "trustedBand",
            SpawnVeto::AnalyticGuard => "analyticGuard",
            SpawnVeto::DigitalWindow => "digitalWindow",
            SpawnVeto::MirrorImage => "mirrorImage",
            SpawnVeto::Warmup => "warmup",
            SpawnVeto::StaticCrash => "staticCrash",
            SpawnVeto::OwnedByTrack => "ownedByTrack",
            SpawnVeto::SidelobePosition => "sidelobePosition",
            SpawnVeto::CoolingDown => "coolingDown",
            SpawnVeto::NotLocalPeak => "notLocalPeak",
            SpawnVeto::Rtty => "rtty",
            SpawnVeto::CoModulated => "coModulated",
        }
    }

    /// A source mask: the input's trusted band, analytic guard, digital
    /// window or mirror image, which no detector change can lift.
    pub fn is_source_mask(self) -> bool {
        matches!(
            self,
            SpawnVeto::TrustedBand
                | SpawnVeto::AnalyticGuard
                | SpawnVeto::DigitalWindow
                | SpawnVeto::MirrorImage
        )
    }
}

/// A spawn-veto census key: the stream second, the uncalibrated channel
/// centre in hundredths of a hertz, and the veto.
pub type VetoKey = (u64, i64, SpawnVeto);

/// Why a traced track closed: a public mirror of the tracker's own close
/// reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TrackCloseReason {
    Unconfirmed,
    HangExpired,
    Silent,
    Merged,
    Evicted,
    Rtty,
    NotCw,
    Unkeyed,
}

impl TrackCloseReason {
    /// The camelCase label the funnel report uses.
    pub fn label(self) -> &'static str {
        match self {
            Self::Unconfirmed => "unconfirmed",
            Self::HangExpired => "hangExpired",
            Self::Silent => "silent",
            Self::Merged => "merged",
            Self::Evicted => "evicted",
            Self::Rtty => "rtty",
            Self::NotCw => "notCw",
            Self::Unkeyed => "unkeyed",
        }
    }
}

impl From<crate::track::CloseReason> for TrackCloseReason {
    fn from(r: crate::track::CloseReason) -> Self {
        use crate::track::CloseReason as C;
        match r {
            C::Unconfirmed => Self::Unconfirmed,
            C::HangExpired => Self::HangExpired,
            C::Silent => Self::Silent,
            C::Merged => Self::Merged,
            // A coverage restriction is a policy eviction, not an observed
            // end of signal. Keep the existing public trace categories.
            C::Evicted | C::Restricted => Self::Evicted,
            C::Rtty => Self::Rtty,
            C::NotCw => Self::NotCw,
            C::Unkeyed => Self::Unkeyed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TrackCloseReason;

    #[test]
    fn restricted_close_is_traced_as_a_policy_eviction() {
        assert_eq!(
            TrackCloseReason::from(crate::track::CloseReason::Restricted),
            TrackCloseReason::Evicted
        );
    }
}

/// One traced track lifecycle fact. Frequencies are uncalibrated channel
/// centres, as the tracker sees them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TrackTraceRecord {
    /// A candidate track was born on its birth channel.
    Spawned {
        track_id: u32,
        freq_hz: f64,
        sample_ts: u64,
    },
    /// The detector confirmed the candidate (CANDIDATE -> ACTIVE).
    Confirmed { track_id: u32, sample_ts: u64 },
    /// The track closed. `sample_ts` is the hop the tracker was on.
    Closed {
        track_id: u32,
        sample_ts: u64,
        reason: TrackCloseReason,
    },
}

/// Everything one traced run recorded, in record order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TraceLog {
    pub records: Vec<TrackTraceRecord>,
    /// Refused rises, counted per hop.
    pub vetoes: BTreeMap<VetoKey, u32>,
}

/// The shared trace handle `ListenObservers::funnel_trace` carries.
#[derive(Debug, Default)]
pub struct FunnelTrace {
    log: Mutex<TraceLog>,
}

impl FunnelTrace {
    /// An empty trace, ready to share with a run.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn push(&self, record: TrackTraceRecord) {
        self.log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .records
            .push(record);
    }

    /// Counts one hop's refused rises.
    pub(crate) fn vetoes(&self, refused: impl IntoIterator<Item = VetoKey>) {
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        for key in refused {
            *log.vetoes.entry(key).or_insert(0) += 1;
        }
    }

    /// Takes everything recorded so far, leaving the trace empty.
    pub fn take(&self) -> TraceLog {
        std::mem::take(&mut *self.log.lock().unwrap_or_else(|e| e.into_inner()))
    }
}
