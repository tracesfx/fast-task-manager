use std::path::Path;
use std::time::Duration;

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use super::gpu::{GpuCounters, GpuReading};
use super::{ProcessSource, RawProcess, RawSample};

/// `ProcessSource` backed by the `sysinfo` crate.
pub struct SysinfoSource {
    sys: System,
    refresh_kind: ProcessRefreshKind,
    /// `None` if the GPU performance counters can't be opened.
    gpu: Option<GpuCounters>,
    gpu_reading: GpuReading,
}

impl SysinfoSource {
    pub fn new() -> Self {
        let mut sys = System::new();
        // Prime the CPU list so the first real sample has a baseline.
        sys.refresh_cpu_usage();
        Self {
            sys,
            refresh_kind: ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory()
                .with_disk_usage()
                // The exe path never changes for a live process; query it once.
                .with_exe(UpdateKind::OnlyIfNotSet),
            gpu: GpuCounters::new(),
            gpu_reading: GpuReading::default(),
        }
    }
}

impl ProcessSource for SysinfoSource {
    fn sample(&mut self, out: &mut RawSample) {
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        self.sys
            .refresh_processes_specifics(ProcessesToUpdate::All, true, self.refresh_kind);
        if let Some(gpu) = &mut self.gpu {
            gpu.collect(&mut self.gpu_reading);
        }
        let gpu_procs = &self.gpu_reading.processes;

        out.processes.clear();
        out.processes.extend(self.sys.processes().values().map(|p| {
            let disk = p.disk_usage();
            let gpu = gpu_procs.get(&p.pid().as_u32()).copied().unwrap_or_default();
            RawProcess {
                pid: p.pid().as_u32(),
                parent_pid: p.parent().map(|pp| pp.as_u32()),
                name: p.name().to_string_lossy().into_owned(),
                exe: p.exe().map(Path::to_path_buf),
                cpu_time: Duration::from_millis(p.accumulated_cpu_time()),
                working_set: p.memory(),
                // On Windows sysinfo fills this with the private byte count
                // (PrivatePageCount / PrivateUsage), not the virtual size.
                private_bytes: p.virtual_memory(),
                read_bytes_total: disk.total_read_bytes,
                write_bytes_total: disk.total_written_bytes,
                gpu_percent: gpu.percent,
                gpu_dedicated: gpu.dedicated,
                gpu_shared: gpu.shared,
                start_time: p.start_time(),
            }
        }));

        let system = &mut out.system;
        system.cpu_percent = self.sys.global_cpu_usage();
        system.per_core_percent.clear();
        system
            .per_core_percent
            .extend(self.sys.cpus().iter().map(|c| c.cpu_usage()));
        system.total_memory = self.sys.total_memory();
        system.used_memory = self.sys.used_memory();
        system.gpu = self.gpu.is_some().then_some(self.gpu_reading.totals);
    }
}
