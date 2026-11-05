//! `ProcessSource` built on `NtQuerySystemInformation`.
//!
//! One `SystemProcessInformation` call per sample returns every process with
//! its CPU times, memory and I/O counters, without opening any of them, so
//! protected processes (System, Registry, csrss, ...) are included too. Only
//! the executable path needs a handle. It is looked up once per process and
//! remembered with the name, keyed by PID *and* creation time so a reused PID
//! is never mistaken for the old process.
//!
//! System CPU comes from `SystemProcessorPerformanceInformation` (per-core
//! idle/kernel/user times, as deltas between samples) and memory from
//! `GlobalMemoryStatusEx`.
//!
//! Limitation: per-core times cover the sampler thread's processor group,
//! so at most 64 logical processors.

use std::collections::HashMap;
use std::ffi::OsString;
use std::mem::size_of;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use windows_sys::Wdk::System::SystemInformation::{
    NtQuerySystemInformation, SystemProcessInformation, SystemProcessorPerformanceInformation,
};
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, STATUS_INFO_LENGTH_MISMATCH};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::System::WindowsProgramming::{
    SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION as CoreTimes, SYSTEM_PROCESS_INFORMATION as ProcessEntry,
};

use super::{ProcessSource, RawProcess, RawSample};
use crate::model::SystemTotals;

/// Seconds from 1601-01-01 (the FILETIME epoch) to 1970-01-01.
const EPOCH_DIFF_SECS: u64 = 11_644_473_600;
/// FILETIME / NT times count 100 ns ticks.
const TICKS_PER_SEC: u64 = 10_000_000;
/// First size of the process buffer; it grows when needed and is kept.
const INITIAL_BUFFER_BYTES: usize = 512 * 1024;
/// Longest path `QueryFullProcessImageNameW` can return, in UTF-16 units.
const MAX_PATH_UNITS: usize = 32_768;
/// Processor group size: the most `SystemProcessorPerformanceInformation`
/// reports.
const MAX_CORES: usize = 64;

/// What never changes while a process runs.
struct Identity {
    name: Arc<str>,
    exe: Option<Arc<Path>>,
    /// Sample number it was last seen in; older entries are dropped.
    seen: u64,
}

pub struct NtSource {
    /// `SystemProcessInformation` output; `u64`s keep it 8-byte aligned.
    buf: Vec<u64>,
    /// Keyed by (PID, creation time in 100 ns ticks).
    known: HashMap<(u32, u64), Identity>,
    samples: u64,
    path_buf: Vec<u16>,
    cores: Vec<CoreTimes>,
    prev_cores: Vec<CoreTimes>,
}

impl NtSource {
    pub fn new() -> Self {
        let mut source = Self {
            buf: Vec::new(),
            known: HashMap::new(),
            samples: 0,
            path_buf: Vec::new(),
            cores: Vec::with_capacity(MAX_CORES),
            prev_cores: Vec::with_capacity(MAX_CORES),
        };
        // A baseline, so the first sample has per-core load.
        source.read_cpu(&mut SystemTotals::default());
        source
    }

    /// Fills `buf` with the process list. False if the call keeps failing.
    fn query_processes(&mut self) -> bool {
        if self.buf.is_empty() {
            self.buf.resize(INITIAL_BUFFER_BYTES / 8, 0);
        }
        // Processes can start between a too-small call and the retry.
        for _ in 0..4 {
            let bytes = self.buf.len() * 8;
            let mut needed = 0u32;
            // SAFETY: the buffer is valid for `bytes` bytes of writes.
            let status = unsafe {
                NtQuerySystemInformation(
                    SystemProcessInformation,
                    self.buf.as_mut_ptr().cast(),
                    bytes as u32,
                    &mut needed,
                )
            };
            if status != STATUS_INFO_LENGTH_MISMATCH {
                return status >= 0;
            }
            let needed = needed as usize;
            let want = if needed > bytes { needed + needed / 8 } else { bytes * 2 };
            self.buf.resize(want.div_ceil(8), 0);
        }
        false
    }

    fn read_cpu(&mut self, system: &mut SystemTotals) {
        std::mem::swap(&mut self.cores, &mut self.prev_cores);
        self.cores.clear();
        let mut len = 0u32;
        // SAFETY: writes at most `capacity` entries, which `set_len` covers.
        let status = unsafe {
            NtQuerySystemInformation(
                SystemProcessorPerformanceInformation,
                self.cores.as_mut_ptr().cast(),
                (self.cores.capacity() * size_of::<CoreTimes>()) as u32,
                &mut len,
            )
        };
        if status >= 0 {
            let count = (len as usize / size_of::<CoreTimes>()).min(self.cores.capacity());
            // SAFETY: the kernel initialized `count` entries of plain data.
            unsafe { self.cores.set_len(count) };
        }

        system.per_core_percent.clear();
        let (mut busy_all, mut total_all) = (0i64, 0i64);
        for (now, before) in self.cores.iter().zip(&self.prev_cores) {
            // Kernel time includes idle time.
            let total = (now.KernelTime - before.KernelTime) + (now.UserTime - before.UserTime);
            let busy = total - (now.IdleTime - before.IdleTime);
            system.per_core_percent.push(percent(busy, total));
            busy_all += busy;
            total_all += total;
        }
        system.cpu_percent = percent(busy_all, total_all);
    }
}

impl ProcessSource for NtSource {
    fn sample(&mut self, out: &mut RawSample) {
        out.processes.clear();
        self.samples += 1;
        if self.query_processes() {
            let Self { buf, known, samples, path_buf, .. } = self;
            read_processes(buf, known, *samples, path_buf, &mut out.processes);
            known.retain(|_, id| id.seen == *samples);
        }
        self.read_cpu(&mut out.system);
        read_memory(&mut out.system);
    }
}

fn percent(busy: i64, total: i64) -> f32 {
    if total <= 0 {
        return 0.0;
    }
    (busy as f64 / total as f64 * 100.0).clamp(0.0, 100.0) as f32
}

/// Walks the `SystemProcessInformation` entries in `buf`.
fn read_processes(
    buf: &[u64],
    known: &mut HashMap<(u32, u64), Identity>,
    sample: u64,
    path_buf: &mut Vec<u16>,
    out: &mut Vec<RawProcess>,
) {
    let base = buf.as_ptr().cast::<u8>();
    let len = buf.len() * 8;
    let mut offset = 0;
    while offset + size_of::<ProcessEntry>() <= len {
        // SAFETY: in bounds (checked above), and the entry is plain data
        // (integers and raw pointers), so any bytes are a valid value.
        let entry = unsafe { base.add(offset).cast::<ProcessEntry>().read_unaligned() };
        let e = Fields(&entry);
        let pid = entry.UniqueProcessId as usize as u32;
        let created = e.create_time();

        let id = known.entry((pid, created)).or_insert_with(|| Identity {
            name: name_of(pid, &entry, base, len),
            exe: exe_path(pid, created, path_buf),
            seen: sample,
        });
        id.seen = sample;

        out.push(RawProcess {
            pid,
            parent_pid: Some(e.parent_pid()).filter(|&p| p != 0),
            name: Arc::clone(&id.name),
            exe: id.exe.clone(),
            // The idle "process" accumulates the idle time of every core.
            cpu_time: if pid == 0 { Duration::ZERO } else { e.cpu_time() },
            working_set: entry.WorkingSetSize as u64,
            // Private bytes, as `PROCESS_MEMORY_COUNTERS_EX::PrivateUsage`.
            private_bytes: entry.PagefileUsage as u64,
            read_bytes_total: e.read_bytes(),
            write_bytes_total: e.write_bytes(),
            gpu_percent: 0.0,
            gpu_dedicated: 0,
            gpu_shared: 0,
            start_time: (created / TICKS_PER_SEC).saturating_sub(EPOCH_DIFF_SECS),
        });

        if entry.NextEntryOffset == 0 {
            break;
        }
        offset += entry.NextEntryOffset as usize;
    }
}

/// The fields windows-sys leaves `Reserved`; offsets as in the NT headers
/// (phnt `SYSTEM_PROCESS_INFORMATION`), unchanged since Windows Vista.
struct Fields<'a>(&'a ProcessEntry);

impl Fields<'_> {
    /// `Reserved1` holds WorkingSetPrivateSize, HardFaultCount,
    /// NumberOfThreadsHighWatermark, CycleTime, CreateTime, UserTime and
    /// KernelTime, in that order.
    fn reserved1(&self, at: usize) -> u64 {
        let bytes: [u8; 8] = self.0.Reserved1[at..at + 8].try_into().unwrap();
        u64::from_ne_bytes(bytes)
    }

    fn create_time(&self) -> u64 {
        self.reserved1(24)
    }

    fn cpu_time(&self) -> Duration {
        let ticks = self.reserved1(32).saturating_add(self.reserved1(40));
        Duration::from_nanos(ticks.saturating_mul(100))
    }

    /// `Reserved2` is InheritedFromUniqueProcessId.
    fn parent_pid(&self) -> u32 {
        self.0.Reserved2 as usize as u32
    }

    /// `Reserved7` is the Read, Write and Other operation counts, then the
    /// Read, Write and Other transfer counts (bytes).
    fn read_bytes(&self) -> u64 {
        self.0.Reserved7[3] as u64
    }

    fn write_bytes(&self) -> u64 {
        self.0.Reserved7[4] as u64
    }
}

/// The image name; its characters live in the same buffer.
fn name_of(pid: u32, entry: &ProcessEntry, base: *const u8, len: usize) -> Arc<str> {
    let name = entry.ImageName;
    let start = name.Buffer as usize;
    let bytes = usize::from(name.Length);
    let in_buffer = start >= base as usize && start + bytes <= base as usize + len && start.is_multiple_of(2);
    if pid == 0 || bytes == 0 || !in_buffer {
        return match pid {
            0 => "System Idle Process".into(),
            _ => format!("<pid {pid}>").into(),
        };
    }
    // SAFETY: checked to lie inside `buf` and to be aligned for u16.
    let units = unsafe { std::slice::from_raw_parts(name.Buffer.cast_const(), bytes / 2) };
    String::from_utf16_lossy(units).into()
}

/// Full path of the executable, if the process can be opened.
fn exe_path(pid: u32, created: u64, path_buf: &mut Vec<u16>) -> Option<Arc<Path>> {
    // The idle process and System have no executable.
    if pid == 0 || pid == 4 {
        return None;
    }
    if path_buf.is_empty() {
        path_buf.resize(MAX_PATH_UNITS, 0);
    }
    // SAFETY: plain Win32 calls on a handle closed before returning; the
    // path buffer is valid for `size` units.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut times = [FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 }; 4];
        let [creation, exit, kernel, user] = &mut times;
        let same_process = GetProcessTimes(handle, creation, exit, kernel, user) != 0
            && (u64::from(creation.dwHighDateTime) << 32 | u64::from(creation.dwLowDateTime)) == created;
        let mut size = path_buf.len() as u32;
        // A PID reused since the snapshot is a different process.
        let ok = same_process
            && QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, path_buf.as_mut_ptr(), &mut size) != 0;
        CloseHandle(handle);
        ok.then(|| Arc::from(PathBuf::from(OsString::from_wide(&path_buf[..size as usize]))))
    }
}

fn read_memory(system: &mut SystemTotals) {
    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        dwMemoryLoad: 0,
        ullTotalPhys: 0,
        ullAvailPhys: 0,
        ullTotalPageFile: 0,
        ullAvailPageFile: 0,
        ullTotalVirtual: 0,
        ullAvailVirtual: 0,
        ullAvailExtendedVirtual: 0,
    };
    // SAFETY: `dwLength` is set as the call requires.
    if unsafe { GlobalMemoryStatusEx(&mut status) } != 0 {
        system.total_memory = status.ullTotalPhys;
        system.used_memory = status.ullTotalPhys - status.ullAvailPhys;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_the_nt_headers() {
        // x64 offsets of the fields read from the documented struct.
        assert_eq!(size_of::<ProcessEntry>(), 256);
        assert_eq!(std::mem::offset_of!(ProcessEntry, Reserved1), 8);
        assert_eq!(std::mem::offset_of!(ProcessEntry, ImageName), 56);
        assert_eq!(std::mem::offset_of!(ProcessEntry, UniqueProcessId), 80);
        assert_eq!(std::mem::offset_of!(ProcessEntry, Reserved2), 88);
        assert_eq!(std::mem::offset_of!(ProcessEntry, WorkingSetSize), 144);
        assert_eq!(std::mem::offset_of!(ProcessEntry, PagefileUsage), 184);
        assert_eq!(std::mem::offset_of!(ProcessEntry, Reserved7), 208);
        assert_eq!(size_of::<CoreTimes>(), 48);
    }

    #[test]
    fn samples_this_process() {
        let mut source = NtSource::new();
        let mut out = RawSample::default();
        source.sample(&mut out);
        let me = std::process::id();
        let p = out.processes.iter().find(|p| p.pid == me).expect("own process listed");
        let exe = std::env::current_exe().unwrap();
        let found = p.exe.as_deref().expect("own path readable").to_string_lossy().into_owned();
        assert!(found.eq_ignore_ascii_case(&exe.to_string_lossy()), "{found} vs {exe:?}");
        assert_eq!(&*p.name, exe.file_name().unwrap().to_str().unwrap());
        assert!(p.working_set > 0 && p.start_time > 1_600_000_000);
        assert!(out.processes.iter().any(|p| p.pid == 4 && &*p.name == "System"));
        assert!(!out.system.per_core_percent.is_empty());
        assert!(out.system.used_memory > 0 && out.system.used_memory <= out.system.total_memory);

        // Identity is remembered: the same allocation next time.
        let name = Arc::clone(&p.name);
        source.sample(&mut out);
        let again = out.processes.iter().find(|p| p.pid == me).unwrap();
        assert!(Arc::ptr_eq(&name, &again.name));
    }
}
