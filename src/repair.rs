//! On-demand session diagnostics and guarded legacy-history repair.
use crate::{Paths, Stop};
use anyhow::{Context,Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Serialize, Deserialize};
use sha2::{Digest,Sha256};
use std::{collections::HashSet, fs, io::Write, path::PathBuf, sync::{Arc, Mutex, atomic::Ordering}, thread, time::Duration};

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Entry {
    pub id: String, pub title: String, pub path: PathBuf, pub archived: bool,
    pub bytes: u64, pub updated: f64, pub seconds: f64, pub compressed: bool,
    pub issue: bool, pub repairable: bool, pub note: String, pub result: String,
    #[serde(default)] pub analysis: Option<crate::clean::Analysis>,
    #[serde(skip)] pub analysis_fingerprint: Option<String>,
    #[serde(default)] pub tags: Vec<crate::tags::Tag>,
    #[serde(default)] pub project_assigned: bool,
    #[serde(default)] pub tags_known: bool,
    #[serde(default)] pub last_status: String,
}
#[derive(Clone, Default)]
pub struct View { pub rows: Vec<Entry>, pub busy: bool, pub batch_running: bool, pub batch_done:usize, pub batch_total:usize, pub compressing: bool, pub decompressing: bool, pub analyzing: bool, pub cleaning: bool, pub current_id: String, pub current_title: String, pub phase: Option<crate::reduce::Phase>, pub phase_done: u64, pub phase_total: u64, pub source_total: u64, pub encoded_total: u64, pub done: usize, pub total: usize, pub revision: u64, pub note: String }
impl View {
    // The bar represents work on the current chat, measured in bytes processed.
    // Before the compressed size is known, assume it will equal the source size.
    pub fn current_fraction(&self)->Option<f64>{
        use crate::reduce::Phase;
        if self.analyzing {return (self.phase_total>0).then(||0.95*(self.phase_done as f64/self.phase_total as f64).clamp(0.,1.));}
        if !self.compressing||self.source_total==0{return None}
        let n=self.source_total as f64;let c=if self.encoded_total==0{n}else{self.encoded_total as f64};
        let base=match self.phase?{Phase::Preparing=>0.,Phase::Compressing=>0.,Phase::CheckingCompressed=>n,Phase::CheckingContent=>n+c,Phase::CheckingSource=>2.*n+c,Phase::Publishing=>3.*n+c,Phase::CheckingPublished=>3.*n+2.*c};
        Some(((base+self.phase_done as f64)/(3.*n+3.*c)).clamp(0.,1.))
    }
}
#[derive(Clone)]
pub struct Service { pub view: Arc<Mutex<View>>, paths: Paths, stop: Stop }
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum BatchAction { Analyze, Clean(bool), Compress, Decompress, Repair, Archive, Delete, ForceDelete }
pub fn note(value:&str,zh:bool)->&str{match (value,zh){("history_missing",true)=>"缺少回合索引，需要检查",("history_missing",false)=>"Turn index missing; needs inspection",("compressed_unindexed",true)=>"旧格式压缩会话：未发现可用的回合索引，需要检查",("compressed_unindexed",false)=>"Legacy compressed session: no turn index; needs inspection",("history_unavailable",true)=>"Codex 无法读取回合记录",("history_unavailable",false)=>"Codex cannot read the turn history",("legacy_index_ready",true)=>"读取模式不兼容 · 可修复",("legacy_index_ready",false)=>"Legacy compatibility · repair available",("missing_source",true)=>"缺少文件和历史记录 · 需要备份",("missing_source",false)=>"Missing file and history · backup required",("history_only",true)=>"仅有数据库历史 · 打开状态待核实",("history_only",false)=>"Database history only · opening unverified",("compressed",true)=>"本地压缩会话",("compressed",false)=>"Local compressed session",("running",true)=>"活动中 · 不可缩减或修复",("running",false)=>"In progress · cannot reduce or repair",(_,true)=>"本地文件存在 · 未验证打开",(_,false)=>"Local file present · opening unverified"}}
fn db(path: PathBuf) -> Result<Connection> {
    let db=Connection::open_with_flags(path,OpenFlags::SQLITE_OPEN_READ_ONLY|OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    db.busy_timeout(Duration::from_millis(500))?;
    db.execute_batch("PRAGMA query_only=ON; PRAGMA temp_store=MEMORY; PRAGMA cache_size=-512")?; Ok(db)
}
// A cache entry contains counts only, never conversation content. A versioned
// file fingerprint prevents reuse after an append, rewrite, or compression.
pub fn analysis_fingerprint(entry:&Entry)->Option<String> {
    let mut path=entry.path.clone();
    if !path.is_file(){let mut name=path.as_os_str().to_os_string();name.push(".zst");path=PathBuf::from(name);}
    let meta=fs::metadata(&path).ok()?;let modified=meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
    Some(format!("v2|{}|{}|{}",path.display(),meta.len(),modified))
}
fn analysis_cache(paths:&Paths)->Result<Connection>{
    let db=Connection::open(paths.runtime.join("clean-analysis-cache.sqlite"))?;
    db.busy_timeout(Duration::from_millis(500))?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS analyses (id TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, json TEXT NOT NULL)")?;Ok(db)
}
fn cached_analysis(db:&Connection,entry:&Entry)->Option<crate::clean::Analysis>{
    let fingerprint=analysis_fingerprint(entry)?;
    let json:String=db.query_row("SELECT json FROM analyses WHERE id=?1 AND fingerprint=?2",params![entry.id,fingerprint],|r|r.get(0)).optional().ok()??;
    serde_json::from_str(&json).ok()
}
fn cache_analysis(db:&Connection,entry:&Entry,fingerprint:&str,analysis:&crate::clean::Analysis)->Result<()> {
    db.execute("INSERT OR REPLACE INTO analyses(id,fingerprint,json) VALUES(?1,?2,?3)",params![entry.id,fingerprint,serde_json::to_string(analysis)?])?;Ok(())
}
fn compressed_history_needs_attention(mode:&str,compressed:bool,turns:i64)->bool{mode=="legacy"&&compressed&&turns==0}
fn inspect(state: &Connection, history: Option<&Connection>, id: &str) -> Result<Entry> {
    let (mut entry, mode):(Entry,String)=state.query_row("SELECT id,coalesce(nullif(name,''),title),rollout_path,archived,updated_at,coalesce(history_mode,'legacy') FROM threads WHERE id=? AND agent_path IS NULL",[id],|r|Ok((Entry{id:r.get(0)?,title:r.get(1)?,path:PathBuf::from(r.get::<_,Option<String>>(2)?.unwrap_or_default()),archived:r.get(3)?,updated:r.get(4)?,..Default::default()},r.get(5)?)))?;
    let membership=state.query_row("SELECT project_id,thread_section_id FROM threads WHERE id=?",[id],|r|Ok((r.get::<_,Option<String>>(0)?,r.get::<_,Option<String>>(1)?)));
    entry.tags_known=membership.is_ok();entry.project_assigned=membership.ok().is_some_and(|(p,g)|p.is_some_and(|s|!s.trim().is_empty())||g.is_some_and(|s|!s.trim().is_empty()));
    let plain=fs::metadata(&entry.path).ok().filter(|m|m.is_file());
    let mut zipped=entry.path.as_os_str().to_os_string(); zipped.push(".zst");
    let zipped=fs::metadata(PathBuf::from(zipped)).ok().filter(|m|m.is_file());
    entry.bytes=plain.as_ref().map_or(0,|m|m.len())+zipped.as_ref().map_or(0,|m|m.len());
    entry.compressed=entry.path.extension().is_some_and(|e|e=="zst") || zipped.is_some();
    let mut indexed=None; let mut turns=0i64; let mut running=false;
    if let Some(h)=history {
        indexed=h.query_row("SELECT next_rollout_byte_offset FROM thread_history_projection_state WHERE thread_id=?",[id],|r|r.get::<_,u64>(0)).optional().unwrap_or(None);
        turns=h.query_row("SELECT count(*) FROM thread_turns WHERE thread_id=?",[id],|r|r.get(0)).unwrap_or(0);
        entry.last_status=h.query_row("SELECT status FROM thread_turns WHERE thread_id=? ORDER BY rollout_ordinal DESC LIMIT 1",[id],|r|r.get::<_,String>(0)).optional().unwrap_or(None).unwrap_or_default();
        running=matches!(entry.last_status.as_str(),"inProgress"|"in_progress"|"running");
        entry.seconds=h.query_row("SELECT coalesce(sum(CASE WHEN completed_at>=started_at THEN completed_at-started_at END),0) FROM thread_turns WHERE thread_id=?",[id],|r|r.get(0)).unwrap_or(0.);
    }
    entry.repairable=mode=="legacy" && !entry.compressed && plain.as_ref().is_some_and(|m|indexed==Some(m.len())) && turns>0 && !running;
    let compressed_unindexed=history.is_some()&&compressed_history_needs_attention(&mode,entry.compressed,turns);
    let history_missing=history.is_some()&&mode=="paginated"&&plain.is_some()&&indexed.is_none()&&turns==0;
    entry.issue=entry.repairable || compressed_unindexed || history_missing || (plain.is_none() && zipped.is_none() && turns==0);
    entry.note=if entry.repairable{"legacy_index_ready"}else if plain.is_none() && zipped.is_none(){if turns>0{"history_only"}else{"missing_source"}}else if running{"running"}else if compressed_unindexed{"compressed_unindexed"}else if history_missing{"history_missing"}else if entry.compressed{"compressed"}else{"local"}.into();
    Ok(entry)
}
impl Service {
    pub fn new(paths: Paths,stop: Stop)->Self{Self{paths,stop,view:Arc::new(Mutex::new(View::default()))}}
    pub fn start_batch(&self,mut ids:Vec<String>,action:BatchAction,activity:crate::State){
        ids.sort();ids.dedup();if ids.is_empty(){return}
        {let mut v=self.view.lock().unwrap();if v.busy||v.batch_running{return}v.batch_running=true;v.batch_total=ids.len();v.batch_done=0;v.note.clear();v.revision+=1;}
        let service=self.clone();thread::spawn(move||{
            let mut done=0usize;let mut skipped=0usize;let mut last_error=String::new();let mut deleter:Option<crate::manager::Deleter>=None;
            for id in ids {
                if service.stop.load(Ordering::Relaxed){break}
                let entry={let v=service.view.lock().unwrap();v.rows.iter().find(|r|r.id==id).cloned()};
                let Some(entry)=entry else{skipped+=1;let mut v=service.view.lock().unwrap();v.batch_done+=1;v.revision+=1;continue};
                let live=activity.lock().unwrap().clone();let busy:HashSet<String>=live.recent.rows.iter().chain(&live.recent.busy_metrics).filter(|r|r.state=="Busy").map(|r|r.id.clone()).collect();
                let active=entry.note=="running"||busy.contains(&id);
                let eligible=match action{BatchAction::Analyze=>entry.bytes>0,BatchAction::Clean(_)=>!entry.compressed&&!entry.repairable&&!active&&!live.recent.stale,BatchAction::Compress=>!entry.compressed&&!entry.repairable&&!active&&!live.recent.stale,BatchAction::Decompress=>entry.compressed&&!active&&!live.recent.stale,BatchAction::Repair=>entry.repairable&&!active&&!live.recent.stale,BatchAction::Archive=>!entry.archived&&!active&&!live.recent.stale,BatchAction::Delete|BatchAction::ForceDelete=>!active&&!live.recent.stale};
                if !eligible {skipped+=1;let reason=if active{"Session is active"}else if live.recent.stale{"Session status is unavailable"}else{"This operation is not available for this session"};last_error=reason.into();let mut v=service.view.lock().unwrap();if let Some(row)=v.rows.iter_mut().find(|r|r.id==id){row.result=reason.into();}v.batch_done+=1;v.revision+=1;continue}
                {let mut v=service.view.lock().unwrap();v.current_id=id.clone();v.current_title=entry.title.clone();v.note=format!("{}: {}",if crate::locale::chinese(){"正在处理"}else{"Processing"},entry.title);v.revision+=1;}
                if matches!(action,BatchAction::Delete|BatchAction::ForceDelete|BatchAction::Archive) {
                    let result=(||->Result<()>{if deleter.is_none(){let mut exes:Vec<_>=crate::reduce::running_codex()?.into_iter().collect();exes.sort();let exe=exes.first().context("No running Codex available")?;deleter=Some(crate::manager::Deleter::open(&service.paths,std::path::Path::new(exe))?);}if action==BatchAction::Archive{deleter.as_mut().unwrap().archive(&service.paths,&id,&service.stop)}else if action==BatchAction::ForceDelete&&entry.issue{deleter.as_mut().unwrap().force_delete(&id)}else{deleter.as_mut().unwrap().delete(&service.paths,&id,&service.stop)}})();
                    let succeeded=result.is_ok();let mut v=service.view.lock().unwrap();match result {Ok(())=>{if action==BatchAction::Archive{if let Some(row)=v.rows.iter_mut().find(|r|r.id==id){row.archived=true;row.result.clear();}}else{v.rows.retain(|r|r.id!=id);}done+=1;},Err(error)=>{skipped+=1;last_error=format!("{error:#}");if let Some(row)=v.rows.iter_mut().find(|r|r.id==id){row.result=last_error.clone();if last_error.contains("list_turns is not supported") {row.issue=true;row.note="history_unavailable".into();}}}}v.batch_done+=1;v.current_id.clear();v.current_title.clear();v.revision+=1;drop(v);if succeeded{activity.lock().unwrap().refresh_events.notify();}continue
                }
                if matches!(action,BatchAction::Clean(_))&&entry.analysis.is_none(){
                    service.start_analysis(id.clone());loop{if !service.view.lock().unwrap().busy||service.stop.load(Ordering::Relaxed){break}thread::sleep(Duration::from_millis(100));}
                    let analyzed=service.view.lock().unwrap().rows.iter().find(|r|r.id==id).is_some_and(|r|r.analysis.is_some());if !analyzed{skipped+=1;let mut v=service.view.lock().unwrap();v.batch_done+=1;v.revision+=1;continue}
                }
                let live=activity.lock().unwrap().clone();let busy:HashSet<String>=live.recent.rows.iter().chain(&live.recent.busy_metrics).filter(|r|r.state=="Busy").map(|r|r.id.clone()).collect();
                match action {
                    BatchAction::Analyze=>service.start_analysis(id.clone()),
                    BatchAction::Clean(io)=>service.start_clean(id.clone(),io,busy,live.recent.stale),
                    BatchAction::Compress=>service.start_reduce(HashSet::from([id.clone()]),busy,activity.clone()),
                    BatchAction::Decompress=>service.start_decompress(id.clone(),busy,live.recent.stale,activity.clone()),
                    BatchAction::Repair=>service.start(Some(HashSet::from([id.clone()])),busy),
                    BatchAction::Archive|BatchAction::Delete|BatchAction::ForceDelete=>unreachable!(),
                }
                loop {if !service.view.lock().unwrap().busy{break}if service.stop.load(Ordering::Relaxed){break}thread::sleep(Duration::from_millis(100));}
                let mut v=service.view.lock().unwrap();let ok=v.rows.iter().find(|r|r.id==id).is_some_and(|r|match action{BatchAction::Analyze=>r.analysis.is_some(),BatchAction::Clean(_)=>r.result.starts_with("cleaned:"),BatchAction::Compress=>r.result.starts_with("reduced:"),BatchAction::Decompress=>r.result=="decompressed",BatchAction::Repair=>r.result=="repaired",_=>false});if ok{done+=1}else{skipped+=1;if let Some(row)=v.rows.iter().find(|r|r.id==id){last_error=row.result.clone();}}v.batch_done+=1;v.revision+=1;
            }
            let mut v=service.view.lock().unwrap();v.batch_running=false;v.current_id.clear();v.current_title.clear();v.note=if crate::locale::chinese(){format!("完成 {done} · 跳过或失败 {skipped}")}else{format!("Completed {done} · skipped or failed {skipped}")};if !last_error.is_empty(){v.note.push_str(" · ");v.note.push_str(&last_error);}v.revision+=1;
        });
    }
    pub fn start_decompress(&self,id:String,busy:HashSet<String>,stale:bool,activity:crate::State){
        {let mut v=self.view.lock().unwrap();if v.busy{return}if stale||busy.contains(&id){v.note=if crate::locale::chinese(){"会话活动中或状态不可用，未解压"}else{"Session is active or status is unavailable; not decompressed"}.into();v.revision+=1;return}v.busy=true;v.decompressing=true;v.current_id=id.clone();v.current_title=v.rows.iter().find(|r|r.id==id).map(|r|r.title.clone()).unwrap_or_else(||id.clone());v.done=0;v.total=1;v.note.clear();v.revision+=1;}
        let (paths,output,stop)=(self.paths.clone(),self.view.clone(),self.stop.clone());thread::spawn(move||{
            unsafe {use windows::Win32::System::Threading::*;let _=SetThreadPriority(GetCurrentThread(),THREAD_MODE_BACKGROUND_BEGIN);}
            let mut last=std::time::Instant::now()-Duration::from_secs(1);let result=crate::reduce::decompress_one(&paths,&id,&stop,Some(&activity),&mut |phase,done,total|{if last.elapsed()>=Duration::from_millis(150)||done==total {let mut v=output.lock().unwrap();v.phase=Some(phase);v.phase_done=done;v.phase_total=total;v.revision+=1;last=std::time::Instant::now();}});
            let indexed=if result.is_ok(){db(paths.home.join("thread_history_1.sqlite")).ok().and_then(|db|db.query_row("SELECT 1 FROM thread_history_projection_state WHERE thread_id=?",[&id],|_|Ok(())).optional().ok().flatten()).is_some()}else{false};
            let mut v=output.lock().unwrap();let zh=crate::locale::chinese();let mut note=String::new();if let Some(row)=v.rows.iter_mut().find(|r|r.id==id){match result {Ok((_before,after))=>{row.bytes=after;row.compressed=false;row.note="local".into();row.analysis=None;row.result="decompressed".into();note=if indexed {if zh{"解压完成；重新分析后可检查是否能清理"}else{"Decompressed; analyze again to check cleanup eligibility"}}else if zh{"解压完成；清理前请先在 Codex 打开会话以建立索引"}else{"Decompressed; open the chat in Codex to build its index before cleaning"}.into();},Err(e)=>{row.result=format!("{e:#}");note=if zh{"解压未完成；悬停查看原因"}else{"Decompression did not complete; hover for details"}.into();}}}v.note=note;v.busy=false;v.decompressing=false;v.current_id.clear();v.current_title.clear();v.phase=None;v.done=1;v.revision+=1;
        });
    }
    pub fn start_clean(&self,id:String,include_io:bool,busy:HashSet<String>,stale:bool){
        {let mut v=self.view.lock().unwrap();if v.busy{return}if stale||busy.contains(&id){v.note=if crate::locale::chinese(){"会话活动中或状态不可用，未清理"}else{"Session is active or status is unavailable; not cleaned"}.into();v.revision+=1;return}v.busy=true;v.cleaning=true;v.current_id=id.clone();v.current_title=v.rows.iter().find(|r|r.id==id).map(|r|r.title.clone()).unwrap_or_else(||id.clone());v.done=0;v.total=1;v.note.clear();v.revision+=1;}
        let (view,paths)=(self.view.clone(),self.paths.clone());thread::spawn(move||{
            use std::os::windows::process::CommandExt;use std::process::{Command,Stdio};
            let result=(||->Result<(u64,u64)>{let mut cmd=Command::new(std::env::current_exe()?);cmd.arg("--clean-worker").arg(&id);if include_io{cmd.arg("--include-command-io");}let output=cmd.creation_flags(0x08004000).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?.wait_with_output()?;
                if !output.status.success(){let recovery_stop=std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));let recovery=crate::clean::recover_all(&paths,&recovery_stop);let error=String::from_utf8_lossy(&output.stderr).chars().take(500).collect::<String>();if let Err(e)=recovery{anyhow::bail!("{error}; recovery pending: {e:#}")}anyhow::bail!("{}",if error.trim().is_empty(){"Cleanup worker stopped; recovery completed"}else{error.trim()})}
                let value:serde_json::Value=serde_json::from_slice(&output.stdout)?;Ok((value["before"].as_u64().context("Missing original size")?,value["after"].as_u64().context("Missing cleaned size")?))})();
            let mut v=view.lock().unwrap();let mut note=String::new();if let Some(row)=v.rows.iter_mut().find(|r|r.id==id){match result{Ok((before,after))=>{row.bytes=after;row.analysis=None;row.result=format!("cleaned:{before}:{after}");note=if crate::locale::chinese(){format!("已清理：{} → {}，减少 {}；备份已保留",crate::clean::display_bytes(before),crate::clean::display_bytes(after),crate::clean::display_bytes(before.saturating_sub(after)))}else{format!("Cleaned: {} → {} · saved {} · backup retained",crate::clean::display_bytes(before),crate::clean::display_bytes(after),crate::clean::display_bytes(before.saturating_sub(after)))};},Err(error)=>{row.result=format!("{error:#}");if row.result.contains("could not rebuild the original session history"){row.issue=true;row.note="history_unavailable".into();}note=if crate::locale::chinese(){format!("清理未完成：{}",row.result)}else{format!("Cleanup failed: {}",row.result)};}}}v.note=note;v.busy=false;v.cleaning=false;v.current_id.clear();v.current_title.clear();v.done=1;v.revision+=1;
        });
    }
    pub fn start_analysis(&self,id:String){
        {let mut v=self.view.lock().unwrap();if v.busy{return}v.busy=true;v.analyzing=true;v.phase_done=0;v.phase_total=0;v.current_id=id.clone();v.current_title=v.rows.iter().find(|r|r.id==id).map(|r|r.title.clone()).unwrap_or_else(||id.clone());v.total=1;v.done=0;v.note.clear();v.revision+=1;}
        let (paths,output,stop)=(self.paths.clone(),self.view.clone(),self.stop.clone());
        thread::spawn(move||{
            unsafe {use windows::Win32::System::Threading::*;let _=SetThreadPriority(GetCurrentThread(),THREAD_MODE_BACKGROUND_BEGIN);}
            let entry=output.lock().unwrap().rows.iter().find(|r|r.id==id).cloned();
            let before=entry.as_ref().and_then(analysis_fingerprint);
            let mut last=std::time::Instant::now()-Duration::from_secs(1);
            let mut result=crate::clean::analyze_progress(&paths,&id,&stop,&mut |done,total|{if last.elapsed()>=Duration::from_millis(150)||done==total {let mut v=output.lock().unwrap();v.phase_done=done;v.phase_total=total;v.revision+=1;last=std::time::Instant::now();}});
            if let (Some(entry),Some(before))=(entry.as_ref(),before.as_ref()) {
                if analysis_fingerprint(entry).as_ref()!=Some(before) {result=Err(anyhow::anyhow!("Session changed during analysis; analyze again"));}
                else if let Ok(analysis)=&result {if let Ok(db)=analysis_cache(&paths){let _=cache_analysis(&db,entry,before,analysis);}}
            }
            let mut v=output.lock().unwrap();
            let zh=crate::locale::chinese();
            let mut message=String::new();
            if let Some(row)=v.rows.iter_mut().find(|r|r.id==id){match result {Ok(analysis)=>{
                let title:String=row.title.chars().take(24).collect();
                let size=crate::clean::display_bytes(analysis.rollout_candidate_bytes);
                message=if zh {format!("{title} · 生成文件 diff 候选：{} 个 / {size}{} · 仅为文本体积，非实际释放空间",analysis.rollout_candidates,if analysis.oversized_rollout_records>0{" · 存在未分类大记录"}else{""})} else {format!("{title} · Generated-file diff candidates: {} / {size}{} · text size, not disk savings",analysis.rollout_candidates,if analysis.oversized_rollout_records>0{" · oversized records unclassified"}else{""})};
                if row.compressed {message=if zh{format!("{title} · 压缩会话暂不支持清理 · 候选 diff {} / {size}",analysis.rollout_candidates)}else{format!("{title} · Compressed sessions cannot yet be cleaned · candidate diffs {} / {size}",analysis.rollout_candidates)};}
                row.analysis=Some(analysis);row.analysis_fingerprint=before.clone();row.result.clear();},Err(error)=>{row.analysis=None;row.analysis_fingerprint=None;row.result=format!("{error:#}");message=if zh{format!("分析失败：{}",row.result)}else{format!("Analysis failed: {}",row.result)};}}}
            v.note=message;
            v.busy=false;v.analyzing=false;v.current_id.clear();v.current_title.clear();v.done=1;v.revision+=1;
        });
    }
    pub fn start_reduce(&self,ids:HashSet<String>,busy:HashSet<String>,activity:crate::State){
        {let mut v=self.view.lock().unwrap();if v.busy{return}v.busy=true;v.compressing=true;v.current_id.clear();v.current_title.clear();v.phase=None;v.phase_done=0;v.phase_total=0;v.source_total=0;v.encoded_total=0;v.done=0;v.total=ids.len();v.note.clear();if ids.len()==1{let id=ids.iter().next().unwrap();v.current_id=id.clone();v.current_title=v.rows.iter().find(|r|&r.id==id).map(|r|r.title.clone()).unwrap_or_else(||id.clone());v.phase=Some(crate::reduce::Phase::Preparing);}v.revision+=1;}
        let (paths,output,stop)=(self.paths.clone(),self.view.clone(),self.stop.clone());
        thread::spawn(move||{
            unsafe {use windows::Win32::System::Threading::*;let _=SetThreadPriority(GetCurrentThread(),THREAD_MODE_BACKGROUND_BEGIN);}
            if !ids.is_empty() {
                if let Err(e)=crate::reduce::verify_runtime(&stop){let mut v=output.lock().unwrap();v.busy=false;v.compressing=false;v.current_id.clear();v.current_title.clear();v.phase=None;v.note=format!("{}: {e:#}",if crate::locale::chinese(){"无法确认 Codex 压缩兼容性，已跳过"}else{"Codex compatibility check failed; skipped"});v.revision+=1;return;}
            }
            for id in ids {
                if stop.load(Ordering::Relaxed){break}
                {let mut v=output.lock().unwrap();v.current_id=id.clone();v.current_title=v.rows.iter().find(|r|r.id==id).map(|r|r.title.clone()).unwrap_or_else(||id.clone());v.phase=Some(crate::reduce::Phase::Preparing);v.phase_done=0;v.phase_total=0;v.source_total=0;v.encoded_total=0;v.revision+=1;}
                let mut last_phase=None;let mut last_update=std::time::Instant::now()-Duration::from_secs(1);
                let mut progress=|phase,done,total|{
                    if last_phase!=Some(phase)||done>=total||last_update.elapsed()>=Duration::from_millis(150){
                        let mut v=output.lock().unwrap();v.phase=Some(phase);v.phase_done=done;v.phase_total=total;if phase==crate::reduce::Phase::Compressing{v.source_total=total;}if phase==crate::reduce::Phase::CheckingCompressed{v.encoded_total=total;}v.revision+=1;last_phase=Some(phase);last_update=std::time::Instant::now();
                    }
                };
                let result=if busy.contains(&id){Err(anyhow::anyhow!("Chat is running"))}else{crate::reduce::one(&paths,&id,&stop,Some(&activity),&mut progress)};
                let mut v=output.lock().unwrap();
                if let Some(row)=v.rows.iter_mut().find(|r|r.id==id){
                    row.result=match result{Ok((before,after))=>{row.bytes=after;row.compressed=true;row.repairable=false;if !row.issue{row.note="compressed".into();}format!("reduced:{before}:{after}")},Err(e)=>{let text=format!("{e:#}");if crate::locale::chinese(){match text.as_str(){"Session requires history repair before reduction"=>"请先修复会话的历史读取模式".into(),"Read-only session file; reduction skipped"=>"会话文件为只读，已跳过".into(),"Cannot inspect session file permissions"=>"无法检查会话文件权限，已跳过".into(),"Compressed file permissions differ from the source"=>"压缩文件权限与原文件不一致，已跳过".into(),_=>text}}else{text}}};
                }
                if let Some(row)=v.rows.iter().find(|r|r.id==id){let result=row.result.clone();v.note=if result.starts_with("reduced:"){if crate::locale::chinese(){"压缩完成，原始内容已校验".into()}else{"Compressed; original content verified".into()}}else if crate::locale::chinese(){format!("压缩未完成：{result}")}else{format!("Compression failed: {result}")};}
                v.done+=1;v.current_id.clear();v.current_title.clear();v.phase=None;v.phase_done=0;v.phase_total=0;v.source_total=0;v.encoded_total=0;v.revision+=1;
            }
            let mut v=output.lock().unwrap();v.busy=false;v.compressing=false;v.current_id.clear();v.current_title.clear();v.phase=None;v.revision+=1;
        });
    }
    pub fn start(&self, repair: Option<HashSet<String>>, busy: HashSet<String>) {
        {let mut v=self.view.lock().unwrap(); if v.busy{return} v.busy=true;v.compressing=false;v.current_id.clear();v.current_title.clear();v.phase=None;v.done=0;v.total=0;v.note.clear();v.revision+=1;}
        let (paths,output,stop)=(self.paths.clone(),self.view.clone(),self.stop.clone());
        thread::spawn(move||{
            unsafe {use windows::Win32::System::Threading::*;let _=SetThreadPriority(GetCurrentThread(),THREAD_MODE_BACKGROUND_BEGIN);}
            let outcome=(||->Result<()>{
                let state=db(paths.home.join("state_5.sqlite"))?;
                let history=db(paths.home.join("thread_history_1.sqlite")).ok();
                let cache=analysis_cache(&paths).ok();
                let internal=crate::visibility::internal_reviews(&state)?;
                let mut query=state.prepare("SELECT id FROM threads WHERE agent_path IS NULL ORDER BY updated_at DESC,id")?;
                let ids=query.query_map([],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
                let ids:Vec<_>=ids.into_iter().filter(|id|!internal.contains(id)&&repair.as_ref().is_none_or(|selected|selected.contains(id))).collect();
                {let mut v=output.lock().unwrap();v.total=ids.len(); if repair.is_none(){v.rows.clear();}}
                for id in ids {
                    if stop.load(Ordering::Relaxed){break}
                    let mut entry=inspect(&state,history.as_ref(),&id)?;
                    if repair.is_none(){entry.analysis=cache.as_ref().and_then(|db|cached_analysis(db,&entry));if entry.analysis.is_some(){entry.analysis_fingerprint=analysis_fingerprint(&entry);}}
                    if repair.is_some(){
                        let result=if busy.contains(&id){Err(anyhow::anyhow!("Chat is running"))}else{repair_one(&paths,&entry,Some(&stop))};
                        entry.result=match result{Ok(())=>{entry.issue=false;entry.repairable=false;entry.note="local".into();"repaired".into()},Err(e)=>format!("{e:#}")};
                    }
                    let mut v=output.lock().unwrap();
                    if let Some(old)=v.rows.iter_mut().find(|r|r.id==id){if repair.is_some(){entry.tags=old.tags.clone();entry.tags_known=old.tags_known;}*old=entry}else{v.rows.push(entry)}
                    v.done+=1;v.revision+=1;
                }
                if repair.is_none(){
                    let mut projected:Vec<crate::data::Row>={let v=output.lock().unwrap();v.rows.iter().map(|entry|crate::data::Row{id:entry.id.clone(),label:entry.title.clone(),..Default::default()}).collect()};
                    let mut sources=crate::tags::Sources::default();sources.enrich(&paths,&state,&serde_json::json!({}),&mut projected);
                    let tags:std::collections::HashMap<_,_>=projected.into_iter().map(|r|(r.id,r.tags)).collect();let mut view=output.lock().unwrap();
                    let global_known=sources.global_known(&paths);for entry in &mut view.rows{entry.tags=tags.get(&entry.id).cloned().unwrap_or_default();entry.project_assigned|=sources.has_project_assignment(&entry.id);entry.tags_known&=global_known;}view.revision+=1;
                }
                Ok(())
            })();
            let mut v=output.lock().unwrap();v.busy=false;v.note=outcome.err().map(|e|format!("{e:#}")).unwrap_or_default();if v.note.is_empty(){let state=crate::read_json(&paths.runtime.join("clean-recovery-status.json"),16384);if let Some(error)=state["error"].as_str().filter(|s|!s.is_empty()){v.note=if crate::locale::chinese(){format!("清理恢复待处理：{error}")}else{format!("Cleanup recovery pending: {error}")};}}v.revision+=1;
        });
    }
}
fn repair_one(paths:&Paths, expected:&Entry, stop:Option<&Stop>)->Result<()> {
    let state=db(paths.home.join("state_5.sqlite"))?;
    let history=db(paths.home.join("thread_history_1.sqlite"))?;
    let current=inspect(&state,Some(&history),&expected.id)?;
    anyhow::ensure!(current.repairable,"No verified automatic repair available");
    let original=fs::metadata(&current.path)?;
    // Durable backups are kept with the application, outside disposable caches.
    anyhow::ensure!(!current.id.is_empty()&&current.id.chars().all(|c|c.is_ascii_alphanumeric()||c=='-'),"Invalid chat ID");
    let folder=paths.app.join("repair-backups").join(format!("{}-{}",current.id,crate::now() as u64));
    fs::create_dir_all(&folder)?;
    let mut backup=fs::OpenOptions::new().write(true).create_new(true).open(folder.join("rollout.jsonl.gz"))?;
    let mut zip=flate2::write::GzEncoder::new(&mut backup,flate2::Compression::fast());
    let mut source=fs::File::open(&current.path)?;let mut buffer=vec![0u8;64*1024];let mut copied=0u64;let mut hash=Sha256::new();
    loop {
        anyhow::ensure!(!stop.is_some_and(|s|s.load(Ordering::Relaxed)),"Repair cancelled before applying changes");
        let n=std::io::Read::read(&mut source,&mut buffer)?;if n==0{break}zip.write_all(&buffer[..n])?;hash.update(&buffer[..n]);copied+=n as u64;
        if copied%(8*1024*1024)==0{thread::sleep(Duration::from_millis(20));}
    }
    zip.finish()?;backup.sync_all()?;
    let digest=hash.finalize();let mut verified=Sha256::new();
    let mut decoder=flate2::read::GzDecoder::new(fs::File::open(folder.join("rollout.jsonl.gz"))?);
    loop {anyhow::ensure!(!stop.is_some_and(|s|s.load(Ordering::Relaxed)),"Repair cancelled before applying changes");let n=std::io::Read::read(&mut decoder,&mut buffer)?;if n==0{break}verified.update(&buffer[..n]);}
    anyhow::ensure!(verified.finalize()==digest,"Backup verification failed; original session preserved");
    anyhow::ensure!(copied==original.len(),"Session changed during backup");
    let after=fs::metadata(&current.path)?;
    anyhow::ensure!(after.len()==original.len()&&after.modified()?==original.modified()?,"Session changed during backup");
    let row_json:String=state.query_row("SELECT json_object('id',id,'history_mode',history_mode,'rollout_path',rollout_path,'updated_at',updated_at) FROM threads WHERE id=?",[&current.id],|r|r.get(0))?;
    let mut record=fs::OpenOptions::new().write(true).create_new(true).open(folder.join("record.json"))?;record.write_all(row_json.as_bytes())?;record.sync_all()?;
    // Check again after backup; never replace content or rewrite a whole database.
    anyhow::ensure!(inspect(&state,Some(&history),&current.id)?.repairable,"Chat state changed");
    let writable=Connection::open_with_flags(paths.home.join("state_5.sqlite"),OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    writable.busy_timeout(Duration::from_millis(500))?;writable.execute_batch("PRAGMA temp_store=MEMORY")?;
    let changed=writable.execute("UPDATE threads SET history_mode='paginated' WHERE id=? AND history_mode='legacy' AND rollout_path=? AND updated_at=?",params![current.id,current.path.to_string_lossy(),current.updated])?;
    anyhow::ensure!(changed==1,"Chat changed; repair skipped");
    let _=fs::write(folder.join("result.json"),serde_json::to_vec(&serde_json::json!({"id":current.id,"action":"legacy_to_paginated","time":crate::now()}))?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selected_delete_skips_active_chats_without_touching_files(){
        let paths=fixture("batch-active");let source=paths.home.join("source.jsonl");let before=fs::read(&source).unwrap();let stop=Arc::new(std::sync::atomic::AtomicBool::new(false));let service=Service::new(paths,stop);service.view.lock().unwrap().rows=vec![Entry{id:"test-chat".into(),title:"test".into(),path:source.clone(),note:"running".into(),..Default::default()}];let state=Arc::new(Mutex::new(crate::Shared::default()));service.start_batch(vec!["test-chat".into()],BatchAction::Delete,state);for _ in 0..100{if !service.view.lock().unwrap().batch_running{break}thread::sleep(Duration::from_millis(10));}let view=service.view.lock().unwrap();assert!(!view.batch_running);assert_eq!(view.rows.len(),1);assert_eq!(fs::read(source).unwrap(),before);
    }
    #[test]
    fn cached_analysis_is_invalidated_when_source_changes(){
        let paths=fixture("analysis-cache");let entry=Entry{id:"test-chat".into(),path:paths.home.join("source.jsonl"),..Default::default()};let db=analysis_cache(&paths).unwrap();let fingerprint=analysis_fingerprint(&entry).unwrap();let analysis=crate::clean::Analysis{estimated_diff_delta:123,..Default::default()};cache_analysis(&db,&entry,&fingerprint,&analysis).unwrap();assert_eq!(cached_analysis(&db,&entry).unwrap().file_reduction(false),Some(123));fs::write(&entry.path,b"changed content\n").unwrap();assert!(cached_analysis(&db,&entry).is_none());
    }
    #[test]
    fn compressed_legacy_without_turn_index_requires_inspection(){assert!(compressed_history_needs_attention("legacy",true,0));assert!(!compressed_history_needs_attention("paginated",true,0));assert!(!compressed_history_needs_attention("legacy",false,0));assert!(!compressed_history_needs_attention("legacy",true,1));}
    #[test]
    fn analysis_progress_reserves_projection_work(){
        let mut v=View{analyzing:true,phase_done:50,phase_total:100,..Default::default()};assert_eq!(v.current_fraction(),Some(0.475));v.phase_done=100;assert_eq!(v.current_fraction(),Some(0.95));v.phase_total=0;assert_eq!(v.current_fraction(),None);
    }
    #[test]
    fn current_chat_progress_is_monotonic_across_stages(){
        use crate::reduce::Phase;
        let mut v=View{compressing:true,source_total:100,phase:Some(Phase::Compressing),phase_done:100,phase_total:100,..Default::default()};
        let mut previous=v.current_fraction().unwrap();
        for (phase,done,total) in [(Phase::CheckingCompressed,0,20),(Phase::CheckingCompressed,20,20),(Phase::CheckingContent,0,100),(Phase::CheckingContent,100,100),(Phase::CheckingSource,100,100),(Phase::Publishing,20,20),(Phase::CheckingPublished,20,20)]{
            v.encoded_total=20;v.phase=Some(phase);v.phase_done=done;v.phase_total=total;let next=v.current_fraction().unwrap();assert!(next>=previous);previous=next;
        }
        assert!((previous-1.).abs()<0.00001);
    }
    fn fixture(name:&str)->Paths {
        let real=Paths::discover().unwrap();let dir=real.runtime.join(format!("health-test-{}-{name}",std::process::id()));fs::create_dir_all(&dir).unwrap();
        let paths=Paths{app:dir.clone(),home:dir.clone(),runtime:dir.clone(),instance:String::new()};
        let state=Connection::open(dir.join("state_5.sqlite")).unwrap();
        state.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,title TEXT,name TEXT,rollout_path TEXT,archived INTEGER,updated_at REAL,history_mode TEXT,agent_path TEXT)").unwrap();
        fs::write(dir.join("source.jsonl"),b"example\n").unwrap();
        state.execute("INSERT INTO threads VALUES('test-chat','test',NULL,?,0,12,'legacy',NULL)",[dir.join("source.jsonl").to_string_lossy().as_ref()]).unwrap();
        let history=Connection::open(dir.join("thread_history_1.sqlite")).unwrap();
        history.execute_batch("CREATE TABLE thread_history_projection_state(thread_id TEXT,next_rollout_byte_offset INTEGER);INSERT INTO thread_history_projection_state VALUES('test-chat',8);CREATE TABLE thread_turns(thread_id TEXT,status TEXT,rollout_ordinal INTEGER,started_at REAL,completed_at REAL);INSERT INTO thread_turns VALUES('test-chat','completed',1,10,20)").unwrap();paths
    }
    #[test]
    fn repair_backs_up_content_and_only_changes_eligible_mode(){
        let paths=fixture("eligible");let s=db(paths.home.join("state_5.sqlite")).unwrap();let h=db(paths.home.join("thread_history_1.sqlite")).unwrap();
        let before=inspect(&s,Some(&h),"test-chat").unwrap();assert!(before.repairable);assert_eq!(before.seconds,10.);
        repair_one(&paths,&before,None).unwrap();assert!(!inspect(&s,Some(&h),"test-chat").unwrap().repairable);
        assert_eq!(fs::read(before.path).unwrap(),b"example\n");
        let folder=fs::read_dir(paths.app.join("repair-backups")).unwrap().next().unwrap().unwrap().path();
        let mut decoder=flate2::read::GzDecoder::new(fs::File::open(folder.join("rollout.jsonl.gz")).unwrap());let mut text=String::new();std::io::Read::read_to_string(&mut decoder,&mut text).unwrap();assert_eq!(text,"example\n");assert!(folder.join("record.json").is_file());
    }
    #[test]
    fn incomplete_history_and_running_sessions_are_never_repaired(){
        let paths=fixture("incomplete");let s=db(paths.home.join("state_5.sqlite")).unwrap();let h=db(paths.home.join("thread_history_1.sqlite")).unwrap();
        let writable=Connection::open(paths.home.join("thread_history_1.sqlite")).unwrap();writable.execute("UPDATE thread_history_projection_state SET next_rollout_byte_offset=4",[]).unwrap();
        let entry=inspect(&s,Some(&h),"test-chat").unwrap();assert!(!entry.repairable);assert!(repair_one(&paths,&entry,None).is_err());
        writable.execute_batch("UPDATE thread_history_projection_state SET next_rollout_byte_offset=8;UPDATE thread_turns SET status='inProgress'").unwrap();assert!(!inspect(&s,Some(&h),"test-chat").unwrap().repairable);
        assert!(!paths.app.join("repair-backups").exists());
    }
    #[test]
    fn scan_separates_compressed_archived_and_missing_sources_without_writes(){
        let paths=fixture("scan");let state=Connection::open(paths.home.join("state_5.sqlite")).unwrap();
        fs::remove_file(paths.home.join("source.jsonl")).unwrap();
        fs::write(paths.home.join("source.jsonl.zst"),zstd::stream::encode_all(b"example\n".as_slice(),1).unwrap()).unwrap();
        state.execute_batch("UPDATE threads SET archived=1,history_mode='paginated';INSERT INTO threads VALUES('missing-chat','missing',NULL,'missing.jsonl',0,13,'paginated',NULL)").unwrap();
        let s=db(paths.home.join("state_5.sqlite")).unwrap();let h=db(paths.home.join("thread_history_1.sqlite")).unwrap();
        let archived=inspect(&s,Some(&h),"test-chat").unwrap();assert!(archived.archived&&archived.compressed&&!archived.issue);assert!(archived.bytes>0);
        let missing=inspect(&s,Some(&h),"missing-chat").unwrap();assert!(missing.issue&&!missing.repairable);
        assert!(!paths.app.join("repair-backups").exists());
    }
}
