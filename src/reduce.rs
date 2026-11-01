//! Lossless, explicitly requested rollout compression.
use crate::{Paths, State, Stop};
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, fs::{self,File,OpenOptions}, io::{Read,Write}, path::{Path,PathBuf}, sync::atomic::Ordering, time::{Duration,Instant}};

/// Refuse to remove an ordinary rollout unless every visible official Codex
/// CLI installation sharing this desktop supports its compression format.
pub fn verify_runtime(stop:&Stop)->Result<()> {
    let candidates=running_codex()?;
    use std::os::windows::process::CommandExt;
    for executable in candidates {
        let mut process=std::process::Command::new(executable).args(["features","list"]).creation_flags(0x08000000).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn()?;
        let started=Instant::now();
        loop {
            if stop.load(Ordering::Relaxed)||started.elapsed()>Duration::from_secs(10){let _=process.kill();let _=process.wait();anyhow::bail!("Codex compatibility check timed out")}
            if let Some(status)=process.try_wait()? {anyhow::ensure!(status.success(),"Codex compatibility check failed");break}
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut output=String::new();if let Some(mut stream)=process.stdout.take(){stream.by_ref().take(65536).read_to_string(&mut output)?;}
        anyhow::ensure!(advertised_compression(&output),"Running Codex does not advertise compressed-session support");
    }
    Ok(())
}
pub(crate) fn running_codex()->Result<HashSet<String>> {
    use windows::Win32::{Foundation::CloseHandle,System::Threading::{OpenProcess,PROCESS_QUERY_LIMITED_INFORMATION,QueryFullProcessImageNameW,PROCESS_NAME_FORMAT}};
    use windows::core::PWSTR;
    let local=std::env::var("LOCALAPPDATA").unwrap_or_default().replace('/',"\\").to_lowercase();
    let user=std::env::var("USERPROFILE").unwrap_or_default().replace('/',"\\").to_lowercase();
    let snapshot=crate::guard::collect::Collector::new().sample(1)?;let mut candidates=HashSet::new();
    for process in snapshot.processes.into_iter().filter(|p|p.name.eq_ignore_ascii_case("codex.exe")) {
        unsafe {
            let Ok(handle)=OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION,false,process.pid) else {anyhow::bail!("Cannot inspect a running Codex process")};
            let mut buffer=[0u16;4096];let mut size=buffer.len() as u32;
            let result=QueryFullProcessImageNameW(handle,PROCESS_NAME_FORMAT(0),PWSTR(buffer.as_mut_ptr()),&mut size);let _=CloseHandle(handle);
            anyhow::ensure!(result.is_ok(),"Cannot identify a running Codex process");let path=String::from_utf16_lossy(&buffer[..size as usize]);let normalized=path.replace('/',"\\").trim_start_matches(r"\\?\").to_lowercase();
            let desktop=!local.is_empty()&&normalized.starts_with(&format!("{local}\\openai\\codex\\bin\\"));
            let vscode=!user.is_empty()&&normalized.starts_with(&format!("{user}\\.vscode\\extensions\\openai.chatgpt-"))&&normalized.contains("\\bin\\windows-");
            anyhow::ensure!(desktop||vscode,"Cannot verify an unrecognized running Codex installation");candidates.insert(path);
        }
    }
    anyhow::ensure!(!candidates.is_empty(),"Cannot verify a running Codex CLI");
    Ok(candidates)
}

struct Stage(PathBuf);
impl Drop for Stage {fn drop(&mut self){let _=fs::remove_file(&self.0);}}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase { Preparing, Compressing, CheckingCompressed, CheckingContent, CheckingSource, Publishing, CheckingPublished }
pub type Progress<'a> = &'a mut dyn FnMut(Phase,u64,u64);
impl Phase {
    pub fn label(self,zh:bool)->&'static str {match (self,zh){
        (Self::Preparing,true)=>"准备",(Self::Preparing,false)=>"Preparing",
        (Self::Compressing,true)=>"压缩",(Self::Compressing,false)=>"Compressing",
        (Self::CheckingCompressed,true)=>"校验压缩文件",(Self::CheckingCompressed,false)=>"Checking compressed file",
        (Self::CheckingContent,true)=>"校验内容",(Self::CheckingContent,false)=>"Checking content",
        (Self::CheckingSource,true)=>"复查源文件",(Self::CheckingSource,false)=>"Checking source",
        (Self::Publishing,true)=>"写入压缩文件",(Self::Publishing,false)=>"Writing compressed file",
        (Self::CheckingPublished,true)=>"最终校验",(Self::CheckingPublished,false)=>"Final verification",
    }}
}
fn advertised_compression(output:&str)->bool{
    output.lines().any(|line|{let fields:Vec<_>=line.split_whitespace().collect();fields.first()==Some(&"local_thread_store_compression")&&fields.len()>=3&&!fields.contains(&"removed")&&matches!(fields.last(),Some(&"true"|&"false"))})
}
fn check_activity(stop:&Stop,state:Option<&State>,id:&str)->Result<()> {
    anyhow::ensure!(!stop.load(Ordering::Relaxed),"Cancelled");
    if let Some(state)=state {let live=state.lock().map_err(|_|anyhow::anyhow!("Cannot verify session activity"))?;
        anyhow::ensure!(!live.recent.rows.iter().chain(&live.recent.busy_metrics).any(|r|r.id==id&&r.state=="Busy"),"Session became active");}
    Ok(())
}
fn hash_reader(mut reader:impl Read,stop:&Stop,state:Option<&State>,id:&str,phase:Phase,total:u64,progress:Progress<'_>)->Result<([u8;32],u64)>{
    let mut hash=Sha256::new();let mut buf=[0u8;65536];let mut len=0u64;
    progress(phase,0,total);
    loop {check_activity(stop,state,id)?;let n=reader.read(&mut buf)?;if n==0{break}hash.update(&buf[..n]);len+=n as u64;progress(phase,len,total);}
    Ok((hash.finalize().into(),len))
}
fn open_db(path:&Path)->Result<Connection>{let db=Connection::open_with_flags(path,OpenFlags::SQLITE_OPEN_READ_ONLY|OpenFlags::SQLITE_OPEN_NO_MUTEX)?;db.busy_timeout(Duration::from_millis(500))?;db.execute_batch("PRAGMA query_only=ON; PRAGMA temp_store=MEMORY")?;Ok(db)}
fn same(meta:&fs::Metadata,other:&fs::Metadata)->bool{use std::os::windows::fs::MetadataExt;meta.len()==other.len()&&meta.creation_time()==other.creation_time()&&meta.last_write_time()==other.last_write_time()}
pub(crate) fn security_descriptor(path:&Path)->Result<Vec<u8>>{
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Security::{GetFileSecurityW,PSECURITY_DESCRIPTOR};
    let wide:Vec<u16>=path.as_os_str().encode_wide().chain(Some(0)).collect();let mut size=0u32;
    const OWNER_GROUP_DACL:u32=1|2|4;
    unsafe{let _=GetFileSecurityW(PCWSTR(wide.as_ptr()),OWNER_GROUP_DACL,PSECURITY_DESCRIPTOR(std::ptr::null_mut()),0,&mut size);}
    anyhow::ensure!((1..=262144).contains(&size),"Cannot inspect session file permissions");
    let mut data=vec![0u8;size as usize];
    unsafe{GetFileSecurityW(PCWSTR(wide.as_ptr()),OWNER_GROUP_DACL,PSECURITY_DESCRIPTOR(data.as_mut_ptr().cast()),size,&mut size).ok()?;}
    Ok(data)
}
pub(crate) fn verify_thread(id:&str,path:&Path)->Result<()> {
    anyhow::ensure!(id.len()==36 && id.chars().all(|c|c.is_ascii_hexdigit()||c=='-'),"Unsupported session ID");
    anyhow::ensure!(path.extension().is_some_and(|e|e=="jsonl"),"Not a plain rollout");
    let mut buf=Vec::new();use std::io::BufRead;
    let mut input=std::io::BufReader::new(File::open(path)?);input.by_ref().take(65536).read_until(b'\n',&mut buf)?;
    let value:serde_json::Value=serde_json::from_slice(&buf)?;
    anyhow::ensure!(value["type"]=="session_meta" && value["payload"]["id"].as_str()==Some(id),"Rollout identity does not match session");Ok(())
}
fn terminal_status(v:&str)->bool{matches!(v,"completed"|"failed"|"interrupted"|"cancelled"|"canceled")}
fn last_turn_status(history:&Connection,id:&str)->Result<Option<String>>{Ok(history.query_row("SELECT status FROM thread_turns WHERE thread_id=? ORDER BY rollout_ordinal DESC LIMIT 1",[id],|r|r.get::<_,String>(0)).optional()?)}
pub(crate) fn confirmed_idle(history:&Connection,id:&str)->Result<bool>{Ok(last_turn_status(history,id)?.is_some_and(|v|terminal_status(&v)))}
fn ensure_terminal_or_previously_confirmed(history:&Connection,id:&str,confirmed_unindexed:bool)->Result<()>{match last_turn_status(history,id)?{Some(v)=>anyhow::ensure!(terminal_status(&v),"Session became active"),None=>anyhow::ensure!(confirmed_unindexed,"Session activity is unknown")};Ok(())}
pub(crate) fn publication_lock(paths:&Paths,id:&str)->Result<(File,Option<File>)>{
    // Open only Codex's existing coordination files, never create or delete them.
    let locks=paths.home.join("thread-writer-locks");let coordination=OpenOptions::new().read(true).write(true).open(locks.join(".coordination.lock")).context("Codex writer coordination unavailable")?;
    coordination.try_lock().map_err(|_|anyhow::anyhow!("Codex writer coordination is busy"))?;
    let writer=match OpenOptions::new().read(true).write(true).open(locks.join(format!("{id}.lock"))){Ok(f)=>{f.try_lock().map_err(|_|anyhow::anyhow!("Session is in use"))?;Some(f)},Err(e) if e.kind()==std::io::ErrorKind::NotFound=>None,Err(e)=>return Err(e.into())};
    Ok((coordination,writer))
}
pub fn one(paths:&Paths,id:&str,stop:&Stop,activity:Option<&State>,progress:Progress<'_>)->Result<(u64,u64)> {
    one_with_idle_confirmation(paths,id,stop,activity,progress,&mut ||crate::desktop::confirm_unloaded_terminal(paths,id,stop))
}
fn one_with_idle_confirmation(paths:&Paths,id:&str,stop:&Stop,activity:Option<&State>,progress:Progress<'_>,confirm:&mut dyn FnMut()->Result<()>)->Result<(u64,u64)> {
    progress(Phase::Preparing,0,0);
    check_activity(stop,activity,id)?;
    let state=open_db(&paths.home.join("state_5.sqlite"))?;let history=open_db(&paths.home.join("thread_history_1.sqlite"))?;
    let (path,mode):(String,String)=state.query_row("SELECT rollout_path,coalesce(history_mode,'legacy') FROM threads WHERE id=? AND agent_path IS NULL",[id],|r|Ok((r.get(0)?,r.get(1)?)))?;
    anyhow::ensure!(mode=="paginated","Session requires history repair before reduction");
    let count:usize=state.query_row("SELECT count(*) FROM threads WHERE rollout_path=?",[&path],|r|r.get(0))?;
    anyhow::ensure!(count==1,"Shared rollout; automatic reduction skipped");
    let path=PathBuf::from(path);let absolute=path.canonicalize()?;
    let allowed=[paths.home.join("sessions"),paths.home.join("archived_sessions")].iter().filter_map(|p|p.canonicalize().ok()).any(|root|absolute.starts_with(root));
    anyhow::ensure!(allowed,"Session is outside local Codex storage");
    verify_thread(id,&path)?;
    let indexed=last_turn_status(&history,id)?;
    if let Some(status)=indexed.as_deref(){anyhow::ensure!(terminal_status(status),"Session is active or its state is unknown");}else{confirm().context("Cannot confirm an unindexed session is inactive")?;}
    check_activity(stop,activity,id)?;
    let before=fs::metadata(&path)?;anyhow::ensure!(!before.permissions().readonly(),"Read-only session file; reduction skipped");let original_security=security_descriptor(&path)?;let mut compressed=path.as_os_str().to_os_string();compressed.push(".zst");let destination=PathBuf::from(compressed);
    anyhow::ensure!(!destination.exists(),"A compressed file already exists");
    // Avoid spending time compressing a source that is already in use.
    drop(publication_lock(paths,id)?);
    let stage=Stage(paths.runtime.join(format!("reduce-{}-{}-{}.zst",id,std::process::id(),crate::now().to_bits())));
    let out=OpenOptions::new().write(true).create_new(true).open(&stage.0)?;
    let mut encoder=zstd::stream::write::Encoder::new(out,3)?;encoder.include_checksum(true)?;
    let mut source=File::open(&path)?;let mut hash=Sha256::new();let mut buf=[0u8;65536];let mut count=0u64;
    progress(Phase::Compressing,0,before.len());
    loop {check_activity(stop,activity,id)?;let n=source.read(&mut buf)?;if n==0{break}encoder.write_all(&buf[..n])?;hash.update(&buf[..n]);count+=n as u64;progress(Phase::Compressing,count,before.len());if count%(8*1024*1024)==0{std::thread::sleep(Duration::from_millis(20));}}
    let out=encoder.finish()?;out.sync_all()?;drop(out);
    anyhow::ensure!(count==before.len()&&same(&before,&fs::metadata(&path)?),"Session changed during compression");
    let expected:[u8;32]=hash.finalize().into();
    let compressed_len=fs::metadata(&stage.0)?.len();anyhow::ensure!(compressed_len<count,"Compression does not save space");
    let (encoded_hash,encoded_len)=hash_reader(File::open(&stage.0)?,stop,activity,id,Phase::CheckingCompressed,compressed_len,progress)?;
    anyhow::ensure!(encoded_len==compressed_len,"Compressed stage changed");
    let (decoded,decoded_len)=hash_reader(zstd::stream::read::Decoder::new(File::open(&stage.0)?)?,stop,activity,id,Phase::CheckingContent,count,progress)?;
    anyhow::ensure!(decoded==expected&&decoded_len==count,"Compressed data failed verification");
    let (current_hash,current_len)=hash_reader(File::open(&path)?,stop,activity,id,Phase::CheckingSource,count,progress)?;
    anyhow::ensure!(current_hash==expected&&current_len==count&&same(&before,&fs::metadata(&path)?),"Session content changed before publication");
    // Coordinate with the same lock Codex uses for session writers. Never create
    // or remove its lock files; older installations without this protocol skip.
    if indexed.is_none(){confirm().context("Cannot reconfirm the unindexed session is inactive")?;}
    let (coordination,writer)=publication_lock(paths,id)?;
    check_activity(stop,activity,id)?;
    ensure_terminal_or_previously_confirmed(&history,id,indexed.is_none())?;
    let (current,mode):(String,String)=state.query_row("SELECT rollout_path,coalesce(history_mode,'legacy') FROM threads WHERE id=? AND agent_path IS NULL",[id],|r|Ok((r.get(0)?,r.get(1)?)))?;
    anyhow::ensure!(mode=="paginated"&&Path::new(&current)==path && same(&before,&fs::metadata(&path)?),"Session changed before publication");
    let mut published=OpenOptions::new().write(true).create_new(true).open(&destination)?;
    let publish=(||->Result<()>{anyhow::ensure!(security_descriptor(&destination)?==original_security,"Compressed file permissions differ from the source");let mut input=File::open(&stage.0)?;let mut copied=0u64;progress(Phase::Publishing,0,compressed_len);loop{check_activity(stop,activity,id)?;let n=input.read(&mut buf)?;if n==0{break}published.write_all(&buf[..n])?;copied+=n as u64;progress(Phase::Publishing,copied,compressed_len);}published.set_times(std::fs::FileTimes::new().set_modified(before.modified()?))?;published.sync_all()?;anyhow::ensure!(copied==compressed_len,"Incomplete compressed file");
        let published_meta=published.metadata()?;let (actual,len)=hash_reader(File::open(&destination)?,stop,activity,id,Phase::CheckingPublished,compressed_len,progress)?;
        anyhow::ensure!(actual==encoded_hash&&len==compressed_len,"Published compressed file failed verification");
        anyhow::ensure!(same(&published_meta,&fs::metadata(&destination)?)&&same(&before,&fs::metadata(&path)?)&&security_descriptor(&path)?==original_security&&ensure_terminal_or_previously_confirmed(&history,id,indexed.is_none()).is_ok(),"Session changed before replacement");
        fs::remove_file(&path)?;Ok(())})();
    let owned=published.metadata().ok();drop(published);
    if publish.is_err()&&path.is_file()&&owned.as_ref().zip(fs::metadata(&destination).ok().as_ref()).is_some_and(|(a,b)|same(a,b)){let _=fs::remove_file(&destination);}publish?;
    drop(writer);drop(coordination);Ok((count,compressed_len))
}

/// Restore a compressed rollout without changing its contents or history index.
/// The compressed source remains in place until the complete plain file has
/// been published and verified while holding Codex's writer coordination lock.
pub fn decompress_one(paths:&Paths,id:&str,stop:&Stop,activity:Option<&State>,progress:Progress<'_>)->Result<(u64,u64)> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{MoveFileExW,MOVEFILE_WRITE_THROUGH};
    fn idle(stop:&Stop,activity:Option<&State>,history:&Connection,id:&str)->Result<()> {
        check_activity(stop,activity,id)?;
        let last:Option<String>=history.query_row("SELECT status FROM thread_turns WHERE thread_id=? ORDER BY rollout_ordinal DESC LIMIT 1",[id],|r|r.get(0)).optional()?;
        if let Some(last)=last {anyhow::ensure!(matches!(last.as_str(),"completed"|"failed"|"interrupted"|"cancelled"|"canceled"),"Session is active or its state is unknown");}
        else if let Some(activity)=activity {let live=activity.lock().map_err(|_|anyhow::anyhow!("Cannot verify session activity"))?;anyhow::ensure!(!live.recent.stale&&live.recent.error.is_empty()&&live.recent.busy_count.is_some(),"Session activity is unavailable");}
        else {anyhow::bail!("Session is active or its state is unknown")}
        Ok(())
    }
    let state=open_db(&paths.home.join("state_5.sqlite"))?;let history=open_db(&paths.home.join("thread_history_1.sqlite"))?;
    let (name,mode):(String,String)=state.query_row("SELECT rollout_path,coalesce(history_mode,'legacy') FROM threads WHERE id=? AND agent_path IS NULL",[id],|r|Ok((r.get(0)?,r.get(1)?)))?;
    anyhow::ensure!(mode=="paginated","Session requires history repair before decompression");
    let mut plain=PathBuf::from(&name);let compressed=if plain.extension().is_some_and(|v|v=="zst"){let source=plain.clone();plain.set_extension("");source}else{let mut source=plain.as_os_str().to_os_string();source.push(".zst");PathBuf::from(source)};
    anyhow::ensure!(plain.extension().is_some_and(|v|v=="jsonl"),"Unsupported session path");
    anyhow::ensure!(Path::new(&name)==plain,"Compressed metadata path requires repair");
    let normalized=|s:&str|s.replace('/',"\\").trim_start_matches(r"\\?\").to_ascii_lowercase();let expected=normalized(&name);
    let mut q=state.prepare("SELECT rollout_path FROM threads WHERE id<>? AND rollout_path IS NOT NULL")?;let mut rows=q.query([id])?;while let Some(row)=rows.next()?{let other:String=row.get(0)?;let other=normalized(&other);anyhow::ensure!(other!=expected&&other!=normalized(&plain.to_string_lossy())&&other!=normalized(&compressed.to_string_lossy()),"Shared rollout; decompression skipped");}drop(rows);drop(q);
    let resolved=compressed.canonicalize()?;anyhow::ensure!([paths.home.join("sessions"),paths.home.join("archived_sessions")].iter().filter_map(|p|p.canonicalize().ok()).any(|p|resolved.starts_with(p)),"Session is outside local Codex storage");
    anyhow::ensure!(plain.parent().and_then(|p|p.canonicalize().ok())==resolved.parent().map(Path::to_path_buf),"Invalid session directory");
    let before=fs::metadata(&compressed)?;anyhow::ensure!(!before.permissions().readonly(),"Read-only compressed session; decompression skipped");idle(stop,activity,&history,id)?;drop(publication_lock(paths,id)?);
    let (initial_hash,initial_len)=hash_reader(File::open(&compressed)?,stop,activity,id,Phase::CheckingSource,before.len(),progress)?;anyhow::ensure!(initial_len==before.len()&&same(&before,&fs::metadata(&compressed)?),"Compressed session changed");
    let stage_path=paths.runtime.join(format!("decompress-{}-{}-{}.jsonl",id,std::process::id(),crate::now().to_bits()));
    let mut output=OpenOptions::new().write(true).create_new(true).open(&stage_path)?;let stage=Stage(stage_path);let mut decoder=zstd::stream::read::Decoder::new(File::open(&compressed)?)?;let mut buffer=[0u8;65536];let mut size=0u64;let mut hash=Sha256::new();progress(Phase::Compressing,0,before.len());
    let mut last_idle=Instant::now()-Duration::from_secs(1);loop {check_activity(stop,activity,id)?;if last_idle.elapsed()>=Duration::from_millis(500){idle(stop,activity,&history,id)?;last_idle=Instant::now();}let n=decoder.read(&mut buffer)?;if n==0{break}output.write_all(&buffer[..n])?;hash.update(&buffer[..n]);size+=n as u64;progress(Phase::Compressing,size,0);}
    output.sync_all()?;drop(output);verify_thread(id,&stage.0)?;let expected_hash:[u8;32]=hash.finalize().into();
    let (checked,checked_len)=hash_reader(File::open(&stage.0)?,stop,activity,id,Phase::CheckingContent,size,progress)?;anyhow::ensure!(checked==expected_hash&&checked_len==size,"Decompressed content failed verification");
    let (source_hash,source_len)=hash_reader(File::open(&compressed)?,stop,activity,id,Phase::CheckingSource,before.len(),progress)?;anyhow::ensure!(source_hash==initial_hash&&source_len==initial_len&&same(&before,&fs::metadata(&compressed)?),"Compressed session changed during decompression");
    let (_coordination,_writer)=publication_lock(paths,id)?;idle(stop,activity,&history,id)?;
    let (current,mode):(String,String)=state.query_row("SELECT rollout_path,coalesce(history_mode,'legacy') FROM threads WHERE id=? AND agent_path IS NULL",[id],|r|Ok((r.get(0)?,r.get(1)?)))?;anyhow::ensure!(current==name&&mode=="paginated"&&same(&before,&fs::metadata(&compressed)?),"Session changed before publication");
    let (again,again_len)=hash_reader(File::open(&compressed)?,stop,activity,id,Phase::CheckingSource,before.len(),progress)?;anyhow::ensure!(again==source_hash&&again_len==source_len,"Compressed session changed before publication");
    if !plain.exists() {
        let parent=plain.parent().context("Missing session directory")?;let adjacent_path=parent.join(format!(".codex-session-bar-decompress-{}-{}-{}.tmp",id,std::process::id(),crate::now().to_bits()));
        let mut output=OpenOptions::new().write(true).create_new(true).open(&adjacent_path)?;let adjacent=Stage(adjacent_path);crate::clean::copy_candidate_security(&compressed,&adjacent.0)?;let mut input=File::open(&stage.0)?;let mut copied=0;progress(Phase::Publishing,0,size);
        let mut last_idle=Instant::now()-Duration::from_secs(1);loop {check_activity(stop,activity,id)?;if last_idle.elapsed()>=Duration::from_millis(500){idle(stop,activity,&history,id)?;last_idle=Instant::now();}let n=input.read(&mut buffer)?;if n==0{break}output.write_all(&buffer[..n])?;copied+=n as u64;progress(Phase::Publishing,copied,size);}output.set_times(std::fs::FileTimes::new().set_modified(before.modified()?))?;output.sync_all()?;drop(output);
        let (actual,len)=hash_reader(File::open(&adjacent.0)?,stop,activity,id,Phase::CheckingPublished,size,progress)?;anyhow::ensure!(actual==expected_hash&&len==size&&crate::clean::security_sddl(&compressed)?==crate::clean::security_sddl(&adjacent.0)?,"Published candidate failed verification");
        idle(stop,activity,&history,id)?;let (final_source,final_len)=hash_reader(File::open(&compressed)?,stop,activity,id,Phase::CheckingSource,before.len(),progress)?;anyhow::ensure!(final_source==source_hash&&final_len==source_len&&same(&before,&fs::metadata(&compressed)?),"Compressed session changed before publication");
        let from:Vec<u16>=adjacent.0.as_os_str().encode_wide().chain(Some(0)).collect();let to:Vec<u16>=plain.as_os_str().encode_wide().chain(Some(0)).collect();unsafe{MoveFileExW(PCWSTR(from.as_ptr()),PCWSTR(to.as_ptr()),MOVEFILE_WRITE_THROUGH).context("Cannot publish decompressed session")?;}
    }
    // A prior interruption may leave both copies. Only remove the compressed
    // file when the existing plain file is byte-identical to decoded content.
    anyhow::ensure!(fs::symlink_metadata(&plain)?.file_type().is_file()&&plain.canonicalize()?.parent()==resolved.parent(),"Invalid plain session file");verify_thread(id,&plain)?;let (actual,len)=hash_reader(File::open(&plain)?,stop,activity,id,Phase::CheckingPublished,size,progress)?;
    anyhow::ensure!(actual==expected_hash&&len==size,"Existing plain session differs; both files were retained");idle(stop,activity,&history,id)?;anyhow::ensure!(same(&before,&fs::metadata(&compressed)?),"Compressed session changed; both files were retained");
    fs::remove_file(&compressed)?;Ok((before.len(),size))
}

#[cfg(test)]
mod tests {
    use super::*;use std::sync::{Arc,atomic::AtomicBool};
    const ID:&str="11111111-1111-4111-8111-111111111111";
    fn run(paths:&Paths,id:&str,stop:&Stop,activity:Option<&State>)->Result<(u64,u64)>{one(paths,id,stop,activity,&mut |_,_,_|{})}
    #[test]
    fn compatibility_requires_complete_supported_feature_entry(){
        assert!(advertised_compression("local_thread_store_compression under development false\n"));
        assert!(!advertised_compression("local_thread_store_compression removed false\n"));
        assert!(!advertised_compression("local_thread_store_compression\n"));
        assert!(!advertised_compression("other_feature stable true\n"));
    }
    fn fixture(name:&str)->(Paths,PathBuf,Vec<u8>,Stop){
        let base=Paths::discover().unwrap();let dir=base.runtime.join(format!("reduce-fixture-{}-{name}",std::process::id()));
        let sessions=dir.join("sessions");let locks=dir.join("thread-writer-locks");fs::create_dir_all(&sessions).unwrap();fs::create_dir_all(&locks).unwrap();
        fs::write(locks.join(".coordination.lock"),[]).unwrap();
        let path=sessions.join(format!("rollout-2026-10-01T00-00-00-{ID}.jsonl"));
        let stamp="2026-10-01T00:00:00Z";
        let data=format!("{}\n{}\n",serde_json::json!({"timestamp":stamp,"type":"session_meta","payload":{"id":ID,"timestamp":stamp,"cwd":dir.to_string_lossy(),"originator":"codex_cli_rs","cli_version":"0.166.0","source":"cli","model_provider":"openai"}}),serde_json::json!({"timestamp":stamp,"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"x".repeat(32000)}]}})).into_bytes();fs::write(&path,&data).unwrap();
        let state=Connection::open(dir.join("state_5.sqlite")).unwrap();state.execute_batch("CREATE TABLE threads(id TEXT,rollout_path TEXT,agent_path TEXT,history_mode TEXT)").unwrap();state.execute("INSERT INTO threads VALUES(?,?,NULL,'paginated')",rusqlite::params![ID,path.to_string_lossy()]).unwrap();
        let history=Connection::open(dir.join("thread_history_1.sqlite")).unwrap();history.execute_batch("CREATE TABLE thread_turns(thread_id TEXT,status TEXT,rollout_ordinal INTEGER);INSERT INTO thread_turns VALUES('11111111-1111-4111-8111-111111111111','completed',1)").unwrap();
        (Paths{home:dir.clone(),app:dir.clone(),runtime:dir,instance:String::new()},path,data,Arc::new(AtomicBool::new(false)))
    }
    #[test]
    fn reduce_preserves_every_byte_in_codex_zstd_format(){
        let (paths,path,data,stop)=fixture("lossless");let (before,after)=run(&paths,ID,&stop,None).unwrap();assert_eq!(before,data.len() as u64);assert!(after<before);assert!(!path.exists());
        let z=PathBuf::from(format!("{}.zst",path.display()));let mut restored=Vec::new();zstd::stream::read::Decoder::new(File::open(z).unwrap()).unwrap().read_to_end(&mut restored).unwrap();assert_eq!(restored,data);
    }
    #[test]
    fn unindexed_compression_requires_fresh_confirmation_and_is_lossless(){
        let (paths,path,data,stop)=fixture("compress-no-index");let db=Connection::open(paths.home.join("thread_history_1.sqlite")).unwrap();db.execute("DELETE FROM thread_turns",[]).unwrap();drop(db);
        let mut denied=||Err(anyhow::anyhow!("unconfirmed"));assert!(one_with_idle_confirmation(&paths,ID,&stop,None,&mut |_,_,_|{},&mut denied).is_err());assert_eq!(fs::read(&path).unwrap(),data);
        let mut calls=0;let mut confirmed=||{calls+=1;Ok(())};let (_,after)=one_with_idle_confirmation(&paths,ID,&stop,None,&mut |_,_,_|{},&mut confirmed).unwrap();assert_eq!(calls,2);assert!(after<data.len() as u64);let z=PathBuf::from(format!("{}.zst",path.display()));let mut restored=Vec::new();zstd::stream::read::Decoder::new(File::open(z).unwrap()).unwrap().read_to_end(&mut restored).unwrap();assert_eq!(restored,data);
    }
    #[test]
    fn unindexed_compression_refuses_new_active_turn_before_publish(){
        let (paths,path,data,stop)=fixture("compress-no-index-active");let history=paths.home.join("thread_history_1.sqlite");let db=Connection::open(&history).unwrap();db.execute("DELETE FROM thread_turns",[]).unwrap();drop(db);let mut calls=0;let mut confirmed=||{calls+=1;if calls==2{let db=Connection::open(&history).unwrap();db.execute("INSERT INTO thread_turns VALUES(?,'inProgress',1)",[ID]).unwrap();}Ok(())};assert!(one_with_idle_confirmation(&paths,ID,&stop,None,&mut |_,_,_|{},&mut confirmed).is_err());assert_eq!(fs::read(&path).unwrap(),data);assert!(!PathBuf::from(format!("{}.zst",path.display())).exists());
    }
    #[test]
    fn decompress_roundtrip_and_interrupted_duplicate_are_lossless(){
        let (paths,path,data,stop)=fixture("decompress");let compressed=PathBuf::from(format!("{}.zst",path.display()));fs::write(&compressed,zstd::stream::encode_all(data.as_slice(),1).unwrap()).unwrap();fs::remove_file(&path).unwrap();
        let (before,after)=decompress_one(&paths,ID,&stop,None,&mut |_,_,_|{}).unwrap();assert!(before<after);assert_eq!(after,data.len() as u64);assert_eq!(fs::read(&path).unwrap(),data);assert!(!compressed.exists());
        fs::write(&compressed,zstd::stream::encode_all(data.as_slice(),1).unwrap()).unwrap();decompress_one(&paths,ID,&stop,None,&mut |_,_,_|{}).unwrap();assert_eq!(fs::read(&path).unwrap(),data);assert!(!compressed.exists());
    }
    #[test]
    fn decompress_refuses_changed_destination_corrupt_data_and_active_session(){
        let (paths,path,data,stop)=fixture("decompress-conflict");let compressed=PathBuf::from(format!("{}.zst",path.display()));fs::write(&compressed,zstd::stream::encode_all(data.as_slice(),1).unwrap()).unwrap();fs::write(&path,b"different").unwrap();
        assert!(decompress_one(&paths,ID,&stop,None,&mut |_,_,_|{}).is_err());assert_eq!(fs::read(&path).unwrap(),b"different");assert!(compressed.exists());fs::remove_file(&path).unwrap();fs::write(&compressed,b"invalid zstd").unwrap();assert!(decompress_one(&paths,ID,&stop,None,&mut |_,_,_|{}).is_err());assert!(!path.exists());assert!(compressed.exists());
        fs::write(&compressed,zstd::stream::encode_all(data.as_slice(),1).unwrap()).unwrap();let db=Connection::open(paths.home.join("thread_history_1.sqlite")).unwrap();db.execute("UPDATE thread_turns SET status='inProgress'",[]).unwrap();assert!(decompress_one(&paths,ID,&stop,None,&mut |_,_,_|{}).is_err());assert!(!path.exists());assert!(compressed.exists());
    }
    #[test]
    fn decompress_without_index_requires_fresh_visible_activity(){
        let (paths,path,data,stop)=fixture("decompress-no-index");let compressed=PathBuf::from(format!("{}.zst",path.display()));fs::write(&compressed,zstd::stream::encode_all(data.as_slice(),1).unwrap()).unwrap();fs::remove_file(&path).unwrap();let db=Connection::open(paths.home.join("thread_history_1.sqlite")).unwrap();db.execute("DELETE FROM thread_turns",[]).unwrap();drop(db);
        assert!(decompress_one(&paths,ID,&stop,None,&mut |_,_,_|{}).is_err());let state=Arc::new(std::sync::Mutex::new(crate::Shared::default()));{let mut live=state.lock().unwrap();live.recent=Arc::new(crate::data::Recent{stale:false,busy_count:Some(0),rows:vec![crate::data::Row{id:ID.into(),state:"Done".into(),..Default::default()}],..Default::default()});}
        decompress_one(&paths,ID,&stop,Some(&state),&mut |_,_,_|{}).unwrap();assert_eq!(fs::read(&path).unwrap(),data);assert!(!compressed.exists());
    }
    #[test]
    #[ignore = "Explicit isolated native Codex fixture; modifies only workspace tmp"]
    fn native_decompress_without_projection(){
        let root=PathBuf::from(std::env::var_os("CODEX_CLEAN_NATIVE_FIXTURE").expect("fixture required")).canonicalize().unwrap();let workspace=Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();assert!(root.starts_with(workspace.join("tmp").canonicalize().unwrap())&&root.file_name().unwrap()=="codex-native-recovery-20261031");
        let home=root.join("home").canonicalize().unwrap();let id=fs::read_to_string(root.join("thread-id.txt")).unwrap();let id=id.trim();let state_db=Connection::open(home.join("state_5.sqlite")).unwrap();let path=PathBuf::from(state_db.query_row("SELECT rollout_path FROM threads WHERE id=?",[id],|r|r.get::<_,String>(0)).unwrap());assert!(path.canonicalize().unwrap().starts_with(&home));let data=fs::read(&path).unwrap();assert!(String::from_utf8_lossy(&data).contains("[Content cleaned by Codex Session Bar]"));let compressed=PathBuf::from(format!("{}.zst",path.display()));assert!(!compressed.exists());fs::write(&compressed,zstd::stream::encode_all(data.as_slice(),1).unwrap()).unwrap();fs::remove_file(&path).unwrap();
        let db=Connection::open(home.join("thread_history_1.sqlite")).unwrap();db.execute("DELETE FROM thread_items WHERE thread_id=?",[id]).unwrap();db.execute("DELETE FROM thread_turns WHERE thread_id=?",[id]).unwrap();db.execute("DELETE FROM thread_history_projection_state WHERE thread_id=?",[id]).unwrap();drop(db);
        let state=Arc::new(std::sync::Mutex::new(crate::Shared::default()));{let mut live=state.lock().unwrap();live.recent=Arc::new(crate::data::Recent{stale:false,busy_count:Some(0),rows:vec![crate::data::Row{id:id.into(),state:"Done".into(),..Default::default()}],..Default::default()});}
        let paths=Paths{home,app:root.clone(),runtime:root,instance:String::new()};let stop=Arc::new(AtomicBool::new(false));decompress_one(&paths,id,&stop,Some(&state),&mut |_,_,_|{}).unwrap();assert_eq!(fs::read(&path).unwrap(),data);assert!(!compressed.exists());
    }
    #[test]
    fn progress_reports_real_bytes_for_each_processing_stage(){
        let (paths,_,data,stop)=fixture("progress");let mut events=Vec::new();
        let (before,after)=one(&paths,ID,&stop,None,&mut |phase,done,total|events.push((phase,done,total))).unwrap();
        assert_eq!(before,data.len() as u64);
        for (phase,total) in [(Phase::Compressing,before),(Phase::CheckingCompressed,after),(Phase::CheckingContent,before),(Phase::CheckingSource,before),(Phase::Publishing,after),(Phase::CheckingPublished,after)] {
            let items:Vec<_>=events.iter().filter(|e|e.0==phase).collect();assert!(!items.is_empty());assert_eq!((items[0].1,items[0].2),(0,total));assert_eq!((items.last().unwrap().1,items.last().unwrap().2),(total,total));assert!(items.windows(2).all(|w|w[0].1<=w[1].1));
        }
    }
    #[test]
    fn active_writer_and_existing_destination_never_replace_original(){
        let (paths,path,data,stop)=fixture("writer");let lock=OpenOptions::new().read(true).write(true).create(true).open(paths.home.join("thread-writer-locks").join(format!("{ID}.lock"))).unwrap();lock.try_lock().unwrap();
        assert!(run(&paths,ID,&stop,None).is_err());assert_eq!(fs::read(&path).unwrap(),data);drop(lock);
        let z=PathBuf::from(format!("{}.zst",path.display()));fs::write(&z,b"sentinel").unwrap();assert!(run(&paths,ID,&stop,None).is_err());assert_eq!(fs::read(&path).unwrap(),data);assert_eq!(fs::read(z).unwrap(),b"sentinel");
    }
    #[test]
    fn unknown_and_active_sessions_are_skipped(){
        let (paths,path,data,stop)=fixture("active");let db=Connection::open(paths.home.join("thread_history_1.sqlite")).unwrap();db.execute("UPDATE thread_turns SET status='inProgress'",[]).unwrap();assert!(run(&paths,ID,&stop,None).is_err());assert_eq!(fs::read(&path).unwrap(),data);
        db.execute("DELETE FROM thread_turns",[]).unwrap();assert!(run(&paths,ID,&stop,None).is_err());assert_eq!(fs::read(&path).unwrap(),data);
    }
    #[test]
    fn cancellation_and_missing_codex_lock_leave_the_original_untouched(){
        let (paths,path,data,stop)=fixture("cancel");let compressed=PathBuf::from(format!("{}.zst",path.display()));
        stop.store(true,Ordering::Relaxed);assert!(run(&paths,ID,&stop,None).is_err());assert_eq!(fs::read(&path).unwrap(),data);assert!(!compressed.exists());
        stop.store(false,Ordering::Relaxed);fs::remove_file(paths.home.join("thread-writer-locks").join(".coordination.lock")).unwrap();
        assert!(run(&paths,ID,&stop,None).is_err());assert_eq!(fs::read(&path).unwrap(),data);assert!(!compressed.exists());
    }
    #[test]
    fn legacy_history_is_not_compressed_before_repair(){
        let (paths,path,data,stop)=fixture("legacy");let db=Connection::open(paths.home.join("state_5.sqlite")).unwrap();
        db.execute("UPDATE threads SET history_mode='legacy'",[]).unwrap();assert!(run(&paths,ID,&stop,None).is_err());assert_eq!(fs::read(&path).unwrap(),data);
        assert!(!PathBuf::from(format!("{}.zst",path.display())).exists());
    }
    #[test]
    fn live_activity_cancels_before_publication(){
        let (paths,path,data,stop)=fixture("live");
        let state=Arc::new(std::sync::Mutex::new(crate::Shared::default()));
        struct Activate(crate::State);
        impl Read for Activate {fn read(&mut self,buf:&mut[u8])->std::io::Result<usize>{
            let mut live=self.0.lock().unwrap();live.recent=Arc::new(crate::data::Recent{busy_metrics:vec![crate::data::Row{id:ID.into(),state:"Busy".into(),..Default::default()}],..Default::default()});buf[0]=42;Ok(1)
        }}
        assert!(hash_reader(Activate(state.clone()),&stop,Some(&state),ID,Phase::CheckingSource,1,&mut |_,_,_|{}).is_err());
        assert!(run(&paths,ID,&stop,Some(&state)).is_err());assert_eq!(fs::read(&path).unwrap(),data);
        assert!(!PathBuf::from(format!("{}.zst",path.display())).exists());
    }
}
