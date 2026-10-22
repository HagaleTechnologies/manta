//! One control owner and reconnect generation per configured HPSDR radio.
//! Network operations and callbacks never hold the coordinator's state lock.

use crate::config::HpsdrAllocation;
use crate::reconnect::{InputHealthTotals, SendSource};
use crate::ResolvedInput;
use anyhow::{anyhow, bail, Context, Result};
use manta_input::IqSource;
use manta_server::backoff::{next_backoff, AttemptOutcome, INITIAL_BACKOFF};
use num_complex::Complex32;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(100);
type Health = Box<dyn FnMut(bool) + Send>;
type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;
type Opener = Box<dyn FnMut() -> Result<Opened> + Send>;

struct Opened {
    sources: Vec<SendSource>,
    started_at: Instant,
    retire: Box<dyn Fn() + Send + Sync>,
}

#[derive(Clone, Debug, PartialEq)]
struct Metadata {
    rate: f64,
    center: f64,
    passband: (f64, f64),
    trusted: Option<(f64, f64)>,
}
impl Metadata {
    fn of(source: &dyn IqSource) -> Self {
        Self {
            rate: source.sample_rate(),
            center: source.center_freq_hz(),
            passband: source.rf_passband_hz(),
            trusted: source.trusted_band_hz(),
        }
    }
}

struct Generation {
    id: u64,
    slots: Mutex<Vec<Option<SendSource>>>,
    metadata: Vec<Metadata>,
    started_at: Instant,
    retired: AtomicBool,
    productive: AtomicBool,
    retire: Box<dyn Fn() + Send + Sync>,
}
impl Generation {
    fn retire(&self) {
        if !self.retired.swap(true, Ordering::SeqCst) {
            (self.retire)();
            // Any source not yet adopted must also lose its buffered data.
            self.slots
                .lock()
                .expect("HPSDR slots poisoned")
                .iter_mut()
                .for_each(|s| {
                    s.take();
                });
        }
    }
}
impl Drop for Generation {
    fn drop(&mut self) {
        self.retire();
    }
}

enum Phase {
    Idle,
    Opening,
    Active(Arc<Generation>),
    Retiring,
    Retrying(Instant),
    Failed(String, bool),
    Stopped,
}
struct State {
    phase: Phase,
    next_id: u64,
    backoff: Duration,
    expected: Option<Vec<Metadata>>,
}

struct Group {
    state: Mutex<State>,
    changed: Condvar,
    opener: Mutex<Opener>,
    stop: Arc<AtomicBool>,
    health: Mutex<Vec<Health>>,
    totals: Vec<OnceLock<Arc<InputHealthTotals>>>,
    attached: Vec<AtomicBool>,
    clock: Clock,
}

impl Group {
    fn new(
        opener: Opener,
        stop: Arc<AtomicBool>,
        mut health: Vec<Health>,
        clock: Clock,
    ) -> Arc<Self> {
        for sink in &mut health {
            sink(false);
        }
        Arc::new(Self {
            totals: (0..health.len()).map(|_| OnceLock::new()).collect(),
            attached: (0..health.len()).map(|_| AtomicBool::new(true)).collect(),
            state: Mutex::new(State {
                phase: Phase::Idle,
                next_id: 0,
                backoff: INITIAL_BACKOFF,
                expected: None,
            }),
            changed: Condvar::new(),
            opener: Mutex::new(opener),
            stop,
            health: Mutex::new(health),
            clock,
        })
    }

    fn now(&self) -> Instant {
        (self.clock)()
    }

    fn stop(&self) {
        let old = {
            let mut state = self.state.lock().expect("HPSDR group poisoned");
            std::mem::replace(&mut state.phase, Phase::Stopped)
        };
        if let Phase::Active(generation) = old {
            generation.retire();
            for total in &self.totals {
                if let Some(total) = total.get() {
                    total.retire_current();
                }
            }
        }
        self.changed.notify_all();
    }

    fn wait(self: &Arc<Self>, index: usize) -> Result<Option<Arc<Generation>>> {
        loop {
            if self.stop.load(Ordering::Relaxed) {
                self.stop();
                return Ok(None);
            }
            let mut state = self.state.lock().expect("HPSDR group poisoned");
            match &state.phase {
                Phase::Active(generation) => return Ok(Some(generation.clone())),
                Phase::Stopped => return Ok(None),
                // Initial errors are reported only by the first member. The
                // engine cancels the sibling waiters after receiving that error.
                Phase::Failed(error, initial) if index == 0 || !initial => bail!("{error}"),
                Phase::Idle if index == 0 => {}
                Phase::Retrying(at) if self.now() >= *at => {}
                _ => {
                    drop(
                        self.changed
                            .wait_timeout(state, POLL)
                            .expect("HPSDR group poisoned"),
                    );
                    continue;
                }
            }
            let id = state.next_id;
            state.next_id += 1;
            state.phase = Phase::Opening;
            drop(state);
            let group = self.clone();
            if let Err(error) =
                std::thread::Builder::new()
                    .name("hpsdr-open".into())
                    .spawn(move || {
                        let opened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            (group.opener.lock().expect("HPSDR opener poisoned"))()
                        }))
                        .unwrap_or_else(|_| Err(anyhow!("HPSDR radio opener panicked")));
                        group.install(id, opened);
                    })
            {
                self.install(id, Err(error.into()));
            }
        }
    }

    fn install(&self, id: u64, opened: Result<Opened>) {
        // Build metadata and counter handles outside the coordination lock.
        let result = opened.map(|opened| {
            let metadata = opened
                .sources
                .iter()
                .map(|s| Metadata::of(s.as_ref()))
                .collect();
            let counters: Vec<_> = opened.sources.iter().map(|s| s.health_counters()).collect();
            let generation = Arc::new(Generation {
                id,
                metadata,
                started_at: opened.started_at,
                slots: Mutex::new(opened.sources.into_iter().map(Some).collect()),
                retired: AtomicBool::new(false),
                productive: AtomicBool::new(false),
                retire: opened.retire,
            });
            (generation, counters)
        });
        let mut state = self.state.lock().expect("HPSDR group poisoned");
        if self.stop.load(Ordering::Relaxed)
            || !matches!(state.phase, Phase::Opening)
            || state.next_id != id + 1
        {
            drop(state);
            if let Ok((generation, _)) = result {
                generation.retire();
            }
            return;
        }
        let mut detached = Vec::new();
        match result {
            Ok((generation, counters)) => {
                if generation.metadata.len() != self.attached.len()
                    || state
                        .expected
                        .as_ref()
                        .is_some_and(|m| *m != generation.metadata)
                {
                    state.phase = Phase::Failed("HPSDR radio reopened with incompatible receiver rate, frequency or passband".into(), id == 0);
                    drop(state);
                    generation.retire();
                    self.changed.notify_all();
                    return;
                }
                for (index, counters) in counters.into_iter().enumerate() {
                    if let Some(total) = self.totals[index].get() {
                        total.attach(counters);
                    } else if let Some(counters) = counters {
                        let _ = self.totals[index].set(Arc::new(InputHealthTotals::new(counters)));
                    }
                    if !self.attached[index].load(Ordering::Relaxed) {
                        detached.push(
                            generation.slots.lock().expect("HPSDR slots poisoned")[index].take(),
                        );
                    }
                }
                state.expected = Some(generation.metadata.clone());
                state.phase = Phase::Active(generation);
            }
            Err(error) if id == 0 => state.phase = Phase::Failed(format!("{error:#}"), true),
            Err(error) => {
                state.backoff = next_backoff(state.backoff, &AttemptOutcome::NeverConnected);
                state.phase = Phase::Retrying(self.now() + state.backoff);
                crate::logging::warning(&format!("HPSDR radio reconnect failed: {error:#}"));
            }
        }
        drop(state);
        drop(detached);
        self.changed.notify_all();
    }

    fn is_current(&self, generation: &Generation) -> bool {
        matches!(&self.state.lock().expect("HPSDR group poisoned").phase,
            Phase::Active(current) if current.id == generation.id)
    }

    fn failed(&self, generation: &Arc<Generation>) {
        {
            let mut state = self.state.lock().expect("HPSDR group poisoned");
            if !matches!(&state.phase, Phase::Active(current) if current.id == generation.id) {
                return;
            }
            state.phase = Phase::Retiring;
        }
        // No replacement can open until old control authority and counters
        // have been retired. A stale read error cannot retire a new radio.
        generation.retire();
        for total in &self.totals {
            if let Some(total) = total.get() {
                total.retire_current();
            }
        }
        for sink in self
            .health
            .lock()
            .expect("HPSDR health poisoned")
            .iter_mut()
        {
            sink(false);
        }
        let mut state = self.state.lock().expect("HPSDR group poisoned");
        if matches!(state.phase, Phase::Retiring) {
            state.backoff = if generation.productive.load(Ordering::Relaxed) {
                INITIAL_BACKOFF
            } else {
                next_backoff(state.backoff, &AttemptOutcome::NeverConnected)
            };
            state.phase = Phase::Retrying(self.now() + state.backoff);
        }
        self.changed.notify_all();
    }

    /// Take this member's source from `generation`. A sibling's failure can
    /// retire the generation between `wait` and this claim; `None` then
    /// means wait for the replacement rather than end the input.
    fn claim(&self, generation: &Generation, index: usize) -> Result<Option<SendSource>> {
        let source = generation.slots.lock().expect("HPSDR slots poisoned")[index].take();
        match source {
            Some(source) => Ok(Some(source)),
            None if !self.is_current(generation) => Ok(None),
            None => bail!("HPSDR receiver already assigned"),
        }
    }

    fn productive(&self, generation: &Generation, index: usize) -> bool {
        let mut health = self.health.lock().expect("HPSDR health poisoned");
        if !self.is_current(generation) {
            return false;
        }
        generation.productive.store(true, Ordering::Relaxed);
        health[index](true);
        true
    }
}
impl Drop for Group {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(crate) struct Member {
    group: Arc<Group>,
    index: usize,
}
impl Member {
    pub(crate) fn open(self) -> Result<GroupSource> {
        loop {
            let generation = self
                .group
                .wait(self.index)?
                .ok_or_else(|| anyhow!("HPSDR startup cancelled"))?;
            let Some(source) = self.group.claim(&generation, self.index)? else {
                continue;
            };
            let metadata = generation.metadata[self.index].clone();
            return Ok(GroupSource {
                group: self.group,
                index: self.index,
                metadata,
                boundary: generation.started_at,
                samples: 0,
                current: Some((generation, source)),
                gap: None,
                resumed: None,
            });
        }
    }
}

pub(crate) struct GroupSource {
    group: Arc<Group>,
    index: usize,
    metadata: Metadata,
    current: Option<(Arc<Generation>, SendSource)>,
    boundary: Instant,
    samples: u64,
    gap: Option<u64>,
    resumed: Option<u64>,
}
impl GroupSource {
    pub(crate) fn input_health(&self) -> Option<Arc<InputHealthTotals>> {
        self.group.totals[self.index].get().cloned()
    }
}
impl IqSource for GroupSource {
    fn sample_rate(&self) -> f64 {
        self.metadata.rate
    }
    fn center_freq_hz(&self) -> f64 {
        self.metadata.center
    }
    fn rf_passband_hz(&self) -> (f64, f64) {
        self.metadata.passband
    }
    fn trusted_band_hz(&self) -> Option<(f64, f64)> {
        self.metadata.trusted
    }
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.group.stop.load(Ordering::Relaxed) {
                let outage = self.resumed.is_some()
                    || self
                        .current
                        .as_ref()
                        .is_none_or(|(g, _)| !self.group.is_current(g));
                self.group.stop();
                if outage {
                    self.gap = Some(
                        (self
                            .group
                            .now()
                            .saturating_duration_since(self.boundary)
                            .as_secs_f64()
                            * self.sample_rate())
                        .round() as u64,
                    );
                }
                return Ok(0);
            }
            if self
                .current
                .as_ref()
                .is_none_or(|(g, _)| !self.group.is_current(g))
            {
                self.current = None;
                let Some(generation) = self.group.wait(self.index)? else {
                    self.gap = Some(
                        (self
                            .group
                            .now()
                            .saturating_duration_since(self.boundary)
                            .as_secs_f64()
                            * self.metadata.rate)
                            .round() as u64,
                    );
                    return Ok(0);
                };
                let Some(source) = self.group.claim(&generation, self.index)? else {
                    continue;
                };
                self.resumed = Some(
                    (generation
                        .started_at
                        .saturating_duration_since(self.boundary)
                        .as_secs_f64()
                        * self.sample_rate())
                    .round() as u64,
                );
                self.samples = 0;
                self.current = Some((generation, source));
            }
            // A stop delivered while a replacement was adopted wins over
            // its first potentially blocking read.
            if self.group.stop.load(Ordering::Relaxed) {
                continue;
            }
            let (generation, source) = self.current.as_mut().expect("adopted generation");
            let result = source.read(buf);
            let inner_gap = source.take_discontinuity();
            if !self.group.is_current(generation) {
                continue;
            }
            match result {
                Ok(n) if n > 0 => {
                    if !self.group.productive(generation, self.index) {
                        continue;
                    }
                    if self.resumed.is_some() || inner_gap.is_some() {
                        self.gap = Some(
                            self.gap.unwrap_or(0)
                                + self.resumed.take().unwrap_or(0)
                                + inner_gap.unwrap_or(0),
                        );
                    }
                    self.samples += n as u64 + inner_gap.unwrap_or(0);
                    self.boundary = generation.started_at
                        + Duration::from_secs_f64(self.samples as f64 / self.metadata.rate);
                    return Ok(n);
                }
                Ok(_) | Err(_) => self.group.failed(generation),
            }
        }
    }
    fn take_discontinuity(&mut self) -> Option<u64> {
        self.gap.take()
    }
}
impl Drop for GroupSource {
    fn drop(&mut self) {
        // Serialize detachment with installation: this member may still own
        // an old source while the replacement slot is waiting to be adopted.
        let (unclaimed, last) = {
            let state = self.group.state.lock().expect("HPSDR group poisoned");
            self.group.attached[self.index].store(false, Ordering::Relaxed);
            let unclaimed = match &state.phase {
                Phase::Active(generation) => {
                    generation.slots.lock().expect("HPSDR slots poisoned")[self.index].take()
                }
                _ => None,
            };
            let last = self
                .group
                .attached
                .iter()
                .all(|a| !a.load(Ordering::Relaxed));
            (unclaimed, last)
        };
        // Source destructors can take driver locks; release the coordinator first.
        drop(unclaimed);
        self.current.take();
        if last {
            self.group.stop();
        }
    }
}

/// Resolve every HPSDR group concurrently, then check aliases before any
/// controller starts. Non-HPSDR inputs still open on their own workers.
struct Endpoints {
    groups: Vec<HpsdrAllocation>,
    names: Vec<String>,
    started: OnceLock<()>,
    results: Mutex<Vec<Option<std::result::Result<SocketAddr, String>>>>,
    changed: Condvar,
}
impl Endpoints {
    fn wait(self: &Arc<Self>, index: usize, stop: &AtomicBool) -> Result<SocketAddr> {
        self.started.get_or_init(|| {
            for (i, group) in self.groups.iter().enumerate() {
                let endpoints = self.clone();
                let (host, port) = (group.host.clone(), group.port);
                let spawn = std::thread::Builder::new()
                    .name("hpsdr-resolve".into())
                    .spawn(move || {
                        let result = (host.as_str(), port)
                            .to_socket_addrs()
                            // Name the endpoint: any group's worker may report it.
                            .map_err(|e| format!("{host}:{port}: {e}"))
                            .and_then(|mut addresses| {
                                addresses
                                    .find(|a| a.is_ipv4())
                                    .ok_or_else(|| format!("no IPv4 address for {host}:{port}"))
                            });
                        endpoints.results.lock().expect("HPSDR endpoints poisoned")[i] =
                            Some(result);
                        endpoints.changed.notify_all();
                    });
                if let Err(e) = spawn {
                    self.results.lock().expect("HPSDR endpoints poisoned")[i] =
                        Some(Err(e.to_string()));
                }
            }
        });
        let mut results = self.results.lock().expect("HPSDR endpoints poisoned");
        loop {
            if stop.load(Ordering::Relaxed) {
                bail!("HPSDR startup cancelled");
            }
            if let Some(error) = results
                .iter()
                .find_map(|r| r.as_ref().and_then(|r| r.as_ref().err()))
            {
                bail!("resolve HPSDR endpoint: {error}");
            }
            if results.iter().all(Option::is_some) {
                break;
            }
            results = self
                .changed
                .wait_timeout(results, POLL)
                .expect("HPSDR endpoints poisoned")
                .0;
        }
        let addresses: Vec<_> = results
            .iter()
            .map(|r| {
                *r.as_ref()
                    .expect("resolved")
                    .as_ref()
                    .expect("checked errors")
            })
            .collect();
        for i in 0..addresses.len() {
            for j in 0..i {
                if addresses[i] == addresses[j] {
                    bail!("HPSDR inputs \"{}\" and \"{}\" resolve to the same radio; use the same host and port for all its bands", self.names[j], self.names[i]);
                }
            }
        }
        Ok(addresses[index])
    }
}

pub(crate) fn prepare(
    allocations: Vec<HpsdrAllocation>,
    inputs: &[ResolvedInput],
    stop: Arc<AtomicBool>,
    mut health: impl FnMut(usize) -> Health,
) -> Vec<Option<Member>> {
    let mut members: Vec<Option<Member>> = (0..inputs.len()).map(|_| None).collect();
    let endpoints = Arc::new(Endpoints {
        names: allocations
            .iter()
            .map(|g| inputs[g.members[0]].name.clone())
            .collect(),
        results: Mutex::new((0..allocations.len()).map(|_| None).collect()),
        groups: allocations.clone(),
        started: OnceLock::new(),
        changed: Condvar::new(),
    });
    for (group_index, allocation) in allocations.into_iter().enumerate() {
        let indices = allocation.members.clone();
        let transforms: Vec<_> = indices
            .iter()
            .map(|&i| (inputs[i].capture_rate_hz, inputs[i].dial_freq_hz))
            .collect();
        let first_name = inputs[indices[0]].name.clone();
        let endpoints = endpoints.clone();
        let worker_stop = stop.clone();
        let mut accepted = None;
        let mut reopen = false;
        let opener = Box::new(move || {
            let peer = match accepted {
                Some(peer) => peer,
                None => {
                    let peer = endpoints.wait(group_index, &worker_stop)?;
                    if allocation.members.len() > 1 {
                        let count = manta_input::hpsdr::probe_receiver_capacity(
                            &peer.ip().to_string(),
                            peer.port(),
                        )?;
                        let endpoint = format!("{}:{}", allocation.host, allocation.port);
                        if let Some(count) = count {
                            if allocation.members.len() > count {
                                bail!("HPSDR radio at {endpoint} has {count} receivers; {} bands are configured", allocation.members.len());
                            }
                        } else if allocation.receivers.is_none() {
                            bail!("cannot determine receiver capacity for HPSDR radio at {endpoint}; set receivers on input \"{first_name}\" to the count supported by its gateware");
                        }
                    }
                    accepted = Some(peer);
                    peer
                }
            };
            if worker_stop.load(Ordering::Relaxed) {
                bail!("HPSDR startup cancelled");
            }
            // The first open uses the probed address. A reopen resolves the
            // configured host again, as a standalone HPSDR input does, so a
            // radio that changed address comes back without a restart.
            let (host, port) = if reopen {
                (allocation.host.clone(), allocation.port)
            } else {
                (peer.ip().to_string(), peer.port())
            };
            reopen = true;
            // MAN-305: a lone input keeps the protocol it configured (and
            // `auto`'s discovery), exactly as the standalone path opens it.
            // Several receivers on one radio speak protocol 1 in manta; the
            // config loader refuses anything else for a shared radio.
            if allocation.members.len() == 1
                && allocation.protocol != manta_input::hpsdr::HpsdrProtocol::One
            {
                let entered = Instant::now();
                let opened = manta_input::hpsdr::open_receiver(
                    &manta_input::HpsdrConfig {
                        host: host.clone(),
                        port,
                        ddc_count: 1,
                        sample_rate_hz: allocation.rate,
                        center_freq_hz: allocation.frequencies.clone(),
                    },
                    allocation.protocol,
                )?;
                crate::log_hpsdr_open_line(&host, port, &opened);
                // MAN-259: the open call's runtime before the radio started
                // streaming is outage, not samples.
                let started_at = entered + opened.source.acquisition_start_offset();
                let (capture, dial) = transforms[0];
                let source = crate::transform_source(opened.source, capture, dial)
                    .context("prepare HPSDR receivers")?;
                return Ok(Opened {
                    sources: vec![source],
                    started_at,
                    // A protocol-2 radio stops on drop (run 0); a boxed
                    // protocol-1 receiver detaches its slot, as the
                    // standalone path's reopen always has.
                    retire: Box::new(|| {}),
                });
            }
            let raw = manta_input::HpsdrDevice::open(manta_input::HpsdrConfig {
                host,
                port,
                ddc_count: allocation.members.len(),
                sample_rate_hz: allocation.rate,
                center_freq_hz: allocation.frequencies.clone(),
            })?;
            let session = raw[0].session();
            let started_at = session.started_at();
            let sources = raw
                .into_iter()
                .zip(&transforms)
                .map(|(raw, &(capture, dial))| {
                    crate::transform_source(Box::new(raw), capture, dial)
                })
                .collect::<Result<Vec<_>>>();
            match sources {
                Ok(sources) => Ok(Opened {
                    sources,
                    started_at,
                    retire: Box::new(move || session.retire()),
                }),
                Err(e) => {
                    session.retire();
                    Err(e).context("prepare HPSDR receivers")
                }
            }
        });
        let group = Group::new(
            opener,
            stop.clone(),
            indices.iter().map(|&i| health(i)).collect(),
            Arc::new(Instant::now),
        );
        for (index, global) in indices.into_iter().enumerate() {
            members[global] = Some(Member {
                group: group.clone(),
                index,
            });
        }
    }
    members
}

#[cfg(test)]
mod tests {
    use super::*;
    use manta_input::InputHealthCounters;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{mpsc, Barrier};

    struct Samples {
        fail: Arc<AtomicBool>,
        counters: Arc<InputHealthCounters>,
        value: f32,
        gap: Option<u64>,
    }
    impl IqSource for Samples {
        fn sample_rate(&self) -> f64 {
            48000.0
        }
        fn center_freq_hz(&self) -> f64 {
            7e6
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            if self.fail.load(Ordering::Relaxed) {
                bail!("radio lost");
            }
            let n = buf.len().min(4);
            buf[..n].fill(Complex32::new(self.value, 0.0));
            Ok(n)
        }
        fn health_counters(&self) -> Option<Arc<InputHealthCounters>> {
            Some(self.counters.clone())
        }
        fn take_discontinuity(&mut self) -> Option<u64> {
            self.gap.take()
        }
    }

    struct Fixture {
        group: Arc<Group>,
        opens: Arc<AtomicUsize>,
        retires: Arc<AtomicUsize>,
        time: Arc<Mutex<Instant>>,
        health: Arc<Mutex<Vec<bool>>>,
        counters: Arc<Mutex<Vec<Vec<Arc<InputHealthCounters>>>>>,
    }
    fn fixture() -> Fixture {
        let time = Arc::new(Mutex::new(Instant::now()));
        let worker_time = time.clone();
        let clock = time.clone();
        let opens = Arc::new(AtomicUsize::new(0));
        let count = opens.clone();
        let retires = Arc::new(AtomicUsize::new(0));
        let retired = retires.clone();
        let counters = Arc::new(Mutex::new(Vec::new()));
        let all_counters = counters.clone();
        let health = Arc::new(Mutex::new(vec![true; 2]));
        let sinks: Vec<Health> = (0..2)
            .map(|i| {
                let health = health.clone();
                Box::new(move |up| health.lock().unwrap()[i] = up) as Health
            })
            .collect();
        let group = Group::new(
            Box::new(move || {
                let generation = count.fetch_add(1, Ordering::SeqCst);
                let cs: Vec<_> = (0..2)
                    .map(|_| Arc::new(InputHealthCounters::new()))
                    .collect();
                all_counters.lock().unwrap().push(cs.clone());
                let sources = cs
                    .into_iter()
                    .map(|counters| {
                        Box::new(Samples {
                            fail: Arc::new(AtomicBool::new(false)),
                            counters,
                            value: generation as f32,
                            gap: None,
                        }) as SendSource
                    })
                    .collect();
                let retired = retired.clone();
                Ok(Opened {
                    sources,
                    started_at: *worker_time.lock().unwrap(),
                    retire: Box::new(move || {
                        retired.fetch_add(1, Ordering::SeqCst);
                    }),
                })
            }),
            Arc::new(AtomicBool::new(false)),
            sinks,
            Arc::new(move || *clock.lock().unwrap()),
        );
        Fixture {
            group,
            opens,
            retires,
            time,
            health,
            counters,
        }
    }
    fn open_pair(group: &Arc<Group>) -> Vec<GroupSource> {
        let barrier = Arc::new(Barrier::new(2));
        let (tx, rx) = mpsc::channel();
        let workers: Vec<_> = (0..2)
            .map(|index| {
                let group = group.clone();
                let barrier = barrier.clone();
                let tx = tx.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let _ = tx.send((index, Member { group, index }.open()));
                })
            })
            .collect();
        let mut results = Vec::new();
        for _ in 0..2 {
            match rx.recv_timeout(Duration::from_secs(2)) {
                Ok(result) => results.push(result),
                Err(_) => {
                    group.stop.store(true, Ordering::Relaxed);
                    group.changed.notify_all();
                    break;
                }
            }
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(
            results.len(),
            2,
            "concurrent opens must finish before deadline"
        );
        results.sort_by_key(|(index, _)| *index);
        results
            .into_iter()
            .map(|(_, result)| result.unwrap())
            .collect()
    }
    fn current(group: &Group) -> Arc<Generation> {
        match &group.state.lock().unwrap().phase {
            Phase::Active(g) => g.clone(),
            _ => panic!("not active"),
        }
    }

    #[test]
    fn concurrent_consumers_share_initial_open_recovery_and_monotonic_totals() {
        let f = fixture();
        let mut sources = open_pair(&f.group);
        assert_eq!(f.opens.load(Ordering::Relaxed), 1);
        assert_eq!(*f.health.lock().unwrap(), [false, false]);
        for src in &mut sources {
            assert_eq!(src.read(&mut [Complex32::default(); 4]).unwrap(), 4);
        }
        assert_eq!(*f.health.lock().unwrap(), [true, true]);
        let old = current(&f.group);
        for c in &f.counters.lock().unwrap()[0] {
            c.record_dropped(3);
        }
        f.group.failed(&old);
        assert_eq!(*f.health.lock().unwrap(), [false, false]);
        assert_eq!(f.retires.load(Ordering::Relaxed), 1);
        *f.time.lock().unwrap() += Duration::from_secs(1);
        let workers: Vec<_> = sources
            .into_iter()
            .map(|mut src| {
                std::thread::spawn(move || {
                    let mut buf = [Complex32::default(); 4];
                    src.read(&mut buf).unwrap();
                    assert_eq!(buf[0].re, 1.0);
                    assert_eq!(src.take_discontinuity(), Some(47996));
                    assert_eq!(src.take_discontinuity(), None);
                    src
                })
            })
            .collect();
        sources = workers.into_iter().map(|w| w.join().unwrap()).collect();
        assert_eq!(f.opens.load(Ordering::Relaxed), 2);
        for c in &f.counters.lock().unwrap()[1] {
            c.record_dropped(2);
        }
        for src in &sources {
            assert_eq!(src.input_health().unwrap().snapshot().dropped_packets, 5);
        }
        f.group.failed(&old); // late error cannot retire the replacement
        assert_eq!(f.retires.load(Ordering::Relaxed), 1);
        assert!(f.group.is_current(&current(&f.group)));
        assert_eq!(*f.health.lock().unwrap(), [true, true]);
        drop(sources.remove(0));
        assert_eq!(sources[0].read(&mut [Complex32::default(); 4]).unwrap(), 4);
        assert_eq!(f.retires.load(Ordering::Relaxed), 1);
        drop(sources);
        assert_eq!(f.retires.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_claim_racing_a_sibling_failure_waits_for_the_replacement() {
        let f = fixture();
        let mut sources = open_pair(&f.group);
        let old = current(&f.group);
        // Member 1 adopts the generation and fails it before member 0 claims.
        f.group.failed(&old);
        assert!(f.group.claim(&old, 0).unwrap().is_none());
        *f.time.lock().unwrap() += Duration::from_secs(2);
        let replacement = f.group.wait(0).unwrap().unwrap();
        assert!(f.group.claim(&replacement, 0).unwrap().is_some());
        let err = f.group.claim(&replacement, 0).err().unwrap().to_string();
        assert!(err.contains("already assigned"), "{err}");
        assert_eq!(sources[1].read(&mut [Complex32::default(); 4]).unwrap(), 4);
    }

    #[test]
    fn dropping_member_before_adoption_detaches_its_replacement_slot() {
        let f = fixture();
        let mut sources = open_pair(&f.group);
        let old = current(&f.group);
        f.group.failed(&old);
        *f.time.lock().unwrap() += Duration::from_secs(2);
        let mut samples = [Complex32::default(); 4];
        assert_eq!(sources[1].read(&mut samples).unwrap(), 4);
        let replacement = current(&f.group);
        assert_ne!(old.id, replacement.id);
        assert!(replacement.slots.lock().unwrap()[0].is_some());
        drop(sources.remove(0));
        assert!(replacement.slots.lock().unwrap()[0].is_none());
        assert!(!f.group.attached[0].load(Ordering::Relaxed));
        assert_eq!(f.retires.load(Ordering::Relaxed), 1);
        assert_eq!(sources[0].read(&mut samples).unwrap(), 4);
        assert_eq!(samples[0].re, 1.0);
        assert!(f.group.is_current(&replacement));
        drop(sources);
        assert_eq!(f.retires.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn reopened_socket_stays_unhealthy_and_never_productive_backoff_caps() {
        let f = fixture();
        let _sources = open_pair(&f.group);
        for round in 0..10 {
            for counters in f.counters.lock().unwrap().last().unwrap() {
                counters.record_dropped(1);
            }
            let generation = current(&f.group);
            f.group.failed(&generation);
            *f.time.lock().unwrap() += Duration::from_secs(60);
            f.group.wait(0).unwrap().unwrap();
            assert_eq!(*f.health.lock().unwrap(), [false, false]);
            for total in &f.group.totals {
                assert_eq!(total.get().unwrap().snapshot().dropped_packets, round + 1);
            }
        }
        assert_eq!(
            f.group.state.lock().unwrap().backoff,
            Duration::from_secs(60)
        );
        assert_eq!(f.opens.load(Ordering::Relaxed), 11);
        assert_eq!(f.retires.load(Ordering::Relaxed), 10);
    }

    #[test]
    fn stop_during_backoff_returns_eof_with_one_pending_gap() {
        let f = fixture();
        let mut sources = open_pair(&f.group);
        sources[0].read(&mut [Complex32::default(); 4]).unwrap();
        f.group.failed(&current(&f.group));
        *f.time.lock().unwrap() += Duration::from_millis(500);
        f.group.stop.store(true, Ordering::Relaxed);
        let start = Instant::now();
        assert_eq!(sources[0].read(&mut [Complex32::default(); 4]).unwrap(), 0);
        assert_eq!(sources[0].take_discontinuity(), Some(23996));
        assert_eq!(sources[0].take_discontinuity(), None);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn stop_abandons_blocked_open_and_retires_late_success() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (retired_tx, retired_rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let group = Group::new(
            Box::new(move || {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                let retired_tx = retired_tx.clone();
                Ok(Opened {
                    sources: vec![Box::new(Samples {
                        fail: Arc::new(AtomicBool::new(false)),
                        counters: Arc::new(InputHealthCounters::new()),
                        value: 0.0,
                        gap: None,
                    })],
                    started_at: Instant::now(),
                    retire: Box::new(move || {
                        let _ = retired_tx.send(());
                    }),
                })
            }),
            stop.clone(),
            vec![Box::new(|_| {})],
            Arc::new(Instant::now),
        );
        let worker = std::thread::spawn(move || Member { group, index: 0 }.open().is_err());
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let at = Instant::now();
        stop.store(true, Ordering::Relaxed);
        assert!(worker.join().unwrap());
        assert!(at.elapsed() < Duration::from_secs(1));
        release_tx.send(()).unwrap();
        retired_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn opener_error_and_panic_fail_initial_startup() {
        for panics in [false, true] {
            let group = Group::new(
                Box::new(move || {
                    assert!(!panics, "test opener panic");
                    bail!("test open error")
                }),
                Arc::new(AtomicBool::new(false)),
                vec![Box::new(|_| {})],
                Arc::new(Instant::now),
            );
            let err = Member { group, index: 0 }.open().err().unwrap().to_string();
            assert!(
                err.contains(if panics {
                    "opener panicked"
                } else {
                    "test open error"
                }),
                "{err}"
            );
        }
    }

    #[test]
    fn delayed_worker_and_buffered_first_read_use_acquisition_boundary() {
        let f = fixture();
        let mut sources = open_pair(&f.group);
        sources[0].read(&mut [Complex32::default(); 4]).unwrap();
        f.group.failed(&current(&f.group));
        // Delay before the worker's acquisition trigger is part of the gap.
        *f.time.lock().unwrap() += Duration::from_secs(3);
        f.group.wait(0).unwrap().unwrap();
        // Delay after it starts has already acquired buffered samples.
        *f.time.lock().unwrap() += Duration::from_secs(2);
        sources[0].read(&mut [Complex32::default(); 4]).unwrap();
        assert_eq!(sources[0].take_discontinuity(), Some(143996));
        assert_eq!(
            sources[0].boundary,
            current(&f.group).started_at + Duration::from_secs_f64(4.0 / 48000.0)
        );
    }
    #[test]
    fn a_late_old_read_is_discarded_after_another_member_reopens() {
        struct BlockedRead {
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
            once: bool,
        }
        impl IqSource for BlockedRead {
            fn sample_rate(&self) -> f64 {
                48000.0
            }
            fn center_freq_hz(&self) -> f64 {
                7e6
            }
            fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
                if !self.once {
                    self.once = true;
                    self.entered.send(()).unwrap();
                    self.release.recv_timeout(Duration::from_secs(5)).unwrap();
                }
                buf[0] = Complex32::new(99.0, 0.0);
                Ok(1)
            }
        }
        let f = fixture();
        let mut sources = open_pair(&f.group);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let old = current(&f.group);
        sources[0].current.as_mut().unwrap().1 = Box::new(BlockedRead {
            entered: entered_tx,
            release: release_rx,
            once: false,
        });
        let mut first = sources.remove(0);
        let reader = std::thread::spawn(move || {
            let mut buf = [Complex32::default(); 4];
            let n = first.read(&mut buf).unwrap();
            (first, n, buf)
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        f.group.failed(&old);
        *f.time.lock().unwrap() += Duration::from_secs(2);
        sources[0].read(&mut [Complex32::default(); 4]).unwrap();
        release_tx.send(()).unwrap();
        let (mut first, n, buf) = reader.join().unwrap();
        assert_eq!(n, 4);
        assert_eq!(buf[0].re, 1.0, "old generation's 99 must never escape");
        assert_eq!(first.take_discontinuity(), Some(96000));
        assert_eq!(f.opens.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn incompatible_reopen_metadata_fails_and_retires_the_new_session() {
        struct WrongRate;
        impl IqSource for WrongRate {
            fn sample_rate(&self) -> f64 {
                96000.0
            }
            fn center_freq_hz(&self) -> f64 {
                7e6
            }
            fn read(&mut self, _: &mut [Complex32]) -> Result<usize> {
                panic!("must not read incompatible metadata")
            }
        }
        let f = fixture();
        let mut sources = open_pair(&f.group);
        f.group.failed(&current(&f.group));
        let retired = f.retires.clone();
        *f.group.opener.lock().unwrap() = Box::new(move || {
            let retired = retired.clone();
            Ok(Opened {
                sources: vec![Box::new(WrongRate), Box::new(WrongRate)],
                started_at: Instant::now(),
                retire: Box::new(move || {
                    retired.fetch_add(1, Ordering::Relaxed);
                }),
            })
        });
        *f.time.lock().unwrap() += Duration::from_secs(2);
        let error = sources[0]
            .read(&mut [Complex32::default(); 4])
            .unwrap_err()
            .to_string();
        assert!(error.contains("incompatible receiver rate, frequency or passband"));
        assert_eq!(f.retires.load(Ordering::Relaxed), 2);
        assert_eq!(*f.health.lock().unwrap(), [false, false]);
    }

    #[test]
    fn stop_during_blocked_reopen_returns_before_the_opener_and_discards_late_success() {
        let f = fixture();
        let mut sources = open_pair(&f.group);
        sources[0].read(&mut [Complex32::default(); 4]).unwrap();
        f.group.failed(&current(&f.group));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (retired_tx, retired_rx) = mpsc::channel();
        *f.group.opener.lock().unwrap() = Box::new(move || {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let retired_tx = retired_tx.clone();
            let sources = (0..2)
                .map(|_| {
                    Box::new(Samples {
                        fail: Arc::new(AtomicBool::new(false)),
                        counters: Arc::new(InputHealthCounters::new()),
                        value: 2.0,
                        gap: None,
                    }) as SendSource
                })
                .collect();
            Ok(Opened {
                sources,
                started_at: Instant::now(),
                retire: Box::new(move || {
                    let _ = retired_tx.send(());
                }),
            })
        });
        *f.time.lock().unwrap() += Duration::from_secs(1);
        let mut source = sources.remove(0);
        let reader = std::thread::spawn(move || {
            let n = source.read(&mut [Complex32::default(); 4]).unwrap();
            (n, source.take_discontinuity())
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let started = Instant::now();
        f.group.stop.store(true, Ordering::Relaxed);
        assert_eq!(reader.join().unwrap(), (0, Some(47996)));
        assert!(started.elapsed() < Duration::from_secs(1));
        release_tx.send(()).unwrap();
        retired_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(*f.health.lock().unwrap(), [false, false]);
    }
}
