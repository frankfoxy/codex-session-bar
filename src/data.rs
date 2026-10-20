use crate::{now, read_json, Paths};
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    time::Duration,
};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Turn {
    #[serde(default)]
    pub status: String,
    pub started_at: Option<f64>,
    pub completed_at: Option<f64>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Rate {
    pub value: f64,
    pub start: f64,
    pub end: f64,
    pub output_tokens: u64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub label: String,
    pub model: String,
    pub cwd: String,
    pub tokens_used: Option<u64>,
    pub turn: Turn,
    pub usage: Value,
    pub token_at: Option<String>,
    pub output_rate: Option<Rate>,
}
impl Snapshot {
    pub fn tokens(&self) -> Option<u64> {
        self.usage
            .pointer("/total_token_usage/total_tokens")
            .and_then(Value::as_u64)
            .or(self.tokens_used)
    }
    pub fn rate(&self) -> Option<f64> {
        let rate = self.output_rate.as_ref()?;
        let start = self.turn.started_at?;
        (rate.start >= start && (0.0..=30.0).contains(&(now() - rate.end))).then_some(rate.value)
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Row {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(skip)]
    pub fallback_title: bool,
    #[serde(default = "unknown")]
    pub state: String,
    #[serde(default)]
    pub updated_at: f64,
    #[serde(default)]
    pub tokens: Option<u64>,
    #[serde(default)]
    pub tps: Option<f64>,
    #[serde(default)]
    pub rate_at: Option<f64>,
    #[serde(default)]
    pub started_at: Option<f64>,
    #[serde(default)]
    pub total_seconds: Option<f64>,
}
fn unknown() -> String {
    "Unknown".into()
}
fn display_title(name: Option<&str>, title: &str) -> String {
    if let Some(name) = name.filter(|name| !name.trim().is_empty()) {
        return name.to_string();
    }
    let title = title.trim();
    let envelope = title.starts_with("# Files")
        || title.starts_with("## Referenced chats")
        || title.starts_with("<environment_context>");
    let request = match title.split_once("## My request:") {
        Some((_, request)) => request.trim(),
        None if envelope => return "Untitled chat".into(),
        None => title,
    };
    for line in request.lines() {
        let mut line = line.trim();
        // A leading attachment/chat mention is context, not the user's title.
        while line.starts_with('[') {
            let Some(link) = line.find("](") else {
                break;
            };
            let Some(end) = line[link + 2..].find(')') else {
                break;
            };
            line = line[link + 2 + end + 1..].trim();
        }
        if line.is_empty() || line.starts_with("<image") || line.starts_with("Image attachment:") {
            continue;
        }
        let text: String = line.chars().take(96).collect();
        return format!("{text}{}", if line.chars().count() > 96 { "…" } else { "" });
    }
    "Untitled chat".into()
}
pub fn visible_chat_title(label: &str) -> Option<String> {
    let label = label.trim();
    match label.to_lowercase().as_str() {
        "" | "codex" | "chatgpt" | "home" | "首页" => None,
        "new chat" | "new conversation" | "新聊天" | "新建会话" => Some("New chat".into()),
        _ => Some(display_title(None, label)),
    }
}
impl Row {
    pub fn rate(&self) -> Option<f64> {
        self.tps.filter(|v| {
            v.is_finite()
                && *v >= 0.0
                && self
                    .rate_at
                    .is_some_and(|at| (0.0..=30.0).contains(&(now() - at)))
        })
    }
    pub fn status_text(&self) -> String {
        format!(
            "{} {}",
            match self.state.as_str() {
                "Busy" => "●",
                "Done" => "✓",
                "Waiting" => "◷",
                "Stopped" => "Ⅱ",
                "Failed" => "!",
                _ => "○",
            },
            self.state
        )
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Recent {
    #[serde(default)]
    pub all_rows: Vec<Row>,
    #[serde(default)]
    pub rows: Vec<Row>,
    #[serde(default)]
    pub busy_metrics: Vec<Row>,
    #[serde(default)]
    pub busy_count: Option<usize>,
    #[serde(default)]
    pub loaded_at: f64,
    #[serde(default)]
    pub stale: bool,
    #[serde(default)]
    pub error: String,
}
impl Recent {
    pub fn totals(&self) -> (Option<u64>, Option<f64>, bool) {
        let mut tokens = None;
        let mut rate = None;
        let mut sources = (0, 0);
        for row in &self.busy_metrics {
            if let Some(t) = row.tokens {
                tokens = Some(tokens.unwrap_or(0) + t);
                sources.0 += 1;
            }
            if let Some(r) = row.rate() {
                rate = Some(rate.unwrap_or(0.0) + r);
                sources.1 += 1;
            }
        }
        if self.busy_count == Some(0) {
            return (Some(0), Some(0.0), false);
        }
        (
            tokens,
            rate,
            self.busy_count
                .is_some_and(|c| c != sources.0 || c != sources.1),
        )
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Quota {
    pub bucket: String,
    pub label: String,
    pub remaining: f64,
    pub reset: Option<f64>,
    pub minutes: Option<f64>,
}

pub fn quotas(value: &Value) -> Vec<Quota> {
    let mut result = Vec::new();
    let fallback =
        serde_json::json!({"codex":value.get("rateLimits").cloned().unwrap_or_default()});
    let buckets = value
        .get("rateLimitsByLimitId")
        .filter(|v| v.is_object() && !v.as_object().unwrap().is_empty())
        .unwrap_or(&fallback);
    if let Some(map) = buckets.as_object() {
        for (key, bucket) in map {
            for slot in ["primary", "secondary"] {
                let window = &bucket[slot];
                if let Some(used) = window["usedPercent"].as_f64().filter(|v| v.is_finite()) {
                    let minutes = window["windowDurationMins"].as_f64();
                    let label = match minutes {
                        Some(m) if m > 0.0 && m % 1440.0 == 0.0 => format!("{}d", m / 1440.0),
                        Some(m) if m > 0.0 && m % 60.0 == 0.0 => format!("{}h", m / 60.0),
                        Some(m) => format!("{m}m"),
                        None => slot.into(),
                    };
                    result.push(Quota {
                        bucket: bucket["limitName"].as_str().unwrap_or(key).into(),
                        label,
                        remaining: (100.0 - used).clamp(0.0, 100.0),
                        reset: window["resetsAt"].as_f64(),
                        minutes,
                    });
                }
            }
        }
    }
    result
}
pub fn compact(value: Option<u64>) -> String {
    match value {
        None => "—".into(),
        Some(v) if v >= 1_000_000 => format!("{:.1}M", v as f64 / 1_000_000.0),
        Some(v) if v >= 1000 => format!("{:.1}k", v as f64 / 1000.0),
        Some(v) => v.to_string(),
    }
}
pub fn duration(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}
pub fn stable_rows(previous: &[Row], incoming: &[Row]) -> Vec<Row> {
    if previous.is_empty() {
        return incoming.to_vec();
    }
    let incoming: HashMap<_, _> = incoming.iter().map(|r| (r.id.as_str(), r)).collect();
    previous
        .iter()
        .map(|old| {
            incoming
                .get(old.id.as_str())
                .copied()
                .unwrap_or(old)
                .clone()
        })
        .collect()
}

#[derive(Clone, Debug, Deserialize)]
pub struct Emoji {
    pub emoji: String,
    pub name: String,
    pub group: String,
    #[serde(default)]
    pub subgroup: String,
}
#[derive(Clone)]
pub struct Catalog {
    pub entries: Vec<Emoji>,
    pub defaults: Vec<String>,
    prefixes: HashMap<char, Vec<String>>,
}
impl Catalog {
    pub fn load(paths: &Paths) -> Result<Self> {
        let value = read_json(&paths.app.join("emoji_catalog.json"), 1024 * 1024);
        let entries: Vec<Emoji> = serde_json::from_value(value["entries"].clone())?;
        let defaults: Vec<String> = "🔴 🟠 🟡 🟢 🔵 🟣 🟤 ⚫ ⚪ 🟥 🟧 🟨 🟩 🟦 🟪 🟫 ⬛ ⬜ 🚧 ✅ ⏸️ ⏳ ❗ 📌 ⭐ 🔥 🆕 👀 🔁 🧊 🔒 ❌ 🎯 🏁 💯 ⚠️ 🐛 🔍 🩺 🚨 💥 🛠️ 🩹 🧹 🔬 ⏱️ 🔌 📦 🧩 ⚡ 📡 🎛️ 🧮 🔋 💻 🧪 📝 🚀 ♻️ 🔀 🌿 ⚙️ 🐍 🌐 🤖 🔐 💡 📚 📊 📈 🧠 ❓ 💬 📧 📅 👥 📋 💰 🧾 📞 🌟 🎉 ❤️ 🍀 ☕ 🌙 🏠 ✈️ 🎮 🐱".split_whitespace().map(String::from).collect();
        let mut prefixes: HashMap<char, Vec<String>> = HashMap::new();
        for e in &entries {
            if let Some(c) = e.emoji.chars().next() {
                prefixes.entry(c).or_default().push(e.emoji.clone());
            }
        }
        for list in prefixes.values_mut() {
            list.sort_by_key(|s| std::cmp::Reverse(s.len()));
        }
        Ok(Self {
            entries,
            defaults,
            prefixes,
        })
    }
    pub fn strip<'a>(&self, title: &'a str) -> &'a str {
        let title = title.trim();
        if let Some(prefixes) = title.chars().next().and_then(|c| self.prefixes.get(&c)) {
            for p in prefixes {
                if let Some(rest) = title.strip_prefix(p.as_str()) {
                    return rest.trim_start();
                }
            }
        }
        title
    }
    pub fn icon(&self, title: &str) -> String {
        let title = title.trim();
        let stripped = self.strip(title);
        let prefix = title[..title.len() - stripped.len()].trim();
        if prefix.is_empty() {
            "◈".into()
        } else {
            prefix.into()
        }
    }
    pub fn rename(&self, title: &str, icon: &str) -> String {
        format!("{} {}", icon, self.strip(title)).trim().into()
    }
    pub fn suggest(&self, title: &str) -> String {
        let title = self.strip(title).to_lowercase();
        let rules: &[(&[&str], &str)] = &[
            (&["无人机", "drone", "uav"], "🚁"),
            (&["session bar", "悬浮条", "工具条"], "🎛️"),
            (
                &["死机", "蓝屏", "系统问题", "crash", "修复", "debug"],
                "🛠️",
            ),
            (&["memory", "内存", "sram", "ddr", "ram_"], "💾"),
            (&["yolo", "神经", "识别", "ai 模块"], "🤖"),
            (&["vllm", "大模型", "llm"], "🧠"),
            (&["相机", "camera", "图像", "jpeg", "vision", "条码"], "📷"),
            (&["drc", "pcb", "altium", "电路", "原理图"], "📐"),
            (&["usb", "mipi", "接口", "串口"], "🔌"),
            (&["gige", "网络", "socket", "隧道", "以太网"], "🌐"),
            (&["rtl", "fpga", "verilog", "仿真"], "🧩"),
            (&["python", "脚本"], "🐍"),
            (&["文档", "readme", "spec", "说明"], "📝"),
            (&["部署", "安装", "deploy", "install"], "🚀"),
            (&["统计", "分析", "report", "数据"], "📊"),
        ];
        let icon = rules
            .iter()
            .find(|(keys, _)| keys.iter().any(|k| title.contains(k)))
            .map(|(_, icon)| *icon)
            .unwrap_or("💬");
        self.entries
            .iter()
            .find(|e| e.emoji == icon)
            .map(|e| e.emoji.clone())
            .unwrap_or_else(|| "💬".into())
    }
    pub fn filter(&self, query: &str, group: &str) -> Vec<usize> {
        let q = query.trim().to_lowercase();
        let include = |entry: &Emoji| {
            q.is_empty()
                || format!(
                    "{} {} {} {}",
                    entry.emoji, entry.name, entry.group, entry.subgroup
                )
                .to_lowercase()
                .contains(&q)
        };
        if q.is_empty() && group == "默认集合" {
            return self
                .defaults
                .iter()
                .filter_map(|icon| self.entries.iter().position(|e| e.emoji == *icon))
                .filter(|i| include(&self.entries[*i]))
                .collect();
        }
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                (!q.is_empty() || group == "All emoji" || e.group == group) && include(e)
            })
            .map(|(i, _)| i)
            .collect()
    }
}

#[derive(Clone, Default)]
struct TokenData {
    usage: Value,
    at: Option<String>,
    rate: Option<Rate>,
}
struct Cache {
    size: u64,
    modified: std::time::SystemTime,
    offset: u64,
    data: TokenData,
}
pub struct Store {
    paths: Paths,
    cache: HashMap<String, Cache>,
}
impl Store {
    pub fn new(paths: &Paths) -> Self {
        Self {
            paths: paths.clone(),
            cache: HashMap::new(),
        }
    }
    fn db(&self, name: &str) -> Result<Connection> {
        let db = Connection::open_with_flags(
            self.paths.home.join(name),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        db.busy_timeout(Duration::from_millis(500))?;
        db.execute_batch("PRAGMA query_only=ON; PRAGMA temp_store=MEMORY;")?;
        Ok(db)
    }
    pub fn sessions(&self) -> Result<Vec<Row>> {
        let db = self.db("state_5.sqlite")?;
        let mut query=db.prepare("SELECT id,title,updated_at,tokens_used,name FROM threads WHERE archived=0 AND agent_path IS NULL ORDER BY updated_at DESC,id")?;
        let rows = query
            .query_map([], |r| {
                Ok(Row {
                    id: r.get(0)?,
                    label: display_title(
                        r.get::<_, Option<String>>(4)?.as_deref(),
                        &r.get::<_, String>(1)?,
                    ),
                    fallback_title: r
                        .get::<_, Option<String>>(4)?
                        .is_none_or(|v| v.trim().is_empty()),
                    updated_at: r.get(2)?,
                    tokens: r.get(3)?,
                    state: unknown(),
                    ..Row::default()
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
    pub fn resolve(&self, label: &str) -> Result<Option<String>> {
        if label.trim().is_empty() {
            return Ok(None);
        }
        let db = self.db("state_5.sqlite")?;
        let mut q=db.prepare("SELECT id FROM threads WHERE coalesce(nullif(name,''),title)=? AND agent_path IS NULL AND archived=0 LIMIT 2")?;
        let ids = q
            .query_map([label], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if !ids.is_empty() {
            return Ok(if ids.len() == 1 {
                ids.into_iter().next()
            } else {
                None
            });
        }
        // The frontend can still show the first prompt while its generated name
        // has already been persisted. Accept that exact alias only if unique.
        let mut query = db.prepare(
            "SELECT id FROM threads WHERE title=? AND agent_path IS NULL AND archived=0 LIMIT 2",
        )?;
        let ids = query
            .query_map([label], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(if ids.len() == 1 {
            ids.into_iter().next()
        } else {
            None
        })
    }
    pub fn label(&self, id: &str) -> Result<String> {
        self.db("state_5.sqlite")?
            .query_row("SELECT name,title FROM threads WHERE id=?", [id], |r| {
                Ok(display_title(
                    r.get::<_, Option<String>>(0)?.as_deref(),
                    &r.get::<_, String>(1)?,
                ))
            })
            .context("Chat not found")
    }
    pub fn read(&mut self, id: &str) -> Result<Snapshot> {
        let db = self.db("state_5.sqlite")?;
        let (mut snap, path) = db.query_row(
            "SELECT id,title,rollout_path,tokens_used,model,cwd,name FROM threads WHERE id=?",
            [id],
            |r| {
                Ok((
                    Snapshot {
                        id: r.get(0)?,
                        label: display_title(
                            r.get::<_, Option<String>>(6)?.as_deref(),
                            &r.get::<_, String>(1)?,
                        ),
                        tokens_used: r.get(3)?,
                        model: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                        cwd: r.get(5)?,
                        ..Snapshot::default()
                    },
                    r.get::<_, String>(2)?,
                ))
            },
        )?;
        snap.turn=self.db("thread_history_1.sqlite")?.query_row("SELECT status,started_at,completed_at FROM thread_turns WHERE thread_id=? ORDER BY rollout_ordinal DESC LIMIT 1",[id],|r|Ok(Turn{status:r.get(0)?,started_at:r.get(1)?,completed_at:r.get(2)?})).optional()?.unwrap_or_default();
        if let Ok(tokens) = self.tokens(&path) {
            snap.usage = tokens.usage;
            snap.token_at = tokens.at;
            snap.output_rate = tokens.rate;
        }
        Ok(snap)
    }
    fn tokens(&mut self, path: &str) -> Result<TokenData> {
        let meta = fs::metadata(path)?;
        let modified = meta.modified()?;
        let size = meta.len();
        if let Some(old) = self.cache.get(path) {
            if old.size == size && old.modified == modified {
                return Ok(old.data.clone());
            }
        }
        let mut start = size.saturating_sub(262144);
        let mut data = TokenData::default();
        if let Some(old) = self.cache.get(path) {
            if size >= old.size {
                data = old.data.clone();
                if size - old.size <= 262144 {
                    start = old.offset;
                }
            }
        }
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::with_capacity(262144);
        file.take(262144).read_to_end(&mut bytes)?;
        let end = bytes
            .iter()
            .rposition(|b| *b == b'\n')
            .map(|p| p + 1)
            .unwrap_or(0);
        for line in bytes[..end].split(|b| *b == b'\n') {
            let Ok(event) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            let payload = &event["payload"];
            if event["type"] != "event_msg"
                || payload["type"] != "token_count"
                || !payload["info"].is_object()
            {
                continue;
            }
            let at = event["timestamp"].as_str().map(String::from);
            if let (Some(before), Some(after), Some(old), Some(new)) = (
                data.at.as_deref().and_then(timestamp),
                at.as_deref().and_then(timestamp),
                data.usage
                    .pointer("/total_token_usage/output_tokens")
                    .and_then(Value::as_u64),
                payload
                    .pointer("/info/total_token_usage/output_tokens")
                    .and_then(Value::as_u64),
            ) {
                if after > before && new > old {
                    data.rate = Some(Rate {
                        value: (new - old) as f64 / (after - before),
                        start: before,
                        end: after,
                        output_tokens: new - old,
                    });
                }
            }
            data.usage = payload["info"].clone();
            data.at = at;
        }
        if self.cache.len() > 128 {
            self.cache.clear();
        }
        self.cache.insert(
            path.into(),
            Cache {
                size,
                modified,
                offset: start + end as u64,
                data: data.clone(),
            },
        );
        Ok(data)
    }
    pub fn recent(&mut self, payload: &Value) -> Result<Recent> {
        let history = self.db("thread_history_1.sqlite")?;
        let entries: HashMap<_, _> = ["pinnedThreads", "threads"]
            .iter()
            .flat_map(|key| payload[*key].as_array().into_iter().flatten())
            .filter(|entry| {
                entry["kind"] == "codex" && !entry["hostId"].as_str().is_some_and(|v| v != "local")
            })
            .filter_map(|entry| entry["id"].as_str().map(|id| (id, entry)))
            .collect();
        // Local metadata supplies the complete list even when the app's status
        // snapshot is bounded. Fetch token logs only for confirmed busy chats.
        let mut rows = self.sessions()?;
        let local =
            crate::body::local_ids(&self.paths, rows.iter().map(|r| r.id.clone()).collect())?;
        rows.retain(|row| local.contains(&row.id));
        let mut turns = history.prepare("SELECT status FROM thread_turns WHERE thread_id=? ORDER BY rollout_ordinal DESC LIMIT 1")?;
        let mut durations = history.prepare("SELECT SUM(CASE WHEN started_at IS NOT NULL AND completed_at >= started_at THEN completed_at-started_at END) FROM thread_turns WHERE thread_id=?")?;
        for row in &mut rows {
            row.total_seconds = durations.query_row([&row.id], |r| r.get(0))?;
            let entry = entries
                .get(row.id.as_str())
                .copied()
                .unwrap_or(&Value::Null);
            if row.fallback_title {
                if let Some(title) = entry["title"]
                    .as_str()
                    .filter(|title| !title.trim().is_empty())
                {
                    row.label = display_title(None, title);
                }
            }
            let turn = turns
                .query_row([&row.id], |r| r.get::<_, String>(0))
                .optional()?
                .unwrap_or_default();
            let state = match entry["status"].as_str().unwrap_or("") {
                "active" | "busy" | "running" | "inProgress" => "Busy",
                "needsAttention" | "needs_attention" | "waiting" | "blocked" => "Waiting",
                "idle" | "completed" => match turn.as_str() {
                    "completed" => "Done",
                    "failed" => "Failed",
                    "interrupted" => "Stopped",
                    _ => "Ready",
                },
                _ => match turn.as_str() {
                    "completed" => "Done",
                    "failed" => "Failed",
                    "interrupted" => "Stopped",
                    _ => "Unknown",
                },
            };
            row.state = state.into();
            let updated = entry["updatedAt"]
                .as_f64()
                .map(|t| if t > 1e12 { t / 1000. } else { t })
                .or_else(|| entry["updatedAt"].as_str().and_then(timestamp))
                .unwrap_or(0.);
            row.updated_at = row.updated_at.max(updated);
            if state == "Busy" {
                if let Ok(snapshot) = self.read(&row.id) {
                    row.tokens = snapshot.tokens();
                    row.started_at = snapshot.turn.started_at;
                    row.tps = snapshot.rate();
                    row.rate_at = snapshot.output_rate.as_ref().map(|r| r.end);
                }
            }
        }
        let busy_metrics: Vec<Row> = rows.iter().filter(|r| r.state == "Busy").cloned().collect();
        rows.sort_by(|a, b| b.updated_at.total_cmp(&a.updated_at).then(a.id.cmp(&b.id)));
        let mut all_rows = rows.clone();
        all_rows.sort_by_key(|r| r.state != "Busy");
        Ok(Recent {
            all_rows,
            rows,
            busy_count: Some(busy_metrics.len()),
            busy_metrics,
            loaded_at: now(),
            ..Recent::default()
        })
    }
}
fn timestamp(value: &str) -> Option<f64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|t| t.timestamp_millis() as f64 / 1000.0)
}
pub fn load_recent(paths: &Paths) -> Recent {
    let mut recent: Recent = serde_json::from_value(read_json(
        &paths.runtime.join("recent-cache.json"),
        1024 * 1024,
    ))
    .unwrap_or_default();
    recent.stale = true;
    for row in recent
        .rows
        .iter_mut()
        .chain(&mut recent.all_rows)
        .chain(&mut recent.busy_metrics)
    {
        if row.label.starts_with("# Files") || row.label.starts_with("## Referenced chats") {
            row.label = display_title(None, &row.label);
        }
    }
    // Upgrade old caches, whose `rows` contained only ten busy-first entries.
    if !recent.all_rows.is_empty() {
        recent.rows = recent.all_rows.clone();
    }
    recent
        .rows
        .sort_by(|a, b| b.updated_at.total_cmp(&a.updated_at).then(a.id.cmp(&b.id)));
    if let Ok(local) =
        crate::body::local_ids(paths, recent.rows.iter().map(|r| r.id.clone()).collect())
    {
        recent.rows.retain(|r| local.contains(&r.id));
        recent.all_rows.retain(|r| local.contains(&r.id));
        recent.busy_metrics.retain(|r| local.contains(&r.id));
        if recent.busy_count.is_some() {
            recent.busy_count = Some(recent.busy_metrics.len());
        }
    }
    recent
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn named_titles_win_and_attachment_envelopes_do_not_become_names() {
        let prompt = "# Files mentioned by the user:\n输出报告.png\n## Referenced chats with Codex:\ncontext\n## My request:\n[@Reference](thread://ref)\n做一个 3D 模型\n放到工程目录";
        assert_eq!(display_title(Some("📷 我的会话"), prompt), "📷 我的会话");
        assert_eq!(display_title(None, prompt), "做一个 3D 模型");
        assert!(!display_title(None, prompt).contains("输出"));
        assert_eq!(
            display_title(None, "## Referenced chats with Codex:\nmetadata"),
            "Untitled chat"
        );
        assert_eq!(
            display_title(None, "[top.v](D:/project/top.v) 输出接口设计\n详细正文"),
            "输出接口设计"
        );
    }
    fn fixture(name: &str) -> (Paths, std::path::PathBuf) {
        let paths = Paths::discover().unwrap();
        let directory = paths
            .runtime
            .join(format!("rust-fixture-{}-{name}", std::process::id()));
        assert!(directory.starts_with(&paths.runtime));
        std::fs::create_dir_all(&directory).unwrap();
        (
            Paths {
                home: directory.clone(),
                ..paths
            },
            directory,
        )
    }

    #[test]
    fn incremental_tokens_preserve_partial_lines_and_reset_after_truncation() {
        let (paths, dir) = fixture("tokens");
        let file = dir.join("rollout.jsonl");
        let event = |at: &str, output: u64| {
            serde_json::json!({"type":"event_msg","timestamp":at,"payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":output+100,"output_tokens":output}}}}).to_string()
        };
        let first = event("2026-10-19T00:00:00Z", 100);
        let second = event("2026-10-19T00:00:10Z", 300);
        let half = second.len() / 2;
        std::fs::write(&file, format!("{first}\n{}", &second[..half])).unwrap();
        let mut store = Store::new(&paths);
        let path = file.to_str().unwrap();
        assert_eq!(
            store
                .tokens(path)
                .unwrap()
                .usage
                .pointer("/total_token_usage/output_tokens")
                .and_then(Value::as_u64),
            Some(100)
        );
        use std::io::Write;
        let mut stream = std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap();
        write!(stream, "{}\n", &second[half..]).unwrap();
        drop(stream);
        let parsed = store.tokens(path).unwrap();
        assert_eq!(parsed.rate.unwrap().value, 20.0);
        std::fs::write(&file, format!("{first}\n")).unwrap();
        let reset = store.tokens(path).unwrap();
        assert!(reset.rate.is_none());
    }

    #[test]
    fn title_identity_excludes_archived_subagents_and_ambiguous_names() {
        let (paths, dir) = fixture("identity");
        let db = Connection::open(dir.join("state_5.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE threads(id TEXT,name TEXT,title TEXT,archived INTEGER,agent_path TEXT,updated_at INTEGER); INSERT INTO threads VALUES('one',NULL,'unique',0,NULL,10),('old',NULL,'unique',1,NULL,9),('child',NULL,'unique',0,'agent',8),('two',NULL,'duplicate',0,NULL,7),('three',NULL,'duplicate',0,NULL,6);").unwrap();
        db.execute_batch("ALTER TABLE threads ADD COLUMN tokens_used INTEGER DEFAULT 12345;")
            .unwrap();
        drop(db);
        let store = Store::new(&paths);
        assert_eq!(store.resolve("unique").unwrap().as_deref(), Some("one"));
        assert!(store.resolve("duplicate").unwrap().is_none());
        assert!(store.resolve("missing").unwrap().is_none());
        assert_eq!(store.sessions().unwrap().len(), 3);
        let db = Connection::open(dir.join("state_5.sqlite")).unwrap();
        db.execute_batch("UPDATE threads SET name='Generated name' WHERE id='one'; UPDATE threads SET name='Second name' WHERE id='two'; UPDATE threads SET name='Third name' WHERE id='three';").unwrap();
        assert_eq!(store.resolve("unique").unwrap().as_deref(), Some("one"));
        assert_eq!(
            store.resolve("Generated name").unwrap().as_deref(),
            Some("one")
        );
        assert!(store.resolve("duplicate").unwrap().is_none());
        assert_eq!(
            store.resolve("Second name").unwrap().as_deref(),
            Some("two")
        );
        assert!(store.resolve("  ").unwrap().is_none());
    }

    #[test]
    fn visible_title_covers_new_chat_before_metadata_arrives() {
        assert_eq!(visible_chat_title("New chat").as_deref(), Some("New chat"));
        assert_eq!(visible_chat_title("新建会话").as_deref(), Some("New chat"));
        assert_eq!(
            visible_chat_title("📷 相机采集").as_deref(),
            Some("📷 相机采集")
        );
        for label in ["", "Codex", "ChatGPT", "Home", "首页"] {
            assert!(visible_chat_title(label).is_none());
        }
    }

    #[test]
    fn recent_includes_all_local_chats_in_recency_order_even_beyond_rpc_window() {
        let (paths, dir) = fixture("all-recent");
        let mut db = Connection::open(dir.join("state_5.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE threads(id TEXT,name TEXT,title TEXT,archived INTEGER,agent_path TEXT,updated_at INTEGER);").unwrap();
        let tx = db.transaction().unwrap();
        for i in 0..80 {
            tx.execute(
                "INSERT INTO threads VALUES(?,NULL,?,0,NULL,?)",
                rusqlite::params![format!("chat-{i}"), format!("会话 {i}"), 10000 - i],
            )
            .unwrap();
        }
        tx.execute(
            "INSERT INTO threads VALUES('child',NULL,'child',0,'agent',20000)",
            [],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO threads VALUES('archived',NULL,'old',1,NULL,30000)",
            [],
        )
        .unwrap();
        tx.commit().unwrap();
        db.execute_batch("ALTER TABLE threads ADD COLUMN tokens_used INTEGER DEFAULT 27190000;")
            .unwrap();
        db.execute_batch("ALTER TABLE threads ADD COLUMN rollout_path TEXT;
            INSERT INTO threads(id,title,archived,updated_at) VALUES('metadata-only','not on this device',0,50000);").unwrap();
        let history = Connection::open(dir.join("thread_history_1.sqlite")).unwrap();
        history.execute_batch("CREATE TABLE thread_turns(thread_id TEXT,status TEXT,rollout_ordinal INTEGER); INSERT INTO thread_turns VALUES('chat-0','completed',1);").unwrap();
        history.execute_batch("ALTER TABLE thread_turns ADD COLUMN started_at REAL; ALTER TABLE thread_turns ADD COLUMN completed_at REAL;
            UPDATE thread_turns SET started_at=100,completed_at=160;
            INSERT INTO thread_turns VALUES('chat-0','completed',2,10000,10120),('chat-0','failed',3,20000,20030),('chat-0','interrupted',4,30000,NULL),('chat-0','completed',5,40000,39999);").unwrap();
        history.execute_batch("CREATE TABLE thread_items(thread_id TEXT,item_id TEXT,item_type TEXT,updated_at_ordinal INTEGER,item_json TEXT);").unwrap();
        for i in 0..79 {
            history
                .execute(
                    "INSERT INTO thread_items VALUES(?,'m','userMessage',1,'{}')",
                    [format!("chat-{i}")],
                )
                .unwrap();
        }
        let rollout = dir.join("local-chat.jsonl");
        std::fs::write(&rollout, b"{}\n").unwrap();
        db.execute(
            "UPDATE threads SET rollout_path=? WHERE id='chat-79'",
            [rollout.to_str().unwrap()],
        )
        .unwrap();
        let mut store = Store::new(&paths);
        let recent = store.recent(&serde_json::json!({"threads":[{"kind":"codex","hostId":"local","id":"chat-79","status":"active"}]})).unwrap();
        assert_eq!(recent.rows.len(), 80);
        assert!(recent.rows.iter().all(|row| row.id != "metadata-only"));
        assert_eq!(recent.rows[0].id, "chat-0");
        assert_eq!(recent.rows[0].state, "Done");
        assert_eq!(recent.rows[0].tokens, Some(27190000));
        assert_eq!(recent.rows[0].total_seconds, Some(210.));
        assert_eq!(recent.rows[1].total_seconds, None);
        assert_eq!(recent.rows[79].id, "chat-79");
        assert_eq!(recent.rows[79].state, "Busy");
        assert_eq!(recent.all_rows[0].id, "chat-79");
        assert_eq!(recent.busy_count, Some(1));
        assert_eq!(recent.rows[1].state, "Unknown");
    }

    #[test]
    fn throughput_from_previous_turn_is_not_displayed() {
        let snapshot = Snapshot {
            turn: Turn {
                started_at: Some(now() - 10.0),
                ..Turn::default()
            },
            output_rate: Some(Rate {
                value: 100.0,
                start: now() - 20.0,
                end: now() - 1.0,
                ..Rate::default()
            }),
            ..Snapshot::default()
        };
        assert!(snapshot.rate().is_none());
    }
    #[test]
    fn open_order_stays_fixed() {
        let old = vec![
            Row {
                id: "one".into(),
                ..Row::default()
            },
            Row {
                id: "two".into(),
                ..Row::default()
            },
        ];
        let incoming = vec![
            Row {
                id: "new".into(),
                ..Row::default()
            },
            Row {
                id: "one".into(),
                state: "Done".into(),
                ..Row::default()
            },
        ];
        let fixed = stable_rows(&old, &incoming);
        assert_eq!(
            fixed.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["one", "two"]
        );
        assert_eq!(fixed[0].state, "Done");
    }
    #[test]
    fn unknown_quota_is_not_zero() {
        assert!(
            quotas(&serde_json::json!({"rateLimits":{"primary":{"windowDurationMins":300}}}))
                .is_empty()
        );
        assert_eq!(quotas(&serde_json::json!({"rateLimits":{"primary":{"usedPercent":0,"windowDurationMins":300}}}))[0].remaining,100.0);
    }
    #[test]
    fn expired_rates_excluded() {
        let r = Row {
            tps: Some(20.0),
            rate_at: Some(now() - 31.0),
            ..Row::default()
        };
        assert!(r.rate().is_none());
    }
    #[test]
    fn token_format_and_duration() {
        assert_eq!(compact(Some(27190000)), "27.2M");
        assert_eq!(compact(None), "—");
        assert_eq!(duration(3661.0), "1:01:01");
    }
}
