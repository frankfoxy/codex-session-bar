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
        if !caption.contains("codex") && !caption.contains("chatgpt") {
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

struct Tracker {
    ui: IUIAutomation,
    document: Option<IUIAutomationElement>,
    hwnd: isize,
    checked: Instant,
}
impl Tracker {
    unsafe fn new() -> Result<Self> {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let ui = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
        Ok(Self {
            ui,
            document: None,
            hwnd: 0,
            checked: Instant::now() - Duration::from_secs(2),
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
        let id = store.resolve(&name)?;
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
                        if live.auto_thread {
                            live.snapshot = None;
                        }
                        live.view_error = format!("{e:#}");
                    }
                }
            }
            thread::sleep(Duration::from_millis(100));
        }
    }));
    let (s, done) = (state.clone(), stop.clone());
    handles.push(thread::spawn(move || unsafe {
        let mut tracker = None;
        while !done.load(Ordering::Relaxed) {
            if tracker.is_none() {
                tracker = Tracker::new().ok();
            }
            let hwnd = s.lock().unwrap().host;
            if hwnd != 0 {
                if let Some(t) = &tracker {
                    if let Ok(Some(slot)) = t.titlebar(hwnd) {
                        let mut live = s.lock().unwrap();
                        live.slot = Some(slot);
                        live.slot_host = hwnd;
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
