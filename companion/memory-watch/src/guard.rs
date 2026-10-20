//! Reactive protection for verified Codex-owned, read-only Git subprocesses.
//! Process names alone never authorize termination; identity is checked again
//! using the same open handle that is passed to TerminateProcess.
use crate::collect::{Collector, Process, Snapshot};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::Write,
    mem::size_of,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use windows::{
    core::{s, w, PCWSTR, PWSTR},
    Win32::{
        Foundation::*,
        Security::*,
        System::{LibraryLoader::*, RemoteDesktop::ProcessIdToSessionId, Threading::*},
        UI::Shell::CommandLineToArgvW,
    },
};

pub const CONFIG: &str = "codex-git-guard-settings.json";
pub const STATUS: &str = "codex-git-guard-status.json";
const COUNTERS: &str = "codex-git-guard-counters.json";
const MIB: u64 = 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub enabled: bool,
    pub max_processes: usize,
    pub max_private_mib: u64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            max_processes: 64,
            max_private_mib: 4096,
        }
    }
}
pub fn read_config(dir: &Path) -> Result<Config> {
    let file = dir.join(CONFIG);
    if !file.exists() {
        let cfg = Config::default();
        atomic(dir, CONFIG, &cfg)?;
        return Ok(cfg);
    }
    ensure!(
        fs::metadata(&file)?.len() <= 4096,
        "Guard settings too large"
    );
    let cfg: Config = serde_json::from_slice(&fs::read(file)?)?;
    ensure!(
        (8..=4096).contains(&cfg.max_processes) && (256..=65536).contains(&cfg.max_private_mib),
        "Invalid Git guard limits"
    );
    Ok(cfg)
}
fn atomic(dir: &Path, name: &str, data: &impl Serialize) -> Result<()> {
    let pending = dir.join(format!("{name}.{}.pending", std::process::id()));
    fs::write(&pending, serde_json::to_vec(data)?)?;
    fs::rename(pending, dir.join(name))?;
    Ok(())
}
#[derive(Clone, Default, Serialize)]
pub struct Status {
    pub version: u32,
    pub pid: u32,
    pub updated_at: f64,
    pub enabled: bool,
    pub healthy: bool,
    pub count: usize,
    pub private_bytes: u64,
    pub eligible: usize,
    pub unknown_git: usize,
    pub max_processes: usize,
    pub max_private_mib: u64,
    pub terminated: usize,
    pub last_alert: String,
    pub error: String,
    pub check_ms: f64,
    pub interval_seconds: u64,
    pub storm_count: u64,
    pub storm_active: bool,
    pub tracking_since: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct Incidents {
    version: u32,
    tracked_since: String,
    storms: u64,
    active: bool,
    terminated: usize,
    #[serde(skip)]
    calm_since: Option<f64>,
    #[serde(skip)]
    last_observation: Option<f64>,
}
impl Incidents {
    fn load(dir: &Path) -> Result<Self> {
        let file = dir.join(COUNTERS);
        if file.exists() {
            ensure!(fs::metadata(&file)?.len() <= 16384, "Guard counters too large");
            let saved: Self = serde_json::from_slice(&fs::read(file)?)?;
            ensure!(saved.version == 1, "Unsupported guard counters");
            return Ok(saved);
        }
        // Preserve the known process total on upgrade; historical alarm logs
        // cannot reconstruct exact episode counts and must not be guessed.
        let old = dir.join(STATUS);
        let previous = if fs::metadata(&old).is_ok_and(|m| m.len() <= 16384) {
            serde_json::from_slice::<serde_json::Value>(&fs::read(old)?).unwrap_or_default()
        } else { serde_json::Value::Null };
        let initial = Self {
            version: 1, tracked_since: chrono::Local::now().to_rfc3339(), storms: 0,
            active: false, terminated: previous["terminated"].as_u64().unwrap_or(0) as usize,
            calm_since: None, last_observation: None,
        };
        initial.save(dir)?;
        Ok(initial)
    }
    fn save(&self, dir: &Path) -> Result<()> {
        let pending = dir.join(format!("{COUNTERS}.{}.pending", std::process::id()));
        let mut file = OpenOptions::new().create(true).truncate(true).write(true).open(&pending)?;
        serde_json::to_writer(&mut file, self)?;
        file.sync_data()?;
        drop(file);
        fs::rename(pending, dir.join(COUNTERS))?;
        Ok(())
    }
    fn pause(&mut self) {
        self.calm_since = None;
        self.last_observation = None;
    }
    fn observe(&mut self, over: bool, at: f64) -> Option<&'static str> {
        if self.last_observation.is_some_and(|last| !(0.0..=15.0).contains(&(at - last))) {
            self.calm_since = None;
        }
        self.last_observation = Some(at);
        if over {
            self.calm_since = None;
            if !self.active {
                self.active = true;
                self.storms += 1;
                return Some("storm_started");
            }
        } else if self.active {
            let start = *self.calm_since.get_or_insert(at);
            if at - start >= 60.0 {
                self.active = false;
                self.calm_since = None;
                return Some("storm_recovered");
            }
        }
        None
    }
}
fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
unsafe fn created(h: HANDLE) -> Result<i64> {
    let (mut a, mut b, mut c, mut d) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    GetProcessTimes(h, &mut a, &mut b, &mut c, &mut d)?;
    Ok(((a.dwHighDateTime as u64) << 32 | a.dwLowDateTime as u64) as i64)
}
unsafe fn image(h: HANDLE) -> Result<String> {
    let mut path = [0u16; 4096];
    let mut n = path.len() as u32;
    QueryFullProcessImageNameW(h, PROCESS_NAME_FORMAT(0), PWSTR(path.as_mut_ptr()), &mut n)?;
    Ok(String::from_utf16_lossy(&path[..n as usize]))
}
unsafe fn owner(h: HANDLE) -> Result<Vec<u8>> {
    let mut token = HANDLE::default();
    OpenProcessToken(h, TOKEN_QUERY, &mut token)?;
    let token = Handle(token);
    let mut required = 0;
    let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut required);
    ensure!(required > 0 && required < 65536, "Invalid token size");
    let mut buf = vec![0u64; (required as usize + 7) / 8];
    GetTokenInformation(
        token.0,
        TokenUser,
        Some(buf.as_mut_ptr() as *mut _),
        required,
        &mut required,
    )?;
    let sid = (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid;
    let n = GetLengthSid(sid) as usize;
    ensure!((8..=128).contains(&n), "Invalid SID");
    Ok(std::slice::from_raw_parts(sid.0 as *const u8, n).to_vec())
}
#[repr(C)]
struct UString {
    len: u16,
    max: u16,
    ptr: *const u16,
}
unsafe fn command(h: HANDLE) -> Result<Vec<String>> {
    type Query =
        unsafe extern "system" fn(HANDLE, u32, *mut std::ffi::c_void, u32, *mut u32) -> i32;
    let address = GetProcAddress(
        GetModuleHandleW(w!("ntdll.dll"))?,
        s!("NtQueryInformationProcess"),
    )
    .context("Native query unavailable")?;
    let query: Query = std::mem::transmute(address);
    let mut buffer = vec![0u64; 8192];
    let mut needed = 0;
    ensure!(
        query(h, 60, buffer.as_mut_ptr() as *mut _, 65536, &mut needed) >= 0,
        "Git command is unavailable"
    );
    let u = &*(buffer.as_ptr() as *const UString);
    let base = buffer.as_ptr() as usize;
    let ptr = u.ptr as usize;
    ensure!(
        u.len % 2 == 0
            && u.len > 0
            && ptr >= base + size_of::<UString>()
            && ptr
                .checked_add(u.len as usize)
                .is_some_and(|end| end <= base + 65536),
        "Invalid native command bounds"
    );
    let mut text = std::slice::from_raw_parts(u.ptr, u.len as usize / 2).to_vec();
    text.push(0);
    let mut argc = 0;
    let argv = CommandLineToArgvW(PCWSTR(text.as_ptr()), &mut argc);
    ensure!(!argv.is_null(), "Invalid Git arguments");
    let result = (|| {
        ensure!((1..=512).contains(&argc), "Too many Git arguments");
        std::slice::from_raw_parts(argv, argc as usize)
            .iter()
            .map(|arg| arg.to_string().map_err(Into::into))
            .collect::<Result<Vec<_>>>()
    })();
    let _ = LocalFree(HLOCAL(argv as *mut _));
    result
}
fn read_verb(args: &[String]) -> Option<String> {
    let mut i = 1;
    while let Some(arg) = args.get(i) {
        if matches!(
            arg.as_str(),
            "-c" | "-C" | "--git-dir" | "--work-tree" | "--namespace" | "--config-env"
        ) {
            args.get(i + 1)?;
            i += 2;
            continue;
        }
        if [
            "--git-dir=",
            "--work-tree=",
            "--namespace=",
            "--config-env=",
        ]
        .iter()
        .any(|p| arg.starts_with(p))
            || matches!(
                arg.as_str(),
                "--no-pager"
                    | "--no-optional-locks"
                    | "--literal-pathspecs"
                    | "--no-replace-objects"
                    | "--no-lazy-fetch"
                    | "--no-advice"
                    | "-P"
            )
        {
            i += 1;
            continue;
        }
        return matches!(
            arg.as_str(),
            "diff"
                | "status"
                | "log"
                | "show"
                | "rev-parse"
                | "ls-files"
                | "ls-tree"
                | "cat-file"
                | "merge-base"
                | "for-each-ref"
                | "check-ignore"
        )
        .then(|| arg.clone());
    }
    None
}
fn codex_path(path: &str, local: &str, user: &str) -> bool {
    let p = path
        .replace('/', "\\")
        .trim_start_matches(r"\\?\")
        .to_lowercase();
    if !p.ends_with("\\codex.exe") {
        return false;
    }
    let local = local.replace('/', "\\").to_lowercase();
    let user = user.replace('/', "\\").to_lowercase();
    (!local.is_empty() && p.starts_with(&format!("{local}\\openai\\codex\\")))
        || (!user.is_empty()
            && p.starts_with(&format!("{user}\\.vscode\\extensions\\openai.chatgpt-"))
            && p.contains("\\bin\\windows-"))
        || p.starts_with("c:\\program files\\windowsapps\\openai.codex_")
}
struct Inspector {
    cache: HashMap<(u32, i64), String>,
    verbs: HashMap<(u32, i64), Option<String>>,
    user: Vec<u8>,
    session: u32,
    local: String,
    home: String,
}
impl Inspector {
    fn new() -> Result<Self> {
        unsafe {
            let mut session = 0;
            ProcessIdToSessionId(GetCurrentProcessId(), &mut session)?;
            Ok(Self {
                cache: HashMap::new(),
                verbs: HashMap::new(),
                user: owner(GetCurrentProcess())?,
                session,
                local: std::env::var("LOCALAPPDATA").unwrap_or_default(),
                home: std::env::var("USERPROFILE").unwrap_or_default(),
            })
        }
    }
    fn path(&mut self, p: &Process) -> Option<String> {
        if let Some(path) = self.cache.get(&(p.pid, p.created_ticks)) {
            return Some(path.clone());
        }
        let value = unsafe {
            let h = Handle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, p.pid).ok()?);
            if created(h.0).ok()? != p.created_ticks || owner(h.0).ok()? != self.user {
                return None;
            }
            let mut session = 0;
            ProcessIdToSessionId(p.pid, &mut session).ok()?;
            if session != self.session {
                return None;
            }
            image(h.0).ok()?
        };
        self.cache.insert((p.pid, p.created_ticks), value.clone());
        Some(value)
    }
    fn origin(&mut self, p: &Process, all: &HashMap<u32, &Process>) -> Option<(u32, i64)> {
        let mut current = p;
        let mut visited = HashSet::new();
        for _ in 0..32 {
            if !visited.insert(current.pid) {
                return None;
            }
            let parent = *all.get(&current.parent_pid)?;
            if parent.created_ticks > current.created_ticks || parent.created_ticks <= 0 {
                return None;
            }
            if parent.name.eq_ignore_ascii_case("codex.exe") {
                let path = self.path(parent)?;
                if codex_path(&path, &self.local, &self.home) {
                    return Some((parent.pid, parent.created_ticks));
                }
            }
            current = parent;
        }
        None
    }
    fn candidates(&mut self, snapshot: &Snapshot) -> (Vec<Candidate>, usize) {
        let all: HashMap<_, _> = snapshot.processes.iter().map(|p| (p.pid, p)).collect();
        self.cache
            .retain(|(pid, birth), _| all.get(pid).is_some_and(|p| p.created_ticks == *birth));
        self.verbs
            .retain(|(pid, birth), _| all.get(pid).is_some_and(|p| p.created_ticks == *birth));
        let mut candidates = Vec::new();
        let mut unknown = 0;
        for p in snapshot
            .processes
            .iter()
            .filter(|p| p.name.eq_ignore_ascii_case("git.exe"))
        {
            let Some(origin) = self.origin(p, &all) else {
                unknown += 1;
                continue;
            };
            let Some(path) = self.path(p).filter(|s| {
                Path::new(s)
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("git.exe"))
            }) else {
                unknown += 1;
                continue;
            };
            let key = (p.pid, p.created_ticks);
            let verb = if let Some(verb) = self.verbs.get(&key) {
                verb.clone()
            } else {
                unsafe {
                    OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, p.pid)
                        .ok()
                        .map(Handle)
                        .and_then(|h| {
                            if created(h.0).ok() == Some(p.created_ticks) {
                                command(h.0).ok().and_then(|a| read_verb(&a))
                            } else {
                                None
                            }
                        })
                }
            };
            self.verbs.insert(key, verb.clone());
            candidates.push(Candidate {
                process: p.clone(),
                origin,
                path,
                verb,
            });
        }
        (candidates, unknown)
    }
}
#[derive(Clone)]
struct Candidate {
    process: Process,
    origin: (u32, i64),
    path: String,
    verb: Option<String>,
}
fn plan<'a>(items: &'a [Candidate], cfg: &Config) -> Vec<&'a Candidate> {
    let mut count = items.len();
    let mut bytes = items.iter().map(|c| c.process.private).sum::<u64>();
    let count_over = count > cfg.max_processes;
    let mem_over = bytes > cfg.max_private_mib * MIB;
    if !count_over && !mem_over {
        return Vec::new();
    }
    let target_count = cfg.max_processes * 4 / 5;
    let target_bytes = cfg.max_private_mib * MIB * 4 / 5;
    let mut eligible: Vec<_> = items.iter().filter(|c| c.verb.is_some()).collect();
    eligible.sort_by_key(|c| {
        (
            std::cmp::Reverse(c.process.private),
            c.process.created_ticks,
        )
    });
    let mut selected = Vec::new();
    for c in eligible {
        if (!count_over || count <= target_count) && (!mem_over || bytes <= target_bytes) {
            break;
        }
        selected.push(c);
        count = count.saturating_sub(1);
        bytes = bytes.saturating_sub(c.process.private);
        if selected.len() == 512 {
            break;
        }
    }
    selected
}
pub struct Guard {
    dir: PathBuf,
    collector: Collector,
    inspector: Inspector,
    next: Instant,
    streak: u32,
    last_popup: Option<Instant>,
    status: Status,
    incidents: Incidents,
    #[cfg(test)]
    scope: Option<HashSet<(u32, i64)>>,
}
impl Guard {
    pub fn new(dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&dir)?;
        let incidents = Incidents::load(&dir)?;
        Ok(Self {
            dir,
            collector: Collector::new(),
            inspector: Inspector::new()?,
            next: Instant::now(),
            streak: 0,
            last_popup: None,
            status: Status { terminated: incidents.terminated, storm_count: incidents.storms,
                storm_active: incidents.active, tracking_since: incidents.tracked_since.clone(), ..Status::default() },
            incidents,
            #[cfg(test)]
            scope: None,
        })
    }
    pub fn wait(&self) -> Duration {
        self.next.saturating_duration_since(Instant::now())
    }
    fn event(&self, value: serde_json::Value) -> Result<()> {
        let path = self.dir.join("codex-git-guard-events.jsonl");
        let closed = self.dir.join("codex-git-guard-events.1.jsonl");
        if closed.exists() {
            crate::recorder::Recorder::new(self.dir.clone()).compress(&closed)?;
        }
        if fs::metadata(&path).is_ok_and(|m| m.len() >= 1024 * 1024) {
            for i in (1..=4).rev() {
                let old = self
                    .dir
                    .join(format!("codex-git-guard-events.{i}.jsonl.gz"));
                if old.exists() {
                    fs::rename(
                        &old,
                        self.dir
                            .join(format!("codex-git-guard-events.{}.jsonl.gz", i + 1)),
                    )?;
                }
            }
            fs::rename(&path, &closed)?;
            crate::recorder::Recorder::new(self.dir.clone()).compress(&closed)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        serde_json::to_writer(&mut file, &value)?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        Ok(())
    }
    fn record_observation(&mut self, over: bool) -> Result<()> {
        let mut next = self.incidents.clone();
        let event = next.observe(over, now());
        if let Some(kind) = event {
            next.save(&self.dir)?;
            self.event(json!({"type":kind,"time":chrono::Local::now().to_rfc3339(),
                "episode":next.storms,"count":self.status.count,"private_bytes":self.status.private_bytes,
                "stable_seconds":60}))?;
        }
        self.incidents = next;
        self.status.storm_count = self.incidents.storms;
        self.status.storm_active = self.incidents.active;
        Ok(())
    }
    pub fn poll(&mut self) -> (Option<String>, String) {
        if Instant::now() < self.next {
            return (None, String::new());
        }
        self.next = Instant::now() + Duration::from_secs(5);
        let started = Instant::now();
        let mut alert = None;
        if let Err(e) = self.check(&mut alert) {
            self.incidents.pause();
            self.status.healthy = false;
            self.status.error = format!("{e:#}");
            self.status.last_alert = format!("Codex Git 防护失败：{e:#}。请检查内存监测程序。 ");
            if self
                .last_popup
                .is_none_or(|at| at.elapsed() > Duration::from_secs(60))
            {
                alert = Some(self.status.last_alert.clone());
                self.last_popup = Some(Instant::now());
            }
        }
        self.status.updated_at = now();
        self.status.interval_seconds = 5;
        self.status.check_ms = started.elapsed().as_secs_f64() * 1000.;
        self.status.version = 1;
        self.status.pid = std::process::id();
        if let Err(e) = atomic(&self.dir, STATUS, &self.status) {
            alert = Some(format!("Codex Git 防护无法写入状态：{e}"));
        }
        let summary = if !self.status.healthy {
            format!("Codex Git 防护异常：{}", self.status.error)
        } else if !self.status.enabled {
            "Codex Git 防护已关闭".into()
        } else {
            format!(
                "Codex Git 防护已开启 · {} 个 · {:.0} MiB · 风暴 {} 次 · 已结束 {} 个进程",
                self.status.count,
                self.status.private_bytes as f64 / MIB as f64,
                self.status.storm_count,
                self.status.terminated
            )
        };
        (alert, summary)
    }
    fn check(&mut self, alert: &mut Option<String>) -> Result<()> {
        let cfg = read_config(&self.dir)?;
        self.status.enabled = cfg.enabled;
        self.status.max_processes = cfg.max_processes;
        self.status.max_private_mib = cfg.max_private_mib;
        self.status.healthy = true;
        self.status.error.clear();
        if !cfg.enabled {
            self.streak = 0;
            self.incidents.pause();
            return Ok(());
        }
        let snapshot = self.collector.sample(1)?;
        let (items, unknown) = self.inspector.candidates(&snapshot);
        #[cfg(test)]
        let items: Vec<_> = items
            .into_iter()
            .filter(|c| {
                self.scope
                    .as_ref()
                    .is_none_or(|s| s.contains(&(c.process.pid, c.process.created_ticks)))
            })
            .collect();
        self.status.count = items.len();
        self.status.private_bytes = items.iter().map(|c| c.process.private).sum();
        self.status.eligible = items.iter().filter(|c| c.verb.is_some()).count();
        self.status.unknown_git = unknown;
        let over = items.len() > cfg.max_processes
            || self.status.private_bytes > cfg.max_private_mib * MIB;
        self.streak = if over { self.streak + 1 } else { 0 };
        if !over {
            self.record_observation(false)?;
            return Ok(());
        }
        // Re-sample before acting. PID/creation-time, ancestry, image, owner and
        // read-only command are revalidated on fresh data and an open handle.
        let fresh = self.collector.sample(1)?;
        let mut inspector = Inspector::new()?;
        let (fresh_items, _) = inspector.candidates(&fresh);
        #[cfg(test)]
        let fresh_items: Vec<_> = fresh_items
            .into_iter()
            .filter(|c| {
                self.scope
                    .as_ref()
                    .is_none_or(|s| s.contains(&(c.process.pid, c.process.created_ticks)))
            })
            .collect();
        let victims = plan(&fresh_items, &cfg);
        if victims.is_empty()
            && fresh_items.len() <= cfg.max_processes
            && fresh_items.iter().map(|c| c.process.private).sum::<u64>()
                <= cfg.max_private_mib * MIB
        {
            self.streak = 0;
            self.record_observation(false)?;
            return Ok(());
        }
        self.record_observation(true)?;
        let reason = if self.status.count > cfg.max_processes {
            "process_limit"
        } else {
            "private_memory_limit"
        };
        let popup_due = self
            .last_popup
            .is_none_or(|at| at.elapsed() > Duration::from_secs(60));
        if popup_due {
            self.event(json!({"time":chrono::Local::now().to_rfc3339(),"type":"limit_exceeded","reason":reason,"count":self.status.count,"private_bytes":self.status.private_bytes,"limits":cfg,"eligible":victims.len()}))?;
        }
        let mut ended = 0;
        let mut pending = 0;
        let mut failed = 0;
        let all: HashMap<_, _> = fresh.processes.iter().map(|p| (p.pid, p)).collect();
        for victim in victims {
            if !read_config(&self.dir)?.enabled {
                break;
            }
            let result = unsafe {
                (|| -> Result<bool> {
                    let h = Handle(OpenProcess(
                        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
                        false,
                        victim.process.pid,
                    )?);
                    ensure!(
                        created(h.0)? == victim.process.created_ticks
                            && image(h.0)?.eq_ignore_ascii_case(&victim.path)
                            && owner(h.0)? == inspector.user,
                        "Process identity changed"
                    );
                    ensure!(
                        inspector.origin(&victim.process, &all) == Some(victim.origin),
                        "Codex ancestry changed"
                    );
                    ensure!(
                        read_verb(&command(h.0)?).is_some(),
                        "Write operation or unknown Git command protected"
                    );
                    self.event(json!({"time":chrono::Local::now().to_rfc3339(),"type":"termination_requested","reason":reason,"pid":victim.process.pid,"created_ticks":victim.process.created_ticks,"codex_pid":victim.origin.0,"codex_created_ticks":victim.origin.1,"image":victim.path,"verb":victim.verb,"private_bytes":victim.process.private}))?;
                    TerminateProcess(h.0, 0xe0000001)?;
                    Ok(WaitForSingleObject(h.0, 20) == WAIT_OBJECT_0)
                })()
            };
            let outcome = match result {
                Ok(true) => {
                    ended += 1;
                    "terminated".to_owned()
                }
                Ok(false) => {
                    pending += 1;
                    "termination_pending".to_owned()
                }
                Err(e) => {
                    failed += 1;
                    format!("skipped: {e:#}")
                }
            };
            self.event(json!({"time":chrono::Local::now().to_rfc3339(),"type":"termination_result","pid":victim.process.pid,"created_ticks":victim.process.created_ticks,"result":outcome}))?;
        }
        self.status.terminated += ended;
        self.incidents.terminated = self.status.terminated;
        self.incidents.save(&self.dir)?;
        self.status.last_alert=format!("Codex Git 超限：{} 个 / {:.0} MiB（上限 {} 个 / {} MiB）。已结束 {} 个，等待退出 {} 个，跳过 {} 个。写操作及无法确认归属的进程不会结束。详见 codex-git-guard-events.jsonl。",self.status.count,self.status.private_bytes as f64/MIB as f64,cfg.max_processes,cfg.max_private_mib,ended,pending,failed);
        if popup_due {
            *alert = Some(self.status.last_alert.clone());
            self.last_popup = Some(Instant::now());
        }
        self.next = Instant::now() + Duration::from_secs(5);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn storm_counter_requires_a_stable_minute_and_survives_serialization() {
        let mut counter = Incidents { version:1, tracked_since:"2026-10-20T00:00:00Z".into(),
            storms:0, active:false, terminated:9592, calm_since:None, last_observation:None };
        assert_eq!(counter.observe(true, 100.), Some("storm_started"));
        for at in [105., 110., 115., 120.] { assert!(counter.observe(true, at).is_none()); }
        assert_eq!(counter.storms, 1);
        assert!(counter.observe(false, 125.).is_none());
        assert!(counter.observe(true, 130.).is_none());
        for at in (135..195).step_by(5) { assert!(counter.observe(false, at as f64).is_none()); }
        assert_eq!(counter.observe(false, 195.), Some("storm_recovered"));
        assert_eq!(counter.observe(true, 200.), Some("storm_started"));
        assert_eq!(counter.storms, 2);
        counter.observe(false, 205.);
        // An unobserved gap is not evidence of a stable minute.
        assert!(counter.observe(false, 300.).is_none());
        assert!(counter.active);
        let mut restored: Incidents = serde_json::from_slice(&serde_json::to_vec(&counter).unwrap()).unwrap();
        assert_eq!(restored.terminated, 9592);
        assert_eq!(restored.storms, 2);
        assert!(restored.observe(true, 400.).is_none());
        assert_eq!(restored.storms, 2);
        restored.pause();
        assert!(restored.observe(false, 500.).is_none());
        assert!(restored.active);
    }
    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }
    #[test]
    fn only_explicit_read_commands_are_eligible() {
        assert_eq!(
            read_verb(&args(&[
                "git",
                "-c",
                "a=b",
                "-C",
                "path",
                "diff",
                "--no-index",
                "NUL",
                "x"
            ]))
            .as_deref(),
            Some("diff")
        );
        for verb in [
            "commit", "push", "clone", "fetch", "checkout", "reset", "clean", "gc", "config", "-x",
        ] {
            assert!(read_verb(&args(&["git", verb])).is_none());
        }
        assert!(read_verb(&args(&["git", "-c"])).is_none());
    }
    #[test]
    fn codex_name_without_install_identity_is_rejected() {
        let local = r"C:\Users\user\AppData\Local";
        let home = r"C:\Users\user";
        assert!(codex_path(
            &format!(r"{local}\OpenAI\Codex\bin\abc\codex.exe"),
            local,
            home
        ));
        assert!(!codex_path(r"C:\Temp\codex.exe", local, home));
        assert!(!codex_path(r"C:\Temp\not-codex.exe", local, home));
    }
    #[test]
    fn victims_use_hysteresis_and_exclude_writers() {
        let cfg = Config {
            max_processes: 8,
            max_private_mib: 256,
            ..Default::default()
        };
        let items: Vec<_> = (0..12)
            .map(|i| Candidate {
                process: Process {
                    pid: i,
                    private: 10 * MIB,
                    created_ticks: i as i64,
                    ..Default::default()
                },
                origin: (99, 1),
                path: "git.exe".into(),
                verb: if i == 0 { None } else { Some("diff".into()) },
            })
            .collect();
        let selected = plan(&items, &cfg);
        assert_eq!(selected.len(), 6);
        assert!(selected.iter().all(|c| c.process.pid != 0));
        assert!(plan(&items[..4], &cfg).is_empty());
        let mut high = items[..2].to_vec();
        high[1].process.private = 300 * MIB;
        assert_eq!(plan(&high, &cfg)[0].process.pid, 1);
    }
    #[test]
    fn native_identity_and_command_query_are_read_only() {
        unsafe {
            assert!(created(GetCurrentProcess()).unwrap() > 0);
            assert!(!owner(GetCurrentProcess()).unwrap().is_empty());
            assert!(!command(GetCurrentProcess()).unwrap().is_empty());
        }
    }
    #[test]
    fn ancestry_rejects_reused_parents_or_non_codex_names() {
        let mut inspector = Inspector::new().unwrap();
        let mut root = Process {
            pid: 999991,
            created_ticks: 10,
            name: "codex.exe".into(),
            ..Default::default()
        };
        let child = Process {
            pid: 999992,
            parent_pid: root.pid,
            created_ticks: 20,
            name: "git.exe".into(),
            ..Default::default()
        };
        inspector.cache.insert(
            (root.pid, root.created_ticks),
            format!("{}\\OpenAI\\Codex\\bin\\abc\\codex.exe", inspector.local),
        );
        assert_eq!(
            inspector.origin(&child, &HashMap::from([(root.pid, &root)])),
            Some((root.pid, 10))
        );
        root.created_ticks = 30;
        assert!(inspector
            .origin(&child, &HashMap::from([(root.pid, &root)]))
            .is_none());
        root.created_ticks = 10;
        root.name = "other.exe".into();
        assert!(inspector
            .origin(&child, &HashMap::from([(root.pid, &root)]))
            .is_none());
        assert!(inspector.origin(&child, &HashMap::new()).is_none());
        let own = Process {
            pid: std::process::id(),
            created_ticks: 1,
            ..Default::default()
        };
        assert!(inspector.path(&own).is_none());
    }
    #[test]
    #[ignore = "Controlled subprocess exercise; requires CODEX_GUARD_FIXTURE_EXE under workspace/tmp"]
    fn native_controlled_guard_terminates_only_owned_read_queries() {
        use std::{
            os::windows::process::CommandExt,
            process::{Child, Command, Stdio},
        };
        struct Children(Vec<Child>);
        impl Drop for Children {
            fn drop(&mut self) {
                for child in &mut self.0 {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
        let workspace = crate::workspace_root(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        let temp = workspace.join("tmp").canonicalize().unwrap();
        let fixture = PathBuf::from(
            std::env::var_os("CODEX_GUARD_FIXTURE_EXE").expect("Controlled fixture is required"),
        )
        .canonicalize()
        .unwrap();
        assert!(fixture.starts_with(&temp));
        assert_eq!(fixture.file_name().unwrap(), "git.exe");
        let dir = temp.join(format!("guard-native-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut children = Children(Vec::new());
        for i in 0..10 {
            children.0.push(
                Command::new(&fixture)
                    .arg(if i == 9 { "commit" } else { "diff" })
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .env("TEMP", &dir)
                    .env("TMP", &dir)
                    .env("TMPDIR", &dir)
                    .creation_flags(0x08000000)
                    .spawn()
                    .unwrap(),
            );
        }
        let mut guard = Guard::new(dir.clone()).unwrap();
        let owned: HashSet<u32> = children.0.iter().map(|c| c.id()).collect();
        let snapshot = guard.collector.sample(5).unwrap();
        guard.scope = Some(
            snapshot
                .processes
                .iter()
                .filter(|p| owned.contains(&p.pid))
                .map(|p| (p.pid, p.created_ticks))
                .collect(),
        );
        let mut config = Config {
            enabled: false,
            max_processes: 8,
            max_private_mib: 256,
        };
        atomic(&dir, CONFIG, &config).unwrap();
        guard.poll();
        assert_eq!(guard.status.terminated, 0);
        assert!(children
            .0
            .iter_mut()
            .all(|p| p.try_wait().unwrap().is_none()));
        config.enabled = true;
        atomic(&dir, CONFIG, &config).unwrap();
        guard.next = Instant::now();
        let (warning, _) = guard.poll();
        assert!(guard.status.healthy, "{}", guard.status.error);
        assert_eq!(
            guard.status.count, 10,
            "Controlled children must be traceable to the actual Codex ancestor"
        );
        assert_eq!(guard.status.terminated, 4);
        assert_eq!(guard.status.storm_count, 1);
        assert!(guard.status.storm_active);
        let restored = Guard::new(dir.clone()).unwrap();
        assert_eq!(restored.status.terminated, 4);
        assert_eq!(restored.status.storm_count, 1);
        assert!(warning.is_some());
        assert!(
            children.0[9].try_wait().unwrap().is_none(),
            "Commit fixture must remain alive"
        );
        let evidence = dir.join("codex-git-guard-events.jsonl");
        assert!(fs::metadata(&evidence).unwrap().len() < 65536);
        let events = fs::read_to_string(&evidence).unwrap();
        let actions: Vec<_> = events
            .lines()
            .map(|s| serde_json::from_str::<serde_json::Value>(s).unwrap())
            .filter(|v| v["type"] == "termination_result")
            .collect();
        assert_eq!(actions.len(), 4);
        assert!(actions
            .iter()
            .all(|v| owned.contains(&(v["pid"].as_u64().unwrap() as u32))));
        assert!(guard.wait() > Duration::from_secs(4));
    }
}
