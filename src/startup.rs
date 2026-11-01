//! User-controlled per-login scheduled task. All task operations run off the UI thread.
use std::{os::windows::process::CommandExt,process::{Command,Output,Stdio},sync::{Arc,Mutex},thread};
const NAME:&str="Codex Session Bar";
#[derive(Clone,Default)]pub struct View{pub enabled:bool,pub exists:bool,pub busy:bool,pub error:String}
#[derive(Clone,Default)]pub struct Service{pub view:Arc<Mutex<View>>}
fn command(args:&[&str])->std::io::Result<Output>{Command::new("schtasks.exe").args(args).creation_flags(0x08000000).stdin(Stdio::null()).output()}
fn xml_text(bytes:&[u8])->String {if bytes.starts_with(&[0xff,0xfe]){String::from_utf16_lossy(&bytes[2..].chunks_exact(2).map(|b|u16::from_le_bytes([b[0],b[1]])).collect::<Vec<_>>())}else{String::from_utf8_lossy(bytes).replace('\0',"")}}
fn task_enabled(xml:&str)->Option<bool>{let lower=xml.to_lowercase();let part=lower.split("<settings>").nth(1)?.split("</settings>").next()?;Some(!part.contains("<enabled>false</enabled>"))}
fn query()->View {match command(&["/Query","/TN",NAME,"/XML"]){Ok(out) if out.status.success()=>{let xml=xml_text(&out.stdout);if let Some(enabled)=task_enabled(&xml){View{exists:true,enabled,..Default::default()}}else{View{error:"Cannot read scheduled task settings".into(),..Default::default()}}},Ok(_)=>View::default(),Err(e)=>View{error:format!("Cannot query startup: {e}"),..Default::default()}}}
impl Service {
 pub fn refresh(&self){let output=self.view.clone();{let mut v=output.lock().unwrap();if v.busy{return}v.busy=true;}thread::spawn(move||{let result=query();*output.lock().unwrap()=result;});}
 pub fn toggle(&self){let output=self.view.clone();{let mut v=output.lock().unwrap();if v.busy{return}v.busy=true;}thread::spawn(move||{
  let before=query();let result=if !before.error.is_empty(){Err(before.error.clone())}else if before.exists {command(&["/Change","/TN",NAME,if before.enabled{"/Disable"}else{"/Enable"}]).map_err(|e|e.to_string())}else{match std::env::current_exe(){Ok(path)=>{let action=format!("\"{}\"",path.display());command(&["/Create","/TN",NAME,"/SC","ONLOGON","/RL","LIMITED","/TR",&action]).map_err(|e|e.to_string())},Err(e)=>Err(e.to_string())}};
  let next=match result{Ok(out) if out.status.success()=>query(),Ok(out)=>{let error=xml_text(&out.stderr);View{error:if error.trim().is_empty(){"Could not update startup task".into()}else{error.chars().take(300).collect()},..before}},Err(error)=>View{error,..before}};*output.lock().unwrap()=next;
 });}
}
#[cfg(test)]mod tests{use super::*;#[test]fn decodes_windows_task_xml(){assert!(xml_text(&[0xff,0xfe,65,0]).starts_with('A'));assert_eq!(xml_text(b"<Settings>"),"<Settings>");assert_eq!(task_enabled("<Triggers><Enabled>false</Enabled></Triggers><Settings></Settings>"),Some(true));assert_eq!(task_enabled("<Settings><Enabled>false</Enabled></Settings>"),Some(false));}}
