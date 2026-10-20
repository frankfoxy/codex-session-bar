use anyhow::{bail, ensure, Result};
use serde::Serialize;
use std::{collections::HashMap, mem::size_of, time::Instant};
use windows::Win32::System::ProcessStatus::{GetPerformanceInfo, PERFORMANCE_INFORMATION};

#[repr(C)]
struct UString {
    len: u16,
    max: u16,
    ptr: *const u16,
}
#[repr(C)]
struct NativeProcess {
    next: u32,
    threads: u32,
    private_ws: i64,
    faults: u32,
    thread_peak: u32,
    cycles: u64,
    created: i64,
    user: i64,
    kernel: i64,
    name: UString,
    priority: i32,
    pid: usize,
    parent: usize,
    handles: u32,
    session: u32,
    key: usize,
    peak_virtual: usize,
    virtual_size: usize,
    page_faults: u32,
    peak_ws: usize,
    ws: usize,
    peak_paged: usize,
    paged: usize,
    peak_nonpaged: usize,
    nonpaged: usize,
    pagefile: usize,
    peak_pagefile: usize,
    private: usize,
    io: [i64; 6],
}
#[link(name = "ntdll")]
extern "system" {
    fn NtQuerySystemInformation(
        class: u32,
        data: *mut std::ffi::c_void,
        size: u32,
        needed: *mut u32,
    ) -> i32;
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Process {
    pub name: String,
    pub pid: u32,
    pub parent_pid: u32,
    pub created_ticks: i64,
    pub private: u64,
    pub working_set: u64,
    pub delta: i64,
    pub growth: i64,
    pub peak_private: u64,
    pub paged_pool: u64,
    pub nonpaged_pool: u64,
    pub threads: u32,
    pub handles: u32,
    pub cpu_percent: f64,
    pub new: bool,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct Group {
    pub name: String,
    pub count: usize,
    pub private: u64,
    pub working_set: u64,
    pub delta: i64,
    pub growth: i64,
    pub peak_private: u64,
    pub cpu_percent: f64,
    pub threads: u32,
    pub handles: u32,
    pub paged_pool: u64,
    pub nonpaged_pool: u64,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct System {
    pub physical_total: u64,
    pub physical_available: u64,
    pub commit_total: u64,
    pub commit_limit: u64,
    pub commit_peak: u64,
    pub paged_pool: u64,
    pub nonpaged_pool: u64,
    pub process_count: u32,
    pub threads: u32,
    pub handles: u32,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct Snapshot {
    pub time: String,
    pub epoch: f64,
    pub interval_seconds: u64,
    pub collection_ms: f64,
    pub system: System,
    pub processes: Vec<Process>,
    pub groups: Vec<Group>,
}
#[derive(Clone)]
struct History {
    created: i64,
    private: u64,
    baseline: u64,
    cpu: i64,
}
pub struct Collector {
    buffer: Vec<u64>,
    processes: HashMap<u32, History>,
    groups: HashMap<String, (u64, u64, u64)>,
    last: Instant,
    first: bool,
    cpus: f64,
}
impl Collector {
    pub fn new() -> Self {
        Self {
            buffer: vec![0; 131072],
            processes: HashMap::new(),
            groups: HashMap::new(),
            last: Instant::now(),
            first: true,
            cpus: std::thread::available_parallelism().map_or(1, |n| n.get()) as f64,
        }
    }
    pub fn sample(&mut self, interval_seconds: u64) -> Result<Snapshot> {
        let begin = Instant::now();
        let mut perf = PERFORMANCE_INFORMATION::default();
        unsafe {
            GetPerformanceInfo(&mut perf, size_of::<PERFORMANCE_INFORMATION>() as u32)?;
        }
        let mut needed = 0;
        let used = loop {
            let bytes = self.buffer.len() * 8;
            let status = unsafe {
                NtQuerySystemInformation(
                    5,
                    self.buffer.as_mut_ptr().cast(),
                    bytes as u32,
                    &mut needed,
                )
            };
            if status == 0 {
                break needed as usize;
            }
            if status as u32 != 0xc0000004 {
                bail!("Process snapshot NTSTATUS 0x{:08x}", status as u32);
            }
            let next = (bytes * 2).max(needed as usize + 65536);
            ensure!(
                next <= 32 * 1024 * 1024,
                "Process snapshot exceeds 32 MiB bound"
            );
            self.buffer.resize(next.div_ceil(8), 0);
        };
        ensure!(
            used <= self.buffer.len() * 8,
            "Invalid process snapshot length"
        );
        let base = self.buffer.as_ptr() as usize;
        let elapsed = self.last.elapsed().as_secs_f64().max(0.001);
        let mut rows = Vec::with_capacity(perf.ProcessCount as usize + 16);
        let mut current = HashMap::new();
        let mut offset = 0;
        loop {
            ensure!(
                offset + size_of::<NativeProcess>() <= used,
                "Truncated process snapshot"
            );
            let item = unsafe { &*((base + offset) as *const NativeProcess) };
            let ptr = item.name.ptr as usize;
            let name = if item.name.len != 0
                && item.name.len % 2 == 0
                && ptr >= base
                && ptr
                    .checked_add(item.name.len as usize)
                    .is_some_and(|end| end <= base + used)
            {
                String::from_utf16_lossy(unsafe {
                    std::slice::from_raw_parts(item.name.ptr, item.name.len as usize / 2)
                })
            } else if item.pid == 0 {
                "Idle".into()
            } else {
                "System".into()
            };
            let old = self
                .processes
                .get(&(item.pid as u32))
                .filter(|h| h.created == item.created);
            let private = item.private as u64;
            let cpu = item.user.saturating_add(item.kernel);
            let baseline = old.map_or(private, |h| h.baseline);
            rows.push(Process {
                name,
                pid: item.pid as u32,
                parent_pid: item.parent as u32,
                created_ticks: item.created,
                private,
                working_set: item.ws as u64,
                delta: old.map_or(0, |h| private as i64 - h.private as i64),
                growth: private as i64 - baseline as i64,
                peak_private: item.peak_pagefile as u64,
                paged_pool: item.paged as u64,
                nonpaged_pool: item.nonpaged as u64,
                threads: item.threads,
                handles: item.handles,
                cpu_percent: old.map_or(0., |h| {
                    ((cpu - h.cpu).max(0) as f64 / 10_000_000. / elapsed / self.cpus * 100.)
                        .clamp(0., 100.)
                }),
                new: old.is_none(),
            });
            current.insert(
                item.pid as u32,
                History {
                    created: item.created,
                    private,
                    baseline,
                    cpu,
                },
            );
            if item.next == 0 {
                break;
            }
            ensure!(
                item.next as usize >= size_of::<NativeProcess>(),
                "Invalid process entry size"
            );
            offset += item.next as usize;
        }
        let mut merged: HashMap<String, Group> = HashMap::new();
        for p in &rows {
            let g = merged
                .entry(p.name.to_lowercase())
                .or_insert_with(|| Group {
                    name: p.name.clone(),
                    ..Default::default()
                });
            g.count += 1;
            g.private += p.private;
            g.working_set += p.working_set;
            g.cpu_percent += p.cpu_percent;
            g.threads += p.threads;
            g.handles += p.handles;
            g.paged_pool += p.paged_pool;
            g.nonpaged_pool += p.nonpaged_pool;
        }
        let mut group_history = HashMap::new();
        for (name, g) in &mut merged {
            let old = self.groups.get(name);
            let baseline = old.map_or(g.private, |v| v.1);
            g.delta = old.map_or(if self.first { 0 } else { g.private as i64 }, |v| {
                g.private as i64 - v.0 as i64
            });
            g.growth = g.private as i64 - baseline as i64;
            g.peak_private = old.map_or(g.private, |v| v.2.max(g.private));
            group_history.insert(name.clone(), (g.private, baseline, g.peak_private));
        }
        let mut groups: Vec<_> = merged.into_values().collect();
        groups.sort_by_key(|g| std::cmp::Reverse(g.private));
        rows.sort_by_key(|p| std::cmp::Reverse(p.private));
        let page = perf.PageSize as u64;
        let snapshot = Snapshot {
            time: chrono::Local::now().to_rfc3339(),
            epoch: chrono::Utc::now().timestamp_millis() as f64 / 1000.,
            interval_seconds,
            collection_ms: begin.elapsed().as_secs_f64() * 1000.,
            system: System {
                physical_total: perf.PhysicalTotal as u64 * page,
                physical_available: perf.PhysicalAvailable as u64 * page,
                commit_total: perf.CommitTotal as u64 * page,
                commit_limit: perf.CommitLimit as u64 * page,
                commit_peak: perf.CommitPeak as u64 * page,
                paged_pool: perf.KernelPaged as u64 * page,
                nonpaged_pool: perf.KernelNonpaged as u64 * page,
                process_count: perf.ProcessCount,
                handles: perf.HandleCount,
                threads: perf.ThreadCount,
            },
            processes: rows,
            groups,
        };
        self.processes = current;
        self.groups = group_history;
        self.last = Instant::now();
        self.first = false;
        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn live_snapshot_and_group_totals() {
        assert_eq!(size_of::<NativeProcess>(), 256);
        let mut c = Collector::new();
        let a = c.sample(30).unwrap();
        assert!(a.processes.iter().any(|p| p.pid == std::process::id()));
        assert!(a.system.commit_limit >= a.system.commit_total);
        assert_eq!(
            a.groups.iter().map(|g| g.private).sum::<u64>(),
            a.processes.iter().map(|p| p.private).sum::<u64>()
        );
        let own = a
            .processes
            .iter()
            .find(|p| p.pid == std::process::id())
            .unwrap();
        let mut reused = c.processes.get(&own.pid).unwrap().clone();
        reused.created -= 1;
        reused.private = 0;
        reused.baseline = 0;
        c.processes.insert(own.pid, reused);
        let b = c.sample(30).unwrap();
        let own = b
            .processes
            .iter()
            .find(|p| p.pid == std::process::id())
            .unwrap();
        assert!(own.new);
        assert_eq!(own.delta, 0);
        assert_eq!(own.growth, 0);
    }
}
