//! Capacity-only continuation. Observations reuse the existing metadata worker.
use crate::{data::Store, desktop::Action, now, read_json, Paths, State, Stop};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    sync::{atomic::Ordering, mpsc::Sender, Mutex},
    thread,
    time::Duration,
};

pub const PROMPT: &str = "继续上次任务";
const MAX_AGE: f64 = 15. * 60.;
const JOURNAL: &str = "capacity-retry-state.json";
const LOG: &str = "capacity-retry-events.jsonl";
const JOURNAL_BYTES: u64 = 128 * 1024;
static LOG_LOCK: Mutex<()> = Mutex::new(());

#[derive(Serialize, Deserialize)]
struct Attempt {
    key: String,
    failed_at: f64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub enabled: bool,
    pub delay_secs: u64,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            delay_secs: 5,
        }
    }
}
impl Settings {
    pub fn clamp(&mut self) {
        self.delay_secs = self.delay_secs.clamp(1, 300);
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Failure {
    pub thread_id: String,
    pub turn_id: String,
    pub label: String,
    pub failed_at: f64,
}
impl Failure {
    fn key(&self) -> String {
        format!("{}:{}", self.thread_id, self.turn_id)
    }
    fn fresh(&self, at: f64) -> bool {
        !self.thread_id.is_empty()
            && !self.turn_id.is_empty()
            && self.failed_at > 0.
            && (0.0..=MAX_AGE).contains(&(at - self.failed_at))
    }
}
pub fn failure(
    thread_id: &str,
    label: &str,
    turn_id: &str,
    status: &str,
    error: Option<&str>,
    failed_at: Option<f64>,
) -> Option<Failure> {
    if status != "failed" {
        return None;
    }
    let raw = error.filter(|value| value.len() <= 8192)?;
    let value: Value = serde_json::from_str(raw).ok()?;
    let code = &value["codexErrorInfo"];
    let overloaded = code == "serverOverloaded"
        || code
            .as_object()
            .is_some_and(|v| v.contains_key("serverOverloaded"));
    let legacy = (code.is_null() || code == "")
        && value["message"].as_str().is_some_and(|message| {
            message.trim().eq_ignore_ascii_case(
                "Selected model is at capacity. Please try a different model.",
            )
        });
    if !overloaded && !legacy {
        return None;
    }
    Some(Failure {
        thread_id: thread_id.into(),
        turn_id: turn_id.into(),
        label: label.chars().take(100).collect(),
        failed_at: failed_at?,
    })
}
#[derive(Clone, Debug)]
pub struct Request {
    pub failure: Failure,
    pub epoch: u64,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct View {
    pub available: bool,
    pub waiting: usize,
    pub submitted: u64,
    pub attempts: u64,
    pub unconfirmed: u64,
    pub tracking_since: f64,
    pub in_flight: bool,
    pub last_note: String,
    pub last_error: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Counters {
    attempts: u64,
    submitted: u64,
    unconfirmed: u64,
    since: f64,
}
impl Counters {
    fn apply(&self, view: &mut View) {
        view.attempts = self.attempts;
        view.submitted = self.submitted;
        view.unconfirmed = self
            .unconfirmed
            .max(self.attempts.saturating_sub(self.submitted));
        view.tracking_since = self.since;
    }
}
impl View {
    pub fn text(&self, enabled: bool, zh: bool) -> String {
        if !enabled {
            return if zh {
                "已关闭 · 不发送继续消息"
            } else {
                "Disabled · no continuation messages"
            }
            .into();
        }
        if !self.available {
            return if zh {
                "等待最新会话状态"
            } else {
                "Waiting for fresh chat metadata"
            }
            .into();
        }
        let state = if self.in_flight {
            if zh {
                "正在提交"
            } else {
                "Submitting"
            }
        } else {
            if zh {
                "监测中"
            } else {
                "Monitoring"
            }
        };
        if zh {
            format!(
                "{state} · 等待 {} · 已提交 {}",
                self.waiting, self.submitted
            )
        } else {
            format!(
                "{state} · Waiting {} · Submitted {}",
                self.waiting, self.submitted
            )
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Journal {
    seen: Vec<Attempt>,
    counters: Counters,
}
impl Journal {
    fn load(paths: &Paths) -> Self {
        serde_json::from_value(read_json(&paths.runtime.join(JOURNAL), JOURNAL_BYTES))
            .unwrap_or_default()
    }
    fn for_send(paths: &Paths) -> Result<Self> {
        let path = paths.runtime.join(JOURNAL);
        match fs::metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Ok(meta) => {
                anyhow::ensure!(
                    meta.len() <= JOURNAL_BYTES,
                    "Capacity retry journal is too large"
                );
                serde_json::from_slice(&fs::read(path)?)
                    .context("Cannot read capacity retry journal; no message was sent")
            }
            Err(error) => Err(error.into()),
        }
    }
    fn claim(&mut self, paths: &Paths, failure: &Failure) -> Result<bool> {
        let key = failure.key();
        if self.seen.iter().any(|attempt| attempt.key == key) {
            return Ok(false);
        }
        self.seen
            .retain(|attempt| attempt.failed_at + MAX_AGE >= now());
        // Never evict a still-eligible turn: at capacity, stop sending instead.
        anyhow::ensure!(
            self.seen.len() < 1024,
            "Capacity retry journal is full; wait for old entries to expire"
        );
        self.seen.push(Attempt {
            key,
            failed_at: failure.failed_at,
        });
        self.counters.attempts = self.counters.attempts.saturating_add(1);
        if self.counters.since <= 0. {
            self.counters.since = now();
        }
        self.save(paths)?;
        Ok(true)
    }
    fn save(&self, paths: &Paths) -> Result<()> {
        let pending = paths.runtime.join(format!(
            "capacity-retry-state.{}.pending",
            std::process::id()
        ));
        let bytes = serde_json::to_vec(self)?;
        anyhow::ensure!(
            bytes.len() as u64 <= JOURNAL_BYTES,
            "Capacity retry journal is too large"
        );
        fs::write(&pending, bytes).context("Cannot record capacity retry before sending")?;
        fs::rename(pending, paths.runtime.join(JOURNAL))
            .context("Cannot save capacity retry record")?;
        Ok(())
    }
}
struct Schedule {
    pending: HashMap<String, Failure>,
    tried: HashSet<String>,
}
impl Schedule {
    fn new(journal: Journal) -> Self {
        Self {
            pending: HashMap::new(),
            tried: journal
                .seen
                .into_iter()
                .map(|attempt| attempt.key)
                .collect(),
        }
    }
    fn sync(&mut self, failures: &[Failure], at: f64) {
        let current: HashMap<_, _> = failures
            .iter()
            .filter(|failure| failure.fresh(at))
            .map(|f| (f.key(), f.clone()))
            .collect();
        self.pending.retain(|key, _| current.contains_key(key));
        for (key, failure) in current {
            if !self.tried.contains(&key) {
                self.pending.insert(key, failure);
            }
        }
    }
    fn take_due(&mut self, at: f64, delay: u64) -> Option<Failure> {
        let key = self
            .pending
            .iter()
            .filter(|(_, f)| f.fresh(at) && f.failed_at + delay as f64 <= at)
            .min_by(|(ak, a), (bk, b)| a.failed_at.total_cmp(&b.failed_at).then(ak.cmp(bk)))
            .map(|(k, _)| k.clone())?;
        let failure = self.pending.remove(&key)?;
        self.tried.insert(key);
        // Latest-turn identity plus the freshness window bounds useful history.
        if self.tried.len() > 512 {
            self.tried
                .retain(|key| self.pending.contains_key(key) || key == &failure.key());
        }
        Some(failure)
    }
}
pub fn configure(state: &State, mut options: Settings) {
    options.clamp();
    let mut live = state.lock().unwrap();
    live.capacity_options = options;
    live.capacity_epoch = live.capacity_epoch.wrapping_add(1);
    if !live.capacity_options.enabled {
        live.capacity_retry.waiting = 0;
    }
    live.refresh_events.notify();
}
pub fn start(
    paths: &Paths,
    state: &State,
    stop: &Stop,
    actions: Sender<Action>,
) -> thread::JoinHandle<()> {
    let (paths, state, stop) = (paths.clone(), state.clone(), stop.clone());
    thread::spawn(move || {
        let journal = Journal::load(&paths);
        journal
            .counters
            .apply(&mut state.lock().unwrap().capacity_retry);
        let mut schedule = Schedule::new(journal);
        let mut epoch = None;
        let events = state.lock().unwrap().refresh_events.clone();
        while !stop.load(Ordering::Relaxed) {
            let generation = events.generation();
            let (options, revision, recent) = {
                let live = state.lock().unwrap();
                (
                    live.capacity_options.clone(),
                    live.capacity_epoch,
                    live.recent.clone(),
                )
            };
            if epoch != Some(revision) {
                schedule = Schedule::new(Journal::load(&paths));
                epoch = Some(revision);
            }
            let fresh = recent.capacity_available
                && !recent.stale
                && recent.loaded_at > 0.
                && (0.0..=120.).contains(&(now() - recent.loaded_at));
            if options.enabled && fresh {
                schedule.sync(&recent.capacity_failures, now());
            } else {
                schedule.pending.clear();
            }
            if options.enabled && fresh {
                if let Some(failure) = schedule.take_due(now(), options.delay_secs) {
                    log(&paths, "queued", &failure, None);
                    if actions
                        .send(Action::CapacityRetry(Request {
                            failure,
                            epoch: revision,
                        }))
                        .is_err()
                    {
                        break;
                    }
                }
            }
            {
                let mut live = state.lock().unwrap();
                live.capacity_retry.waiting = schedule.pending.len();
                live.capacity_retry.available = fresh;
            }
            // Reuse the existing source signal; an idle check reads memory only.
            events.wait(generation, Duration::from_secs(1));
        }
    })
}
fn enabled(state: &State, epoch: u64) -> bool {
    let live = state.lock().unwrap();
    live.capacity_options.enabled && live.capacity_epoch == epoch
}
pub fn dispatch(
    paths: &Paths,
    state: &State,
    store: &Store,
    request: &Request,
    submit: impl FnOnce(&str, &str) -> Result<()>,
) -> Result<String> {
    let failure = &request.failure;
    if !enabled(state, request.epoch) || !failure.fresh(now()) {
        log(
            paths,
            "skipped",
            failure,
            Some("disabled, settings changed or old failure"),
        );
        return Ok("Capacity retry skipped".into());
    }
    let current = store
        .capacity_failure(&failure.thread_id)
        .inspect_err(|error| log(paths, "blocked", failure, Some(&format!("{error:#}"))))?;
    if current
        .as_ref()
        .is_none_or(|value| value.turn_id != failure.turn_id || !value.fresh(now()))
    {
        log(
            paths,
            "skipped",
            failure,
            Some("latest turn changed, recovered or stopped"),
        );
        return Ok("Capacity retry skipped".into());
    }
    if !enabled(state, request.epoch) {
        log(
            paths,
            "skipped",
            failure,
            Some("disabled, settings changed or old failure"),
        );
        return Ok("Capacity retry skipped".into());
    }
    let mut journal = Journal::for_send(paths)
        .inspect_err(|error| log(paths, "blocked", failure, Some(&format!("{error:#}"))))?;
    if !journal
        .claim(paths, failure)
        .inspect_err(|error| log(paths, "blocked", failure, Some(&format!("{error:#}"))))?
    {
        log(paths, "duplicate_skipped", failure, None);
        return Ok("Capacity retry already attempted".into());
    }
    let number = journal.counters.attempts;
    let delay = state.lock().unwrap().capacity_options.delay_secs;
    {
        let mut live = state.lock().unwrap();
        journal.counters.apply(&mut live.capacity_retry);
        live.capacity_retry.in_flight = true;
        live.capacity_retry.last_error.clear();
    }
    log_record(
        paths,
        "send_started",
        failure,
        None,
        Some(number),
        Some(delay),
    );
    let result = submit(&failure.thread_id, PROMPT);
    if result.is_ok() {
        journal.counters.submitted = journal.counters.submitted.saturating_add(1);
    } else {
        journal.counters.unconfirmed = journal.counters.unconfirmed.saturating_add(1);
    }
    let persistence_error = journal.save(paths).err().map(|error| format!("{error:#}"));
    let mut live = state.lock().unwrap();
    journal.counters.apply(&mut live.capacity_retry);
    live.capacity_retry.in_flight = false;
    match result {
        Ok(()) => {
            live.capacity_retry.last_note = failure.label.clone();
            if let Some(error) = &persistence_error {
                live.capacity_retry.last_error =
                    format!("Continuation submitted; counters could not be saved: {error}");
            }
            drop(live);
            log_record(
                paths,
                "submitted",
                failure,
                persistence_error.as_deref(),
                Some(number),
                Some(delay),
            );
            Ok("Capacity continuation submitted".into())
        }
        Err(error) => {
            live.capacity_retry.last_error = format!("{error:#}").chars().take(300).collect();
            drop(live);
            log_record(
                paths,
                "unconfirmed",
                failure,
                Some(&format!("{error:#}")),
                Some(number),
                Some(delay),
            );
            Err(error
                .context("Continuation unconfirmed; the same failed turn will not be sent again"))
        }
    }
}
fn log(paths: &Paths, event: &str, failure: &Failure, error: Option<&str>) {
    log_record(paths, event, failure, error, None, None);
}
fn log_record(
    paths: &Paths,
    event: &str,
    failure: &Failure,
    error: Option<&str>,
    number: Option<u64>,
    delay: Option<u64>,
) {
    let _lock = LOG_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let path = paths.runtime.join(LOG);
    let result = (|| -> Result<()> {
        if fs::metadata(&path).is_ok_and(|m| m.len() >= 128 * 1024) {
            let pending = paths.runtime.join("capacity-retry-events.gz.pending");
            let mut encoder = flate2::write::GzEncoder::new(
                fs::File::create(&pending)?,
                flate2::Compression::fast(),
            );
            std::io::copy(&mut fs::File::open(&path)?, &mut encoder)?;
            encoder.finish()?;
            for index in (1..4).rev() {
                let old = paths
                    .runtime
                    .join(format!("capacity-retry-events.{index}.gz"));
                if old.exists() {
                    fs::rename(
                        old,
                        paths
                            .runtime
                            .join(format!("capacity-retry-events.{}.gz", index + 1)),
                    )?;
                }
            }
            fs::rename(pending, paths.runtime.join("capacity-retry-events.1.gz"))?;
            fs::remove_file(&path)?;
        }
        let mut file = OpenOptions::new().append(true).create(true).open(path)?;
        serde_json::to_writer(
            &mut file,
            &json!({"event":event,"time":chrono::Local::now().to_rfc3339(),"at":now(),"thread_id":failure.thread_id,"thread_name":failure.label,"failed_turn":failure.turn_id,"failed_at":failure.failed_at,"retry_number":number,"delay_seconds":delay,"error":error.map(|value|value.chars().take(300).collect::<String>())}),
        )?;
        file.write_all(b"\n")?;
        Ok(())
    })();
    let _ = result;
}

pub fn recent_events(paths: &Paths) -> Result<Vec<crate::guard::LogEntry>> {
    let mut file = match fs::File::open(paths.runtime.join(LOG)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let start = file.metadata()?.len().saturating_sub(65536);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity(65536);
    file.take(65536).read_to_end(&mut bytes)?;
    Ok(parse_events(&bytes, start > 0, crate::locale::chinese()))
}
fn parse_events(bytes: &[u8], skip_first: bool, zh: bool) -> Vec<crate::guard::LogEntry> {
    let end = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let mut rows = Vec::new();
    for line in bytes[..end]
        .split(|byte| *byte == b'\n')
        .skip(usize::from(skip_first))
    {
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let event = value["event"].as_str().unwrap_or("");
        let label = match (event, zh) {
            ("queued", true) => "等待重试",
            ("queued", false) => "Queued",
            ("send_started", true) => "触发重试",
            ("send_started", false) => "Retry started",
            ("submitted", true) => "已提交继续消息",
            ("submitted", false) => "Continuation submitted",
            ("unconfirmed", true) => "提交未确认",
            ("unconfirmed", false) => "Submission unconfirmed",
            ("skipped", true) => "已取消重试",
            ("skipped", false) => "Retry cancelled",
            ("duplicate_skipped", true) => "已跳过重复回合",
            ("duplicate_skipped", false) => "Duplicate skipped",
            ("blocked", true) => "重试未发起",
            ("blocked", false) => "Retry blocked",
            _ => continue,
        };
        let number = value["retry_number"]
            .as_u64()
            .map(|number| format!(" #{number}"))
            .unwrap_or_default();
        let name = value["thread_name"]
            .as_str()
            .filter(|name| !name.is_empty())
            .or_else(|| value["thread_id"].as_str())
            .unwrap_or("—");
        let time = value["time"]
            .as_str()
            .and_then(|time| chrono::DateTime::parse_from_rfc3339(time).ok())
            .map(|time| time.format("%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "—".into());
        let delay = value["delay_seconds"]
            .as_u64()
            .map(|delay| {
                if zh {
                    format!(" · 等待 {delay} 秒")
                } else {
                    format!(" · Delay {delay}s")
                }
            })
            .unwrap_or_default();
        let reason = value["error"].as_str().unwrap_or("");
        let reason = if zh {
            match reason {
                "disabled, settings changed or old failure" => "扩展关闭、设置变更或错误已过期",
                "latest turn changed, recovered or stopped" => "会话已进入新回合、已恢复或已停止",
                other => other,
            }
        } else {
            reason
        };
        rows.push(crate::guard::LogEntry {
            time,
            title: format!(
                "{label}{number} · {}",
                name.chars().take(100).collect::<String>()
            ),
            detail: format!(
                "{} {} · {} {}{delay}{}{}",
                if zh { "会话" } else { "Chat" },
                value["thread_id"].as_str().unwrap_or("—"),
                if zh { "失败回合" } else { "Failed turn" },
                value["failed_turn"].as_str().unwrap_or("—"),
                if reason.is_empty() { "" } else { " · " },
                reason.chars().take(300).collect::<String>()
            ),
            warning: matches!(event, "unconfirmed" | "blocked")
                || (event == "submitted" && !reason.is_empty()),
        });
    }
    rows.reverse();
    rows.truncate(64);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> Failure {
        Failure {
            thread_id: "chat".into(),
            turn_id: "turn".into(),
            label: "Test".into(),
            failed_at: 1000.,
        }
    }
    #[test]
    fn only_structured_latest_failed_capacity_errors_match() {
        let error = r#"{"message":"Selected model is at capacity. Please try a different model.","codexErrorInfo":"serverOverloaded"}"#;
        assert!(failure("chat", "Test", "turn", "failed", Some(error), Some(1000.)).is_some());
        for status in ["inProgress", "interrupted", "completed"] {
            assert!(failure("chat", "Test", "turn", status, Some(error), Some(1000.)).is_none());
        }
        for other in [
            r#"{"message":"capacity","codexErrorInfo":"usageLimitExceeded"}"#,
            r#"{"message":"Selected model is at capacity. Please try a different model.","codexErrorInfo":"unauthorized"}"#,
            r#"{"message":"network timed out"}"#,
        ] {
            assert!(failure("chat", "Test", "turn", "failed", Some(other), Some(1000.)).is_none());
        }
        assert!(failure(
            "chat",
            "Test",
            "turn",
            "failed",
            Some(r#"{"message":"Selected model is at capacity. Please try a different model."}"#),
            Some(1000.)
        )
        .is_some());
    }
    #[test]
    fn delay_is_five_seconds_and_new_turns_or_manual_recovery_cancel_old_retries() {
        let mut schedule = Schedule::new(Journal::default());
        let failure = sample();
        schedule.sync(&[failure.clone()], 1000.);
        assert!(schedule.take_due(1004., 5).is_none());
        assert_eq!(schedule.take_due(1005., 5), Some(failure.clone()));
        schedule.sync(&[failure.clone()], 1006.);
        assert!(schedule.take_due(1010., 5).is_none());
        let next = Failure {
            turn_id: "new".into(),
            failed_at: 1010.,
            ..failure
        };
        schedule.sync(&[next], 1010.);
        schedule.sync(&[], 1012.);
        assert!(schedule.take_due(1016., 5).is_none());
    }
    #[test]
    fn old_errors_and_attempted_turns_never_restart_and_settings_are_bounded() {
        let failure = sample();
        let mut schedule = Schedule::new(Journal {
            seen: vec![Attempt {
                key: failure.key(),
                failed_at: failure.failed_at,
            }],
            ..Default::default()
        });
        schedule.sync(&[failure.clone()], 1001.);
        assert!(schedule.pending.is_empty());
        schedule = Schedule::new(Journal::default());
        schedule.sync(&[failure], 2000.);
        assert!(schedule.pending.is_empty());
        let mut options: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(options.delay_secs, 5);
        assert!(!options.enabled);
        options.delay_secs = 0;
        options.clamp();
        assert_eq!(options.delay_secs, 1);
        options.delay_secs = 10000;
        options.clamp();
        assert_eq!(options.delay_secs, 300);
    }
    struct Fixture {
        paths: Paths,
    }
    impl Fixture {
        fn new() -> Self {
            let workspace = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .canonicalize()
                .unwrap();
            let tmp = workspace.join("tmp");
            let suffix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = tmp.join(format!(
                "capacity-retry-test-{}-{suffix}",
                std::process::id()
            ));
            assert!(root.starts_with(&tmp));
            fs::create_dir_all(&root).unwrap();
            let state = rusqlite::Connection::open(root.join("state_5.sqlite")).unwrap();
            state.execute_batch("CREATE TABLE threads(id TEXT,title TEXT,name TEXT,rollout_path TEXT,archived INTEGER,agent_path TEXT);").unwrap();
            let rollout = root.join("test-rollout.jsonl");
            fs::write(&rollout, b"{}\n").unwrap();
            state
                .execute(
                    "INSERT INTO threads VALUES('chat','Test','Test',?,0,NULL)",
                    [rollout.to_str().unwrap()],
                )
                .unwrap();
            let history = rusqlite::Connection::open(root.join("thread_history_1.sqlite")).unwrap();
            history.execute_batch("CREATE TABLE thread_turns(thread_id TEXT,turn_id TEXT,status TEXT,error_json TEXT,completed_at REAL,rollout_ordinal INTEGER);").unwrap();
            history.execute("INSERT INTO thread_turns VALUES('chat','turn','failed',?,?,1)",rusqlite::params![r#"{"codexErrorInfo":"serverOverloaded","message":"Selected model is at capacity. Please try a different model."}"#,now()]).unwrap();
            Self {
                paths: Paths {
                    app: root.clone(),
                    home: root.clone(),
                    runtime: root,
                    instance: "test".into(),
                },
            }
        }
        fn state(&self) -> State {
            let mut live = crate::Shared::default();
            live.capacity_options.enabled = true;
            std::sync::Arc::new(std::sync::Mutex::new(live))
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let root = self.paths.runtime.canonicalize().unwrap();
            let tmp = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .canonicalize()
                .unwrap()
                .join("tmp")
                .canonicalize()
                .unwrap();
            assert!(root.starts_with(tmp));
            let _ = fs::remove_dir_all(root);
        }
    }
    #[test]
    fn dispatch_revalidates_live_turn_and_disabled_epoch_before_sending() {
        let fixture = Fixture::new();
        let state = fixture.state();
        let store = Store::new(&fixture.paths);
        let request = Request {
            failure: store.capacity_failure("chat").unwrap().unwrap(),
            epoch: 0,
        };
        configure(
            &state,
            Settings {
                enabled: false,
                delay_secs: 5,
            },
        );
        dispatch(&fixture.paths, &state, &store, &request, |_, _| {
            panic!("Disabled retries cannot send")
        })
        .unwrap();
        configure(
            &state,
            Settings {
                enabled: true,
                delay_secs: 5,
            },
        );
        dispatch(&fixture.paths, &state, &store, &request, |_, _| {
            panic!("Old queued generation cannot send")
        })
        .unwrap();
        let request = Request {
            epoch: 2,
            ..request
        };
        let history =
            rusqlite::Connection::open(fixture.paths.home.join("thread_history_1.sqlite")).unwrap();
        history
            .execute("UPDATE thread_turns SET status='inProgress'", [])
            .unwrap();
        dispatch(&fixture.paths, &state, &store, &request, |_, _| {
            panic!("Recovered chat cannot receive a duplicate continuation")
        })
        .unwrap();
        assert!(!fixture.paths.runtime.join(JOURNAL).exists());
    }
    #[test]
    fn confirmed_or_ambiguous_sends_are_not_replayed_after_restart() {
        for confirmed in [true, false] {
            let fixture = Fixture::new();
            let state = fixture.state();
            let store = Store::new(&fixture.paths);
            let request = Request {
                failure: store.capacity_failure("chat").unwrap().unwrap(),
                epoch: 0,
            };
            let result = dispatch(
                &fixture.paths,
                &state,
                &store,
                &request,
                |thread, prompt| {
                    assert_eq!(thread, "chat");
                    assert_eq!(prompt, PROMPT);
                    if confirmed {
                        Ok(())
                    } else {
                        anyhow::bail!("connection closed before confirmation")
                    }
                },
            );
            assert_eq!(result.is_ok(), confirmed);
            let restarted = fixture.state();
            dispatch(&fixture.paths, &restarted, &store, &request, |_, _| {
                panic!("Same failed turn must not be replayed")
            })
            .unwrap();
            assert_eq!(state.lock().unwrap().capacity_retry.attempts, 1);
            assert!(!state.lock().unwrap().capacity_retry.in_flight);
            assert_eq!(
                state.lock().unwrap().capacity_retry.submitted,
                u64::from(confirmed)
            );
        }
    }
    #[test]
    fn journal_keeps_all_eligible_turns_and_fails_closed_when_full() {
        let fixture = Fixture::new();
        let at = now();
        let mut journal = Journal {
            seen: (0..1024)
                .map(|index| Attempt {
                    key: format!("chat:{index}"),
                    failed_at: at,
                })
                .collect(),
            ..Default::default()
        };
        let next = Failure {
            turn_id: "next".into(),
            failed_at: at,
            ..sample()
        };
        assert!(journal.claim(&fixture.paths, &next).is_err());
        assert_eq!(journal.seen.first().unwrap().key, "chat:0");
        journal.seen[0].failed_at = at - MAX_AGE - 10.;
        assert!(journal.claim(&fixture.paths, &next).unwrap());
        let restored = Journal::for_send(&fixture.paths).unwrap();
        assert_eq!(restored.seen.len(), 1024);
        assert!(restored.seen.iter().any(|attempt| attempt.key == "chat:1"));
        assert!(restored
            .seen
            .iter()
            .any(|attempt| attempt.key == next.key()));
    }
    #[test]
    fn retry_totals_persist_and_dedup_does_not_increment_them() {
        let fixture = Fixture::new();
        let state = fixture.state();
        let store = Store::new(&fixture.paths);
        let first = Request {
            failure: store.capacity_failure("chat").unwrap().unwrap(),
            epoch: 0,
        };
        dispatch(&fixture.paths, &state, &store, &first, |_, _| Ok(())).unwrap();
        let history =
            rusqlite::Connection::open(fixture.paths.home.join("thread_history_1.sqlite")).unwrap();
        history
            .execute(
                "UPDATE thread_turns SET turn_id='second',completed_at=?",
                [now()],
            )
            .unwrap();
        let second = Request {
            failure: store.capacity_failure("chat").unwrap().unwrap(),
            epoch: 0,
        };
        assert!(dispatch(
            &fixture.paths,
            &state,
            &store,
            &second,
            |_, _| anyhow::bail!("unconfirmed transport")
        )
        .is_err());
        dispatch(&fixture.paths, &state, &store, &second, |_, _| {
            panic!("Duplicate must not submit")
        })
        .unwrap();
        let restored = Journal::for_send(&fixture.paths).unwrap();
        let mut view = View::default();
        restored.counters.apply(&mut view);
        assert_eq!((view.attempts, view.submitted, view.unconfirmed), (2, 1, 1));
        assert!(view.tracking_since > 0.);
        let legacy: Journal = serde_json::from_str(r#"{"seen":[]}"#).unwrap();
        assert_eq!(
            legacy.counters.attempts, 0,
            "Legacy dedup records are not presented as complete historical counts"
        );
        let events = recent_events(&fixture.paths).unwrap();
        assert!(events.iter().any(|event| event.title.contains("#2")
            && event.warning
            && event.detail.contains("second")));
        assert!(events
            .iter()
            .any(|event| event.title.contains("#1") && event.detail.contains("5")));
    }
    #[test]
    fn retry_log_tail_ignores_partial_rows_and_exposes_chat_and_turn_context() {
        let bytes=b"cut first row\n{\"event\":\"send_started\",\"time\":\"2026-10-26T19:00:00+08:00\",\"thread_name\":\"Example\",\"thread_id\":\"chat-123\",\"failed_turn\":\"turn-456\",\"retry_number\":7,\"delay_seconds\":5}\n{\"event\":\"unconfirmed\",\"thread_id\":\"chat-123\",\"failed_turn\":\"turn-456\",\"error\":\"connection lost\"}\n{\"event\":\"submitted\"";
        let rows = parse_events(bytes, true, false);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].warning && rows[0].detail.contains("connection lost"));
        assert_eq!(rows[1].time, "10-26 19:00:00");
        assert!(
            rows[1].title.contains("#7")
                && rows[1].detail.contains("chat-123")
                && rows[1].detail.contains("turn-456")
        );
        let zh = parse_events(bytes, true, true);
        assert!(zh[1].title.contains("触发重试"));
    }
    #[test]
    fn concurrent_event_writes_remain_complete_and_logs_rotate_compressed() {
        let fixture = Fixture::new();
        let mut writers = Vec::new();
        for _ in 0..4 {
            let paths = fixture.paths.clone();
            writers.push(std::thread::spawn(move || {
                for _ in 0..20 {
                    log(&paths, "queued", &sample(), None);
                }
            }));
        }
        for writer in writers {
            writer.join().unwrap();
        }
        assert_eq!(recent_events(&fixture.paths).unwrap().len(), 64);
        let file = OpenOptions::new()
            .write(true)
            .open(fixture.paths.runtime.join(LOG))
            .unwrap();
        file.set_len(128 * 1024).unwrap();
        drop(file);
        log(&fixture.paths, "send_started", &sample(), None);
        assert!(fixture
            .paths
            .runtime
            .join("capacity-retry-events.1.gz")
            .is_file());
        assert_eq!(recent_events(&fixture.paths).unwrap().len(), 1);
    }
    #[test]
    fn cached_failures_and_a_corrupt_retry_journal_cannot_trigger_messages() {
        let fixture = Fixture::new();
        let state = fixture.state();
        let store = Store::new(&fixture.paths);
        let request = Request {
            failure: store.capacity_failure("chat").unwrap().unwrap(),
            epoch: 0,
        };
        let recent = crate::data::Recent {
            capacity_failures: vec![request.failure.clone()],
            capacity_available: true,
            ..Default::default()
        };
        let restored: crate::data::Recent =
            serde_json::from_value(serde_json::to_value(recent).unwrap()).unwrap();
        assert!(restored.capacity_failures.is_empty());
        assert!(!restored.capacity_available);
        fs::write(fixture.paths.runtime.join(JOURNAL), b"not json").unwrap();
        assert!(
            dispatch(&fixture.paths, &state, &store, &request, |_, _| panic!(
                "Unusable dedupe record must stop sending"
            ))
            .is_err()
        );
    }
}
