# Fast Task Manager

Windows-only task manager in Rust (eframe/egui 0.31, sysinfo 0.34, windows-sys 0.59).
Goals: idle under ~1% CPU and ~50 MB RAM; the UI never blocks on polling.

## Build & environment gotchas

- `cargo` is in `%USERPROFILE%\.cargo\bin` but **not on the shell PATH**. Prefix commands:
  `$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"; cargo build`
- **Smart App Control is on** (`HKLM:\SYSTEM\CurrentControlSet\Control\CI\Policy` →
  `VerifiedAndReputablePolicyState = 1`). It intermittently blocks freshly compiled
  unsigned executables with `os error 4551` (build scripts in `--release`, test
  binaries, sometimes the app). Do **not** disable it (Windows can't turn it back on
  without a reset); report the block instead. The debug build has generally been allowed to run.
- Release profile: `lto = "fat"`, `opt-level = 3`, `codegen-units = 1`, `panic = "abort"`, `strip = true`.
  Release builds hide the console (`windows_subsystem = "windows"`).
- sysinfo is built with `default-features = false, features = ["system"]` (no rayon pool).
  0.34 is the minimum: `Process::accumulated_cpu_time` doesn't exist in 0.33.
- Checks: `cargo clippy --all-targets` (currently clean), `cargo test` (unit tests in `src/view.rs`, `src/treemap.rs`).
  `.github/workflows/ci.yml` runs both on `windows-latest` (clippy with `-D warnings`),
  since Smart App Control often blocks the test binary locally.

## Architecture (data flows top to bottom)

- `src/source/` — `ProcessSource` trait. Backends report **raw cumulative counters**
  (CPU time, total bytes read/written), not rates, into a reused `RawSample` buffer.
  `SysinfoSource` is the only backend; the plan is a `NtQuerySystemInformation`
  backend later. Exception: system CPU % / per-core % come pre-computed.
- `src/source/gpu.rs` — GPU usage via PDH performance counters (`GPU Engine`,
  `GPU Process Memory`, `GPU Adapter Memory`; English names via
  `PdhAddEnglishCounterW`), owned by `SysinfoSource` and joined by PID. Also
  pre-computed: PDH formats engine `Utilization Percentage` over the time between
  collects. Per-process GPU % = busiest engine; system GPU % = busiest engine summed
  over processes (Task Manager's definition). Memory is dedicated/shared bytes.
  If the counters can't be opened, `SystemTotals::gpu` is `None` and GPU cells stay blank.
- `src/sampler.rs` — background thread. Computes per-process CPU % (normalized to
  logical cores: `Δcpu / (Δwall × cores) × 100`) and disk B/s from deltas, keyed by
  (PID, start_time) so a reused PID never produces a bogus delta. Publishes
  `Arc<Snapshot>` behind a `Mutex` (lock held only for the swap). Control channel:
  `SetInterval`, `RefreshNow` (via cloneable `Refresher`), `Stop`.
- `src/model.rs` — immutable `Snapshot`, `ProcessInfo`, `SystemTotals`.
- `src/view.rs` — snapshot → table rows. Modes: Apps (grouped by lowercase exe path,
  falling back to `name:<name>`), Tree (parent/child), Processes (flat). Filtering,
  sorting, and `ViewState` (persists across snapshots).
- `src/actions.rs` — end process/group/tree, set priority, open file location
  (`explorer /select,"..."` via `raw_arg`). Each request runs on its own thread and
  returns an `Outcome` shown as a toast.
- `src/treemap.rs` — live treemap (resizable panel above the table, toggle in the
  controls row). Own squarified layout over app groups (`ViewState::app_groups`,
  filter-aware, any mode); size/color metric selectable (Memory/CPU/Disk I/O); color
  is a fixed log scale (not relative to the max). Tiles below `MIN_TILE_AREA` px² go
  into "Other" (with hysteresis). Layout order is stable: previous order, and a tile
  only overtakes its predecessor when `REORDER_RATIO`× bigger. Double-click a group
  drills into its processes (breadcrumb / Backspace / mouse-back to return). Clicking
  a tile selects the `RowId` and scrolls the table to it.
- `src/app.rs` — egui UI: totals panel, mode buttons + filter, treemap, table, context menus, toasts.
- `src/format.rs` — display formatting **and** the matching quantized sort keys.

## Invariants — keep these when changing code

- **Repaint only on demand.** The UI repaints on input, on `request_repaint()` from
  the sampler after a publish, after an action completes, via `request_repaint_after`
  while a toast is visible, or while a treemap transition runs (≤300 ms after a layout
  change; skipped when no tile moves). Never add continuous repaints/animations.
  Verified before the treemap: idle = 2 frames per snapshot (egui always adds one
  follow-up frame); not yet re-measured with it. To debug, log
  `ctx.repaint_causes()` per frame.
- **The view (and the treemap's groups) is rebuilt only** when `snapshot.seq` changes or `view_dirty` is set
  (sort/mode/filter/expand changes). Row indices in `View` are valid only for the
  snapshot they were built from — always rebuild before drawing after swapping snapshots.
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

- Release-mode CPU/RAM against the targets has **not been measured** (release build
  blocked by Smart App Control). Debug build measured ~160 MB working set / ~90 MB
  private; much is likely debug + OpenGL driver. If release is still over 50 MB,
  try the wgpu/DX12 renderer or trimming eframe features.
- Disk columns come from Windows I/O counters, which include non-disk I/O.
- GPU tracking was written and cross-checked (`--target x86_64-pc-windows-msvc`) without
  running on Windows; verify values against Task Manager. Total VRAM isn't shown (needs DXGI).
  PDH wildcard enumeration of `GPU Engine` can be slow with many processes — watch "sample took".
- Tree rows show each process's own values, not subtree totals.
- No confirmation before ending processes (matches Task Manager).
- Next planned step: a `NtQuerySystemInformation` backend implementing `ProcessSource`.
