//! Bounded recent-user previews, keyed by the current physical history source.
use crate::{body, data::Row, tags};
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

const META:&str="SELECT item_id,rollout_ordinal,updated_at_ordinal FROM thread_items WHERE thread_id=?1 AND item_type='userMessage' AND rollout_ordinal<?2 ORDER BY rollout_ordinal DESC LIMIT ?3";
const MESSAGES:&str="SELECT item_id,rollout_ordinal,updated_at_ordinal,CASE WHEN length(CAST(item_json AS BLOB))<=65536 THEN item_json ELSE NULL END FROM thread_items WHERE thread_id=?1 AND item_type='userMessage' AND rollout_ordinal<?2 ORDER BY rollout_ordinal DESC LIMIT 64";

#[derive(Clone, Debug, PartialEq, Eq)]
struct Revision {
    item: String,
    ordinal: i64,
    updated: i64,
}
#[derive(Default)]
struct Cached {
    source: String,
    checked: Vec<Revision>,
    text: String,
}
#[derive(Default)]
pub struct Store {
    cache: HashMap<String, Cached>,
    #[cfg(test)]
    body_reads: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    const ROOT: &str = "11111111-1111-4111-8111-111111111111";
    const ALIAS: &str = "22222222-2222-4222-8222-222222222222";
    fn fixture() -> (Connection, Connection, Vec<Row>, Store) {
        let state = Connection::open_in_memory().unwrap();
        let history = Connection::open_in_memory().unwrap();
        state
            .execute_batch(
                "CREATE TABLE threads(id TEXT,rollout_path TEXT,archived INTEGER,agent_path TEXT);",
            )
            .unwrap();
        state
            .execute("INSERT INTO threads VALUES(?,NULL,0,NULL)", [ROOT])
            .unwrap();
        history.execute_batch("CREATE TABLE thread_items(thread_id TEXT,item_id TEXT,item_type TEXT,rollout_ordinal INTEGER,updated_at_ordinal INTEGER,item_json TEXT);
            CREATE INDEX users ON thread_items(thread_id,rollout_ordinal) WHERE item_type='userMessage';
            CREATE TABLE thread_history_projection_state(thread_id TEXT PRIMARY KEY,next_rollout_ordinal INTEGER);").unwrap();
        (
            state,
            history,
            vec![Row {
                id: ROOT.into(),
                ..Default::default()
            }],
            Store::default(),
        )
    }
    fn insert(db: &Connection, source: &str, item: &str, ordinal: i64, revision: i64, text: &str) {
        let json =
            serde_json::json!({"type":"userMessage","content":[{"type":"text","text":text}]})
                .to_string();
        db.execute(
            "INSERT INTO thread_items VALUES(?,?,'userMessage',?,?,?)",
            params![source, item, ordinal, revision, json],
        )
        .unwrap();
    }
    #[test]
    fn resumed_rollout_uses_its_history_id_and_warm_refresh_reads_no_bodies() {
        let (state, history, mut rows, mut store) = fixture();
        insert(
            &history,
            ROOT,
            "old",
            2000,
            2000,
            "原来会话中很久以前的旧提示词。",
        );
        insert(
            &history,
            ALIAS,
            "new",
            20,
            20,
            "这是恢复会话中最新的有效用户提问。",
        );
        state
            .execute(
                "UPDATE threads SET rollout_path=?",
                [format!(
                    r"C:\sessions\rollout-2026-10-18T21-51-55-{ROOT}_{ALIAS}.jsonl"
                )],
            )
            .unwrap();
        store.refresh(&state, &history, &mut rows).unwrap();
        assert_eq!(rows[0].prompt, "这是恢复会话中最新的有效用户提问。");
        let count = store.body_reads;
        store.refresh(&state, &history, &mut rows).unwrap();
        assert_eq!(store.body_reads, count);
        insert(
            &history,
            ALIAS,
            "newer",
            21,
            21,
            "恢复会话中第二个刚刚发送的新提问。",
        );
        store.refresh(&state, &history, &mut rows).unwrap();
        assert_eq!(rows[0].prompt, "恢复会话中第二个刚刚发送的新提问。");
    }
    #[test]
    fn identity_invalidates_equal_revision_and_source_switches() {
        let (state, history, mut rows, mut store) = fixture();
        insert(
            &history,
            ROOT,
            "old-id",
            1,
            7,
            "缓存更新之前的用户提问内容。",
        );
        store.refresh(&state, &history, &mut rows).unwrap();
        history.execute("DELETE FROM thread_items", []).unwrap();
        insert(
            &history,
            ROOT,
            "new-id",
            1,
            7,
            "序号相同但实际已经更换的新提问。",
        );
        store.refresh(&state, &history, &mut rows).unwrap();
        assert_eq!(rows[0].prompt, "序号相同但实际已经更换的新提问。");
        insert(
            &history,
            ALIAS,
            "new-id",
            1,
            7,
            "历史来源也变化时要再次刷新提示词。",
        );
        state
            .execute(
                "UPDATE threads SET rollout_path=?",
                [format!("rollout-date-{ROOT}_{ALIAS}.jsonl")],
            )
            .unwrap();
        store.refresh(&state, &history, &mut rows).unwrap();
        assert_eq!(rows[0].prompt, "历史来源也变化时要再次刷新提示词。");
    }
    #[test]
    fn short_prompt_fallback_refreshes_when_a_preceding_candidate_is_edited() {
        let (state, history, mut rows, mut store) = fixture();
        insert(
            &history,
            ROOT,
            "long",
            1,
            1,
            "此前一个完整而有效的用户提问内容。",
        );
        insert(
            &history,
            ROOT,
            "context",
            2,
            2,
            "# AGENTS.md instructions\nmetadata",
        );
        insert(&history, ROOT, "short", 3, 3, "继续");
        store.refresh(&state, &history, &mut rows).unwrap();
        assert_eq!(rows[0].prompt, "此前一个完整而有效的用户提问内容。");
        let count = store.body_reads;
        store.refresh(&state, &history, &mut rows).unwrap();
        assert_eq!(store.body_reads, count);
        history
            .execute("DELETE FROM thread_items WHERE item_id='context'", [])
            .unwrap();
        insert(
            &history,
            ROOT,
            "context",
            2,
            4,
            "刚刚编辑并且应该优先显示的完整提问。",
        );
        store.refresh(&state, &history, &mut rows).unwrap();
        assert_eq!(rows[0].prompt, "刚刚编辑并且应该优先显示的完整提问。");
    }
    #[test]
    fn projection_restart_excludes_old_high_ordinals_and_does_not_reuse_parent_data() {
        let (state, history, mut rows, mut store) = fixture();
        insert(
            &history,
            ROOT,
            "old",
            999,
            999,
            "旧索引中序号很大的历史提问内容。",
        );
        store.refresh(&state, &history, &mut rows).unwrap();
        insert(
            &history,
            ROOT,
            "new",
            1,
            1,
            "重新建立索引后最新有效的用户提问。",
        );
        history
            .execute(
                "INSERT INTO thread_history_projection_state VALUES(?,2)",
                [ROOT],
            )
            .unwrap();
        store.refresh(&state, &history, &mut rows).unwrap();
        assert_eq!(rows[0].prompt, "重新建立索引后最新有效的用户提问。");
        state
            .execute(
                "UPDATE threads SET rollout_path=?",
                [format!("rollout-date-{ROOT}_{ALIAS}.jsonl")],
            )
            .unwrap();
        history
            .execute(
                "INSERT INTO thread_history_projection_state VALUES(?,0)",
                [ALIAS],
            )
            .unwrap();
        store.refresh(&state, &history, &mut rows).unwrap();
        assert!(
            rows[0].prompt.is_empty(),
            "An unwarmed active source must not show a parent's old prompt"
        );
    }
    #[test]
    fn alias_is_bound_to_the_public_id_and_requires_an_existing_projection() {
        let (state, history, mut rows, mut store) = fixture();
        insert(
            &history,
            ROOT,
            "root",
            1,
            1,
            "正常会话中最新而且完整的用户提问。",
        );
        assert!(alias(ROOT, &format!("unrelated-{ALIAS}.jsonl")).is_none());
        assert!(alias(ROOT, &format!("rollout-{ROOT}_not-a-uuid.jsonl")).is_none());
        state
            .execute(
                "UPDATE threads SET rollout_path=?",
                [format!("rollout-date-{ROOT}_{ALIAS}.jsonl")],
            )
            .unwrap();
        store.refresh(&state, &history, &mut rows).unwrap();
        assert_eq!(rows[0].prompt, "正常会话中最新而且完整的用户提问。");
    }
}
fn uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}
pub(crate) fn alias(id: &str, path: &str) -> Option<String> {
    if !uuid(id) {
        return None;
    }
    let stem = Path::new(path).file_stem()?.to_str()?;
    let (_, suffix) = stem.split_once(&format!("-{id}_"))?;
    let parts: Vec<_> = suffix.split('_').collect();
    if parts.is_empty() || !parts.iter().all(|part| uuid(part)) {
        return None;
    }
    let last = *parts.last()?;
    (last != id).then(|| last.to_string())
}
fn revision(row: &rusqlite::Row<'_>) -> rusqlite::Result<Revision> {
    Ok(Revision {
        item: row.get(0)?,
        ordinal: row.get(1)?,
        updated: row.get(2)?,
    })
}
impl Store {
    pub fn source(&self, id: &str) -> Option<&str> {
        self.cache.get(id).map(|cached| cached.source.as_str())
    }
    pub fn refresh(
        &mut self,
        state: &Connection,
        history: &Connection,
        rows: &mut [Row],
    ) -> Result<()> {
        let (Ok(mut meta), Ok(mut messages)) = (history.prepare(META), history.prepare(MESSAGES))
        else {
            return Ok(());
        };
        let mut limits=history.prepare("SELECT next_rollout_ordinal FROM thread_history_projection_state WHERE thread_id=?").ok();
        let mut paths = HashMap::new();
        if let Ok(mut query)=state.prepare("SELECT id,rollout_path FROM threads WHERE archived=0 AND agent_path IS NULL AND rollout_path IS NOT NULL") {
            for row in query.query_map([],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))? {
                let (id,path)=row?;paths.insert(id,path);
            }
        }
        for row in rows.iter_mut() {
            let mut source = row.id.clone();
            let mut bound = limits
                .as_mut()
                .and_then(|query| {
                    query
                        .query_row([&source], |row| row.get::<_, i64>(0))
                        .optional()
                        .ok()
                        .flatten()
                })
                .unwrap_or(i64::MAX)
                .max(0);
            if let Some(candidate) = paths.get(&row.id).and_then(|path| alias(&row.id, path)) {
                let candidate_bound = limits.as_mut().and_then(|query| {
                    query
                        .query_row([&candidate], |row| row.get::<_, i64>(0))
                        .optional()
                        .ok()
                        .flatten()
                });
                let limit = candidate_bound.unwrap_or(i64::MAX).max(0);
                let head = meta
                    .query_row(params![candidate, limit, 1], revision)
                    .optional()?;
                if candidate_bound.is_some() || head.is_some() {
                    source = candidate;
                    bound = limit;
                }
            }
            let head = meta
                .query_row(params![source, bound, 1], revision)
                .optional()?;
            let unchanged = if let Some(cached) = self
                .cache
                .get(&row.id)
                .filter(|cached| cached.source == source)
            {
                if cached.checked.first() == head.as_ref() {
                    if cached.checked.len() <= 1 {
                        true
                    } else {
                        let current = meta
                            .query_map(
                                params![source, bound, cached.checked.len().min(64) as i64],
                                revision,
                            )?
                            .collect::<rusqlite::Result<Vec<_>>>()?;
                        current == cached.checked
                    }
                } else {
                    false
                }
            } else {
                false
            };
            if !unchanged {
                let mut checked = Vec::new();
                let mut first = String::new();
                let mut chosen = String::new();
                let mut seen = false;
                let mut values = messages.query(params![source, bound])?;
                while let Some(value) = values.next()? {
                    checked.push(revision(value)?);
                    let Some(json) = value.get::<_, Option<String>>(3)? else {
                        continue;
                    };
                    #[cfg(test)]
                    {
                        self.body_reads += 1;
                    }
                    let Some(text) = body::history_text(&json) else {
                        continue;
                    };
                    let text = tags::simplify_prompt(&text);
                    if text.is_empty() {
                        continue;
                    }
                    let count = text.chars().filter(|c| !c.is_whitespace()).count();
                    if (!seen && count >= 10) || (seen && count > 10) {
                        chosen = text;
                        break;
                    }
                    if !seen {
                        first = text;
                        seen = true;
                    }
                }
                if chosen.is_empty() {
                    chosen = first;
                }
                self.cache.insert(
                    row.id.clone(),
                    Cached {
                        source,
                        checked,
                        text: chosen,
                    },
                );
            }
            row.prompt = self
                .cache
                .get(&row.id)
                .map(|cached| cached.text.clone())
                .unwrap_or_default();
        }
        let ids: HashSet<_> = rows.iter().map(|row| row.id.as_str()).collect();
        self.cache.retain(|id, _| ids.contains(id.as_str()));
        Ok(())
    }
}
