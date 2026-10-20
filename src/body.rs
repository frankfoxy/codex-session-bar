//! On-demand, bounded-memory cache of visible chat text. Never runs while Body is off.
use crate::{Paths, Stop};
use anyhow::Result;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde_json::Value;
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Seek, SeekFrom},
    os::windows::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, UNIX_EPOCH},
};

const BATCH: u64 = 8 * 1024 * 1024;
const MAX_MESSAGE: usize = 4 * 1024 * 1024;

#[derive(Clone, Default)]
pub struct Status {
    pub generation: u64,
    pub complete: bool,
    pub indexed: usize,
    pub available: usize,
    pub total: usize,
    pub bytes_read: u64,
    pub skipped: usize,
    pub hits: HashSet<String>,
    pub error: String,
}
struct Request {
    generation: u64,
    query: String,
    exact: bool,
    ids: HashSet<String>,
}
pub struct Service {
    sender: Option<mpsc::Sender<Request>>,
    generation: Arc<AtomicU64>,
    status: Arc<Mutex<Status>>,
    interrupt: Arc<Mutex<Vec<rusqlite::InterruptHandle>>>,
    handle: Option<thread::JoinHandle<()>>,
}
impl Service {
    pub fn new(paths: Paths, stop: Stop) -> Self {
        let (sender, receiver) = mpsc::channel::<Request>();
        let generation = Arc::new(AtomicU64::new(0));
        let status = Arc::new(Mutex::new(Status::default()));
        let interrupt = Arc::new(Mutex::new(Vec::new()));
        let sql_interrupt = interrupt.clone();
        let (version, output) = (generation.clone(), status.clone());
        let handle = thread::spawn(move || {
            unsafe {
                use windows::Win32::System::Threading::*;
                let _ = SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
            }
            let mut index: Option<Index> = None;
            while !stop.load(Ordering::Relaxed) {
                let mut request = match receiver.recv_timeout(Duration::from_millis(100)) {
                    Ok(v) => v,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(_) => break,
                };
                // Only the newest query matters. Typing never queues a full scan per key.
                while let Ok(newer) = receiver.recv_timeout(Duration::from_millis(200)) {
                    request = newer;
                }
                let control = Control {
                    generation: &version,
                    expected: request.generation,
                    stop: &stop,
                };
                if control.cancelled() {
                    continue;
                }
                let mut state = Status {
                    generation: request.generation,
                    total: request.ids.len(),
                    ..Status::default()
                };
                let result = (|| -> Result<()> {
                    if index.is_none() {
                        let opened = Index::open(&paths.runtime.join("body-search.sqlite"))?;
                        *sql_interrupt.lock().unwrap() = vec![opened.db.get_interrupt_handle()];
                        index = Some(opened);
                    }
                    let index = index.as_mut().unwrap();
                    let history = History::open(&paths)?;
                    let mut interrupts = vec![index.db.get_interrupt_handle()];
                    if let Some(history) = &history {
                        interrupts.push(history.db.get_interrupt_handle());
                    }
                    *sql_interrupt.lock().unwrap() = interrupts;
                    let mut history_ids = HashSet::new();
                    if let Some(history) = &history {
                        for id in &request.ids {
                            control.check()?;
                            if history.has_messages(id)? {
                                history_ids.insert(id.clone());
                            }
                        }
                    }
                    let mut sources = sources(&paths, &request.ids)?;
                    sources.retain(|(id, _)| !history_ids.contains(id));
                    // Small files finish first; a single giant tool log cannot block all chats.
                    sources
                        .sort_by_key(|(_, path)| fs::metadata(path).map(|m| m.len()).unwrap_or(0));
                    state.available = sources.len() + history_ids.len();
                    if let Some(history) = &history {
                        for id in &history_ids {
                            control.check()?;
                            index.update_history(id, history, &control, &mut state, &output)?;
                            state.indexed += 1;
                            publish(&output, &state, &control);
                        }
                    }
                    for (id, path) in sources {
                        control.check()?;
                        index.update(&id, &path, &control, &mut state, &output)?;
                        state.indexed += 1;
                        publish(&output, &state, &control);
                    }
                    state.hits = index.search(&request.query, request.exact, &control)?;
                    state.hits.retain(|id| request.ids.contains(id));
                    index.db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
                    Ok(())
                })();
                if control.cancelled() {
                    continue;
                }
                state.complete = true;
                if let Err(e) = result {
                    state.error = format!("{e:#}");
                }
                publish(&output, &state, &control);
            }
        });
        Self {
            sender: Some(sender),
            generation,
            status,
            interrupt,
            handle: Some(handle),
        }
    }
    pub fn submit(&self, query: &str, exact: bool, ids: impl Iterator<Item = String>) -> u64 {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        for handle in self.interrupt.lock().unwrap().iter() {
            handle.interrupt();
        }
        let request = Request {
            generation,
            query: query.into(),
            exact,
            ids: ids.collect(),
        };
        *self.status.lock().unwrap() = Status {
            generation,
            total: request.ids.len(),
            ..Status::default()
        };
        if let Some(sender) = &self.sender {
            let _ = sender.send(request);
        }
        generation
    }
    pub fn cancel(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        for handle in self.interrupt.lock().unwrap().iter() {
            handle.interrupt();
        }
    }
    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        self.cancel();
        self.sender.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
struct Control<'a> {
    generation: &'a AtomicU64,
    expected: u64,
    stop: &'a Stop,
}
impl Control<'_> {
    fn cancelled(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
            || self.generation.load(Ordering::Relaxed) != self.expected
    }
    fn check(&self) -> Result<()> {
        anyhow::ensure!(!self.cancelled(), "Body search cancelled");
        Ok(())
    }
}
fn publish(output: &Mutex<Status>, state: &Status, control: &Control) {
    if !control.cancelled() {
        *output.lock().unwrap() = state.clone();
    }
}
fn sources(paths: &Paths, ids: &HashSet<String>) -> Result<Vec<(String, PathBuf)>> {
    let db = Connection::open_with_flags(
        paths.home.join("state_5.sqlite"),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.busy_timeout(Duration::from_millis(500))?;
    let mut query =
        db.prepare("SELECT id,rollout_path FROM threads WHERE archived=0 AND agent_path IS NULL")?;
    let rows = query.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    let mut result = Vec::new();
    for row in rows {
        let (id, path) = row?;
        if ids.contains(&id) {
            if let Some(path) = path.map(PathBuf::from).filter(|p| p.is_file()) {
                result.push((id, path));
            }
        }
    }
    Ok(result)
}

struct Index {
    db: Connection,
}
struct History {
    db: Connection,
    identity: u64,
}
impl History {
    fn open(paths: &Paths) -> Result<Option<Self>> {
        let path = paths.home.join("thread_history_1.sqlite");
        if !path.is_file() {
            return Ok(None);
        }
        let db = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        db.busy_timeout(Duration::from_millis(500))?;
        db.execute_batch(
            "PRAGMA query_only=ON; PRAGMA temp_store=MEMORY; PRAGMA cache_size=-1024",
        )?;
        if db
            .prepare(
                "SELECT thread_id,item_id,updated_at_ordinal,item_json FROM thread_items LIMIT 0",
            )
            .is_err()
        {
            return Ok(None);
        }
        Ok(Some(Self {
            db,
            identity: fs::metadata(path)?.creation_time(),
        }))
    }
    fn has_messages(&self, id: &str) -> Result<bool> {
        // Separate predicates let SQLite use its existing partial user-message index.
        for kind in ["userMessage", "agentMessage"] {
            if self.db.query_row("SELECT EXISTS(SELECT 1 FROM thread_items WHERE thread_id=? AND item_type=? LIMIT 1)",params![id,kind],|r|r.get::<_,bool>(0))? { return Ok(true); }
        }
        Ok(false)
    }
}
impl Index {
    fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.busy_timeout(Duration::from_millis(500))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA temp_store=MEMORY; PRAGMA cache_size=-2048; PRAGMA max_page_count=65536; PRAGMA wal_autocheckpoint=256; PRAGMA journal_size_limit=4194304;
            CREATE TABLE IF NOT EXISTS sources(id TEXT PRIMARY KEY,path TEXT NOT NULL,identity INTEGER NOT NULL,size INTEGER NOT NULL,modified INTEGER NOT NULL,offset INTEGER NOT NULL,skipped INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS history_sources(id TEXT PRIMARY KEY,identity INTEGER NOT NULL,cursor INTEGER NOT NULL,skipped INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS history_items(chat TEXT NOT NULL,item TEXT NOT NULL,first_row INTEGER NOT NULL,last_row INTEGER NOT NULL,PRIMARY KEY(chat,item));
            CREATE VIRTUAL TABLE IF NOT EXISTS messages USING fts5(chat UNINDEXED,body,tokenize='trigram',detail=none);")?;
        Ok(Self { db })
    }
    fn update_history(
        &mut self,
        id: &str,
        history: &History,
        control: &Control,
        state: &mut Status,
        output: &Mutex<Status>,
    ) -> Result<()> {
        let maximum: i64 = history.db.query_row(
            "SELECT coalesce(max(updated_at_ordinal),-1) FROM thread_items WHERE thread_id=?",
            [id],
            |r| r.get(0),
        )?;
        let old: Option<(u64, i64, usize)> = self
            .db
            .query_row(
                "SELECT identity,cursor,skipped FROM history_sources WHERE id=?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let (mut cursor, mut skipped) = match old {
            Some((identity, cursor, skipped))
                if identity == history.identity && cursor <= maximum =>
            {
                (cursor, skipped)
            }
            _ => {
                let had_raw: bool = self.db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sources WHERE id=?)",
                    [id],
                    |r| r.get(0),
                )?;
                if old.is_some() || had_raw {
                    let tx = self.db.transaction()?;
                    tx.execute("DELETE FROM messages WHERE chat=?", [id])?;
                    tx.execute("DELETE FROM history_items WHERE chat=?", [id])?;
                    tx.execute("DELETE FROM history_sources WHERE id=?", [id])?;
                    tx.execute("DELETE FROM sources WHERE id=?", [id])?;
                    tx.commit()?;
                }
                (-1, 0)
            }
        };
        while cursor < maximum {
            control.check()?;
            let mut query=history.db.prepare("SELECT item_id,updated_at_ordinal,CASE WHEN length(CAST(item_json AS BLOB))<=4194304 THEN item_json ELSE NULL END FROM thread_items WHERE thread_id=? AND updated_at_ordinal>? AND updated_at_ordinal<=? AND item_type IN ('userMessage','agentMessage') ORDER BY updated_at_ordinal")?;
            let items = query.query_map(params![id, cursor, maximum], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })?;
            let tx = self.db.transaction()?;
            let mut count = 0;
            for item in items {
                control.check()?;
                let (item, ordinal, json) = item?;
                // Never split equal ordinals across commits: older projections
                // can assign one ordinal to several messages.
                if count >= 128 && ordinal > cursor {
                    break;
                }
                let old: Option<(i64, i64)> = tx
                    .query_row(
                        "SELECT first_row,last_row FROM history_items WHERE chat=? AND item=?",
                        params![id, item],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                if let Some((first, last)) = old {
                    tx.execute(
                        "DELETE FROM messages WHERE rowid BETWEEN ? AND ?",
                        params![first, last],
                    )?;
                }
                tx.execute(
                    "DELETE FROM history_items WHERE chat=? AND item=?",
                    params![id, item],
                )?;
                if let Some(json) = json {
                    state.bytes_read += json.len() as u64;
                    if let Some(text) = history_text(&json) {
                        let text = text.to_lowercase();
                        let (mut at, mut first, mut last) = (0, 0, 0);
                        while at < text.len() {
                            control.check()?;
                            let mut end = (at + 16 * 1024).min(text.len());
                            while !text.is_char_boundary(end) {
                                end -= 1;
                            }
                            tx.execute(
                                "INSERT INTO messages(chat,body) VALUES(?,?)",
                                params![id, &text[at..end]],
                            )?;
                            last = tx.last_insert_rowid();
                            if first == 0 {
                                first = last;
                            }
                            if end == text.len() {
                                break;
                            }
                            at = end - 1024;
                            while !text.is_char_boundary(at) {
                                at += 1;
                            }
                        }
                        if first > 0 {
                            tx.execute(
                                "INSERT INTO history_items VALUES(?,?,?,?)",
                                params![id, item, first, last],
                            )?;
                        }
                    }
                } else {
                    skipped += 1;
                }
                cursor = ordinal;
                count += 1;
            }
            if count == 0 {
                cursor = maximum;
            }
            tx.execute(
                "INSERT OR REPLACE INTO history_sources VALUES(?,?,?,?)",
                params![id, history.identity, cursor, skipped],
            )?;
            tx.commit()?;
            publish(output, state, control);
            thread::sleep(Duration::from_millis(10));
        }
        state.skipped += skipped;
        Ok(())
    }
    fn update(
        &mut self,
        id: &str,
        path: &Path,
        control: &Control,
        state: &mut Status,
        output: &Mutex<Status>,
    ) -> Result<()> {
        let meta = fs::metadata(path)?;
        let size = meta.len();
        let identity = meta.creation_time();
        let modified = meta.modified()?.duration_since(UNIX_EPOCH)?.as_nanos() as i64;
        let path_text = path.to_string_lossy();
        let saved: Option<(String, u64, u64, i64, u64, usize)> = self
            .db
            .query_row(
                "SELECT path,identity,size,modified,offset,skipped FROM sources WHERE id=?",
                [id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .optional()?;
        let (mut offset, mut skipped) = match saved {
            Some((old_path, old_identity, old_size, old_modified, old_offset, old_skipped))
                if old_path == path_text
                    && old_identity == identity
                    && size >= old_size
                    && size >= old_offset
                    && !(size == old_size && modified != old_modified) =>
            {
                (old_offset, old_skipped)
            }
            _ => {
                let tx = self.db.transaction()?;
                tx.execute("DELETE FROM messages WHERE chat=?", [id])?;
                tx.execute("DELETE FROM history_items WHERE chat=?", [id])?;
                tx.execute("DELETE FROM history_sources WHERE id=?", [id])?;
                tx.execute("DELETE FROM sources WHERE id=?", [id])?;
                tx.commit()?;
                (0, 0)
            }
        };
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(0x08000000 /* FILE_FLAG_SEQUENTIAL_SCAN */)
            .open(path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut reader = BufReader::with_capacity(64 * 1024, file);
        let mut throttled = 0u64;
        while offset < size {
            control.check()?;
            let start = offset;
            let tx = self.db.transaction()?;
            let mut incomplete = false;
            loop {
                let (end, line, oversized) =
                    next_message_line(&mut reader, offset, size, control, state, &mut throttled)?;
                if end == offset {
                    incomplete = true;
                    break;
                }
                if oversized {
                    skipped += 1;
                }
                if let Some(line) = line {
                    if let Some(text) = visible_text(&line) {
                        let text = text.to_lowercase();
                        let mut at = 0;
                        while at < text.len() {
                            control.check()?;
                            let mut end = (at + 16 * 1024).min(text.len());
                            while !text.is_char_boundary(end) {
                                end -= 1;
                            }
                            tx.execute(
                                "INSERT INTO messages(chat,body) VALUES(?,?)",
                                params![id, &text[at..end]],
                            )?;
                            if end == text.len() {
                                break;
                            }
                            at = end - 1024;
                            while !text.is_char_boundary(at) {
                                at += 1;
                            }
                        }
                    }
                }
                offset = end;
                if offset >= size || offset - start >= BATCH {
                    break;
                }
            }
            tx.execute(
                "INSERT OR REPLACE INTO sources VALUES(?,?,?,?,?,?,?)",
                params![
                    id,
                    path_text.as_ref(),
                    identity,
                    size,
                    modified,
                    offset,
                    skipped
                ],
            )?;
            tx.commit()?;
            publish(output, state, control);
            if incomplete {
                break;
            }
        }
        state.skipped += skipped;
        Ok(())
    }
    fn search(&self, query: &str, exact: bool, control: &Control) -> Result<HashSet<String>> {
        let query = query.trim().to_lowercase();
        let terms: Vec<&str> = if exact {
            vec![&query]
        } else {
            query.split_whitespace().collect()
        };
        let mut hits: Option<HashSet<String>> = None;
        for term in terms {
            control.check()?;
            // LIKE uses the trigram index for >=3 characters. Short Chinese words
            // and literal wildcard symbols still work via the small text cache.
            let literal = term.contains(['%', '_']);
            let mut stmt = self.db.prepare(if literal {
                "SELECT DISTINCT chat FROM messages WHERE instr(body,?)>0"
            } else {
                "SELECT DISTINCT chat FROM messages WHERE body LIKE ?"
            })?;
            let pattern = if literal {
                term.to_string()
            } else {
                format!("%{term}%")
            };
            let found = stmt
                .query_map([pattern], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<HashSet<_>>>()?;
            hits = Some(match hits {
                None => found,
                Some(old) => old.intersection(&found).cloned().collect(),
            });
            if hits.as_ref().is_some_and(HashSet::is_empty) {
                break;
            }
        }
        Ok(hits.unwrap_or_default())
    }
}

fn candidate(prefix: &[u8]) -> Option<bool> {
    let prefix: String = String::from_utf8_lossy(prefix)
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    if [
        "event_msg",
        "session_meta",
        "turn_context",
        "token_usage_record",
        "reasoning",
        "function_call",
        "function_call_output",
        "custom_tool_call",
        "custom_tool_call_output",
    ]
    .iter()
    .any(|kind| prefix.contains(&format!("\"type\":\"{kind}\"")))
        || ["developer", "system", "tool"]
            .iter()
            .any(|role| prefix.contains(&format!("\"role\":\"{role}\"")))
    {
        return Some(false);
    }
    if prefix.contains("\"type\":\"response_item\"")
        && prefix.contains("\"type\":\"message\"")
        && (prefix.contains("\"role\":\"user\"") || prefix.contains("\"role\":\"assistant\""))
    {
        Some(true)
    } else {
        None
    } // JSON field order is not fixed; preserve an unknown message prefix.
}
fn next_message_line(
    reader: &mut BufReader<File>,
    start: u64,
    limit: u64,
    control: &Control,
    state: &mut Status,
    throttled: &mut u64,
) -> Result<(u64, Option<Vec<u8>>, bool)> {
    let mut position = start;
    let mut bytes = Vec::new();
    let mut selected: Option<bool> = None;
    let mut oversized = false;
    while position < limit {
        control.check()?;
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok((start, None, false));
        }
        let available = &available[..available.len().min((limit - position) as usize)];
        let newline = available.iter().position(|&b| b == b'\n');
        let count = newline.map(|n| n + 1).unwrap_or(available.len());
        if selected != Some(false) && !oversized {
            if bytes.len() + count <= MAX_MESSAGE {
                bytes.extend_from_slice(&available[..count]);
            } else {
                bytes.clear();
                oversized = true;
            }
            if selected.is_none() && (bytes.len() >= 2048 || newline.is_some()) {
                selected = candidate(&bytes[..bytes.len().min(2048)]);
                if selected == Some(false) {
                    bytes.clear();
                }
            }
        }
        reader.consume(count);
        position += count as u64;
        state.bytes_read += count as u64;
        *throttled += count as u64;
        if *throttled >= BATCH {
            thread::sleep(Duration::from_millis(20));
            *throttled = 0;
        }
        if newline.is_some() {
            return Ok((
                position,
                (selected != Some(false) && !oversized).then_some(bytes),
                oversized && selected != Some(false),
            ));
        }
    }
    // Do not consume an incomplete last event durably: retry it after append.
    Ok((start, None, false))
}
fn visible_text(bytes: &[u8]) -> Option<String> {
    let event: Value = serde_json::from_slice(bytes).ok()?;
    let payload = &event["payload"];
    if event["type"] != "response_item"
        || payload["type"] != "message"
        || !matches!(payload["role"].as_str(), Some("user" | "assistant"))
        || payload["channel"] == "analysis"
    {
        return None;
    }
    let text = payload["content"]
        .as_array()?
        .iter()
        .filter(|part| matches!(part["type"].as_str(), Some("input_text" | "output_text")))
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty()
        || text.starts_with("<environment_context>")
        || text.starts_with("# AGENTS.md instructions")
    {
        None
    } else {
        Some(text)
    }
}
fn history_text(json: &str) -> Option<String> {
    let item: Value = serde_json::from_str(json).ok()?;
    let text = match item["type"].as_str()? {
        "userMessage" => item["content"]
            .as_array()?
            .iter()
            .filter(|p| p["type"] == "text")
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        "agentMessage" if item["phase"] != "analysis" => item["text"].as_str()?.to_string(),
        _ => return None,
    };
    if text.is_empty()
        || text.starts_with("<environment_context>")
        || text.starts_with("# AGENTS.md instructions")
    {
        None
    } else {
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, sync::atomic::AtomicBool};
    struct Fixture {
        file: PathBuf,
        index: Index,
        stop: Stop,
        generation: AtomicU64,
    }
    impl Fixture {
        fn new(name: &str) -> Self {
            let paths = Paths::discover().unwrap();
            let dir = paths
                .runtime
                .join(format!("rust-body-fixture-{}-{name}", std::process::id()));
            assert!(dir.starts_with(&paths.runtime));
            fs::create_dir_all(&dir).unwrap();
            Self {
                index: Index::open(&dir.join("index.sqlite")).unwrap(),
                file: dir.join("messages.jsonl"),
                stop: Arc::new(AtomicBool::new(false)),
                generation: AtomicU64::new(1),
            }
        }
        fn update(&mut self) -> Status {
            let control = Control {
                generation: &self.generation,
                expected: 1,
                stop: &self.stop,
            };
            let mut state = Status::default();
            self.index
                .update("chat", &self.file, &control, &mut state, &Mutex::default())
                .unwrap();
            state
        }
        fn has(&self, query: &str, exact: bool) -> bool {
            self.index
                .search(
                    query,
                    exact,
                    &Control {
                        generation: &self.generation,
                        expected: 1,
                        stop: &self.stop,
                    },
                )
                .unwrap()
                .contains("chat")
        }
    }
    fn line(role: &str, text: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":role,"content":[{"type":"input_text","text":text}]}})
        )
    }
    #[test]
    fn visible_only_chinese_literal_terms_and_incremental_append() {
        let mut f = Fixture::new("append");
        fs::write(
            &f.file,
            [
                line("user", "相机 100% camera_buffer"),
                line("assistant", "memory leak detected"),
                line("developer", "hidden-secret"),
            ]
            .concat(),
        )
        .unwrap();
        let cold = f.update();
        assert!(cold.bytes_read > 0);
        assert!(f.has("相机 memory", false));
        assert!(f.has("memory leak", true));
        assert!(!f.has("memory detected", true));
        assert!(f.has("100%", false));
        assert!(f.has("camera_buffer", false));
        assert!(!f.has("hidden-secret", false));
        assert_eq!(f.update().bytes_read, 0);
        OpenOptions::new()
            .append(true)
            .open(&f.file)
            .unwrap()
            .write_all(line("assistant", "incremental marker").as_bytes())
            .unwrap();
        let warm = f.update();
        assert!(warm.bytes_read > 0 && warm.bytes_read < cold.bytes_read);
        assert!(f.has("incremental marker", true));
        let count: usize = f
            .index
            .db
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3);
    }
    #[test]
    fn incomplete_lines_truncation_and_cancellation() {
        let mut f = Fixture::new("recovery");
        let event = line("user", "unfinished marker");
        fs::write(&f.file, &event.as_bytes()[..event.len() / 2]).unwrap();
        f.update();
        assert!(!f.has("unfinished", false));
        OpenOptions::new()
            .append(true)
            .open(&f.file)
            .unwrap()
            .write_all(&event.as_bytes()[event.len() / 2..])
            .unwrap();
        f.update();
        assert!(f.has("unfinished", false));
        fs::write(&f.file, line("user", "new")).unwrap();
        f.update();
        assert!(!f.has("unfinished", false));
        assert!(f.has("new", false));
        f.generation.store(2, Ordering::Relaxed);
        let result = f.index.search(
            "new",
            false,
            &Control {
                generation: &f.generation,
                expected: 1,
                stop: &f.stop,
            },
        );
        assert!(result.is_err());
    }
    #[test]
    fn large_tool_outputs_are_skipped_and_cross_chunk_phrase_survives() {
        let mut f = Fixture::new("bounds");
        let mut text = "x".repeat(16380);
        text.push_str("cross boundary phrase");
        let mut file = File::create(&f.file).unwrap();
        file.write_all(format!("{{\"type\":\"response_item\",\"payload\":{{\"type\":\"function_call_output\",\"output\":\"{}\"}}}}\n", "hidden-tool".repeat(600000)).as_bytes()).unwrap();
        file.write_all(line("assistant", &text).as_bytes()).unwrap();
        drop(file);
        assert_eq!(f.update().skipped, 0);
        assert!(f.has("cross boundary phrase", true));
        assert!(!f.has("hidden-tool", false));
    }
    #[test]
    fn async_latest_query_wins_and_warm_query_reads_no_logs() {
        let f = Fixture::new("service");
        fs::write(&f.file, line("user", "memory 相机")).unwrap();
        let dir = f.file.parent().unwrap().to_path_buf();
        let db = Connection::open(dir.join("state_5.sqlite")).unwrap();
        db.execute_batch(
            "CREATE TABLE threads(id TEXT,rollout_path TEXT,archived INTEGER,agent_path TEXT)",
        )
        .unwrap();
        db.execute(
            "INSERT INTO threads VALUES('chat',?,0,NULL)",
            [f.file.to_string_lossy().as_ref()],
        )
        .unwrap();
        let paths = Paths {
            home: dir.clone(),
            runtime: dir,
            ..Paths::discover().unwrap()
        };
        let service = Service::new(paths, f.stop.clone());
        service.submit("old-query", false, ["chat".to_string()].into_iter());
        let latest = service.submit("memory", false, ["chat".to_string()].into_iter());
        let wait = |generation| {
            for _ in 0..1000 {
                let status = service.status();
                if status.generation == generation && status.complete {
                    return status;
                }
                thread::sleep(Duration::from_millis(10));
            }
            panic!("Background body search timed out");
        };
        let cold = wait(latest);
        assert!(cold.error.is_empty(), "{}", cold.error);
        assert!(cold.hits.contains("chat"));
        assert!(cold.bytes_read > 0);
        let warm = wait(service.submit("相机", false, ["chat".to_string()].into_iter()));
        assert!(warm.hits.contains("chat"));
        assert_eq!(warm.bytes_read, 0);
    }
    #[test]
    fn cached_history_batches_tied_ordinals_and_replaces_updated_items() {
        let mut f = Fixture::new("history");
        let paths = Paths {
            home: f.file.parent().unwrap().to_path_buf(),
            ..Paths::discover().unwrap()
        };
        let history_db = Connection::open(paths.home.join("thread_history_1.sqlite")).unwrap();
        history_db.execute_batch("CREATE TABLE thread_items(thread_id TEXT,item_id TEXT,item_type TEXT,item_json TEXT,updated_at_ordinal INTEGER); CREATE INDEX updated ON thread_items(thread_id,updated_at_ordinal)").unwrap();
        for i in 0..140 {
            let json=serde_json::json!({"type":"userMessage","content":[{"type":"text","text":format!("序号 {i}")}]}).to_string();
            history_db
                .execute(
                    "INSERT INTO thread_items VALUES('chat',?,'userMessage',?,0)",
                    params![format!("user-{i}"), json],
                )
                .unwrap();
        }
        let json=serde_json::json!({"type":"agentMessage","text":format!("{}history exclusive old", "x".repeat(17000)),"phase":"final"}).to_string();
        history_db
            .execute(
                "INSERT INTO thread_items VALUES('chat','agent','agentMessage',?,1)",
                [json],
            )
            .unwrap();
        history_db.execute("INSERT INTO thread_items VALUES('chat','tool','commandExecution','hidden-tool',100)",[]).unwrap();
        let history = History::open(&paths).unwrap().unwrap();
        assert!(history.has_messages("chat").unwrap());
        let mut cold = Status::default();
        f.index
            .update_history(
                "chat",
                &history,
                &Control {
                    generation: &f.generation,
                    expected: 1,
                    stop: &f.stop,
                },
                &mut cold,
                &Mutex::default(),
            )
            .unwrap();
        assert!(f.has("序号 139", true));
        assert!(f.has("history exclusive old", true));
        assert!(!f.has("hidden-tool", false));
        let mut warm = Status::default();
        f.index
            .update_history(
                "chat",
                &history,
                &Control {
                    generation: &f.generation,
                    expected: 1,
                    stop: &f.stop,
                },
                &mut warm,
                &Mutex::default(),
            )
            .unwrap();
        assert_eq!(warm.bytes_read, 0);
        let json =
            serde_json::json!({"type":"agentMessage","text":"replacement only","phase":"final"})
                .to_string();
        history_db
            .execute(
                "UPDATE thread_items SET item_json=?,updated_at_ordinal=101 WHERE item_id='agent'",
                [json],
            )
            .unwrap();
        let mut changed = Status::default();
        f.index
            .update_history(
                "chat",
                &history,
                &Control {
                    generation: &f.generation,
                    expected: 1,
                    stop: &f.stop,
                },
                &mut changed,
                &Mutex::default(),
            )
            .unwrap();
        assert!(changed.bytes_read > 0 && changed.bytes_read < cold.bytes_read);
        assert!(f.has("replacement only", true));
        assert!(!f.has("history exclusive old", true));
        assert!(f.has("序号 139", true));
    }
}
