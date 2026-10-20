use std::path::Path;
use std::time::Duration;

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use super::{ProcessSource, RawProcess, RawSample};

/// `ProcessSource` backed by the `sysinfo` crate.
pub struct SysinfoSource {
    sys: System,
    refresh_kind: ProcessRefreshKind,
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
        }
    }
}

impl ProcessSource for SysinfoSource {
    fn sample(&mut self, out: &mut RawSample) {
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        self.sys
            .refresh_processes_specifics(ProcessesToUpdate::All, true, self.refresh_kind);

        out.processes.clear();
        out.processes.extend(self.sys.processes().values().map(|p| {
            let disk = p.disk_usage();
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
    }
}
