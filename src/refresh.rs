use crate::{Paths, Stop};
use notify::{RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub current_secs: u64,
    pub list_secs: u64,
    pub fallback_secs: u64,
    pub cache_secs: u64,
    pub diagnostic_secs: u64,
    pub host_ms: u64,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            current_secs: 1,
            list_secs: 5,
            fallback_secs: 60,
            cache_secs: 30,
            diagnostic_secs: 5,
            host_ms: 500,
        }
    }
}
impl Settings {
    pub fn clamp(&mut self) {
        self.current_secs = self.current_secs.clamp(1, 10);
        self.list_secs = self.list_secs.clamp(2, 60);
        self.fallback_secs = self.fallback_secs.clamp(15, 300);
        self.cache_secs = self.cache_secs.clamp(15, 600);
        self.diagnostic_secs = self.diagnostic_secs.clamp(5, 120);
        self.host_ms = self.host_ms.clamp(250, 2000);
    }
    pub fn adjust(&mut self, field: usize, delta: i32) {
        let value = match field {
            0 => &mut self.current_secs,
            1 => &mut self.list_secs,
            2 => &mut self.fallback_secs,
            3 => &mut self.cache_secs,
            4 => &mut self.diagnostic_secs,
            _ => &mut self.host_ms,
        };
        *value = (*value as i64 + delta as i64).max(0) as u64;
        self.clamp();
    }
    pub fn value(&self, field: usize) -> String {
        match field {
            0 => format!("{} s", self.current_secs),
            1 => format!("{} s", self.list_secs),
            2 => format!("{} s", self.fallback_secs),
            3 => format!("{} s", self.cache_secs),
            4 => format!("{} s", self.diagnostic_secs),
            _ => format!("{} ms", self.host_ms),
        }
    }
}

#[derive(Default)]
pub struct Events {
    generation: Mutex<u64>,
    changed: Condvar,
    pub watching: AtomicBool,
}
impl Events {
    pub fn generation(&self) -> u64 {
        *self.generation.lock().unwrap()
    }
    pub fn notify(&self) {
        let mut n = self.generation.lock().unwrap();
        *n = n.wrapping_add(1);
        self.changed.notify_all();
    }
    pub fn wait(&self, known: u64, delay: Duration) {
        let current = self.generation.lock().unwrap();
        if *current == known {
            let _ = self
                .changed
                .wait_timeout(current, delay.min(Duration::from_secs(1)))
                .unwrap();
        }
    }
}

// notify's Windows backend uses overlapped ReadDirectoryChangesW callbacks,
// rather than repeatedly statting or opening files. Keep only relevant source
// changes; ignore access events and SQLite shared-memory lock traffic.
pub fn watch(paths: &Paths, events: Arc<Events>, stop: Stop) -> std::thread::JoinHandle<()> {
    let home = paths.home.clone();
    std::thread::spawn(move || {
        let callback_events = events.clone();
        let callback_home = home.clone();
        let mut watcher = notify::recommended_watcher(
            move |result: notify::Result<notify::Event>| match result {
                Ok(event) if !matches!(event.kind, notify::EventKind::Access(_)) => {
                    if event.need_rescan() || event.paths.iter().any(|path| {
                        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                        (path.parent() == Some(callback_home.as_path())
                            && (name.starts_with("state_") || name.starts_with("thread_history_"))
                            && (name.ends_with(".sqlite") || name.ends_with(".sqlite-wal")))
                            || (path.starts_with(callback_home.join("sessions"))
                                && path.extension().is_some_and(|e| e == "jsonl"))
                    }) {
                        callback_events.notify();
                    }
                }
                Err(_) => {
                    callback_events.watching.store(false, Ordering::Relaxed);
                    callback_events.notify();
                }
                _ => {}
            },
        )
        .ok();
        let mut active = false;
        if let Some(watcher) = watcher.as_mut() {
            active = watcher.watch(&home, RecursiveMode::NonRecursive).is_ok();
            let sessions = home.join("sessions");
            if sessions.is_dir() {
                active = watcher.watch(&sessions, RecursiveMode::Recursive).is_ok() || active;
            }
        }
        events.watching.store(active, Ordering::Relaxed);
        while !stop.load(Ordering::Relaxed) {
            // Only a shutdown wait; file delivery runs asynchronously inside notify.
            events.wait(events.generation(), Duration::from_secs(1));
        }
        drop(watcher);
        events.watching.store(false, Ordering::Relaxed);
        events.notify();
    })
}

pub struct Gate {
    pub generation: u64,
    pub updated: Option<Instant>,
}
impl Gate {
    pub fn new() -> Self {
        Self {
            generation: u64::MAX,
            updated: None,
        }
    }
    pub fn due(&self, generation: u64, minimum: u64, fallback: u64) -> bool {
        self.due_at(generation, minimum, fallback, Instant::now())
    }
    fn due_at(&self, generation: u64, minimum: u64, fallback: u64, at: Instant) -> bool {
        let Some(updated) = self.updated else { return true; };
        let elapsed = at.saturating_duration_since(updated);
        elapsed >= Duration::from_secs(fallback)
            || (generation != self.generation && elapsed >= Duration::from_secs(minimum))
    }
    pub fn mark(&mut self, generation: u64) {
        self.generation = generation;
        self.updated = Some(Instant::now());
    }
    pub fn pause(&self, events: &Events, minimum: u64) {
        // Once an event is pending, coalesce its burst until the minimum gap.
        if events.generation() != self.generation {
            std::thread::sleep(
                Duration::from_millis(50)
                    .min(Duration::from_secs(minimum).saturating_sub(self.updated.map(|at| at.elapsed()).unwrap_or_default())),
            );
        } else {
            events.wait(self.generation, Duration::from_secs(1));
        }
    }
}

// Failures have their own deadline, independent of file notifications and the
// normal fallback interval. No fabricated Instant before the system boot time.
#[derive(Default)]
pub struct Retry {
    failures: u32,
    next: Option<Instant>,
}
impl Retry {
    pub fn pending(&self) -> bool { self.next.is_some() }
    pub fn due(&self) -> bool { self.due_at(Instant::now()) }
    fn due_at(&self, at: Instant) -> bool { self.next.is_some_and(|next| at >= next) }
    pub fn waiting(&self) -> bool { self.pending() && !self.due() }
    pub fn success(&mut self) { *self = Self::default(); }
    pub fn failed(&mut self) { self.failed_at(Instant::now()); }
    fn failed_at(&mut self, at: Instant) {
        let delay = match self.failures { 0 => 1, 1 => 2, _ => 5 };
        self.next = Some(at + Duration::from_secs(delay));
        self.failures = self.failures.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unchanged_sources_wait_for_fallback_and_events_coalesce() {
        let events = Events::default();
        let mut gate = Gate::new();
        assert!(gate.due(events.generation(), 1, 60));
        gate.mark(events.generation());
        assert!(!gate.due(events.generation(), 1, 60));
        events.notify();
        assert!(!gate.due(events.generation(), 1, 60));
        let at = gate.updated.unwrap();
        assert!(gate.due_at(events.generation(), 1, 60, at + Duration::from_secs(2)));
        gate.mark(events.generation());
        assert!(gate.due_at(events.generation(), 1, 60, gate.updated.unwrap() + Duration::from_secs(61)));
    }
    #[test]
    fn cold_start_and_failed_sync_never_depend_on_uptime_or_source_events() {
        let gate = Gate::new();
        let at = Instant::now();
        assert!(gate.updated.is_none());
        assert!(gate.due_at(0, 5, 300, at));
        let mut retry = Retry::default();
        assert!(!retry.pending());
        for delay in [1, 2, 5, 5, 5] {
            retry.failed_at(at);
            assert_eq!(retry.next.unwrap().duration_since(at), Duration::from_secs(delay));
            assert!(!retry.due_at(at + Duration::from_millis(delay*1000-1)));
            assert!(retry.due_at(at + Duration::from_secs(delay)));
        }
        retry.success();
        assert!(!retry.pending());
        retry.failed_at(at);
        assert_eq!(retry.next.unwrap().duration_since(at), Duration::from_secs(1));
        let mut another_source = Retry::default();
        another_source.failed_at(at);
        retry.success();
        assert!(another_source.pending(), "One source recovering must not cancel another source's retry");
    }
    #[test]
    fn refresh_options_migrate_clamp_and_roundtrip() {
        let mut cfg: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg, Settings::default());
        cfg.adjust(0, -100);
        cfg.adjust(5, 10000);
        assert_eq!(cfg.current_secs, 1);
        assert_eq!(cfg.host_ms, 2000);
        assert_eq!(
            serde_json::from_str::<Settings>(&serde_json::to_string(&cfg).unwrap()).unwrap(),
            cfg
        );
    }
    #[test]
    fn native_file_change_wakes_waiters_and_stop_releases_watch() {
        let mut paths = Paths::discover().unwrap();
        let fixture = paths
            .runtime
            .join(format!("refresh-events-{}", std::process::id()));
        assert!(fixture.starts_with(&paths.runtime));
        std::fs::create_dir_all(fixture.join("sessions")).unwrap();
        paths.home = fixture.clone();
        let events = Arc::new(Events::default());
        let stop = Arc::new(AtomicBool::new(false));
        let watcher = watch(&paths, events.clone(), stop.clone());
        for _ in 0..100 {
            if events.watching.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(events.watching.load(Ordering::Relaxed));
        let before = events.generation();
        std::fs::write(fixture.join("state_5.sqlite-wal"), b"change").unwrap();
        for _ in 0..20 {
            if events.generation() != before {
                break;
            }
            events.wait(before, Duration::from_millis(100));
        }
        let received = events.generation() != before;
        stop.store(true, Ordering::Relaxed);
        events.notify();
        watcher.join().unwrap();
        assert!(!events.watching.load(Ordering::Relaxed));
        assert!(received);
        std::fs::remove_dir_all(fixture).unwrap();
    }
}
