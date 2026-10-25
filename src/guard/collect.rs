//! Lightweight process identities and private bytes used only by Git protection.
//! No whole-system memory history, grouping, CPU tracking or sample recording.
use anyhow::{bail, ensure, Result};
use std::mem::size_of;
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


#[derive(Clone, Debug, Default)]
pub struct Process {
    pub name: String,
    pub pid: u32,
    pub parent_pid: u32,
    pub created_ticks: i64,
    pub private: u64,
}
pub struct Snapshot { pub processes: Vec<Process> }
pub struct Collector { buffer: Vec<u64> }
impl Collector {
    pub fn new() -> Self { Self { buffer: vec![0; 131072] } }
    pub fn sample(&mut self, _interval_seconds: u64) -> Result<Snapshot> {
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
        let mut rows = Vec::new();
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
            rows.push(Process {
                name, pid: item.pid as u32, parent_pid: item.parent as u32,
                created_ticks: item.created, private: item.private as u64,
            });
            if item.next == 0 {
                break;
            }
            ensure!(
                item.next as usize >= size_of::<NativeProcess>(),
                "Invalid process entry size"
            );
            offset += item.next as usize;
        }
        Ok(Snapshot { processes: rows })
    }
}
#[cfg(test)] mod tests {
    use super::*;
    #[test] fn live_snapshot_contains_current_process_identity() {
        assert_eq!(size_of::<NativeProcess>(), 256);
        let mut collector = Collector::new();
        let first = collector.sample(5).unwrap();
        let own = first.processes.iter().find(|p| p.pid == std::process::id()).unwrap();
        assert!(own.created_ticks > 0);
        assert!(own.private > 0);
        let second = collector.sample(5).unwrap();
        assert_eq!(second.processes.iter().find(|p| p.pid == own.pid).unwrap().created_ticks, own.created_ticks);
    }
}
