//! Read-only estimates of generated-file diffs stored in a session.
use crate::{Paths,Stop};
use anyhow::{Context,Result};
use rusqlite::{Connection,OpenFlags};
use serde::{Deserialize,Serialize};
use serde_json::Value;
use std::{collections::BTreeMap,fs::File,io::{BufRead,BufReader},path::PathBuf,sync::atomic::Ordering,time::Duration};

const RECORD_LIMIT:usize=8*1024*1024;
// Cleaning can shrink large output records before native preflight applies its
// own lower per-record limit. Larger inputs are rejected without modification.
const CLEAN_RECORD_LIMIT:usize=64*1024*1024;
pub fn display_bytes(bytes:u64)->String {
    if bytes>=1024*1024 {format!("{:.2} MiB",bytes as f64/1048576.)}
    else if bytes>=1024 {format!("{:.1} KiB",bytes as f64/1024.)}
    else {format!("{bytes} B")}
}
#[derive(Clone,Default,Debug,Serialize,Deserialize)]
pub struct Analysis {
    pub rollout_changes:u64,pub rollout_diff_bytes:u64,pub rollout_candidates:u64,pub rollout_candidate_bytes:u64,
    pub projection_changes:u64,pub projection_diff_bytes:u64,pub projection_candidates:u64,pub projection_candidate_bytes:u64,
    #[serde(default)] pub rollout_tool_outputs:u64,
    #[serde(default)] pub rollout_tool_output_bytes:u64,
    #[serde(default)] pub rollout_tool_inputs:u64,
    #[serde(default)] pub rollout_tool_input_bytes:u64,
    pub oversized_rollout_records:u64,pub oversized_rollout_bytes:u64,pub oversized_projection_items:u64,pub oversized_projection_bytes:u64,pub compressed:bool,
    #[serde(default)] pub rollout_extensions:BTreeMap<String,(u64,u64)>,
    #[serde(default)] pub rollout_record_types:BTreeMap<String,(u64,u64)>,
    #[serde(default)] pub estimated_diff_delta:i64,
    #[serde(default)] pub estimated_with_io_delta:i64,
    #[serde(default)] pub estimate_incomplete:bool,
}
impl Analysis {
    pub fn file_reduction(&self,with_io:bool)->Option<u64> {
        if self.compressed || self.estimate_incomplete {return None}
        Some(if with_io{self.estimated_with_io_delta}else{self.estimated_diff_delta}.max(0) as u64)
    }
    // Entire JSONL records are mutually exclusive; candidate diff text is a
    // subset and must never be added to this breakdown a second time.
    pub fn record_breakdown(&self)->[u64;7] {
        let mut totals=[0u64;7];
        for (kind,(_,size)) in &self.rollout_record_types {
            let i=match kind.as_str(){
                "event_msg/item_completed/FileChange"=>0,
                "response_item/function_call_output"|"response_item/custom_tool_call_output"=>1,
                "event_msg/item_completed/CommandExecution"=>2,
                "response_item/reasoning"=>3,
                "response_item/message"|"event_msg/user_message"|"event_msg/agent_message"=>4,
                _=>5,
            };
            totals[i]=totals[i].saturating_add(*size);
        }
        totals[6]=self.oversized_rollout_bytes;
        totals
    }
}
fn category(value:&Value)->String{
    fn field<'a>(value:&'a Value)->&'a str{value.as_str().filter(|s|s.len()<=48&&s.bytes().all(|b|b.is_ascii_alphanumeric()||b==b'_'||b==b'-')).unwrap_or("other")}
    let kind=field(&value["type"]);let payload=field(&value["payload"]["type"]);
    if kind=="event_msg"&&payload=="item_completed"{format!("{kind}/{payload}/{}",field(&value["payload"]["item"]["type"]))}
    else if payload=="other"{kind.into()}else{format!("{kind}/{payload}")}
}
fn extension(path:&str)->String{path.rsplit(['/', '\\']).next().unwrap_or("").rsplit_once('.').map(|(_,ext)|ext.to_ascii_lowercase()).filter(|x|x.len()<=24&&!x.is_empty()).unwrap_or_else(||"(none)".into())}
fn generated_path(path:&str)->bool{
    matches!(extension(path).as_str(),"vcd"|"fst"|"wlf"|"ghw"|"fsdb"|"vpd"|"ucdb"|"log"|"dump"|"raw")
}
/// Transform a single parsed rollout record in memory. Publishing the result
/// requires coordinating the rollout with Codex's SQLite projection and is
/// intentionally separate from this function.
#[allow(dead_code)]
fn clean_record(value:&mut Value,include_command_io:bool)->usize {
    const PLACEHOLDER:&str="[Content cleaned by Codex Session Bar]";
    fn replace(value:&mut Value)->usize {
        let Some(text)=value.as_str() else{return 0};
        let before=text.len();if before<=PLACEHOLDER.len(){return 0}
        *value=Value::String(PLACEHOLDER.into());before-PLACEHOLDER.len()
    }
    let mut saved=0;let kind=value["type"].as_str().unwrap_or("").to_owned();let payload=&mut value["payload"];
    if kind=="event_msg"&&payload["type"]=="item_completed" {
        let item=&mut payload["item"];
        if item["type"]=="FileChange" {
            if let Some(changes)=item["changes"].as_object_mut(){for (path,change) in changes {if generated_path(path){for field in ["unified_diff","content"]{if let Some(v)=change.get_mut(field){saved+=replace(v);}}}}}
        } else if include_command_io&&item["type"]=="CommandExecution" {
            // Different Codex versions retain one or several copies of the
            // same output. Keep the command and metadata, not those payloads.
            for key in ["aggregated_output","stdout","stderr","formatted_output","interaction_input"] {if let Some(v)=item.get_mut(key){saved+=replace(v);}}
        }
    } else if include_command_io&&kind=="response_item" {
        match payload["type"].as_str() {
            Some("function_call_output"|"custom_tool_call_output")=>if let Some(v)=payload.get_mut("output"){saved+=replace(v);},
            Some("function_call")=>{
                // Preserve the command and unknown schema fields. Only known
                // payload-bearing fields are eligible for replacement.
                if let Some(args)=payload.get_mut("arguments") {if let Some(text)=args.as_str(){if let Ok(mut parsed)=serde_json::from_str::<Value>(text){let mut changed=0;for key in ["stdin","input","content","data"]{if let Some(v)=parsed.get_mut(key){changed+=replace(v);}}if changed>0{*args=Value::String(parsed.to_string());saved+=changed;}}}}
            },
            _=>{}
        }
    }
    saved
}
/// Stage a cleaned rollout without publishing it. Preserve all unchanged bytes,
/// and map every original record boundary to its new offset so that callers
/// can update turn offsets without guessing. Unsupported records fail closed.
#[allow(dead_code)]
fn stage_rollout(reader:&mut impl BufRead,writer:&mut impl std::io::Write,include_command_io:bool,stop:&Stop)->Result<(u64,u64,Vec<(u64,u64)>)>{
    let (mut original,mut staged)=(0u64,0u64);let mut boundaries=Vec::new();
    while let Some((record,length))=next_record_limited(reader,stop,CLEAN_RECORD_LIMIT)? {
        anyhow::ensure!(!stop.load(Ordering::Relaxed),"Cancelled");
        let bytes=record.context("Session contains an oversized record; cleanup is not supported")?;
        anyhow::ensure!(!bytes.is_empty()&&bytes.last()==Some(&b'\n'),"Session ends with an incomplete record");
        let mut value:Value=serde_json::from_slice(&bytes).context("Invalid session record; cleanup stopped")?;
        boundaries.push((original,staged));
        let changed=clean_record(&mut value,include_command_io);
        if changed==0 {writer.write_all(&bytes)?;staged+=bytes.len() as u64;}
        else {let rewritten=serde_json::to_vec(&value)?;writer.write_all(&rewritten)?;writer.write_all(b"\n")?;staged+=rewritten.len() as u64+1;}
        original=original.checked_add(length).context("Session offset overflow")?;
    }
    boundaries.push((original,staged));writer.flush()?;Ok((original,staged,boundaries))
}
#[allow(dead_code)]
fn stage_projection(db:&mut Connection,id:&str,old_len:u64,new_len:u64,boundaries:&[(u64,u64)],include_command_io:bool)->Result<()> {
    use rusqlite::{params,TransactionBehavior};
    let tx=db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current:u64=tx.query_row("SELECT next_rollout_byte_offset FROM thread_history_projection_state WHERE thread_id=?",[id],|r|r.get(0))?;
    anyhow::ensure!(current==old_len,"Projection is not synchronized with the original session");
    let map=|old:u64|->Result<u64>{let i=boundaries.binary_search_by_key(&old,|v|v.0).map_err(|_|anyhow::anyhow!("Projection offset does not match a record boundary"))?;Ok(boundaries[i].1)};
    let turns={let mut q=tx.prepare("SELECT turn_id,rollout_byte_offset,rollout_end_byte_offset FROM thread_turns WHERE thread_id=?")?;let rows=q.query_map([id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,u64>(1)?,r.get::<_,Option<u64>>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;rows};
    for (turn,start,end) in turns {tx.execute("UPDATE thread_turns SET rollout_byte_offset=?,rollout_end_byte_offset=? WHERE thread_id=? AND turn_id=?",params![map(start)?,end.map(&map).transpose()?,id,turn])?;}
    let mut query=tx.prepare("SELECT turn_id,item_id,item_type,CASE WHEN length(CAST(item_json AS BLOB))<=? THEN item_json ELSE NULL END FROM thread_items WHERE thread_id=? AND item_type IN ('fileChange','commandExecution')")?;
    let mut items=query.query(params![CLEAN_RECORD_LIMIT,id])?;
    while let Some(row)=items.next()? {
        let turn:String=row.get(0)?;let item:String=row.get(1)?;let kind:String=row.get(2)?;let text:Option<String>=row.get(3)?;
        let text=text.context("Projection contains an oversized record; cleanup is not supported")?;
        let mut value:Value=serde_json::from_str(&text)?;let changed=clean_projected_item(&mut value,&kind,include_command_io);
        if changed {anyhow::ensure!(tx.execute("UPDATE thread_items SET item_json=? WHERE thread_id=? AND turn_id=? AND item_id=? AND item_json=?",params![value.to_string(),id,turn,item,text])?==1,"Projected item changed");}
    }
    drop(items);drop(query);
    anyhow::ensure!(map(old_len)?==new_len,"Staged session offset mismatch");
    tx.execute("UPDATE thread_history_projection_state SET next_rollout_byte_offset=? WHERE thread_id=?",params![new_len,id])?;tx.commit()?;Ok(())
}
fn clean_projected_item(value:&mut Value,kind:&str,include_command_io:bool)->bool {
    const PLACEHOLDER:&str="[Content cleaned by Codex Session Bar]";let mut changed=false;
    if kind=="commandExecution"&&include_command_io {for key in ["aggregatedOutput","interactionInput","stdout","stderr","formattedOutput"]{if let Some(v)=value.get_mut(key){if v.as_str().is_some_and(|s|s.len()>PLACEHOLDER.len()){*v=Value::String(PLACEHOLDER.into());changed=true;}}}}
    if kind=="fileChange" {if let Some(changes)=value.get_mut("changes").and_then(Value::as_array_mut){for change in changes {if change["path"].as_str().is_some_and(generated_path){if let Some(v)=change.get_mut("diff"){if v.as_str().is_some_and(|s|s.len()>PLACEHOLDER.len()){*v=Value::String(PLACEHOLDER.into());changed=true;}}}}}}
    changed
}
/// A journal recovery must never restore a stale snapshot over subsequent
/// writes. The caller must verify the digest again while holding Codex's writer
/// lock before taking any action; ambiguous states are explicitly rejected.
#[allow(dead_code)]
#[derive(Debug,PartialEq,Eq)]
enum RecoveryAction { AbortUnpublished, PublishStaged, FinishProjection, Complete, Manual }
#[allow(dead_code)]
fn recovery_action(old_digest:[u8;32],new_digest:[u8;32],file_digest:[u8;32],old_len:u64,new_len:u64,projection_offset:u64)->RecoveryAction {
    if old_digest==new_digest||old_len==new_len{return RecoveryAction::Manual}
    match (file_digest==old_digest,file_digest==new_digest,projection_offset==old_len,projection_offset==new_len) {
        (true,false,true,false)=>RecoveryAction::AbortUnpublished,
        (true,false,false,true)=>RecoveryAction::PublishStaged,
        (false,true,true,false)=>RecoveryAction::FinishProjection,
        (false,true,false,true)=>RecoveryAction::Complete,
        _=>RecoveryAction::Manual,
    }
}
/// Immutable description written and synced before either live store changes.
/// All paths are derived from the session ID and journal directory by the
/// caller, never trusted from this file.
#[allow(dead_code)]
#[derive(Debug,Serialize,Deserialize)]
struct CleanJournal { version:u8, thread_id:String, old_len:u64, new_len:u64, old_digest:[u8;32], new_digest:[u8;32], boundaries:Vec<(u64,u64)>, include_command_io:bool, guard_secret:[u8;32] }
#[allow(dead_code)]
impl CleanJournal {
    fn validate(&self)->Result<()> {
        anyhow::ensure!(self.version==1&&self.thread_id.len()==36&&self.thread_id.chars().all(|c|c.is_ascii_hexdigit()||c=='-'),"Invalid clean journal identity");
        anyhow::ensure!(self.new_len<self.old_len&&self.old_digest!=self.new_digest,"Invalid clean journal sizes or digests");
        anyhow::ensure!(self.guard_secret!=[0;32],"Invalid cleanup guard secret");
        anyhow::ensure!(self.boundaries.first()==Some(&(0,0))&&self.boundaries.last()==Some(&(self.old_len,self.new_len)),"Invalid clean journal boundaries");
        anyhow::ensure!(self.boundaries.windows(2).all(|w|w[0].0<w[1].0&&w[0].1<w[1].1),"Non-monotonic clean journal boundaries");Ok(())
    }
    fn write_new(&self,folder:&std::path::Path)->Result<()> {
        use std::io::Write;self.validate()?;let bytes=serde_json::to_vec(self)?;anyhow::ensure!(bytes.len()<64*1024*1024,"Clean journal exceeds the recovery limit");let path=folder.join("clean-journal.json");let mut out=std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;out.write_all(&bytes)?;out.write_all(b"\n")?;out.sync_all()?;Ok(())
    }
    fn read(folder:&std::path::Path)->Result<Self> {
        use std::io::Read;let input=File::open(folder.join("clean-journal.json"))?;anyhow::ensure!(input.metadata()?.len()<=64*1024*1024,"Clean journal exceeds the recovery limit");let value:Self=serde_json::from_reader(input.take(64*1024*1024+1))?;value.validate()?;Ok(value)
    }
}
/// Compare every projected item that cleaning is not allowed to modify. Both
/// databases are supplied by the caller; the native preflight must build the
/// candidate database independently from the candidate rollout.
#[allow(dead_code)]
fn verify_unchanged_projection(original:&Connection,candidate:&Connection,id:&str,include_command_io:bool)->Result<()> {
    use rusqlite::params;
    let sql="SELECT turn_id,item_id,item_type,CASE WHEN length(CAST(item_json AS BLOB))<=? THEN item_json ELSE NULL END FROM thread_items WHERE thread_id=? ORDER BY rollout_ordinal,turn_id,item_id";
    let mut left=original.prepare(sql)?;let mut right=candidate.prepare(sql)?;let mut a=left.query(params![CLEAN_RECORD_LIMIT,id])?;let mut b=right.query(params![CLEAN_RECORD_LIMIT,id])?;
    loop {
        match (a.next()?,b.next()?) {
            (None,None)=>break,
            (Some(x),Some(y))=>{
                for i in 0..3 {anyhow::ensure!(x.get::<_,String>(i)?==y.get::<_,String>(i)?,"Candidate changed a non-tool history item");}
                let first:Option<String>=x.get(3)?;let second:Option<String>=y.get(3)?;
                let first=first.context("Original history item exceeds the verification limit")?;let second=second.context("Candidate history item exceeds the verification limit")?;let kind:String=x.get(2)?;
                let mut expected:Value=serde_json::from_str(&first)?;clean_projected_item(&mut expected,&kind,include_command_io);
                anyhow::ensure!(expected==serde_json::from_str::<Value>(&second)?,"Candidate changed unsupported history content");
            },
            _=>anyhow::bail!("Candidate changed the number of non-tool history items"),
        }
    }
    Ok(())
}
/// Ask the installed native Codex to project the staged rollout in a private
/// scratch home, then compare all history items outside the explicitly cleaned
/// types. This never modifies the source databases or rollout.
#[allow(dead_code)]
fn preflight_native(paths:&Paths,id:&str,candidate:&std::path::Path,codex:&std::path::Path,include_command_io:bool,stop:&Stop)->Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command,Stdio};
    use std::time::Instant;
    anyhow::ensure!(id.len()==36&&id.chars().all(|c|c.is_ascii_hexdigit()||c=='-'),"Invalid session ID");
    let root=paths.runtime.canonicalize()?;let scratch=root.join(format!("clean-preflight-{}-{}",std::process::id(),crate::now().to_bits()));std::fs::create_dir(&scratch)?;
    struct Scratch { path:PathBuf, parent:PathBuf }
    impl Drop for Scratch {fn drop(&mut self){if self.path.parent()==Some(self.parent.as_path())&&self.path.file_name().is_some_and(|v|v.to_string_lossy().starts_with("clean-preflight-")){let _=std::fs::remove_dir_all(&self.path);}}}
    let _guard=Scratch{path:scratch.clone(),parent:root};
    let home=scratch.join("home");let sessions=home.join("sessions").join("2026").join("01").join("01");std::fs::create_dir_all(&sessions)?;
    let state=Connection::open_with_flags(paths.home.join("state_5.sqlite"),OpenFlags::SQLITE_OPEN_READ_ONLY|OpenFlags::SQLITE_OPEN_NO_MUTEX)?;state.busy_timeout(Duration::from_secs(2))?;
    let cloned=home.join("state_5.sqlite");state.execute("VACUUM INTO ?",[cloned.to_string_lossy().as_ref()]).context("Cannot make isolated metadata snapshot")?;
    let staged=sessions.join(format!("rollout-2026-01-01T00-00-00-{id}.jsonl"));std::fs::copy(candidate,&staged)?;
    let cloned_state=Connection::open(&cloned)?;anyhow::ensure!(cloned_state.execute("UPDATE threads SET rollout_path=? WHERE id=? AND agent_path IS NULL",rusqlite::params![staged.to_string_lossy().as_ref(),id])?==1,"Session metadata unavailable");drop(cloned_state);
    let markers=home.join("rollout-migrations");std::fs::create_dir(&markers)?;File::create(markers.join(format!("{id}.pending")))?.sync_all()?;
    let mut child=Command::new(codex).args(["migrate-rollouts","--apply","--thread",id,"--max-mib-per-second","32","--json"]).env("CODEX_HOME",&home).env("TEMP",&scratch).env("TMP",&scratch).env("TMPDIR",&scratch).creation_flags(0x08000000).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().context("Cannot start isolated Codex preflight")?;
    let started=Instant::now();loop{if stop.load(Ordering::Relaxed)||started.elapsed()>Duration::from_secs(300){let _=child.kill();let _=child.wait();anyhow::bail!("Isolated Codex preflight cancelled or timed out")};if let Some(status)=child.try_wait()?{anyhow::ensure!(status.success(),"Codex could not rebuild the staged session");break}std::thread::sleep(Duration::from_millis(100));}
    let old=Connection::open_with_flags(paths.home.join("thread_history_1.sqlite"),OpenFlags::SQLITE_OPEN_READ_ONLY|OpenFlags::SQLITE_OPEN_NO_MUTEX)?;let new=Connection::open_with_flags(home.join("thread_history_1.sqlite"),OpenFlags::SQLITE_OPEN_READ_ONLY|OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    let expected:u64=new.query_row("SELECT next_rollout_byte_offset FROM thread_history_projection_state WHERE thread_id=?",[id],|r|r.get(0))?;anyhow::ensure!(expected==std::fs::metadata(&staged)?.len(),"Codex preflight index is incomplete");verify_unchanged_projection(&old,&new,id,include_command_io)?;Ok(())
}
/// Durable, verified backup. The source must be rechecked under Codex's writer
/// lock before publication; this function alone does not authorize a rewrite.
#[allow(dead_code)]
fn backup_rollout(paths:&Paths,id:&str,source:&std::path::Path,stop:&Stop)->Result<(PathBuf,[u8;32],u64)> {
    use sha2::{Digest,Sha256};use std::io::{Read,Write};
    anyhow::ensure!(id.len()==36&&id.chars().all(|c|c.is_ascii_hexdigit()||c=='-'),"Invalid session ID");
    let original=std::fs::metadata(source)?;let parent=paths.app.join("repair-backups");std::fs::create_dir_all(&parent)?;
    let folder=parent.join(format!("clean-{id}-{}-{}",std::process::id(),crate::now().to_bits()));std::fs::create_dir(&folder)?;
    struct Partial {folder:PathBuf,keep:bool}impl Drop for Partial {fn drop(&mut self){if !self.keep {if let (Ok(folder),Some(parent))=(self.folder.canonicalize(),self.folder.parent().and_then(|p|p.canonicalize().ok())){if folder.parent()==Some(parent.as_path())&&folder.file_name().is_some_and(|v|v.to_string_lossy().starts_with("clean-")){let _=std::fs::remove_dir_all(folder);}}}}}let mut partial=Partial{folder:folder.clone(),keep:false};
    let backup=folder.join("rollout.jsonl.gz");let target=std::fs::OpenOptions::new().write(true).create_new(true).open(&backup)?;
    let mut encoder=flate2::write::GzEncoder::new(target,flate2::Compression::fast());let mut file=File::open(source)?;let mut hash=Sha256::new();let mut buf=[0u8;65536];let mut total=0u64;
    loop{anyhow::ensure!(!stop.load(Ordering::Relaxed),"Backup cancelled");let n=file.read(&mut buf)?;if n==0{break}encoder.write_all(&buf[..n])?;hash.update(&buf[..n]);total+=n as u64;}
    let target=encoder.finish()?;target.sync_all()?;drop(target);
    let after=std::fs::metadata(source)?;anyhow::ensure!(total==original.len()&&after.len()==original.len()&&after.modified()?==original.modified()?,"Session changed during backup");
    let expected:[u8;32]=hash.finalize().into();let mut decoder=flate2::read::GzDecoder::new(File::open(&backup)?);let mut verify=Sha256::new();let mut checked=0u64;
    loop{anyhow::ensure!(!stop.load(Ordering::Relaxed),"Backup verification cancelled");let n=decoder.read(&mut buf)?;if n==0{break}verify.update(&buf[..n]);checked+=n as u64;}
    let digest:[u8;32]=verify.finalize().into();anyhow::ensure!(checked==total&&digest==expected,"Backup verification failed");partial.keep=true;Ok((folder,expected,total))
}
/// Snapshot only the target thread's projection. A whole database restore would
/// overwrite unrelated sessions that continued to write during cleanup.
#[allow(dead_code)]
fn projection_schema(db:&Connection)->Result<Vec<(String,String)>> {
    let mut triggers=db.prepare("SELECT name,tbl_name,sql FROM main.sqlite_master WHERE type='trigger' ORDER BY name")?;
    let triggers=triggers.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (name,table,sql) in triggers {
        let normalized:String=sql.chars().filter(|c|!c.is_whitespace()&&*c!=';'&&*c!='\"').flat_map(char::to_lowercase).collect();
        anyhow::ensure!(name=="thread_realtime_items_projection_cleanup"&&table=="thread_history_projection_state"&&normalized=="createtriggerthread_realtime_items_projection_cleanupafterdeleteonthread_history_projection_statebegindeletefromthread_realtime_itemswherethread_id=old.thread_idend","Unknown history trigger; cleanup is not supported");
    }
    let mut statement=db.prepare("SELECT name,sql FROM main.sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name != '_sqlx_migrations' ORDER BY name")?;
    let tables=statement.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut result=Vec::new();
    for (name,sql) in tables {
        let quoted=name.replace('"',"\"\"");let mut info=db.prepare(&format!("PRAGMA main.table_info(\"{quoted}\")"))?;let names=info.query_map([],|r|r.get::<_,String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?;
        if names.iter().any(|n|n=="thread_id") {
            anyhow::ensure!(matches!(name.as_str(),"thread_items"|"thread_turns"|"thread_history_projection_state"|"thread_realtime_items"),"Unknown per-thread history table; cleanup is not supported");result.push((name,sql));
        }
    }
    for required in ["thread_items","thread_turns","thread_history_projection_state"] {anyhow::ensure!(result.iter().any(|(name,_)|name==required),"Required history table is missing");}Ok(result)
}
#[allow(dead_code)]
fn snapshot_projection(db:&mut Connection,id:&str,snapshot:&std::path::Path)->Result<()> {
    use rusqlite::params;anyhow::ensure!(!snapshot.exists(),"Projection backup already exists");let schema=projection_schema(db)?;
    db.execute("ATTACH DATABASE ? AS clean_snapshot",[snapshot.to_string_lossy().as_ref()])?;
    let result=(||->Result<()>{let tx=db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;tx.execute_batch("CREATE TABLE clean_snapshot.clean_schema(name TEXT PRIMARY KEY, sql TEXT NOT NULL)")?;
        for (name,sql) in &schema {tx.execute("INSERT INTO clean_snapshot.clean_schema VALUES(?,?)",params![name,sql])?;tx.execute(&format!("CREATE TABLE clean_snapshot.\"{name}\" AS SELECT * FROM main.\"{name}\" WHERE thread_id=?"),[id])?;}tx.commit()?;Ok(())})();
    let detached=db.execute_batch("DETACH DATABASE clean_snapshot");result?;detached?;std::fs::OpenOptions::new().write(true).open(snapshot)?.sync_all()?;Ok(())
}
#[allow(dead_code)]
fn restore_projection(db:&mut Connection,id:&str,snapshot:&std::path::Path)->Result<()> {
    let schema=projection_schema(db)?;db.execute("ATTACH DATABASE ? AS clean_snapshot",[snapshot.to_string_lossy().as_ref()])?;
    let result=(||->Result<()>{let saved={let mut q=db.prepare("SELECT name,sql FROM clean_snapshot.clean_schema ORDER BY name")?;let rows=q.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;rows};anyhow::ensure!(schema==saved,"History schema changed since the backup");
        let tx=db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;for (name,_) in &schema{tx.execute(&format!("DELETE FROM main.\"{name}\" WHERE thread_id=?"),[id])?;tx.execute(&format!("INSERT INTO main.\"{name}\" SELECT * FROM clean_snapshot.\"{name}\" WHERE thread_id=?"),[id])?;}tx.commit()?;Ok(())})();
    let detached=db.execute_batch("DETACH DATABASE clean_snapshot");result?;detached?;Ok(())
}
/// Install only a previously absent projection, in one transaction. Existing
/// rows always win, even if they appeared while the isolated rebuild ran.
fn install_empty_projection(db:&mut Connection,id:&str,snapshot:&std::path::Path)->Result<()> {
    use rusqlite::params;let schema=projection_schema(db)?;db.execute("ATTACH DATABASE ? AS clean_snapshot",[snapshot.to_string_lossy().as_ref()])?;
    let result=(||->Result<()>{let saved={let mut q=db.prepare("SELECT name,sql FROM clean_snapshot.clean_schema ORDER BY name")?;let rows=q.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;rows};anyhow::ensure!(schema==saved,"Rebuilt history schema does not match the live schema");
        let tx=db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for (name,_) in &schema {let count:u64=tx.query_row(&format!("SELECT count(*) FROM main.\"{name}\" WHERE thread_id=?"),[id],|r|r.get(0))?;anyhow::ensure!(count==0,"Session history appeared while rebuilding; retry safely");}
        for (name,_) in &schema {tx.execute(&format!("INSERT INTO main.\"{name}\" SELECT * FROM clean_snapshot.\"{name}\" WHERE thread_id=?"),params![id])?;}tx.commit()?;Ok(())})();
    let detached=db.execute_batch("DETACH DATABASE clean_snapshot");result?;detached?;Ok(())
}

/// Rebuild a missing projection only from a byte-for-byte isolated copy. A
/// fresh live activity check, writer locks, and a second digest check guard the
/// atomic import; no rollout bytes are changed by this step.
fn rebuild_missing_projection<F:FnMut()->Result<()>>(paths:&Paths,id:&str,source:&std::path::Path,codex:&std::path::Path,stop:&Stop,mut check_idle:F)->Result<()> {
    use std::os::windows::process::CommandExt;use std::process::{Command,Stdio};use std::time::Instant;
    check_idle()?;let (digest,len)=hash_file(source,stop)?;
    let root=paths.runtime.canonicalize()?;let scratch=root.join(format!("clean-index-{}-{}",std::process::id(),crate::now().to_bits()));std::fs::create_dir(&scratch)?;
    struct Scratch{path:PathBuf,parent:PathBuf}impl Drop for Scratch{fn drop(&mut self){if self.path.parent()==Some(self.parent.as_path())&&self.path.file_name().is_some_and(|v|v.to_string_lossy().starts_with("clean-index-")){let _=std::fs::remove_dir_all(&self.path);}}}let _scratch=Scratch{path:scratch.clone(),parent:root};
    let home=scratch.join("home");let sessions=home.join("sessions").join("2026").join("01").join("01");std::fs::create_dir_all(&sessions)?;
    let state=Connection::open_with_flags(paths.home.join("state_5.sqlite"),OpenFlags::SQLITE_OPEN_READ_ONLY|OpenFlags::SQLITE_OPEN_NO_MUTEX)?;state.busy_timeout(Duration::from_secs(2))?;
    let cloned=home.join("state_5.sqlite");state.execute("VACUUM INTO ?",[cloned.to_string_lossy().as_ref()])?;
    let staged=sessions.join(format!("rollout-2026-01-01T00-00-00-{id}.jsonl"));std::fs::copy(source,&staged)?;anyhow::ensure!(hash_file(&staged,stop)?==(digest,len),"Session changed during isolated index rebuild");
    let state=Connection::open(&cloned)?;anyhow::ensure!(state.execute("UPDATE threads SET rollout_path=? WHERE id=? AND agent_path IS NULL",rusqlite::params![staged.to_string_lossy().as_ref(),id])?==1,"Session metadata unavailable");drop(state);
    let markers=home.join("rollout-migrations");std::fs::create_dir(&markers)?;File::create(markers.join(format!("{id}.pending")))?.sync_all()?;
    let mut child=Command::new(codex).args(["migrate-rollouts","--apply","--thread",id,"--max-mib-per-second","32","--json"]).env("CODEX_HOME",&home).env("TEMP",&scratch).env("TMP",&scratch).env("TMPDIR",&scratch).creation_flags(0x08000000).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("Cannot start isolated history reconstruction")?;
    // Drain both pipes concurrently; retain only bounded diagnostics.
    fn drain<R:std::io::Read+Send+'static>(mut input:R)->std::thread::JoinHandle<Vec<u8>>{std::thread::spawn(move||{let mut kept=Vec::new();let mut buf=[0u8;4096];while let Ok(n)=input.read(&mut buf){if n==0{break}let take=n.min(32768usize.saturating_sub(kept.len()));kept.extend_from_slice(&buf[..take]);}kept})}
    let stdout=drain(child.stdout.take().context("Migration output unavailable")?);let stderr=drain(child.stderr.take().context("Migration error output unavailable")?);
    let started=Instant::now();let status=loop {if stop.load(Ordering::Relaxed)||started.elapsed()>Duration::from_secs(900){let _=child.kill();let _=child.wait();anyhow::bail!("Isolated history reconstruction cancelled or timed out")}if let Some(status)=child.try_wait()?{break status}std::thread::sleep(Duration::from_millis(100));};
    let out=stdout.join().unwrap_or_default();let err=stderr.join().unwrap_or_default();
    if !status.success(){let detail=serde_json::from_slice::<serde_json::Value>(&out).ok().and_then(|v|v.get("outcomes")?.as_array()?.first()?.get("message")?.as_str().map(str::to_owned)).unwrap_or_else(||String::from_utf8_lossy(&err).lines().last().unwrap_or("").to_owned());anyhow::bail!("Codex could not rebuild the original session history: {}",detail.chars().take(700).collect::<String>());}

    let mut rebuilt=Connection::open(home.join("thread_history_1.sqlite"))?;let offset:u64=rebuilt.query_row("SELECT next_rollout_byte_offset FROM thread_history_projection_state WHERE thread_id=?",[id],|r|r.get(0))?;anyhow::ensure!(offset==len&&crate::reduce::confirmed_idle(&rebuilt,id)?,"Rebuilt history is incomplete or active");
    let snapshot=scratch.join("projection.sqlite");snapshot_projection(&mut rebuilt,id,&snapshot)?;drop(rebuilt);check_idle()?;
    let _locks=crate::reduce::publication_lock(paths,id)?;anyhow::ensure!(hash_file(source,stop)?==(digest,len),"Session changed before history import");
    let mut live=Connection::open(paths.home.join("thread_history_1.sqlite"))?;live.busy_timeout(Duration::from_secs(2))?;install_empty_projection(&mut live,id,&snapshot)?;Ok(())
}
fn hash_file(path:&std::path::Path,stop:&Stop)->Result<([u8;32],u64)> {
    use sha2::{Digest,Sha256};use std::io::Read;let mut input=File::open(path)?;let mut digest=Sha256::new();let mut bytes=0u64;let mut buffer=[0u8;65536];
    loop {anyhow::ensure!(!stop.load(Ordering::Relaxed),"Cancelled");let n=input.read(&mut buffer)?;if n==0{break}digest.update(&buffer[..n]);bytes+=n as u64;}Ok((digest.finalize().into(),bytes))
}
fn session_path(paths:&Paths,id:&str)->Result<PathBuf>{
    let db=Connection::open_with_flags(paths.home.join("state_5.sqlite"),OpenFlags::SQLITE_OPEN_READ_ONLY|OpenFlags::SQLITE_OPEN_NO_MUTEX)?;db.busy_timeout(Duration::from_secs(2))?;
    let (name,mode):(String,String)=db.query_row("SELECT rollout_path,coalesce(history_mode,'legacy') FROM threads WHERE id=? AND agent_path IS NULL",[id],|r|Ok((r.get(0)?,r.get(1)?)))?;
    anyhow::ensure!(mode=="paginated","Session requires history repair before cleaning");let normalize=|s:&str|s.replace('/',"\\").trim_start_matches(r"\\?\").to_ascii_lowercase();let expected=normalize(&name);
    let mut others=db.prepare("SELECT rollout_path FROM threads WHERE id<>? AND rollout_path IS NOT NULL")?;let mut rows=others.query([id])?;while let Some(row)=rows.next()?{let other:String=row.get(0)?;anyhow::ensure!(normalize(&other)!=expected,"Shared session file; cleaning skipped");}drop(rows);drop(others);
    let path=PathBuf::from(name);anyhow::ensure!(path.extension().is_some_and(|v|v=="jsonl")&&path.is_file(),"Compressed or missing session files are not yet supported");
    let resolved=path.canonicalize()?;anyhow::ensure!([paths.home.join("sessions"),paths.home.join("archived_sessions")].iter().filter_map(|p|p.canonicalize().ok()).any(|p|resolved.starts_with(p)),"Session is outside local Codex storage");crate::reduce::verify_thread(id,&resolved)?;Ok(resolved)
}
fn same_volume_candidate(source:&std::path::Path,folder:&std::path::Path)->Result<PathBuf>{
    let parent=source.parent().context("Session has no parent directory")?;let token=folder.file_name().and_then(|v|v.to_str()).context("Invalid cleanup backup folder")?;
    anyhow::ensure!(token.starts_with("clean-")&&token.chars().all(|c|c.is_ascii_alphanumeric()||c=='-'),"Invalid cleanup candidate identity");Ok(parent.join(format!(".codex-session-bar-{token}.tmp")))
}
pub(crate) fn security_sddl(path:&std::path::Path)->Result<String> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{HLOCAL,LocalFree};
    use windows::Win32::Security::{OBJECT_SECURITY_INFORMATION,PSECURITY_DESCRIPTOR};
    use windows::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;
    let mut descriptor=crate::reduce::security_descriptor(path)?;
    let mut value=PWSTR::null();let mut len=0u32;
    unsafe {ConvertSecurityDescriptorToStringSecurityDescriptorW(PSECURITY_DESCRIPTOR(descriptor.as_mut_ptr().cast()),1,OBJECT_SECURITY_INFORMATION(1|2|4),&mut value,Some(&mut len))?;}
    let result=unsafe {value.to_string()};unsafe {let _=LocalFree(HLOCAL(value.0.cast()));}
    // AI records that Windows has processed inheritance; it does not grant or
    // deny access. Compare owners, groups, protection and the actual ACEs.
    Ok(result?.replace("D:AI(","D:(").replace("D:PAI(","D:P("))
}
pub(crate) fn copy_candidate_security(source:&std::path::Path,candidate:&std::path::Path)->Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR,SetFileSecurityW};
    if security_sddl(source)?==security_sddl(candidate)? {return Ok(())}
    let mut descriptor=crate::reduce::security_descriptor(source)?;
    let wide:Vec<u16>=candidate.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe { SetFileSecurityW(PCWSTR(wide.as_ptr()),windows::Win32::Security::OBJECT_SECURITY_INFORMATION(4),PSECURITY_DESCRIPTOR(descriptor.as_mut_ptr().cast())).ok()?; }
    let original=security_sddl(source)?;let copied=security_sddl(candidate)?;
    anyhow::ensure!(copied==original,"Could not preserve session file permissions (source: {original}; candidate: {copied})");
    Ok(())
}
fn replace_atomic(source:&std::path::Path,candidate:&std::path::Path)->Result<()> {
    use std::os::windows::ffi::OsStrExt;use windows::core::PCWSTR;use windows::Win32::Storage::FileSystem::{MoveFileExW,MOVEFILE_REPLACE_EXISTING,MOVEFILE_WRITE_THROUGH};
    anyhow::ensure!(security_sddl(source)?==security_sddl(candidate)?,"Cleanup candidate permissions differ from the original");
    let src:Vec<u16>=source.as_os_str().encode_wide().chain(Some(0)).collect();let new:Vec<u16>=candidate.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe{MoveFileExW(PCWSTR(new.as_ptr()),PCWSTR(src.as_ptr()),MOVEFILE_REPLACE_EXISTING|MOVEFILE_WRITE_THROUGH).context("Same-volume session replacement failed")?};Ok(())
}
fn finish_journal(folder:&std::path::Path,name:&str)->Result<()>{
    let candidate=folder.join("candidate.jsonl");if candidate.is_file(){std::fs::remove_file(candidate)?;}std::fs::rename(folder.join("clean-journal.json"),folder.join(name))?;Ok(())
}
fn recover_one(paths:&Paths,folder:&std::path::Path,stop:&Stop)->Result<()> {
    let journal=CleanJournal::read(folder)?;let source=session_path(paths,&journal.thread_id)?;let _locks=crate::reduce::publication_lock(paths,&journal.thread_id)?;recover_locked(paths,folder,&source,&journal,stop).map(|_|())
}
fn recover_locked(paths:&Paths,folder:&std::path::Path,source:&std::path::Path,journal:&CleanJournal,stop:&Stop)->Result<bool> {
    let mut history=Connection::open(paths.home.join("thread_history_1.sqlite"))?;history.busy_timeout(Duration::from_secs(2))?;
    let offset:u64=history.query_row("SELECT next_rollout_byte_offset FROM thread_history_projection_state WHERE thread_id=?",[&journal.thread_id],|r|r.get(0))?;let (digest,_)=hash_file(&source,stop)?;
    let action=recovery_action(journal.old_digest,journal.new_digest,digest,journal.old_len,journal.new_len,offset);
    match action {
        RecoveryAction::AbortUnpublished=>{},
        RecoveryAction::PublishStaged=>restore_projection(&mut history,&journal.thread_id,&folder.join("projection.sqlite"))?,
        RecoveryAction::FinishProjection=>stage_projection(&mut history,&journal.thread_id,journal.old_len,journal.new_len,&journal.boundaries,journal.include_command_io)?,
        RecoveryAction::Complete=>{},
        RecoveryAction::Manual=>anyhow::bail!("Session changed after interrupted cleanup; backup retained for manual recovery"),
    }
    let adjacent=same_volume_candidate(&source,folder)?;if let Ok(meta)=std::fs::symlink_metadata(&adjacent){anyhow::ensure!(meta.file_type().is_file(),"Unrecognized adjacent candidate; not removed");std::fs::remove_file(adjacent)?;}
    let completed=matches!(action,RecoveryAction::FinishProjection|RecoveryAction::Complete);finish_journal(folder,if completed{"clean-complete.json"}else{"clean-aborted.json"})?;Ok(completed)
}
pub fn recover_all(paths:&Paths,stop:&Stop)->Result<()> {
    let root=paths.app.join("repair-backups");if !root.is_dir(){return Ok(())}let mut errors=Vec::new();
    for entry in std::fs::read_dir(&root)? {let entry=entry?;if !entry.file_type()?.is_dir()||!entry.file_name().to_string_lossy().starts_with("clean-"){continue}let folder=entry.path();if folder.join("clean-journal.json").is_file(){if let Err(e)=recover_one(paths,&folder,stop){errors.push(format!("{}: {e:#}",entry.file_name().to_string_lossy()));}}}
    anyhow::ensure!(errors.is_empty(),"{}",errors.join("; "));Ok(())
}
fn commit_prepared(paths:&Paths,folder:&std::path::Path,secret:[u8;32],stop:&Stop,_fault:u8)->Result<()> {
    let journal=CleanJournal::read(folder)?;anyhow::ensure!(secret==journal.guard_secret,"Invalid cleanup guard handshake");
    anyhow::ensure!(crate::reduce::publication_lock(paths,&journal.thread_id).is_err(),"Cleanup guard is not holding the Codex lock");
    let source=session_path(paths,&journal.thread_id)?;let adjacent=same_volume_candidate(&source,folder)?;
    let (old,old_len)=hash_file(&source,stop)?;let (new,new_len)=hash_file(&adjacent,stop)?;anyhow::ensure!(old==journal.old_digest&&old_len==journal.old_len&&new==journal.new_digest&&new_len==journal.new_len,"Cleanup files changed before commit");
    let mut history=Connection::open(paths.home.join("thread_history_1.sqlite"))?;history.busy_timeout(Duration::from_secs(2))?;
    stage_projection(&mut history,&journal.thread_id,journal.old_len,journal.new_len,&journal.boundaries,journal.include_command_io)?;
    #[cfg(test)] if _fault==1 {std::process::exit(77);}
    replace_atomic(&source,&adjacent)?;
    #[cfg(test)] if _fault==2 {std::process::exit(77);}
    let (actual,len)=hash_file(&source,stop)?;anyhow::ensure!(actual==journal.new_digest&&len==journal.new_len,"Published session failed verification");Ok(())
}
pub fn commit_from_cli(paths:&Paths,folder_name:&str,stop:&Stop)->Result<()> {
    use std::io::Read;anyhow::ensure!(folder_name.starts_with("clean-")&&folder_name.chars().all(|c|c.is_ascii_alphanumeric()||c=='-'),"Invalid cleanup folder");
    let root=paths.app.join("repair-backups").canonicalize()?;let folder=root.join(folder_name);anyhow::ensure!(folder.parent()==Some(root.as_path())&&folder.canonicalize()?.parent()==Some(root.as_path()),"Invalid cleanup folder");
    let mut secret=[0u8;32];std::io::stdin().read_exact(&mut secret).context("Cleanup guard handshake missing")?;commit_prepared(paths,&folder,secret,stop,0)
}
fn run_guarded_commit(folder:&std::path::Path,journal:&CleanJournal,_fault:u8)->Result<()> {
    use std::os::windows::io::AsRawHandle;use std::os::windows::process::CommandExt;use std::process::{Command,Stdio};use std::io::Write;
    use windows::core::PCWSTR;use windows::Win32::Foundation::{CloseHandle,HANDLE};use windows::Win32::System::JobObjects::*;
    struct Job(HANDLE);impl Drop for Job{fn drop(&mut self){unsafe{let _=CloseHandle(self.0);}}}
    let job=Job(unsafe{CreateJobObjectW(None,PCWSTR::null())?});let mut info=JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();info.BasicLimitInformation.LimitFlags=JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    unsafe{SetInformationJobObject(job.0,JobObjectExtendedLimitInformation,&info as *const _ as *const _,std::mem::size_of_val(&info) as u32)?;}
    let mut command=Command::new(std::env::current_exe()?);
    #[cfg(test)] {command.args(["--ignored","--exact","clean::tests::native_commit_fixture_child"]).env("CODEX_CLEAN_FIXTURE_COMMIT",folder).env("CODEX_CLEAN_FIXTURE_FAULT",_fault.to_string());}
    #[cfg(not(test))] {command.arg("--clean-commit").arg(folder.file_name().context("Missing cleanup folder")?);}
    let mut child=command.creation_flags(0x08000000).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let process=HANDLE(child.as_raw_handle());if let Err(error)=unsafe{AssignProcessToJobObject(job.0,process)}{let _=child.kill();let _=child.wait();return Err(error.into())}
    child.stdin.take().context("Cleanup guard pipe unavailable")?.write_all(&journal.guard_secret)?;
    #[cfg(test)] if _fault==3 {std::process::exit(78);}
    let output=child.wait_with_output()?;anyhow::ensure!(output.status.success(),"Cleanup commit stopped (exit {:?}): {} {}",output.status.code(),String::from_utf8_lossy(&output.stderr).chars().take(400).collect::<String>(),String::from_utf8_lossy(&output.stdout).chars().take(700).collect::<String>());Ok(())
}
/// Run in a separate worker process so a Session Bar UI crash does not stop an
/// in-progress publication. The native session lock is held for both stores.
pub fn clean_session(paths:&Paths,id:&str,include_command_io:bool,codex:&[PathBuf],stop:&Stop)->Result<(u64,u64)> {
    clean_session_inner(paths,id,include_command_io,codex,stop,0)
}
fn clean_session_inner(paths:&Paths,id:&str,include_command_io:bool,codex:&[PathBuf],stop:&Stop,_fault:u8)->Result<(u64,u64)> {
    use std::io::Write;
    recover_all(paths,stop)?;let source=session_path(paths,id)?;
    let history=Connection::open_with_flags(paths.home.join("thread_history_1.sqlite"),OpenFlags::SQLITE_OPEN_READ_ONLY|OpenFlags::SQLITE_OPEN_NO_MUTEX)?;history.busy_timeout(Duration::from_secs(2))?;
    if !crate::reduce::confirmed_idle(&history,id)? {
        let projection:u64=history.query_row("SELECT count(*) FROM thread_history_projection_state WHERE thread_id=?",[id],|r|r.get(0))?;
        let turns:u64=history.query_row("SELECT count(*) FROM thread_turns WHERE thread_id=?",[id],|r|r.get(0))?;
        anyhow::ensure!(projection==0&&turns==0,"Session is active or its state is unknown");drop(history);
        let native=codex.first().context("No running Codex available for history reconstruction")?;
        rebuild_missing_projection(paths,id,&source,native,stop,||crate::desktop::confirm_unloaded_terminal(paths,id,stop))?;
    }else{drop(history);}
    {let _locks=crate::reduce::publication_lock(paths,id)?;}
    let (folder,old_digest,old_len)=backup_rollout(paths,id,&source,stop)?;
    struct Prepared {folder:PathBuf,retained:bool}
    impl Drop for Prepared {fn drop(&mut self){if !self.retained&&!self.folder.join("clean-journal.json").exists(){if let (Ok(folder),Some(parent))=(self.folder.canonicalize(),self.folder.parent().and_then(|p|p.canonicalize().ok())){if folder.parent()==Some(parent.as_path())&&folder.file_name().is_some_and(|v|v.to_string_lossy().starts_with("clean-")){let _=std::fs::remove_dir_all(folder);}}}}}
    let mut prepared=Prepared{folder:folder.clone(),retained:false};
    let candidate=folder.join("candidate.jsonl");let mut output=std::fs::OpenOptions::new().write(true).create_new(true).open(&candidate)?;
    let verified=flate2::read::GzDecoder::new(File::open(folder.join("rollout.jsonl.gz"))?);let (before,after,boundaries)=stage_rollout(&mut BufReader::new(verified),&mut output,include_command_io,stop)?;output.flush()?;output.sync_all()?;drop(output);
    anyhow::ensure!(before==old_len,"Session changed during cleaning");anyhow::ensure!(after<before,"No supported cleanable content found");let (new_digest,new_len)=hash_file(&candidate,stop)?;anyhow::ensure!(new_len==after,"Candidate size changed");
    anyhow::ensure!(!codex.is_empty(),"No running Codex available for compatibility checks");
    for executable in codex {preflight_native(paths,id,&candidate,executable,include_command_io,stop)?;}
    let result=(||->Result<()>{let _locks=crate::reduce::publication_lock(paths,id)?;let mut history=Connection::open(paths.home.join("thread_history_1.sqlite"))?;history.busy_timeout(Duration::from_secs(2))?;
        anyhow::ensure!(crate::reduce::confirmed_idle(&history,id)?,"Session became active");let current=session_path(paths,id)?;anyhow::ensure!(current.canonicalize()?==source.canonicalize()?,"Session path changed");
        let (current_digest,current_len)=hash_file(&source,stop)?;anyhow::ensure!(current_digest==old_digest&&current_len==old_len,"Session changed before publication");
        snapshot_projection(&mut history,id,&folder.join("projection.sqlite"))?;
        let journal=CleanJournal{version:1,thread_id:id.into(),old_len,new_len,old_digest,new_digest,boundaries,include_command_io,guard_secret:rand::random()};
        let adjacent=same_volume_candidate(&source,&folder)?;let mut final_candidate=std::fs::OpenOptions::new().write(true).create_new(true).open(&adjacent)?;
        if let Err(error)=copy_candidate_security(&source,&adjacent).and_then(|_|journal.write_new(&folder)){drop(final_candidate);let _=std::fs::remove_file(&adjacent);return Err(error)}
        #[cfg(test)] if _fault==4 {std::process::exit(78);}
        #[cfg(test)] if _fault==5 {use std::io::Read;let mut staged=File::open(&candidate)?.take(new_len/2);std::io::copy(&mut staged,&mut final_candidate)?;final_candidate.sync_all()?;std::process::exit(78);}
        let mut staged=File::open(&candidate)?;std::io::copy(&mut staged,&mut final_candidate)?;final_candidate.set_times(std::fs::FileTimes::new().set_modified(std::fs::metadata(&source)?.modified()?))?;final_candidate.sync_all()?;drop(final_candidate);
        let (digest,len)=hash_file(&adjacent,stop)?;anyhow::ensure!(digest==new_digest&&len==new_len,"Same-volume candidate failed verification");
        drop(history);let commit=run_guarded_commit(&folder,&journal,_fault);
        let recovery_stop=std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));let completed=recover_locked(paths,&folder,&source,&journal,&recovery_stop)?;
        if completed {Ok(())}else{match commit{Err(error)=>Err(error),Ok(())=>anyhow::bail!("Cleanup commit did not complete; original restored")}}
    })();
    if let Err(error)=result {if folder.join("clean-journal.json").is_file(){let recovery_stop=std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));if let Err(recovery)=recover_one(paths,&folder,&recovery_stop){anyhow::bail!("{error:#}; recovery pending: {recovery:#}")}}return Err(error)}
    prepared.retained=true;Ok((old_len,new_len))
}
fn rollout_record(value:&Value,out:&mut Analysis){
    let item=&value["payload"]["item"];
    if value["type"]!="event_msg" || value["payload"]["type"]!="item_completed" || item["type"]!="FileChange"{return}
    if let Some(changes)=item["changes"].as_object(){for (path,change) in changes {
        let size=change["unified_diff"].as_str().or_else(||change["content"].as_str()).map_or(0,|s|s.len()) as u64;
        if size==0{continue}out.rollout_changes+=1;out.rollout_diff_bytes+=size;let totals=out.rollout_extensions.entry(extension(path)).or_default();totals.0+=1;totals.1+=size;
        if generated_path(path){out.rollout_candidates+=1;out.rollout_candidate_bytes+=size;}
    }}
}
fn projection_item(value:&Value,out:&mut Analysis){
    if value["type"]!="fileChange"{return}
    if let Some(changes)=value["changes"].as_array(){for change in changes {
        let size=change["diff"].as_str().map_or(0,|s|s.len()) as u64;if size==0{continue}
        out.projection_changes+=1;out.projection_diff_bytes+=size;
        if change["path"].as_str().is_some_and(generated_path){out.projection_candidates+=1;out.projection_candidate_bytes+=size;}
    }}
}
// Drain oversized lines without allocating their full content.
fn next_record(reader:&mut impl BufRead,stop:&Stop)->Result<Option<(Option<Vec<u8>>,u64)>>{
    next_record_limited(reader,stop,RECORD_LIMIT)
}
fn next_record_limited(reader:&mut impl BufRead,stop:&Stop,limit:usize)->Result<Option<(Option<Vec<u8>>,u64)>>{
    let mut line=Vec::new();let mut oversized=false;let mut any=false;let mut total=0u64;
    loop {
        anyhow::ensure!(!stop.load(Ordering::Relaxed),"Analysis cancelled");
        let chunk=reader.fill_buf()?;if chunk.is_empty(){return if any{Ok(Some((if oversized{None}else{Some(line)},total)))}else{Ok(None)}}
        any=true;let n=chunk.iter().position(|b|*b==b'\n').map_or(chunk.len(),|p|p+1);let end=chunk[n-1]==b'\n';
        total+=n as u64;
        if !oversized {if line.len()+n>limit{oversized=true;line.clear()}else{line.extend_from_slice(&chunk[..n]);}}
        reader.consume(n);if end{return Ok(Some((if oversized{None}else{Some(line)},total)))}
    }
}
fn scan_rollout_progress(reader:&mut impl BufRead,stop:&Stop,out:&mut Analysis,progress:&mut dyn FnMut(u64))->Result<()>{
    let mut scanned=0u64;
    while let Some((record,size))=next_record(reader,stop)? {scanned=scanned.saturating_add(size);progress(scanned);match record {
        None=>{out.oversized_rollout_records+=1;out.oversized_rollout_bytes+=size;},
        Some(bytes)=>{if bytes.iter().all(u8::is_ascii_whitespace){continue}if bytes.last()!=Some(&b'\n'){out.estimate_incomplete=true;}let value:Value=serde_json::from_slice(&bytes).context("Invalid session record; analysis stopped")?;let total=out.rollout_record_types.entry(category(&value)).or_default();total.0+=1;total.1+=bytes.len() as u64;rollout_record(&value,out);
            let payload=&value["payload"];
            let candidate=(value["type"]=="event_msg"&&payload["type"]=="item_completed"&&matches!(payload["item"]["type"].as_str(),Some("FileChange"|"CommandExecution"))) || (value["type"]=="response_item"&&matches!(payload["type"].as_str(),Some("function_call_output"|"custom_tool_call_output"|"function_call")));
            if candidate {for with_io in [false,true] {let mut changed=value.clone();if clean_record(&mut changed,with_io)>0 {let new_len=serde_json::to_vec(&changed)?.len()+1;let delta=bytes.len() as i64-new_len as i64;let total=if with_io{&mut out.estimated_with_io_delta}else{&mut out.estimated_diff_delta};*total=total.saturating_add(delta);}}}
            if value["type"]=="response_item"&&matches!(payload["type"].as_str(),Some("function_call_output"|"custom_tool_call_output")){if let Some(text)=payload["output"].as_str().filter(|s|!s.is_empty()){out.rollout_tool_outputs+=1;out.rollout_tool_output_bytes+=text.len() as u64;}}
            if value["type"]=="response_item"&&payload["type"]=="function_call" {if let Some(args)=payload["arguments"].as_str().and_then(|s|serde_json::from_str::<Value>(s).ok()){for key in ["stdin","input","content","data"]{if let Some(text)=args[key].as_str().filter(|s|!s.is_empty()){out.rollout_tool_inputs+=1;out.rollout_tool_input_bytes+=text.len() as u64;}}}}
            if value["type"]=="event_msg"&&payload["type"]=="item_completed"&&payload["item"]["type"]=="CommandExecution"{for key in ["aggregated_output","stdout","stderr","formatted_output"]{if let Some(text)=payload["item"][key].as_str().filter(|s|!s.is_empty()){out.rollout_tool_outputs+=1;out.rollout_tool_output_bytes+=text.len() as u64;}}if let Some(text)=payload["item"]["interaction_input"].as_str().filter(|s|!s.is_empty()){out.rollout_tool_inputs+=1;out.rollout_tool_input_bytes+=text.len() as u64;}}
        }
    }}Ok(())
}
#[cfg(test)] fn scan_rollout(reader:&mut impl BufRead,stop:&Stop,out:&mut Analysis)->Result<()>{scan_rollout_progress(reader,stop,out,&mut |_|{})}
pub fn analyze(paths:&Paths,id:&str,stop:&Stop)->Result<Analysis>{analyze_progress(paths,id,stop,&mut |_,_|{})}
pub fn analyze_progress(paths:&Paths,id:&str,stop:&Stop,progress:&mut dyn FnMut(u64,u64))->Result<Analysis>{
    let state=Connection::open_with_flags(paths.home.join("state_5.sqlite"),OpenFlags::SQLITE_OPEN_READ_ONLY|OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    state.busy_timeout(Duration::from_millis(500))?;state.execute_batch("PRAGMA query_only=ON; PRAGMA temp_store=MEMORY")?;
    let raw:String=state.query_row("SELECT rollout_path FROM threads WHERE id=? AND agent_path IS NULL",[id],|r|r.get(0))?;
    let mut path=PathBuf::from(raw);let mut out=Analysis::default();
    if !path.is_file(){let mut name=path.as_os_str().to_os_string();name.push(".zst");path=PathBuf::from(name);}
    let absolute=path.canonicalize()?;
    anyhow::ensure!([paths.home.join("sessions"),paths.home.join("archived_sessions")].iter().filter_map(|p|p.canonicalize().ok()).any(|root|absolute.starts_with(root)),"Session file is outside local Codex storage");
    out.compressed=path.extension().is_some_and(|x|x=="zst");let f=File::open(&absolute)?;
    let total=if out.compressed{0}else{f.metadata()?.len()};progress(0,total);
    if out.compressed {scan_rollout_progress(&mut BufReader::new(zstd::stream::read::Decoder::new(f)?),stop,&mut out,&mut |n|progress(n,total))?}else{scan_rollout_progress(&mut BufReader::new(f),stop,&mut out,&mut |n|progress(n,total))?}
    let history=Connection::open_with_flags(paths.home.join("thread_history_1.sqlite"),OpenFlags::SQLITE_OPEN_READ_ONLY|OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    history.busy_timeout(Duration::from_millis(500))?;history.execute_batch("PRAGMA query_only=ON; PRAGMA temp_store=MEMORY")?;
    let mut query=history.prepare("SELECT CASE WHEN length(CAST(item_json AS BLOB))<=? THEN item_json ELSE NULL END,length(CAST(item_json AS BLOB)) FROM thread_items WHERE thread_id=? AND item_type='fileChange'")?;
    let mut rows=query.query(rusqlite::params![RECORD_LIMIT,id])?;
    while let Some(row)=rows.next()?{anyhow::ensure!(!stop.load(Ordering::Relaxed),"Analysis cancelled");let text:Option<String>=row.get(0)?;if let Some(text)=text{let value:Value=serde_json::from_str(&text).context("Invalid file-change projection; analysis stopped")?;projection_item(&value,&mut out)}else{out.oversized_projection_items+=1;out.oversized_projection_bytes+=row.get::<_,u64>(1)?;}}
    Ok(out)
}

#[cfg(test)]mod tests{
    use super::*;use std::sync::{Arc,atomic::AtomicBool};use serde_json::json;
    #[test]fn installing_projection_refuses_to_replace_existing_thread_rows(){
        let paths=Paths::discover().unwrap();let root=paths.runtime.join(format!("install-projection-{}-{}",std::process::id(),crate::now().to_bits()));std::fs::create_dir(&root).unwrap();let snapshot=root.join("projection.sqlite");
        let mut built=Connection::open_in_memory().unwrap();built.execute_batch("CREATE TABLE thread_history_projection_state(thread_id TEXT PRIMARY KEY,next_rollout_byte_offset INTEGER);CREATE TABLE thread_turns(thread_id TEXT,turn_id TEXT);CREATE TABLE thread_items(thread_id TEXT,item_id TEXT);INSERT INTO thread_history_projection_state VALUES('a',100);INSERT INTO thread_turns VALUES('a','finished');INSERT INTO thread_items VALUES('a','msg')").unwrap();snapshot_projection(&mut built,"a",&snapshot).unwrap();
        let mut live=Connection::open_in_memory().unwrap();live.execute_batch("CREATE TABLE thread_history_projection_state(thread_id TEXT PRIMARY KEY,next_rollout_byte_offset INTEGER);CREATE TABLE thread_turns(thread_id TEXT,turn_id TEXT);CREATE TABLE thread_items(thread_id TEXT,item_id TEXT);INSERT INTO thread_items VALUES('b','untouched')").unwrap();install_empty_projection(&mut live,"a",&snapshot).unwrap();assert_eq!(live.query_row("SELECT count(*) FROM thread_items",[],|r|r.get::<_,u64>(0)).unwrap(),2);assert!(install_empty_projection(&mut live,"a",&snapshot).is_err());assert_eq!(live.query_row("SELECT count(*) FROM thread_items",[],|r|r.get::<_,u64>(0)).unwrap(),2);
    }
    #[test]
    #[ignore = "Explicit native Codex test in a separate workspace tmp fixture"]
    fn native_missing_projection_is_rebuilt_without_changing_rollout(){
        let root=PathBuf::from(std::env::var_os("CODEX_CLEAN_MISSING_FIXTURE").expect("fixture required")).canonicalize().unwrap();let workspace=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();assert!(root.starts_with(workspace.join("tmp").canonicalize().unwrap()));
        let home=root.join("home");let id=std::fs::read_to_string(root.join("thread-id.txt")).unwrap();let id=id.trim();let state=Connection::open(home.join("state_5.sqlite")).unwrap();let source=PathBuf::from(state.query_row("SELECT rollout_path FROM threads WHERE id=?",[id],|r|r.get::<_,String>(0)).unwrap());assert!(source.canonicalize().unwrap().starts_with(home.canonicalize().unwrap()));drop(state);
        let mut history=Connection::open(home.join("thread_history_1.sqlite")).unwrap();let schema=projection_schema(&history).unwrap();{let tx=history.transaction().unwrap();for (name,_) in &schema {tx.execute(&format!("DELETE FROM \"{name}\" WHERE thread_id=?"),[id]).unwrap();}tx.commit().unwrap();}drop(history);
        let app=root.join("cleanup-fault-app");std::fs::create_dir_all(&app).unwrap();let paths=Paths{home,app,runtime:root.clone(),instance:String::new()};let stop=Arc::new(AtomicBool::new(false));let before=hash_file(&source,&stop).unwrap();let exe=PathBuf::from(std::env::var_os("CODEX_CLEAN_NATIVE_EXE").expect("native executable required"));
        assert!(rebuild_missing_projection(&paths,id,&source,&exe,&stop,||Err(anyhow::anyhow!("active"))).is_err());assert_eq!(hash_file(&source,&stop).unwrap(),before);
        let mut checks=0;rebuild_missing_projection(&paths,id,&source,&exe,&stop,||{checks+=1;Ok(())}).unwrap();assert_eq!(checks,2);assert_eq!(hash_file(&source,&stop).unwrap(),before);let db=Connection::open(paths.home.join("thread_history_1.sqlite")).unwrap();assert!(crate::reduce::confirmed_idle(&db,id).unwrap());assert_eq!(db.query_row("SELECT next_rollout_byte_offset FROM thread_history_projection_state WHERE thread_id=?",[id],|r|r.get::<_,u64>(0)).unwrap(),before.1);drop(db);
        let (old,new)=clean_session(&paths,id,true,std::slice::from_ref(&exe),&stop).unwrap();assert_eq!(old,before.1);assert!(new<old);let db=Connection::open(paths.home.join("thread_history_1.sqlite")).unwrap();assert_eq!(db.query_row("SELECT next_rollout_byte_offset FROM thread_history_projection_state WHERE thread_id=?",[id],|r|r.get::<_,u64>(0)).unwrap(),new);recover_all(&paths,&stop).unwrap();
    }
    #[test]fn analysis_progress_counts_rollout_bytes(){
        let input=b"{\"type\":\"x\"}\n{\"type\":\"y\"}\n";let stop=Arc::new(AtomicBool::new(false));let mut out=Analysis::default();let mut seen=Vec::new();
        scan_rollout_progress(&mut BufReader::new(&input[..]),&stop,&mut out,&mut |n|seen.push(n)).unwrap();
        assert_eq!(seen.len(),2);assert!(seen[0]<seen[1]);assert_eq!(*seen.last().unwrap(),input.len() as u64);
    }
    #[test]fn estimated_reduction_matches_staged_json_for_both_modes(){
        let entries=[json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"FileChange","changes":{"capture.vcd":{"unified_diff":"a".repeat(500)}}}}}),json!({"type":"response_item","payload":{"type":"function_call_output","output":"b".repeat(800)}})];
        let input=entries.iter().map(|v|format!("{v}\n")).collect::<String>();let stop=Arc::new(AtomicBool::new(false));let mut analysis=Analysis::default();scan_rollout(&mut BufReader::new(input.as_bytes()),&stop,&mut analysis).unwrap();
        for io in [false,true] {let mut output=Vec::new();let (before,after,_)=stage_rollout(&mut BufReader::new(input.as_bytes()),&mut output,io,&stop).unwrap();assert_eq!(analysis.file_reduction(io),Some(before-after));}
        assert!(analysis.file_reduction(true)>analysis.file_reduction(false));analysis.estimate_incomplete=true;assert_eq!(analysis.file_reduction(true),None);
    }
    #[test]fn cleaning_preserves_commands_and_defaults_to_generated_file_diffs(){
        let diff=json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"FileChange","changes":{"capture.vcd":{"unified_diff":"a".repeat(200)},"src/main.rs":{"unified_diff":"b".repeat(200)}}}}});
        let output=json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"x".repeat(200)}});
        let call=json!({"type":"response_item","payload":{"type":"function_call","name":"shell_command","call_id":"call-1","arguments":json!({"command":"echo hi","stdin":"z".repeat(200)}).to_string()}});
        let mut d=diff.clone();assert!(clean_record(&mut d,false)>0);assert_eq!(d["payload"]["item"]["changes"]["src/main.rs"],diff["payload"]["item"]["changes"]["src/main.rs"]);
        let mut o=output.clone();let mut c=call.clone();assert_eq!(clean_record(&mut o,false),0);assert_eq!(clean_record(&mut c,false),0);assert_eq!(o,output);assert_eq!(c,call);
        assert!(clean_record(&mut o,true)>0);assert!(clean_record(&mut c,true)>0);assert_eq!(o["payload"]["call_id"],"call-1");let args:Value=serde_json::from_str(c["payload"]["arguments"].as_str().unwrap()).unwrap();assert_eq!(args["command"],"echo hi");assert!(args["stdin"].as_str().unwrap().contains("Content cleaned"));
    }
    #[test]fn cleaning_command_event_replaces_each_payload_copy_without_touching_command(){
        let mut event=json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","id":"c1","command":["echo test"],"stdout":"a".repeat(100),"stderr":"b".repeat(100),"aggregated_output":"c".repeat(100),"formatted_output":"d".repeat(100),"interaction_input":"e".repeat(100)}}});
        let original=event.clone();assert_eq!(clean_record(&mut event,false),0);assert_eq!(event,original);assert!(clean_record(&mut event,true)>0);
        let item=&event["payload"]["item"];assert_eq!(item["command"][0],"echo test");assert_eq!(item["id"],"c1");for key in ["stdout","stderr","aggregated_output","formatted_output","interaction_input"]{assert!(item[key].as_str().unwrap().contains("Content cleaned"));}
    }
    #[test]fn staging_maps_record_boundaries_and_preserves_unmodified_bytes(){
        let keep="{ \"type\": \"session_meta\", \"payload\": {} }\n";
        let call=json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"x".repeat(200)}}).to_string()+"\n";
        let end="{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\"}}\n";
        let input=format!("{keep}{call}{end}");let mut out=Vec::new();let stop=Arc::new(AtomicBool::new(false));
        let (before,after,map)=stage_rollout(&mut BufReader::new(input.as_bytes()),&mut out,true,&stop).unwrap();let result=String::from_utf8(out).unwrap();
        assert_eq!(before,input.len() as u64);assert_eq!(after,result.len() as u64);assert!(after<before);assert!(result.starts_with(keep));assert!(result.ends_with(end));assert!(result.contains("Content cleaned"));
        assert_eq!(map[1],(keep.len() as u64,keep.len() as u64));assert_eq!(map.last(),Some(&(before,after)));
        let new_start=map[2].1 as usize;assert_eq!(&result[new_start..],end);
    }
    #[test]fn staging_rejects_incomplete_or_oversized_records(){
        let stop=Arc::new(AtomicBool::new(false));let mut out=Vec::new();assert!(stage_rollout(&mut BufReader::new(&b"{}"[..]),&mut out,true,&stop).is_err());
        let mut data=vec![b'x';CLEAN_RECORD_LIMIT+1];data.push(b'\n');out.clear();assert!(stage_rollout(&mut BufReader::new(data.as_slice()),&mut out,true,&stop).is_err());assert!(out.is_empty());
    }
    #[test]fn projection_offsets_and_items_update_together_and_rollback_on_invalid_boundary(){
        let mut db=Connection::open_in_memory().unwrap();db.execute_batch("CREATE TABLE thread_history_projection_state(thread_id TEXT PRIMARY KEY,next_rollout_byte_offset INTEGER);CREATE TABLE thread_turns(thread_id TEXT,turn_id TEXT,rollout_byte_offset INTEGER,rollout_end_byte_offset INTEGER);CREATE TABLE thread_items(thread_id TEXT,turn_id TEXT,item_id TEXT,item_type TEXT,item_json TEXT);INSERT INTO thread_history_projection_state VALUES('a',100);INSERT INTO thread_turns VALUES('a','t',20,100)").unwrap();
        let item=json!({"type":"commandExecution","command":"echo hello","aggregatedOutput":"x".repeat(100)}).to_string();db.execute("INSERT INTO thread_items VALUES('a','t','c','commandExecution',?)",[&item]).unwrap();
        assert!(stage_projection(&mut db,"a",100,50,&[(0,0),(100,50)],true).is_err());assert_eq!(db.query_row("SELECT next_rollout_byte_offset FROM thread_history_projection_state WHERE thread_id='a'",[],|r|r.get::<_,u64>(0)).unwrap(),100);assert_eq!(db.query_row("SELECT item_json FROM thread_items",[],|r|r.get::<_,String>(0)).unwrap(),item);
        stage_projection(&mut db,"a",100,50,&[(0,0),(20,10),(100,50)],true).unwrap();let offsets:(u64,u64)=db.query_row("SELECT rollout_byte_offset,rollout_end_byte_offset FROM thread_turns",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();assert_eq!(offsets,(10,50));let text:String=db.query_row("SELECT item_json FROM thread_items",[],|r|r.get(0)).unwrap();let value:Value=serde_json::from_str(&text).unwrap();assert_eq!(value["command"],"echo hello");assert!(value["aggregatedOutput"].as_str().unwrap().contains("Content cleaned"));
    }
    #[test]fn recovery_rejects_unrecognized_files_and_advanced_projection(){
        let (old,new,other)=([1;32],[2;32],[3;32]);
        assert_eq!(recovery_action(old,new,old,100,50,100),RecoveryAction::AbortUnpublished);
        assert_eq!(recovery_action(old,new,old,100,50,50),RecoveryAction::PublishStaged);
        assert_eq!(recovery_action(old,new,new,100,50,100),RecoveryAction::FinishProjection);
        assert_eq!(recovery_action(old,new,new,100,50,50),RecoveryAction::Complete);
        assert_eq!(recovery_action(old,new,other,100,50,50),RecoveryAction::Manual);
        assert_eq!(recovery_action(old,new,new,100,50,120),RecoveryAction::Manual);
        assert_eq!(recovery_action(old,new,new,100,100,100),RecoveryAction::Manual);
    }
    #[test]fn clean_journal_requires_a_valid_monotonic_mapping(){
        let mut j=CleanJournal{version:1,thread_id:"11111111-1111-4111-8111-111111111111".into(),old_len:100,new_len:50,old_digest:[1;32],new_digest:[2;32],boundaries:vec![(0,0),(20,10),(100,50)],include_command_io:false,guard_secret:[3;32]};assert!(j.validate().is_ok());j.boundaries[1]=(20,60);assert!(j.validate().is_err());j.boundaries=vec![(0,0),(100,50)];j.thread_id="../invalid".into();assert!(j.validate().is_err());
    }
    #[test]fn clean_journal_is_durable_and_refuses_overwrite(){
        let paths=Paths::discover().unwrap();let folder=paths.runtime.join(format!("journal-test-{}-{}",std::process::id(),crate::now().to_bits()));assert!(folder.starts_with(&paths.runtime));std::fs::create_dir(&folder).unwrap();
        let j=CleanJournal{version:1,thread_id:"11111111-1111-4111-8111-111111111111".into(),old_len:100,new_len:50,old_digest:[1;32],new_digest:[2;32],boundaries:vec![(0,0),(100,50)],include_command_io:true,guard_secret:[3;32]};j.write_new(&folder).unwrap();assert!(j.write_new(&folder).is_err());let restored=CleanJournal::read(&folder).unwrap();assert_eq!(restored.boundaries,j.boundaries);assert!(restored.include_command_io);std::fs::remove_dir_all(folder).unwrap();
    }
    #[test]fn preflight_detects_changed_or_missing_conversation_items(){
        fn db()->Connection{let c=Connection::open_in_memory().unwrap();c.execute_batch("CREATE TABLE thread_items(thread_id TEXT,turn_id TEXT,item_id TEXT,item_type TEXT,item_json TEXT,rollout_ordinal INTEGER);INSERT INTO thread_items VALUES('a','t','u','userMessage','{\"text\":\"keep\"}',1)").unwrap();let item=json!({"command":"keep","aggregatedOutput":"x".repeat(100)}).to_string();c.execute("INSERT INTO thread_items VALUES('a','t','c','commandExecution',?,2)",[item]).unwrap();c}
        let a=db();let b=db();let item=json!({"command":"keep","aggregatedOutput":"[Content cleaned by Codex Session Bar]"}).to_string();b.execute("UPDATE thread_items SET item_json=? WHERE item_id='c'",[item]).unwrap();assert!(verify_unchanged_projection(&a,&b,"a",true).is_ok());assert!(verify_unchanged_projection(&a,&b,"a",false).is_err());
        b.execute("UPDATE thread_items SET item_json='{\"text\":\"different\"}' WHERE item_id='u'",[]).unwrap();assert!(verify_unchanged_projection(&a,&b,"a",true).is_err());b.execute("DELETE FROM thread_items WHERE item_id='u'",[]).unwrap();assert!(verify_unchanged_projection(&a,&b,"a",true).is_err());
    }
    #[test]fn backup_verifies_content_without_modifying_source(){
        let mut paths=Paths::discover().unwrap();let root=paths.runtime.join(format!("clean-backup-test-{}-{}",std::process::id(),crate::now().to_bits()));assert!(root.starts_with(&paths.runtime));std::fs::create_dir(&root).unwrap();paths.app=root.clone();let source=root.join("source.jsonl");let data=b"{\"message\":\"synthetic\"}\n";std::fs::write(&source,data).unwrap();let stop=Arc::new(AtomicBool::new(false));let (folder,_,len)=backup_rollout(&paths,"11111111-1111-4111-8111-111111111111",&source,&stop).unwrap();assert_eq!(len,data.len() as u64);assert_eq!(std::fs::read(&source).unwrap(),data);assert!(folder.join("rollout.jsonl.gz").is_file());std::fs::remove_dir_all(root).unwrap();
    }
    #[test]fn failed_same_volume_replacement_keeps_original_and_candidate(){
        use std::os::windows::fs::OpenOptionsExt;let paths=Paths::discover().unwrap();let root=paths.runtime.join(format!("replace-test-{}-{}",std::process::id(),crate::now().to_bits()));assert!(root.starts_with(&paths.runtime));std::fs::create_dir(&root).unwrap();let source=root.join("source.jsonl");let candidate=root.join("candidate.jsonl");std::fs::write(&source,b"original").unwrap();std::fs::write(&candidate,b"candidate").unwrap();
        let locked=std::fs::OpenOptions::new().read(true).share_mode(3).open(&source).unwrap();let result=replace_atomic(&source,&candidate);assert!(result.is_err());drop(locked);assert_eq!(std::fs::read(&source).unwrap(),b"original");assert_eq!(std::fs::read(&candidate).unwrap(),b"candidate");std::fs::remove_dir_all(root).unwrap();
    }
    #[test]fn projection_restore_is_thread_scoped_and_rejects_schema_changes(){
        let paths=Paths::discover().unwrap();let root=paths.runtime.join(format!("projection-backup-test-{}-{}",std::process::id(),crate::now().to_bits()));assert!(root.starts_with(&paths.runtime));std::fs::create_dir(&root).unwrap();let snapshot=root.join("projection.sqlite");
        let mut db=Connection::open_in_memory().unwrap();db.execute_batch("CREATE TABLE thread_history_projection_state(thread_id TEXT PRIMARY KEY,next_rollout_byte_offset INTEGER);CREATE TABLE thread_turns(thread_id TEXT,turn_id TEXT);CREATE TABLE thread_items(thread_id TEXT,item_id TEXT);INSERT INTO thread_history_projection_state VALUES('a',100),('b',200);INSERT INTO thread_turns VALUES('a','old-a'),('b','old-b');INSERT INTO thread_items VALUES('a','item-a'),('b','item-b')").unwrap();
        snapshot_projection(&mut db,"a",&snapshot).unwrap();db.execute_batch("UPDATE thread_history_projection_state SET next_rollout_byte_offset=50 WHERE thread_id='a';DELETE FROM thread_items WHERE thread_id='a';INSERT INTO thread_items VALUES('b','new-b')").unwrap();restore_projection(&mut db,"a",&snapshot).unwrap();
        assert_eq!(db.query_row("SELECT next_rollout_byte_offset FROM thread_history_projection_state WHERE thread_id='a'",[],|r|r.get::<_,u64>(0)).unwrap(),100);assert_eq!(db.query_row("SELECT count(*) FROM thread_items WHERE thread_id='b'",[],|r|r.get::<_,u64>(0)).unwrap(),2);
        db.execute_batch("ALTER TABLE thread_items ADD COLUMN extra TEXT").unwrap();assert!(restore_projection(&mut db,"a",&snapshot).is_err());assert_eq!(db.query_row("SELECT count(*) FROM thread_items WHERE thread_id='b'",[],|r|r.get::<_,u64>(0)).unwrap(),2);
        db.execute_batch("CREATE TRIGGER unexpected AFTER DELETE ON thread_items BEGIN DELETE FROM thread_turns; END").unwrap();assert!(projection_schema(&db).is_err());drop(db);std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[ignore = "Explicit native Codex fixture; CODEX_CLEAN_NATIVE_FIXTURE must be inside workspace tmp"]
    fn native_isolated_fixture_stages_cleaned_file_and_projection(){
        let root=std::path::PathBuf::from(std::env::var_os("CODEX_CLEAN_NATIVE_FIXTURE").expect("fixture required")).canonicalize().unwrap();
        let workspace=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();let tmp=workspace.join("tmp").canonicalize().unwrap();
        assert!(root.starts_with(&tmp)&&root.file_name().unwrap()=="codex-native-recovery-20261031");
        let home=root.join("home").canonicalize().unwrap();assert!(home.starts_with(&root));let id=std::fs::read_to_string(root.join("thread-id.txt")).unwrap();let id=id.trim();
        let state=Connection::open_with_flags(home.join("state_5.sqlite"),OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();let path:PathBuf=PathBuf::from(state.query_row("SELECT rollout_path FROM threads WHERE id=?",[id],|r|r.get::<_,String>(0)).unwrap());assert!(path.canonicalize().unwrap().starts_with(&home));
        let original=std::fs::read(&path).unwrap();assert!(String::from_utf8_lossy(&original).contains("SYNTHETIC_LARGE_LOG"));
        let stop=Arc::new(AtomicBool::new(false));let mut staged=Vec::new();let (before,after,map)=stage_rollout(&mut BufReader::new(original.as_slice()),&mut staged,true,&stop).unwrap();assert!(after<before);assert!(!String::from_utf8_lossy(&staged).contains("SYNTHETIC_LARGE_LOG"));
        if let Some(exe)=std::env::var_os("CODEX_CLEAN_NATIVE_EXE") {let candidate=root.join("candidate-for-preflight.jsonl");std::fs::write(&candidate,&staged).unwrap();let mut paths=Paths::discover().unwrap();paths.home=home.clone();paths.runtime=root.clone();preflight_native(&paths,id,&candidate,&PathBuf::from(exe),true,&stop).unwrap();std::fs::remove_file(candidate).unwrap();}
        let mut history=Connection::open(home.join("thread_history_1.sqlite")).unwrap();let snapshot=root.join(format!("fixture-projection-{}-{}.sqlite",std::process::id(),crate::now().to_bits()));snapshot_projection(&mut history,id,&snapshot).unwrap();
        stage_projection(&mut history,id,before,after,&map,true).unwrap();std::fs::write(&path,&staged).unwrap();
        // Simulate a failed publication after both stores changed; restore only
        // this thread while the isolated fixture has no active writer.
        std::fs::write(&path,&original).unwrap();restore_projection(&mut history,id,&snapshot).unwrap();let restored:u64=history.query_row("SELECT next_rollout_byte_offset FROM thread_history_projection_state WHERE thread_id=?",[id],|r|r.get(0)).unwrap();assert_eq!(restored,before);assert_eq!(std::fs::read(&path).unwrap(),original);
        stage_projection(&mut history,id,before,after,&map,true).unwrap();std::fs::write(&path,&staged).unwrap();std::fs::remove_file(snapshot).unwrap();
        let offset:u64=history.query_row("SELECT next_rollout_byte_offset FROM thread_history_projection_state WHERE thread_id=?",[id],|r|r.get(0)).unwrap();assert_eq!(offset,after);
    }
    #[test]
    #[ignore = "Explicit native Codex fixture; runs child-process fault injection under workspace tmp"]
    fn native_cleanup_fault_recovery(){
        let root=PathBuf::from(std::env::var_os("CODEX_CLEAN_NATIVE_FIXTURE").expect("fixture required")).canonicalize().unwrap();let workspace=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();assert!(root.starts_with(workspace.join("tmp").canonicalize().unwrap())&&root.file_name().unwrap()=="codex-native-recovery-20261031");
        let home=root.join("home").canonicalize().unwrap();let id=std::fs::read_to_string(root.join("thread-id.txt")).unwrap();let id=id.trim();let exe=PathBuf::from(std::env::var_os("CODEX_CLEAN_NATIVE_EXE").expect("native Codex required"));let stop=Arc::new(AtomicBool::new(false));let mut paths=Paths::discover().unwrap();paths.home=home;paths.runtime=root.clone();paths.app=root.join("cleanup-fault-app");std::fs::create_dir_all(&paths.app).unwrap();
        let source=session_path(&paths,id).unwrap();let original=std::fs::read(&source).unwrap();assert!(String::from_utf8_lossy(&original).contains("SYNTHETIC_LARGE_LOG"));let old_len=original.len() as u64;let snapshot=root.join(format!("fault-baseline-{}-{}.sqlite",std::process::id(),crate::now().to_bits()));
        {let mut db=Connection::open(paths.home.join("thread_history_1.sqlite")).unwrap();snapshot_projection(&mut db,id,&snapshot).unwrap();}
        assert!(clean_session(&paths,id,false,std::slice::from_ref(&exe),&stop).is_err());assert_eq!(std::fs::read(&source).unwrap(),original);
        for fault in [1u8,2u8] {
            std::fs::write(&source,&original).unwrap();{let mut db=Connection::open(paths.home.join("thread_history_1.sqlite")).unwrap();restore_projection(&mut db,id,&snapshot).unwrap();}
            let result=clean_session_inner(&paths,id,true,std::slice::from_ref(&exe),&stop,fault);if fault==1{let error=result.unwrap_err();assert!(format!("{error:#}").contains("77"),"fault did not reach the commit child: {error:#}")}else{assert!(result.is_ok(),"fault {fault}: {result:?}")}
            recover_all(&paths,&stop).unwrap();let data=std::fs::read(&source).unwrap();let db=Connection::open(paths.home.join("thread_history_1.sqlite")).unwrap();let offset:u64=db.query_row("SELECT next_rollout_byte_offset FROM thread_history_projection_state WHERE thread_id=?",[id],|r|r.get(0)).unwrap();assert_eq!(offset,data.len() as u64);
            if fault==1{assert_eq!(data,original);assert_eq!(offset,old_len)}else{assert!(data.len()<original.len());assert!(!String::from_utf8_lossy(&data).contains("SYNTHETIC_LARGE_LOG"));}
        }
        for fault in [3u8,4u8,5u8] {
            std::fs::write(&source,&original).unwrap();{let mut db=Connection::open(paths.home.join("thread_history_1.sqlite")).unwrap();restore_projection(&mut db,id,&snapshot).unwrap();}
            let guard=std::process::Command::new(std::env::current_exe().unwrap()).args(["--ignored","--exact","clean::tests::native_guard_fixture_child"]).env("CODEX_CLEAN_GUARD_FAULT",fault.to_string()).status().unwrap();assert_eq!(guard.code(),Some(78));
            let mut recovered=false;for _ in 0..100 {if recover_all(&paths,&stop).is_ok(){recovered=true;break}std::thread::sleep(Duration::from_millis(100));}assert!(recovered);let bytes=std::fs::read(&source).unwrap();let db=Connection::open(paths.home.join("thread_history_1.sqlite")).unwrap();let offset:u64=db.query_row("SELECT next_rollout_byte_offset FROM thread_history_projection_state WHERE thread_id=?",[id],|r|r.get(0)).unwrap();assert_eq!(offset,bytes.len() as u64);assert!(bytes==original||!String::from_utf8_lossy(&bytes).contains("SYNTHETIC_LARGE_LOG"));drop(db);
        }
        std::fs::write(&source,&original).unwrap();{let mut db=Connection::open(paths.home.join("thread_history_1.sqlite")).unwrap();restore_projection(&mut db,id,&snapshot).unwrap();}
        let (before,after)=clean_session(&paths,id,true,std::slice::from_ref(&exe),&stop).unwrap();assert_eq!(before,old_len);assert!(after<before);assert_eq!(std::fs::metadata(&source).unwrap().len(),after);recover_all(&paths,&stop).unwrap();
        std::fs::remove_file(snapshot).unwrap();
    }
    #[test]
    #[ignore = "Commit child used only by isolated native cleanup fault tests"]
    fn native_commit_fixture_child(){
        use std::io::Read;let root=PathBuf::from(std::env::var_os("CODEX_CLEAN_NATIVE_FIXTURE").unwrap()).canonicalize().unwrap();let workspace=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();assert!(root.starts_with(workspace.join("tmp").canonicalize().unwrap())&&root.file_name().unwrap()=="codex-native-recovery-20261031");
        let folder=PathBuf::from(std::env::var_os("CODEX_CLEAN_FIXTURE_COMMIT").unwrap()).canonicalize().unwrap();assert!(folder.starts_with(&root));let mut paths=Paths::discover().unwrap();paths.home=root.join("home").canonicalize().unwrap();paths.runtime=root.clone();paths.app=root.join("cleanup-fault-app");let stop=Arc::new(AtomicBool::new(false));let mut secret=[0u8;32];std::io::stdin().read_exact(&mut secret).unwrap();let fault=std::env::var("CODEX_CLEAN_FIXTURE_FAULT").unwrap().parse::<u8>().unwrap();commit_prepared(&paths,&folder,secret,&stop,fault).unwrap();
    }
    #[test]
    #[ignore = "Guard child used only by isolated native cleanup fault tests"]
    fn native_guard_fixture_child(){
        let root=PathBuf::from(std::env::var_os("CODEX_CLEAN_NATIVE_FIXTURE").unwrap()).canonicalize().unwrap();let workspace=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();assert!(root.starts_with(workspace.join("tmp").canonicalize().unwrap())&&root.file_name().unwrap()=="codex-native-recovery-20261031");
        let id=std::fs::read_to_string(root.join("thread-id.txt")).unwrap();let exe=PathBuf::from(std::env::var_os("CODEX_CLEAN_NATIVE_EXE").unwrap());let stop=Arc::new(AtomicBool::new(false));let mut paths=Paths::discover().unwrap();paths.home=root.join("home").canonicalize().unwrap();paths.runtime=root.clone();paths.app=root.join("cleanup-fault-app");let fault=std::env::var("CODEX_CLEAN_GUARD_FAULT").unwrap().parse::<u8>().unwrap();let _=clean_session_inner(&paths,id.trim(),true,std::slice::from_ref(&exe),&stop,fault);panic!("Guard fault injection did not terminate the child");
    }
    #[test]fn record_breakdown_counts_whole_records_once_and_keeps_unclassified_separate(){
        let mut a=Analysis::default();a.rollout_record_types.insert("event_msg/item_completed/FileChange".into(),(2,100));a.rollout_record_types.insert("response_item/custom_tool_call_output".into(),(1,200));a.rollout_record_types.insert("response_item/reasoning".into(),(1,300));a.rollout_record_types.insert("response_item/message".into(),(1,400));a.rollout_record_types.insert("other".into(),(1,500));a.oversized_rollout_bytes=600;a.rollout_candidate_bytes=40;
        assert_eq!(a.record_breakdown(),[100,200,0,300,400,500,600]);
    }
    #[test]fn generated_diff_analysis_keeps_source_code_separate_and_counts_both_stores(){
        let mut out=Analysis::default();let sample=json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"FileChange","changes":{"x/sim.VCD":{"unified_diff":"12345"},"src/module.v":{"unified_diff":"abc"},"tmp/run.log":{"content":"1234567"}}}}});rollout_record(&sample,&mut out);
        projection_item(&json!({"type":"fileChange","changes":[{"path":"x/sim.VCD","diff":"12345"},{"path":"src/module.v","diff":"abc"},{"path":"tmp/run.log","diff":"1234567"}]}),&mut out);
        assert_eq!((out.rollout_changes,out.rollout_diff_bytes,out.rollout_candidates,out.rollout_candidate_bytes),(3,15,2,12));assert_eq!((out.projection_changes,out.projection_diff_bytes,out.projection_candidates,out.projection_candidate_bytes),(3,15,2,12));assert!(!generated_path("src/file.v"));
    }
    #[test]fn oversized_record_is_counted_without_buffering_and_following_record_is_read(){
        let mut data=vec![b'x';RECORD_LIMIT+1];data.push(b'\n');data.extend_from_slice(b"{}\n");let stop=Arc::new(AtomicBool::new(false));let mut input=BufReader::new(data.as_slice());let mut out=Analysis::default();scan_rollout(&mut input,&stop,&mut out).unwrap();assert_eq!(out.oversized_rollout_records,1);assert_eq!(out.oversized_rollout_bytes,RECORD_LIMIT as u64+2);
    }
    #[test]fn preview_counts_only_recognized_string_output_without_mutating_input(){
        let lines=[json!({"type":"response_item","payload":{"type":"function_call","arguments":"{\"command\":\"echo test\",\"stdin\":\"hello\"}"}}),json!({"type":"response_item","payload":{"type":"function_call_output","output":"123456"}}),json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","aggregated_output":"1234","stdout":"1234","interaction_input":"abc"}}}),json!({"type":"response_item","payload":{"type":"custom_tool_call_output","output":{"unknown":"shape"}}})];
        let data=lines.iter().map(|v|format!("{v}\n")).collect::<String>();let before=data.clone();let mut out=Analysis::default();let stop=Arc::new(AtomicBool::new(false));scan_rollout(&mut BufReader::new(data.as_bytes()),&stop,&mut out).unwrap();assert_eq!((out.rollout_tool_outputs,out.rollout_tool_output_bytes),(3,14));assert_eq!((out.rollout_tool_inputs,out.rollout_tool_input_bytes),(2,8));assert_eq!(data,before);
    }
    #[test]fn analysis_reads_plain_and_compressed_synthetic_sessions_without_modifying_them(){
        let mut paths=Paths::discover().unwrap();let root=paths.runtime.join(format!("diff-analysis-test-{}-{}",std::process::id(),crate::now().to_bits()));assert!(root.starts_with(&paths.runtime));std::fs::create_dir_all(root.join("sessions")).unwrap();paths.home=root.clone();
        let id="11111111-1111-4111-8111-111111111111";let path=root.join("sessions").join("rollout.jsonl");
        let record=json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"FileChange","changes":{"run/trace.vcd":{"unified_diff":"waveform"},"src/main.rs":{"unified_diff":"code"}}}}});let data=format!("{}\n",record);std::fs::write(&path,&data).unwrap();
        let state=Connection::open(root.join("state_5.sqlite")).unwrap();state.execute_batch("CREATE TABLE threads(id TEXT,rollout_path TEXT,agent_path TEXT)").unwrap();state.execute("INSERT INTO threads VALUES(?,?,NULL)",rusqlite::params![id,path.to_string_lossy()]).unwrap();
        let history=Connection::open(root.join("thread_history_1.sqlite")).unwrap();history.execute_batch("CREATE TABLE thread_items(thread_id TEXT,item_type TEXT,item_json TEXT)").unwrap();let projected=json!({"type":"fileChange","changes":[{"path":"run/trace.vcd","diff":"waveform"},{"path":"src/main.rs","diff":"code"}]}).to_string();history.execute("INSERT INTO thread_items VALUES(?,'fileChange',?)",rusqlite::params![id,projected]).unwrap();
        let stop=Arc::new(AtomicBool::new(false));let result=analyze(&paths,id,&stop).unwrap();assert_eq!((result.rollout_candidates,result.rollout_candidate_bytes,result.projection_candidates,result.projection_candidate_bytes),(1,8,1,8));assert_eq!(result.rollout_record_types.get("event_msg/item_completed/FileChange").map(|v|v.0),Some(1));assert_eq!(std::fs::read_to_string(&path).unwrap(),data);
        let zipped=PathBuf::from(format!("{}.zst",path.display()));std::fs::write(&zipped,zstd::stream::encode_all(data.as_bytes(),1).unwrap()).unwrap();std::fs::remove_file(&path).unwrap();let compressed=analyze(&paths,id,&stop).unwrap();assert!(compressed.compressed);assert_eq!(compressed.rollout_candidate_bytes,8);assert!(zipped.is_file());
        drop(state);drop(history);std::fs::remove_dir_all(root).unwrap();
    }
}
