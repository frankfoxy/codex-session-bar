//! Reactive protection for verified Codex-owned, read-only Git subprocesses.
//! Process names alone never authorize termination; identity is checked again
//! using the same open handle that is passed to TerminateProcess.
use super::collect::{Collector, Process, Snapshot};
use super::attribution;
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
fn effective_interval(enabled:bool,healthy:bool,active:bool,normal:u64)->u64 {
    if enabled&&(active||!healthy){1}else{normal}
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub enabled: bool,
    pub max_processes: usize,
    pub max_private_mib: u64,
    pub interval_seconds: u64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            max_processes: 16,
            max_private_mib: 1024,
            interval_seconds: 5,
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
        (8..=4096).contains(&cfg.max_processes) && (256..=65536).contains(&cfg.max_private_mib) && (1..=60).contains(&cfg.interval_seconds),
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
    pub pending_terminations: usize,
    pub storm_count: u64,
    pub storm_active: bool,
    pub tracking_since: String,
    pub observed_new_git_60s: usize,
    pub episode_terminated: usize,
    pub peak_count: usize,
}
#[derive(Clone, Serialize, Deserialize)]
struct Incidents {
    version: u32,
    tracked_since: String,
    storms: u64,
    active: bool,
    terminated: usize,
    #[serde(default)]
    terminated_at_start: usize,
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
            let value:serde_json::Value=serde_json::from_slice(&fs::read(file)?)?;
            let mut saved: Self = serde_json::from_value(value.clone())?;
            if value.get("terminated_at_start").is_none(){saved.terminated_at_start=saved.terminated;}
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
            terminated_at_start: 0, calm_since: None, last_observation: None,
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
                self.terminated_at_start = self.terminated;
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
        if selected.len() == 2048 {
            break;
        }
    }
    selected
}
#[derive(Clone, Default, Serialize)]
struct Source {
    git_pid: u32,
    codex_pid: u32,
    git_created_ticks: i64,
    codex_created_ticks: i64,
    image: String,
    command: Vec<String>,
    context: attribution::Context,
}
fn source(candidate: &Candidate) -> Source {
    let mut result=Source {git_pid:candidate.process.pid,codex_pid:candidate.origin.0,
        git_created_ticks:candidate.process.created_ticks,codex_created_ticks:candidate.origin.1,
        image:candidate.path.clone(),..Source::default()};
    // Context is optional and never used as permission to terminate a process.
    unsafe {if let Ok(raw)=OpenProcess(PROCESS_QUERY_INFORMATION|PROCESS_VM_READ,false,result.git_pid){
        let handle=Handle(raw);
        if created(handle.0).ok()==Some(result.git_created_ticks){
            result.context=attribution::process(handle.0);
            result.command=command(handle.0).unwrap_or_default().into_iter().take(40)
                .map(|arg|arg.chars().take(512).collect()).collect();
        }
    }}
    if let Some(home)=attribution::codex_home(){attribution::resolve(&mut result.context,&home);}
    result
}
fn warning(status:&Status,sources:&[Source],skipped:usize)->String {
    let pids=sources.iter().map(|s|format!("{} → {}",s.codex_pid,s.git_pid)).collect::<Vec<_>>().join("；");
    let context=sources.first().map(|s|&s.context);
    let session=match context {
        Some(c) if !c.session_id.is_empty()=>format!("{} ({}) [进程会话 ID]",c.session_title.chars().take(48).collect::<String>(),c.session_id),
        _=>"暂无法确认；Codex 后台可能由多个会话共用".into(),
    };
    let cwd=context.filter(|c|!c.cwd.is_empty()).map(|c|c.cwd.chars().take(160).collect::<String>()).unwrap_or_else(||"未能读取".into());
    let clue=context.and_then(|c|c.candidates.first()).map(|s|format!("\r\n目录匹配线索（非确认归属）：{s}")).unwrap_or_default();
    format!("Codex Git 保护：{}\r\n\r\n本次累计已确认结束 {} 个 Git；另有 {} 个已下达结束命令，等待确认。\r\n近 60 秒至少观察到 {} 个新 Git；当前 {} 个，本次峰值 {} 个。\r\n上限：{} 个 / 合计 {} MiB；当前合计 {:.0} MiB。\r\n来源 PID（Codex → Git 示例）：{}\r\n进程：git.exe；完整路径和命令已记录到日志。\r\n会话：{}\r\n工作目录：{}{}\r\n\r\n本次跳过 {} 次（自行退出或安全校验未通过）；写操作保留。\r\n提示窗口不阻塞防护；当前每 {} 秒继续检查。\r\n日志：codex-git-guard-events.jsonl",
        if status.storm_active{"正在处理 / 观察恢复"}else{"已恢复"},status.episode_terminated,status.pending_terminations,
        status.observed_new_git_60s,status.count,status.peak_count,status.max_processes,status.max_private_mib,
        status.private_bytes as f64/MIB as f64,if pids.is_empty(){"未记录"}else{&pids},session,cwd,clue,skipped,status.interval_seconds)
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
    normal_interval: u64,
    pending: HashMap<(u32,i64),Handle>,
    traffic: attribution::Traffic,
    sources: Vec<Source>,
    skipped: usize,
    #[cfg(test)]
    scope: Option<HashSet<(u32, i64)>>,
}
impl Guard {
    pub fn new(dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&dir)?;
        let incidents = Incidents::load(&dir)?;
        Ok(Self {
            dir,
            normal_interval: 5,
            pending: HashMap::new(),
            traffic: attribution::Traffic::default(), sources: Vec::new(), skipped: 0,
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
            super::archive::compress(&closed)?;
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
            super::archive::compress(&closed)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        serde_json::to_writer(&mut file, &value)?;
        file.write_all(b"\n")?;
        file.flush()?;
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
            self.status.last_alert = format!("Codex Git 防护失败：{e:#}。请检查 Session Bar 的 Git 防护设置与日志。 ");
            if self
                .last_popup
                .is_none_or(|at| at.elapsed() > Duration::from_secs(60))
            {
                alert = Some(self.status.last_alert.clone());
                self.last_popup = Some(Instant::now());
            }
        }
        self.status.updated_at = now();
        self.status.interval_seconds = effective_interval(self.status.enabled,self.status.healthy,self.status.storm_active,self.normal_interval);
        self.next=started+Duration::from_secs(self.status.interval_seconds);
        self.status.pending_terminations=self.pending.len();
        self.status.episode_terminated=self.status.terminated.saturating_sub(self.incidents.terminated_at_start);
        if self.status.healthy && !self.sources.is_empty() {
            self.status.last_alert=warning(&self.status,&self.sources,self.skipped);
            if alert.is_some(){alert=Some(self.status.last_alert.clone());}
        }
        if let Ok(file)=OpenOptions::new().write(true).open(self.dir.join("codex-git-guard-events.jsonl")){let _=file.sync_data();}
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
        let mut confirmed=Vec::new();
        self.pending.retain(|identity,handle|unsafe{if WaitForSingleObject(handle.0,0)==WAIT_OBJECT_0 {confirmed.push(*identity);false}else{true}});
        for (pid,created_ticks) in confirmed {
            self.status.terminated+=1;
            self.event(json!({"time":chrono::Local::now().to_rfc3339(),"type":"termination_confirmed","pid":pid,"created_ticks":created_ticks}))?;
        }
        if self.incidents.terminated!=self.status.terminated {self.incidents.terminated=self.status.terminated;self.incidents.save(&self.dir)?;}
        let cfg = read_config(&self.dir)?;
        self.normal_interval=cfg.interval_seconds;
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
        self.status.observed_new_git_60s=self.traffic.observe(items.iter().map(|c|(c.process.pid,c.process.created_ticks)),now());
        if self.incidents.active {self.status.peak_count=self.status.peak_count.max(items.len());}
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
        let new_episode=!self.incidents.active;
        if new_episode {self.status.peak_count=self.status.count;self.skipped=0;self.sources.clear();}
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
        if self.sources.is_empty() || popup_due {
            let mut examples:Vec<_>=fresh_items.iter().collect();
            examples.sort_by_key(|c|(c.origin.0,c.process.pid));
            self.sources=examples.into_iter().take(4).map(source).collect();
            self.event(json!({"time":chrono::Local::now().to_rfc3339(),"type":"storm_context",
                "episode":self.incidents.storms,"observation_window_seconds":60,"observed_new_git":self.status.observed_new_git_60s,
                "sources":self.sources}))?;
        }
        let mut ended = 0;
        let mut pending = 0;
        let mut failed = 0;
        let all: HashMap<_, _> = fresh.processes.iter().map(|p| (p.pid, p)).collect();
        let deadline=Instant::now()+Duration::from_millis(750);
        for victim in victims {
            if Instant::now()>=deadline {break;}
            if self.pending.contains_key(&(victim.process.pid,victim.process.created_ticks)){continue;}
            // Check cancellation/config between small batches, not once per PID.
            if (ended+pending+failed)%32==0 && !read_config(&self.dir)?.enabled {break;}
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
                    if WaitForSingleObject(h.0,0)==WAIT_OBJECT_0 {Ok(true)}else{
                        if self.pending.len()<4096 {self.pending.insert((victim.process.pid,victim.process.created_ticks),h);}
                        Ok(false)
                    }
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
        self.skipped+=failed;
        if popup_due {
            *alert = Some(String::new());
            self.last_popup = Some(Instant::now());
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum Event { Status(String), Update(String), Alert(String) }

// Enforcement has its own worker: UI queues and modal
// notifications must never hold up the next process check.
pub struct Runner {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Runner {
    pub fn start(dir:PathBuf,events:std::sync::mpsc::SyncSender<Event>)->Self {
        use std::sync::{Arc,atomic::{AtomicBool,Ordering}};
        let stop=Arc::new(AtomicBool::new(false));let done=stop.clone();
        let thread=std::thread::spawn(move||{
            let mut guard=None;let mut note=String::new();let mut pending_alert=None;let mut last_warning=String::new();
            while !done.load(Ordering::Relaxed) {
                if guard.is_none(){match Guard::new(dir.clone()){Ok(value)=>guard=Some(value),Err(error)=>{
                    pending_alert=Some(format!("Codex Git 防护无法启动：{error:#}"));
                }}}
                let wait=if let Some(g)=guard.as_mut(){
                    let (alert,status)=g.poll();if alert.is_some(){pending_alert=alert;}
                    if !status.is_empty()&&status!=note&&events.try_send(Event::Status(status.clone())).is_ok(){note=status;}
                    if !g.status.last_alert.is_empty()&&g.status.last_alert!=last_warning&&events.try_send(Event::Update(g.status.last_alert.clone())).is_ok(){last_warning=g.status.last_alert.clone();}
                    g.wait()
                }else{Duration::from_secs(5)};
                if let Some(alert)=pending_alert.take(){
                    if let Err(std::sync::mpsc::TrySendError::Full(Event::Alert(message)))=events.try_send(Event::Alert(alert)){pending_alert=Some(message);}
                }
                let deadline=Instant::now()+wait.max(Duration::from_millis(5));
                while !done.load(Ordering::Relaxed)&&Instant::now()<deadline {
                    std::thread::sleep(deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(100)));
                }
            }
        });
        Self{stop,thread:Some(thread)}
    }
}
impl Drop for Runner {
    fn drop(&mut self){self.stop.store(true,std::sync::atomic::Ordering::Relaxed);if let Some(thread)=self.thread.take(){let _=thread.join();}}
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn warning_distinguishes_confirmed_exits_window_and_session_evidence(){
        let status=Status{episode_terminated:12,pending_terminations:4,observed_new_git_60s:82,count:20,peak_count:64,max_processes:16,max_private_mib:1024,interval_seconds:1,storm_active:true,..Status::default()};
        let mut source=Source{git_pid:123,codex_pid:456,..Source::default()};
        source.context.cwd=r"D:\project".into();source.context.candidates.push("相关会话 (candidate)".into());
        let text=warning(&status,&[source.clone()],3);
        for fragment in ["已确认结束 12 个","另有 4 个","近 60 秒至少观察到 82","当前 20 个","峰值 64","456 → 123","暂无法确认","非确认归属","每 1 秒"]{assert!(text.contains(fragment),"{fragment}");}
        source.context.session_id="01a14d83-72e0-7e41-a98c-8d7dbf8d0174".into();source.context.session_title="真实会话".into();
        assert!(warning(&status,&[source],0).contains("真实会话 (01a14d83"));
    }
    #[test]
    fn defaults_and_fast_checks_recover_only_after_stable_observations(){
        let cfg:Config=serde_json::from_str("{}").unwrap();assert_eq!((cfg.max_processes,cfg.max_private_mib,cfg.interval_seconds),(16,1024,5));
        assert_eq!(effective_interval(true,true,false,15),15);
        assert_eq!(effective_interval(true,true,true,15),1);
        assert_eq!(effective_interval(true,false,false,15),1);
        assert_eq!(effective_interval(false,true,true,15),15);
    }
    #[test]
    fn full_ui_queue_does_not_stop_guard_worker(){
        let workspace=crate::guard::test_workspace();
        let temp=workspace.join("tmp").canonicalize().unwrap();let dir=temp.join(format!("guard-queue-test-{}",std::process::id()));fs::create_dir_all(&dir).unwrap();
        atomic(&dir,CONFIG,&Config{enabled:false,interval_seconds:1,..Config::default()}).unwrap();
        let (tx,_rx)=std::sync::mpsc::sync_channel(1);tx.try_send(Event::Status("blocked UI".into())).unwrap();
        let runner=Runner::start(dir.clone(),tx);
        std::thread::sleep(Duration::from_millis(150));
        let first:serde_json::Value=serde_json::from_slice(&fs::read(dir.join(STATUS)).unwrap()).unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        let last:serde_json::Value=serde_json::from_slice(&fs::read(dir.join(STATUS)).unwrap()).unwrap();
        assert!(last["updated_at"].as_f64().unwrap()-first["updated_at"].as_f64().unwrap()>=0.8);
        assert_eq!(last["healthy"],true);assert_eq!(last["enabled"],false);
        drop(runner);fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn storm_counter_requires_a_stable_minute_and_survives_serialization() {
        let mut counter = Incidents { version:1, tracked_since:"2026-10-20T00:00:00Z".into(),
            storms:0, active:false, terminated:9592, terminated_at_start:9592, calm_since:None, last_observation:None };
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
            interval_seconds: 5,
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
        let workspace = crate::guard::test_workspace();
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
                    .env("CODEX_THREAD_ID", "00000000-0000-4000-8000-000000000001")
                    .current_dir(&dir)
                    .creation_flags(0x08000000)
                    .spawn()
                    .unwrap(),
            );
        }
        let mut guard = Guard::new(dir.clone()).unwrap();
        let mut owned: HashSet<u32> = children.0.iter().map(|c| c.id()).collect();
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
            interval_seconds: 5,
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
        std::thread::sleep(Duration::from_millis(80));
        guard.next=Instant::now();let _=guard.poll();
        assert_eq!(guard.status.terminated, 4);
        assert_eq!(guard.status.storm_count, 1);
        assert!(guard.status.storm_active);
        let restored = Guard::new(dir.clone()).unwrap();
        assert_eq!(restored.status.terminated, 4);
        assert_eq!(restored.status.storm_count, 1);
        assert!(warning.is_some());
        assert!(guard.status.last_alert.contains("已确认结束 4 个"));
        assert_eq!(guard.status.observed_new_git_60s,10);
        assert_eq!(guard.sources[0].context.session_id,"00000000-0000-4000-8000-000000000001");
        assert_eq!(guard.sources[0].context.evidence,"process_environment");
        assert!(!guard.sources[0].command.is_empty());
        assert!(
            children.0[9].try_wait().unwrap().is_none(),
            "Commit fixture must remain alive"
        );
        // More queries appear while the first warning remains unacknowledged.
        for _ in 0..4 {
            let child=Command::new(&fixture).arg("diff").stdout(Stdio::null()).stderr(Stdio::null()).creation_flags(0x08000000).spawn().unwrap();
            owned.insert(child.id());children.0.push(child);
        }
        let snapshot=guard.collector.sample(1).unwrap();guard.scope=Some(snapshot.processes.iter().filter(|p|owned.contains(&p.pid)).map(|p|(p.pid,p.created_ticks)).collect());
        guard.next=Instant::now();let _=guard.poll();std::thread::sleep(Duration::from_millis(80));guard.next=Instant::now();let _=guard.poll();
        assert_eq!(guard.status.terminated,8);assert_eq!(guard.status.storm_count,1);
        assert_eq!(guard.status.episode_terminated,8);
        assert_eq!(guard.status.observed_new_git_60s,14);
        assert!(children.0[9].try_wait().unwrap().is_none());
        let evidence = dir.join("codex-git-guard-events.jsonl");
        assert!(fs::metadata(&evidence).unwrap().len() < 65536);
        let events = fs::read_to_string(&evidence).unwrap();
        let actions: Vec<_> = events
            .lines()
            .map(|s| serde_json::from_str::<serde_json::Value>(s).unwrap())
            .filter(|v| v["type"] == "termination_result")
            .collect();
        assert_eq!(actions.len(), 8);
        assert!(actions
            .iter()
            .all(|v| owned.contains(&(v["pid"].as_u64().unwrap() as u32))));
        assert!(guard.wait() <= Duration::from_secs(1));
    }
}
