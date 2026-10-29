//! Integrated Codex Git protection, settings and status. No companion process.
mod worker;
mod collect;
mod attribution;
mod archive;
mod notice;
use crate::{now, read_json, Paths, State, Stop};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    sync::atomic::Ordering,
    thread,
    time::{Duration, Instant},
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
    pub interval_seconds: u64,
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
            max_processes: 16,
            max_private_mib: 1024,
            interval_seconds: 5,
            note: "Starting Git protection…".into(),
            storm_count: None,
            storm_active: false,
            tracking_since: String::new(),
        }
    }
}
fn dir(paths: &Paths) -> PathBuf {
    paths.runtime.join("git-guard")
}
pub fn migrate_legacy(legacy:&std::path::Path,destination:&std::path::Path)->anyhow::Result<()> {
    // Copy only small settings/counters; never remove the old records or override
    // an already initialized installation. Runtime files follow the OS temp root.
    if !legacy.is_dir() || [CONFIG,STATUS,"codex-git-guard-counters.json"].iter().any(|name|destination.join(name).exists()) {return Ok(());}
    for name in [CONFIG,STATUS,"codex-git-guard-counters.json"] {
        let source=legacy.join(name);
        if !fs::metadata(&source).is_ok_and(|m|m.is_file()&&m.len()<=16384) {continue;}
        fs::create_dir_all(destination)?;
        let mut input=fs::File::open(&source)?;
        match fs::OpenOptions::new().write(true).create_new(true).open(destination.join(name)) {
            Ok(mut output)=>{std::io::copy(&mut input,&mut output)?;},
            Err(error) if error.kind()==std::io::ErrorKind::AlreadyExists=>{},
            Err(error)=>return Err(error.into()),
        }
    }
    Ok(())
}
pub fn log_dir(paths: &Paths) -> PathBuf { dir(paths) }
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(default)]
pub struct Limits {pub max_processes:u64,pub max_private_mib:u64,pub interval_seconds:u64}
impl Default for Limits {fn default()->Self {Self{max_processes:16,max_private_mib:1024,interval_seconds:5}}}
impl Limits {
    pub fn load(paths:&Paths)->Self {serde_json::from_value(read_json(&dir(paths).join(CONFIG),4096)).unwrap_or_default()}
    pub fn adjust(&mut self,field:usize,delta:i32){
        let (value,low,high)=match field {0=>(&mut self.max_processes,8,4096),1=>(&mut self.max_private_mib,256,65536),_=>(&mut self.interval_seconds,1,60)};
        *value=(*value as i64+delta as i64).clamp(low,high) as u64;
    }
    pub fn save(&self,paths:&Paths)->anyhow::Result<()> {
        anyhow::ensure!((8..=4096).contains(&self.max_processes)&&(256..=65536).contains(&self.max_private_mib)&&(1..=60).contains(&self.interval_seconds),"Invalid Git guard limits");
        let folder=dir(paths);fs::create_dir_all(&folder)?;
        let mut value=read_json(&folder.join(CONFIG),4096);if !value.is_object(){value=json!({"enabled":true});}
        value["max_processes"]=json!(self.max_processes);value["max_private_mib"]=json!(self.max_private_mib);value["interval_seconds"]=json!(self.interval_seconds);
        let pending=folder.join(format!("guard-limits.{}.pending",std::process::id()));fs::write(&pending,serde_json::to_vec(&value)?)?;fs::rename(pending,folder.join(CONFIG))?;Ok(())
    }
}
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
            "storage_restored" | "storage_recovered" => (
                if crate::locale::chinese() {"防护文件已恢复"} else {"Protection storage recovered"}.into(),
                if crate::locale::chinese() {"继续使用最近有效设置与累计计数；进程检查未因写入错误停用"} else {"Continuing checks with last valid settings and counters"}.into(), false),
            "storm_started" => (format!("Git storm #{} detected", value["episode"].as_u64().unwrap_or(0)),
                format!("{} Git · {:.0} MiB · protection activated", value["count"].as_u64().unwrap_or(0), value["private_bytes"].as_u64().unwrap_or(0) as f64 / 1048576.), true),
            "storm_recovered" => ("Storm recovered".into(), "Below limits for 60 seconds".into(), false),
            "limit_exceeded" => ("Limit exceeded".into(), format!("{} Git · {:.0} MiB · {}", value["count"].as_u64().unwrap_or(0), value["private_bytes"].as_u64().unwrap_or(0) as f64 / 1048576., value["reason"].as_str().unwrap_or("")), true),
            "termination_requested" => (format!("Ending read-only Git · PID {pid}"), format!("{} · {:.0} MiB · Codex PID {}", value["verb"].as_str().unwrap_or("query"), value["private_bytes"].as_u64().unwrap_or(0) as f64 / 1048576., value["codex_pid"].as_u64().unwrap_or(0)), false),
            "termination_confirmed" => (format!("Git ended · PID {pid}"),"Asynchronous exit confirmed".into(),false),
            "storm_context" => {
                let source=&value["sources"][0];let context=&source["context"];
                let session=context["session_title"].as_str().filter(|s|!s.is_empty()).or_else(||context["session_id"].as_str().filter(|s|!s.is_empty()));
                let title=if crate::locale::chinese(){"Git 来源线索"}else{"Git source evidence"};
                let detail=format!("Codex PID {} → Git PID {} · {} · {} · {}",
                    source["codex_pid"].as_u64().unwrap_or(0),source["git_pid"].as_u64().unwrap_or(0),
                    if crate::locale::chinese(){format!("近 60 秒 ≥ {} 个新 Git",value["observed_new_git"].as_u64().unwrap_or(0))}else{format!("≥ {} new Git / 60s",value["observed_new_git"].as_u64().unwrap_or(0))},
                    session.unwrap_or(if crate::locale::chinese(){"会话未确认"}else{"Chat unconfirmed"}),
                    context["cwd"].as_str().unwrap_or("").chars().take(160).collect::<String>());
                (title.into(),detail,false)
            }
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
        value = json!({"max_processes":16,"max_private_mib":1024,"interval_seconds":5});
    }
    value["enabled"] = json!(enabled);
    let pending = dir.join(format!("guard-control.{}.pending", std::process::id()));
    fs::write(&pending, serde_json::to_vec(&value)?)?;
    fs::rename(pending, dir.join(CONFIG))?;
    Ok(())
}
fn snapshot(paths: &Paths) -> View {
    let dir = dir(paths);
    let cfg = read_json(&dir.join(CONFIG), 4096);
    let status = read_json(&dir.join(STATUS), 16384);
    let enabled = cfg["enabled"].as_bool().unwrap_or(true);
    let freshness=status["interval_seconds"].as_u64().unwrap_or(5).clamp(1,60)*2+5;
    let fresh = status["version"] == 1
        && status["pid"].as_u64() == Some(std::process::id() as u64)
        && status["updated_at"]
            .as_f64()
            .is_some_and(|at| (0.0..=freshness as f64).contains(&(now() - at)));
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
            status["error"].as_str().unwrap_or("check Git protection logs")
        )
    } else if fresh {
        "Applying change…".into()
    } else {
        "Git protection is starting or unavailable".into()
    };
    View {
        enabled,
        available,
        count: status["count"].as_u64().unwrap_or(0) as usize,
        private_bytes: status["private_bytes"].as_u64().unwrap_or(0),
        terminated: status["terminated"].as_u64().unwrap_or(0) as usize,
        max_processes: cfg["max_processes"].as_u64().unwrap_or(16) as usize,
        max_private_mib: cfg["max_private_mib"].as_u64().unwrap_or(1024),
        interval_seconds: status["interval_seconds"].as_u64().unwrap_or(5),
        note,
        storm_count: status["storm_count"].as_u64(),
        storm_active: status["storm_active"].as_bool().unwrap_or(false),
        tracking_since: status["tracking_since"].as_str().unwrap_or("").to_string(),
    }
}
pub fn start(paths: &Paths, state: &State, stop: &Stop) -> thread::JoinHandle<()> {
    let (paths, state, stop) = (paths.clone(), state.clone(), stop.clone());
    thread::spawn(move || {
        let folder = dir(&paths);
        let (tx, rx) = std::sync::mpsc::sync_channel(32);
        let _enforcement = worker::Runner::start(folder.clone(), tx);
        let mut refreshed: Option<Instant> = None;
        while !stop.load(Ordering::Relaxed) {
            if refreshed.is_none_or(|at| at.elapsed() >= Duration::from_secs(1)) {
                state.lock().unwrap().guard = snapshot(&paths);
                refreshed = Some(Instant::now());
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(worker::Event::Alert(message)) => notice::show(&folder, &message),
                Ok(worker::Event::Update(message)) => notice::update(&folder, &message),
                Ok(worker::Event::Status(note)) => { if !note.is_empty() { refreshed = None; } },
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {},
            }
        }
    })
}

#[cfg(test)]
pub(crate) fn test_workspace() -> PathBuf {
    let root = std::env::var_os("CODEX_SESSION_BAR_WORKSPACE").map(PathBuf::from)
        .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors().find(|p| p.file_name().is_some_and(|n| n == "tools"))
            .and_then(|p| p.parent()).unwrap_or(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))).to_path_buf());
    root.canonicalize().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_settings_and_counts_are_copied_without_overwriting_new_settings() {
        let root=test_workspace().join("tmp").canonicalize().unwrap();
        let fixture=root.join(format!("guard-migration-{}",std::process::id()));
        let legacy=fixture.join("old");let target=fixture.join("new/git-guard");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join(CONFIG),br#"{"enabled":false,"max_processes":24}"#).unwrap();
        fs::write(legacy.join(STATUS),br#"{"storm_count":20,"terminated":51013}"#).unwrap();
        migrate_legacy(&legacy,&target).unwrap();
        assert_eq!(read_json(&target.join(CONFIG),4096)["max_processes"],24);
        assert!(legacy.join(CONFIG).is_file());
        fs::write(legacy.join(CONFIG),br#"{"max_processes":64}"#).unwrap();
        migrate_legacy(&legacy,&target).unwrap();
        assert_eq!(read_json(&target.join(CONFIG),4096)["max_processes"],24);
        fs::remove_dir_all(fixture).unwrap();
    }
    #[test]
    fn integrated_worker_preserves_preferences_and_rejects_companion_status() {
        let temp = test_workspace().join("tmp").canonicalize().unwrap();
        let folder = temp.join(format!("guard-integrated-test-{}", std::process::id()));
        let paths = Paths { app: folder.clone(), home: folder.clone(), runtime: folder.join("runtime"), instance: "test".into() };
        fs::create_dir_all(dir(&paths)).unwrap();
        let config = json!({"enabled":false,"max_processes":31,"max_private_mib":1024,"interval_seconds":1});
        fs::write(dir(&paths).join(CONFIG), config.to_string()).unwrap();
        fs::write(dir(&paths).join("codex-git-guard-counters.json"), json!({
            "version":1,"tracked_since":"2026-10-20T00:00:00Z","storms":20,"active":false,
            "terminated":51013,"terminated_at_start":50855
        }).to_string()).unwrap();
        fs::write(dir(&paths).join(STATUS), json!({"version":1,"pid":0,"updated_at":now(),"healthy":true,"enabled":false}).to_string()).unwrap();
        assert!(!snapshot(&paths).available, "Old standalone heartbeat cannot report integrated protection as healthy");
        let state = std::sync::Arc::new(std::sync::Mutex::new(crate::Shared::default()));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker = start(&paths, &state, &stop);
        let deadline = Instant::now() + Duration::from_secs(4);
        while !state.lock().unwrap().guard.available && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let view = state.lock().unwrap().guard.clone();
        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        assert!(view.available, "{}", view.note);
        assert!(!view.enabled);
        assert_eq!(view.max_processes, 31);
        assert_eq!(view.storm_count, Some(20));
        assert_eq!(view.terminated, 51013);
        assert_eq!(read_json(&dir(&paths).join(CONFIG),4096), config);
        assert!(!dir(&paths).join("samples.jsonl").exists());
        assert!(!dir(&paths).join("latest.json").exists());
        let status=read_json(&dir(&paths).join(STATUS),16384);
        assert_eq!(status["pid"], std::process::id());
        let updated = status["updated_at"].clone();
        thread::sleep(Duration::from_millis(1100));
        assert_eq!(read_json(&dir(&paths).join(STATUS),16384)["updated_at"], updated, "Worker stops with Session Bar");
        fs::remove_dir_all(folder).unwrap();
    }
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
