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
    CapacityRetry(crate::capacity_retry::Request),
    Navigate(String),
    Icon(String, String),
    Refresh,
}

// A desktop list request can hang on a specific older chat. Keep the local
// metadata list intact and temporarily reduce only the live status window.
struct ListLimit {
    limit: usize,
    probe_at: Option<Instant>,
}
impl Default for ListLimit {
    fn default() -> Self { Self { limit: 20, probe_at: None } }
}
impl ListLimit {
    fn request(&self, now: Instant) -> usize {
        if self.probe_at.is_some_and(|at| now >= at) { 20 } else { self.limit }
    }
    fn success(&mut self, requested: usize) {
        if requested == 20 { *self = Self::default(); }
    }
    fn failed(&mut self, requested: usize, error: &str, now: Instant) {
        if !error.contains("Desktop request timed out") { return; }
        self.limit = match requested { 20.. => 10, 6..=19 => 5, 3..=5 => 2, _ => 1 };
        self.probe_at = Some(now + Duration::from_secs(300));
    }
}

// The recent sidebar snapshot is bounded; history mode provides all local
// unarchived chat statuses through cursor pages. Never publish an incomplete
// scan as fresh, because it could hide a running or capacity-failed chat.
fn history_statuses(mut fetch:impl FnMut(Option<&str>,usize)->Result<Value>)->Result<Vec<Value>>{
    let mut cursor:Option<String>=None;let mut seen=std::collections::HashSet::new();let mut rows=Vec::new();
    loop {
        let mut limit=20;
        let page=loop{match fetch(cursor.as_deref(),limit){
            Ok(page)=>break page,
            Err(error) if format!("{error:#}").contains("Desktop request timed out") && limit>1=>{limit=match limit{20=>10,10=>5,5=>2,_=>1};},
            Err(error)=>return Err(error.context("Could not read all local chat status pages")),
        }};
        let entries=page["threads"].as_array().context("Chat status page lacks threads")?;
        anyhow::ensure!(entries.len()<=limit,"Chat status page exceeded its requested limit");
        rows.extend(entries.iter().filter(|entry|entry["kind"]=="codex" && entry["hostId"]=="local").cloned());
        match page.get("nextCursor"){
            None|Some(Value::Null)=>break,
            Some(Value::String(next)) if !next.is_empty()=>{
                anyhow::ensure!(seen.insert(next.clone()) && seen.len()<=10000,"Chat status page cursor did not advance");cursor=Some(next.clone());
            },
            _=>bail!("Invalid chat status page cursor"),
        }
    }
    Ok(rows)
}
fn merge_history(payload:&mut Value,history:Vec<Value>){
    let mut positions:std::collections::HashMap<String,Vec<(&str,usize)>>=std::collections::HashMap::new();
    for key in ["pinnedThreads","threads"] {
        if let Some(rows)=payload[key].as_array(){for (index,row) in rows.iter().enumerate(){if let Some(id)=row["id"].as_str(){positions.entry(id.to_owned()).or_default().push((key,index));}}}
    }
    for entry in history {
        let Some(id)=entry["id"].as_str() else{continue};
        if let Some(matches)=positions.get(id) {
            for (key,index) in matches {payload[*key][*index]["status"]=entry["status"].clone();}
        } else if let Some(rows)=payload.get_mut("threads").and_then(Value::as_array_mut){
            let key=id.to_owned();let index=rows.len();rows.push(entry);positions.insert(key,vec![("threads",index)]);
        }
    }
}

// Reuse recent snapshots for previously active chats; only query missing IDs.
// Unavailable snapshots remain tracked and never authorize a continuation.
fn priority_statuses(tracked:&mut Vec<String>,payload:&Value,mut fetch:impl FnMut(&str)->Result<Value>)->(Vec<Value>,bool){
    let mut output=Vec::new();let mut finished=std::collections::HashSet::new();let mut incomplete=false;
    for id in tracked.iter(){
        let recent=["pinnedThreads","threads"].iter().flat_map(|key|payload[*key].as_array().into_iter().flatten()).find(|row|row["id"].as_str()==Some(id));
        let status=if let Some(row)=recent {row["status"].as_str().map(str::to_owned)} else {
            match fetch(id){Ok(value) if value["thread"]["id"].as_str()==Some(id)=>value["thread"]["status"]["type"].as_str().map(str::to_owned),_=>None}
        };
        if let Some(status)=status {
            if !matches!(status.as_str(),"active"|"busy"|"running"|"inProgress"){finished.insert(id.clone());}
            if recent.is_none(){output.push(json!({"kind":"codex","hostId":"local","id":id,"status":status}));}
        } else {incomplete=true;}
    }
    tracked.retain(|id|!finished.contains(id));(output,incomplete)
}
fn merge_priority(payload:&mut Value,mut priority:Vec<Value>){
    if let Some(rows)=payload.get_mut("threads").and_then(Value::as_array_mut){priority.append(rows);*rows=priority;}
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
        // Bundled files can retain old packaging timestamps after an update.
        adapters.sort_by_key(|p| (p.parent().and_then(Path::file_name).map(|n|n.to_string_lossy().split('.').filter_map(|v|v.parse::<u64>().ok()).collect::<Vec<_>>()).unwrap_or_default(),fs::metadata(p).and_then(|m| m.modified()).ok()));
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
        self.call_raw("initialize",json!({"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"codex-session-bar-rust","version":env!("CARGO_PKG_VERSION")}}))?;
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
        self.tool_attempts(name, args, context, 2)
    }
    fn tool_once(&mut self,name:&str,args:Value,context:&str)->Result<Value>{self.tool_attempts(name,args,context,1)}
    fn read_tool(&mut self, name: &str, args: Value, context: &str) -> Result<Value> {
        // Background retries are scheduled externally; avoid chaining two
        // blocking requests before allowing the other source to update.
        self.tool_attempts(name, args, context, 1)
    }
    fn tool_attempts(&mut self, name: &str, args: Value, context: &str, attempts: usize) -> Result<Value> {
        for attempt in 0..attempts {
            if self
                .child
                .as_mut()
                .map_or(true, |p| p.try_wait().ok().flatten().is_some())
                || !Path::new(&self.pipe).exists()
            {
                if let Err(error)=self.start(){self.close();return Err(error);}
            }
            match self.call_raw(
                "tools/call",
                json!({"name":name,"arguments":args,"_meta":{"codexThreadId":context}}),
            ) {
                Ok(result) => return payload(&result),
                Err(e) => {
                    self.close();
                    if attempt + 1 == attempts || self.stop.load(Ordering::Relaxed) {
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
        .or_else(||env::var_os("LOCALAPPDATA").and_then(|local|bundled_nodes(&PathBuf::from(local).join("OpenAI/Codex/runtimes/cua_node")).into_iter().next()))
        .or_else(|| {
            env::var_os("PATH").and_then(|paths| {
                env::split_paths(&paths)
                    .map(|p| p.join("node.exe"))
                    .find(|p| p.is_file())
            })
        })
}
fn bundled_nodes(root:&Path)->Vec<PathBuf>{
    let mut nodes:Vec<_>=fs::read_dir(root).into_iter().flatten().filter_map(|entry|entry.ok()).take(32).map(|entry|entry.path().join("bin/node.exe")).filter(|p|p.is_file()).collect();
    nodes.sort_by_key(|p|std::cmp::Reverse(fs::metadata(p).and_then(|m|m.modified()).ok()));nodes
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
fn synced_quotas(value: &Value) -> Result<Vec<crate::data::Quota>> {
    let windows = quotas(value);
    anyhow::ensure!(!windows.is_empty(), "Account limits not available yet; retrying");
    Ok(windows)
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
        handles.push(crate::refresh::watch(paths, state.lock().unwrap().refresh_events.clone(), stop.clone()));
        let (p, s, done) = (paths.clone(), state.clone(), stop.clone());
        handles.push(thread::spawn(move || {
            let mut store = Store::new(&p);
            let mut last_id = String::new();
            let mut gate = crate::refresh::Gate::new();
            let mut retry = crate::refresh::Retry::default();
            let events = s.lock().unwrap().refresh_events.clone();
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
                    let cfg = s.lock().unwrap().refresh_options.clone();
                    if id != last_id { retry.success(); }
                    if retry.waiting() || (id == last_id && !retry.due() && !gate.due(events.generation(), cfg.current_secs, cfg.fallback_secs)) {
                        if retry.pending() { events.wait(events.generation(),Duration::from_secs(1)); }
                        else { gate.pause(&events,cfg.current_secs); }
                        continue;
                    }
                    last_id = id.clone();
                    gate.mark(events.generation());
                    match store.read(&id) {
                        Ok(snapshot) => {
                            retry.success();
                            let mut live = s.lock().unwrap();
                            if (auto && live.auto_thread && live.view.as_deref() == Some(&id))
                                || (!auto && !live.auto_thread && live.fixed_thread == id)
                            {
                                live.snapshot = Some(snapshot);
                                live.snapshot_error.clear();
                            }
                        }
                        Err(e) => {
                            retry.failed();
                            let mut live = s.lock().unwrap();
                            if (auto && live.auto_thread && live.view.as_deref() == Some(&id))
                                || (!auto && !live.auto_thread && live.fixed_thread == id) {
                                live.snapshot_error = format!("{e:#}");
                            }
                        }
                    }
                } else {
                    last_id.clear();
                    retry.success();
                }
                events.wait(events.generation(),Duration::from_secs(1));
            }
        }));
        let (p, s, done) = (paths.clone(), state.clone(), stop.clone());
        handles.push(thread::spawn(move||{
            let mut rpc=Rpc::new(&p,done.clone());let mut store=Store::new(&p);
            let mut last_quota: Option<Instant> = None;
            let mut last_cache: Option<Instant> = None;
            let mut list_retry=crate::refresh::Retry::default();
            let mut list_limit=ListLimit::default();
            let mut quota_retry=crate::refresh::Retry::default();
            let events=s.lock().unwrap().refresh_events.clone();let mut gate=crate::refresh::Gate::new();
            let mut capacity_fallback=crate::capacity_retry::NativeFallback::default();
            let mut tracked:Vec<String>=s.lock().unwrap().recent.busy_metrics.iter().map(|row|row.id.clone()).collect();
            while !done.load(Ordering::Relaxed) {
                let cfg=s.lock().unwrap().refresh_options.clone();
                let forced=s.lock().unwrap().quota_at<0.;
                // A pending manual quota refresh stays flagged until success;
                // it must not keep refreshing a healthy chat list every second.
                let list_due = if list_retry.pending() { list_retry.due() } else { (forced && !quota_retry.pending()) || gate.due(events.generation(),cfg.list_secs,cfg.fallback_secs) };
                let quota_due = if quota_retry.pending() { quota_retry.due() } else { forced || last_quota.is_none_or(|at| at.elapsed() >= Duration::from_secs(60)) };
                if !list_due && !quota_due {
                    if list_retry.pending() || quota_retry.pending() { events.wait(events.generation(),Duration::from_secs(1)); }
                    else { gate.pause(&events,cfg.list_secs); }
                    continue;
                }
                let ctx=context(&s.lock().unwrap());
                if list_due {
                gate.mark(events.generation());
                let requested=list_limit.request(Instant::now());
                match rpc.read_tool("list_threads",json!({"limit":requested}),&ctx).and_then(|mut payload|{
                    let history=history_statuses(|cursor,limit|{
                        let mut args=json!({"mode":"history","source":"codex","hostId":"local","limit":limit});
                        if let Some(cursor)=cursor {args["cursor"]=json!(cursor);}
                        rpc.read_tool("list_threads",args,&ctx)
                    })?;
                    merge_history(&mut payload,history);
                    let mut next_tracked=tracked.clone();
                    let (priority,priority_incomplete)=priority_statuses(&mut next_tracked,&payload,|id|rpc.read_tool("read_thread",json!({"threadId":id,"hostId":"local","turnLimit":1,"includeOutputs":false,"maxOutputCharsPerItem":64}),&ctx));
                    merge_priority(&mut payload,priority);store.recent(&payload).map(|recent|(recent,priority_incomplete,next_tracked))
                }) {
                    Ok((mut recent,priority_incomplete,next_tracked))=>{
                        list_limit.success(requested);
                        tracked=next_tracked;
                        if priority_incomplete {recent.error="Some live statuses are temporarily unavailable".into();}
                        let capacity_enabled=s.lock().unwrap().capacity_options.enabled;
                        capacity_fallback.resolve(&p,&mut recent,capacity_enabled,|id|rpc.read_tool("read_thread",json!({"threadId":id,"hostId":"local","turnLimit":1,"includeOutputs":false,"maxOutputCharsPerItem":256}),&ctx));
                        for row in &recent.busy_metrics {if !tracked.contains(&row.id){tracked.push(row.id.clone());}}
                        list_retry.success();
                        let mut live=s.lock().unwrap();
                        recent.carry_read_state(&live.recent);
                        recent.apply_opened(&mut live.opened_results);
                        if last_cache.is_none_or(|at| at.elapsed()>=Duration::from_secs(cfg.cache_secs)) {
                            p.write("recent-cache.json",&json!({"schema":1,"rows":recent.rows,"all_rows":recent.all_rows,"busy_count":recent.busy_count,"busy_metrics":recent.busy_metrics,"loaded_at":recent.loaded_at,"stale":false}));
                            last_cache=Some(Instant::now());
                        }
                        live.recent=std::sync::Arc::new(recent);
                    },
                    Err(e)=>{let error=format!("{e:#}");list_limit.failed(requested,&error,Instant::now());list_retry.failed();
                        // Without the desktop window, retain a useful current local
                        // list from SQLite. It remains explicitly stale: local
                        // metadata cannot prove that no Codex writer is active.
                        let host=s.lock().unwrap().host;let offline=unsafe{crate::host::bounds(host).is_none()};
                        let local=if offline{store.recent(&json!({})).ok()}else{None};
                        let mut live=s.lock().unwrap();if let Some(mut local)=local{local.stale=true;local.error=error;local.carry_read_state(&live.recent);local.apply_opened(&mut live.opened_results);live.recent=std::sync::Arc::new(local);}else{let recent=std::sync::Arc::make_mut(&mut live.recent);recent.stale=true;recent.error=error;}
                    }
                }
                }
                if quota_due {
                    match rpc.read_tool("get_usage_limits",json!({}),&ctx).and_then(|value|synced_quotas(&value)){
                        Ok(windows)=>{quota_retry.success();let mut live=s.lock().unwrap();live.quota=windows;live.quota_at=now();live.quota_error.clear();last_quota=Some(Instant::now());}
                        Err(e)=>{quota_retry.failed();s.lock().unwrap().quota_error=format!("{e:#}");}
                    }
                }
                events.wait(events.generation(),Duration::from_secs(1));
            }
        }));
        let (p, s, done, cat) = (paths.clone(), state.clone(), stop.clone(), catalog.clone());
        handles.push(thread::spawn(move || {
            let mut rpc = Rpc::new(&p, done.clone());
            let store = Store::new(&p);
            let mut pending = std::collections::VecDeque::new();
            let mut last_action=Instant::now();
            while !done.load(Ordering::Relaxed) {
                let action = if let Some(v) = pending.pop_front() {
                    v
                } else {
                    match receiver.recv_timeout(Duration::from_millis(100)) {
                        Ok(v) => v,
                        Err(_)=>{if last_action.elapsed()>=Duration::from_secs(30){rpc.close();}continue;},
                    }
                };
                last_action=Instant::now();
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
                let capacity_action=matches!(action,Action::CapacityRetry(_));
                let result = (|| -> Result<String> {
                    match action {
                        Action::CapacityRetry(ref request)=>{
                            let check=if crate::capacity_retry::eligible(&s,request){rpc.read_tool("read_thread",json!({"threadId":request.failure.thread_id,"hostId":"local","turnLimit":1,"includeOutputs":false,"maxOutputCharsPerItem":256}),&ctx)}else{Ok(Value::Null)};
                            if !crate::capacity_retry::check_native(&p,&s,request,check)? {return Ok("Capacity retry skipped".into());}
                            let mut verified=request.clone();verified.failure.native_verified_at=Some(crate::now());
                            crate::capacity_retry::dispatch(&p,&s,&store,&verified,|thread,prompt|{
                            let value=rpc.tool_once("send_message_to_thread",json!({"threadId":thread,"hostId":"local","prompt":prompt}),&ctx)?;
                            anyhow::ensure!(value["threadId"]==thread,"Codex did not confirm the continuation target");Ok(())
                        })},
                        Action::Refresh => {
                            s.lock().unwrap().quota_at = -1.0;
                            Ok("Refreshing account limits…".into())
                        }
                        Action::Navigate(ref id) => {
                            if let Some(project)=store.unique_project_name(id) {
                                let hwnd=s.lock().unwrap().host;
                                let _=crate::host::expand_project(hwnd,&project);
                            }
                            let value =
                                rpc.tool("navigate_to_codex_page", json!({"threadId":id}), &ctx)?;
                            anyhow::ensure!(
                                value["navigated"] == true,
                                "Codex did not confirm navigation"
                            );
                            let mut live=s.lock().unwrap();
                            if let Some(at)=std::sync::Arc::make_mut(&mut live.recent).mark_opened(id) {
                                live.opened_results.insert(id.clone(),at);
                            }
                            Ok("Chat opened in Codex".into())
                        }
                        Action::Icon(ref id, ref icon) => {
                            let old = store.label(id)?;
                            let label = cat.rename(&old, icon);
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
                            let recent = std::sync::Arc::make_mut(&mut live.recent);
                            recent.loaded_at=now();
                            for row in recent.rows.iter_mut().chain(&mut recent.all_rows).chain(&mut recent.busy_metrics) {
                                if row.id == *id {
                                    row.label = label.clone();
                                }
                            }
                            Ok("Icon synced to Codex".into())
                        }
                    }
                })();
                let mut live = s.lock().unwrap();
                if capacity_action {if let Err(error)=&result {live.capacity_retry.last_error=format!("{error:#}").chars().take(300).collect();}}
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

pub fn inspect_capacity(paths:&Paths)->Result<()> {
    let stop=std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut rpc=Rpc::new(paths,stop);
    let mut store=Store::new(paths);
    let context=default_context(paths);
    let payload=rpc.read_tool("list_threads",json!({"limit":50}),&context)?;
    let mut recent=store.recent(&payload)?;
    crate::capacity_retry::NativeFallback::default().resolve(paths,&mut recent,true,|id|rpc.read_tool("read_thread",json!({"threadId":id,"hostId":"local","turnLimit":1,"includeOutputs":false,"maxOutputCharsPerItem":256}),&context));
    let mut rows=Vec::new();
    for failure in recent.capacity_failures.iter().take(12) {
        let mut row=json!({"thread_id":failure.thread_id,"name":failure.label,"failed_turn":failure.turn_id,"failed_at":failure.failed_at,"observed_at":failure.observed_at,"eligible":failure.fresh(crate::now())});
        if failure.fresh(crate::now()) {
            match rpc.read_tool("read_thread",json!({"threadId":failure.thread_id,"hostId":"local","turnLimit":1,"includeOutputs":false,"maxOutputCharsPerItem":256}),&context) {
                Ok(value)=>{row["native_confirms"]=json!(crate::capacity_retry::native_matches(&value,failure));row["native_status"]=value["thread"]["status"].clone();}
                Err(error)=>row["native_error"]=json!(format!("{error:#}")),
            }
        }
        rows.push(row);
    }
    println!("{}",json!({"version":env!("CARGO_PKG_VERSION"),"available":recent.capacity_available,"failures":rows,"mutations":false}));Ok(())
}
pub fn inspect_prompts(paths:&Paths)->Result<()> {
    let stop=std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut rpc=Rpc::new(paths,stop);let mut store=Store::new(paths);let context=default_context(paths);
    let payload=rpc.read_tool("list_threads",json!({"limit":50}),&context)?;
    let cold=Instant::now();let recent=store.recent(&payload)?;let cold_ms=cold.elapsed().as_secs_f64()*1000.;
    let warm=Instant::now();let _=store.recent(&payload)?;let warm_ms=warm.elapsed().as_secs_f64()*1000.;
    let rows:Vec<_>=recent.rows.iter().take(12).map(|row|json!({"id":row.id,"name":row.label,"prompt":row.prompt,"history_source":store.prompt_source(&row.id)})).collect();
    println!("{}",json!({"version":env!("CARGO_PKG_VERSION"),"mutations":false,"rows":rows,"local_chats":recent.rows.len(),"cold_ms":cold_ms,"warm_ms":warm_ms}));Ok(())
}
pub fn inspect(paths: &Paths) -> Result<()> {
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut rpc = Rpc::new(paths, stop);
    let mut store = Store::new(paths);
    let context = default_context(paths);
    let payload=rpc.tool("list_threads", json!({"limit":50}), &context)?;
    let cold=Instant::now();
    let recent=store.recent(&payload)?;
    let cold_ms=cold.elapsed().as_secs_f64()*1000.;
    let warm=Instant::now();
    for _ in 0..3 {let _=store.recent(&payload)?;}
    let warm_ms=warm.elapsed().as_secs_f64()*1000./3.;
    let cat=Catalog::load(paths)?;
    let mut search=crate::search::ChatSearch::default();search.sync(&recent.rows,&cat);
    let titles=Instant::now();for _ in 0..1000 {std::hint::black_box(search.filter(&recent.rows,"huihua",false));}
    let title_us=titles.elapsed().as_secs_f64()*1e6/1000.;
    let icons=Instant::now();for _ in 0..1000 {std::hint::black_box(cat.filter("camera","默认集合"));}
    let emoji_us=icons.elapsed().as_secs_f64()*1e6/1000.;
    let custom=crate::tags::Custom::load(paths);let tag_catalog=custom.catalog(&recent.rows);
    let chosen=tag_catalog.first().map(|(tag,_)|std::collections::HashSet::from([tag.key.clone()])).unwrap_or_default();
    let tags=Instant::now();for _ in 0..1000{std::hint::black_box(recent.rows.iter().filter(|row|custom.matches(row,&chosen)).count());}
    let tag_us=tags.elapsed().as_secs_f64()*1e6/1000.;
    let mut tag_names=crate::search::LabelSearch::default();tag_names.sync(tag_catalog.iter().map(|(t,_)|(t.key.as_str(),t.label.as_str())));
    let tag_pinyin=Instant::now();for _ in 0..1000{std::hint::black_box(tag_catalog.iter().filter(|(tag,_)|tag_names.matches(&tag.key,"pc")).count());}
    let tag_pinyin_us=tag_pinyin.elapsed().as_secs_f64()*1e6/1000.;
    let metrics=json!({"metadata_cold_ms":cold_ms,"metadata_warm_ms":warm_ms,"title_pinyin_filter_us":title_us,"emoji_filter_us":emoji_us,"tag_filter_us":tag_us,"tag_pinyin_filter_us":tag_pinyin_us,
        "prompt_coverage":recent.rows.iter().filter(|r|!r.prompt.is_empty()).count(),"project_tag_chats":recent.rows.iter().filter(|r|r.tags.iter().any(|t|t.kind==crate::tags::Kind::Project)).count(),
        "group_tag_chats":recent.rows.iter().filter(|r|r.tags.iter().any(|t|t.kind==crate::tags::Kind::Group)).count()});
    let sorted = recent.rows.windows(2).all(|w| w[0].updated_at >= w[1].updated_at);
    let local_count = recent.rows.len();
    let quota = quotas(&rpc.tool("get_usage_limits", json!({}), &context)?);
    paths.write("rust-inspect.json",&json!({"time":now(),"rows":recent.rows.len(),"local_chats":local_count,"sorted_by_recency":sorted,"busy":recent.busy_count,"quota":quota,"performance":metrics,"runtime":"rust","mutations":false}));
    println!(
        "{}",
        json!({"rows":recent.rows.len(),"local_chats":local_count,"sorted_by_recency":sorted,"busy":recent.busy_count,"quota_windows":quota.len(),"performance":metrics,"runtime":"rust","mutations":false})
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

/// A missing local projection is not evidence of inactivity. Ask the live
/// desktop and accept only an unloaded/idle thread with a terminal last turn.
pub(crate) fn confirm_unloaded_terminal(paths:&Paths,id:&str,stop:&Stop)->Result<()> {
    let mut rpc=Rpc::new(paths,stop.clone());let value=rpc.read_tool("read_thread",json!({"threadId":id,"hostId":"local","turnLimit":1,"includeOutputs":false,"maxOutputCharsPerItem":64}),&default_context(paths))?;
    validate_unloaded_terminal(&value,id)
}
pub(crate) fn confirm_inactive_for_delete(paths:&Paths,id:&str,stop:&Stop)->Result<()> {
    let mut rpc=Rpc::new(paths,stop.clone());let value=rpc.read_tool("read_thread",json!({"threadId":id,"hostId":"local","turnLimit":1,"includeOutputs":false,"maxOutputCharsPerItem":64}),&default_context(paths))?;
    validate_inactive_for_delete(&value,id)
}
fn validate_inactive_for_delete(value:&Value,id:&str)->Result<()> {
    if value.pointer("/thread/id").and_then(Value::as_str)==Some(id) && matches!(value.pointer("/thread/status/type").and_then(Value::as_str),Some("notLoaded"|"not_loaded"|"idle")) && value.get("turns").and_then(Value::as_array).is_some_and(|turns|turns.is_empty()){return Ok(())}
    validate_unloaded_terminal(&value,id)
}
fn validate_unloaded_terminal(value:&Value,id:&str)->Result<()> {
    anyhow::ensure!(value.pointer("/thread/id").and_then(Value::as_str)==Some(id),"Codex did not confirm the session identity");
    let status=value.pointer("/thread/status/type").and_then(Value::as_str).unwrap_or("");
    anyhow::ensure!(matches!(status,"notLoaded"|"not_loaded"|"idle"),"Codex cannot confirm the session is inactive");
    let turn=value.pointer("/turns/0/status").and_then(Value::as_str).unwrap_or("");
    anyhow::ensure!(matches!(turn,"completed"|"failed"|"interrupted"|"cancelled"|"canceled"),"Codex cannot confirm that the latest turn has finished");Ok(())
}

#[cfg(test)]mod tests {
    use super::*;
    #[test]fn missing_projection_requires_confirmed_unloaded_terminal_turn(){
        let mut value=json!({"thread":{"id":"a","status":{"type":"notLoaded"}},"turns":[{"status":"completed"}]});assert!(validate_unloaded_terminal(&value,"a").is_ok());assert!(validate_unloaded_terminal(&value,"b").is_err());value["thread"]["status"]["type"]=json!("active");assert!(validate_unloaded_terminal(&value,"a").is_err());value["thread"]["status"]["type"]=json!("notLoaded");value["turns"][0]["status"]=json!("inProgress");assert!(validate_unloaded_terminal(&value,"a").is_err());value["turns"]=json!([]);assert!(validate_unloaded_terminal(&value,"a").is_err());
    }
    #[test]fn deletion_accepts_empty_inactive_chat_but_refuses_active_or_wrong_identity(){let mut v=json!({"thread":{"id":"a","status":{"type":"notLoaded"}},"turns":[]});assert!(validate_inactive_for_delete(&v,"a").is_ok());assert!(validate_inactive_for_delete(&v,"b").is_err());v["thread"]["status"]["type"]=json!("active");assert!(validate_inactive_for_delete(&v,"a").is_err());}
    #[test]fn timeout_reduces_only_status_window_and_periodically_retries_full_window(){
        let now=Instant::now();let mut policy=ListLimit::default();assert_eq!(policy.request(now),20);
        policy.failed(20,"Desktop request timed out",now);assert_eq!(policy.request(now),10);
        policy.success(10);assert_eq!(policy.request(now+Duration::from_secs(299)),10);
        assert_eq!(policy.request(now+Duration::from_secs(300)),20);
        policy.failed(20,"Desktop request timed out",now+Duration::from_secs(300));assert_eq!(policy.request(now+Duration::from_secs(301)),10);
        policy.failed(10,"Desktop request timed out",now);assert_eq!(policy.request(now),5);
        policy.failed(5,"Desktop request timed out",now);assert_eq!(policy.request(now),2);
        policy.failed(2,"Desktop request timed out",now);assert_eq!(policy.request(now),1);
        policy.failed(1,"Desktop request timed out",now);assert_eq!(policy.request(now),1);
        policy.success(20);assert_eq!(policy.request(now),20);
        policy.failed(20,"permission denied",now);assert_eq!(policy.request(now),20);
    }
    #[test]fn history_pages_collect_all_statuses_and_preserve_sidebar_metadata(){
        let mut calls=Vec::new();
        let history=history_statuses(|cursor,limit|{calls.push((cursor.map(str::to_owned),limit));match cursor {
            None=>Ok(json!({"threads":[{"kind":"codex","hostId":"local","id":"first","status":"active"}],"nextCursor":"next"})),
            Some("next")=>Ok(json!({"threads":[{"kind":"codex","hostId":"local","id":"older","status":"active"}],"nextCursor":null})),
            _=>panic!("unexpected cursor"),
        }}).unwrap();
        assert_eq!(calls,vec![(None,20),(Some("next".into()),20)]);assert_eq!(history.len(),2);
        let mut recent=json!({"pinnedThreads":[],"threads":[{"id":"first","status":"idle","isUnread":true}]});
        merge_history(&mut recent,history);assert_eq!(recent["threads"][0]["status"],"active");assert_eq!(recent["threads"][0]["isUnread"],true);assert_eq!(recent["threads"][1]["id"],"older");
    }
    #[test]fn history_reduces_timed_out_page_and_rejects_partial_or_looping_pages(){
        let mut calls=Vec::new();let rows=history_statuses(|_,limit|{calls.push(limit);if limit==20{Err(anyhow::anyhow!("Desktop request timed out"))}else{Ok(json!({"threads":[],"nextCursor":null}))}}).unwrap();
        assert!(rows.is_empty());assert_eq!(calls,vec![20,10]);
        assert!(history_statuses(|_,_|Ok(json!({"threads":[],"nextCursor":"repeat"}))).is_err());
        assert!(history_statuses(|cursor,_|if cursor.is_none(){Ok(json!({"threads":[],"nextCursor":"next"}))}else{Err(anyhow::anyhow!("connection closed"))}).is_err());
    }
    #[test]fn priority_snapshots_track_active_and_failed_chats_without_reusing_missing_status(){
        let mut tracked=vec!["running".into(),"capacity".into(),"unavailable".into()];
        let mut list=json!({"threads":[{"id":"running","status":"active"}]});
        let (rows,incomplete)=priority_statuses(&mut tracked,&list,|id|{
            assert_ne!(id,"running");
            if id=="capacity"{Ok(json!({"thread":{"id":id,"status":{"type":"systemError"}}}))}else{Err(anyhow::anyhow!("timeout"))}
        });
        assert!(incomplete);assert_eq!(tracked,vec!["running","unavailable"]);
        assert_eq!(rows.len(),1);assert_eq!(rows[0]["status"],"systemError");
        merge_priority(&mut list,rows);assert_eq!(list["threads"][0]["id"],"capacity");assert_eq!(list["threads"][1]["id"],"running");
        let (rows,incomplete)=priority_statuses(&mut tracked,&json!({"threads":[]}),|_|Err(anyhow::anyhow!("timeout")));
        assert!(rows.is_empty()&&incomplete);assert_eq!(tracked,vec!["running","unavailable"]);
        let (rows,incomplete)=priority_statuses(&mut tracked,&json!({"threads":[{"id":"running","status":"active"}]}),|id|Ok(json!({"thread":{"id":id,"status":{"type":"idle"}}})));
        assert!(!incomplete);assert_eq!(rows.len(),1);assert_eq!(tracked,vec!["running"]);
    }
    #[test]fn empty_startup_limits_are_retryable_until_real_data_arrives(){
        assert!(synced_quotas(&json!({"rateLimits":null})).is_err());
        assert!(synced_quotas(&json!({"rateLimitsByLimitId":{}})).is_err());
        let ready=synced_quotas(&json!({"rateLimits":{"primary":{"usedPercent":0,"windowDurationMins":10080,"resetsAt":1800000000}}})).unwrap();
        assert_eq!(ready[0].remaining,100.);
    }
    #[test]fn bundled_runtime_discovery_ignores_missing_and_unrelated_entries(){
        let paths=Paths::discover().unwrap();let dir=paths.runtime.join(format!("runtime-discovery-test-{}",std::process::id()));
        fs::create_dir_all(dir.join("new-runtime/bin")).unwrap();fs::create_dir_all(dir.join("missing/bin")).unwrap();
        fs::write(dir.join("new-runtime/bin/node.exe"),b"fixture").unwrap();
        assert_eq!(bundled_nodes(&dir),vec![dir.join("new-runtime/bin/node.exe")]);
        assert!(bundled_nodes(&dir.join("absent")).is_empty());fs::remove_dir_all(dir).unwrap();
    }
}
