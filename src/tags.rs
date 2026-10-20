//! Local tag preferences. Codex metadata is read-only; custom tags never alter it.
use crate::{data::Row, read_json, Paths};
use anyhow::{ensure, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    time::SystemTime,
};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Project,
    Group,
    #[default]
    Custom,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tag {
    pub key: String,
    pub label: String,
    pub kind: Kind,
    #[serde(default)]
    pub color: usize,
}
impl Tag {
    pub fn colors(&self) -> (u32, u32) {
        match self.kind {
            Kind::Project => (0x283c4e, 0x92c8f3),
            Kind::Group => (0x3c334d, 0xc3acf0),
            Kind::Custom => PALETTE[self.color % PALETTE.len()],
        }
    }
}
pub const PALETTE: [(u32, u32); 6] = [
    (0x29463e, 0x94d4b7),
    (0x4b3c29, 0xe9c18c),
    (0x4b303b, 0xe5a9bb),
    (0x30444b, 0x9bd4de),
    (0x3f4030, 0xd0d49b),
    (0x414049, 0xc5c5d5),
];
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Custom {
    pub definitions: Vec<Tag>,
    pub chats: HashMap<String, Vec<String>>,
}
impl Custom {
    pub fn load(paths: &Paths) -> Self {
        let mut value: Self =
            serde_json::from_value(read_json(&paths.app.join("tags.json"), 1024 * 1024))
                .unwrap_or_default();
        value.definitions.retain(|t| {
            t.kind == Kind::Custom && t.key.starts_with("custom:") && !t.label.trim().is_empty()
        });
        value
    }
    pub fn save(&self, paths: &Paths) -> Result<()> {
        let pending = paths.runtime.join("tags.pending");
        fs::write(&pending, serde_json::to_vec_pretty(self)?)?;
        fs::rename(pending, paths.app.join("tags.json"))?;
        Ok(())
    }
    pub fn for_row(&self, row: &Row) -> Vec<Tag> {
        let mut result = row.tags.clone();
        if let Some(keys) = self.chats.get(&row.id) {
            result.extend(
                self.definitions
                    .iter()
                    .filter(|t| keys.contains(&t.key))
                    .cloned(),
            );
        }
        result
    }
    pub fn matches(&self, row: &Row, keys: &HashSet<String>) -> bool {
        keys.is_empty()
            || keys.iter().all(|key| {
                row.tags.iter().any(|t| &t.key == key)
                    || self
                        .chats
                        .get(&row.id)
                        .is_some_and(|tags| tags.contains(key))
            })
    }
    pub fn toggle(&mut self, id: &str, key: &str) {
        if !self
            .definitions
            .iter()
            .any(|t| t.key == key && t.kind == Kind::Custom)
        {
            return;
        }
        let keys = self.chats.entry(id.into()).or_default();
        if keys.iter().any(|k| k == key) {
            keys.retain(|k| k != key);
        } else {
            keys.push(key.into());
        }
    }
    pub fn add(&mut self, id: &str, label: &str, color: usize) -> Result<()> {
        let label = label.trim();
        ensure!(
            !label.is_empty() && label.chars().count() <= 32,
            "Use 1–32 characters"
        );
        ensure!(
            !label.chars().any(char::is_control),
            "Use a single-line tag name"
        );
        let key = format!("custom:{}", label.to_lowercase());
        if !self.definitions.iter().any(|t| t.key == key) {
            ensure!(self.definitions.len() < 256, "Custom tag limit reached");
            self.definitions.push(Tag {
                key: key.clone(),
                label: label.into(),
                kind: Kind::Custom,
                color: color % PALETTE.len(),
            });
        }
        let tags = self.chats.entry(id.into()).or_default();
        if !tags.contains(&key) {
            tags.push(key);
        }
        Ok(())
    }
    pub fn catalog(&self, rows: &[Row]) -> Vec<(Tag, usize)> {
        let mut map = BTreeMap::<String, (Tag, usize)>::new();
        for tag in &self.definitions {
            map.insert(tag.key.clone(), (tag.clone(), 0));
        }
        for row in rows {
            for tag in self.for_row(row) {
                let entry = map.entry(tag.key.clone()).or_insert((tag, 0));
                entry.1 += 1;
            }
        }
        let mut result: Vec<_> = map.into_values().collect();
        result.sort_by(|a, b| {
            kind_order(&a.0.kind)
                .cmp(&kind_order(&b.0.kind))
                .then(a.0.label.to_lowercase().cmp(&b.0.label.to_lowercase()))
        });
        result
    }
}
fn kind_order(k: &Kind) -> u8 {
    match k {
        Kind::Project => 0,
        Kind::Group => 1,
        Kind::Custom => 2,
    }
}

fn local_assignment(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned).or_else(|| {
        (value["projectKind"].as_str() == Some("local"))
            .then(|| value["projectId"].as_str().map(str::to_owned))
            .flatten()
    })
}
fn normalized_root(path: &str) -> String {
    let path = path.replace('/', "\\");
    let path = path
        .strip_prefix(r"\\?\UNC\")
        .map(|p| format!(r"\\{p}"))
        .unwrap_or_else(|| path.strip_prefix(r"\\?\").unwrap_or(&path).to_string());
    path.trim_end_matches('\\').to_lowercase()
}
fn root_project(cwd: &str, roots: &[(String, String)]) -> Option<String> {
    let cwd = normalized_root(cwd);
    if cwd.is_empty() {
        return None;
    }
    let mut best = 0;
    let mut ids = HashSet::new();
    for (root, id) in roots {
        if root.is_empty() {
            continue;
        }
        if cwd == *root
            || cwd
                .strip_prefix(root)
                .is_some_and(|tail| tail.starts_with('\\'))
        {
            if root.len() > best {
                best = root.len();
                ids.clear();
            }
            if root.len() == best {
                ids.insert(id.clone());
            }
        }
    }
    if ids.len() == 1 {
        ids.into_iter().next()
    } else {
        None
    }
}
#[derive(Default)]
pub struct Sources {
    stamp: Option<(SystemTime, u64)>,
    global: Value,
}
impl Sources {
    pub fn enrich(&mut self, paths: &Paths, db: &Connection, payload: &Value, rows: &mut [Row]) {
        let file = paths.home.join(".codex-global-state.json");
        let stamp = fs::metadata(&file)
            .ok()
            .and_then(|m| Some((m.modified().ok()?, m.len())));
        if stamp != self.stamp {
            self.global = read_json(&file, 2 * 1024 * 1024);
            self.stamp = stamp;
        }
        let g = &self.global;
        let mut names = HashMap::<String, String>::new();
        let mut legacy = HashMap::<String, String>::new();
        if let Some(hosts) = g["app-server-project-id-by-legacy-project-id-by-host"].as_object() {
            for (host, map) in hosts {
                if host.starts_with("local:") {
                    if let Some(map) = map.as_object() {
                        for (k, v) in map {
                            if let Some(s) = v.as_str() {
                                legacy.insert(k.clone(), s.into());
                            }
                        }
                    }
                }
            }
        }
        let mut roots = Vec::<(String, String)>::new();
        if let Some(projects) = g["local-projects"].as_object() {
            for (id, p) in projects {
                if let Some(paths) = p["rootPaths"].as_array() {
                    for path in paths.iter().filter_map(Value::as_str) {
                        roots.push((normalized_root(path), legacy.get(id).unwrap_or(id).clone()));
                    }
                }
                if let Some(name) = p["name"].as_str() {
                    names.insert(id.clone(), name.into());
                    if let Some(new) = legacy.get(id) {
                        names.insert(new.clone(), name.into());
                    }
                }
            }
        }
        if let Ok(mut q) = db.prepare("SELECT id,name FROM projects") {
            if let Ok(entries) =
                q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            {
                for (id, name) in entries.flatten() {
                    names.insert(id, name);
                }
            }
        }
        if let Ok(mut q) = db.prepare("SELECT project_id,path FROM project_roots") {
            if let Ok(values) =
                q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            {
                for (id, path) in values.flatten() {
                    roots.push((normalized_root(&path), id));
                }
            }
        }
        let mut working_dirs = HashMap::<String, String>::new();
        if let Ok(mut q) =
            db.prepare("SELECT id,cwd FROM threads WHERE archived=0 AND agent_path IS NULL")
        {
            if let Ok(values) =
                q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            {
                working_dirs.extend(values.flatten());
            }
        }
        let mut groups = HashMap::<String, Tag>::new();
        let mut sections: Vec<Value> = payload["sections"].as_array().cloned().unwrap_or_default();
        if let Some(accounts) = g
            .pointer("/electron-persisted-atom-state/sidebar-custom-sections-v3")
            .and_then(Value::as_object)
        {
            for account in accounts.values() {
                if let Some(v) = account["sections"].as_array() {
                    sections.extend(v.iter().cloned());
                }
            }
        }
        for section in sections {
            let Some(name) = section["name"].as_str() else {
                continue;
            };
            let id = section["sectionId"]
                .as_str()
                .or_else(|| section["id"].as_str())
                .unwrap_or("");
            if ["pinned", "threads", "chats", ""].contains(&id) {
                continue;
            }
            let tag = Tag {
                key: format!("group:{id}"),
                label: name.into(),
                kind: Kind::Group,
                color: 0,
            };
            groups.insert(id.into(), tag.clone());
            if let Some(host) = section
                .pointer("/hostSectionIds/local")
                .and_then(Value::as_str)
            {
                groups.insert(host.into(), tag.clone());
            }
            if let Some(items) = section["itemKeys"].as_array() {
                for item in items.iter().filter_map(Value::as_str) {
                    groups.insert(item.into(), tag.clone());
                    if let Some(old) = item.strip_prefix("codex:project:") {
                        if let Some(new) = legacy.get(old) {
                            groups.insert(format!("codex:project:{new}"), tag.clone());
                        }
                    }
                }
            }
        }
        let mut membership = HashMap::<String, (Option<String>, Option<String>)>::new();
        if let Ok(mut q)=db.prepare("SELECT id,project_id,thread_section_id FROM threads WHERE archived=0 AND agent_path IS NULL"){
            if let Ok(values)=q.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,Option<String>>(2)?))){for(id,p,s)in values.flatten(){membership.insert(id,(p,s));}}
        }
        for row in rows {
            let (project, section) = membership.get(&row.id).cloned().unwrap_or_default();
            let project = project
                .or_else(|| local_assignment(&g["thread-project-assignments"][&row.id]))
                .or_else(|| {
                    working_dirs
                        .get(&row.id)
                        .and_then(|cwd| root_project(cwd, &roots))
                });
            if let Some(p) = &project {
                let canonical = legacy.get(p).unwrap_or(p);
                if let Some(name) = names.get(canonical).or_else(|| names.get(p)) {
                    row.tags.push(Tag {
                        key: format!("project:{}", legacy.get(p).unwrap_or(p)),
                        label: name.clone(),
                        kind: Kind::Project,
                        color: 0,
                    });
                }
            }
            let group = groups
                .get(&format!("codex:thread:local:{}", row.id))
                .or_else(|| section.as_ref().and_then(|s| groups.get(s)))
                .or_else(|| {
                    project
                        .as_ref()
                        .and_then(|p| groups.get(&format!("codex:project:{p}")))
                });
            if let Some(tag) = group {
                row.tags.push(tag.clone());
            }
        }
    }
}

/// Compact visible user prose, excluding transport envelopes and context blocks.
pub fn simplify_prompt(raw: &str) -> String {
    let mut text = raw.trim();
    if let Some((_, request)) = text.split_once("## My request:") {
        text = request.trim();
    }
    let skip = [
        "# AGENTS.md",
        "<environment_context>",
        "<heartbeat>",
        "<send_user_message_question_reply>",
    ];
    if skip.iter().any(|prefix| text.starts_with(prefix)) {
        return String::new();
    }
    let mut in_context = false;
    let mut parts = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("<in-app-browser-context") {
            in_context = true;
            continue;
        }
        if line.starts_with("</in-app-browser-context>") {
            in_context = false;
            continue;
        }
        if in_context
            || line.is_empty()
            || line.starts_with("<image")
            || line.starts_with("# Files mentioned")
            || line.starts_with("## Referenced chats")
            || line.starts_with("Image attachment:")
            || line.starts_with("Distinguish instructions")
            || line.starts_with("```")
        {
            continue;
        }
        parts.push(line);
        if parts.iter().map(|p| p.len()).sum::<usize>() > 1500 {
            break;
        }
    }
    let joined = parts
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut preview: String = joined.chars().take(220).collect();
    if joined.chars().count() > 220 {
        preview.push('…');
    }
    preview
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_project_assignments_and_root_paths_match_without_ambiguity() {
        use serde_json::json;
        assert_eq!(
            local_assignment(&json!({"projectKind":"local","projectId":"p"})),
            Some("p".into())
        );
        assert_eq!(local_assignment(&json!("old")), Some("old".into()));
        assert_eq!(
            local_assignment(&json!({"projectKind":"cloud","projectId":"p"})),
            None
        );
        let roots = vec![
            (normalized_root(r"D:\Work\project"), "a".into()),
            (normalized_root(r"D:\Work\project\nested"), "b".into()),
        ];
        assert_eq!(
            root_project(r"\\?\D:\WORK\Project\", &roots),
            Some("a".into())
        );
        assert_eq!(
            root_project("D:/Work/project/nested/src", &roots),
            Some("b".into())
        );
        assert_eq!(root_project(r"D:\Work\project-other", &roots), None);
        let mut ambiguous = roots.clone();
        ambiguous.push((normalized_root(r"D:\Work\project"), "other".into()));
        assert_eq!(root_project(r"D:\Work\project", &ambiguous), None);
    }
    #[test]
    fn enrich_recovers_object_assignment_and_existing_workspace_project() {
        use serde_json::json;
        let base = Paths::discover().unwrap();
        let paths = Paths {
            home: base
                .runtime
                .join(format!("absent-tag-source-{}", std::process::id())),
            ..base
        };
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(r"CREATE TABLE projects(id TEXT,name TEXT);CREATE TABLE project_roots(project_id TEXT,path TEXT);CREATE TABLE threads(id TEXT,project_id TEXT,thread_section_id TEXT,archived INTEGER,agent_path TEXT,cwd TEXT);INSERT INTO projects VALUES('new','工程甲'),('explicit','工程乙');
            INSERT INTO project_roots VALUES('new','D:\Work\demo');
            INSERT INTO threads VALUES('object',NULL,NULL,0,NULL,''),('root',NULL,NULL,0,NULL,'\\?\D:\WORK\demo\src'),('explicit','explicit',NULL,0,NULL,'D:\Work\demo');").unwrap();
        let mut sources = Sources {
            stamp: None,
            global: json!({"local-projects":{"old":{"name":"旧名称"}},"app-server-project-id-by-legacy-project-id-by-host":{"local:test":{"old":"new"}},"thread-project-assignments":{"object":{"projectKind":"local","projectId":"old"}}}),
        };
        let mut rows = vec![
            Row {
                id: "object".into(),
                ..Default::default()
            },
            Row {
                id: "root".into(),
                ..Default::default()
            },
            Row {
                id: "explicit".into(),
                ..Default::default()
            },
        ];
        sources.enrich(&paths, &db, &json!({}), &mut rows);
        assert_eq!(rows[0].tags[0].key, "project:new");
        // Current database names supersede migrated legacy names.
        assert_eq!(rows[0].tags[0].label, "工程甲");
        assert_eq!(rows[1].tags[0].label, "工程甲");
        assert_eq!(rows[2].tags[0].label, "工程乙");
    }
    #[test]
    fn tags_keep_sources_locked_and_filter_all_selected() {
        let r = Row {
            id: "a".into(),
            tags: vec![Tag {
                key: "project:p".into(),
                label: "P".into(),
                kind: Kind::Project,
                color: 5,
            }],
            ..Default::default()
        };
        let mut tags = Custom::default();
        tags.toggle("a", "project:p");
        assert!(tags.chats.is_empty());
        tags.add("a", "排查", 0).unwrap();
        tags.add("a", "排查", 2).unwrap();
        assert_eq!(tags.definitions.len(), 1);
        assert!(tags.matches(
            &r,
            &HashSet::from(["project:p".into(), "custom:排查".into()])
        ));
        assert!(!tags.matches(&r, &HashSet::from(["custom:不存在".into()])));
        assert_eq!(r.tags[0].colors(), (0x283c4e, 0x92c8f3));
    }
    #[test]
    fn prompt_strips_attachment_context_and_short_follows() {
        assert_eq!(simplify_prompt("# Files mentioned by the user:\nx.png\n## My request:\n请检查图像采集链路中的帧丢失问题。"),"请检查图像采集链路中的帧丢失问题。");
        assert_eq!(simplify_prompt("<heartbeat>test</heartbeat>"), "");
        assert_eq!(simplify_prompt("<in-app-browser-context>\nnot user content\n</in-app-browser-context>\n请检查图像采集链路。"),"请检查图像采集链路。");
    }
}
