//! Optional profiling, compiled in with `--features puffin`.
//!
//! - Every `profile_scope!` (and egui's and eframe's own scopes) goes to
//!   puffin. Connect `puffin_viewer` to `127.0.0.1:8585` to browse frames.
//! - `FTM_PROFILE_LOG=<file>` also writes a plain-text summary: frames,
//!   time and heap allocations per `profile_scope!` section, and eframe's
//!   main-thread breakdown. Collection starts `FTM_PROFILE_SKIP` seconds
//!   (default 15) after launch, so startup doesn't skew idle numbers. The
//!   file is rewritten every few seconds and on exit.
//!
//! Without the feature, `profile_scope!` expands to nothing.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use puffin::{FrameData, ScopeCollection};

const ADDR: &str = "127.0.0.1:8585";
const WRITE_EVERY: Duration = Duration::from_secs(5);

/// Counts heap allocations (including reallocations), per process and per
/// thread, so a section can report how many it made, and the bytes in use.
struct CountingAlloc;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static HEAP_BYTES: AtomicI64 = AtomicI64::new(0);

thread_local! {
    // Const-initialized with no destructor: safe to touch from the allocator.
    static THREAD_ALLOCS: Cell<u64> = const { Cell::new(0) };
}

fn count_alloc() {
    ALLOCS.fetch_add(1, Relaxed);
    let _ = THREAD_ALLOCS.try_with(|c| c.set(c.get() + 1));
}

fn thread_allocs() -> u64 {
    THREAD_ALLOCS.try_with(Cell::get).unwrap_or(0)
}

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count_alloc();
        HEAP_BYTES.fetch_add(layout.size() as i64, Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count_alloc();
        HEAP_BYTES.fetch_add(layout.size() as i64, Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count_alloc();
        HEAP_BYTES.fetch_add(new_size as i64 - layout.size() as i64, Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        HEAP_BYTES.fetch_sub(layout.size() as i64, Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

#[derive(Default)]
struct Stat {
    calls: u64,
    nanos: u64,
    allocs: u64,
}

impl Stat {
    fn add(&mut self, nanos: u64, allocs: u64) {
        self.calls += 1;
        self.nanos += nanos;
        self.allocs += allocs;
    }
}

/// What has been collected since `since`.
struct Stats {
    since: Instant,
    allocs_at_start: u64,
    frames: u64,
    /// Rust heap bytes in use at the last frame (the app's working state;
    /// at exit most of it is already freed).
    heap_bytes: i64,
    sections: BTreeMap<&'static str, Stat>,
    /// Main-thread puffin scopes by path ("parent / child").
    eframe: BTreeMap<String, Stat>,
}

static STATS: Mutex<Option<Stats>> = Mutex::new(None);

fn stats() -> MutexGuard<'static, Option<Stats>> {
    STATS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Times and counts the allocations of the rest of a block; made by
/// `profile_scope!`.
pub struct Section {
    name: &'static str,
    start: Instant,
    allocs: u64,
}

impl Section {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            allocs: thread_allocs(),
            start: Instant::now(),
        }
    }
}

impl Drop for Section {
    fn drop(&mut self) {
        let nanos = self.start.elapsed().as_nanos() as u64;
        let allocs = thread_allocs() - self.allocs;
        if let Some(s) = stats().as_mut() {
            s.sections.entry(self.name).or_default().add(nanos, allocs);
        }
    }
}

/// Keeps the profiler running; dropping it writes the final summary.
pub struct Profiler {
    _server: Option<puffin_http::Server>,
    log: Option<PathBuf>,
}

impl Drop for Profiler {
    fn drop(&mut self) {
        if let Some(path) = &self.log {
            write_report(path);
        }
    }
}

pub fn start() -> Profiler {
    puffin::set_scopes_on(true);
    let server = match puffin_http::Server::new(ADDR) {
        Ok(server) => {
            eprintln!("puffin: serving on {ADDR} (connect puffin_viewer)");
            Some(server)
        }
        Err(e) => {
            eprintln!("puffin: can't serve on {ADDR}: {e}");
            None
        }
    };

    let log = std::env::var_os("FTM_PROFILE_LOG").map(PathBuf::from);
    if let Some(path) = log.clone() {
        let skip = std::env::var("FTM_PROFILE_SKIP")
            .ok()
            .and_then(|s| s.parse().ok())
            .map_or(Duration::from_secs(15), Duration::from_secs_f64);
        let launched = Instant::now();
        // The sink is `Fn`; it is only ever called from one thread at a time.
        let state = Mutex::new((ScopeCollection::default(), Instant::now()));
        puffin::GlobalProfiler::lock().add_sink(Box::new(move |frame| {
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            let (scopes, written) = &mut *state;
            for details in &frame.scope_delta {
                scopes.insert(Arc::clone(details));
            }
            let mut guard = stats();
            let Some(s) = guard.as_mut() else {
                if launched.elapsed() >= skip {
                    *guard = Some(Stats {
                        since: Instant::now(),
                        allocs_at_start: ALLOCS.load(Relaxed),
                        frames: 0,
                        heap_bytes: 0,
                        sections: BTreeMap::new(),
                        eframe: BTreeMap::new(),
                    });
                }
                return;
            };
            s.frames += 1;
            s.heap_bytes = HEAP_BYTES.load(Relaxed);
            record_main_thread(&frame, scopes, &mut s.eframe);
            drop(guard);
            if written.elapsed() >= WRITE_EVERY {
                *written = Instant::now();
                write_report(&path);
            }
        }));
    }

    Profiler { _server: server, log }
}

/// Main-thread scopes this deep are summed by their path of names.
const EFRAME_DEPTH: usize = 4;

/// Adds the main thread's scopes in `frame` to `out`, keyed by path.
fn record_main_thread(frame: &FrameData, scopes: &ScopeCollection, out: &mut BTreeMap<String, Stat>) {
    let Ok(unpacked) = frame.unpacked() else {
        return;
    };
    for (thread, stream) in &unpacked.thread_streams {
        if thread.name == "main" {
            record_children(&stream.stream, 0, "", 0, scopes, out);
        }
    }
}

fn record_children(
    stream: &puffin::Stream,
    offset: u64,
    parent: &str,
    depth: usize,
    scopes: &ScopeCollection,
    out: &mut BTreeMap<String, Stat>,
) {
    let Ok(reader) = puffin::Reader::with_offset(stream, offset) else {
        return;
    };
    // The reader stops at the end of the parent.
    for scope in reader.flatten() {
        let name = scopes.fetch_by_id(&scope.id).map_or("?", |d| d.name().as_ref());
        // Drop eframe's long generic prefixes.
        let name = name.rsplit("::").next().unwrap_or(name);
        let path = if parent.is_empty() { name.to_owned() } else { format!("{parent} / {name}") };
        out.entry(path.clone()).or_default().add(scope.record.duration_ns as u64, 0);
        if depth + 1 < EFRAME_DEPTH {
            record_children(stream, scope.child_begin_position, &path, depth + 1, scopes, out);
        }
    }
}

fn write_report(path: &PathBuf) {
    let guard = stats();
    let Some(s) = guard.as_ref() else {
        return;
    };
    let secs = s.since.elapsed().as_secs_f64().max(1e-9);
    let frames = s.frames.max(1) as f64;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "window {secs:.1} s, {} frames ({:.2}/s), {:.0} allocations/s ({:.0}/frame, all threads)\n",
        s.frames,
        s.frames as f64 / secs,
        (ALLOCS.load(Relaxed) - s.allocs_at_start) as f64 / secs,
        (ALLOCS.load(Relaxed) - s.allocs_at_start) as f64 / frames,
    );
    let _ = writeln!(
        out,
        "Rust heap in use at the last frame: {:.1} MB (excludes the GPU driver and other native allocations)
",
        s.heap_bytes as f64 / (1024.0 * 1024.0)
    );
    let _ = writeln!(
        out,
        "{:<64} {:>8} {:>8} {:>10} {:>10} {:>12}",
        "section", "calls", "calls/s", "avg us", "ms/s", "allocs/call"
    );
    table(&mut out, secs, s.sections.iter().map(|(k, v)| (*k, v)));
    let _ = writeln!(out, "\neframe main thread (puffin, {EFRAME_DEPTH} levels)");
    table(&mut out, secs, s.eframe.iter().map(|(k, v)| (k.as_str(), v)));
    drop(guard);
    let _ = std::fs::write(path, out);
}

/// One line per row, most total time first.
fn table<'a>(out: &mut String, secs: f64, rows: impl Iterator<Item = (&'a str, &'a Stat)>) {
    let mut rows: Vec<_> = rows.collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.1.nanos));
    for (name, st) in rows {
        let calls = st.calls.max(1) as f64;
        let _ = writeln!(
            out,
            "{name:<64} {:>8} {:>8.2} {:>10.1} {:>10.2} {:>12.1}",
            st.calls,
            st.calls as f64 / secs,
            st.nanos as f64 / calls / 1e3,
            st.nanos as f64 / secs / 1e6,
            st.allocs as f64 / calls,
        );
    }
}
