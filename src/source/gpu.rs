//! GPU usage from the Windows "GPU Engine" / "GPU Process Memory" / "GPU
//! Adapter Memory" performance counters (the same source Task Manager uses,
//! WDDM 2.0+).
//!
//! Instance names look like
//! `pid_1234_luid_0x00000000_0x0000C2A3_phys_0_eng_3_engtype_3D` (engines) or
//! `pid_1234_luid_0x00000000_0x0000C2A3_phys_0` (process memory).
//!
//! Utilization is the one value read *formatted* rather than raw: PDH turns
//! each engine's running time into a percentage over the time between the
//! last two `PdhCollectQueryData` calls, so, like system CPU %, GPU % arrives
//! pre-computed. Following Task Manager:
//! - a process's GPU % is its busiest engine;
//! - system GPU % is the busiest engine, summed over all processes.

use std::collections::HashMap;
use std::ptr;

use windows_sys::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
    PdhOpenQueryW, PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT, PDH_FMT_COUNTERVALUE_ITEM_W,
    PDH_FMT_DOUBLE, PDH_FMT_LARGE, PDH_MORE_DATA,
};

use crate::model::GpuTotals;

const ERROR_SUCCESS: u32 = 0;
/// `PDH_FMT_NOCAP100`: don't clamp engine utilization to 100 before we sum it.
const PDH_FMT_NOCAP100: PDH_FMT = 0x0000_8000;

/// One process's GPU usage, summed over all adapters.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessGpu {
    pub percent: f32,
    pub dedicated: u64,
    pub shared: u64,
}

/// The result of one collection. Reused between samples.
#[derive(Debug, Default)]
pub struct GpuReading {
    pub processes: HashMap<u32, ProcessGpu>,
    pub totals: GpuTotals,
}

pub struct GpuCounters {
    query: isize,
    engine_utilization: isize,
    process_dedicated: Option<isize>,
    process_shared: Option<isize>,
    adapter_dedicated: Option<isize>,
    adapter_shared: Option<isize>,
    /// Backing store for PDH's item arrays (`u64` for alignment).
    buf: Vec<u64>,
    /// Engine (instance name without the `pid_N_` prefix) → summed utilization.
    /// Keys are kept between samples so steady state doesn't allocate.
    engines: HashMap<Vec<u16>, f64>,
}

impl GpuCounters {
    /// `None` if the GPU engine counters aren't available (no WDDM 2.0
    /// driver, some VMs, or the performance counters are disabled).
    pub fn new() -> Option<Self> {
        let mut query = 0;
        // SAFETY: valid out-pointer; a null data source means live data.
        if unsafe { PdhOpenQueryW(ptr::null(), 0, &mut query) } != ERROR_SUCCESS {
            return None;
        }
        let add = |path: &str| -> Option<isize> {
            let wide: Vec<u16> = path.encode_utf16().chain([0]).collect();
            let mut counter = 0;
            // SAFETY: `wide` is NUL-terminated and outlives the call.
            let status = unsafe { PdhAddEnglishCounterW(query, wide.as_ptr(), 0, &mut counter) };
            (status == ERROR_SUCCESS).then_some(counter)
        };

        let Some(engine_utilization) = add(r"\GPU Engine(*)\Utilization Percentage") else {
            // SAFETY: `query` is open and closed exactly once.
            unsafe { PdhCloseQuery(query) };
            return None;
        };
        let counters = Self {
            query,
            engine_utilization,
            process_dedicated: add(r"\GPU Process Memory(*)\Dedicated Usage"),
            process_shared: add(r"\GPU Process Memory(*)\Shared Usage"),
            adapter_dedicated: add(r"\GPU Adapter Memory(*)\Dedicated Usage"),
            adapter_shared: add(r"\GPU Adapter Memory(*)\Shared Usage"),
            buf: Vec::new(),
            engines: HashMap::new(),
        };
        // Prime the rate counter so the first real sample has a baseline.
        // SAFETY: `query` is open.
        unsafe { PdhCollectQueryData(query) };
        Some(counters)
    }

    /// Collects all counters into `out` (cleared first).
    pub fn collect(&mut self, out: &mut GpuReading) {
        out.processes.clear();
        out.totals = GpuTotals::default();
        // SAFETY: `self.query` is open.
        if unsafe { PdhCollectQueryData(self.query) } != ERROR_SUCCESS {
            return;
        }

        for v in self.engines.values_mut() {
            *v = 0.0;
        }
        let engines = &mut self.engines;
        let procs = &mut out.processes;
        read_array(&mut self.buf, self.engine_utilization, PDH_FMT_DOUBLE | PDH_FMT_NOCAP100, |name, item| {
            // SAFETY: requested as PDH_FMT_DOUBLE.
            let pct = unsafe { item.FmtValue.Anonymous.doubleValue };
            let Some((pid, engine)) = split_instance(name) else {
                return;
            };
            if !pct.is_finite() || pct <= 0.0 {
                return;
            }
            let entry = procs.entry(pid).or_default();
            entry.percent = entry.percent.max(pct as f32);
            match engines.get_mut(engine) {
                Some(sum) => *sum += pct,
                None => {
                    engines.insert(engine.to_vec(), pct);
                }
            }
        });
        let busiest = engines.values().copied().fold(0.0, f64::max);
        out.totals.percent = busiest.min(100.0) as f32;
        for p in procs.values_mut() {
            p.percent = p.percent.min(100.0);
        }

        let mut per_process = |counter: Option<isize>, field: fn(&mut ProcessGpu) -> &mut u64| {
            if let Some(counter) = counter {
                read_array(&mut self.buf, counter, PDH_FMT_LARGE, |name, item| {
                    // SAFETY: requested as PDH_FMT_LARGE.
                    let bytes = unsafe { item.FmtValue.Anonymous.largeValue };
                    if let (Some((pid, _)), Ok(bytes @ 1..)) = (split_instance(name), u64::try_from(bytes)) {
                        *field(procs.entry(pid).or_default()) += bytes;
                    }
                });
            }
        };
        per_process(self.process_dedicated, |p| &mut p.dedicated);
        per_process(self.process_shared, |p| &mut p.shared);

        let mut adapter_sum = |counter: Option<isize>| {
            let mut total = 0u64;
            if let Some(counter) = counter {
                read_array(&mut self.buf, counter, PDH_FMT_LARGE, |_, item| {
                    // SAFETY: requested as PDH_FMT_LARGE.
                    let bytes = unsafe { item.FmtValue.Anonymous.largeValue };
                    total += u64::try_from(bytes).unwrap_or(0);
                });
            }
            total
        };
        out.totals.dedicated = adapter_sum(self.adapter_dedicated);
        out.totals.shared = adapter_sum(self.adapter_shared);
    }
}

impl Drop for GpuCounters {
    fn drop(&mut self) {
        // SAFETY: opened in `new`, closed only here (also frees the counters).
        unsafe { PdhCloseQuery(self.query) };
    }
}

/// Calls `f(instance_name, item)` for every instance of a wildcard counter
/// that has valid data.
fn read_array(
    buf: &mut Vec<u64>,
    counter: isize,
    format: PDH_FMT,
    mut f: impl FnMut(&[u16], &PDH_FMT_COUNTERVALUE_ITEM_W),
) {
    // Instances can appear between the size query and the read; retry a few times.
    for _ in 0..4 {
        let mut size_bytes = (buf.len() * 8) as u32;
        let mut count = 0u32;
        // SAFETY: `buf` holds `size_bytes` writable, 8-aligned bytes (or the
        // pointer is unused when the size is 0).
        let status = unsafe {
            PdhGetFormattedCounterArrayW(
                counter,
                format,
                &mut size_bytes,
                &mut count,
                buf.as_mut_ptr().cast(),
            )
        };
        if status == PDH_MORE_DATA {
            buf.resize((size_bytes as usize).div_ceil(8) + 64, 0);
            continue;
        }
        if status != ERROR_SUCCESS {
            return;
        }
        // SAFETY: on success PDH wrote `count` items at the start of `buf`;
        // their name pointers point into the rest of `buf`.
        let items = unsafe {
            std::slice::from_raw_parts(buf.as_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>(), count as usize)
        };
        for item in items {
            let status = item.FmtValue.CStatus;
            if status != PDH_CSTATUS_VALID_DATA && status != PDH_CSTATUS_NEW_DATA {
                continue;
            }
            // SAFETY: a NUL-terminated wide string inside `buf`.
            let name = unsafe { wide_str(item.szName) };
            f(name, item);
        }
        return;
    }
}

unsafe fn wide_str<'a>(p: *const u16) -> &'a [u16] {
    if p.is_null() {
        return &[];
    }
    let mut len = 0;
    while *p.add(len) != 0 {
        len += 1;
    }
    std::slice::from_raw_parts(p, len)
}

/// `pid_1234_luid_..._eng_0_engtype_3D` → `(1234, "luid_..._eng_0_engtype_3D")`.
fn split_instance(name: &[u16]) -> Option<(u32, &[u16])> {
    let rest = name.strip_prefix(&[b'p' as u16, b'i' as u16, b'd' as u16, b'_' as u16])?;
    let digits = rest.iter().take_while(|&&c| (b'0' as u16..=b'9' as u16).contains(&c)).count();
    if digits == 0 || rest.get(digits) != Some(&(b'_' as u16)) {
        return None;
    }
    let mut pid: u32 = 0;
    for &c in &rest[..digits] {
        pid = pid.checked_mul(10)?.checked_add(u32::from(c - b'0' as u16))?;
    }
    Some((pid, &rest[digits + 1..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn splits_engine_instance_names() {
        let name = w("pid_1234_luid_0x00000000_0x0000C2A3_phys_0_eng_3_engtype_3D");
        let (pid, engine) = split_instance(&name).unwrap();
        assert_eq!(pid, 1234);
        assert_eq!(engine, &w("luid_0x00000000_0x0000C2A3_phys_0_eng_3_engtype_3D")[..]);

        assert!(split_instance(&w("luid_0x0_phys_0")).is_none());
        assert!(split_instance(&w("pid__luid")).is_none());
        assert!(split_instance(&w("pid_99999999999_luid")).is_none());
    }
}
