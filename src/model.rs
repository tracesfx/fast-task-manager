//! Immutable data published by the sampler and read by the UI.

use std::path::PathBuf;
use std::time::Duration;

/// One process, with rates already computed from deltas between samples.
#[derive(Clone, Debug)]
pub struct ProcessInfo {
    pub pid: u32,
    pub parent_pid: Option<u32>,
    pub name: String,
    pub exe: Option<PathBuf>,
    /// 0..=100, normalized to all logical cores (100% = every core saturated).
    pub cpu_percent: f32,
    pub working_set: u64,
    pub private_bytes: u64,
    pub disk_read_per_sec: u64,
    pub disk_write_per_sec: u64,
    /// Busiest GPU engine used by this process, 0..=100.
    pub gpu_percent: f32,
    /// Dedicated (on-board) GPU memory committed by this process.
    pub gpu_dedicated: u64,
    /// Shared (system RAM) GPU memory committed by this process.
    pub gpu_shared: u64,
    /// Seconds since the Unix epoch; 0 if unknown (e.g. Idle / System).
    pub start_time: u64,
}

#[derive(Clone, Debug, Default)]
pub struct SystemTotals {
    pub cpu_percent: f32,
    pub per_core_percent: Vec<f32>,
    pub total_memory: u64,
    pub used_memory: u64,
    /// `None` when the GPU performance counters aren't available.
    pub gpu: Option<GpuTotals>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct GpuTotals {
    /// Busiest engine across all adapters, summed over processes, 0..=100.
    pub percent: f32,
    /// Dedicated GPU memory in use, all adapters.
    pub dedicated: u64,
    /// Shared GPU memory in use, all adapters.
    pub shared: u64,
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
