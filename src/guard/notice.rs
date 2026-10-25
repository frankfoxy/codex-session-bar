//! Alerts never run on the enforcement thread or the Session Bar UI thread.
use std::{fs, path::Path, sync::atomic::{AtomicBool, Ordering}, thread};
use windows::{core::{w, PCWSTR}, Win32::{Foundation::*, UI::WindowsAndMessaging::*}};

static OPEN: AtomicBool = AtomicBool::new(false);
fn wide(text: &str) -> Vec<u16> { text.encode_utf16().chain(Some(0)).collect() }

pub fn update(dir: &Path, message: &str) {
    let _ = fs::write(dir.join("guard-warning-latest.json"), serde_json::json!({
        "updated_at":chrono::Local::now().to_rfc3339(), "test":false,"message":message
    }).to_string());
    unsafe {
        if let Ok(window) = FindWindowW(w!("#32770"), w!("Codex Git 内存防护告警")) {
            update_window(window, message);
        }
    }
}

pub fn show(dir: &Path, message: &str) {
    update(dir, message);
    if OPEN.swap(true, Ordering::AcqRel) { return; }
    let message = message.to_owned();
    if thread::Builder::new().name("git-guard-notice".into()).spawn(move || {
        unsafe {
            let _ = MessageBoxW(None, PCWSTR(wide(&message).as_ptr()),
                w!("Codex Git 内存防护告警"), MB_OK | MB_ICONWARNING | MB_TOPMOST);
        }
        OPEN.store(false, Ordering::Release);
    }).is_err() { OPEN.store(false, Ordering::Release); }
}

unsafe fn update_window(window: HWND, message: &str) -> bool {
    let mut pid = 0;
    GetWindowThreadProcessId(window, Some(&mut pid));
    if pid != std::process::id() { return false; }
    let Ok(label) = GetDlgItem(window, 65535) else { return false; };
    let text = wide(message);
    let mut result = 0;
    SendMessageTimeoutW(label, WM_SETTEXT, WPARAM(0), LPARAM(text.as_ptr() as isize),
        SMTO_ABORTIFHUNG | SMTO_BLOCK, 100, Some(&mut result)).0 != 0
}

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn existing_notice_updates_without_opening_a_dialog() { unsafe {
        let window = CreateWindowExW(WINDOW_EX_STYLE::default(), w!("STATIC"), w!("Hidden guard notice test"),
            WS_POPUP, 0, 0, 400, 300, None, None, None, None).unwrap();
        let label = CreateWindowExW(WINDOW_EX_STYLE::default(), w!("STATIC"), w!("old"),
            WS_CHILD, 0, 0, 380, 280, window, HMENU(65535usize as *mut _), None, None).unwrap();
        assert!(update_window(window, "已确认结束 8 个；待确认 2 个"));
        let mut text = [0u16;128];
        let length = GetWindowTextW(label, &mut text);
        assert_eq!(String::from_utf16_lossy(&text[..length as usize]), "已确认结束 8 个；待确认 2 个");
        assert!(!IsWindowVisible(window).as_bool());
        let _ = DestroyWindow(window);
    }}
}
