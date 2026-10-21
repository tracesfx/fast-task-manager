//! Immutable data published by the sampler and read by the UI.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// One process, with rates already computed from deltas between samples.
#[derive(Clone, Debug)]
pub struct ProcessInfo {
    pub pid: u32,
    pub parent_pid: Option<u32>,
    /// Name, path and group key never change while a process runs; they
    /// are shared from one snapshot to the next rather than copied.
    pub name: Arc<str>,
    pub exe: Option<Arc<Path>>,
    /// The app group it belongs to (`view::group_key`).
    pub group_key: Arc<str>,
    /// 0..=100, normalized to all logical cores (100% = every core saturated).
    pub cpu_percent: f32,
    pub working_set: u64,
    pub private_bytes: u64,
    pub disk_read_per_sec: u64,
    pub disk_write_per_sec: u64,
    /// Seconds since the Unix epoch; 0 if unknown (e.g. Idle / System).
    pub start_time: u64,
}

#[derive(Clone, Debug, Default)]
pub struct SystemTotals {
    pub cpu_percent: f32,
    pub per_core_percent: Vec<f32>,
    pub total_memory: u64,
    pub used_memory: u64,
}

/// A complete, immutable view of the system at one point in time.
/// Shared with the UI as `Arc<Snapshot>`; never mutated after publishing.
#[derive(Debug, Default)]
pub struct Snapshot {
    /// Increments with every published snapshot (0 = nothing sampled yet).
    pub seq: u64,
    /// Wall-clock time of the sample, seconds since the Unix epoch.
    pub unix_time: u64,
    pub processes: Vec<ProcessInfo>,
    pub system: SystemTotals,
    /// How long the source took to collect this sample.
    pub sample_cost: Duration,
}
