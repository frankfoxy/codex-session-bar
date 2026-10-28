//! Public announcement tracking. Aggregator deadlines are not account reset timers.
mod reminder;
use crate::{now, read_json, Paths, State, Stop};
use anyhow::{ensure, Context, Result};
pub use reminder::Reminder;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

const CACHE: &str = "reset-notice-cache.json";
const LOCALE_API: &str = "https://codex-resets.com/api/resets?locale=zh-CN";
const MAX_RESPONSE: u64 = 1024 * 1024;
const STALE_AFTER: f64 = 6. * 3600.;
const PENDING_AFTER: f64 = 3600.;
const BELL_WINDOW: f64 = 3. * 3600.;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bell {
    Neutral,
    Upcoming,
    Reset,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Divin,
    #[default]
    #[serde(other)]
    CodexResets,
}
impl Source {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Divin => "AI 哨兵 (divin.cc)",
            Self::CodexResets => "Codex Resets (codex-resets.com)",
        }
    }
    pub fn url(&self) -> &'static str {
        match self {
            Self::Divin => "https://divin.cc/",
            Self::CodexResets => "https://codex-resets.com/",
        }
    }
    pub fn api(&self) -> &'static str {
        match self {
            Self::Divin => "https://divin.cc/api/events",
            Self::CodexResets => "https://codex-resets.com/api/v1/status",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub enabled: bool,
    pub source: Source,
    pub check_minutes: u64,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            source: Source::CodexResets,
            check_minutes: 5,
        }
    }
}
impl Settings {
    pub fn clamp(&mut self) {
        self.check_minutes = self.check_minutes.clamp(1, 60);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Bound {
    #[default]
    At,
    Before,
    Around,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Post {
    pub author: String,
    pub original: String,
    pub translation: String,
    pub url: String,
    pub observed: bool,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Notice {
    pub id: String,
    pub at: f64,
    pub announced_at: f64,
    pub bound: Bound,
    pub summary: String,
    pub expression: String,
    pub posts: Vec<Post>,
    pub completed: bool,
}
fn date(at: f64, format: &str) -> String {
    if !at.is_finite() || at <= 0. {
        return "—".into();
    }
    chrono::DateTime::from_timestamp(at as i64, 0)
        .map(|at| at.with_timezone(&chrono::Local).format(format).to_string())
        .unwrap_or_else(|| "—".into())
}
fn clip(text: &str, length: usize) -> String {
    let cleaned = text.replace('\r', "").replace('\0', "");
    let mut chars = cleaned.chars();
    let mut prefix: String = chars.by_ref().take(length).collect();
    if chars.next().is_some() {
        if length > 0 {
            prefix.pop();
            prefix.push('…');
        }
        prefix
    } else {
        prefix
    }
}
impl Notice {
    pub fn short(&self, at: f64, zh: bool) -> String {
        if self.at <= at {
            return if zh {
                "待确认"
            } else {
                "Pending confirmation"
            }
            .into();
        }
        let marker = match self.bound {
            Bound::Before => "≤",
            Bound::Around => "≈",
            Bound::At => "",
        };
        let format = if date(self.at, "%Y-%m-%d") == date(at, "%Y-%m-%d") {
            "%H:%M"
        } else {
            "%m/%d %H:%M"
        };
        format!(
            "{marker}{} · {}",
            date(self.at, format),
            crate::telemetry::countdown(self.at, at)
        )
    }
    fn valid_at(&self, at: f64) -> bool {
        !self.completed && self.at.is_finite() && self.at > 0. && self.at >= at - PENDING_AFTER
    }
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct View {
    pub source: Source,
    pub checked_at: f64,
    pub attempted_at: f64,
    pub next_check_at: f64,
    pub error: String,
    pub current: Option<Notice>,
    pub history: Vec<Notice>,
}
impl View {
    pub fn badge(&self, at: f64, zh: bool) -> (Bell, String) {
        if let Some(notice) = self.active(at).filter(|notice| notice.at > at) {
            return (
                if notice.at - at <= BELL_WINDOW {
                    Bell::Upcoming
                } else {
                    Bell::Neutral
                },
                notice.short(at, zh),
            );
        }
        if self.checked_at > 0. && (0.0..=STALE_AFTER).contains(&(at - self.checked_at)) {
            if let Some(notice) = self
                .history
                .iter()
                .filter(|notice| {
                    notice.completed
                        && notice.at > 0.
                        && (0.0..=BELL_WINDOW).contains(&(at - notice.at))
                        && date(notice.at, "%Y-%m-%d") == date(at, "%Y-%m-%d")
                })
                .max_by(|a, b| a.at.total_cmp(&b.at))
            {
                let observed = notice.posts.first().is_some_and(|post| post.observed);
                let label = match (observed, zh) {
                    (true, true) => "已观测",
                    (false, true) => "已复位",
                    (true, false) => "Observed",
                    (false, false) => "Reset",
                };
                return (Bell::Reset, format!("{label} {}", date(notice.at, "%H:%M")));
            }
        }
        (Bell::Neutral, String::new())
    }
    pub fn check_status(&self, zh: bool) -> String {
        let checked = date(self.checked_at, "%m-%d %H:%M");
        let next = date(self.next_check_at, "%H:%M");
        if self.checked_at <= 0. {
            if !self.error.is_empty() {
                return if zh {
                    "首次检查失败"
                } else {
                    "First check failed"
                }
                .into();
            }
            return if zh {
                "等待首次检查"
            } else {
                "Waiting for first check"
            }
            .into();
        }
        if self.error.is_empty() {
            if zh {
                format!("上次成功：{checked} · 下次：{next}")
            } else {
                format!("Last success: {checked} · Next: {next}")
            }
        } else if zh {
            format!("检查异常 · 上次成功：{checked}")
        } else {
            format!("Check failed · Last success: {checked}")
        }
    }
    pub fn active(&self, at: f64) -> Option<&Notice> {
        if !at.is_finite()
            || !self.checked_at.is_finite()
            || self.checked_at > at + 300.
            || !(0.0..=STALE_AFTER).contains(&(at - self.checked_at).max(0.))
            || self.checked_at <= 0.
        {
            return None;
        }
        self.current.as_ref().filter(|notice| notice.valid_at(at))
    }
    pub fn tooltip(&self, at: f64, zh: bool) -> String {
        let mut lines = vec![if zh {
            "Codex 重置预告"
        } else {
            "Codex reset announcement"
        }
        .to_string()];
        let current = self.current.as_ref();
        let stale = self.checked_at > 0.
            && self.active(at).is_none()
            && (at - self.checked_at > STALE_AFTER || self.checked_at > at + 300.);
        if let Some(notice) = current {
            let qualifier = match (notice.bound, zh) {
                (Bound::Before, true) => "之前",
                (Bound::Before, false) => "by",
                (Bound::Around, true) => "左右",
                (Bound::Around, false) => "around",
                _ => "",
            };
            if stale {
                lines.push(
                    if zh {
                        "缓存已过期，预告仅供参考"
                    } else {
                        "Outdated cache; forecast for reference only"
                    }
                    .into(),
                );
            } else if notice.at <= at {
                lines.push(
                    if zh {
                        "预告时间已过，尚未确认重置"
                    } else {
                        "Forecast time passed; reset not confirmed"
                    }
                    .into(),
                );
            }
            let time = date(notice.at, "%m-%d %H:%M %:z");
            lines.push(if zh {
                format!("预计重置: {time}{qualifier}")
            } else {
                format!("Expected reset: {qualifier} {time}").replace(":  ", ": ")
            });
            if !stale && self.active(at).is_some() && notice.at > at {
                lines.push(format!(
                    "{}: {}",
                    if zh { "剩余" } else { "In" },
                    crate::telemetry::countdown(notice.at, at)
                ));
            }
        } else {
            lines.push(
                if zh {
                    "暂无明确的下一次重置时间"
                } else {
                    "No explicit next reset time"
                }
                .into(),
            );
        }
        let detail = current.or_else(|| self.history.first());
        if let Some(notice) = detail {
            lines.push(String::new());
            lines.push(format!(
                "{} · {}{}",
                if current.is_some() {
                    if zh {
                        "本次预告"
                    } else {
                        "Announcement"
                    }
                } else if zh {
                    "最近消息"
                } else {
                    "Latest notice"
                },
                date(notice.announced_at, "%m-%d %H:%M"),
                notice_marker(notice, zh)
            ));
            if current.is_none() && notice.at > 0. {
                lines.push(format!(
                    "{}: {}",
                    if notice.completed {
                        if zh {
                            "来源报告重置"
                        } else {
                            "Source-reported reset"
                        }
                    } else if zh {
                        "此前预计（未确认）"
                    } else {
                        "Earlier forecast (unconfirmed)"
                    },
                    date(notice.at, "%m-%d %H:%M %:z")
                ));
            }
            if notice.posts.is_empty() {
                if !notice.summary.is_empty() {
                    lines.push(notice_excerpt(&notice.summary, 180));
                }
            } else {
                for post in notice.posts.iter().take(2) {
                    if !post.author.is_empty() && !post.observed {
                        lines.push(clip(&post.author, 60));
                    }
                    if !post.original.is_empty() {
                        lines.push(format!(
                            "{}: {}",
                            if zh { "原文" } else { "Original" },
                            notice_excerpt(&post.original, 180)
                        ));
                    }
                    if !post.translation.is_empty() {
                        lines.push(format!(
                            "{}: {}",
                            if zh { "译文" } else { "Chinese translation" },
                            notice_excerpt(&post.translation, 180)
                        ));
                    }
                }
            }
        }
        let others: Vec<_> = self
            .history
            .iter()
            .filter(|notice| detail.is_none_or(|selected| selected.id != notice.id))
            .take(2)
            .collect();
        if !others.is_empty() {
            lines.push(String::new());
            lines.push(
                if zh {
                    "更早消息"
                } else {
                    "Earlier notices"
                }
                .into(),
            );
            for notice in others {
                let summary = notice
                    .posts
                    .first()
                    .filter(|post| zh && !post.translation.is_empty())
                    .map(|post| post.translation.as_str())
                    .unwrap_or(&notice.summary);
                lines.push(format!(
                    "{}{} · {}",
                    date(notice.announced_at, "%m-%d %H:%M"),
                    notice_marker(notice, zh),
                    notice_excerpt(summary, 72)
                ));
            }
        }
        lines.push(String::new());
        lines.push(format!(
            "{}: {}",
            if zh { "来源" } else { "Source" },
            self.source.label()
        ));
        lines.push(self.check_status(zh));
        if !self.error.is_empty() {
            lines.push(format!(
                "{}: {}",
                if zh { "检查失败" } else { "Check error" },
                notice_excerpt(&self.error, 100)
            ));
        }
        if stale && current.is_none() {
            lines.push(
                if zh {
                    "缓存已过期，仅供参考"
                } else {
                    "Outdated cache; for reference only"
                }
                .into(),
            );
        }
        lines.push(
            if zh {
                "第三方整理，非账户额度倒计时 · 点击查看来源"
            } else {
                "Third-party tracker, separate from account limits · Click to open source"
            }
            .into(),
        );
        lines.join("\n")
    }
}
fn notice_marker(notice: &Notice, zh: bool) -> &'static str {
    if notice.posts.first().is_some_and(|post| post.observed) {
        if zh {
            " [观测]"
        } else {
            " [observed]"
        }
    } else if !notice.completed {
        if zh {
            " [预告]"
        } else {
            " [forecast]"
        }
    } else {
        ""
    }
}
fn notice_excerpt(text: &str, limit: usize) -> String {
    let compact = text
        .split_whitespace()
        .filter(|word| !word.starts_with("https://") && !word.starts_with("http://"))
        .collect::<Vec<_>>()
        .join(" ");
    clip(&compact, limit)
}

fn timestamp(value: &Value) -> Option<f64> {
    let time = chrono::DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()?
        .timestamp_millis() as f64
        / 1000.;
    (time.is_finite() && time > 0.).then_some(time)
}
fn string(value: &Value, key: &str) -> String {
    clip(value[key].as_str().unwrap_or_default(), 600)
}
fn trusted(url: &str) -> bool {
    url.strip_prefix("https://x.com/thsottiaux/status/")
        .is_some_and(|id| {
            !id.is_empty() && id.len() <= 32 && id.bytes().all(|byte| byte.is_ascii_digit())
        })
}
fn post(value: &Value) -> Option<Post> {
    let url = string(value, "url");
    if value["handle"].as_str() != Some("thsottiaux") || !trusted(&url) {
        return None;
    }
    Some(Post {
        author: "Tibo (@thsottiaux)".into(),
        original: string(value, "text"),
        translation: string(value, "translation"),
        url,
        observed: false,
    })
}
fn divin_posts(event: &Value) -> Vec<Post> {
    let entries: Vec<_> = event["posts"]
        .as_array()
        .into_iter()
        .flatten()
        .take(64)
        .filter_map(|value| {
            post(value).map(|post| (post, value["role"].as_str().unwrap_or_default()))
        })
        .collect();
    let mut selected = Vec::<Post>::new();
    let mut add = |entry: Option<&(Post, &str)>| {
        if let Some((post, _)) = entry {
            if selected.len() < 3 && !selected.iter().any(|saved| saved.url == post.url) {
                selected.push(post.clone());
            }
        }
    };
    add(entries
        .iter()
        .find(|(_, role)| *role == "announce")
        .or_else(|| entries.first()));
    let expression = event["forecast"]["expression"]
        .as_str()
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    if !expression.is_empty() {
        add(entries.iter().find(|(post, _)| {
            post.original
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_ascii_lowercase()
                .contains(&expression)
        }));
    }
    add(entries
        .iter()
        .rev()
        .find(|(_, role)| *role == "update")
        .or_else(|| entries.last()));
    for entry in &entries {
        add(Some(entry));
    }
    selected
}
fn keep_recent(mut notices: Vec<Notice>, current: Option<&Notice>) -> Vec<Notice> {
    notices.retain(|notice| {
        !notice.id.is_empty()
            && current.is_none_or(|selected| notice.id != selected.id || notice.at != selected.at)
    });
    notices.sort_by(|a, b| b.announced_at.total_cmp(&a.announced_at));
    let mut seen = HashSet::new();
    notices.retain(|notice| seen.insert((notice.id.clone(), notice.at.to_bits())));
    notices.truncate(3);
    notices
}
pub(crate) fn parse_divin(bytes: &[u8], at: f64) -> Result<View> {
    let value: Value = serde_json::from_slice(bytes)?;
    let events = value["events"]
        .as_array()
        .context("AI Sentinel returned no events array")?;
    let mut candidates = Vec::new();
    let mut history = Vec::new();
    for event in events.iter().take(512) {
        if event["tool"] != "codex" || event["type"] != "usage_reset" {
            continue;
        }
        let posts = divin_posts(event);
        if posts.is_empty() {
            continue;
        }
        let forecast = &event["forecast"];
        let bound = match forecast["bound"].as_str() {
            Some("before") => Bound::Before,
            Some("around") => Bound::Around,
            _ => Bound::At,
        };
        let notice = Notice {
            id: string(event, "id"),
            at: if event["stage"] == "completed" || !event["completedAt"].is_null() {
                timestamp(&event["completedAt"]).unwrap_or(0.)
            } else {
                timestamp(&forecast["at"]).unwrap_or(0.)
            },
            announced_at: timestamp(&event["announcedAt"])
                .or_else(|| timestamp(&event["updatedAt"]))
                .unwrap_or(0.),
            bound,
            summary: string(event, "summary"),
            expression: string(forecast, "expression"),
            posts,
            completed: event["stage"] == "completed" || !event["completedAt"].is_null(),
        };
        let explicit_bound = matches!(forecast["bound"].as_str(), Some("before" | "at" | "around"));
        if matches!(event["stage"].as_str(), Some("scheduled" | "rolling_out"))
            && event["history"] != true
            && forecast["assumedZone"] == false
            && explicit_bound
            && notice.valid_at(at)
        {
            candidates.push(notice.clone());
        }
        history.push(notice);
    }
    candidates.sort_by(|a, b| b.announced_at.total_cmp(&a.announced_at));
    let current = candidates.into_iter().next();
    Ok(View {
        source: Source::Divin,
        checked_at: at,
        error: String::new(),
        history: keep_recent(history, current.as_ref()),
        current,
        ..Default::default()
    })
}
fn codex_bound(text: &str) -> Bound {
    let text = text.trim().to_ascii_lowercase();
    if text.starts_with("by ")
        || text.contains(" before ")
        || text.starts_with("before ")
        || text.contains("within ")
    {
        Bound::Before
    } else if text.contains("around ") || text.contains("about ") || text.contains('~') {
        Bound::Around
    } else {
        Bound::At
    }
}
fn enrich_codex(view: &mut View, bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    let value: Value = serde_json::from_slice(bytes)?;
    for current in view.current.iter_mut().chain(view.history.iter_mut()) {
        let scheduled = &value["scheduled"];
        if scheduled["tweet_id"].as_str() == Some(current.id.as_str())
            && scheduled["tweet_url"].as_str()
                == current.posts.first().map(|post| post.url.as_str())
            && timestamp(&scheduled["scheduled_for"]) == Some(current.at)
        {
            if let Some(post) = current.posts.first_mut() {
                post.translation = string(scheduled, "display_text");
            }
        }
    }
    if let Some(events) = value["events"].as_array() {
        let mut history = view.history.clone();
        for event in events.iter().take(100) {
            if event["reset_type"] != "regular" {
                continue;
            }
            let observed = event["source"] == "observed";
            let url = string(event, "tweet_url");
            if !observed && !trusted(&url) {
                continue;
            }
            let text = string(event, "text");
            let announced_at = timestamp(&event["announced_at"]).unwrap_or(0.);
            // Prefer the localized copy of an item already supplied by v1/status.
            let id = string(event, "tweet_id");
            history.retain(|notice| notice.id != id);
            history.push(Notice {
                id,
                at: announced_at,
                announced_at,
                bound: Bound::At,
                summary: text.clone(),
                expression: String::new(),
                completed: true,
                posts: vec![Post {
                    author: if observed {
                        "Codex Resets"
                    } else {
                        "Tibo (@thsottiaux)"
                    }
                    .into(),
                    original: text,
                    translation: string(event, "display_text"),
                    url: if trusted(&url) { url } else { String::new() },
                    observed,
                }],
            });
        }
        view.history = keep_recent(history, view.current.as_ref());
    }
    Ok(())
}
pub(crate) fn parse_codex(status_bytes: &[u8], localized_bytes: &[u8], at: f64) -> Result<View> {
    let value: Value = serde_json::from_slice(status_bytes)?;
    ensure!(
        value["data"].is_object(),
        "Codex Resets returned no status data"
    );
    let scheduled = value["data"]
        .get("scheduled_reset")
        .context("Codex Resets status is missing scheduled_reset")?;
    ensure!(
        scheduled.is_null() || scheduled.is_object(),
        "Codex Resets scheduled_reset must be an object or null"
    );
    let url = string(&scheduled["source"], "url");
    let mut view = View {
        source: Source::CodexResets,
        checked_at: at,
        ..Default::default()
    };
    // Completion is available on the primary API even if translations/history
    // are temporarily unavailable. It is distinct from a future forecast.
    let latest = &value["data"]["latest_reset"];
    let observed = latest["source"]["type"] == "observed";
    let latest_url = string(&latest["source"], "url");
    let official = latest["source"]["type"] == "x_post"
        && latest["source"]["author"] == "thsottiaux"
        && trusted(&latest_url);
    if latest["reset_type"] == "regular" && (observed || official) {
        if let Some(time) = timestamp(&latest["announced_at"]) {
            let id = string(latest, "id");
            let original = string(latest, "text");
            if !id.is_empty() && !original.is_empty() {
                view.history.push(Notice {
                    id,
                    at: time,
                    announced_at: time,
                    completed: true,
                    summary: original.clone(),
                    posts: vec![Post {
                        author: if observed {
                            "Codex Resets"
                        } else {
                            "Tibo (@thsottiaux)"
                        }
                        .into(),
                        original,
                        url: if trusted(&latest_url) {
                            latest_url
                        } else {
                            String::new()
                        },
                        observed,
                        ..Default::default()
                    }],
                    ..Default::default()
                });
            }
        }
    }
    if scheduled["status"] == "scheduled"
        && scheduled["reset_type"] == "regular"
        && scheduled["source"]["type"] == "x_post"
        && scheduled["source"]["author"] == "thsottiaux"
        && trusted(&url)
    {
        if let Some(time) = timestamp(&scheduled["scheduled_for"]) {
            let original = string(scheduled, "text");
            let notice = Notice {
                id: string(scheduled, "id"),
                at: time,
                announced_at: timestamp(&scheduled["announced_at"]).unwrap_or(0.),
                bound: codex_bound(&original),
                summary: original.clone(),
                expression: original.clone(),
                posts: vec![Post {
                    author: "Tibo (@thsottiaux)".into(),
                    original,
                    translation: String::new(),
                    url,
                    observed: false,
                }],
                completed: false,
            };
            if notice.valid_at(at) {
                view.current = Some(notice);
            } else {
                view.history.push(notice);
            }
        }
    }
    enrich_codex(&mut view, localized_bytes)?;
    Ok(view)
}

fn copy_enrichment(next: &mut View, previous: &View) {
    if next.source != previous.source {
        return;
    }
    let mut history = next.history.clone();
    history.extend(previous.history.clone());
    if let Some(old) = previous.current.as_ref().filter(|old| {
        next.current
            .as_ref()
            .is_none_or(|current| current.id != old.id || current.at != old.at)
    }) {
        history.push(old.clone());
    }
    next.history = keep_recent(history, next.current.as_ref());
    if let (Some(next), Some(previous)) = (&mut next.current, &previous.current) {
        if next.id == previous.id && next.at == previous.at {
            for post in &mut next.posts {
                if post.translation.is_empty() {
                    if let Some(old) = previous.posts.iter().find(|old| old.url == post.url) {
                        post.translation = old.translation.clone();
                    }
                }
            }
        }
    }
}
fn response(
    url: &str,
    etags: &mut HashMap<String, String>,
    stop: &AtomicBool,
) -> Result<Option<Vec<u8>>> {
    let mut body = Vec::new();
    let result = crate::update::http::get_notice(
        url,
        etags.get(url).map(String::as_str).unwrap_or(""),
        &mut body,
        MAX_RESPONSE,
        stop,
    )?;
    if result.status == 304 {
        return Ok(None);
    }
    ensure!(
        result.status == 200,
        "Announcement source returned HTTP {}",
        result.status
    );
    etags.insert(url.into(), result.etag);
    Ok(Some(body))
}
fn fetch(
    source: Source,
    previous: &View,
    etags: &mut HashMap<String, String>,
    stop: &AtomicBool,
    requested: Option<&Mutex<Settings>>,
    at: f64,
) -> Result<View> {
    allow_request(source, stop, requested)?;
    let primary = response(source.api(), etags, stop)?;
    let mut next = match (source, primary) {
        (Source::Divin, Some(bytes)) => parse_divin(&bytes, at)?,
        (Source::CodexResets, Some(bytes)) => parse_codex(&bytes, &[], at)?,
        (_, None) => {
            let mut view = previous.clone();
            view.checked_at = at;
            view.error.clear();
            view
        }
    };
    copy_enrichment(&mut next, previous);
    if source == Source::CodexResets {
        // A source can be disabled/switched while the primary request finishes.
        // Do not start the optional locale/history request after that change.
        allow_request(source, stop, requested)?;
        match response(LOCALE_API, etags, stop) {
            Ok(Some(bytes)) => {
                if let Err(error) = enrich_codex(&mut next, &bytes) {
                    etags.remove(LOCALE_API);
                    next.error = format!("Translation/history response: {error:#}");
                }
            }
            Ok(None) => {}
            Err(error) => {
                next.error = format!("Translation/history unavailable: {error:#}");
            }
        }
    }
    Ok(next)
}
fn allow_request(
    source: Source,
    stop: &AtomicBool,
    requested: Option<&Mutex<Settings>>,
) -> Result<()> {
    ensure!(
        !stop.load(Ordering::Relaxed),
        "Announcement request cancelled"
    );
    if let Some(requested) = requested {
        let settings = requested.lock().unwrap();
        ensure!(
            settings.enabled && settings.source == source,
            "Announcement source was disabled or changed"
        );
    }
    Ok(())
}
fn publish(paths: &Paths, state: &State, view: &View) {
    state.lock().unwrap().reset_notice = Arc::new(view.clone());
    paths.write(CACHE, view);
}
enum Command {
    Configure(Settings),
    Acknowledged(String, bool),
    Stop,
}
#[derive(Clone)]
pub struct Control {
    tx: mpsc::Sender<Command>,
    settings: Arc<Mutex<Settings>>,
    state: State,
    alerts: Arc<Mutex<mpsc::Receiver<Reminder>>>,
}
impl Control {
    pub fn configure(&self, mut settings: Settings) -> bool {
        settings.clamp();
        let mut current = self.settings.lock().unwrap();
        let clear = current.source != settings.source || !settings.enabled;
        *current = settings.clone();
        if clear {
            while self.alerts.lock().unwrap().try_recv().is_ok() {}
            self.state.lock().unwrap().reset_notice = Arc::new(View {
                source: settings.source,
                ..Default::default()
            });
        }
        self.tx.send(Command::Configure(settings)).is_ok()
    }
    pub fn next_reminder(&self) -> Option<Reminder> {
        self.alerts.lock().unwrap().try_recv().ok()
    }
    pub fn acknowledge(&self, key: String, delivered: bool) {
        let _ = self.tx.send(Command::Acknowledged(key, delivered));
    }
}
fn acknowledge(paths: &Paths, tracker: &mut reminder::Tracker, key: &str, delivered: bool) {
    tracker.acknowledge(key, delivered);
    if delivered {
        tracker.save(paths);
    }
    reminder::log(
        paths,
        json!({"event":"notification","at":now(),"key":key,"submitted_to_windows":delivered}),
    );
}
pub struct Service {
    control: Control,
    handle: Option<thread::JoinHandle<()>>,
}
impl Service {
    pub fn control(&self) -> Control {
        self.control.clone()
    }
    pub fn start(paths: &Paths, state: &State, stop: &Stop, mut settings: Settings) -> Self {
        settings.clamp();
        let mut initial: View =
            serde_json::from_value(read_json(&paths.runtime.join(CACHE), 65536))
                .unwrap_or_default();
        if initial.source != settings.source || !settings.enabled {
            initial = View {
                source: settings.source,
                ..Default::default()
            };
        }
        initial.next_check_at = if settings.enabled { now() + 5. } else { 0. };
        state.lock().unwrap().reset_notice = Arc::new(initial.clone());
        let (tx, rx) = mpsc::channel();
        let (alert_tx, alert_rx) = mpsc::channel();
        let requested = Arc::new(Mutex::new(settings.clone()));
        let control = Control {
            tx,
            settings: requested.clone(),
            state: state.clone(),
            alerts: Arc::new(Mutex::new(alert_rx)),
        };
        let (paths, state, stop) = (paths.clone(), state.clone(), stop.clone());
        let handle = thread::spawn(move || {
            let mut view = initial;
            let mut etags = HashMap::new();
            let mut reminders = reminder::Tracker::load(&paths);
            let mut next = Instant::now() + Duration::from_secs(5);
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let command = if settings.enabled {
                    rx.recv_timeout(next.saturating_duration_since(Instant::now()))
                } else {
                    rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
                };
                match command {
                    Ok(Command::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Ok(Command::Acknowledged(key, delivered)) => {
                        acknowledge(&paths, &mut reminders, &key, delivered)
                    }
                    Ok(Command::Configure(mut updated)) => {
                        while let Ok(command) = rx.try_recv() {
                            match command {
                                Command::Stop => return,
                                Command::Configure(value) => updated = value,
                                Command::Acknowledged(key, delivered) => {
                                    acknowledge(&paths, &mut reminders, &key, delivered)
                                }
                            }
                        }
                        updated.clamp();
                        let restart = updated.source != settings.source
                            || updated.enabled != settings.enabled;
                        settings = updated;
                        if restart {
                            reminders.reset_pending();
                            view = View {
                                source: settings.source,
                                ..Default::default()
                            };
                            etags.clear();
                            publish(&paths, &state, &view);
                        }
                        next = Instant::now()
                            + if restart && settings.enabled {
                                Duration::ZERO
                            } else {
                                Duration::from_secs(settings.check_minutes * 60)
                            };
                        view.next_check_at = if settings.enabled {
                            now() + next.saturating_duration_since(Instant::now()).as_secs_f64()
                        } else {
                            0.
                        };
                        publish(&paths, &state, &view);
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        let attempted = now();
                        reminder::log(
                            &paths,
                            json!({"event":"check_started","at":attempted,"pid":std::process::id(),"source":settings.source,"api":settings.source.api(),"interval_minutes":settings.check_minutes}),
                        );
                        let result = fetch(
                            settings.source,
                            &view,
                            &mut etags,
                            &stop,
                            Some(requested.as_ref()),
                            now(),
                        );
                        if stop.load(Ordering::Relaxed) {
                            reminder::log(
                                &paths,
                                json!({"event":"check_cancelled","at":now(),"started_at":attempted,"source":settings.source,"reason":"shutdown"}),
                            );
                            break;
                        }
                        let wanted = requested.lock().unwrap();
                        if wanted.enabled && wanted.source == settings.source {
                            let success = result.is_ok();
                            match result {
                                Ok(fetched) => view = fetched,
                                Err(error) => {
                                    etags.clear();
                                    view.error = format!("{error:#}");
                                }
                            }
                            view.attempted_at = attempted;
                            view.next_check_at = now() + (settings.check_minutes * 60) as f64;
                            if success {
                                for alert in reminders.collect(&view, now()) {
                                    let key = alert.key.clone();
                                    reminder::log(
                                        &paths,
                                        json!({"event":"notification_queued","at":now(),"source":alert.source,"key":key,"notice_at":alert.notice.at,"completed":alert.notice.completed}),
                                    );
                                    if alert_tx.send(alert).is_err() {
                                        reminders.acknowledge(&key, false);
                                    }
                                }
                            }
                            publish(&paths, &state, &view);
                            drop(wanted);
                            reminder::check_log(&paths, &view, settings.source, success);
                        } else {
                            drop(wanted);
                            reminder::log(
                                &paths,
                                json!({"event":"check_cancelled","at":now(),"started_at":attempted,"source":settings.source,"reason":"source changed or disabled"}),
                            );
                        }
                        next = Instant::now() + Duration::from_secs(settings.check_minutes * 60);
                    }
                }
            }
        });
        Self {
            control,
            handle: Some(handle),
        }
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.control.tx.send(Command::Stop);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
pub fn inspect(_paths: &Paths, source: Source) -> Result<()> {
    let view = fetch(
        source,
        &View {
            source,
            ..Default::default()
        },
        &mut HashMap::new(),
        &AtomicBool::new(false),
        None,
        now(),
    )?;
    println!(
        "{}",
        json!({"source":source.label(),"api":source.api(),"explicit_reset_at":view.current.as_ref().map(|notice|notice.at),"bound":view.current.as_ref().map(|notice|notice.bound),"post_count":view.current.as_ref().map(|notice|notice.posts.len()).unwrap_or(0),"translation_available":view.current.as_ref().is_some_and(|notice|notice.posts.iter().any(|post|!post.translation.is_empty())),"history_count":view.history.len(),"latest_completed_at":view.history.iter().filter(|notice|notice.completed).map(|notice|notice.at).max_by(f64::total_cmp),"bell":format!("{:?}",view.badge(now(),true).0),"error":view.error})
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bell_windows_require_confirmation_and_the_same_local_day() {
        use chrono::TimeZone;
        let noon = chrono::Local
            .with_ymd_and_hms(2026, 10, 26, 12, 0, 0)
            .single()
            .unwrap()
            .timestamp() as f64;
        let mut view = View {
            checked_at: noon,
            current: Some(Notice {
                id: "next".into(),
                at: noon + 3. * 3600.,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(view.badge(noon, true).0, Bell::Upcoming);
        view.current.as_mut().unwrap().at += 1.;
        assert_eq!(view.badge(noon, true).0, Bell::Neutral);
        view.current.as_mut().unwrap().at = noon - 1.;
        assert_eq!(
            view.badge(noon, true).0,
            Bell::Neutral,
            "Passing a forecast alone cannot turn the bell green"
        );
        view.current = None;
        view.history.push(Notice {
            id: "confirmed".into(),
            at: noon - 3. * 3600.,
            completed: true,
            ..Default::default()
        });
        assert_eq!(view.badge(noon, true).0, Bell::Reset);
        view.history[0].at -= 1.;
        assert_eq!(view.badge(noon, true).0, Bell::Neutral);
        let midnight = chrono::Local
            .with_ymd_and_hms(2026, 10, 26, 0, 30, 0)
            .single()
            .unwrap()
            .timestamp() as f64;
        view.checked_at = midnight;
        view.history[0].at = midnight - 3600.;
        assert_eq!(
            view.badge(midnight, true).0,
            Bell::Neutral,
            "A reset yesterday must not remain green today"
        );
    }
    #[test]
    fn status_only_completion_drives_bell_and_reminder_without_localized_api() {
        let bytes=br#"{"data":{"scheduled_reset":null,"latest_reset":{"id":"observed-123","reset_type":"regular","announced_at":"2026-10-26T07:00:00Z","text":"Usage limits reset","source":{"type":"observed"}}}}"#;
        let at = timestamp(&json!("2026-10-26T08:00:00Z")).unwrap();
        let view = parse_codex(bytes, &[], at).unwrap();
        assert!(view.current.is_none());
        assert_eq!(view.history.len(), 1);
        assert!(view.history[0].completed && view.history[0].posts[0].observed);
        assert_eq!(view.badge(at, true).0, Bell::Reset);
        assert_eq!(reminder::Tracker::default().collect(&view, at).len(), 1);
        let mut localized = json!({"events":[{"tweet_id":"observed-123","reset_type":"regular","source":"observed","announced_at":"2026-10-26T07:00:00Z","text":"Usage limits reset","display_text":"已观测到复位"}]});
        let enriched = parse_codex(bytes, &serde_json::to_vec(&localized).unwrap(), at).unwrap();
        assert_eq!(enriched.history.len(), 1);
        assert_eq!(enriched.history[0].posts[0].translation, "已观测到复位");
        localized["events"][0]["reset_type"] = json!("banked");
        let ignored = parse_codex(
            br#"{"data":{"scheduled_reset":null}}"#,
            &serde_json::to_vec(&localized).unwrap(),
            at,
        )
        .unwrap();
        assert!(ignored.history.is_empty());
    }
    #[test]
    fn explicit_divin_completion_uses_actual_time_instead_of_forecast() {
        let at = timestamp(&json!("2026-10-26T08:00:00Z")).unwrap();
        let mut entry = event();
        entry["stage"] = json!("completed");
        entry["forecast"] = Value::Null;
        entry["completedAt"] = json!("2026-10-26T07:00:00Z");
        let view =
            parse_divin(&serde_json::to_vec(&json!({"events":[entry]})).unwrap(), at).unwrap();
        assert_eq!(view.badge(at, false).0, Bell::Reset);
    }
    #[test]
    fn followup_requests_obey_current_source_and_enabled_setting() {
        let stop = AtomicBool::new(false);
        let settings = Mutex::new(Settings {
            source: Source::CodexResets,
            ..Default::default()
        });
        assert!(allow_request(Source::CodexResets, &stop, Some(&settings)).is_ok());
        settings.lock().unwrap().enabled = false;
        assert!(
            allow_request(Source::CodexResets, &stop, Some(&settings)).is_err(),
            "Do not start locale/history after disabling during status fetch"
        );
        {
            let mut value = settings.lock().unwrap();
            value.enabled = true;
            value.source = Source::Divin;
        }
        assert!(allow_request(Source::CodexResets, &stop, Some(&settings)).is_err());
        settings.lock().unwrap().source = Source::CodexResets;
        settings.lock().unwrap().check_minutes = 10;
        assert!(allow_request(Source::CodexResets, &stop, Some(&settings)).is_ok());
        assert!(
            allow_request(Source::CodexResets, &stop, None).is_ok(),
            "Read-only inspection has no live configuration gate"
        );
        stop.store(true, Ordering::Relaxed);
        assert!(allow_request(Source::CodexResets, &stop, None).is_err());
    }
    fn event() -> Value {
        json!({"id":"1","tool":"codex","type":"usage_reset","stage":"scheduled","history":false,"announcedAt":"2026-10-25T03:00:00Z","completedAt":null,"summary":"Reset before the announced time.","forecast":{"at":"2026-10-25T07:00:00Z","bound":"before","expression":"By the stated time","assumedZone":false},"posts":[{"handle":"thsottiaux","url":"https://x.com/thsottiaux/status/1","text":"A scheduled reset.","translation":"一次预告重置。"}]})
    }
    fn at() -> f64 {
        timestamp(&json!("2026-10-25T06:00:00Z")).unwrap()
    }
    fn divin(events: Vec<Value>) -> View {
        parse_divin(
            &serde_json::to_vec(&json!({"events":events})).unwrap(),
            at(),
        )
        .unwrap()
    }
    #[test]
    fn explicit_deadlines_remain_deadlines_and_do_not_complete_on_clock() {
        let view = divin(vec![event()]);
        let notice = view.active(at()).unwrap();
        assert_eq!(notice.bound, Bound::Before);
        assert!(notice.short(at(), true).contains('≤'));
        assert!(notice.short(notice.at + 1., true).contains("待确认"));
        assert!(view.active(notice.at + 1.).is_some());
        assert!(view.active(notice.at + PENDING_AFTER + 1.).is_none());
        assert!(view.tooltip(at(), true).contains("译文"));
        assert!(view.tooltip(at(), true).contains("一次预告"));
        assert!(view.active(at() + STALE_AFTER + 1.).is_none());
    }
    #[test]
    fn uncertain_historical_completed_and_other_product_events_are_not_predictions() {
        for field in ["timezone", "history", "completed", "claude", "url", "vague"] {
            let mut value = event();
            match field {
                "timezone" => value["forecast"]["assumedZone"] = json!(true),
                "history" => value["history"] = json!(true),
                "completed" => value["stage"] = json!("completed"),
                "claude" => value["tool"] = json!("claude-code"),
                "url" => value["posts"][0]["url"] = json!("https://example.com/fake"),
                _ => value["forecast"]["at"] = Value::Null,
            }
            assert!(divin(vec![value]).current.is_none(), "{field}");
        }
    }
    #[test]
    fn history_is_bounded_and_legacy_settings_enable_the_default_source() {
        let mut events = vec![event()];
        for i in 2..9 {
            let mut old = event();
            old["id"] = json!(i.to_string());
            old["stage"] = json!("completed");
            events.push(old);
        }
        let view = divin(events);
        assert_eq!(view.history.len(), 3);
        assert!(view.history.iter().all(|notice| notice.id != "1"));
        let mut settings: Settings = serde_json::from_str("{}").unwrap();
        assert!(settings.enabled);
        assert_eq!(settings.source, Source::CodexResets);
        assert_eq!(settings.check_minutes, 5);
        settings.check_minutes = 0;
        settings.clamp();
        assert_eq!(settings.check_minutes, 1);
        settings.check_minutes = 100;
        settings.clamp();
        assert_eq!(settings.check_minutes, 60);
        assert_eq!(
            serde_json::from_str::<Source>("\"future_source\"").unwrap(),
            Source::CodexResets
        );
    }
    fn codex() -> (Value, Value) {
        let source = json!({"type":"x_post","author":"thsottiaux","url":"https://x.com/thsottiaux/status/2"});
        (
            json!({"data":{"scheduled_reset":{"id":"2","status":"scheduled","reset_type":"regular","announced_at":"2026-10-25T03:00:00Z","scheduled_for":"2026-10-25T07:00:00Z","text":"By the stated time","source":source},"active_watch":{"reset_chance_percent":99}}}),
            json!({"scheduled":{"tweet_id":"2","tweet_url":"https://x.com/thsottiaux/status/2","scheduled_for":"2026-10-25T07:00:00Z","display_text":"预告时间之前"},"events":[{"tweet_id":"observed-1","reset_type":"regular","announced_at":"2026-10-24T03:00:00Z","text":"Observed reset","display_text":"观测到重置","source":"observed","tweet_url":"https://x.com/thsottiaux/status/1"}]}),
        )
    }
    #[test]
    fn codex_status_drives_time_and_locale_pairs_by_identity() {
        let (status, mut locale) = codex();
        let parse = |locale: &Value| {
            parse_codex(
                &serde_json::to_vec(&status).unwrap(),
                &serde_json::to_vec(locale).unwrap(),
                at(),
            )
            .unwrap()
        };
        let view = parse(&locale);
        assert_eq!(view.current.as_ref().unwrap().bound, Bound::Before);
        assert_eq!(
            view.current.as_ref().unwrap().posts[0].translation,
            "预告时间之前"
        );
        assert!(view.history[0].posts[0].observed);
        assert!(view.tooltip(at(), true).contains("[观测]"));
        locale["scheduled"]["tweet_id"] = json!("wrong");
        assert!(parse(&locale).current.unwrap().posts[0]
            .translation
            .is_empty());
    }
    #[test]
    fn codex_explicit_null_is_valid_but_missing_or_wrong_typed_schedule_fails() {
        let empty = parse_codex(br#"{"data":{"scheduled_reset":null}}"#, &[], at()).unwrap();
        assert!(empty.current.is_none());
        for malformed in [
            json!({"data":{}}),
            json!({"data":{"scheduled_reset":[]}}),
            json!({"data":{"scheduled_reset":false}}),
            json!({"data":{"scheduled_reset":"unknown"}}),
        ] {
            assert!(parse_codex(&serde_json::to_vec(&malformed).unwrap(), &[], at()).is_err());
        }
    }
    #[test]
    fn expired_codex_schedule_retains_unconfirmed_original_and_translation() {
        let (status, locale) = codex();
        let expired_at = timestamp(&status["data"]["scheduled_reset"]["scheduled_for"]).unwrap()
            + PENDING_AFTER
            + 1.;
        let view = parse_codex(
            &serde_json::to_vec(&status).unwrap(),
            &serde_json::to_vec(&locale).unwrap(),
            expired_at,
        )
        .unwrap();
        assert!(view.current.is_none());
        let previous = view.history.iter().find(|notice| notice.id == "2").unwrap();
        assert!(!previous.completed);
        assert_eq!(previous.posts[0].translation, "预告时间之前");
        let tooltip = view.tooltip(expired_at, true);
        assert!(tooltip.contains("最近消息"));
        assert!(tooltip.contains("By the stated time"));
        assert!(tooltip.contains("预告时间之前"));
        assert!(tooltip.contains("此前预计（未确认）"));
    }
    #[test]
    fn disappearing_forecasts_remain_history_without_crossing_sources() {
        let (status, locale) = codex();
        let previous = parse_codex(
            &serde_json::to_vec(&status).unwrap(),
            &serde_json::to_vec(&locale).unwrap(),
            at(),
        )
        .unwrap();
        let mut next =
            parse_codex(br#"{"data":{"scheduled_reset":null}}"#, &[], at() + 10.).unwrap();
        copy_enrichment(&mut next, &previous);
        assert!(next.current.is_none());
        assert!(next
            .history
            .iter()
            .any(|notice| notice.id == "2" && !notice.completed));
        assert!(next
            .tooltip(at() + 10., true)
            .contains("By the stated time"));
        let mut another = View {
            source: Source::Divin,
            ..Default::default()
        };
        copy_enrichment(&mut another, &previous);
        assert!(another.history.is_empty());
        let stale = previous.tooltip(at() + STALE_AFTER + 1., true);
        assert!(stale.contains("缓存已过期"));
    }
    #[test]
    fn codex_probability_and_unverified_sources_never_become_reset_times() {
        let (status, locale) = codex();
        for field in ["null", "banked", "observed", "author"] {
            let mut value = status.clone();
            match field {
                "null" => value["data"]["scheduled_reset"]["scheduled_for"] = Value::Null,
                "banked" => value["data"]["scheduled_reset"]["reset_type"] = json!("banked"),
                "observed" => {
                    value["data"]["scheduled_reset"]["source"]["type"] = json!("observed")
                }
                _ => value["data"]["scheduled_reset"]["source"]["author"] = json!("unverified"),
            };
            assert!(parse_codex(
                &serde_json::to_vec(&value).unwrap(),
                &serde_json::to_vec(&locale).unwrap(),
                at()
            )
            .unwrap()
            .current
            .is_none());
        }
    }
    #[test]
    fn tooltips_are_bounded_even_for_long_external_posts() {
        let mut value = event();
        value["posts"][0]["text"] = json!("x".repeat(10000));
        value["posts"][0]["translation"] = json!("译".repeat(10000));
        let view = divin(vec![value]);
        assert!(view.tooltip(at(), true).chars().count() <= 1400);
    }
    #[test]
    fn tooltip_prioritizes_status_and_deduplicates_history_and_check_details() {
        let latest = Notice {
            id: "latest".into(),
            announced_at: at() - 60.,
            posts: vec![Post {
                author: "Tibo".into(),
                original: "A brief update. https://t.co/example".into(),
                translation: "简短更新。 https://t.co/example".into(),
                ..Default::default()
            }],
            summary: "A brief update.".into(),
            ..Default::default()
        };
        let view = View {
            checked_at: at(),
            next_check_at: at() + 300.,
            history: vec![latest],
            ..Default::default()
        };
        for zh in [true, false] {
            let tip = view.tooltip(at(), zh);
            assert_eq!(tip.matches("A brief update.").count(), 1);
            assert_eq!(tip.matches("简短更新。").count(), 1);
            assert!(!tip.contains("https://"));
            assert!(!tip.contains("Bell:") && !tip.contains("铃铛："));
            let lines: Vec<_> = tip.lines().collect();
            assert!(lines[1].contains(if zh { "暂无明确" } else { "No explicit" }));
            assert_eq!(
                tip.matches(if zh { "上次成功" } else { "Last success" })
                    .count(),
                1
            );
        }
        let mut failed = view.clone();
        failed.error = "Service unavailable".into();
        assert_eq!(
            failed
                .tooltip(at(), false)
                .matches("Service unavailable")
                .count(),
            1
        );
    }
    #[test]
    fn tooltip_keeps_future_pending_and_stale_forecasts_distinct() {
        let mut view = View {
            checked_at: at(),
            current: Some(Notice {
                id: "next".into(),
                at: at() + 300.,
                bound: Bound::Before,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(view
            .tooltip(at(), false)
            .lines()
            .nth(1)
            .unwrap()
            .starts_with("Expected reset: by "));
        assert!(view.tooltip(at(), true).contains("剩余:"));
        view.current.as_mut().unwrap().at = at() - 1.;
        assert!(view
            .tooltip(at(), true)
            .lines()
            .nth(1)
            .unwrap()
            .contains("尚未确认重置"));
        assert!(!view.tooltip(at(), true).contains("剩余:"));
        assert!(view
            .tooltip(at() + STALE_AFTER + 1., true)
            .lines()
            .nth(1)
            .unwrap()
            .contains("缓存已过期"));
    }
    #[test]
    fn divin_keeps_announcement_time_wording_and_latest_revision() {
        let mut value = event();
        value["forecast"]["expression"] = json!("By the revised time");
        value["posts"] = json!([
            {"handle":"thsottiaux","url":"https://x.com/thsottiaux/status/1","text":"The reset announcement.","role":"announce"},
            {"handle":"thsottiaux","url":"https://x.com/thsottiaux/status/2","text":"An earlier update.","role":"update"},
            {"handle":"thsottiaux","url":"https://x.com/thsottiaux/status/3","text":"By the revised time, please wait.","role":"update"},
            {"handle":"thsottiaux","url":"https://x.com/thsottiaux/status/4","text":"Additional context.","role":"update"},
            {"handle":"thsottiaux","url":"https://x.com/thsottiaux/status/5","text":"Latest revision.","role":"update"}
        ]);
        let posts = divin(vec![value]).current.unwrap().posts;
        assert_eq!(
            posts
                .iter()
                .map(|post| post.url.rsplit('/').next().unwrap())
                .collect::<Vec<_>>(),
            ["1", "3", "5"]
        );
        assert_eq!(clip("长文内容", 3).chars().count(), 3);
        assert_eq!(clip("hello", 0), "");
    }
}
