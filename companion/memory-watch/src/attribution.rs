//! Best-effort, read-only process context. Optional metadata never authorizes
//! termination. Only explicitly allowed session identifiers are retained.
use std::{collections::HashMap,path::{Path,PathBuf},time::Duration};
use serde::{Serialize,Deserialize};
use windows::{core::{s,w},Win32::{Foundation::*,System::{Diagnostics::Debug::ReadProcessMemory,LibraryLoader::*}}};
#[derive(Clone,Default,Serialize,Deserialize)]
pub struct Context {pub cwd:String,pub session_id:String,pub session_title:String,pub evidence:String,pub candidates:Vec<String>}
fn normalize(path:&str)->String {path.trim_start_matches(r"\\?\").replace('/',"\\").trim_end_matches('\\').to_lowercase()}
fn session_key(value:&str)->bool {value.len()==36&&value.bytes().enumerate().all(|(i,b)|if [8,13,18,23].contains(&i){b==b'-'}else{b.is_ascii_hexdigit()})}
fn allowed_session(environment:&str)->String {
    for key in ["CODEX_THREAD_ID","CODEX_SESSION_ID","CODEX_THREAD_UUID"] {
        if let Some(value)=environment.split('\0').filter_map(|p|p.split_once('=')).find_map(|(k,v)|(k==key&&session_key(v)).then_some(v)){return value.to_owned();}
    }String::new()
}
unsafe fn bytes(h:HANDLE,address:usize,size:usize)->Option<Vec<u8>> {
    if !(0x10000..0x0000_8000_0000_0000).contains(&address)||size==0||size>65536{return None;}
    let mut data=vec![0;size];let mut read=0;
    let _=ReadProcessMemory(h,address as *const _,data.as_mut_ptr() as *mut _,size,Some(&mut read));
    if read==0{return None;}data.truncate(read);Some(data)
}
fn pointer(data:&[u8],at:usize)->Option<usize>{Some(u64::from_le_bytes(data.get(at..at+8)?.try_into().ok()?) as usize)}
/// Native x64 PEB fields are optional and bounds-checked. Unsupported layouts
/// or access failures yield no context; safety decisions use separate handles.
pub unsafe fn process(h:HANDLE)->Context {
    let mut result=Context::default();
    #[cfg(target_pointer_width="64")]
    let read=(||->Option<(String,String)>{
        type Query=unsafe extern "system" fn(HANDLE,u32,*mut core::ffi::c_void,u32,*mut u32)->i32;
        let query:Query=std::mem::transmute(GetProcAddress(GetModuleHandleW(w!("ntdll.dll")).ok()?,s!("NtQueryInformationProcess"))?);
        let mut info=[0u8;48];let mut needed=0;if query(h,0,info.as_mut_ptr() as *mut _,48,&mut needed)<0{return None;}
        let peb=pointer(&info,8)?;let parameters=pointer(&bytes(h,peb+0x20,8)?,0)?;
        let params=bytes(h,parameters,0x88)?;if params.len()!=0x88||params[8]&1==0{return None;}
        let length=u16::from_le_bytes(params.get(0x38..0x3a)?.try_into().ok()?) as usize;
        if length==0||length>16384||length%2!=0{return None;}
        let cwd=bytes(h,pointer(&params,0x40)?,length)?;if cwd.len()!=length{return None;}
        let decode=|data:&[u8]|String::from_utf16_lossy(&data.chunks_exact(2).map(|p|u16::from_le_bytes([p[0],p[1]])).collect::<Vec<_>>());
        let cwd=decode(&cwd);if !(cwd.starts_with(r"\\")||cwd.as_bytes().get(1)==Some(&b':')){return None;}
        let session=pointer(&params,0x80).and_then(|p|bytes(h,p,65536)).map(|data|allowed_session(&decode(&data))).unwrap_or_default();
        Some((cwd,session))
    })();
    #[cfg(not(target_pointer_width="64"))]let read:Option<(String,String)>=None;
    if let Some((cwd,session))=read {result.cwd=cwd;result.session_id=session;}
    result
}
pub fn resolve(context:&mut Context,home:&Path) {
    if !context.session_id.is_empty(){context.evidence="process_environment".into();}
    let path=home.join("state_5.sqlite");
    let Ok(db)=rusqlite::Connection::open_with_flags(path,rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY|rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX)else{return;};
    let _=db.busy_timeout(Duration::from_millis(25));let _=db.execute_batch("PRAGMA query_only=ON;PRAGMA temp_store=MEMORY;PRAGMA cache_size=-256;");
    let Ok(mut query)=db.prepare("SELECT id,substr(coalesce(nullif(name,''),title),1,80),cwd FROM threads WHERE archived=0 AND agent_path IS NULL ORDER BY updated_at DESC LIMIT 512")else{return;};
    let Ok(rows)=query.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?.unwrap_or_default())))else{return;};
    for row in rows.flatten(){
        if !context.session_id.is_empty()&&row.0==context.session_id{context.session_title=row.1;context.evidence="process_environment".into();return;}
        if !context.cwd.is_empty()&&normalize(&row.2)==normalize(&context.cwd)&&context.candidates.len()<3 {context.candidates.push(format!("{} ({})",row.1.chars().take(48).collect::<String>(),row.0));}
    }
    if context.session_id.is_empty(){context.evidence=if context.candidates.is_empty(){"unavailable"}else{"cwd_candidates_only"}.into();}
}
pub fn codex_home()->Option<PathBuf>{std::env::var_os("CODEX_HOME").map(PathBuf::from).or_else(||std::env::var_os("USERPROFILE").map(|p|PathBuf::from(p).join(".codex")))}
#[derive(Default)]pub struct Traffic {seen:HashMap<(u32,i64),f64>}
impl Traffic {
    pub fn observe(&mut self,items:impl Iterator<Item=(u32,i64)>,at:f64)->usize {
        self.seen.retain(|_,born|*born>=at-60.);
        for identity in items {let born=identity.1 as f64/10_000_000.-11_644_473_600.;if born>=at-60.&&born<=at+1.&&self.seen.len()<8192{self.seen.entry(identity).or_insert(born);}}
        self.seen.len()
    }
}
#[cfg(test)]mod tests {
    use super::*;
    #[test]fn session_filter_never_retains_unrelated_environment(){
        assert_eq!(allowed_session("API_KEY=secret\0CODEX_THREAD_ID=01a14d83-72e0-7e41-a98c-8d7dbf8d0174\0"),"01a14d83-72e0-7e41-a98c-8d7dbf8d0174");
        assert!(allowed_session("CODEX_THREAD_ID=not-a-session\0TOKEN=private").is_empty());
    }
    #[test]fn rolling_births_are_unique_and_expire(){let mut traffic=Traffic::default();let ticks=|at:f64|((at+11644473600.)*1e7) as i64;assert_eq!(traffic.observe([(1,ticks(995.)),(2,ticks(900.))].into_iter(),1000.),1);assert_eq!(traffic.observe([(1,ticks(995.))].into_iter(),1001.),1);assert_eq!(traffic.observe(std::iter::empty(),1060.),0);}
    #[test]fn current_process_context_is_read_only(){unsafe{let c=process(windows::Win32::System::Threading::GetCurrentProcess());assert_eq!(normalize(&c.cwd),normalize(&std::env::current_dir().unwrap().to_string_lossy()));}}
    #[test]fn directory_matches_remain_candidates_until_session_id_is_present(){
        let root=crate::workspace_root(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        let temp=root.join("tmp").canonicalize().unwrap();let folder=temp.join(format!("guard-attribution-test-{}",std::process::id()));
        assert!(folder.starts_with(&temp));std::fs::create_dir_all(&folder).unwrap();
        let db=rusqlite::Connection::open(folder.join("state_5.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE threads(id TEXT,name TEXT,title TEXT,cwd TEXT,archived INTEGER,agent_path TEXT,updated_at INTEGER); INSERT INTO threads VALUES('00000000-0000-4000-8000-000000000001','','fallback title','D:\\project',0,NULL,2),('00000000-0000-4000-8000-000000000002','renamed title','old title','D:\\project',0,NULL,1);").unwrap();drop(db);
        let mut context=Context{cwd:r"\\?\D:\project".into(),..Context::default()};resolve(&mut context,&folder);
        assert_eq!(context.evidence,"cwd_candidates_only");assert_eq!(context.candidates.len(),2);assert!(context.session_title.is_empty());
        context.session_id="00000000-0000-4000-8000-000000000002".into();resolve(&mut context,&folder);
        assert_eq!(context.session_title,"renamed title");assert_eq!(context.evidence,"process_environment");
        std::fs::remove_dir_all(folder).unwrap();
    }
}
