use crate::{now, read_json, Paths};
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    cell::RefCell,
    collections::HashMap,
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    rc::Rc,
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
    #[serde(default)]
    pub execution: crate::telemetry::Execution,
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
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub execution: crate::telemetry::Execution,
    #[serde(default)]
    pub tags: Vec<crate::tags::Tag>,
    #[serde(skip)]
    pub fallback_title: bool,
    #[serde(default = "unknown")]
    pub state: String,
    // None means the bounded desktop list did not include this chat.
    #[serde(default)]
    pub is_unread: Option<bool>,
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
    pub fn newly_completed(&self) -> bool {
        self.state == "Done" && self.is_unread == Some(true)
    }
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
        match self.state.as_str() {
                "Busy" => "●",
                "Done" => "✓",
                "Waiting" => "◷",
                "Stopped" => "Ⅱ",
                "Failed" => "!",
                _ => "○",
            }.into()
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Recent {
    // Live projection only: cached errors must never start old conversations.
    #[serde(skip)]
    pub capacity_failures: Vec<crate::capacity_retry::Failure>,
    #[serde(skip)]
    pub capacity_available: bool,
    #[serde(skip)]
    pub capacity_unresolved: Vec<(String,String)>,
    #[serde(skip)]
    pub capacity_error: String,
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
    pub fn new_result_count(&self) -> usize {
        self.rows.iter().filter(|row| row.newly_completed()).count()
    }
    pub fn carry_read_state(&mut self, previous: &Self) {
        let known: HashMap<_, _> = previous
            .rows
            .iter()
            .map(|row| (row.id.as_str(), row.is_unread))
            .collect();
        for row in self.rows.iter_mut().chain(&mut self.all_rows).chain(&mut self.busy_metrics) {
            if row.is_unread.is_none() {
                row.is_unread = known.get(row.id.as_str()).copied().flatten();
            }
        }
    }
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
    // Earliest expiry among available earned-reset credits; not quota renewal.
    #[serde(default)]
    pub banked_reset: Option<f64>,
    #[serde(default)]
    pub banked_available: Option<u64>,
    #[serde(default)]
    pub banked_expiring: Option<u64>,
    #[serde(default)]
    pub banked_expiries: Vec<Option<f64>>,
}
impl Quota {
    pub fn banked_tooltip(&self, at: f64) -> String {
        let zh = crate::locale::chinese();
        let mut text = format!("{}\n{}: {}", if zh { "Banked Reset 到期时间" } else { "Banked reset expiry" },
            if zh { "可用张数" } else { "Available resets" }, self.banked_available.map(|n|n.to_string()).unwrap_or_else(||"—".into()));
        for (index, expiry) in self.banked_expiries.iter().enumerate() {
            let detail = expiry.map(|expiry| format!("{} · {} {}", crate::telemetry::local_reset(expiry),
                if zh { "剩余" } else { "remaining" }, crate::telemetry::countdown(expiry, at)))
                .unwrap_or_else(|| if zh { "未提供到期时间".into() } else { "Expiry not provided".into() });
            text.push_str(&format!("\n{}. {detail}", index+1));
        }
        if let Some(missing) = self.banked_available.and_then(|count| count.checked_sub(self.banked_expiries.len() as u64)).filter(|&n| n > 0) {
            text.push_str(&if zh { format!("\n另有 {missing} 张未提供到期明细") } else { format!("\nExpiry details unavailable for {missing} more resets") });
        }
        text
    }
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
                        banked_reset: None,
                        banked_available: None,
                        banked_expiring: None,
                        banked_expiries: Vec::new(),
                    });
                }
            }
        }
    }
    if let Some(quota) = result.first_mut() {
        let resets = &value["rateLimitResetCredits"];
        let mut expiries: Vec<Option<f64>> = resets["credits"].as_array().into_iter().flatten()
            .filter(|credit| credit["status"] == "available" && credit["resetType"] == "codexRateLimits")
            .map(|credit| credit["expiresAt"].as_f64().filter(|at| at.is_finite()))
            .filter(|expiry| expiry.is_none_or(|at| at > now())).collect();
        expiries.sort_by(|a,b| match (a,b) { (Some(a),Some(b))=>a.total_cmp(b), (None,Some(_))=>std::cmp::Ordering::Greater, (Some(_),None)=>std::cmp::Ordering::Less, _=>std::cmp::Ordering::Equal });
        quota.banked_reset = expiries.iter().flatten().copied().min_by(f64::total_cmp);
        quota.banked_available = resets["availableCount"].as_u64();
        quota.banked_expiring = quota.banked_reset.map(|at| expiries.iter().filter(|&&expiry| expiry == Some(at)).count() as u64);
        quota.banked_expiries = expiries;
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
    searchable: Vec<String>,
    default_indices: Vec<usize>,
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
        let searchable=entries.iter().map(|e|format!("{} {} {} {}",e.emoji,e.name,e.group,e.subgroup).to_lowercase()).collect();
        let default_indices=defaults.iter().filter_map(|icon|entries.iter().position(|e|&e.emoji==icon)).collect();
        Ok(Self {
            entries,
            defaults,
            prefixes,
            searchable,
            default_indices,
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
        if q.is_empty() && group == "默认集合" {
            return self.default_indices.clone();
        }
        self.entries
            .iter()
            .enumerate()
            .filter(|(i, e)| {
                (!q.is_empty() || group == "All emoji" || e.group == group)
                    && (q.is_empty() || self.searchable[*i].contains(&q))
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
    execution: crate::telemetry::Execution,
    context_at: Option<f64>,
}
struct Cache {
    size: u64,
    modified: std::time::SystemTime,
    offset: u64,
    data: TokenData,
}
pub struct Store {
    paths: Paths,
    databases: RefCell<HashMap<String, Rc<Connection>>>,
    cache: HashMap<String, Cache>,
    turn_contexts: HashMap<String, (String, u64, Option<crate::telemetry::Execution>)>,
    prompts: crate::prompts::Store,
    tag_sources: crate::tags::Sources,
}
impl Store {
    pub fn prompt_source(&self,id:&str)->Option<&str>{self.prompts.source(id)}
    pub fn new(paths: &Paths) -> Self {
        Self {
            paths: paths.clone(),
            databases: RefCell::new(HashMap::new()),
            cache: HashMap::new(),
            turn_contexts: HashMap::new(),
            prompts: crate::prompts::Store::default(),
            tag_sources: crate::tags::Sources::default(),
        }
    }
    fn db(&self, name: &str) -> Result<Rc<Connection>> {
        if let Some(db) = self.databases.borrow().get(name) { return Ok(Rc::clone(db)); }
        let db = Connection::open_with_flags(
            self.paths.home.join(name),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        db.busy_timeout(Duration::from_millis(500))?;
        db.execute_batch("PRAGMA query_only=ON; PRAGMA temp_store=MEMORY; PRAGMA cache_size=-512;")?;
        let db = Rc::new(db);
        self.databases.borrow_mut().insert(name.into(), Rc::clone(&db));
        Ok(db)
    }
    pub fn sessions(&self) -> Result<Vec<Row>> {
        let db = self.db("state_5.sqlite")?;
        let mut query=db.prepare("SELECT id,title,updated_at,tokens_used,name,model,reasoning_effort FROM threads WHERE archived=0 AND agent_path IS NULL ORDER BY updated_at DESC,id")
            .or_else(|_|db.prepare("SELECT id,title,updated_at,tokens_used,name FROM threads WHERE archived=0 AND agent_path IS NULL ORDER BY updated_at DESC,id"))?;
        let has_execution = query.column_count() >= 7;
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
                    execution: if has_execution {
                        crate::telemetry::Execution { model:r.get::<_,Option<String>>(5)?.unwrap_or_default(), effort:r.get::<_,Option<String>>(6)?.unwrap_or_default(), service_tier:None }
                    } else { Default::default() },
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
    pub fn directory(&self,id:&str)->Result<String> {
        Ok(self.db("state_5.sqlite")?.query_row("SELECT cwd FROM threads WHERE id=?",[id],|r|r.get::<_,Option<String>>(0))?.unwrap_or_default())
    }
    pub fn markdown(&self,id:&str,title:&str)->Result<String> {
        // Explicit copy only: bounded, read-only, and run outside the UI thread.
        // Skip tool/system records and hidden reasoning; preserve visible Markdown.
        let db=self.db("thread_history_1.sqlite").context("Local chat history unavailable")?;
        let mut statement=db.prepare("SELECT item_type,CASE WHEN length(CAST(item_json AS BLOB))<=4194304 THEN item_json ELSE NULL END FROM thread_items WHERE thread_id=? AND item_type IN ('userMessage','agentMessage') ORDER BY rollout_ordinal,item_id LIMIT 20001")?;
        let mut rows=statement.query([id])?;
        let mut output=format!("# {}\n\n",title.replace(['\r','\n']," "));
        let (mut count,mut visible)=(0,0);
        while let Some(row)=rows.next()? {
            count+=1;
            anyhow::ensure!(count<=20000,"Chat too large to copy safely");
            let json=row.get::<_,Option<String>>(1)?.context("Chat message too large to copy safely")?;
            let Some(text)=crate::body::history_text(&json) else{continue;};
            visible+=1;
            let role=if row.get::<_,String>(0)?=="userMessage"{"User"}else{"Assistant"};
            anyhow::ensure!(output.len()+text.len()+32<=16*1024*1024,"Chat too large to copy safely");
            output.push_str(&format!("## {role}\n\n{text}\n\n"));
        }
        anyhow::ensure!(visible>0,"No local chat messages available");
        Ok(output)
    }
    pub fn capacity_target_exists(&self,id:&str)->Result<bool> {
        let db=self.db("state_5.sqlite")?;
        Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM threads WHERE id=? AND archived=0 AND agent_path IS NULL)",[id],|row|row.get(0))?)
    }
    pub fn capacity_failure(&self,id:&str)->Result<Option<crate::capacity_retry::Failure>> {
        let state=self.db("state_5.sqlite")?;
        let row=state.query_row("SELECT COALESCE(NULLIF(name,''),title),rollout_path FROM threads WHERE id=? AND archived=0 AND agent_path IS NULL",[id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional()?;
        let Some((label,path))=row.filter(|(_,path)|std::path::Path::new(path).is_file()) else{return Ok(None);};
        let _=path;
        let history=self.db("thread_history_1.sqlite")?;
        let latest=history.query_row("SELECT turn_id,status,CASE WHEN length(CAST(error_json AS BLOB))<=8192 THEN error_json ELSE NULL END,completed_at FROM thread_turns WHERE thread_id=? ORDER BY COALESCE(started_at,completed_at,0) DESC,rollout_ordinal DESC LIMIT 1",[id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,Option<f64>>(3)?))).optional()?;
        Ok(latest.and_then(|(turn,status,error,at)|crate::capacity_retry::failure(id,&label,&turn,&status,error.as_deref(),at)))
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
        snap.execution.model = snap.model.clone();
        snap.execution.effort = db.query_row("SELECT reasoning_effort FROM threads WHERE id=?", [id], |r| r.get::<_,Option<String>>(0)).ok().flatten().unwrap_or_default();
        // The history projection points directly at the current turn. Read at
        // most 128 KiB once per turn instead of searching a large rollout.
        if let Ok((turn, offset)) = self.db("thread_history_1.sqlite")?.query_row(
            "SELECT turn_id,rollout_byte_offset FROM thread_turns WHERE thread_id=? ORDER BY rollout_ordinal DESC LIMIT 1",
            [id], |r| Ok((r.get::<_,String>(0)?, r.get::<_,Option<u64>>(1)?))) {
            if let Some(context) = offset.and_then(|offset| self.turn_context(&path, &turn, offset)) {
                if !context.model.is_empty() { snap.execution.model = context.model; }
                if !context.effort.is_empty() { snap.execution.effort = context.effort; }
                snap.execution.service_tier = context.service_tier;
            }
        }
        if let Ok(tokens) = self.tokens(&path) {
            if tokens.context_at.is_some_and(|at| snap.turn.started_at.is_some_and(|start| at >= start - 2.)) {
                if !tokens.execution.model.is_empty() { snap.execution.model = tokens.execution.model.clone(); }
                if !tokens.execution.effort.is_empty() { snap.execution.effort = tokens.execution.effort.clone(); }
                snap.execution.service_tier = tokens.execution.service_tier.clone();
            }
            snap.usage = tokens.usage;
            snap.token_at = tokens.at;
            snap.output_rate = tokens.rate;
        }
        Ok(snap)
    }
    fn turn_context(&mut self, path: &str, turn: &str, offset: u64) -> Option<crate::telemetry::Execution> {
        let size = fs::metadata(path).ok()?.len();
        if let Some((old_turn, old_size, context)) = self.turn_contexts.get(path) {
            if old_turn == turn && ((context.is_some() && size >= *old_size) || *old_size == size) { return context.clone(); }
        }
        let mut file = File::open(path).ok()?;
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut bytes = Vec::with_capacity(131072);
        file.take(131072).read_to_end(&mut bytes).ok()?;
        let mut context = None;
        for line in bytes.split(|b| *b == b'\n') {
            let Ok(event) = serde_json::from_slice::<Value>(line) else { continue; };
            if event["type"] == "turn_context" {
                if event["payload"]["turn_id"].as_str() == Some(turn) {
                    context = Some(crate::telemetry::Execution::from_context(&event["payload"]));
                }
                break;
            }
        }
        if self.turn_contexts.len() >= 128 { self.turn_contexts.clear(); }
        self.turn_contexts.insert(path.into(), (turn.into(), size, context.clone()));
        context
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
            if event["type"] == "turn_context" {
                data.execution = crate::telemetry::Execution::from_context(payload);
                data.context_at = event["timestamp"].as_str().and_then(timestamp);
                continue;
            }
            if event["type"] == "event_msg" && payload["type"] == "task_started" {
                data.execution = Default::default();
                data.context_at = None;
            }
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
        if let Ok(db) = self.db("state_5.sqlite") {
            self.tag_sources.enrich(&self.paths, &db, payload, &mut rows);
        }
        // Indexed, bounded latest-user-message lookups stay on the existing worker.
        // JSON is only parsed when the last user item changes, never per repaint.
        let state=self.db("state_5.sqlite")?;
        self.prompts.refresh(&state,&history,&mut rows)?;
        let local =
            crate::body::local_ids(&self.paths, rows.iter().map(|r| r.id.clone()).collect())?;
        rows.retain(|row| local.contains(&row.id));
        let mut turns = history.prepare("SELECT status FROM thread_turns WHERE thread_id=? ORDER BY rollout_ordinal DESC LIMIT 1")?;
        let mut capacity_turns=history.prepare("SELECT turn_id,status,CASE WHEN length(CAST(error_json AS BLOB))<=8192 THEN error_json ELSE NULL END,completed_at FROM thread_turns WHERE thread_id=? ORDER BY COALESCE(started_at,completed_at,0) DESC,rollout_ordinal DESC LIMIT 1").ok();
        let mut capacity_failures=Vec::new();
        let mut capacity_unresolved=Vec::new();
        let capacity_available=capacity_turns.is_some();
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
            if state=="Failed" || crate::capacity_retry::live_error(&entry["status"]) {
                let before=capacity_failures.len();
                if let Some(query)=capacity_turns.as_mut() {
                    if let Ok(Some((turn,status,error,at)))=query.query_row([&row.id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,Option<f64>>(3)?))).optional() {
                        if let Some(mut failure)=crate::capacity_retry::failure(&row.id,&row.label,&turn,&status,error.as_deref(),at) {
                            crate::capacity_retry::observe_live(&mut failure,&entry["status"]);
                            if failure.observed_at.is_some(){row.state="Failed".into();}
                            capacity_failures.push(failure);
                        }
                    }
                }
                if crate::capacity_retry::live_error(&entry["status"]) && capacity_failures.len()==before {
                    capacity_unresolved.push((row.id.clone(),row.label.clone()));
                }
            }
            row.is_unread = entry["isUnread"].as_bool();
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
                    row.execution = snapshot.execution;
                }
            }
        }
        let busy_metrics: Vec<Row> = rows.iter().filter(|r| r.state == "Busy").cloned().collect();
        rows.sort_by(|a, b| b.updated_at.total_cmp(&a.updated_at).then(a.id.cmp(&b.id)));
        let mut all_rows = rows.clone();
        all_rows.sort_by_key(|r| r.state != "Busy");
        Ok(Recent {
            capacity_failures,
            capacity_available,
            capacity_unresolved,
            capacity_error:String::new(),
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
    use serde_json::json;
    #[test]
    fn markdown_copy_uses_visible_messages_in_order_without_tools_or_reasoning() {
        let (paths,dir)=fixture("markdown-copy");
        let db=Connection::open(dir.join("thread_history_1.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE thread_items(thread_id TEXT,item_id TEXT,item_type TEXT,rollout_ordinal INTEGER,item_json TEXT);").unwrap();
        for (id,kind,ordinal,item) in [("u","userMessage",1,json!({"type":"userMessage","content":[{"type":"text","text":"请检查 **接口**"}]})),("hidden","agentMessage",2,json!({"type":"agentMessage","phase":"analysis","text":"hidden reasoning"})),("a","agentMessage",3,json!({"type":"agentMessage","phase":"final","text":"已检查。\n```rust\nlet ok=true;\n```"})),("tool","commandExecution",4,json!({"type":"commandExecution","text":"tool output"}))] {
            db.execute("INSERT INTO thread_items VALUES('chat',?,?,?,?)",rusqlite::params![id,kind,ordinal,item.to_string()]).unwrap();
        }
        let store=Store::new(&paths);let text=store.markdown("chat","接口 [联调]").unwrap();
        assert!(text.starts_with("# 接口 [联调]\n\n## User\n\n请检查 **接口**"));
        assert!(text.contains("## Assistant\n\n已检查。\n```rust"));
        assert!(!text.contains("hidden reasoning")&&!text.contains("tool output"));
        assert!(store.markdown("missing","Missing").is_err());
        drop(store);drop(db);std::fs::remove_dir_all(dir).unwrap();
    }
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
    fn recent_prompt_fallback_and_auto_tags_follow_source_metadata() {
        let (paths,dir)=fixture("prompts-tags");
        let db=Connection::open(dir.join("state_5.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE threads(id TEXT,title TEXT,name TEXT,updated_at REAL,tokens_used INTEGER,archived INTEGER,agent_path TEXT,rollout_path TEXT,project_id TEXT,thread_section_id TEXT);
            CREATE TABLE projects(id TEXT,name TEXT);
            INSERT INTO projects VALUES('project','工程甲');
            INSERT INTO threads VALUES('a','A',NULL,10,0,0,NULL,NULL,'project',NULL),('b','B',NULL,9,0,0,NULL,NULL,'project',NULL);").unwrap();
        let history=Connection::open(dir.join("thread_history_1.sqlite")).unwrap();
        history.execute_batch("CREATE TABLE thread_turns(thread_id TEXT,status TEXT,rollout_ordinal INTEGER,started_at REAL,completed_at REAL);
            CREATE TABLE thread_items(thread_id TEXT,item_id TEXT,item_type TEXT,rollout_ordinal INTEGER,updated_at_ordinal INTEGER,item_json TEXT);").unwrap();
        let msg=|text:&str|serde_json::json!({"type":"userMessage","content":[{"type":"text","text":text}]}).to_string();
        for(id,ordinal,text)in [("a",1,"请检查图像采集链路中的帧丢失问题。"),("a",2,"继续"),("a",3,"# AGENTS.md instructions\nignore context"),("b",1,"更早之前的有效提问内容超过十个字。"),("b",2,"一二三四五六七八九十")]{
            history.execute("INSERT INTO thread_items VALUES(?,?,'userMessage',?,?,?)",rusqlite::params![id,ordinal.to_string(),ordinal,ordinal,msg(text)]).unwrap();
        }
        let payload=serde_json::json!({"sections":[{"sectionId":"group","name":"IP","itemKeys":["codex:project:project"]}]});
        let mut store=Store::new(&paths);let recent=store.recent(&payload).unwrap();
        let a=recent.rows.iter().find(|r|r.id=="a").unwrap();
        assert_eq!(a.prompt,"请检查图像采集链路中的帧丢失问题。");
        assert_eq!(a.tags.iter().map(|t|t.label.as_str()).collect::<Vec<_>>(),["工程甲","IP"]);
        assert_eq!(recent.rows.iter().find(|r|r.id=="b").unwrap().prompt,"一二三四五六七八九十");
        // Warm refresh keeps prompts cached; a new user item invalidates only A.
        assert_eq!(store.recent(&payload).unwrap().rows[0].prompt,a.prompt);
        history.execute("INSERT INTO thread_items VALUES('a','4','userMessage',4,4,?)",[msg("现在检查图像采集链路中的背压处理。")]).unwrap();
        assert_eq!(store.recent(&payload).unwrap().rows[0].prompt,"现在检查图像采集链路中的背压处理。");
        let mut custom=crate::tags::Custom::default();custom.add("a","排查",2).unwrap();
        let isolated=Paths{app:dir.clone(),runtime:dir.clone(),..paths};
        custom.save(&isolated).unwrap();let restored=crate::tags::Custom::load(&isolated);
        assert_eq!(restored.for_row(a).len(),3);assert_eq!(restored.definitions[0].color,2);
    }

    #[test]
    fn completed_chats_keep_model_and_effort_without_reading_rollouts() {
        let (paths, dir) = fixture("completed-execution");
        let db = Connection::open(dir.join("state_5.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE threads(id TEXT,title TEXT,name TEXT,updated_at REAL,tokens_used INTEGER,archived INTEGER,agent_path TEXT,model TEXT,reasoning_effort TEXT);
            INSERT INTO threads VALUES('done','Completed chat',NULL,10,1200,0,NULL,'gpt-6.1-sol','xhigh');").unwrap();
        let rows = Store::new(&paths).sessions().unwrap();
        assert_eq!(rows[0].execution.short(), "6.1 Sol · XH");
        assert!(rows[0].execution.service_tier.is_none());
    }

    #[test]
    fn turn_metadata_is_incremental_and_new_turn_does_not_inherit_fast() {
        let (paths, dir) = fixture("execution");
        let file = dir.join("execution.jsonl");
        let context = |effort: &str, tier: Option<&str>| serde_json::json!({"type":"turn_context","timestamp":"2026-10-22T00:00:00Z","payload":{"model":"gpt-6.1-sol","effort":effort,"service_tier":tier}}).to_string();
        std::fs::write(&file, format!("{}\n", context("high", Some("priority")))).unwrap();
        let mut store = Store::new(&paths);
        let path = file.to_str().unwrap();
        assert!(store.tokens(path).unwrap().execution.fast());
        use std::io::Write;
        let mut stream = std::fs::OpenOptions::new().append(true).open(&file).unwrap();
        writeln!(stream, "{}", serde_json::json!({"type":"event_msg","payload":{"type":"task_started"}})).unwrap();
        assert!(store.tokens(path).unwrap().context_at.is_none());
        writeln!(stream, "{}", context("xhigh", None)).unwrap();
        let next = store.tokens(path).unwrap();
        assert_eq!(next.execution.short(), "6.1 Sol · XH");
        assert!(next.execution.service_tier.is_none());
    }

    #[test]
    fn banked_countdown_uses_nearest_available_credit_expiry() {
        let at = now() + 86400.;
        let mut v = serde_json::json!({"rateLimits":{"primary":{"usedPercent":30,"windowDurationMins":10080,"resetsAt":1800000000},"secondary":{"usedPercent":10,"windowDurationMins":300,"resetsAt":1800000010}},"rateLimitResetCredits":{"availableCount":5,"credits":[]}});
        assert!(quotas(&v)[0].banked_reset.is_none());
        v["rateLimitResetCredits"]["credits"] = serde_json::json!([
            {"expiresAt":at+1000.,"status":"available","resetType":"codexRateLimits"},
            {"expiresAt":at,"status":"available","resetType":"codexRateLimits"},
            {"expiresAt":at,"status":"available","resetType":"codexRateLimits"},
            {"expiresAt":at-100.,"status":"redeemed","resetType":"codexRateLimits"},
            {"expiresAt":now()-100.,"status":"available","resetType":"codexRateLimits"},
            {"expiresAt":null,"status":"available","resetType":"codexRateLimits"}]);
        let quotas = quotas(&v);
        let q = &quotas[0];
        assert_eq!(q.banked_reset, Some(at));
        assert_eq!(q.banked_available, Some(5));
        assert_eq!(q.banked_expiring, Some(2));
        assert_eq!(q.banked_expiries, vec![Some(at), Some(at), Some(at+1000.), None]);
        let tip = q.banked_tooltip(now());
        assert_eq!(tip.matches(&crate::telemetry::local_reset(at)).count(), 2);
        assert!(tip.contains(&crate::telemetry::local_reset(at+1000.)));
        assert!(tip.contains("4. "));
        assert_eq!(q.reset, Some(1800000000.));
        assert!(quotas[1].banked_reset.is_none());
    }

    #[test]
    fn indexed_turn_context_survives_large_output_and_never_crosses_turns() {
        let (paths, dir) = fixture("indexed-context");
        let file = dir.join("indexed.jsonl");
        let context = serde_json::json!({"type":"turn_context","payload":{"turn_id":"current","model":"gpt-6-astra","effort":"xhigh","service_tier":"fast"}});
        std::fs::write(&file, format!("{context}\n{}\n", " ".repeat(300000))).unwrap();
        let mut store = Store::new(&paths);
        let path = file.to_str().unwrap();
        assert!(store.tokens(path).unwrap().execution.model.is_empty());
        assert_eq!(store.turn_context(path, "current", 0).unwrap().short(), "6 Astra · XH ⚡");
        assert!(store.turn_context(path, "next", 0).is_none());
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
        let cached = store.db("state_5.sqlite").unwrap();
        assert!(Rc::ptr_eq(&cached, &store.db("state_5.sqlite").unwrap()));
        assert_eq!(cached.query_row("PRAGMA cache_size", [], |r| r.get::<_, i32>(0)).unwrap(), -512);
        assert!(cached.execute("DELETE FROM threads", []).is_err());
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
        let recent = store.recent(&serde_json::json!({"threads":[
            {"kind":"codex","hostId":"local","id":"chat-79","status":"active","isUnread":true},
            {"kind":"codex","hostId":"local","id":"chat-0","status":"idle","isUnread":true}
        ]})).unwrap();
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
        assert!(recent.rows[0].newly_completed());
        assert!(!recent.rows[79].newly_completed());
        assert_eq!(recent.new_result_count(), 1);
        let mut read = store.recent(&serde_json::json!({"threads":[
            {"kind":"codex","hostId":"local","id":"chat-0","status":"idle","isUnread":false}
        ]})).unwrap();
        read.carry_read_state(&recent);
        assert_eq!(read.rows[0].is_unread, Some(false));
        assert_eq!(read.new_result_count(), 0);
        let mut outside_window = store.recent(&serde_json::json!({})).unwrap();
        outside_window.carry_read_state(&recent);
        assert!(outside_window.rows[0].newly_completed());
        outside_window.carry_read_state(&read);
        // An explicit newer value always wins over an older cached value.
        assert!(outside_window.rows[0].newly_completed());
        let cached: Recent = serde_json::from_value(serde_json::to_value(&read).unwrap()).unwrap();
        let mut restored = store.recent(&serde_json::json!({})).unwrap();
        restored.carry_read_state(&cached);
        assert_eq!(restored.rows[0].is_unread, Some(false));
        assert_eq!(restored.new_result_count(), 0);
        history.execute_batch("ALTER TABLE thread_turns ADD COLUMN turn_id TEXT; ALTER TABLE thread_turns ADD COLUMN error_json TEXT;").unwrap();
        db.execute("UPDATE threads SET rollout_path=? WHERE id='chat-0'",[rollout.to_str().unwrap()]).unwrap();
        history.execute("INSERT INTO thread_turns(thread_id,status,rollout_ordinal,started_at,completed_at,turn_id,error_json) VALUES('chat-0','failed',0,?,?,?,?)",rusqlite::params![now()-7200.,now()-3600.,"live-capacity",r#"{"codexErrorInfo":"serverOverloaded"}"#]).unwrap();
        let live=store.recent(&serde_json::json!({"threads":[{"kind":"codex","hostId":"local","id":"chat-0","status":"systemError"}]})).unwrap();
        let failure=live.capacity_failures.iter().find(|failure|failure.thread_id=="chat-0").expect("Live error must survive history ordinal resets and an old failure timestamp");
        assert_eq!(failure.turn_id,"live-capacity");assert!(failure.observed_at.is_some());assert!(failure.fresh(now()));
        assert_eq!(live.rows.iter().find(|row|row.id=="chat-0").unwrap().state,"Failed");
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
                prompt:"刷新后的最近用户提问。".into(),
                ..Row::default()
            },
        ];
        let fixed = stable_rows(&old, &incoming);
        assert_eq!(fixed.iter().find(|row|row.id=="one").unwrap().prompt,"刷新后的最近用户提问。");
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
