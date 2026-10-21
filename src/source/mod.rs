//! Process data backends.
//!
//! A `ProcessSource` reports *raw cumulative counters* (total CPU time, total
//! bytes read/written). Rates are computed by the sampler from deltas between
//! samples, so a backend only has to read counters.
//!
//! `NtSource` is the backend the app uses: one
//! `NtQuerySystemInformation(SystemProcessInformation)` call per sample.
//! `SysinfoSource` (feature `sysinfo`) is the previous backend, kept to
//! compare against: `FTM_SOURCE=sysinfo` selects it at runtime, and the tests
//! below check the two agree and benchmark them.

mod nt_source;
#[cfg(feature = "sysinfo")]
mod sysinfo_source;

pub use nt_source::NtSource;
#[cfg(feature = "sysinfo")]
pub use sysinfo_source::SysinfoSource;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::model::SystemTotals;

#[derive(Clone, Debug)]
pub struct RawProcess {
    pub pid: u32,
    pub parent_pid: Option<u32>,
    /// Shared, so a backend that remembers it per process hands out the
    /// same allocation on every sample.
    pub name: Arc<str>,
    pub exe: Option<Arc<Path>>,
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

#[cfg(all(test, feature = "sysinfo"))]
mod tests {
    use std::collections::HashMap;
    use std::time::Instant;

    use super::*;

    fn sample(source: &mut impl ProcessSource) -> RawSample {
        let mut out = RawSample::default();
        source.sample(&mut out);
        out
    }

    /// Both backends see the same processes with the same identity.
    #[test]
    fn backends_agree() {
        let (mut nt, mut si) = (NtSource::new(), SysinfoSource::new());
        let (a, b) = (sample(&mut nt), sample(&mut si));
        let by_pid: HashMap<u32, &RawProcess> = b.processes.iter().map(|p| (p.pid, p)).collect();

        let (mut matched, mut unopened) = (0, 0);
        for p in &a.processes {
            let Some(q) = by_pid.get(&p.pid) else {
                continue;
            };
            // sysinfo has no start time (nor CPU time or memory) for the
            // processes it can't open; this backend reads them anyway.
            if q.start_time == 0 {
                unopened += 1;
                assert!(p.start_time != 0 || p.pid == 0, "pid {} ({})", p.pid, p.name);
            } else if q.start_time != p.start_time {
                continue; // PID reused between the two samples
            }
            matched += 1;
            if p.pid != 0 {
                assert_eq!(p.name, q.name, "pid {}", p.pid);
            }
            assert_eq!(p.parent_pid, q.parent_pid, "parent of {} ({})", p.pid, p.name);
            // sysinfo reports "" when it can open a process but not read its
            // path; this backend may succeed where it didn't.
            if let (Some(x), Some(y)) = (&p.exe, &q.exe) {
                if !y.as_os_str().is_empty() {
                    let (x, y) = (x.to_string_lossy(), y.to_string_lossy());
                    assert!(x.eq_ignore_ascii_case(&y), "exe of {}: {x} vs {y}", p.pid);
                }
            }
        }
        // Processes come and go between the two samples, but most match.
        println!("{matched} of {} matched; sysinfo couldn't open {unopened}", b.processes.len());
        assert!(matched * 10 >= b.processes.len() * 9, "{matched} of {}", b.processes.len());
        assert_eq!(a.system.per_core_percent.len(), b.system.per_core_percent.len());
        assert_eq!(a.system.total_memory, b.system.total_memory);
    }

    /// Prints how long a sample takes with each backend:
    /// `cargo test --release --features sysinfo -- --ignored --nocapture bench_backends`
    #[test]
    #[ignore]
    fn bench_backends() {
        const RUNS: usize = 50;
        fn bench(name: &str, source: &mut impl ProcessSource) {
            let mut out = RawSample::default();
            // The first sample fills caches (exe paths, sysinfo's handles).
            source.sample(&mut out);
            let mut times: Vec<f64> = (0..RUNS)
                .map(|_| {
                    let started = Instant::now();
                    source.sample(&mut out);
                    started.elapsed().as_secs_f64() * 1e3
                })
                .collect();
            times.sort_by(f64::total_cmp);
            println!(
                "{name:<8} {} processes: median {:.2} ms, p90 {:.2} ms, min {:.2} ms",
                out.processes.len(),
                times[RUNS / 2],
                times[RUNS * 9 / 10],
                times[0]
            );
        }
        bench("nt", &mut NtSource::new());
        bench("sysinfo", &mut SysinfoSource::new());
    }
}
