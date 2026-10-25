//! GitHub stable releases, explicit consent and a separate transactional installer.
mod http;
use crate::{locale, now, read_json, Paths, State, Stop};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::{self, File},
    io::Read,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};
use windows::{
    core::PCWSTR,
    Win32::{Foundation::*, System::Threading::*, UI::WindowsAndMessaging::*},
};

const API: &str = "https://api.github.com/repos/frankfoxy/codex-session-bar/releases/latest";
const DOWNLOADS: &str = "https://github.com/frankfoxy/codex-session-bar/releases/download/";
const INTERVAL: Duration = Duration::from_secs(30 * 60);
const MAX_ZIP: u64 = 32 * 1024 * 1024;
const INSTALLER: &str = include_str!("update/install.ps1");

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct Version([u64; 3]);
impl Version {
    fn parse(text: &str) -> Option<Self> {
        let text = text.strip_prefix('v').unwrap_or(text);
        let parts: Vec<_> = text.split('.').collect();
        if parts.len() != 3 {
            return None;
        }
        let mut version = [0; 3];
        for (i, part) in parts.iter().enumerate() {
            if part.is_empty()
                || part.len() > 9
                || !part.bytes().all(|b| b.is_ascii_digit())
                || (part.len() > 1 && part.starts_with('0'))
            {
                return None;
            }
            version[i] = part.parse().ok()?;
        }
        Some(Self(version))
    }
    fn label(self) -> String {
        format!("{}.{}.{}", self.0[0], self.0[1], self.0[2])
    }
}
#[derive(Clone, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}
#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}
#[derive(Clone)]
struct Candidate {
    version: Version,
    archive: Asset,
    checksums: Asset,
}
fn candidate(release: &Release, current: Version) -> Result<Option<Candidate>> {
    if release.draft || release.prerelease {
        return Ok(None);
    }
    let Some(version) = Version::parse(&release.tag_name) else {
        return Ok(None);
    };
    if version <= current {
        return Ok(None);
    }
    let name = format!("codex-session-bar-{}-windows-x64.zip", version.label());
    let asset = |name: &str, maximum: u64| -> Result<Asset> {
        let matches: Vec<_> = release.assets.iter().filter(|a| a.name == name).collect();
        ensure!(matches.len() == 1, "Release must have one {name}");
        let value = matches[0];
        ensure!(
            value.size > 0 && value.size <= maximum,
            "Invalid release asset size"
        );
        ensure!(
            value.browser_download_url == format!("{DOWNLOADS}{}/{name}", release.tag_name),
            "Release asset belongs to a different repository or tag"
        );
        if let Some(hash) = value.digest.as_deref() {
            ensure!(
                hash.strip_prefix("sha256:").is_some_and(valid_hash),
                "Invalid release digest"
            );
        }
        Ok(value.clone())
    };
    Ok(Some(Candidate {
        version,
        archive: asset(&name, MAX_ZIP)?,
        checksums: asset("SHA256SUMS.txt", 65536)?,
    }))
}
fn valid_hash(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())
}
fn expected_hash(text: &str, filename: &str) -> Result<String> {
    let mut matches = Vec::new();
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() == 2 && fields[1].trim_start_matches('*') == filename {
            ensure!(valid_hash(fields[0]), "Invalid SHA-256 checksum");
            matches.push(fields[0].to_lowercase());
        }
    }
    ensure!(matches.len() == 1, "Missing or ambiguous package checksum");
    Ok(matches.remove(0))
}
fn hash_file(path: &Path) -> Result<String> {
    let mut input = File::open(path)?;
    let mut hash = Sha256::new();
    let mut chunk = [0u8; 65536];
    loop {
        let n = input.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        hash.update(&chunk[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn plain(path: &Path) -> String {
    path.to_string_lossy()
        .trim_start_matches(r"\\?\")
        .to_owned()
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct View {
    pub phase: String,
    pub detail: String,
    pub last_check_at: Option<f64>,
    pub last_result: String,
    pub last_detail: String,
}
impl View {
    fn load(paths: &Paths) -> Self {
        let saved = read_json(&paths.runtime.join("update-status.json"), 16384);
        let mut view: Self = serde_json::from_value(saved.clone()).unwrap_or_default();
        if view.last_result.is_empty()
            && matches!(
                view.phase.as_str(),
                "up_to_date" | "available" | "check_failed"
            )
        {
            view.last_check_at = saved["time"].as_f64();
            view.last_result = view.phase.clone();
            view.last_detail = view.detail.clone();
        }
        if view.busy() {
            view.phase.clear();
        }
        view
    }
    fn change(&mut self, phase: &str, detail: &str, at: f64) {
        self.phase = phase.into();
        self.detail = detail.into();
        if matches!(phase, "up_to_date" | "available" | "check_failed") {
            self.last_check_at = Some(at);
            self.last_result = phase.into();
            self.last_detail = detail.into();
        }
    }
    pub fn busy(&self) -> bool {
        matches!(
            self.phase.as_str(),
            "checking" | "downloading" | "restarting"
        )
    }
    pub fn time_label(&self, zh: bool) -> String {
        let time = self
            .last_check_at
            .filter(|at| at.is_finite() && *at > 0.)
            .and_then(|at| chrono::DateTime::from_timestamp(at as i64, 0))
            .map(|at| {
                at.with_timezone(&chrono::Local)
                    .format("%m-%d %H:%M:%S")
                    .to_string()
            });
        match time {
            Some(time) => format!(
                "{} {time}",
                if zh {
                    "上次检查："
                } else {
                    "Last checked:"
                }
            ),
            None => if zh {
                "尚未检查"
            } else {
                "Not checked yet"
            }
            .into(),
        }
    }
    pub fn result_label(&self, zh: bool) -> String {
        let latest = &self.last_detail;
        let text = match self.phase.as_str() {
            "checking" => {
                if zh {
                    "正在检查 GitHub…"
                } else {
                    "Checking GitHub…"
                }
            }
            "downloading" => {
                if zh {
                    "正在下载并校验…"
                } else {
                    "Downloading and verifying…"
                }
            }
            "restarting" => {
                if zh {
                    "正在重启…"
                } else {
                    "Restarting…"
                }
            }
            "error" => {
                if zh {
                    "更新失败 · 当前程序继续运行"
                } else {
                    "Update failed · current app retained"
                }
            }
            _ => match self.last_result.as_str() {
                "up_to_date" => {
                    return if zh {
                        format!("无需更新 · GitHub {latest}")
                    } else {
                        format!("Up to date · GitHub {latest}")
                    }
                }
                "available" => {
                    return if self.phase == "declined" {
                        if zh {
                            format!("发现 {latest} · 已暂缓")
                        } else {
                            format!("{latest} available · postponed")
                        }
                    } else {
                        if zh {
                            format!("发现新版本 {latest}")
                        } else {
                            format!("Version {latest} available")
                        }
                    }
                }
                "check_failed" => {
                    if zh {
                        "检查失败 · 可重试"
                    } else {
                        "Check failed · retry available"
                    }
                }
                _ => {
                    if zh {
                        "每 30 分钟自动检查"
                    } else {
                        "Automatic checks every 30 minutes"
                    }
                }
            },
        };
        text.into()
    }
    pub fn tooltip(&self, zh: bool) -> String {
        let mut text = format!("{}\n{}", self.time_label(zh), self.result_label(zh));
        if matches!(self.phase.as_str(), "error" | "check_failed") && !self.detail.is_empty() {
            text.push('\n');
            text.push_str(&self.detail);
        }
        text
    }
}
fn status(paths: &Paths, state: &State, phase: &str, detail: &str) {
    let view = {
        let mut shared = state.lock().unwrap();
        shared.updater.change(phase, detail, now());
        shared.updater.clone()
    };
    paths.write(
        "update-status.json",
        &json!({"time":now(),"current_version":env!("CARGO_PKG_VERSION"),
        "phase":phase,"detail":detail,"interval_seconds":INTERVAL.as_secs(),
        "last_check_at":view.last_check_at,"last_result":view.last_result,"last_detail":view.last_detail}),
    );
}
fn fetch(etag: &mut String, cache: &mut Option<Release>, stop: &AtomicBool) -> Result<()> {
    let mut body = Vec::new();
    let response = http::get(API, etag, &mut body, 1024 * 1024, stop)?;
    if response.status == 200 {
        let release: Release =
            serde_json::from_slice(&body).context("Invalid GitHub release metadata")?;
        *cache = Some(release);
        *etag = response.etag;
    }
    Ok(())
}
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
fn prompt_text(version: Version, zh: bool) -> String {
    let version = version.label();
    let current = env!("CARGO_PKG_VERSION");
    if zh {
        format!("发现新版本 {version}（当前 {current}）。\r\n\r\n是否更新？\r\n确认后将下载更新并重启 Session Bar，保留现有设置。")
    } else {
        format!("Version {version} is available (current: {current}).\r\n\r\nUpdate now?\r\nSession Bar will download the update and restart. Your settings will be kept.")
    }
}
enum Event {
    Stop,
    Check,
    Decision(Candidate, bool),
}
#[derive(Clone)]
pub struct Control {
    tx: mpsc::Sender<Event>,
    busy: Arc<AtomicBool>,
}
impl Control {
    pub fn busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }
    pub fn request(&self) -> bool {
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        if self.tx.send(Event::Check).is_err() {
            self.busy.store(false, Ordering::Release);
            return false;
        }
        true
    }
}
pub struct Service {
    tx: mpsc::Sender<Event>,
    handle: Option<thread::JoinHandle<()>>,
    control: Control,
}
impl Service {
    pub fn control(&self) -> Control {
        self.control.clone()
    }
    pub fn start(
        paths: &Paths,
        state: &State,
        stop: &Stop,
        bar: isize,
        quit_message: u32,
    ) -> Option<Self> {
        // Source-tree tests and staging probes must never update an installation.
        let installed = paths
            .app
            .join("bin/codex-session-bar.exe")
            .canonicalize()
            .ok()?;
        if std::env::current_exe().ok()?.canonicalize().ok()? != installed {
            return None;
        }
        state.lock().unwrap().updater = View::load(paths);
        let (paths, state, stop) = (paths.clone(), state.clone(), stop.clone());
        let (tx, rx) = mpsc::channel();
        let busy = Arc::new(AtomicBool::new(false));
        let control = Control {
            tx: tx.clone(),
            busy: busy.clone(),
        };
        let events = tx.clone();
        let handle = thread::spawn(move || {
            let mut etag = String::new();
            let mut cache = None;
            let mut offered = HashSet::new();
            let mut prompt_open = false;
            // Give the Codex language observer time to initialize before a prompt.
            let mut next = Instant::now() + Duration::from_secs(10);
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                match rx.recv_timeout(next.saturating_duration_since(Instant::now())) {
                    Ok(Event::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Ok(Event::Decision(release, true)) => {
                        prompt_open = false;
                        busy.store(true, Ordering::Release);
                        status(&paths, &state, "downloading", &release.version.label());
                        if let Err(error) =
                            install(&paths, &state, &stop, &release, bar, quit_message)
                        {
                            status(&paths, &state, "error", &format!("{error:#}"));
                            if !stop.load(Ordering::Relaxed) {
                                let zh = locale::chinese();
                                let message = if zh {
                                    format!("更新未完成，当前程序继续运行。\r\n{error:#}")
                                } else {
                                    format!("The update did not complete. Your current installation is retained.\r\n{error:#}")
                                };
                                thread::spawn(move || unsafe {
                                    let _ = MessageBoxW(
                                        None,
                                        PCWSTR(wide(&message).as_ptr()),
                                        PCWSTR(
                                            wide(if zh {
                                                "Session Bar 更新"
                                            } else {
                                                "Session Bar update"
                                            })
                                            .as_ptr(),
                                        ),
                                        MB_OK | MB_ICONWARNING | MB_TOPMOST,
                                    );
                                });
                            }
                        }
                        busy.store(false, Ordering::Release);
                    }
                    Ok(Event::Decision(_, false)) => {
                        prompt_open = false;
                        status(
                            &paths,
                            &state,
                            "declined",
                            "The same version will not prompt again until restart",
                        );
                    }
                    event @ (Ok(Event::Check) | Err(mpsc::RecvTimeoutError::Timeout)) => {
                        let manual = matches!(event, Ok(Event::Check));
                        next = Instant::now() + INTERVAL;
                        if !manual && busy.swap(true, Ordering::AcqRel) {
                            continue;
                        }
                        status(&paths, &state, "checking", "");
                        let result = (|| -> Result<()> {
                            fetch(&mut etag, &mut cache, &stop)?;
                            let Some(release) = cache.as_ref() else {
                                return Ok(());
                            };
                            let newer = candidate(
                                release,
                                Version::parse(env!("CARGO_PKG_VERSION")).unwrap(),
                            )?;
                            if let Some(release) = newer {
                                status(&paths, &state, "available", &release.version.label());
                                if !prompt_open
                                    && (manual || !offered.contains(&release.version.label()))
                                {
                                    offered.insert(release.version.label());
                                    prompt_open = true;
                                    let tx = events.clone();
                                    let message = prompt_text(release.version, locale::chinese());
                                    thread::spawn(move || {
                                        let accepted = unsafe {
                                            MessageBoxW(
                                                None,
                                                PCWSTR(wide(&message).as_ptr()),
                                                PCWSTR(
                                                    wide(if locale::chinese() {
                                                        "Session Bar 更新"
                                                    } else {
                                                        "Session Bar update"
                                                    })
                                                    .as_ptr(),
                                                ),
                                                MB_YESNO
                                                    | MB_DEFBUTTON2
                                                    | MB_ICONQUESTION
                                                    | MB_TOPMOST,
                                            ) == IDYES
                                        };
                                        let _ = tx.send(Event::Decision(release, accepted));
                                    });
                                }
                            } else {
                                status(&paths, &state, "up_to_date", &release.tag_name);
                            }
                            Ok(())
                        })();
                        if let Err(error) = result {
                            status(&paths, &state, "check_failed", &format!("{error:#}"));
                        }
                        busy.store(false, Ordering::Release);
                    }
                }
            }
        });
        Some(Self {
            tx,
            handle: Some(handle),
            control,
        })
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.tx.send(Event::Stop);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
fn download(asset: &Asset, path: &Path, stop: &AtomicBool) -> Result<()> {
    let mut file = File::create(path)?;
    let response = http::get(&asset.browser_download_url, "", &mut file, asset.size, stop)?;
    ensure!(response.bytes == asset.size, "Incomplete update download");
    file.sync_all()?;
    if let Some(digest) = &asset.digest {
        ensure!(
            format!("sha256:{}", hash_file(path)?) == digest.to_lowercase(),
            "GitHub asset checksum mismatch"
        );
    }
    Ok(())
}
fn creation_ticks() -> Result<u64> {
    unsafe {
        let (mut born, mut end, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        GetProcessTimes(
            GetCurrentProcess(),
            &mut born,
            &mut end,
            &mut kernel,
            &mut user,
        )?;
        Ok(((born.dwHighDateTime as u64) << 32) | born.dwLowDateTime as u64)
    }
}
fn powershell(
    script: &Path,
    plan: &Path,
    mode: &str,
    folder: &Path,
) -> Result<std::process::Child> {
    let shell =
        PathBuf::from(std::env::var_os("SystemRoot").context("Windows directory is unavailable")?)
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let errors = File::options()
        .create(true)
        .append(true)
        .open(folder.join("installer-errors.log"))?;
    let mut command = Command::new(shell);
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-WindowStyle",
            "Hidden",
            "-File",
        ])
        .arg(script)
        .arg("-PlanPath")
        .arg(plan)
        .arg("-Mode")
        .arg(mode);
    if mode == "Install" {
        command.arg("-Notify");
    }
    Ok(command
        .current_dir(folder)
        .env("TEMP", folder)
        .env("TMP", folder)
        .env("TMPDIR", folder)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(errors)
        .creation_flags(CREATE_NO_WINDOW.0)
        .spawn()?)
}
fn install(
    paths: &Paths,
    state: &State,
    stop: &AtomicBool,
    release: &Candidate,
    bar: isize,
    quit_message: u32,
) -> Result<()> {
    ensure!(!stop.load(Ordering::Relaxed), "Update cancelled");
    let temp = paths
        .runtime
        .parent()
        .context("Workspace tmp is unavailable")?
        .canonicalize()?;
    let workspace = paths
        .runtime
        .parent()
        .and_then(Path::parent)
        .context("Workspace is unavailable")?
        .canonicalize()?;
    ensure!(
        temp.parent() == Some(workspace.as_path()) && temp.file_name().is_some_and(|n| n == "tmp"),
        "Updates require the workspace's direct tmp directory"
    );
    let folder = temp.join(format!(
        "session-bar-update-{}-{}-{}",
        release.version.label(),
        std::process::id(),
        (now() * 1000.) as u64
    ));
    ensure!(
        folder.parent() == Some(temp.as_path()),
        "Update must stay within workspace/tmp"
    );
    fs::create_dir(&folder)?;
    let archive = folder.join(&release.archive.name);
    let checksums = folder.join("SHA256SUMS.txt");
    download(&release.checksums, &checksums, stop)?;
    let expected = expected_hash(&fs::read_to_string(checksums)?, &release.archive.name)?;
    download(&release.archive, &archive, stop)?;
    ensure!(
        hash_file(&archive)? == expected,
        "Package SHA-256 does not match SHA256SUMS.txt"
    );
    let app = paths.app.canonicalize()?;
    ensure!(
        std::env::current_exe()?.canonicalize()?
            == app.join("bin/codex-session-bar.exe").canonicalize()?,
        "Only the running installation may be updated"
    );
    let plan = folder.join("plan.json");
    let script = folder.join("install.ps1");
    // Windows PowerShell 5.1 requires a BOM for Chinese literals.
    let mut script_bytes = vec![0xef, 0xbb, 0xbf];
    script_bytes.extend_from_slice(INSTALLER.as_bytes());
    fs::write(&script, script_bytes)?;
    fs::write(
        &plan,
        serde_json::to_vec(&json!({"version":release.version.label(),"app":plain(&app),
        "tmp_root":plain(&temp),"folder":plain(&folder),"archive":plain(&archive),"sha256":expected,
        "parent_pid":std::process::id(),"parent_created":creation_ticks()?,"chinese":locale::chinese()}))?,
    )?;
    let mut prepare = powershell(&script, &plan, "Prepare", &folder)?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(result) = prepare.try_wait()? {
            ensure!(
                result.success(),
                "Could not prepare update; see {}",
                plain(&folder.join("installer-errors.log"))
            );
            break;
        }
        if stop.load(Ordering::Relaxed) || Instant::now() >= deadline {
            let _ = prepare.kill();
            let _ = prepare.wait();
            anyhow::bail!("Update preparation cancelled or timed out");
        }
        thread::sleep(Duration::from_millis(100));
    }
    ensure!(!stop.load(Ordering::Relaxed), "Update cancelled");
    let mut helper = powershell(&script, &plan, "Install", &folder)?;
    let ready = folder.join("ready.json");
    let deadline = Instant::now() + Duration::from_secs(15);
    while read_json(&ready, 4096)["phase"] != "ready" {
        if let Some(result) = helper.try_wait()? {
            anyhow::bail!("Update helper exited before handoff: {result}");
        }
        if stop.load(Ordering::Relaxed) || Instant::now() >= deadline {
            let _ = helper.kill();
            let _ = helper.wait();
            anyhow::bail!("Update handoff timed out");
        }
        thread::sleep(Duration::from_millis(100));
    }
    status(paths, state, "restarting", &release.version.label());
    let result = unsafe {
        let window = HWND(bar as *mut _);
        let mut pid = 0;
        GetWindowThreadProcessId(window, Some(&mut pid));
        ensure!(pid == std::process::id(), "Update window identity changed");
        PostMessageW(window, quit_message, WPARAM(0), LPARAM(0))
    };
    if let Err(error) = result {
        let _ = helper.kill();
        let _ = helper.wait();
        return Err(error.into());
    }
    Ok(())
}
pub fn inspect(_paths: &Paths) -> Result<()> {
    let mut cache = None;
    fetch(&mut String::new(), &mut cache, &AtomicBool::new(false))?;
    let release = cache.context("No release metadata")?;
    let newer = candidate(&release, Version::parse(env!("CARGO_PKG_VERSION")).unwrap())?.is_some();
    println!(
        "{}",
        json!({"current":env!("CARGO_PKG_VERSION"),"latest":release.tag_name,"update_available":newer,"interval_seconds":INTERVAL.as_secs()})
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn check_history_survives_progress_declining_and_update_failures() {
        let mut view = View::default();
        assert_eq!(view.time_label(true), "尚未检查");
        view.change("up_to_date", "v0.2.1", 1000.);
        view.change("checking", "", 1010.);
        assert_eq!(view.last_check_at, Some(1000.));
        assert!(view.busy());
        view.change("available", "0.2.3", 1020.);
        view.change("declined", "postponed", 1030.);
        assert_eq!(view.last_check_at, Some(1020.));
        assert!(view.result_label(true).contains("已暂缓"));
        view.change("downloading", "0.2.3", 1040.);
        assert!(view.busy());
        view.change("error", "download interrupted", 1050.);
        assert_eq!(view.last_check_at, Some(1020.));
        assert!(view.tooltip(false).contains("download interrupted"));
        view.change("check_failed", "HTTP 403", 1060.);
        assert_eq!(view.last_check_at, Some(1060.));
        assert_eq!(view.last_result, "check_failed");
        assert!(view.result_label(true).contains("失败"));
        assert!(view.result_label(false).contains("failed"));
        let restored: View = serde_json::from_value(serde_json::to_value(&view).unwrap()).unwrap();
        assert_eq!(restored.last_detail, "HTTP 403");
        assert!(!restored.busy());
    }
    #[test]
    fn manual_requests_are_coalesced_and_allow_retry_after_completion() {
        let (tx, rx) = mpsc::channel();
        let control = Control {
            tx,
            busy: Arc::new(AtomicBool::new(false)),
        };
        assert!(control.request());
        assert!(control.busy());
        for _ in 0..20 {
            assert!(!control.request());
        }
        assert!(matches!(rx.try_recv().unwrap(), Event::Check));
        assert!(rx.try_recv().is_err());
        control.busy.store(false, Ordering::Release);
        assert!(control.request());
        assert!(matches!(rx.try_recv().unwrap(), Event::Check));
        drop(rx);
        control.busy.store(false, Ordering::Release);
        assert!(!control.request());
        assert!(!control.busy());
    }
    #[test]
    #[ignore = "Read-only GitHub network/download probe; never installs or restarts"]
    fn published_assets_download_and_verify_without_installing() {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace = std::env::var_os("CODEX_SESSION_BAR_WORKSPACE")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                if manifest
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|n| n == "tools")
                {
                    manifest.parent().unwrap().parent().unwrap().to_path_buf()
                } else {
                    manifest
                }
            });
        let temp = workspace
            .canonicalize()
            .unwrap()
            .join("tmp")
            .canonicalize()
            .unwrap();
        let folder = temp.join(format!(
            "session-bar-download-test-{}-{}",
            std::process::id(),
            (now() * 1000.) as u64
        ));
        assert_eq!(folder.parent(), Some(temp.as_path()));
        fs::create_dir(&folder).unwrap();
        let stop = AtomicBool::new(false);
        let mut cache = None;
        fetch(&mut String::new(), &mut cache, &stop).unwrap();
        let release = candidate(&cache.unwrap(), Version([0, 0, 0]))
            .unwrap()
            .unwrap();
        let sums = folder.join("SHA256SUMS.txt");
        let archive = folder.join(&release.archive.name);
        download(&release.checksums, &sums, &stop).unwrap();
        download(&release.archive, &archive, &stop).unwrap();
        assert_eq!(
            hash_file(&archive).unwrap(),
            expected_hash(&fs::read_to_string(sums).unwrap(), &release.archive.name).unwrap()
        );
        assert!(!folder.join("plan.json").exists());
        assert!(!folder.join("ready.json").exists());
    }
    fn release(tag: &str) -> Release {
        let version = tag.trim_start_matches('v');
        let archive = format!("codex-session-bar-{version}-windows-x64.zip");
        serde_json::from_value(json!({"tag_name":tag,"draft":false,"prerelease":false,"assets":[
            {"name":archive,"size":123,"browser_download_url":format!("{DOWNLOADS}{tag}/{archive}")},
            {"name":"SHA256SUMS.txt","size":106,"browser_download_url":format!("{DOWNLOADS}{tag}/SHA256SUMS.txt")}
        ]})).unwrap()
    }
    #[test]
    fn stable_versions_compare_numerically_without_downgrades() {
        assert!(Version::parse("v0.2.10") > Version::parse("0.2.9"));
        for invalid in [
            "0.2",
            "0.2.3-rc.1",
            "0.2.3+test",
            "../0.2.3",
            "0.02.3",
            "0.2.3\r\n",
        ] {
            assert!(Version::parse(invalid).is_none());
        }
        let current = Version::parse("0.2.2").unwrap();
        for version in ["v0.2.1", "v0.2.2"] {
            assert!(candidate(&release(version), current).unwrap().is_none());
        }
        assert!(candidate(&release("v0.2.3"), current).unwrap().is_some());
        assert_eq!(INTERVAL, Duration::from_secs(1800));
    }
    #[test]
    fn releases_require_exact_assets_from_this_repository() {
        let current = Version::parse("0.2.2").unwrap();
        let mut value = release("v0.2.3");
        value.prerelease = true;
        assert!(candidate(&value, current).unwrap().is_none());
        value.prerelease = false;
        value.draft = true;
        assert!(candidate(&value, current).unwrap().is_none());
        value.draft = false;
        value.assets[0].browser_download_url = value.assets[0]
            .browser_download_url
            .replace("frankfoxy", "somebody");
        assert!(candidate(&value, current).is_err());
        value = release("v0.2.3");
        value.assets[0].size = MAX_ZIP + 1;
        assert!(candidate(&value, current).is_err());
        value = release("v0.2.3");
        value.assets.push(value.assets[0].clone());
        assert!(candidate(&value, current).is_err());
        value = release("v0.2.3");
        value.assets.remove(1);
        assert!(candidate(&value, current).is_err());
    }
    #[test]
    fn checksums_match_exact_filename_and_reject_ambiguity() {
        let hash = "a".repeat(64);
        let name = "package.zip";
        assert_eq!(
            expected_hash(&format!("{hash}  {name}\r\n"), name).unwrap(),
            hash
        );
        assert!(expected_hash(&format!("{hash} other.zip"), name).is_err());
        assert!(expected_hash(&format!("{hash} {name}\n{hash} {name}"), name).is_err());
        assert!(expected_hash("bad package.zip", name).is_err());
        assert!(prompt_text(Version([1, 2, 3]), true).contains("是否更新"));
        assert!(prompt_text(Version([1, 2, 3]), false).contains("Update now?"));
    }
}
