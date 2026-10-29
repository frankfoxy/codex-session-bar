//! Hover-only, bounded previews. No network, idle queries or dedicated timers.
use crate::Paths;
use anyhow::Result;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

const LIMIT: usize = 900;
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Preview {
    Text(String),
    Empty,
    Unavailable,
}
impl Preview {
    pub fn text(&self, zh: bool) -> String {
        match self {
            Self::Text(text) => format!("{}\n{text}", if zh { "最新回复" } else { "Latest reply" }),
            Self::Empty => if zh {
                "最新一轮暂无可显示的回复"
            } else {
                "No visible reply in the latest exchange yet"
            }
            .into(),
            Self::Unavailable => if zh {
                "最新回复暂不可用，稍后重试"
            } else {
                "Latest reply unavailable; retrying shortly"
            }
            .into(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Key {
    source: String,
    user: Option<(String, String, i64, i64)>,
    agents: Vec<(String, i64, i64)>,
}
#[derive(Default)]
pub struct Reader {
    cache: VecDeque<(String, Key, Preview)>,
    #[cfg(test)]
    body_reads: usize,
}
fn bound(history: &Connection, id: &str) -> i64 {
    history
        .query_row(
            "SELECT next_rollout_ordinal FROM thread_history_projection_state WHERE thread_id=?",
            [id],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .ok()
        .flatten()
        .unwrap_or(i64::MAX)
        .max(0)
}
impl Reader {
    pub fn read(&mut self, state: &Connection, history: &Connection, id: &str) -> Result<Preview> {
        let path = state
            .query_row(
                "SELECT rollout_path FROM threads WHERE id=? AND archived=0 AND agent_path IS NULL",
                [id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let mut source = id.to_string();
        if let Some(alias) = path.as_deref().and_then(|p| crate::prompts::alias(id, p)) {
            let projected=history.query_row("SELECT next_rollout_ordinal FROM thread_history_projection_state WHERE thread_id=?",[&alias],|r|r.get::<_,i64>(0)).optional().ok().flatten().is_some();
            let users=history.query_row("SELECT EXISTS(SELECT 1 FROM thread_items WHERE thread_id=? AND item_type='userMessage' LIMIT 1)",[&alias],|r|r.get::<_,bool>(0))?;
            if projected || users {
                source = alias;
            }
        }
        let limit = bound(history, &source);
        let user=history.query_row("SELECT item_id,turn_id,rollout_ordinal,updated_at_ordinal FROM thread_items WHERE thread_id=? AND item_type='userMessage' AND rollout_ordinal<? ORDER BY rollout_ordinal DESC LIMIT 1",params![source,limit],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,i64>(3)?))).optional()?;
        let mut agents = Vec::new();
        if let Some((_, turn, ordinal, _)) = &user {
            let mut q=history.prepare("SELECT item_id,rollout_ordinal,updated_at_ordinal FROM thread_items WHERE thread_id=? AND turn_id=? AND rollout_ordinal>? AND rollout_ordinal<? AND item_type='agentMessage' ORDER BY rollout_ordinal DESC LIMIT 16")?;
            agents = q
                .query_map(params![source, turn, ordinal, limit], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
        }
        let key = Key {
            source: source.clone(),
            user,
            agents,
        };
        if let Some(index) = self
            .cache
            .iter()
            .position(|(visible, cached, _)| visible == id && cached == &key)
        {
            let entry = self.cache.remove(index).unwrap();
            let value = entry.2.clone();
            self.cache.push_front(entry);
            return Ok(value);
        }
        let mut result = Preview::Empty;
        if let Some((_, turn, _, _)) = &key.user {
            // JSON extraction happens only after a revision changes. Even a long
            // final response returns at most 901 characters to the Rust process.
            let mut q=history.prepare("SELECT CASE WHEN length(CAST(item_json AS BLOB))<=4194304 AND json_valid(item_json) THEN json_extract(item_json,'$.phase') END,CASE WHEN length(CAST(item_json AS BLOB))<=4194304 AND json_valid(item_json) THEN CASE WHEN json_extract(item_json,'$.type')='agentMessage' AND coalesce(json_extract(item_json,'$.phase'),'')!='analysis' THEN substr(json_extract(item_json,'$.text'),1,901) END END FROM thread_items WHERE thread_id=? AND turn_id=? AND item_id=?")?;
            for (item, _, _) in &key.agents {
                #[cfg(test)]
                {
                    self.body_reads += 1;
                }
                let (phase, text) = q.query_row(params![source, turn, item], |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, Option<String>>(1)?,
                    ))
                })?;
                if phase.as_deref() == Some("analysis") {
                    continue;
                }
                match text {
                    Some(text) if !text.trim().is_empty() => {
                        result = Preview::Text(excerpt(&text));
                        break;
                    }
                    // Do not substitute an older reply when the newest body is absent.
                    None => {
                        result = Preview::Unavailable;
                        break;
                    }
                    _ => {}
                }
            }
        }
        self.cache.retain(|(visible, _, _)| visible != id);
        self.cache.push_front((id.into(), key, result.clone()));
        self.cache.truncate(8);
        Ok(result)
    }
}
fn excerpt(raw: &str) -> String {
    let mut output = String::new();
    let mut lines = 0;
    let mut truncated = false;
    for (index, c) in raw
        .trim()
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .enumerate()
    {
        if index >= LIMIT || (c == '\n' && lines >= 13) {
            truncated = true;
            break;
        }
        if c == '\n' {
            lines += 1;
        }
        output.push(c);
    }
    if truncated {
        output.push('…');
    }
    output
}
fn open(paths: &Paths, name: &str) -> Result<Connection> {
    let db = Connection::open_with_flags(
        paths.home.join(name),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.busy_timeout(Duration::from_millis(200))?;
    db.execute_batch("PRAGMA query_only=ON;PRAGMA temp_store=MEMORY;PRAGMA cache_size=-512")?;
    Ok(db)
}
pub fn inspect(paths:&Paths,id:&str)->Result<()> {
    let state=open(paths,"state_5.sqlite")?;let history=open(paths,"thread_history_1.sqlite")?;
    let mut reader=Reader::default();let start=Instant::now();let result=reader.read(&state,&history,id)?;let cold_ms=start.elapsed().as_secs_f64()*1000.;
    let start=Instant::now();let _=reader.read(&state,&history,id)?;let warm_ms=start.elapsed().as_secs_f64()*1000.;
    let source=reader.cache.front().map(|(_,key,_)|key.source.as_str());
    println!("{}",serde_json::json!({"id":id,"history_source":source,"preview":result.text(crate::locale::chinese()),"cold_ms":cold_ms,"warm_ms":warm_ms,"mutations":false}));Ok(())
}
pub struct Service {
    tx: Option<mpsc::SyncSender<String>>,
    handle: Option<thread::JoinHandle<()>>,
    values: Arc<Mutex<HashMap<String, Preview>>>,
    revision: Arc<AtomicU64>,
    last: Option<(String, u64, Instant)>,
}
impl Service {
    pub fn new(paths: Paths) -> Self {
        let (tx, rx) = mpsc::sync_channel::<String>(1);
        let values = Arc::new(Mutex::new(HashMap::new()));
        let revision = Arc::new(AtomicU64::new(0));
        let (output, changed) = (values.clone(), revision.clone());
        let handle = thread::Builder::new()
            .name("chat-reply-preview".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                let mut reader = Reader::default();
                let mut databases = None;
                loop {
                    let mut id = match rx.recv_timeout(Duration::from_secs(30)) {
                        Ok(id) => id,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            databases = None;
                            continue;
                        }
                        Err(_) => break,
                    };
                    for latest in rx.try_iter() {
                        id = latest;
                    }
                    let result = (|| -> Result<Preview> {
                        if databases.is_none() {
                            databases = Some((
                                open(&paths, "state_5.sqlite")?,
                                open(&paths, "thread_history_1.sqlite")?,
                            ));
                        }
                        let (state, history) = databases.as_ref().unwrap();
                        reader.read(state, history, &id)
                    })()
                    .unwrap_or(Preview::Unavailable);
                    let mut published = output.lock().unwrap();
                    if published.get(&id) != Some(&result) {
                        if published.len() >= 8 && !published.contains_key(&id) {
                            published.clear();
                        }
                        published.insert(id, result);
                        changed.fetch_add(1, Ordering::Release);
                    }
                }
            })
            .ok();
        Self {
            tx: Some(tx),
            handle,
            values,
            revision,
            last: None,
        }
    }
    pub fn request(&mut self, id: &str, generation: u64) {
        if self.last.as_ref().is_some_and(|(last, event, at)| {
            last == id
                && (at.elapsed() < Duration::from_secs(1)
                    || (*event == generation && at.elapsed() < Duration::from_secs(5)))
        }) {
            return;
        }
        if self
            .tx
            .as_ref()
            .is_some_and(|tx| tx.try_send(id.into()).is_ok())
        {
            self.last = Some((id.into(), generation, Instant::now()));
        }
    }
    pub fn peek(&self, id: &str) -> Option<Preview> {
        if self.handle.is_none() && self.tx.is_some() {
            Some(Preview::Unavailable)
        } else {
            self.values.lock().unwrap().get(id).cloned()
        }
    }
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }
    pub fn inactive(&mut self) {
        self.last = None;
    }
    // Native --self-test uses an in-memory reply without starting a worker.
    pub fn fixture(id: &str, text: &str) -> Self {
        Self {
            tx: None,
            handle: None,
            values: Arc::new(Mutex::new(HashMap::from([(
                id.into(),
                Preview::Text(text.into()),
            )]))),
            revision: Arc::new(AtomicU64::new(1)),
            last: None,
        }
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const ROOT: &str = "11111111-1111-4111-8111-111111111111";
    const ALIAS: &str = "22222222-2222-4222-8222-222222222222";
    fn schema(state: &Connection, history: &Connection) {
        state
            .execute_batch(
                "CREATE TABLE threads(id TEXT,rollout_path TEXT,archived INTEGER,agent_path TEXT);",
            )
            .unwrap();
        state
            .execute("INSERT INTO threads VALUES(?,NULL,0,NULL)", [ROOT])
            .unwrap();
        history.execute_batch("CREATE TABLE thread_items(thread_id TEXT,turn_id TEXT,item_id TEXT,item_type TEXT,rollout_ordinal INTEGER,updated_at_ordinal INTEGER,item_json TEXT,PRIMARY KEY(thread_id,turn_id,item_id));
            CREATE INDEX user_head ON thread_items(thread_id,rollout_ordinal) WHERE item_type='userMessage';
            CREATE INDEX turn_page ON thread_items(thread_id,turn_id,rollout_ordinal);
            CREATE TABLE thread_history_projection_state(thread_id TEXT PRIMARY KEY,next_rollout_ordinal INTEGER);").unwrap();
    }
    fn insert(
        db: &Connection,
        source: &str,
        turn: &str,
        id: &str,
        kind: &str,
        ordinal: i64,
        phase: &str,
        text: &str,
    ) {
        let item = serde_json::json!({"type":kind,"phase":phase,"text":text});
        db.execute(
            "INSERT OR REPLACE INTO thread_items VALUES(?,?,?,?,?,?,?)",
            params![source, turn, id, kind, ordinal, ordinal, item.to_string()],
        )
        .unwrap();
    }
    #[test]
    fn latest_visible_reply_is_bound_to_latest_user_and_warm_reads_skip_bodies() {
        let (s, h) = (
            Connection::open_in_memory().unwrap(),
            Connection::open_in_memory().unwrap(),
        );
        schema(&s, &h);
        insert(
            &h,
            ROOT,
            "one",
            "u",
            "userMessage",
            1,
            "",
            "Prompt should never be shown",
        );
        insert(
            &h,
            ROOT,
            "one",
            "a",
            "agentMessage",
            2,
            "commentary",
            "Current visible response",
        );
        insert(
            &h,
            ROOT,
            "one",
            "hidden",
            "agentMessage",
            3,
            "analysis",
            "Private reasoning",
        );
        insert(
            &h,
            ROOT,
            "one",
            "tool",
            "commandExecution",
            4,
            "",
            "Tool output",
        );
        let mut reader = Reader::default();
        assert_eq!(
            reader.read(&s, &h, ROOT).unwrap(),
            Preview::Text("Current visible response".into())
        );
        let reads = reader.body_reads;
        reader.read(&s, &h, ROOT).unwrap();
        assert_eq!(reader.body_reads, reads);
        h.execute(
            "UPDATE thread_items SET updated_at_ordinal=10,item_json=? WHERE item_id='a'",
            [
                serde_json::json!({"type":"agentMessage","phase":"final","text":"Edited response"})
                    .to_string(),
            ],
        )
        .unwrap();
        assert_eq!(
            reader.read(&s, &h, ROOT).unwrap(),
            Preview::Text("Edited response".into())
        );
        insert(
            &h,
            ROOT,
            "two",
            "new-u",
            "userMessage",
            11,
            "",
            "A newer prompt",
        );
        assert_eq!(
            reader.read(&s, &h, ROOT).unwrap(),
            Preview::Empty,
            "Never show the previous turn's answer for a new prompt"
        );
        insert(
            &h,
            ROOT,
            "one",
            "wrong-turn",
            "agentMessage",
            12,
            "final",
            "Late old turn output",
        );
        assert_eq!(reader.read(&s, &h, ROOT).unwrap(), Preview::Empty);
        insert(
            &h,
            ROOT,
            "two",
            "new-a",
            "agentMessage",
            13,
            "final",
            "Latest response",
        );
        assert_eq!(
            reader.read(&s, &h, ROOT).unwrap(),
            Preview::Text("Latest response".into())
        );
    }
    #[test]
    fn resumed_sources_and_projection_restarts_do_not_leak_old_replies() {
        let (s, h) = (
            Connection::open_in_memory().unwrap(),
            Connection::open_in_memory().unwrap(),
        );
        schema(&s, &h);
        insert(&h, ROOT, "old", "u", "userMessage", 50, "", "Old prompt");
        insert(
            &h,
            ROOT,
            "old",
            "a",
            "agentMessage",
            51,
            "final",
            "Old answer",
        );
        insert(&h, ALIAS, "new", "u", "userMessage", 1, "", "New prompt");
        insert(
            &h,
            ALIAS,
            "new",
            "a",
            "agentMessage",
            2,
            "final",
            "Resumed answer",
        );
        s.execute(
            "UPDATE threads SET rollout_path=?",
            [format!("rollout-date-{ROOT}_{ALIAS}.jsonl")],
        )
        .unwrap();
        let mut reader = Reader::default();
        assert_eq!(
            reader.read(&s, &h, ROOT).unwrap(),
            Preview::Text("Resumed answer".into())
        );
        h.execute(
            "INSERT INTO thread_history_projection_state VALUES(?,0)",
            [ALIAS],
        )
        .unwrap();
        assert_eq!(reader.read(&s, &h, ROOT).unwrap(), Preview::Empty);
        h.execute(
            "UPDATE thread_history_projection_state SET next_rollout_ordinal=2",
            [],
        )
        .unwrap();
        assert_eq!(reader.read(&s, &h, ROOT).unwrap(), Preview::Empty);
        h.execute(
            "UPDATE thread_history_projection_state SET next_rollout_ordinal=3",
            [],
        )
        .unwrap();
        assert_eq!(
            reader.read(&s, &h, ROOT).unwrap(),
            Preview::Text("Resumed answer".into())
        );
    }
    #[test]
    fn long_unicode_and_multiline_replies_are_bounded() {
        let (s, h) = (
            Connection::open_in_memory().unwrap(),
            Connection::open_in_memory().unwrap(),
        );
        schema(&s, &h);
        insert(&h, ROOT, "t", "u", "userMessage", 1, "", "prompt");
        insert(
            &h,
            ROOT,
            "t",
            "a",
            "agentMessage",
            2,
            "final",
            &"最新回复内容".repeat(20000),
        );
        let Preview::Text(text) = Reader::default().read(&s, &h, ROOT).unwrap() else {
            panic!("Missing long reply")
        };
        assert_eq!(text.chars().count(), 901);
        assert!(text.ends_with('…'));
        let multiline = excerpt(&"line\n".repeat(100));
        assert!(multiline.lines().count() <= 14);
        assert!(!excerpt("safe\0content").contains('\0'));
    }
    #[test]
    fn asynchronous_hover_refresh_uses_local_read_only_databases() {
        let root = crate::guard::test_workspace()
            .join("tmp")
            .canonicalize()
            .unwrap();
        let dir = root.join(format!("reply-worker-{}", std::process::id()));
        fs_create(&dir);
        let paths = Paths {
            home: dir.clone(),
            ..Paths::discover().unwrap()
        };
        let s = Connection::open(dir.join("state_5.sqlite")).unwrap();
        let h = Connection::open(dir.join("thread_history_1.sqlite")).unwrap();
        schema(&s, &h);
        insert(&h, ROOT, "t", "u", "userMessage", 1, "", "prompt");
        insert(
            &h,
            ROOT,
            "t",
            "a",
            "agentMessage",
            2,
            "final",
            "Reply from background worker",
        );
        let mut service = Service::new(paths.clone());
        assert_eq!(service.revision(), 0);
        service.request(ROOT, 0);
        let deadline = Instant::now() + Duration::from_secs(3);
        while service.peek(ROOT).is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            service.peek(ROOT),
            Some(Preview::Text("Reply from background worker".into()))
        );
        assert!(open(&paths, "state_5.sqlite")
            .unwrap()
            .execute("DELETE FROM threads", [])
            .is_err());
        insert(
            &h,
            ROOT,
            "t",
            "a",
            "agentMessage",
            2,
            "final",
            "Fresh reply after change",
        );
        h.execute(
            "UPDATE thread_items SET updated_at_ordinal=3 WHERE item_id='a'",
            [],
        )
        .unwrap();
        service.inactive();
        service.request(ROOT, 1);
        let deadline = Instant::now() + Duration::from_secs(3);
        while service.peek(ROOT) != Some(Preview::Text("Fresh reply after change".into()))
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            service.peek(ROOT),
            Some(Preview::Text("Fresh reply after change".into()))
        );
        drop(service);
        drop(s);
        drop(h);
        std::fs::remove_dir_all(dir).unwrap();
    }
    fn fs_create(path: &std::path::Path) {
        std::fs::create_dir_all(path).unwrap();
    }
}
