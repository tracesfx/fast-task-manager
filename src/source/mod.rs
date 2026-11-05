//! Process data backends.
//!
//! A `ProcessSource` reports *raw cumulative counters* (total CPU time, total
//! bytes read/written). Rates are computed by the sampler from deltas between
//! samples, so a backend only has to read counters. This maps directly onto
//! `NtQuerySystemInformation(SystemProcessInformation)`, which returns
//! KernelTime/UserTime and I/O transfer counts per process.
//!
//! GPU usage is the exception (like system CPU %): it comes from performance
//! counters that already report percentages; see `gpu`.

mod gpu;
mod sysinfo_source;

pub use sysinfo_source::SysinfoSource;

use std::path::PathBuf;
use std::time::Duration;

use crate::model::SystemTotals;

#[derive(Clone, Debug)]
pub struct RawProcess {
    pub pid: u32,
    pub parent_pid: Option<u32>,
    pub name: String,
    pub exe: Option<PathBuf>,
    /// Total CPU time (kernel + user) consumed since the process started.
    pub cpu_time: Duration,
    pub working_set: u64,
    pub private_bytes: u64,
    /// Total bytes read since the process started.
    pub read_bytes_total: u64,
    /// Total bytes written since the process started.
    pub write_bytes_total: u64,
    /// Busiest GPU engine, 0..=100 (pre-computed by the counter source).
    pub gpu_percent: f32,
    /// Dedicated / shared GPU memory committed, bytes (current, not cumulative).
    pub gpu_dedicated: u64,
    pub gpu_shared: u64,
    /// Seconds since the Unix epoch; 0 if unknown.
    pub start_time: u64,
}

#[derive(Debug, Default)]
pub struct RawSample {
    pub processes: Vec<RawProcess>,
    /// System totals are already percentages; a backend that reads raw
    /// per-core idle/busy times computes them internally. `system.gpu` is
    /// `None` when GPU counters are unavailable.
    pub system: SystemTotals,
}

pub trait ProcessSource: Send {
    /// Refresh all data and write it into `out`, reusing its allocations.
    /// `out` is cleared by the implementation.
    fn sample(&mut self, out: &mut RawSample);
}
