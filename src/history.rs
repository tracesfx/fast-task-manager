//! Rolling history of the last `WINDOW` of samples: system totals plus CPU
//! and memory per app group.
//!
//! Recorded on the sampler thread, so nothing is missed while the window is
//! minimized or between frames. The sampler aggregates a `Sample` outside the
//! lock and only holds it for `push`. Memory is bounded by time: points older
//! than `WINDOW` are dropped, and a group that hasn't been seen for
//! `EXITED_KEEP` is forgotten entirely.
//!
//! Points are indexed by sample number rather than storing a time each: the
//! system series holds the timestamps, and a group's series starts at some
//! sample number and is contiguous from there (an absent sample is filled with
//! zeros when the group comes back).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::model::Snapshot;

pub const WINDOW: Duration = Duration::from_secs(300);
/// History of a group with no running processes is dropped after this long.
const EXITED_KEEP: Duration = Duration::from_secs(60);
/// Top CPU consumers remembered per sample.
pub const TOP_N: usize = 5;

#[derive(Clone, Debug)]
pub struct TopApp {
    pub name: Arc<str>,
    pub cpu_percent: f32,
}

#[derive(Clone, Debug)]
pub struct SystemPoint {
    /// Seconds since the history was created.
    pub t: f64,
    pub cpu_percent: f32,
    pub used_memory: u64,
    pub disk_read_per_sec: u64,
    pub disk_write_per_sec: u64,
    /// Busiest app groups at this moment, busiest first.
    pub top: Box<[TopApp]>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GroupPoint {
    pub cpu_percent: f32,
    /// Working set in KiB (keeps a point at 8 bytes).
    mem_kib: u32,
}

impl GroupPoint {
    pub fn working_set(self) -> u64 {
        u64::from(self.mem_kib) * 1024
    }
}

pub struct GroupHistory {
    pub name: Arc<str>,
    /// Sample number of `points[0]`.
    first: u64,
    points: VecDeque<GroupPoint>,
    /// `t` of the last sample the group had processes in.
    last_seen: f64,
}

impl GroupHistory {
    pub fn points(&self) -> impl ExactSizeIterator<Item = GroupPoint> + '_ {
        self.points.iter().copied()
    }
}

/// One snapshot reduced to what the history keeps; built outside the lock.
pub struct Sample {
    cpu_percent: f32,
    used_memory: u64,
    disk_read_per_sec: u64,
    disk_write_per_sec: u64,
    /// (group key, display name, point), in no particular order.
    groups: Vec<(Arc<str>, Arc<str>, GroupPoint)>,
}

impl Sample {
    pub fn of(snap: &Snapshot) -> Self {
        profile_scope!("history sample");
        let mut index: HashMap<&str, usize> = HashMap::new();
        let mut groups: Vec<(Arc<str>, Arc<str>, f32, u64)> = Vec::new();
        let (mut read, mut write) = (0u64, 0u64);
        for p in &snap.processes {
            read += p.disk_read_per_sec;
            write += p.disk_write_per_sec;
            let g = *index.entry(&p.group_key).or_insert_with(|| {
                groups.push((Arc::clone(&p.group_key), Arc::clone(&p.name), 0.0, 0));
                groups.len() - 1
            });
            groups[g].2 += p.cpu_percent;
            groups[g].3 += p.working_set;
        }
        Self {
            cpu_percent: snap.system.cpu_percent,
            used_memory: snap.system.used_memory,
            disk_read_per_sec: read,
            disk_write_per_sec: write,
            groups: groups
                .into_iter()
                .map(|(key, name, cpu, mem)| {
                    let point = GroupPoint {
                        cpu_percent: cpu.min(100.0),
                        mem_kib: (mem / 1024).min(u64::from(u32::MAX)) as u32,
                    };
                    (key, name, point)
                })
                .collect(),
        }
    }
}

pub struct History {
    epoch: Instant,
    /// Sample number of `system[0]`.
    first: u64,
    system: VecDeque<SystemPoint>,
    groups: HashMap<Arc<str>, GroupHistory>,
}

impl Default for History {
    fn default() -> Self {
        Self::new(Instant::now())
    }
}

impl History {
    pub fn new(epoch: Instant) -> Self {
        Self {
            epoch,
            first: 0,
            system: VecDeque::new(),
            groups: HashMap::new(),
        }
    }

    pub fn system(&self) -> &VecDeque<SystemPoint> {
        &self.system
    }

    /// `t` of the newest sample (0 if there is none).
    pub fn now(&self) -> f64 {
        self.system.back().map_or(0.0, |p| p.t)
    }

    pub fn group(&self, key: &str) -> Option<&GroupHistory> {
        self.groups.get(key)
    }

    /// The group's points with their `t`, oldest first.
    pub fn group_series<'a>(&'a self, g: &'a GroupHistory) -> impl Iterator<Item = (f64, GroupPoint)> + 'a {
        let skip = g.first.saturating_sub(self.first) as usize;
        self.system.iter().skip(skip).map(|s| s.t).zip(g.points())
    }

    pub fn push(&mut self, at: Instant, sample: Sample) {
        let t = at.saturating_duration_since(self.epoch).as_secs_f64();
        let n = self.first + self.system.len() as u64;

        let mut top: Vec<(usize, f32)> = sample
            .groups
            .iter()
            .enumerate()
            .filter(|(_, g)| g.2.cpu_percent > 0.0)
            .map(|(i, g)| (i, g.2.cpu_percent))
            .collect();
        top.sort_by(|a, b| b.1.total_cmp(&a.1));
        top.truncate(TOP_N);
        let mut top: Vec<(usize, Option<Arc<str>>, f32)> =
            top.into_iter().map(|(i, cpu)| (i, None, cpu)).collect();

        for (i, (key, name, point)) in sample.groups.into_iter().enumerate() {
            let g = self.groups.entry(key).or_insert_with(|| GroupHistory {
                name,
                first: n,
                points: VecDeque::new(),
                last_seen: t,
            });
            // Back after an absence (or new): fill the gap with zeros.
            let end = g.first + g.points.len() as u64;
            if g.points.is_empty() {
                g.first = n;
            } else if end < n {
                g.points.extend(std::iter::repeat_n(GroupPoint::default(), (n - end) as usize));
            }
            g.points.push_back(point);
            g.last_seen = t;
            if let Some(entry) = top.iter_mut().find(|e| e.0 == i) {
                entry.1 = Some(Arc::clone(&g.name));
            }
        }

        self.system.push_back(SystemPoint {
            t,
            cpu_percent: sample.cpu_percent,
            used_memory: sample.used_memory,
            disk_read_per_sec: sample.disk_read_per_sec,
            disk_write_per_sec: sample.disk_write_per_sec,
            top: top
                .into_iter()
                .filter_map(|(_, name, cpu_percent)| Some(TopApp { name: name?, cpu_percent }))
                .collect(),
        });
        self.prune(t);
    }

    fn prune(&mut self, now: f64) {
        let oldest = now - WINDOW.as_secs_f64();
        while self.system.front().is_some_and(|p| p.t < oldest) {
            self.system.pop_front();
            self.first += 1;
        }
        shrink(&mut self.system);

        let keep_since = now - EXITED_KEEP.as_secs_f64();
        let first = self.first;
        self.groups.retain(|_, g| {
            let stale = first.saturating_sub(g.first).min(g.points.len() as u64);
            g.points.drain(..stale as usize);
            g.first += stale;
            shrink(&mut g.points);
            g.last_seen >= keep_since && !g.points.is_empty()
        });
    }
}

/// Releases capacity left over from a faster sampling interval.
fn shrink<T>(deque: &mut VecDeque<T>) {
    if deque.capacity() > 2 * deque.len() + 64 {
        deque.shrink_to(deque.len() + deque.len() / 4);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProcessInfo;
    use crate::view;
    use std::path::PathBuf;

    fn proc(pid: u32, name: &str, cpu: f32, mem: u64) -> ProcessInfo {
        let exe = PathBuf::from(format!(r"C:\Apps\{name}"));
        ProcessInfo {
            pid,
            parent_pid: None,
            group_key: view::group_key(name, Some(&exe)).into(),
            name: name.into(),
            exe: Some(exe.into()),
            cpu_percent: cpu,
            working_set: mem,
            private_bytes: 0,
            disk_read_per_sec: 10,
            disk_write_per_sec: 5,
            start_time: 1,
        }
    }

    fn sample(processes: Vec<ProcessInfo>) -> Sample {
        Sample::of(&Snapshot {
            processes,
            ..Default::default()
        })
    }

    fn key(name: &str) -> String {
        format!(r"c:\apps\{name}")
    }

    #[test]
    fn groups_sum_and_top_consumers() {
        let epoch = Instant::now();
        let mut h = History::new(epoch);
        h.push(
            epoch,
            sample(vec![
                proc(1, "a.exe", 2.0, 4096),
                proc(2, "a.exe", 3.0, 4096),
                proc(3, "b.exe", 1.0, 1024),
                proc(4, "c.exe", 0.0, 1024),
            ]),
        );
        let a = h.group(&key("a.exe")).unwrap();
        let points: Vec<GroupPoint> = a.points().collect();
        assert_eq!(points.len(), 1);
        assert!((points[0].cpu_percent - 5.0).abs() < 1e-6);
        assert_eq!(points[0].working_set(), 8192);

        let s = &h.system()[0];
        assert_eq!(s.disk_read_per_sec, 40);
        let top: Vec<&str> = s.top.iter().map(|t| &*t.name).collect();
        // Idle groups are not "top consumers".
        assert_eq!(top, vec!["a.exe", "b.exe"]);
    }

    #[test]
    fn gaps_fill_with_zeros_and_series_align() {
        let epoch = Instant::now();
        let mut h = History::new(epoch);
        let at = |s: u64| epoch + Duration::from_secs(s);
        h.push(at(0), sample(vec![proc(1, "a.exe", 1.0, 0), proc(2, "b.exe", 1.0, 0)]));
        h.push(at(1), sample(vec![proc(2, "b.exe", 1.0, 0)]));
        h.push(at(2), sample(vec![proc(1, "a.exe", 2.0, 0), proc(2, "b.exe", 1.0, 0)]));
        h.push(at(3), sample(vec![proc(3, "c.exe", 4.0, 0)]));

        let a = h.group(&key("a.exe")).unwrap();
        let series: Vec<(f64, f32)> = h.group_series(a).map(|(t, p)| (t, p.cpu_percent)).collect();
        assert_eq!(series, vec![(0.0, 1.0), (1.0, 0.0), (2.0, 2.0)]);
        let c = h.group(&key("c.exe")).unwrap();
        let series: Vec<f64> = h.group_series(c).map(|(t, _)| t).collect();
        assert_eq!(series, vec![3.0]);
    }

    #[test]
    fn old_points_and_exited_groups_are_dropped() {
        let epoch = Instant::now();
        let mut h = History::new(epoch);
        let window = WINDOW.as_secs();
        h.push(epoch, sample(vec![proc(1, "gone.exe", 1.0, 0), proc(2, "stays.exe", 1.0, 0)]));
        for s in 1..=window + 10 {
            h.push(epoch + Duration::from_secs(s), sample(vec![proc(2, "stays.exe", 1.0, 0)]));
        }
        assert!(h.group(&key("gone.exe")).is_none());
        let stays = h.group(&key("stays.exe")).unwrap();
        assert_eq!(stays.points().len(), h.system().len());
        assert_eq!(h.system().len() as u64, window + 1);
        assert_eq!(h.group_series(stays).next().unwrap().0, 10.0);
    }
}
