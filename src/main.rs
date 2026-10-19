#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod data;
mod desktop;
mod host;
mod ui;

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

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub thread: String,
    #[serde(default = "yes")]
    pub auto_thread: bool,
    #[serde(default)]
    pub host_offset: Option<[i32; 2]>,
    #[serde(default)]
    pub position_version: u32,
}
fn yes() -> bool {
    true
}

#[derive(Clone, Default, Serialize)]
pub struct Shared {
    pub host: isize,
    pub view: Option<String>,
    pub view_label: String,
    pub view_error: String,
    pub detected_at: f64,
    pub slot: Option<[i32; 4]>,
    pub slot_host: isize,
    pub snapshot: Option<data::Snapshot>,
    pub recent: data::Recent,
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
    let paths = Paths::discover()?;
    let args: Vec<String> = env::args().collect();
    if args.iter().any(|s| s == "--self-test") {
        return ui::self_test(&paths);
    }
    if args.iter().any(|s| s == "--inspect") {
        return desktop::inspect(&paths);
    }
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
    if settings.thread.is_empty() {
        settings.thread = desktop::default_context(&paths);
    }
    let state = Arc::new(Mutex::new(Shared {
        auto_thread: settings.auto_thread,
        fixed_thread: settings.thread.clone(),
        recent: data::load_recent(&paths),
        ..Shared::default()
    }));
    let stop = Arc::new(AtomicBool::new(false));
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
