//! Background sampling thread.
//!
//! The thread owns the `ProcessSource`, turns raw counters into rates, and
//! publishes an immutable `Arc<Snapshot>`. Readers only ever lock long enough
//! to clone the `Arc`, so the UI never waits on a poll.

use std::collections::HashMap;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::model::{ProcessInfo, Snapshot};
use crate::source::{ProcessSource, RawSample};

enum Control {
    SetInterval(Duration),
    RefreshNow,
    Stop,
}

/// Cloneable, `Send` handle that asks the sampler for an immediate sample.
#[derive(Clone)]
pub struct Refresher(Sender<Control>);

impl Refresher {
    pub fn refresh_now(&self) {
        let _ = self.0.send(Control::RefreshNow);
    }
}

pub struct SamplerHandle {
    latest: Arc<Mutex<Arc<Snapshot>>>,
    ctrl: Sender<Control>,
    thread: Option<JoinHandle<()>>,
}

impl SamplerHandle {
    /// Start sampling. `notify` is called after each snapshot is published
    /// (the UI uses it to request a repaint).
    pub fn spawn<S, F>(source: S, interval: Duration, notify: F) -> Self
    where
        S: ProcessSource + 'static,
        F: Fn() + Send + 'static,
    {
        let latest = Arc::new(Mutex::new(Arc::new(Snapshot::default())));
        let (ctrl, ctrl_rx) = mpsc::channel();
        let shared = Arc::clone(&latest);

        let thread = thread::Builder::new()
            .name("sampler".into())
            .spawn(move || {
                let mut sampler = Sampler::new(source);
                let mut interval = interval;
                loop {
                    let started = Instant::now();
                    let snapshot = Arc::new(sampler.next_snapshot());
                    // Swap under the lock, drop the old snapshot outside it.
                    let old = std::mem::replace(&mut *shared.lock().unwrap(), snapshot);
                    drop(old);
                    notify();

                    let wait = interval.saturating_sub(started.elapsed());
                    match ctrl_rx.recv_timeout(wait) {
                        Ok(Control::SetInterval(d)) => interval = d,
                        Ok(Control::RefreshNow) => {}
                        Ok(Control::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => {}
                    }
                }
            })
            .expect("failed to spawn sampler thread");

        Self {
            latest,
            ctrl,
            thread: Some(thread),
        }
    }

    /// The most recently published snapshot. Cheap: one lock + `Arc` clone.
    pub fn latest(&self) -> Arc<Snapshot> {
        Arc::clone(&self.latest.lock().unwrap())
    }

    /// Change the polling interval; takes effect immediately.
    pub fn set_interval(&self, interval: Duration) {
        let _ = self.ctrl.send(Control::SetInterval(interval));
    }

    pub fn refresher(&self) -> Refresher {
        Refresher(self.ctrl.clone())
    }
}

impl Drop for SamplerHandle {
    fn drop(&mut self) {
        let _ = self.ctrl.send(Control::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Counters remembered from the previous sample, keyed by PID.
struct PrevCounters {
    start_time: u64,
    cpu_time: Duration,
    read_bytes_total: u64,
    write_bytes_total: u64,
}

struct Sampler<S> {
    source: S,
    raw: RawSample,
    prev: HashMap<u32, PrevCounters>,
    next_prev: HashMap<u32, PrevCounters>,
    last_sampled_at: Option<Instant>,
    seq: u64,
}

impl<S: ProcessSource> Sampler<S> {
    fn new(source: S) -> Self {
        Self {
            source,
            raw: RawSample::default(),
            prev: HashMap::new(),
            next_prev: HashMap::new(),
            last_sampled_at: None,
            seq: 0,
        }
    }

    fn next_snapshot(&mut self) -> Snapshot {
        let started = Instant::now();
        self.source.sample(&mut self.raw);
        let sampled_at = Instant::now();
        let sample_cost = sampled_at - started;

        let elapsed_secs = self
            .last_sampled_at
            .map(|t| (sampled_at - t).as_secs_f64())
            .filter(|&s| s > 0.0);
        self.last_sampled_at = Some(sampled_at);

        let cores = self.raw.system.per_core_percent.len().max(1) as f64;

        self.next_prev.clear();
        let mut processes = Vec::with_capacity(self.raw.processes.len());
        for p in self.raw.processes.drain(..) {
            let (cpu_percent, read_rate, write_rate) = match (self.prev.get(&p.pid), elapsed_secs)
            {
                // Same PID *and* same start time: the same process, not a reused PID.
                (Some(old), Some(secs)) if old.start_time == p.start_time => {
                    let cpu = p.cpu_time.saturating_sub(old.cpu_time).as_secs_f64();
                    let read = p.read_bytes_total.saturating_sub(old.read_bytes_total);
                    let write = p.write_bytes_total.saturating_sub(old.write_bytes_total);
                    (
                        (cpu / (secs * cores) * 100.0).clamp(0.0, 100.0) as f32,
                        (read as f64 / secs) as u64,
                        (write as f64 / secs) as u64,
                    )
                }
                _ => (0.0, 0, 0),
            };

            self.next_prev.insert(
                p.pid,
                PrevCounters {
                    start_time: p.start_time,
                    cpu_time: p.cpu_time,
                    read_bytes_total: p.read_bytes_total,
                    write_bytes_total: p.write_bytes_total,
                },
            );

            processes.push(ProcessInfo {
                pid: p.pid,
                parent_pid: p.parent_pid,
                name: p.name,
                exe: p.exe,
                cpu_percent,
                working_set: p.working_set,
                private_bytes: p.private_bytes,
                disk_read_per_sec: read_rate,
                disk_write_per_sec: write_rate,
                start_time: p.start_time,
            });
        }
        std::mem::swap(&mut self.prev, &mut self.next_prev);

        self.seq += 1;
        Snapshot {
            seq: self.seq,
            unix_time: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            processes,
            system: self.raw.system.clone(),
            sample_cost,
        }
    }
}
