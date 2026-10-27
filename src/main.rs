#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod body;
mod data;
mod prompts;
mod desktop;
mod host;
mod guard;
mod search;
mod tags;
mod ui;
mod refresh;
mod locale;
mod telemetry;
mod update;
mod reset_notice;
mod capacity_retry;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[derive(Clone)]
pub struct Paths {
    pub app: PathBuf,
    pub home: PathBuf,
    pub runtime: PathBuf,
    pub instance: String,
}
impl Paths {
    pub fn discover() -> Result<Self> {
        let exe = env::current_exe()?;
        let mut app = exe
            .parent()
            .context("Executable directory unavailable")?
            .to_path_buf();
        if app.file_name().is_some_and(|name| name == "bin") {
            app.pop();
        }
        if !app.join("emoji_catalog.json").exists() {
            app = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        }
        let workspace = env::var_os("CODEX_SESSION_BAR_WORKSPACE")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                if app
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|n| n == "tools")
                {
                    app.parent().unwrap().parent().unwrap().to_path_buf()
                } else {
                    app.clone()
                }
            })
            .canonicalize()?;
        let temp = workspace.join("tmp");
        let runtime = temp.join("codex-session-bar");
        anyhow::ensure!(
            runtime.parent() == Some(temp.as_path()) && temp.parent() == Some(workspace.as_path()),
            "Invalid workspace temp directory"
        );
        fs::create_dir_all(&runtime)?;
        for name in ["TEMP", "TMP", "TMPDIR"] {
            env::set_var(name, &runtime);
        }
        let home = env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env::var_os("USERPROFILE").unwrap_or_default()).join(".codex")
            })
            .canonicalize()?;
        // Node's entry-point resolver can reject extended-length drive paths.
        // Keep the adapter path in the ordinary absolute Windows form.
        let home = PathBuf::from(home.to_string_lossy().trim_start_matches(r"\\?\"));
        let normalized = home
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_lowercase();
        let instance = format!(
            "codex-session-bar-{:x}",
            Sha256::digest(normalized.as_bytes())
        );
        Ok(Self {
            app,
            home,
            runtime,
            instance: instance[..30].to_string(),
        })
    }
    pub fn write(&self, name: &str, value: &impl Serialize) {
        let pending = self
            .runtime
            .join(format!("{name}.{}.pending", std::process::id()));
        if let Ok(bytes) = serde_json::to_vec_pretty(value) {
            if fs::write(&pending, bytes).is_ok() {
                let _ = fs::rename(&pending, self.runtime.join(name));
            }
        }
    }
}

pub fn read_json(path: &Path, bound: u64) -> serde_json::Value {
    if fs::metadata(path).is_ok_and(|m| m.len() <= bound) {
        fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    } else {
        serde_json::Value::Null
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub thread: String,
    #[serde(default = "yes")]
    pub auto_thread: bool,
    #[serde(default)]
    pub host_offset: Option<[i32; 2]>,
    #[serde(default)]
    pub position_version: u32,
    #[serde(default = "default_recent_rows")]
    pub recent_rows: usize,
    #[serde(default)]
    pub recent_shortcut: Shortcut,
    #[serde(default)]
    pub compact_recent: bool,
    #[serde(default="default_title_width")]
    pub recent_title_width: u32,
    #[serde(default)]
    pub refresh: refresh::Settings,
    #[serde(default)]
    pub reset_notice: reset_notice::Settings,
    #[serde(default)]
    pub capacity_retry: capacity_retry::Settings,
}
fn default_recent_rows() -> usize { 10 }
fn default_title_width()->u32{200}
impl Default for Settings {
    fn default() -> Self {
        Self {
            thread: String::new(), auto_thread: true, host_offset: None,
            position_version: 0, recent_rows: default_recent_rows(),
            recent_shortcut: Shortcut::default(),
            compact_recent: false,
            recent_title_width:default_title_width(),
            refresh:refresh::Settings::default(),
            reset_notice:reset_notice::Settings::default(),
            capacity_retry:capacity_retry::Settings::default(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Shortcut {
    pub enabled: bool,
    pub key: u16,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}
impl Default for Shortcut {
    fn default() -> Self { Self { enabled: true, key: b'J' as u16, ctrl: true, alt: true, shift: false } }
}
impl Shortcut {
    pub fn valid(&self) -> bool {
        !self.enabled || ((self.ctrl || self.alt)
            && ((b'A' as u16..=b'Z' as u16).contains(&self.key)
                || (b'0' as u16..=b'9' as u16).contains(&self.key)
                || (0x70..=0x87).contains(&self.key)))
    }
    pub fn label(&self) -> String {
        if !self.enabled { return "Shortcut disabled".into(); }
        let mut parts = Vec::new();
        if self.ctrl { parts.push("Ctrl".into()); }
        if self.alt { parts.push("Alt".into()); }
        if self.shift { parts.push("Shift".into()); }
        parts.push(if (0x70..=0x87).contains(&self.key) {
            format!("F{}", self.key - 0x70 + 1)
        } else { char::from_u32(self.key as u32).unwrap_or('?').to_string() });
        parts.join("+")
    }
    pub fn control_word(&self) -> u16 {
        self.key | (((self.shift as u16) | ((self.ctrl as u16) << 1) | ((self.alt as u16) << 2)) << 8)
    }
    pub fn from_control_word(word: u16, enabled: bool) -> Self {
        Self { enabled, key: word & 0xff, shift: word & 0x100 != 0, ctrl: word & 0x200 != 0, alt: word & 0x400 != 0 }
    }
}
fn yes() -> bool {
    true
}

#[derive(Clone, Default, Serialize)]
pub struct Shared {
    #[serde(skip)]
    pub refresh_events: Arc<refresh::Events>,
    pub refresh_options: refresh::Settings,
    pub guard: guard::View,
    pub updater: update::View,
    pub reset_notice: Arc<reset_notice::View>,
    pub capacity_options: capacity_retry::Settings,
    pub capacity_epoch: u64,
    pub capacity_retry: capacity_retry::View,
    pub host: isize,
    pub view: Option<String>,
    pub view_label: String,
    pub view_error: String,
    pub snapshot_error: String,
    pub detected_at: f64,
    pub slot: Option<[i32; 4]>,
    pub slot_host: isize,
    pub snapshot: Option<data::Snapshot>,
    pub recent: Arc<data::Recent>,
    pub quota: Vec<data::Quota>,
    pub quota_at: f64,
    pub quota_error: String,
    pub action_note: String,
    pub action_pending: bool,
    pub auto_thread: bool,
    pub fixed_thread: String,
}
pub type State = Arc<Mutex<Shared>>;
pub type Stop = Arc<AtomicBool>;

fn run() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.iter().any(|s| s == "--version" || s == "-V") {
        println!("Codex Session Bar {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let paths = Paths::discover()?;
    if args.iter().any(|s| s == "--self-test") {
        return ui::self_test(&paths);
    }
    if args.iter().any(|s| s == "--check-update") {
        return update::inspect(&paths);
    }
    if args.iter().any(|s|s=="--inspect-reset-notice") {
        let source=if args.iter().any(|s|s=="--divin"){reset_notice::Source::Divin}else{reset_notice::Source::CodexResets};
        return reset_notice::inspect(&paths,source);
    }
    if args.iter().any(|s| s == "--inspect") {
        return desktop::inspect(&paths);
    }
    if args.iter().any(|s|s=="--inspect-capacity-retry") {return desktop::inspect_capacity(&paths);}
    if args.iter().any(|s|s=="--inspect-prompts") {return desktop::inspect_prompts(&paths);}
    if args.iter().any(|s| s == "--observe") {
        return host::inspect(&paths);
    }
    if args.iter().any(|s| s == "--quit") {
        return ui::request_quit(&paths);
    }
    if args.iter().any(|s| s == "--hide") {
        return ui::request_hide(&paths);
    }
    let mut settings: Settings =
        serde_json::from_value(read_json(&paths.app.join("settings.json"), 16384)).unwrap_or_else(
            |_| Settings {
                auto_thread: true,
                ..Settings::default()
            },
        );
    settings.reset_notice.clamp();
    settings.capacity_retry.clamp();
    if settings.thread.is_empty() {
        settings.thread = desktop::default_context(&paths);
    }
    let state = Arc::new(Mutex::new(Shared {
        auto_thread: settings.auto_thread,
        fixed_thread: settings.thread.clone(),
        refresh_options: settings.refresh.clone(),
        capacity_options:settings.capacity_retry.clone(),
        recent: data::load_recent(&paths).into(),
        ..Shared::default()
    }));
    let stop = Arc::new(AtomicBool::new(false));
    let panic_path=paths.runtime.join("rust-worker-error.json");
    let previous=std::panic::take_hook();
    std::panic::set_hook(Box::new(move|info|{
        let text=info.payload().downcast_ref::<&str>().copied().or_else(||info.payload().downcast_ref::<String>().map(String::as_str)).unwrap_or("Worker panicked");
        let _=fs::write(&panic_path,serde_json::json!({"time":now(),"message":text.chars().take(512).collect::<String>(),"location":info.location().map(|l|format!("{}:{}",l.file(),l.line()))}).to_string());previous(info);
    }));
    ui::run(paths, settings, state, stop.clone())?;
    stop.store(true, Ordering::Relaxed);
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        if let Ok(paths) = Paths::discover() {
            paths.write(
                "rust-error.json",
                &serde_json::json!({"time": now(), "error": format!("{error:#}")}),
            );
        }
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;
    #[test]
    fn older_settings_gain_defaults_without_losing_window_or_chat() {
        let settings: Settings = serde_json::from_value(serde_json::json!({
            "thread":"saved-chat", "host_offset":[35,2], "auto_thread":false
        })).unwrap();
        assert_eq!(settings.thread, "saved-chat");
        assert_eq!(settings.host_offset, Some([35,2]));
        assert!(!settings.auto_thread);
        assert_eq!(settings.recent_rows, 10);
        assert_eq!(settings.recent_title_width,200);
        assert_eq!(settings.recent_shortcut.label(), "Ctrl+Alt+J");
    }
    #[test]
    fn shortcut_capture_roundtrips_and_rejects_global_plain_keys() {
        let mut shortcut = Shortcut::from_control_word(0x274, true);
        assert_eq!(shortcut.label(), "Ctrl+F5");
        assert!(shortcut.valid());
        assert_eq!(shortcut.control_word(), 0x274);
        shortcut = Shortcut::from_control_word(b'K' as u16, true);
        assert!(!shortcut.valid());
        shortcut.enabled = false;
        assert!(shortcut.valid());
        let settings: Settings = serde_json::from_value(serde_json::json!({"recent_rows":14,"recent_shortcut":shortcut})).unwrap();
        let restored: Settings = serde_json::from_value(serde_json::to_value(&settings).unwrap()).unwrap();
        assert_eq!(restored.recent_rows, 14);
        assert_eq!(restored.recent_shortcut, settings.recent_shortcut);
    }
}
