//! Internal approval reviews are persisted threads, but not user chats.
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
    let mut query = db.prepare(&format!("SELECT id,{source},{kind} FROM threads"))?;
    let rows = query.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;
    let mut hidden = HashSet::new();
    for row in rows {
        let (id, source, kind) = row?;
        let guardian = source
            .as_deref()
            .and_then(|source| serde_json::from_str::<serde_json::Value>(source).ok())
            .is_some_and(|source| source["subagent"]["other"].as_str() == Some("guardian"));
        if kind.as_deref() == Some("guardian_review") || guardian {
            hidden.insert(id);
        }
    }
    Ok(hidden)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_markers_hide_reviews_without_hiding_user_names_or_forks() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE threads(id TEXT,source TEXT,thread_source TEXT);
            INSERT INTO threads VALUES('kind','vscode','guardian_review'),
                ('legacy','{\"subagent\":{\"other\":\"guardian\"}}','subagent'),
                ('ordinary','vscode','user'),('fork','vscode','agent_forked_thread'),
                ('Guardian review','{broken','user');",
        )
        .unwrap();
        assert_eq!(
            internal_reviews(&db).unwrap(),
            HashSet::from(["kind".into(), "legacy".into()])
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
}
