use crate::{data::Store, now, Paths, State, Stop};
use anyhow::Result;
use std::{
    sync::atomic::Ordering,
    thread,
    time::{Duration, Instant},
};
use windows::{
    core::{PWSTR, VARIANT},
    Win32::{
        Foundation::*,
        System::{Com::*, Threading::*},
        UI::{Accessibility::*, HiDpi::*, WindowsAndMessaging::*},
    },
};

pub unsafe fn visible_windows() -> Vec<isize> {
    unsafe extern "system" fn visit(hwnd: HWND, param: LPARAM) -> BOOL {
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return true.into();
        }
        let mut title = [0u16; 256];
        let length = GetWindowTextW(hwnd, &mut title);
        let caption = String::from_utf16_lossy(&title[..length as usize]).to_lowercase();
        let owned = GetWindow(hwnd, GW_OWNER).is_ok_and(|h| !h.is_invalid());
        if !main_window_candidate(
            &caption,
            owned,
            GetWindowLongW(hwnd, GWL_STYLE) as u32,
            GetWindowLongW(hwnd, GWL_EXSTYLE) as u32,
        ) {
            return true.into();
        }
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            let mut name = [0u16; 1024];
            let mut count = name.len() as u32;
            if QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_FORMAT(0),
                PWSTR(name.as_mut_ptr()),
                &mut count,
            )
            .is_ok()
            {
                let exe = String::from_utf16_lossy(&name[..count as usize]).to_lowercase();
                if exe.contains("openai.codex") || exe.contains("openai\\codex") {
                    (*(param.0 as *mut Vec<isize>)).push(hwnd.0 as isize);
                }
            }
            let _ = CloseHandle(process);
        }
        true.into()
    }
    let mut windows = Vec::new();
    let _ = EnumWindows(Some(visit), LPARAM(&mut windows as *mut _ as isize));
    let foreground = GetForegroundWindow().0 as isize;
    windows.sort_by_key(|h| *h != foreground);
    windows
}
fn main_window_candidate(caption: &str, owned: bool, style: u32, extended: u32) -> bool {
    let title = caption.trim();
    !owned
        && style & WS_CHILD.0 == 0
        && extended & WS_EX_TOOLWINDOW.0 == 0
        && (title == "codex"
            || title == "chatgpt"
            || title.ends_with(" - codex")
            || title.ends_with(" - chatgpt"))
}
pub unsafe fn bounds(hwnd: isize) -> Option<[i32; 4]> {
    let h = HWND(hwnd as *mut _);
    if hwnd == 0 || !IsWindow(h).as_bool() {
        return None;
    }
    let mut rect = RECT::default();
    GetWindowRect(h, &mut rect).ok()?;
    Some([rect.left, rect.top, rect.right, rect.bottom])
}
pub unsafe fn shown(hwnd: isize) -> bool {
    let h = HWND(hwnd as *mut _);
    hwnd != 0 && IsWindow(h).as_bool() && IsWindowVisible(h).as_bool() && !IsIconic(h).as_bool()
}

/// Best-effort sidebar expansion before navigating. Never toggle an already
/// expanded project, and never guess if the accessibility tree is ambiguous.
pub fn expand_project(hwnd:isize,name:&str) -> Result<bool> {
    unsafe {
        if !shown(hwnd) || name.trim().is_empty() {return Ok(false);}
        CoInitializeEx(None,COINIT_MULTITHREADED).ok()?;
        let result=(|| -> Result<bool> {
            let ui:IUIAutomation=CoCreateInstance(&CUIAutomation,None,CLSCTX_INPROC_SERVER)?;
            let root=ui.ElementFromHandle(HWND(hwnd as *mut _))?;
            let groups=ui.CreatePropertyCondition(UIA_ControlTypePropertyId,&VARIANT::from(UIA_GroupControlTypeId.0))?;
            let all=root.FindAll(TreeScope_Descendants,&groups)?;
            let buttons=ui.CreatePropertyCondition(UIA_ControlTypePropertyId,&VARIANT::from(UIA_ButtonControlTypeId.0))?;
            let lists=ui.CreatePropertyCondition(UIA_ControlTypePropertyId,&VARIANT::from(UIA_ListControlTypeId.0))?;
            let mut candidates=Vec::new();
            for i in 0..all.Length()?.min(4096) {
                let group=all.GetElement(i)?;
                if group.CurrentName().ok().map(|s|s.to_string()).as_deref()!=Some(name) {continue;}
                let matches=group.FindAll(TreeScope_Descendants,&buttons)?;
                let mut exact=Vec::new();
                for j in 0..matches.Length()?.min(512) {let b=matches.GetElement(j)?;if b.CurrentName().ok().map(|s|s.to_string()).as_deref()==Some(name){exact.push(b);}}
                if exact.len()==1 {candidates.push((group,exact.remove(0)));}
            }
            if candidates.len()!=1 {return Ok(false);}
            let (group,button)=candidates.remove(0);
            // The project's nested chat list only exists while expanded.
            if group.FindFirst(TreeScope_Descendants,&lists).is_ok(){return Ok(true);}
            if let Ok(pattern)=button.GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(UIA_ExpandCollapsePatternId) {
                if pattern.CurrentExpandCollapseState()? != ExpandCollapseState_Collapsed {return Ok(false);}
                pattern.Expand()?;
            } else {
                // The current Codex version exposes its project header as a
                // plain button. Only invoke after the exact unique project
                // group was found and its chat list is confirmed absent.
                let invoke:IUIAutomationInvokePattern=button.GetCurrentPatternAs(UIA_InvokePatternId)?;
                invoke.Invoke()?;
            }
            for _ in 0..10 {thread::sleep(Duration::from_millis(50));if group.FindFirst(TreeScope_Descendants,&lists).is_ok(){return Ok(true);}}
            Ok(false)
        })();
        CoUninitialize();
        result
    }
}

struct Tracker {
    ui: IUIAutomation,
    document: Option<IUIAutomationElement>,
    hwnd: isize,
    checked: Instant,
    resolved_name: String,
    resolved_id: Option<String>,
    resolved_at: Instant,
}
impl Tracker {
    unsafe fn new() -> Result<Self> {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let ui = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
        Ok(Self {
            ui,
            document: None,
            hwnd: 0,
            checked: Instant::now(),
            resolved_name: String::new(),
            resolved_id: None,
            resolved_at: Instant::now(),
        })
    }
    unsafe fn poll(&mut self, store: &Store) -> Result<(isize, Option<String>, String)> {
        if self.document.is_none() || self.checked.elapsed() > Duration::from_secs(1) {
            let windows = visible_windows();
            self.checked = Instant::now();
            if windows.is_empty() {
                self.document = None;
                return Ok((self.hwnd, None, String::new()));
            }
            let fg = GetForegroundWindow().0 as isize;
            let h = if windows.contains(&fg) {
                fg
            } else if windows.contains(&self.hwnd) {
                self.hwnd
            } else {
                windows[0]
            };
            if h != self.hwnd || self.document.is_none() {
                self.resolved_name.clear();
                self.hwnd = h;
                let root = self.ui.ElementFromHandle(HWND(h as *mut _))?;
                let condition = self.ui.CreatePropertyCondition(
                    UIA_ControlTypePropertyId,
                    &VARIANT::from(UIA_DocumentControlTypeId.0),
                )?;
                self.document = root.FindFirst(TreeScope_Descendants, &condition).ok();
            }
        }
        let Some(document) = &self.document else {
            return Ok((self.hwnd, None, String::new()));
        };
        let name = document.CurrentName()?.to_string();
        if name.is_empty() {
            self.document = None;
            return Ok((self.hwnd, None, String::new()));
        }
        let retry = if self.resolved_id.is_some() { Duration::from_secs(5) } else { Duration::from_millis(500) };
        if self.resolved_name != name || self.resolved_at.elapsed() >= retry {
            if let Ok(culture)=document.CurrentCulture(){crate::locale::follow(culture as u16);}
            self.resolved_id = store.resolve(&name)?;
            self.resolved_name = name.clone();
            self.resolved_at = Instant::now();
        }
        let id = self.resolved_id.clone();
        if document.CurrentName()?.to_string() != name {
            return Ok((self.hwnd, None, String::new()));
        }
        Ok((self.hwnd, id, name))
    }
    unsafe fn titlebar(&self, hwnd: isize) -> Result<Option<[i32; 4]>> {
        if !shown(hwnd) {
            return Ok(None);
        }
        let Some(rect) = bounds(hwnd) else {
            return Ok(None);
        };
        let scale = GetDpiForWindow(HWND(hwnd as *mut _)).max(96) as f64 / 96.0;
        let root = self.ui.ElementFromHandle(HWND(hwnd as *mut _))?;
        let condition = self.ui.CreateTrueCondition()?;
        let mut controls = Vec::new();
        let mut count = 0;
        unsafe fn visit(
            element: &IUIAutomationElement,
            condition: &IUIAutomationCondition,
            host: [i32; 4],
            scale: f64,
            depth: u32,
            documents: u32,
            count: &mut usize,
            controls: &mut Vec<[i32; 4]>,
        ) -> windows::core::Result<()> {
            if depth > 22 || *count >= 160 {
                return Ok(());
            }
            let r = element.CurrentBoundingRectangle()?;
            if r.bottom <= host[1] || r.top as f64 >= host[1] as f64 + 40.0 * scale {
                return Ok(());
            }
            *count += 1;
            let kind = element.CurrentControlType()?.0;
            let mut documents = documents;
            if kind == 50030 {
                if documents > 0 {
                    return Ok(());
                }
                documents += 1;
            }
            if matches!(kind, 50000 | 50005 | 50010 | 50011 | 50020)
                && r.bottom as f64 <= host[1] as f64 + 44.0 * scale
            {
                controls.push([
                    ((r.left - host[0]) as f64 / scale).round() as i32,
                    ((r.top - host[1]) as f64 / scale).round() as i32,
                    ((r.right - host[0]) as f64 / scale).round() as i32,
                    ((r.bottom - host[1]) as f64 / scale).round() as i32,
                ]);
            }
            let children = element.FindAll(TreeScope_Children, condition)?;
            for index in 0..children.Length()? {
                let _ = visit(
                    &children.GetElement(index)?,
                    condition,
                    host,
                    scale,
                    depth + 1,
                    documents,
                    count,
                    controls,
                );
                if *count >= 160 {
                    break;
                }
            }
            Ok(())
        }
        visit(
            &root,
            &condition,
            rect,
            scale,
            0,
            0,
            &mut count,
            &mut controls,
        )?;
        let width = ((rect[2] - rect[0]) as f64 / scale).round() as i32;
        controls.push([width - 140, 0, width, 36]);
        Ok(Some(titlebar_slot(width, &controls)))
    }
}
impl Drop for Tracker {
    fn drop(&mut self) {
        self.document = None;
        unsafe {
            CoUninitialize();
        }
    }
}

pub fn titlebar_slot(width: i32, controls: &[[i32; 4]]) -> [i32; 4] {
    if controls.is_empty() {
        return [340, 2, 340.max(width - 150), 42];
    }
    let mut intervals: Vec<_> = controls
        .iter()
        .filter(|r| r[2] > r[0] && r[3] > 2 && r[1] < 42)
        .map(|r| ((r[0] - 8).max(0), (r[2] + 8).min(width)))
        .collect();
    intervals.sort();
    let (mut cursor, mut best) = (8, (0, 0, 0));
    for (left, right) in intervals {
        if left > cursor && left - cursor > best.0 {
            best = (left - cursor, cursor, left);
        }
        cursor = cursor.max(right);
    }
    if width - 8 - cursor > best.0 {
        best = (width - 8 - cursor, cursor, width - 8);
    }
    [best.1, 2, best.2, 42]
}
pub fn start(paths: &Paths, state: &State, stop: &Stop) -> Vec<thread::JoinHandle<()>> {
    let mut handles = Vec::new();
    let (p, s, done) = (paths.clone(), state.clone(), stop.clone());
    handles.push(thread::spawn(move || unsafe {
        let mut tracker = None;
        let store = Store::new(&p);
        while !done.load(Ordering::Relaxed) {
            if tracker.is_none() {
                tracker = Tracker::new().ok();
            }
            if let Some(t) = tracker.as_mut() {
                match t.poll(&store) {
                    Ok((hwnd, id, label)) => {
                        let mut live = s.lock().unwrap();
                        if live.view != id || live.view_label != label {
                            live.refresh_events.notify();
                            live.detected_at = now();
                            if live.auto_thread {
                                live.snapshot = None;
                            }
                        }
                        live.host = hwnd;
                        live.view = id;
                        live.view_label = label;
                        live.view_error.clear();
                    }
                    Err(e) => {
                        t.document = None;
                        let mut live = s.lock().unwrap();
                        live.view = None;
                        live.view_label.clear();
                        if live.auto_thread {
                            live.snapshot = None;
                        }
                        live.view_error = format!("{e:#}");
                    }
                }
            }
            let delay=s.lock().unwrap().refresh_options.host_ms;
            thread::sleep(Duration::from_millis(delay));
        }
    }));
    let (s, done) = (state.clone(), stop.clone());
    handles.push(thread::spawn(move || unsafe {
        let mut tracker = None;
        let mut last_geometry = None;
        let mut layout_checked = Instant::now();
        while !done.load(Ordering::Relaxed) {
            if tracker.is_none() {
                tracker = Tracker::new().ok();
            }
            let hwnd = s.lock().unwrap().host;
            if hwnd != 0 {
                let geometry = bounds(hwnd).map(|r| (hwnd,r[2]-r[0],GetDpiForWindow(HWND(hwnd as *mut _))));
                let fallback=s.lock().unwrap().refresh_options.fallback_secs;
                if geometry!=last_geometry || layout_checked.elapsed()>=Duration::from_secs(fallback) {
                  if let Some(t) = &tracker {
                    if let Ok(Some(slot)) = t.titlebar(hwnd) {
                        let mut live = s.lock().unwrap();
                        live.slot = Some(slot);
                        live.slot_host = hwnd;
                        last_geometry=geometry;
                        layout_checked=Instant::now();
                    }
                  }
                }
            }
            for _ in 0..10 {
                if done.load(Ordering::Relaxed) {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }));
    handles
}

pub fn inspect(paths: &Paths) -> Result<()> {
    unsafe {
        let mut tracker = Tracker::new()?;
        let mut store = Store::new(paths);
        let (hwnd, id, label) = tracker.poll(&store)?;
        let snapshot = id.as_deref().and_then(|id| store.read(id).ok());
        let slot = tracker.titlebar(hwnd).ok().flatten();
        paths.write("rust-observe.json",&serde_json::json!({"time":now(),"host_window":hwnd,"identified":id.is_some(),"label":label,"snapshot":snapshot,"slot":slot,"mutations":false}));
        println!(
            "{}",
            serde_json::json!({"host_found":hwnd!=0,"identified":id.is_some(),"tokens_available":snapshot.as_ref().and_then(|s|s.tokens()).is_some(),"slot":slot,"mutations":false})
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_follow_main_window() {
        let main = WS_OVERLAPPEDWINDOW.0;
        assert!(main_window_candidate("codex", false, main, 0));
        assert!(main_window_candidate("chatgpt", false, main, 0));
        assert!(!main_window_candidate("about chatgpt", false, main, 0));
        assert!(!main_window_candidate("codex", true, main, 0));
        assert!(!main_window_candidate(
            "codex",
            false,
            main,
            WS_EX_TOOLWINDOW.0
        ));
        assert!(!main_window_candidate("codex", false, WS_CHILD.0, 0));
    }
    #[test]
    fn blank_slot_avoids_menus() {
        assert_eq!(
            titlebar_slot(
                1482,
                &[[6, 4, 102, 32], [108, 6, 317, 30], [1342, 0, 1482, 36]]
            ),
            [325, 2, 1334, 42]
        );
    }
}
