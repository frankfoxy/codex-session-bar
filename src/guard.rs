//! Session Bar controls; enforcement belongs to the independent Memory Watch.
use crate::{now, read_json, Paths, State, Stop};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    os::windows::process::CommandExt,
    path::PathBuf,
    process::Command,
    sync::atomic::Ordering,
    thread,
    time::{Duration, Instant},
};
use windows::{
    core::w,
    Win32::{System::Threading::CREATE_NO_WINDOW, UI::WindowsAndMessaging::*},
};
const CONFIG: &str = "codex-git-guard-settings.json";
const STATUS: &str = "codex-git-guard-status.json";
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct View {
    pub enabled: bool,
    pub available: bool,
    pub count: usize,
    pub private_bytes: u64,
    pub terminated: usize,
    pub max_processes: usize,
    pub max_private_mib: u64,
    pub note: String,
    pub storm_count: Option<u64>,
    pub storm_active: bool,
    pub tracking_since: String,
}
impl Default for View {
    fn default() -> Self {
        Self {
            enabled: true,
            available: false,
            count: 0,
            private_bytes: 0,
            terminated: 0,
            max_processes: 64,
            max_private_mib: 4096,
            note: "Connecting to Memory Watch…".into(),
            storm_count: None,
            storm_active: false,
            tracking_since: String::new(),
        }
    }
}
fn dir(paths: &Paths) -> PathBuf {
    paths.runtime.parent().unwrap().join("memory-watch-rust")
}
pub fn log_dir(paths: &Paths) -> PathBuf { dir(paths) }
#[derive(Clone, Default)]
pub struct LogEntry {
    pub time: String,
    pub title: String,
    pub detail: String,
    pub warning: bool,
}
pub fn recent_events(paths: &Paths) -> anyhow::Result<Vec<LogEntry>> {
    let path = dir(paths).join("codex-git-guard-events.jsonl");
    if !path.exists() { return Ok(Vec::new()); }
    let mut file = fs::File::open(path)?;
    let start = file.metadata()?.len().saturating_sub(65536);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity(65536);
    file.take(65536).read_to_end(&mut bytes)?;
    Ok(parse_events(&bytes, start > 0))
}
fn parse_events(bytes: &[u8], skip_first: bool) -> Vec<LogEntry> {
    let end = bytes.iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0);
    let mut entries = Vec::new();
    for line in bytes[..end].split(|b| *b == b'\n').skip(usize::from(skip_first)) {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else { continue; };
        let kind = value["type"].as_str().unwrap_or("");
        let pid = value["pid"].as_u64().unwrap_or(0);
        let (title, detail, warning) = match kind {
            "storm_started" => (format!("Git storm #{} detected", value["episode"].as_u64().unwrap_or(0)),
                format!("{} Git · {:.0} MiB · protection activated", value["count"].as_u64().unwrap_or(0), value["private_bytes"].as_u64().unwrap_or(0) as f64 / 1048576.), true),
            "storm_recovered" => ("Storm recovered".into(), "Below limits for 60 seconds".into(), false),
            "limit_exceeded" => ("Limit exceeded".into(), format!("{} Git · {:.0} MiB · {}", value["count"].as_u64().unwrap_or(0), value["private_bytes"].as_u64().unwrap_or(0) as f64 / 1048576., value["reason"].as_str().unwrap_or("")), true),
            "termination_requested" => (format!("Ending read-only Git · PID {pid}"), format!("{} · {:.0} MiB · Codex PID {}", value["verb"].as_str().unwrap_or("query"), value["private_bytes"].as_u64().unwrap_or(0) as f64 / 1048576., value["codex_pid"].as_u64().unwrap_or(0)), false),
            "termination_result" => {
                let result = value["result"].as_str().unwrap_or("unknown");
                (format!("{} · PID {pid}", if result == "terminated" { "Git ended" } else { "Git skipped / pending" }), result.chars().take(180).collect(), result != "terminated")
            }
            _ => continue,
        };
        let time = value["time"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.format("%m-%d %H:%M:%S").to_string()).unwrap_or_else(|| "—".into());
        entries.push(LogEntry { time, title, detail, warning });
    }
    entries.into_iter().rev().take(200).collect()
}
pub fn set(paths: &Paths, enabled: bool) -> anyhow::Result<()> {
    let dir = dir(paths);
    fs::create_dir_all(&dir)?;
    let mut value = read_json(&dir.join(CONFIG), 4096);
    if !value.is_object() {
        value = json!({"max_processes":64,"max_private_mib":4096});
    }
    value["enabled"] = json!(enabled);
    let pending = dir.join(format!("guard-control.{}.pending", std::process::id()));
    fs::write(&pending, serde_json::to_vec(&value)?)?;
    fs::rename(pending, dir.join(CONFIG))?;
    Ok(())
}
fn backend(paths: &Paths) -> Option<(PathBuf, PathBuf)> {
    let bundled = paths.app.join("bin/memory-watch.exe");
    if bundled.is_file() {
        return Some((bundled, paths.app.join("memory-watch")));
    }
    if paths
        .app
        .parent()
        .and_then(|p| p.file_name())
        .is_some_and(|n| n == "tools")
    {
        let app = paths.app.parent()?.join("memory-watch-rs");
        let exe = app.join("bin/memory-watch.exe");
        if exe.is_file() {
            return Some((exe, app));
        }
    }
    None
}
fn snapshot(paths: &Paths) -> View {
    let dir = dir(paths);
    let cfg = read_json(&dir.join(CONFIG), 4096);
    let status = read_json(&dir.join(STATUS), 16384);
    let enabled = cfg["enabled"].as_bool().unwrap_or(true);
    let mut pid = 0;
    let native = unsafe {
        FindWindowW(w!("RustMemoryWatch"), None)
            .ok()
            .map(|window| {
                GetWindowThreadProcessId(window, Some(&mut pid));
            })
            .is_some()
    };
    let fresh = status["version"] == 1
        && native
        && status["pid"].as_u64() == Some(pid as u64)
        && status["updated_at"]
            .as_f64()
            .is_some_and(|at| (0.0..=15.0).contains(&(now() - at)));
    let healthy = fresh && status["healthy"].as_bool() == Some(true);
    let available = healthy && status["enabled"].as_bool() == Some(enabled);
    let note = if available {
        if enabled {
            format!(
                "{} Git · {:.0} MiB · ended {}",
                status["count"].as_u64().unwrap_or(0),
                status["private_bytes"].as_u64().unwrap_or(0) as f64 / 1048576.,
                status["terminated"].as_u64().unwrap_or(0)
            )
        } else {
            "Protection is off".into()
        }
    } else if fresh && !healthy {
        format!(
            "Guard error: {}",
            status["error"].as_str().unwrap_or("check Memory Watch")
        )
    } else if native && !fresh {
        "Memory Watch guard is unavailable or outdated".into()
    } else if fresh {
        "Applying change…".into()
    } else {
        "Protection unavailable · start Memory Watch".into()
    };
    View {
        enabled,
        available,
        count: status["count"].as_u64().unwrap_or(0) as usize,
        private_bytes: status["private_bytes"].as_u64().unwrap_or(0),
        terminated: status["terminated"].as_u64().unwrap_or(0) as usize,
        max_processes: cfg["max_processes"].as_u64().unwrap_or(64) as usize,
        max_private_mib: cfg["max_private_mib"].as_u64().unwrap_or(4096),
        note,
        storm_count: status["storm_count"].as_u64(),
        storm_active: status["storm_active"].as_bool().unwrap_or(false),
        tracking_since: status["tracking_since"].as_str().unwrap_or("").to_string(),
    }
}
pub fn start(paths: &Paths, state: &State, stop: &Stop) -> thread::JoinHandle<()> {
    let (paths, state, stop) = (paths.clone(), state.clone(), stop.clone());
    thread::spawn(move || {
        let mut retry = Instant::now() - Duration::from_secs(60);
        while !stop.load(Ordering::Relaxed) {
            let mut view = snapshot(&paths);
            let running = unsafe { FindWindowW(w!("RustMemoryWatch"), None).is_ok() };
            if view.enabled && !running && retry.elapsed() > Duration::from_secs(60) {
                retry = Instant::now();
                if let Some((exe, home)) = backend(&paths) {
                    let result = fs::create_dir_all(&home).and_then(|_| {
                        Command::new(exe)
                            .arg("--background")
                            .current_dir(&home)
                            .env("MEMORY_WATCH_APP_DIR", &home)
                            .env(
                                "CODEX_SESSION_BAR_WORKSPACE",
                                paths.runtime.parent().unwrap().parent().unwrap(),
                            )
                            .env("TEMP", &paths.runtime)
                            .env("TMP", &paths.runtime)
                            .env("TMPDIR", &paths.runtime)
                            .creation_flags(CREATE_NO_WINDOW.0)
                            .spawn()
                    });
                    if let Err(error) = result {
                        view.note = format!("Cannot start Memory Watch: {error}");
                    } else {
                        view.note = "Starting Memory Watch…".into();
                    }
                }
            }
            state.lock().unwrap().guard = view;
            for _ in 0..10 {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn log_tail_discards_partial_records_and_distinguishes_counts() {
        let data = b"truncated\n{\"type\":\"storm_started\",\"episode\":2,\"count\":100}\n{\"type\":\"termination_result\",\"pid\":123,\"result\":\"terminated\"}\n{\"type\":\"termination_result\",\"pid\":124,\"result\":\"skipped: identity changed\"}\n{\"type\":\"storm_recovered\"}\n{\"type\":";
        let rows = parse_events(data, true);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].title, "Storm recovered");
        assert!(rows[1].warning);
        assert!(!rows[2].warning);
        assert_eq!(rows[2].title, "Git ended · PID 123");
        assert_eq!(rows[3].title, "Git storm #2 detected");
        assert!(!rows[3].title.contains("100"));
    }
}
