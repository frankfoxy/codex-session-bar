//! Internal approval reviews and spawned agents are not user chats.
use anyhow::Result;
use rusqlite::Connection;
use std::collections::HashSet;

pub fn internal_reviews(db: &Connection) -> Result<HashSet<String>> {
    let mut schema = db.prepare("PRAGMA table_info(threads)")?;
    let columns = schema
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<HashSet<_>>>()?;
    // Older Codex databases (and imported records) may only have one marker.
    let source = if columns.contains("source") {
        "source"
    } else {
        "NULL"
    };
    let kind = if columns.contains("thread_source") {
        "thread_source"
    } else {
        "NULL"
    };
    // Codex can create an attached side-chat shell before it has a real turn.
    // It inherits its parent's name but is not a second populated chat.
    let attached = if columns.contains("attached_thread_parent_id") && columns.contains("has_user_event") && columns.contains("tokens_used") && columns.contains("title") {
        "CASE WHEN NULLIF(TRIM(attached_thread_parent_id),'') IS NOT NULL AND COALESCE(has_user_event,0)=0 AND COALESCE(tokens_used,0)=0 AND COALESCE(TRIM(title),'')='' THEN 1 ELSE 0 END"
    } else { "0" };
    let mut query = db.prepare(&format!("SELECT id,{source},{kind},{attached} FROM threads"))?;
    let rows = query.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, i64>(3)? != 0,
        ))
    })?;
    let mut hidden = HashSet::new();
    for row in rows {
        let (id, source, kind, empty_attached) = row?;
        let internal_source = source
            .as_deref()
            .and_then(|source| serde_json::from_str::<serde_json::Value>(source).ok())
            .is_some_and(|source| source.get("subagent").is_some_and(|value| !value.is_null()));
        if matches!(kind.as_deref(), Some("guardian_review" | "subagent")) || internal_source || empty_attached {
            hidden.insert(id);
        }
    }
    Ok(hidden)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_markers_hide_reviews_and_spawned_agents_without_hiding_user_chats_or_forks() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE threads(id TEXT,source TEXT,thread_source TEXT);
            INSERT INTO threads VALUES('kind','vscode','guardian_review'),
                ('legacy','{\"subagent\":{\"other\":\"guardian\"}}','subagent'),
                ('spawn-kind','vscode','subagent'),
                ('spawn-source','{\"subagent\":{\"thread_spawn\":{\"parent_thread_id\":\"parent\",\"agent_path\":null}}}',NULL),
                ('ordinary','vscode','user'),('fork','vscode','agent_forked_thread'),
                ('Guardian review','{broken','user');",
        )
        .unwrap();
        assert_eq!(
            internal_reviews(&db).unwrap(),
            HashSet::from(["kind".into(), "legacy".into(), "spawn-kind".into(), "spawn-source".into()])
        );
    }
    #[test]
    fn old_schemas_keep_chats_and_still_use_available_markers() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE threads(id TEXT); INSERT INTO threads VALUES('ordinary');")
            .unwrap();
        assert!(internal_reviews(&db).unwrap().is_empty());
        db.execute_batch(
            "ALTER TABLE threads ADD COLUMN source TEXT;
            UPDATE threads SET source='{\"subagent\":{\"other\":\"guardian\"}}';",
        )
        .unwrap();
        assert!(internal_reviews(&db).unwrap().contains("ordinary"));
    }
    #[test]
    fn hides_only_empty_attached_shells() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE threads(id TEXT, source TEXT, thread_source TEXT, attached_thread_parent_id TEXT, has_user_event INTEGER, tokens_used INTEGER, title TEXT);
            INSERT INTO threads VALUES ('shell','vscode','user','parent',0,0,''),
                ('real','vscode','user','parent',1,0,''),
                ('used','vscode','user','parent',0,10,''),
                ('titled','vscode','user','parent',0,0,'Named'),
                ('ordinary','vscode','user',NULL,0,0,'');").unwrap();
        assert_eq!(internal_reviews(&db).unwrap(), HashSet::from(["shell".into()]));
    }
}
