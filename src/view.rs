//! Turns a snapshot into the rows shown in the table: grouping into apps,
//! parent/child tree layout, filtering and sorting.
//!
//! Rebuilt only when a new snapshot arrives or a view setting changes. All
//! UI state (expanded groups, collapsed tree nodes, selection) is keyed by
//! group key or `ProcKey`, never by row index, so it survives rebuilds.
//!
//! Ordering is stable: rows compare on the sort column at its *displayed*
//! precision, and ties keep the order they had in the previous build.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::format;
use crate::model::{ProcessInfo, Snapshot};

/// Identifies one process across snapshots; the start time guards against
/// PID reuse.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProcKey {
    pub pid: u32,
    pub start_time: u64,
}

impl ProcKey {
    pub fn of(p: &ProcessInfo) -> Self {
        Self {
            pid: p.pid,
            start_time: p.start_time,
        }
    }
}

/// Stable identity of a table row.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RowId {
    Group(Arc<str>),
    Proc(ProcKey),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ViewMode {
    /// Processes grouped by executable.
    Apps,
    /// Parent/child process tree.
    Tree,
    /// Flat list of all processes.
    Processes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortColumn {
    Pid,
    ParentPid,
    Name,
    Cpu,
    Gpu,
    WorkingSet,
    Private,
    GpuMemory,
    DiskRead,
    DiskWrite,
    Started,
    Path,
}

impl SortColumn {
    /// Numeric "how much" columns sort biggest-first on first click.
    pub fn default_descending(self) -> bool {
        matches!(
            self,
            Self::Cpu
                | Self::Gpu
                | Self::WorkingSet
                | Self::Private
                | Self::GpuMemory
                | Self::DiskRead
                | Self::DiskWrite
        )
    }

    fn compare(self, a: &Metrics, b: &Metrics) -> Ordering {
        match self {
            Self::Pid => a.pid.cmp(&b.pid),
            Self::ParentPid => a.parent_pid.cmp(&b.parent_pid),
            Self::Name => cmp_ignore_ascii_case(a.name, b.name),
            Self::Cpu => format::cpu_key(a.cpu_percent).cmp(&format::cpu_key(b.cpu_percent)),
            Self::Gpu => format::cpu_key(a.gpu_percent).cmp(&format::cpu_key(b.gpu_percent)),
            Self::WorkingSet => format::mem_key(a.working_set).cmp(&format::mem_key(b.working_set)),
            Self::Private => {
                format::mem_key(a.private_bytes).cmp(&format::mem_key(b.private_bytes))
            }
            Self::GpuMemory => {
                format::mem_key(a.gpu_dedicated).cmp(&format::mem_key(b.gpu_dedicated))
            }
            Self::DiskRead => {
                format::rate_key(a.disk_read_per_sec).cmp(&format::rate_key(b.disk_read_per_sec))
            }
            Self::DiskWrite => format::rate_key(a.disk_write_per_sec)
                .cmp(&format::rate_key(b.disk_write_per_sec)),
            // The column shows "running for", so a later start sorts lower.
            Self::Started => b.start_time.cmp(&a.start_time),
            Self::Path => a.exe.cmp(&b.exe),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sort {
    pub column: SortColumn,
    pub descending: bool,
}

impl Sort {
    fn cmp(self, a: &Metrics, b: &Metrics) -> Ordering {
        let ord = self.column.compare(a, b);
        if self.descending {
            ord.reverse()
        } else {
            ord
        }
    }
}

/// The values shown in one row, for either a process or a group.
pub struct Metrics<'a> {
    pub pid: u32,
    pub parent_pid: Option<u32>,
    pub name: &'a str,
    pub exe: Option<&'a Path>,
    pub cpu_percent: f32,
    pub working_set: u64,
    pub private_bytes: u64,
    pub disk_read_per_sec: u64,
    pub disk_write_per_sec: u64,
    pub gpu_percent: f32,
    pub gpu_dedicated: u64,
    pub gpu_shared: u64,
    pub start_time: u64,
}

impl<'a> Metrics<'a> {
    pub fn of(p: &'a ProcessInfo) -> Self {
        Self {
            pid: p.pid,
            parent_pid: p.parent_pid,
            name: &p.name,
            exe: p.exe.as_deref(),
            cpu_percent: p.cpu_percent,
            working_set: p.working_set,
            private_bytes: p.private_bytes,
            disk_read_per_sec: p.disk_read_per_sec,
            disk_write_per_sec: p.disk_write_per_sec,
            gpu_percent: p.gpu_percent,
            gpu_dedicated: p.gpu_dedicated,
            gpu_shared: p.gpu_shared,
            start_time: p.start_time,
        }
    }
}

/// Processes sharing one executable.
pub struct Group {
    pub key: Arc<str>,
    /// Indices into `Snapshot::processes`, sorted. Only filter matches.
    pub members: Vec<usize>,
    min_pid: u32,
    cpu_percent: f32,
    working_set: u64,
    private_bytes: u64,
    disk_read_per_sec: u64,
    disk_write_per_sec: u64,
    gpu_percent: f32,
    gpu_dedicated: u64,
    gpu_shared: u64,
    /// Earliest known start time of any member (0 = unknown).
    start_time: u64,
}

impl Group {
    fn new(key: Arc<str>) -> Self {
        Self {
            key,
            members: Vec::new(),
            min_pid: u32::MAX,
            cpu_percent: 0.0,
            working_set: 0,
            private_bytes: 0,
            disk_read_per_sec: 0,
            disk_write_per_sec: 0,
            gpu_percent: 0.0,
            gpu_dedicated: 0,
            gpu_shared: 0,
            start_time: 0,
        }
    }

    fn add(&mut self, index: usize, p: &ProcessInfo) {
        self.members.push(index);
        self.min_pid = self.min_pid.min(p.pid);
        self.cpu_percent += p.cpu_percent;
        self.working_set += p.working_set;
        self.private_bytes += p.private_bytes;
        self.disk_read_per_sec += p.disk_read_per_sec;
        self.disk_write_per_sec += p.disk_write_per_sec;
        self.gpu_percent += p.gpu_percent;
        self.gpu_dedicated += p.gpu_dedicated;
        self.gpu_shared += p.gpu_shared;
        if p.start_time != 0 && (self.start_time == 0 || p.start_time < self.start_time) {
            self.start_time = p.start_time;
        }
    }

    /// Summed metrics; name and path come from the (identical) members.
    pub fn metrics<'a>(&'a self, procs: &'a [ProcessInfo]) -> Metrics<'a> {
        let first = &procs[self.members[0]];
        Metrics {
            pid: self.min_pid,
            parent_pid: None,
            name: &first.name,
            exe: first.exe.as_deref(),
            cpu_percent: self.cpu_percent.min(100.0),
            working_set: self.working_set,
            private_bytes: self.private_bytes,
            disk_read_per_sec: self.disk_read_per_sec,
            disk_write_per_sec: self.disk_write_per_sec,
            gpu_percent: self.gpu_percent.min(100.0),
            gpu_dedicated: self.gpu_dedicated,
            gpu_shared: self.gpu_shared,
            start_time: self.start_time,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// Index into `View::groups`.
    Group(usize),
    /// Index into `Snapshot::processes`.
    Process(usize),
}

#[derive(Clone, Copy, Debug)]
pub struct Row {
    pub kind: RowKind,
    pub depth: u16,
    /// `Some(expanded)` if the row has children it can show or hide.
    pub expander: Option<bool>,
    /// Shown only as the ancestor of a filter match (tree mode).
    pub context_only: bool,
}

/// Everything the table needs for one snapshot.
#[derive(Default)]
pub struct View {
    pub groups: Vec<Group>,
    pub rows: Vec<Row>,
    /// Tree mode only: children of each process, by snapshot index.
    pub children: Vec<Vec<usize>>,
}

impl View {
    pub fn row_id(&self, row: &Row, procs: &[ProcessInfo]) -> RowId {
        match row.kind {
            RowKind::Group(g) => RowId::Group(Arc::clone(&self.groups[g].key)),
            RowKind::Process(i) => RowId::Proc(ProcKey::of(&procs[i])),
        }
    }

    pub fn find_group(&self, key: &str) -> Option<&Group> {
        self.groups.iter().find(|g| &*g.key == key)
    }

    /// `root` followed by all its descendants (tree mode), parents first.
    pub fn subtree(&self, root: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut stack = vec![root];
        while let Some(i) = stack.pop() {
            if !seen.insert(i) {
                continue;
            }
            out.push(i);
            if let Some(children) = self.children.get(i) {
                stack.extend(children.iter().rev());
            }
        }
        out
    }
}

/// Display order from the previous build, used to break ties.
#[derive(Default)]
struct Ranks {
    groups: HashMap<Arc<str>, u32>,
    procs: HashMap<ProcKey, u32>,
}

struct RankBuilder<'a> {
    old: &'a Ranks,
    new: Ranks,
    next: u32,
}

impl<'a> RankBuilder<'a> {
    fn new(old: &'a Ranks) -> Self {
        Self {
            old,
            new: Ranks {
                groups: HashMap::with_capacity(old.groups.len()),
                procs: HashMap::with_capacity(old.procs.len()),
            },
            next: 0,
        }
    }

    fn old_proc(&self, p: &ProcessInfo) -> u32 {
        self.old.procs.get(&ProcKey::of(p)).copied().unwrap_or(u32::MAX)
    }

    fn old_group(&self, key: &str) -> u32 {
        self.old.groups.get(key).copied().unwrap_or(u32::MAX)
    }

    fn record_proc(&mut self, p: &ProcessInfo) {
        self.new.procs.insert(ProcKey::of(p), self.next);
        self.next += 1;
    }

    fn record_group(&mut self, key: &Arc<str>) {
        self.new.groups.insert(Arc::clone(key), self.next);
        self.next += 1;
    }

    fn sort_procs(&self, indices: &mut [usize], procs: &[ProcessInfo], sort: Sort) {
        indices.sort_by(|&a, &b| {
            let (pa, pb) = (&procs[a], &procs[b]);
            sort.cmp(&Metrics::of(pa), &Metrics::of(pb))
                .then_with(|| self.old_proc(pa).cmp(&self.old_proc(pb)))
                .then(pa.pid.cmp(&pb.pid))
        });
    }
}

/// View settings and per-row UI state that persist across snapshots.
pub struct ViewState {
    pub mode: ViewMode,
    pub sort: Sort,
    pub filter: String,
    /// Apps mode: groups are collapsed unless listed here.
    pub expanded_groups: HashSet<Arc<str>>,
    /// Tree mode: nodes are expanded unless listed here.
    pub collapsed_procs: HashSet<ProcKey>,
    ranks: Ranks,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            mode: ViewMode::Apps,
            sort: Sort {
                column: SortColumn::Cpu,
                descending: true,
            },
            filter: String::new(),
            expanded_groups: HashSet::new(),
            collapsed_procs: HashSet::new(),
            ranks: Ranks::default(),
        }
    }
}

impl ViewState {
    pub fn toggle(&mut self, id: &RowId) {
        match id {
            RowId::Group(key) => {
                if !self.expanded_groups.remove(key) {
                    self.expanded_groups.insert(Arc::clone(key));
                }
            }
            RowId::Proc(key) => {
                if !self.collapsed_procs.remove(key) {
                    self.collapsed_procs.insert(*key);
                }
            }
        }
    }

    /// App groups of the processes matching the filter, whatever the mode
    /// (for the treemap). Members are in snapshot order.
    pub fn app_groups(&self, snap: &Snapshot) -> Vec<Group> {
        group_apps(&snap.processes, &self.matches(snap))
    }

    fn matches(&self, snap: &Snapshot) -> Vec<bool> {
        let filter = Filter::new(&self.filter);
        snap.processes.iter().map(|p| filter.matches(p)).collect()
    }

    pub fn build(&mut self, snap: &Snapshot) -> View {
        let procs = &snap.processes;
        let filter = Filter::new(&self.filter);
        let matches = self.matches(snap);

        let mut ranks = RankBuilder::new(&self.ranks);
        let view = match self.mode {
            ViewMode::Apps => build_apps(procs, &matches, self.sort, &self.expanded_groups, &mut ranks),
            ViewMode::Tree => build_tree(
                procs,
                &matches,
                filter.is_active(),
                self.sort,
                &self.collapsed_procs,
                &mut ranks,
            ),
            ViewMode::Processes => build_flat(procs, &matches, self.sort, &mut ranks),
        };
        self.ranks = ranks.new;

        // Forget collapsed state of processes that no longer exist.
        if !self.collapsed_procs.is_empty() {
            let alive: HashSet<ProcKey> = procs.iter().map(ProcKey::of).collect();
            self.collapsed_procs.retain(|k| alive.contains(k));
        }
        view
    }
}

/// Groups the matching processes by executable, in first-seen order.
fn group_apps(procs: &[ProcessInfo], matches: &[bool]) -> Vec<Group> {
    let mut index: HashMap<&str, usize> = HashMap::new();
    let mut groups: Vec<Group> = Vec::new();
    for (i, p) in procs.iter().enumerate() {
        if !matches[i] {
            continue;
        }
        let g = *index.entry(&p.group_key).or_insert_with(|| {
            groups.push(Group::new(Arc::clone(&p.group_key)));
            groups.len() - 1
        });
        groups[g].add(i, p);
    }
    groups
}

fn build_apps(
    procs: &[ProcessInfo],
    matches: &[bool],
    sort: Sort,
    expanded: &HashSet<Arc<str>>,
    ranks: &mut RankBuilder,
) -> View {
    let mut groups = group_apps(procs, matches);
    for group in &mut groups {
        ranks.sort_procs(&mut group.members, procs, sort);
    }

    let mut order: Vec<usize> = (0..groups.len()).collect();
    order.sort_by(|&a, &b| {
        let (ga, gb) = (&groups[a], &groups[b]);
        sort.cmp(&ga.metrics(procs), &gb.metrics(procs))
            .then_with(|| ranks.old_group(&ga.key).cmp(&ranks.old_group(&gb.key)))
            .then(ga.min_pid.cmp(&gb.min_pid))
    });

    let mut rows = Vec::with_capacity(order.len());
    for g in order {
        let group = &groups[g];
        ranks.record_group(&group.key);
        let expandable = group.members.len() > 1;
        let is_expanded = expandable && expanded.contains(&group.key);
        rows.push(Row {
            kind: RowKind::Group(g),
            depth: 0,
            expander: expandable.then_some(is_expanded),
            context_only: false,
        });
        for &i in &group.members {
            ranks.record_proc(&procs[i]);
            if is_expanded {
                rows.push(Row {
                    kind: RowKind::Process(i),
                    depth: 1,
                    expander: None,
                    context_only: false,
                });
            }
        }
    }

    View {
        groups,
        rows,
        children: Vec::new(),
    }
}

fn build_flat(procs: &[ProcessInfo], matches: &[bool], sort: Sort, ranks: &mut RankBuilder) -> View {
    let mut order: Vec<usize> = (0..procs.len()).filter(|&i| matches[i]).collect();
    ranks.sort_procs(&mut order, procs, sort);
    let rows = order
        .into_iter()
        .map(|i| {
            ranks.record_proc(&procs[i]);
            Row {
                kind: RowKind::Process(i),
                depth: 0,
                expander: None,
                context_only: false,
            }
        })
        .collect();
    View {
        groups: Vec::new(),
        rows,
        children: Vec::new(),
    }
}

fn build_tree(
    procs: &[ProcessInfo],
    matches: &[bool],
    filtering: bool,
    sort: Sort,
    collapsed: &HashSet<ProcKey>,
    ranks: &mut RankBuilder,
) -> View {
    let n = procs.len();
    let by_pid: HashMap<u32, usize> = procs.iter().enumerate().map(|(i, p)| (p.pid, i)).collect();

    let mut children = vec![Vec::new(); n];
    let mut roots = Vec::new();
    for (i, p) in procs.iter().enumerate() {
        // A parent must have started no later than its child; otherwise the
        // parent PID has been reused by an unrelated process.
        let parent = p
            .parent_pid
            .and_then(|pp| by_pid.get(&pp).copied())
            .filter(|&j| j != i && procs[j].start_time <= p.start_time);
        match parent {
            Some(j) => children[j].push(i),
            None => roots.push(i),
        }
    }

    // Anything unreachable from a root sits on a parent cycle; promote one
    // node of each such cycle to a root so nothing disappears.
    let mut reachable = vec![false; n];
    let mut stack = roots.clone();
    for start in 0..=n {
        while let Some(i) = stack.pop() {
            if !std::mem::replace(&mut reachable[i], true) {
                stack.extend(&children[i]);
            }
        }
        if start < n && !reachable[start] {
            roots.push(start);
            stack.push(start);
        }
    }

    ranks.sort_procs(&mut roots, procs, sort);
    for list in &mut children {
        if list.len() > 1 {
            ranks.sort_procs(list, procs, sort);
        }
    }

    // With a filter, a node is shown if it or any descendant matches.
    let mut include = matches.to_vec();
    if filtering {
        let mut visited = vec![false; n];
        for &r in &roots {
            mark_included(r, &children, &mut include, &mut visited);
        }
    }

    let mut emitter = TreeEmitter {
        procs,
        children: &children,
        include: &include,
        matches,
        filtering,
        collapsed,
        visited: vec![false; n],
        rows: Vec::with_capacity(n),
    };
    for &r in &roots {
        emitter.emit(r, 0, true, ranks);
    }
    let rows = emitter.rows;

    View {
        groups: Vec::new(),
        rows,
        children,
    }
}

fn mark_included(i: usize, children: &[Vec<usize>], include: &mut [bool], visited: &mut [bool]) -> bool {
    visited[i] = true;
    let mut any = include[i];
    for &c in &children[i] {
        if !visited[c] && mark_included(c, children, include, visited) {
            any = true;
        }
    }
    include[i] = any;
    any
}

struct TreeEmitter<'a> {
    procs: &'a [ProcessInfo],
    children: &'a [Vec<usize>],
    include: &'a [bool],
    matches: &'a [bool],
    filtering: bool,
    collapsed: &'a HashSet<ProcKey>,
    visited: Vec<bool>,
    rows: Vec<Row>,
}

impl TreeEmitter<'_> {
    fn emit(&mut self, i: usize, depth: u16, visible: bool, ranks: &mut RankBuilder) {
        self.visited[i] = true;
        let p = &self.procs[i];
        ranks.record_proc(p);

        let shown = visible && self.include[i];
        // While filtering, paths to matches are always open.
        let expanded = self.filtering || !self.collapsed.contains(&ProcKey::of(p));
        if shown {
            let has_children = self.children[i].iter().any(|&c| self.include[c]);
            self.rows.push(Row {
                kind: RowKind::Process(i),
                depth,
                expander: has_children.then_some(expanded),
                context_only: self.filtering && !self.matches[i],
            });
        }
        let children = self.children;
        for &c in &children[i] {
            if !self.visited[c] {
                self.emit(c, depth + 1, shown && expanded, ranks);
            }
        }
    }
}

/// Executables are grouped by full path; processes whose path can't be read
/// (protected system processes) fall back to grouping by name. Computed
/// once per process by the sampler (`ProcessInfo::group_key`).
pub fn group_key(name: &str, exe: Option<&Path>) -> String {
    match exe {
        Some(exe) if !exe.as_os_str().is_empty() => exe.to_string_lossy().to_lowercase(),
        _ => format!("name:{}", name.to_lowercase()),
    }
}

struct Filter<'a> {
    text: &'a str,
    pid: Option<u32>,
}

impl<'a> Filter<'a> {
    fn new(text: &'a str) -> Self {
        let text = text.trim();
        Self {
            text,
            pid: text.parse().ok(),
        }
    }

    fn is_active(&self) -> bool {
        !self.text.is_empty()
    }

    fn matches(&self, p: &ProcessInfo) -> bool {
        !self.is_active()
            || self.pid == Some(p.pid)
            || contains_ignore_ascii_case(&p.name, self.text)
            || p.exe
                .as_ref()
                .is_some_and(|e| contains_ignore_ascii_case(&e.to_string_lossy(), self.text))
    }
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    n.is_empty() || h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

fn cmp_ignore_ascii_case(a: &str, b: &str) -> Ordering {
    a.bytes()
        .map(|c| c.to_ascii_lowercase())
        .cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn proc(pid: u32, ppid: Option<u32>, name: &str, cpu: f32, start: u64) -> ProcessInfo {
        let exe = PathBuf::from(format!(r"C:\Apps\{name}"));
        ProcessInfo {
            pid,
            parent_pid: ppid,
            group_key: group_key(name, Some(&exe)).into(),
            name: name.into(),
            exe: Some(exe.into()),
            cpu_percent: cpu,
            working_set: 0,
            private_bytes: 0,
            disk_read_per_sec: 0,
            disk_write_per_sec: 0,
            gpu_percent: 0.0,
            gpu_dedicated: 0,
            gpu_shared: 0,
            start_time: start,
        }
    }

    fn snap(seq: u64, processes: Vec<ProcessInfo>) -> Snapshot {
        Snapshot {
            seq,
            processes,
            ..Default::default()
        }
    }

    fn pids(view: &View, s: &Snapshot) -> Vec<u32> {
        view.rows
            .iter()
            .map(|r| match r.kind {
                RowKind::Process(i) => s.processes[i].pid,
                RowKind::Group(g) => view.groups[g].min_pid,
            })
            .collect()
    }

    #[test]
    fn groups_by_executable_with_sums() {
        let s = snap(1, vec![
            proc(1, None, "chrome.exe", 1.0, 10),
            proc(2, None, "chrome.exe", 2.0, 11),
            proc(3, None, "code.exe", 0.5, 12),
        ]);
        let mut state = ViewState::default();
        let view = state.build(&s);
        assert_eq!(view.rows.len(), 2);
        let chrome = view.find_group(r"c:\apps\chrome.exe").unwrap();
        assert_eq!(chrome.members.len(), 2);
        assert!((chrome.metrics(&s.processes).cpu_percent - 3.0).abs() < 1e-6);
        assert_eq!(view.rows[0].expander, Some(false));

        state.toggle(&RowId::Group(Arc::clone(&chrome.key)));
        assert_eq!(state.build(&s).rows.len(), 4);
    }

    #[test]
    fn ties_keep_previous_order() {
        let mut state = ViewState {
            mode: ViewMode::Processes,
            ..Default::default()
        };
        let first = snap(1, vec![proc(1, None, "a", 1.0, 1), proc(2, None, "b", 1.0, 1)]);
        assert_eq!(pids(&state.build(&first), &first), vec![1, 2]);
        // pid 2 is busier for one sample, then they tie again at display precision.
        let busy = snap(2, vec![proc(1, None, "a", 1.0, 1), proc(2, None, "b", 5.0, 1)]);
        assert_eq!(pids(&state.build(&busy), &busy), vec![2, 1]);
        let tie = snap(3, vec![proc(1, None, "a", 1.01, 1), proc(2, None, "b", 0.99, 1)]);
        assert_eq!(pids(&state.build(&tie), &tie), vec![2, 1]);
    }

    #[test]
    fn tree_ignores_reused_parent_pids_and_cycles() {
        let s = snap(1, vec![
            proc(10, None, "root.exe", 0.0, 100),
            proc(11, Some(10), "child.exe", 0.0, 200),
            // Parent PID 10 belongs to a process started *after* this one.
            proc(12, Some(10), "orphan.exe", 0.0, 50),
            // Two processes claiming each other as parent.
            proc(20, Some(21), "x.exe", 0.0, 300),
            proc(21, Some(20), "y.exe", 0.0, 300),
        ]);
        let mut state = ViewState {
            mode: ViewMode::Tree,
            sort: Sort { column: SortColumn::Pid, descending: false },
            ..Default::default()
        };
        let view = state.build(&s);
        let rows: Vec<(u32, u16)> = view
            .rows
            .iter()
            .map(|r| match r.kind {
                RowKind::Process(i) => (s.processes[i].pid, r.depth),
                RowKind::Group(_) => unreachable!(),
            })
            .collect();
        assert_eq!(rows.len(), 5, "every process appears exactly once: {rows:?}");
        assert!(rows.contains(&(11, 1)));
        assert!(rows.contains(&(12, 0)));
        assert_eq!(view.subtree(0).len(), 2);
    }

    #[test]
    fn tree_filter_keeps_ancestors_as_context() {
        let s = snap(1, vec![
            proc(1, None, "explorer.exe", 0.0, 1),
            proc(2, Some(1), "cmd.exe", 0.0, 2),
            proc(3, Some(2), "python.exe", 0.0, 3),
            proc(4, None, "other.exe", 0.0, 4),
        ]);
        let mut state = ViewState {
            mode: ViewMode::Tree,
            filter: "PYTHON".into(),
            ..Default::default()
        };
        // Collapsed state is overridden while filtering.
        state.collapsed_procs.insert(ProcKey { pid: 1, start_time: 1 });
        let view = state.build(&s);
        assert_eq!(pids(&view, &s), vec![1, 2, 3]);
        let context: Vec<bool> = view.rows.iter().map(|r| r.context_only).collect();
        assert_eq!(context, vec![true, true, false]);
    }
}
