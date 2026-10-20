use crate::{
    data::{quotas, Catalog, Store},
    now, Paths, Shared, State, Stop,
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{atomic::Ordering, mpsc},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub enum Action {
    Navigate(String),
    Icon(String, String),
    Refresh,
}
pub struct Rpc {
    paths: Paths,
    stop: Stop,
    child: Option<Child>,
    input: Option<ChildStdin>,
    replies: Option<mpsc::Receiver<Value>>,
    next: u64,
    pipe: String,
}
impl Rpc {
    pub fn new(paths: &Paths, stop: Stop) -> Self {
        Self {
            paths: paths.clone(),
            stop,
            child: None,
            input: None,
            replies: None,
            next: 0,
            pipe: String::new(),
        }
    }
    fn start(&mut self) -> Result<()> {
        self.close();
        let node = find_node().context("Node.js was not found")?;
        let root = self
            .paths
            .home
            .join("plugins/cache/openai-bundled/codex-app-tools");
        let mut adapters: Vec<_> = fs::read_dir(root)?
            .filter_map(|entry| entry.ok())
            .map(|e| e.path().join("server.mjs"))
            .filter(|p| p.is_file())
            .collect();
        adapters.sort_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).ok());
        let adapter = adapters
            .pop()
            .context("Codex Desktop App Tools adapter unavailable")?;
        let configured = env::var("CODEX_APP_TOOLS_PIPE_PATH")
            .ok()
            .filter(|p| Path::new(p).exists());
        self.pipe = if let Some(p) = configured {
            p
        } else {
            let pipes: Vec<String> = fs::read_dir(r"\\.\pipe\")?
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("codex-app-tools-"))
                .map(|n| format!(r"\\.\pipe\{n}"))
                .collect();
            anyhow::ensure!(
                pipes.len() == 1,
                "Cannot uniquely identify the Codex desktop connection"
            );
            pipes[0].clone()
        };
        let error_file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.paths.runtime.join("rust-desktop-stderr.log"))?;
        let mut child = Command::new(node)
            .arg(adapter)
            .env("CODEX_APP_TOOLS_PIPE_PATH", &self.pipe)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(error_file)
            .creation_flags(0x08000000)
            .spawn()?;
        let stdout = child.stdout.take().context("Adapter stdout unavailable")?;
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = Vec::new();
                match (&mut reader).take(1024 * 1024).read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => break,
                    _ => {}
                }
                if line.last() != Some(&b'\n') {
                    break;
                }
                if let Ok(v) = serde_json::from_slice::<Value>(&line) {
                    if v.get("id").is_some() && sender.send(v).is_err() {
                        return;
                    }
                }
            }
            let _ = sender.send(json!({"id":-1,"error":{"message":"Desktop connection closed"}}));
        });
        self.input = child.stdin.take();
        self.child = Some(child);
        self.replies = Some(receiver);
        self.call_raw("initialize",json!({"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"codex-session-bar-rust","version":"0.2.0"}}))?;
        self.write(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))?;
        Ok(())
    }
    fn write(&mut self, value: &Value) -> Result<()> {
        let input = self.input.as_mut().context("Desktop not connected")?;
        serde_json::to_writer(&mut *input, value)?;
        input.write_all(b"\n")?;
        input.flush()?;
        Ok(())
    }
    fn call_raw(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next += 1;
        let id = self.next;
        self.write(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline && !self.stop.load(Ordering::Relaxed) {
            match self
                .replies
                .as_ref()
                .context("Desktop reader unavailable")?
                .recv_timeout(Duration::from_millis(100))
            {
                Ok(v) if v["id"].as_u64() == Some(id) || v["id"].as_i64() == Some(-1) => {
                    if let Some(e) = v.get("error") {
                        bail!("{}", e["message"].as_str().unwrap_or("Desktop error"));
                    }
                    return Ok(v["result"].clone());
                }
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => bail!("Desktop connection closed"),
            }
        }
        bail!("Desktop request timed out")
    }
    pub fn tool(&mut self, name: &str, args: Value, context: &str) -> Result<Value> {
        for attempt in 0..2 {
            if self
                .child
                .as_mut()
                .map_or(true, |p| p.try_wait().ok().flatten().is_some())
                || !Path::new(&self.pipe).exists()
            {
                self.start()?;
            }
            match self.call_raw(
                "tools/call",
                json!({"name":name,"arguments":args,"_meta":{"codexThreadId":context}}),
            ) {
                Ok(result) => return payload(&result),
                Err(e) => {
                    self.close();
                    if attempt == 1 || self.stop.load(Ordering::Relaxed) {
                        return Err(e);
                    }
                }
            }
        }
        unreachable!()
    }
    fn close(&mut self) {
        self.input.take();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.replies.take();
    }
}
impl Drop for Rpc {
    fn drop(&mut self) {
        self.close();
    }
}
fn find_node() -> Option<PathBuf> {
    env::var_os("CODEX_MCP_NODE_PATH")
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .or_else(|| {
            env::var_os("PATH").and_then(|paths| {
                env::split_paths(&paths)
                    .map(|p| p.join("node.exe"))
                    .find(|p| p.is_file())
            })
        })
}
fn payload(value: &Value) -> Result<Value> {
    if value["isError"].as_bool() == Some(true) {
        bail!(
            "{}",
            value["content"]
                .as_array()
                .and_then(|a| a.iter().find_map(|v| v["text"].as_str()))
                .unwrap_or("Desktop action failed")
        );
    }
    if let Some(content) = value["content"].as_array() {
        for item in content {
            if item["type"] == "text" {
                if let Some(text) = item["text"].as_str() {
                    if let Ok(v) = serde_json::from_str::<Value>(text) {
                        if v.is_object() {
                            return Ok(v);
                        }
                    }
                }
            }
        }
    }
    if let Some(v) = value.get("structuredContent").filter(|v| v.is_object()) {
        return Ok(v.clone());
    }
    bail!("Desktop response did not include structured data")
}
fn wait(stop: &Stop, millis: u64) {
    for _ in 0..millis / 50 {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
}
fn context(state: &Shared) -> String {
    state
        .view
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| state.fixed_thread.clone())
}

pub struct Workers {
    pub actions: mpsc::Sender<Action>,
    handles: Vec<thread::JoinHandle<()>>,
    stop: Stop,
}
impl Workers {
    pub fn start(paths: &Paths, state: &State, stop: &Stop, catalog: &Catalog) -> Self {
        let mut handles = Vec::new();
        let (sender, receiver) = mpsc::channel();
        let (p, s, done) = (paths.clone(), state.clone(), stop.clone());
        handles.push(thread::spawn(move || {
            let mut store = Store::new(&p);
            let mut last_id = String::new();
            let mut refreshed = Instant::now() - Duration::from_secs(1);
            while !done.load(Ordering::Relaxed) {
                let (id, auto) = {
                    let live = s.lock().unwrap();
                    (
                        if live.auto_thread {
                            live.view.clone()
                        } else {
                            Some(live.fixed_thread.clone())
                        },
                        live.auto_thread,
                    )
                };
                if let Some(id) = id.filter(|id| !id.is_empty()) {
                    if id == last_id && refreshed.elapsed() < Duration::from_millis(500) {
                        wait(&done, 50);
                        continue;
                    }
                    last_id = id.clone();
                    refreshed = Instant::now();
                    match store.read(&id) {
                        Ok(snapshot) => {
                            let mut live = s.lock().unwrap();
                            if (auto && live.auto_thread && live.view.as_deref() == Some(&id))
                                || (!auto && !live.auto_thread && live.fixed_thread == id)
                            {
                                live.snapshot = Some(snapshot);
                            }
                        }
                        Err(e) => {
                            let mut live = s.lock().unwrap();
                            if live.view.as_deref() == Some(&id) {
                                live.view_error = format!("{e:#}");
                            }
                        }
                    }
                }
                wait(&done, 50);
            }
        }));
        let (p, s, done) = (paths.clone(), state.clone(), stop.clone());
        handles.push(thread::spawn(move||{
            let mut rpc=Rpc::new(&p,done.clone());let mut store=Store::new(&p);
            while !done.load(Ordering::Relaxed) {
                let ctx=context(&s.lock().unwrap());
                match rpc.tool("list_threads",json!({"limit":50}),&ctx).and_then(|payload|store.recent(&payload)) {
                    Ok(recent)=>{p.write("recent-cache.json",&json!({"schema":1,"rows":recent.rows,"all_rows":recent.all_rows,"busy_count":recent.busy_count,"busy_metrics":recent.busy_metrics,"loaded_at":recent.loaded_at,"stale":false}));s.lock().unwrap().recent=recent;},
                    Err(e)=>{let mut live=s.lock().unwrap();live.recent.stale=true;live.recent.error=format!("{e:#}");}
                }
                wait(&done,2000);
            }
        }));
        let (p, s, done) = (paths.clone(), state.clone(), stop.clone());
        handles.push(thread::spawn(move || {
            let mut rpc = Rpc::new(&p, done.clone());
            let mut last = 0.0;
            while !done.load(Ordering::Relaxed) {
                if now() - last >= 60.0 || s.lock().unwrap().quota_at < 0.0 {
                    let ctx = context(&s.lock().unwrap());
                    match rpc.tool("get_usage_limits", json!({}), &ctx) {
                        Ok(value) => {
                            let mut live = s.lock().unwrap();
                            live.quota = quotas(&value);
                            live.quota_at = now();
                            live.quota_error.clear();
                            last = now();
                        }
                        Err(e) => {
                            s.lock().unwrap().quota_error = format!("{e:#}");
                            last = now() - 45.0;
                        }
                    }
                }
                wait(&done, 250);
            }
        }));
        let (p, s, done, cat) = (paths.clone(), state.clone(), stop.clone(), catalog.clone());
        handles.push(thread::spawn(move || {
            let mut rpc = Rpc::new(&p, done.clone());
            let store = Store::new(&p);
            let mut pending = std::collections::VecDeque::new();
            while !done.load(Ordering::Relaxed) {
                let action = if let Some(v) = pending.pop_front() {
                    v
                } else {
                    match receiver.recv_timeout(Duration::from_millis(100)) {
                        Ok(v) => v,
                        Err(_) => continue,
                    }
                };
                for next in receiver.try_iter() {
                    if matches!(next, Action::Navigate(_)) {
                        pending.retain(|a| !matches!(a, Action::Navigate(_)));
                    }
                    pending.push_back(next);
                }
                let action = if matches!(action, Action::Navigate(_))
                    && pending.iter().any(|a| matches!(a, Action::Navigate(_)))
                {
                    continue;
                } else {
                    action
                };
                let ctx = context(&s.lock().unwrap());
                let result = (|| -> Result<String> {
                    match action {
                        Action::Refresh => {
                            s.lock().unwrap().quota_at = -1.0;
                            Ok("Refreshing account limits…".into())
                        }
                        Action::Navigate(ref id) => {
                            let value =
                                rpc.tool("navigate_to_codex_page", json!({"threadId":id}), &ctx)?;
                            anyhow::ensure!(
                                value["navigated"] == true,
                                "Codex did not confirm navigation"
                            );
                            Ok("Chat opened in Codex".into())
                        }
                        Action::Icon(ref id, ref icon) => {
                            let label = cat.rename(&store.label(id)?, icon);
                            let value = rpc.tool(
                                "set_thread_title",
                                json!({"threadId":id,"title":label}),
                                &ctx,
                            )?;
                            anyhow::ensure!(
                                value["threadId"] == *id && value["title"] == label,
                                "Codex did not confirm the title change"
                            );
                            let mut live = s.lock().unwrap();
                            if let Some(snapshot) = live.snapshot.as_mut().filter(|v| v.id == *id) {
                                snapshot.label = label.clone();
                            }
                            for row in &mut live.recent.rows {
                                if row.id == *id {
                                    row.label = label.clone();
                                }
                            }
                            Ok("Icon synced to Codex".into())
                        }
                    }
                })();
                let mut live = s.lock().unwrap();
                live.action_note = match result {
                    Ok(note) => note,
                    Err(e) => format!("Action failed: {e:#}"),
                };
                live.action_pending = false;
            }
        }));
        Self {
            actions: sender,
            handles,
            stop: stop.clone(),
        }
    }
}
impl Drop for Workers {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

pub fn inspect(paths: &Paths) -> Result<()> {
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut rpc = Rpc::new(paths, stop);
    let mut store = Store::new(paths);
    let context = default_context(paths);
    let recent = rpc
        .tool("list_threads", json!({"limit":50}), &context)
        .and_then(|p| store.recent(&p))?;
    let sorted = recent.rows.windows(2).all(|w| w[0].updated_at >= w[1].updated_at);
    let local_count = store.sessions()?.len();
    let quota = quotas(&rpc.tool("get_usage_limits", json!({}), &context)?);
    paths.write("rust-inspect.json",&json!({"time":now(),"rows":recent.rows.len(),"local_chats":local_count,"sorted_by_recency":sorted,"busy":recent.busy_count,"quota":quota,"runtime":"rust","mutations":false}));
    println!(
        "{}",
        json!({"rows":recent.rows.len(),"local_chats":local_count,"sorted_by_recency":sorted,"busy":recent.busy_count,"quota_windows":quota.len(),"runtime":"rust","mutations":false})
    );
    Ok(())
}

pub fn default_context(paths: &Paths) -> String {
    // Transport metadata never determines the visible chat or home overview.
    env::var("CODEX_THREAD_ID")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| {
            crate::read_json(&paths.app.join("settings.json"), 16384)
                .get("thread")
                .and_then(Value::as_str)
                .map(String::from)
                .filter(|v| !v.is_empty())
        })
        .or_else(|| {
            Store::new(paths)
                .sessions()
                .ok()?
                .into_iter()
                .next()
                .map(|r| r.id)
        })
        .unwrap_or_default()
}
