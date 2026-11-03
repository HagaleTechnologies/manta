//! MAN-198: the write-only track trace the loss funnel reads. A
//! `TrackManager` with a trace set records every candidate birth,
//! CANDIDATE -> ACTIVE confirmation and close here; with none set it
//! records nothing and decodes exactly as before.

use std::sync::{Arc, Mutex};

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
            C::Evicted => Self::Evicted,
            C::Rtty => Self::Rtty,
            C::NotCw => Self::NotCw,
            C::Unkeyed => Self::Unkeyed,
        }
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

    /// Takes everything recorded so far, leaving the trace empty.
    pub fn take(&self) -> TraceLog {
        std::mem::take(&mut *self.log.lock().unwrap_or_else(|e| e.into_inner()))
    }
}
