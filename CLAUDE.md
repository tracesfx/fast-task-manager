# Fast Task Manager

Windows-only task manager in Rust (eframe/egui 0.31, egui_dock 0.16, windows-sys 0.59;
sysinfo 0.34 only behind feature `sysinfo`).
Goals: idle under ~1% CPU and ~50 MB RAM; the UI never blocks on polling.

## Build & environment gotchas

- `cargo` is in `%USERPROFILE%\.cargo\bin` but **not on the shell PATH**. Prefix commands:
  `$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"; cargo build`
- **Smart App Control is on** (`HKLM:\SYSTEM\CurrentControlSet\Control\CI\Policy` →
  `VerifiedAndReputablePolicyState = 1`). It intermittently blocks freshly compiled
  unsigned executables with `os error 4551` (build scripts in `--release`, test
  binaries, sometimes the app). Do **not** disable it (Windows can't turn it back on
  without a reset); report the block instead. The debug build has generally been allowed to run.
  Plain `cargo build --release` fails locally: SAC blocks rustc from loading the release
  `windows-interface` proc-macro DLL (pulled in by eframe's accesskit, not sysinfo);
  Code Integrity events 3033/3077 show it. Do **not** work around a block by relinking
  or tweaking build settings until a binary gets a different verdict — that is
  circumventing the policy. Measure release builds on CI or ask the user.
- Release profile: `lto = "fat"`, `opt-level = 3`, `codegen-units = 1`, `panic = "abort"`, `strip = true`.
  Release builds hide the console (`windows_subsystem = "windows"`).
- Features: `puffin` (profiler, see `src/profiler.rs`), `sysinfo` (old backend for
  comparison; `FTM_SOURCE=sysinfo` selects it at runtime). sysinfo is built with
  `default-features = false, features = ["system"]`; 0.34 is the minimum.
- Profiling: `cargo run --release --features puffin` and connect `puffin_viewer` to
  127.0.0.1:8585, or set `FTM_PROFILE_LOG=<file>` for a text summary (frames/s, time and
  allocations per `profile_scope!` section, eframe's main-thread breakdown, Rust heap in
  use). Collection starts after `FTM_PROFILE_SKIP` s (default 15). `profile_scope!` is
  a no-op without the feature.
- Running the app for measurements rewrites `%APPDATA%\Fast Task Manager\data\app.ron`;
  a second instance can't register the hotkey and saves `hotkey:false`. Back it up first.
- Checks: `cargo clippy --all-targets` and `--all-features` (both clean), `cargo test --features sysinfo`
  (unit tests in `src/view.rs`, `src/treemap.rs`, `src/layout.rs`, `src/settings.rs`, `src/app.rs`
  (save/load round trip), `src/graphs.rs`, `src/history.rs`, `src/source/` (NT layout, self-sample,
  NT-vs-sysinfo agreement)). Backend benchmark:
  `cargo test --features sysinfo bench_backends -- --ignored --nocapture`.
  `.github/workflows/ci.yml` runs clippy (both configs, `-D warnings`) and the tests on
  `windows-latest`, since Smart App Control often blocks the test binary locally.

## Architecture (data flows top to bottom)

- `src/source/` — `ProcessSource` trait. Backends report **raw cumulative counters**
  (CPU time, total bytes read/written), not rates, into a reused `RawSample` buffer.
  Exception: system CPU % / per-core % come pre-computed. `NtSource` (default): one
  `NtQuerySystemInformation(SystemProcessInformation)` per sample into a reused buffer —
  no handles, so protected processes have real CPU/memory/start times (sysinfo can't
  open ~30% of processes and reported 0 for them). Fields windows-sys leaves `Reserved*`
  are read at phnt offsets (asserted in tests). Name and exe path (`QueryFullProcessImageNameW`,
  limited-query handle, creation time re-checked) are looked up once per process and shared
  as `Arc<str>`/`Arc<Path>`, keyed by (PID, creation time). PID 0 reports no CPU time.
  Per-core times come from `SystemProcessorPerformanceInformation` (one processor group,
  ≤ 64 cores). Debug benchmark: NT ~4.4 ms/sample vs sysinfo 24–62 ms (~455 processes).
- `src/source/gpu.rs` — `WithGpu<S>` wraps either backend and fills GPU fields by PID
  from PDH performance counters (`GPU Engine`, `GPU Process Memory`, `GPU Adapter
  Memory`; English names via `PdhAddEnglishCounterW`). Pre-computed like system CPU %:
  PDH formats engine `Utilization Percentage` over the time between collects.
  Per-process GPU % = busiest engine; system GPU % = busiest engine summed over
  processes (Task Manager's definition). Memory is dedicated/shared bytes. If the
  counters can't be opened, `SystemTotals::gpu` is `None`: GPU cells stay blank and
  the GPU graphs are hidden.
- `src/sampler.rs` — background thread. Computes per-process CPU % (normalized to
  logical cores: `Δcpu / (Δwall × cores) × 100`) and disk B/s from deltas, keyed by
  (PID, start_time) so a reused PID never produces a bogus delta. Computes each process's
  `group_key` once (kept with the previous counters) and shares it as `Arc<str>`. Publishes
  `Arc<Snapshot>` behind a `Mutex` (lock held only for the swap). Control channel:
  `SetInterval`, `RefreshNow` (via cloneable `Refresher`), `Stop`.
- `src/history.rs` — ring buffers of the last 5 min (`WINDOW`): system totals
  (CPU, used RAM, summed disk R/W, top-5 apps by CPU per sample) and CPU +
  working set per app group (same `view::group_key`). Recorded **on the sampler
  thread** (so nothing is lost while minimized): `Sample::of` aggregates outside
  the lock, `push` appends under it. Group series are indexed by sample number
  (timestamps live only in the system series); gaps are zero-filled. Bounded by
  time; groups unseen for `EXITED_KEEP` (60 s) are dropped. Shared as
  `Arc<Mutex<History>>`; the UI locks it once per frame.
- `src/model.rs` — immutable `Snapshot`, `ProcessInfo`, `SystemTotals`. `ProcessInfo`'s
  `name`, `exe` and `group_key` are `Arc`s shared across snapshots; group keys
  (`RowId::Group`, `Group::key`, history, treemap) are `Arc<str>`, so cloning is cheap.
- `src/profiler.rs` — feature `puffin` only: puffin_http server, a counting global
  allocator, `Section` (behind `profile_scope!`) and the summary log.
- `src/view.rs` — snapshot → table rows. Modes: Apps (grouped by lowercase exe path,
  falling back to `name:<name>`), Tree (parent/child), Processes (flat). Filtering,
  sorting, and `ViewState` (persists across snapshots).
- `src/actions.rs` — end process/group/tree, set priority, open file location
  (`explorer /select,"..."` via `raw_arg`). Each request runs on its own thread and
  returns an `Outcome` shown as a toast.
- `src/treemap.rs` — live treemap (a dock pane). Own squarified layout over app groups (`ViewState::app_groups`,
  filter-aware, any mode); size/color metric selectable (Memory/CPU/Disk I/O/GPU/GPU memory); color
  is a fixed log scale (not relative to the max) through a configurable gradient. Tiles below `MIN_TILE_AREA` px² go
  into "Other" (with hysteresis). Layout order is stable: previous order, and a tile
  only overtakes its predecessor when `REORDER_RATIO`× bigger. A transition runs only
  when a tile enters/leaves or a corner moves ≥ `MIN_MOVE` (3 px); smaller moves and
  color-only changes snap. Label galleys are cached per tile (invalidated by width,
  color, or a new egui font atlas, tracked by `Weak` pointer). Double-click a group
  drills into its processes (breadcrumb / Backspace / mouse-back to return). Clicking
  a tile selects the `RowId` and scrolls the table to it.
- `src/graphs.rs` — egui_plot graphs (0.32 = the egui 0.31 release): small
  CPU/RAM/disk plots in the top bar (hovering CPU lists that moment's top apps),
  full-size ones plus per-core load in the Performance pane,
  a hand-painted CPU sparkline per app row (Apps mode only, own column), and the
  detail panel's CPU/memory plots. x = seconds relative to the newest sample, so
  graphs change only on a snapshot; interactions (drag/zoom) are disabled.
- `src/layout.rs` — the dock panes (`Pane`: Processes, Treemap, Performance,
  App details, Top apps/mini) and layout presets (Default/Visual/Detailed/Compact).
  Each pane is in the dock at most once; View menu closes/reopens them.
- `src/settings.rs` — `Settings` (interval, theme/accent/text size, UI scale = egui
  zoom, treemap gradient + default metrics, column order/visibility (columns added in a
  newer version are inserted after their predecessor in `TableColumn::ALL`), totals bar,
  always-on-top, hotkey) and the Settings window. `normalize()` repairs loaded values.
- `src/hotkey.rs` — global Ctrl+Alt+T show/hide: `RegisterHotKey` on its own thread
  with a `GetMessageW` loop; shows/hides via Win32 `ShowWindow` (a window hidden
  through eframe stops running `update`, so it couldn't unhide itself).
- `src/app.rs` — egui UI: menu bar, totals bar, then an `egui_dock` `DockArea`
  (`Panes` is the `TabViewer`; actions are collected and applied after the dock).
  Clicking an app shows it in App details if that pane is open; "App details" in the
  context menu reopens it; Esc / ✖ clears it. Context menus, toasts.
  Persistence (eframe `persistence` feature, RON in `%APPDATA%\Fast Task Manager\data\app.ron`):
  `Persisted` (settings, view mode, sort, compact restore size) under `APP_KEY`, the
  dock under `DOCK_KEY` separately so an unloadable layout doesn't lose settings.
  Settings are pushed to egui/sampler/window/treemap only when they differ from
  `Applied` (what was last applied), never per frame. The Compact preset shrinks the
  window and sets always-on-top; other presets restore the previous size.
- `src/format.rs` — display formatting **and** the matching quantized sort keys.

## Invariants — keep these when changing code

- **Repaint only on demand.** The UI repaints on input, on `request_repaint()` from
  the sampler after a publish, after an action completes, via `request_repaint_after`
  while a toast is visible, or while a treemap transition runs (≤300 ms after a layout
  change; skipped when no tile moves). Never add continuous repaints/animations.
  Idle = 2 frames per snapshot (egui always adds one follow-up frame), plus a treemap
  transition (~18 frames) only when something visibly moves. Measured with the Default
  layout at 1 s: 21.4 frames/s before the `MIN_MOVE` rule, ~3.2–3.7/s after. Frames are
  what costs: back-to-back frames made Intel's GL driver stall ~12 ms per frame in
  `make_not_current` (vsync wait); isolated frames cost ~0.7 ms there. To debug, use the
  `puffin` summary or log `ctx.repaint_causes()` per frame.
- **The view (and the treemap's groups) is rebuilt only** when `snapshot.seq` changes or `view_dirty` is set
  (sort/mode/filter/expand changes). Row indices in `View` are valid only for the
  snapshot they were built from — always rebuild before drawing after swapping snapshots.
- **No per-frame work that scales with the process count or the row count.** The table
  is virtualized (`body.rows`, ~14 rows laid out per frame); numeric cells are painted
  without a child `Ui`; never recompute group keys or scan every process per frame.
- **UI state is keyed by identity, never by row index:** `RowId::Group(key)` or
  `RowId::Proc(ProcKey { pid, start_time })`. Apps groups are collapsed unless in
  `expanded_groups`; tree nodes are expanded unless in `collapsed_procs` (pruned
  when processes die). While filtering, tree paths to matches are forced open and
  non-matching ancestors are shown dimmed (`context_only`).
- **Stable ordering:** sort keys are quantized to the displayed precision
  (`format::*_key`), ties fall back to the previous build's rank (`Ranks`), then PID.
  If you change a display format, change its `_key` function to match.
- **Tree parent validity:** a parent must have `start_time <= child.start_time`
  (otherwise the parent PID was reused). Nodes unreachable from roots (parent cycles)
  are promoted to roots so nothing disappears.
- **Action safety:** before acting, `actions::open` checks the live process's creation
  time against the snapshot `start_time` (±1 s) to avoid hitting a reused PID.
  Critical processes (PID 0/4, csrss, lsass, wininit, etc.) are refused outright.
  Realtime priority is intentionally not offered. Access denied → toast suggesting admin.

## Status / known gaps

- Measured (Default layout, 1 s interval, Intel Iris Xe, 8 threads; % of one core):
  before the 2026-10 performance pass, release idle 38% CPU, 157 MB working set /
  91 MB private, 48 ms to first frame; debug idle 43%. After, debug idle 5.7–13% (noisy,
  four runs), same memory, ~220 ms debug startup. Release after: not yet measured (see
  the Smart App Control note).
- RAM is not ours: the Rust heap is ~6 MB of ~88 MB private bytes; the rest is the GL
  driver and other native allocations. The 50 MB goal needs a different renderer. A
  quick wgpu try (eframe feature `wgpu`) picked Vulkan and panicked in egui-wgpu
  ("Buffer slices can not be empty"); forcing DX12 and finding that is the next step.
- AccessKit is active on this machine (some UI Automation client), costing ~0.3 ms per
  frame plus node building.
- Disk columns come from Windows I/O counters, which include non-disk I/O.
- GPU tracking (v0.2.0) was cross-checked with clippy for `x86_64-pc-windows-msvc` and CI,
  but not compared against Task Manager yet. Total VRAM isn't shown (needs DXGI). PDH
  wildcard enumeration of `GPU Engine` can be slow with many processes — watch "sample
  took" (the `gpu counters` profile scope).
- Releases: push a `vX.Y.Z` tag matching `Cargo.toml`; `.github/workflows/release.yml`
  builds the Inno Setup installer. Keep the `AppId` in `installer/fast-task-manager.iss`
  unchanged so a new setup upgrades the existing install in place.
- Tree rows show each process's own values, not subtree totals.
- No confirmation before ending processes (matches Task Manager).
