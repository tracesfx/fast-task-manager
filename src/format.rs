//! Human-readable formatting helpers for the UI, plus sort keys that are
//! quantized to the displayed precision. Sorting on the displayed value means
//! two rows that *look* equal never swap places because of invisible noise.

const KIB: f64 = 1024.0;
const MIB: f64 = 1024.0 * 1024.0;

pub fn bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = b as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{b} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Process memory, always in MB (like Task Manager) so columns line up.
pub fn mem(b: u64) -> String {
    format!("{:.1} MB", b as f64 / MIB)
}

pub fn rate(bytes_per_sec: u64) -> String {
    let b = bytes_per_sec as f64;
    if bytes_per_sec == 0 {
        "0".to_owned()
    } else if b < MIB {
        format!("{:.1} KB/s", b / KIB)
    } else {
        format!("{:.1} MB/s", b / MIB)
    }
}

pub fn cpu(percent: f32) -> String {
    format!("{percent:.1}")
}

/// Formats a span of seconds as `[Nd ]HH:MM:SS`.
pub fn elapsed(secs: u64) -> String {
    let days = secs / 86_400;
    let h = (secs % 86_400) / 3_600;
    let m = (secs % 3_600) / 60;
    let s = secs % 60;
    if days > 0 {
        format!("{days}d {h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}")
    }
}

pub fn cpu_key(percent: f32) -> i64 {
    (percent as f64 * 10.0).round() as i64
}

pub fn mem_key(b: u64) -> u64 {
    (b as f64 / MIB * 10.0).round() as u64
}

pub fn rate_key(bytes_per_sec: u64) -> (u8, u64) {
    let b = bytes_per_sec as f64;
    if bytes_per_sec == 0 {
        (0, 0)
    } else if b < MIB {
        (1, (b / KIB * 10.0).round() as u64)
    } else {
        (2, (b / MIB * 10.0).round() as u64)
    }
}
