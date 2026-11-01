//! Session Manager's native Codex deletion transport.
use crate::{Paths,Stop};
use anyhow::{Context,Result};
use serde_json::{json,Value};
use std::{collections::HashSet,io::{BufRead,BufReader,Read,Write},os::windows::process::CommandExt,path::Path,process::{Child,ChildStdin,Command,Stdio},sync::mpsc,thread,time::{Duration,Instant}};
pub fn orphan(row:&crate::repair::Entry,custom:&crate::tags::Custom)->bool {
 !row.archived&&row.tags_known&&!row.project_assigned&&row.tags.is_empty()&&custom.chats.get(&row.id).is_none_or(|keys|!custom.definitions.iter().any(|tag|keys.contains(&tag.key)))
}
pub fn toggle_filtered(selected:&mut HashSet<String>,rows:&[crate::repair::Entry]) {
 let all=!rows.is_empty()&&rows.iter().all(|row|selected.contains(&row.id));for row in rows{if all{selected.remove(&row.id);}else{selected.insert(row.id.clone());}}
}
fn operation_error(error:&Value,zh:bool)->String {
 let message=error.get("message").and_then(Value::as_str).unwrap_or("");
 if message.contains("already has an active writer") {
  if zh {"Codex 仍占用此会话的写入器。请先在 Codex 中切换到其他会话或关闭该会话，再重试；若仍被占用，请重启 Codex 后重试。".into()}
  else {"Codex still holds a writer for this chat. Switch to another chat or close it in Codex, then retry; if it remains in use, restart Codex and retry.".into()}
 } else {format!("{}: {}",if zh{"Codex 拒绝操作"}else{"Codex rejected the operation"},error)}
}
struct Server { child:Child,input:ChildStdin,replies:mpsc::Receiver<Value> }
impl Drop for Server{fn drop(&mut self){let _=self.child.kill();let _=self.child.wait();}}
impl Server {
 fn open(exe:&Path,home:&Path,temp:&Path)->Result<Self>{
  let mut child=Command::new(exe).arg("app-server").env("CODEX_HOME",home).env("TEMP",temp).env("TMP",temp).env("TMPDIR",temp).creation_flags(0x08000000).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
  let input=child.stdin.take().context("Codex stdin unavailable")?;let stdout=child.stdout.take().context("Codex stdout unavailable")?;let(tx,rx)=mpsc::channel();
  thread::spawn(move||{let mut r=BufReader::new(stdout);loop{let mut data=Vec::new();match r.by_ref().take(1024*1024+1).read_until(b'\n',&mut data){Ok(0)|Err(_)=>break,Ok(_)=>{if data.len()>1024*1024{break}if let Ok(value)=serde_json::from_slice(&data){if tx.send(value).is_err(){break}}}}}});
  let mut s=Self{child,input,replies:rx};s.request(1,"initialize",json!({"clientInfo":{"name":"codex-session-bar","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}))?;s.send(json!({"jsonrpc":"2.0","method":"initialized","params":{}}))?;Ok(s)
 }
 fn send(&mut self,v:Value)->Result<()>{serde_json::to_writer(&mut self.input,&v)?;self.input.write_all(b"\n")?;self.input.flush()?;Ok(())}
 fn request(&mut self,id:u64,method:&str,params:Value)->Result<Value>{self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;let until=Instant::now()+Duration::from_secs(if method=="thread/delete"{900}else{30});loop{let msg=self.replies.recv_timeout(until.saturating_duration_since(Instant::now())).context("Codex did not confirm the operation; its outcome is unknown")?;if msg["id"].as_u64()==Some(id){if let Some(error)=msg.get("error"){anyhow::bail!("{}",operation_error(error,crate::locale::chinese()));}return msg.get("result").cloned().context("Codex returned no operation result")}}}
}
pub struct Deleter {server:Server,next:u64}
impl Deleter {
 pub fn open(paths:&Paths,exe:&Path)->Result<Self>{Ok(Self{server:Server::open(exe,&paths.home,&paths.runtime)?,next:2})}
 /// One request only; an ambiguous failure must never be retried automatically.
 pub fn delete(&mut self,paths:&Paths,id:&str,stop:&Stop)->Result<()> {
  anyhow::ensure!(id.len()==36&&id.chars().all(|c|c.is_ascii_hexdigit()||c=='-'),"Invalid session ID");
  crate::desktop::confirm_inactive_for_delete(paths,id,stop)?;self.submit(id)
 }
 pub fn force_delete(&mut self,id:&str)->Result<()> {anyhow::ensure!(id.len()==36&&id.chars().all(|c|c.is_ascii_hexdigit()||c=='-'),"Invalid session ID");self.submit(id)}
 pub fn archive(&mut self,paths:&Paths,id:&str,stop:&Stop)->Result<()> {anyhow::ensure!(id.len()==36&&id.chars().all(|c|c.is_ascii_hexdigit()||c=='-'),"Invalid session ID");crate::desktop::confirm_inactive_for_delete(paths,id,stop)?;self.submit_archive(id)}
 fn submit_archive(&mut self,id:&str)->Result<()> {let request=self.next;self.next+=1;self.server.request(request,"thread/archive",json!({"threadId":id}))?;Ok(())}
 fn submit(&mut self,id:&str)->Result<()> {let request=self.next;self.next+=1;self.server.request(request,"thread/delete",json!({"threadId":id}))?;Ok(())}
}
#[cfg(test)]mod tests{use super::*;
 #[test]fn active_writer_error_explains_why_and_other_errors_keep_details(){let e=json!({"code":-32600,"message":"thread 123 already has an active writer"});assert!(operation_error(&e,true).contains("写入器"));assert!(operation_error(&e,false).contains("writer"));let other=json!({"code":-32600,"message":"permission denied"});assert!(operation_error(&other,false).contains("permission denied"));}
 #[test]fn select_all_is_limited_to_filtered_rows_and_keeps_hidden_selection(){let rows=vec![crate::repair::Entry{id:"a".into(),..Default::default()},crate::repair::Entry{id:"b".into(),..Default::default()}];let mut selected=HashSet::from(["hidden".to_string()]);toggle_filtered(&mut selected,&rows);assert_eq!(selected.len(),3);toggle_filtered(&mut selected,&rows);assert_eq!(selected,HashSet::from(["hidden".to_string()]));}
 #[test]fn orphan_requires_known_metadata_no_project_and_no_tags(){let mut row=crate::repair::Entry{id:"a".into(),tags_known:true,..Default::default()};let mut custom=crate::tags::Custom::default();assert!(orphan(&row,&custom));row.archived=true;assert!(!orphan(&row,&custom));row.archived=false;row.project_assigned=true;assert!(!orphan(&row,&custom));row.project_assigned=false;custom.add("a","test",0).unwrap();assert!(!orphan(&row,&custom));row.tags_known=false;assert!(!orphan(&row,&crate::tags::Custom::default()));}
 #[test]
 #[ignore="Explicit native archive test in an independent workspace tmp fixture"]
 fn native_archive_marks_only_selected_fixture(){
  let root=std::path::PathBuf::from(std::env::var_os("CODEX_MANAGER_ARCHIVE_FIXTURE").expect("fixture required")).canonicalize().unwrap();let workspace=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();assert!(root.starts_with(workspace.join("tmp").canonicalize().unwrap()));let home=root.join("home");let id=std::fs::read_to_string(root.join("thread-id.txt")).unwrap();let id=id.trim();let db=rusqlite::Connection::open(home.join("state_5.sqlite")).unwrap();let before:u64=db.query_row("SELECT count(*) FROM threads WHERE id<>?",[id],|r|r.get(0)).unwrap();drop(db);
  let exe=std::path::PathBuf::from(std::env::var_os("CODEX_CLEAN_NATIVE_EXE").unwrap());let paths=Paths{home:home.clone(),runtime:root.clone(),app:root.clone(),instance:String::new()};let mut server=Deleter::open(&paths,&exe).unwrap();server.submit_archive(id).unwrap();drop(server);let db=rusqlite::Connection::open(home.join("state_5.sqlite")).unwrap();assert_eq!(db.query_row("SELECT archived FROM threads WHERE id=?",[id],|r|r.get::<_,u64>(0)).unwrap(),1);assert_eq!(db.query_row("SELECT count(*) FROM threads WHERE id<>?",[id],|r|r.get::<_,u64>(0)).unwrap(),before);
 }
 #[test]
 #[ignore="Explicit force-delete test in an independent workspace tmp fixture"]
 fn native_force_delete_legacy_compressed_fixture(){
  let root=std::path::PathBuf::from(std::env::var_os("CODEX_MANAGER_FORCE_FIXTURE").unwrap()).canonicalize().unwrap();let workspace=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();assert!(root.starts_with(workspace.join("tmp").canonicalize().unwrap()));let home=root.join("home");let id=std::fs::read_to_string(root.join("thread-id.txt")).unwrap();let id=id.trim();let db=rusqlite::Connection::open(home.join("state_5.sqlite")).unwrap();let source=std::path::PathBuf::from(format!("{}.zst",db.query_row("SELECT rollout_path FROM threads WHERE id=?",[id],|r|r.get::<_,String>(0)).unwrap()));assert!(source.canonicalize().unwrap().starts_with(home.canonicalize().unwrap()));let other:u64=db.query_row("SELECT count(*) FROM threads WHERE id<>?",[id],|r|r.get(0)).unwrap();drop(db);
  let exe=std::path::PathBuf::from(std::env::var_os("CODEX_CLEAN_NATIVE_EXE").unwrap());let paths=Paths{home:home.clone(),runtime:root.clone(),app:root.clone(),instance:String::new()};let mut deleter=Deleter::open(&paths,&exe).unwrap();deleter.force_delete(id).unwrap();drop(deleter);let db=rusqlite::Connection::open(home.join("state_5.sqlite")).unwrap();assert_eq!(db.query_row("SELECT count(*) FROM threads WHERE id=?",[id],|r|r.get::<_,u64>(0)).unwrap(),0);assert_eq!(db.query_row("SELECT count(*) FROM threads WHERE id<>?",[id],|r|r.get::<_,u64>(0)).unwrap(),other);assert!(!source.exists());
 }
 #[test]
 #[ignore="Explicit native delete test in an independent workspace tmp fixture"]
 fn native_delete_removes_only_selected_fixture(){
  let root=std::path::PathBuf::from(std::env::var_os("CODEX_MANAGER_DELETE_FIXTURE").expect("fixture required")).canonicalize().unwrap();let workspace=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();assert!(root.starts_with(workspace.join("tmp").canonicalize().unwrap()));let home=root.join("home");let id=std::fs::read_to_string(root.join("thread-id.txt")).unwrap();let id=id.trim();let db=rusqlite::Connection::open(home.join("state_5.sqlite")).unwrap();let source=std::path::PathBuf::from(db.query_row("SELECT rollout_path FROM threads WHERE id=?",[id],|r|r.get::<_,String>(0)).unwrap());assert!(source.canonicalize().unwrap().starts_with(home.canonicalize().unwrap()));let other:u64=db.query_row("SELECT count(*) FROM threads WHERE id<>?",[id],|r|r.get(0)).unwrap();drop(db);
  let exe=std::path::PathBuf::from(std::env::var_os("CODEX_CLEAN_NATIVE_EXE").unwrap());let paths=Paths{home:home.clone(),runtime:root.clone(),app:root.clone(),instance:String::new()};let mut deleter=Deleter::open(&paths,&exe).unwrap();deleter.submit(id).unwrap();drop(deleter);let db=rusqlite::Connection::open(home.join("state_5.sqlite")).unwrap();assert_eq!(db.query_row("SELECT count(*) FROM threads WHERE id=?",[id],|r|r.get::<_,u64>(0)).unwrap(),0);assert_eq!(db.query_row("SELECT count(*) FROM threads WHERE id<>?",[id],|r|r.get::<_,u64>(0)).unwrap(),other);assert!(!source.exists());
 }
}
