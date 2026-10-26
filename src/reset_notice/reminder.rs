use super::{clip, date, Notice, Source, View};
use crate::{read_json, Paths};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::Write,
};

const SEEN: &str = "reset-notice-reminders.json";
pub const LOG: &str = "reset-notice-checks.jsonl";
const MAX_AGE: f64 = 24. * 3600.;

#[derive(Clone, Debug)]
pub struct Reminder {
    pub key: String,
    pub source: Source,
    pub notice: Notice,
}
impl Reminder {
    pub fn text(&self, zh: bool) -> (String, String) {
        let observed = self.notice.posts.first().is_some_and(|post| post.observed);
        let title = match (self.notice.completed, observed, zh) {
            (true, true, true) => "Codex 额度重置：站点已观测到",
            (true, false, true) => "Codex 额度重置：来源报告已完成",
            (false, _, true) => "Codex 新的重置预告",
            (true, true, false) => "Codex reset observed by tracker",
            (true, false, false) => "Codex reset reported complete",
            (false, _, false) => "New Codex reset announcement",
        };
        let when = if self.notice.completed {
            date(self.notice.at, "%m-%d %H:%M %:z")
        } else {
            self.notice.short(crate::now(), zh)
        };
        let disclaimer = if zh {
            "请在 Codex 中核对你的账户额度。"
        } else {
            "Check your own account limits in Codex."
        };
        (
            title.into(),
            clip(
                &format!("{} · {when}\n{disclaimer}", self.source.label()),
                230,
            ),
        )
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Tracker {
    seen: Vec<String>,
    #[serde(skip)]
    pending: HashSet<String>,
}
impl Tracker {
    pub fn load(paths: &Paths) -> Self {
        let mut value: Self =
            serde_json::from_value(read_json(&paths.runtime.join(SEEN), 16384)).unwrap_or_default();
        if value.seen.len() > 64 {
            value.seen.drain(..value.seen.len() - 64);
        }
        value
    }
    pub fn reset_pending(&mut self) {
        self.pending.clear();
    }
    pub fn collect(&mut self, view: &View, at: f64) -> Vec<Reminder> {
        if view.checked_at <= 0. || !(0.0..=super::STALE_AFTER).contains(&(at - view.checked_at)) {
            return vec![];
        }
        let completion = view
            .history
            .iter()
            .filter(|notice| {
                notice.completed
                    && notice.at.is_finite()
                    && notice.at > 0.
                    && (0.0..=MAX_AGE).contains(&(at - notice.at))
            })
            .max_by(|a, b| a.at.total_cmp(&b.at));
        let forecast = view.active(at).filter(|notice| notice.at > at);
        let mut result = Vec::new();
        for notice in completion.into_iter().chain(forecast) {
            // The two trackers refer to the same post with/without "observed-".
            let id = notice.id.strip_prefix("observed-").unwrap_or(&notice.id);
            if id.is_empty() {
                continue;
            }
            let key = if notice.completed {
                format!("complete:{id}")
            } else {
                format!("forecast:{id}:{}", notice.at as i64)
            };
            if !self.seen.contains(&key) && self.pending.insert(key.clone()) {
                result.push(Reminder {
                    key,
                    source: view.source,
                    notice: notice.clone(),
                });
            }
        }
        result
    }
    pub fn acknowledge(&mut self, key: &str, delivered: bool) {
        self.pending.remove(key);
        if delivered && !self.seen.iter().any(|old| old == key) {
            self.seen.push(key.into());
            if self.seen.len() > 64 {
                self.seen.remove(0);
            }
        }
    }
    pub fn save(&self, paths: &Paths) {
        paths.write(SEEN, self);
    }
}

// Small diagnostic records; rotate to four compressed archives without keeping a
// growing history of source posts or copying response bodies into the log.
pub(super) fn log(paths: &Paths, mut value: serde_json::Value) {
    let at = value["at"].as_f64().unwrap_or_else(crate::now);
    value["time"] = json!(date(at, "%Y-%m-%d %H:%M:%S %:z"));
    value["pid"] = json!(std::process::id());
    let path = paths.runtime.join(LOG);
    let result = (|| -> std::io::Result<()> {
        if fs::metadata(&path).is_ok_and(|m| m.len() >= 128 * 1024) {
            let pending = paths.runtime.join("reset-notice-checks.1.gz.pending");
            let mut gzip = flate2::write::GzEncoder::new(
                fs::File::create(&pending)?,
                flate2::Compression::fast(),
            );
            std::io::copy(&mut fs::File::open(&path)?, &mut gzip)?;
            gzip.finish()?;
            for index in (1..4).rev() {
                let older = paths
                    .runtime
                    .join(format!("reset-notice-checks.{index}.gz"));
                if older.exists() {
                    fs::rename(
                        older,
                        paths
                            .runtime
                            .join(format!("reset-notice-checks.{}.gz", index + 1)),
                    )?;
                }
            }
            fs::rename(pending, paths.runtime.join("reset-notice-checks.1.gz"))?;
            fs::remove_file(&path)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        serde_json::to_writer(&mut file, &value)?;
        file.write_all(b"\n")
    })();
    let _ = result;
}
pub(super) fn check_log(paths: &Paths, view: &View, source: Source, success: bool) {
    log(
        paths,
        json!({"event":"check","at":crate::now(),"started_at":view.attempted_at,"duration_ms":((crate::now()-view.attempted_at).max(0.)*1000.) as u64,
        "source":source,"api":source.api(),"success":success,"error":clip(&view.error,300),
        "last_success_at":view.checked_at,"next_check_at":view.next_check_at,
        "forecast_at":view.current.as_ref().map(|n|n.at),
        "latest_completed":view.history.iter().find(|n|n.completed).map(|n|json!({"id":n.id,"at":n.at}))}),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    fn view(at: f64) -> View {
        View {
            source: Source::CodexResets,
            checked_at: at,
            history: vec![Notice {
                id: "observed-123".into(),
                at: at - 60.,
                completed: true,
                ..Default::default()
            }],
            ..Default::default()
        }
    }
    #[test]
    fn completion_is_delivered_once_across_checks_restarts_and_source_switches() {
        let at = 1_800_000_000.;
        let mut tracker = Tracker::default();
        let view = view(at);
        let alert = tracker.collect(&view, at);
        assert_eq!(alert.len(), 1);
        assert!(
            tracker.collect(&view, at + 1.).is_empty(),
            "Pending notification must not be enqueued twice"
        );
        tracker.acknowledge(&alert[0].key, true);
        let mut restored: Tracker =
            serde_json::from_value(serde_json::to_value(&tracker).unwrap()).unwrap();
        assert!(restored.collect(&view, at + 2.).is_empty());
        let mut other = view.clone();
        other.source = Source::Divin;
        other.history[0].id = "123".into();
        assert!(restored.collect(&other, at + 3.).is_empty());
    }
    #[test]
    fn failed_delivery_retries_and_old_or_future_completed_records_do_not_notify() {
        let at = 1_800_000_000.;
        let view = view(at);
        let mut tracker = Tracker::default();
        let alerts = tracker.collect(&view, at);
        tracker.acknowledge(&alerts[0].key, false);
        assert_eq!(tracker.collect(&view, at).len(), 1);
        for time in [at - MAX_AGE - 1., at + 1., f64::NAN] {
            let mut bad = view.clone();
            bad.history[0].at = time;
            assert!(Tracker::default().collect(&bad, at).is_empty());
        }
        let mut stale = view.clone();
        stale.checked_at = at - super::super::STALE_AFTER - 1.;
        assert!(Tracker::default().collect(&stale, at).is_empty());
    }
    #[test]
    fn announcement_deadline_is_not_a_confirmation_and_observations_are_labelled() {
        let at = 1_800_000_000.;
        let mut view = view(at);
        view.history[0].completed = false;
        assert!(Tracker::default().collect(&view, at).is_empty());
        view.current = Some(Notice {
            id: "future".into(),
            at: at + 60.,
            ..Default::default()
        });
        let alert = Tracker::default().collect(&view, at);
        assert_eq!(alert.len(), 1);
        assert!(!alert[0].notice.completed);
        let mut notice = alert[0].clone();
        notice.notice.completed = true;
        notice.notice.posts.push(super::super::Post {
            observed: true,
            ..Default::default()
        });
        assert!(notice.text(true).0.contains("站点"));
        assert!(notice.text(false).1.contains("your own account"));
    }
    #[test]
    fn scan_log_rotates_to_bounded_compressed_archives_in_workspace_tmp() {
        use std::{io::Read, path::PathBuf};
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .canonicalize()
            .unwrap();
        let tmp = workspace.join("tmp");
        let root = tmp.join(format!("reset-log-test-{}", std::process::id()));
        assert!(root.starts_with(&tmp));
        fs::create_dir_all(&root).unwrap();
        let paths = Paths {
            app: workspace.clone(),
            home: workspace,
            runtime: root.clone(),
            instance: "test".into(),
        };
        for index in 0..6 {
            fs::write(root.join(LOG), vec![b'x'; 128 * 1024]).unwrap();
            log(&paths, json!({"event":"test","index":index}));
        }
        for index in 1..=4 {
            let file =
                fs::File::open(root.join(format!("reset-notice-checks.{index}.gz"))).unwrap();
            let mut decoder = flate2::read::GzDecoder::new(file);
            let mut byte = [0];
            decoder.read_exact(&mut byte).unwrap();
            assert_eq!(byte[0], b'x');
        }
        assert!(!root.join("reset-notice-checks.5.gz").exists());
        assert!(fs::metadata(root.join(LOG)).unwrap().len() < 1024);
        let resolved = root.canonicalize().unwrap();
        assert!(resolved.starts_with(tmp.canonicalize().unwrap()));
        fs::remove_dir_all(resolved).unwrap();
    }
}
