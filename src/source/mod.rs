//! Process data backends.
//!
//! A `ProcessSource` reports *raw cumulative counters* (total CPU time, total
//! bytes read/written). Rates are computed by the sampler from deltas between
//! samples, so a backend only has to read counters. This maps directly onto
//! `NtQuerySystemInformation(SystemProcessInformation)`, which returns
//! KernelTime/UserTime and I/O transfer counts per process.

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
    /// Seconds since the Unix epoch; 0 if unknown.
    pub start_time: u64,
}

#[derive(Debug, Default)]
pub struct RawSample {
    pub processes: Vec<RawProcess>,
    /// System totals are already percentages; a backend that reads raw
    /// per-core idle/busy times computes them internally.
    pub system: SystemTotals,
}

pub trait ProcessSource: Send {
    /// Refresh all data and write it into `out`, reusing its allocations.
    /// `out` is cleared by the implementation.
    fn sample(&mut self, out: &mut RawSample);
}
