#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod collect;
mod guard;
mod recorder;
use anyhow::{Context, Result};
use collect::Snapshot;
use recorder::Recorder;
use serde_json::json;
use std::{
    cell::RefCell,
    collections::HashMap,
    fs,
    mem::size_of,
    path::{Path, PathBuf},
    sync::{mpsc, Arc},
    thread,
    time::{Duration, Instant},
};
use windows::{
    core::{w, PCWSTR, PWSTR},
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{LibraryLoader::*, Threading::*},
        UI::{Controls::*, HiDpi::*, Shell::*, WindowsAndMessaging::*},
    },
};
const CLASS: PCWSTR = w!("RustMemoryWatch");
const TRAY: u32 = WM_APP + 1;
const SHOW: u32 = WM_APP + 2;
const CAPTURE: u32 = WM_APP + 3;
const QUIT: u32 = WM_APP + 4;
const PAUSE: u32 = WM_APP + 5;
const RESUME: u32 = WM_APP + 6;
const CHECK: u32 = WM_APP + 7;
const GUARD_WARNING_TEST: u32 = WM_APP + 8;
const INTERVALS: [u64; 5] = [15, 30, 60, 120, 300];
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn mib(v: u64) -> String {
    format!("{:.1}", v as f64 / 1048576.)
}
fn gib(v: u64) -> String {
    format!("{:.2}", v as f64 / 1073741824.)
}
fn change(v: i64) -> String {
    format!("{:+.1}", v as f64 / 1048576.)
}
#[derive(Clone)]
struct Paths {
    app: PathBuf,
    logs: PathBuf,
}
fn workspace_root(app: &Path) -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("CODEX_SESSION_BAR_WORKSPACE") {
        return Ok(PathBuf::from(root).canonicalize()?);
    }
    if let Some(tools) = app
        .ancestors()
        .find(|p| p.file_name().is_some_and(|n| n == "tools"))
    {
        return Ok(tools
            .parent()
            .context("Workspace unavailable")?
            .canonicalize()?);
    }
    if let Some(repo) = app
        .ancestors()
        .find(|p| p.join("emoji_catalog.json").is_file())
    {
        return Ok(repo.canonicalize()?);
    }
    Ok(app.canonicalize()?)
}
impl Paths {
    fn discover() -> Result<Self> {
        let exe = std::env::current_exe()?;
        let mut app = exe
            .parent()
            .context("No executable directory")?
            .to_path_buf();
        let deployed = app.file_name().is_some_and(|s| s == "bin");
        if deployed {
            app.pop();
        }
        if let Some(home) = std::env::var_os("MEMORY_WATCH_APP_DIR") {
            app = PathBuf::from(home).canonicalize()?;
        } else if deployed && app.join("emoji_catalog.json").is_file() {
            // Directly opening the bundled monitor must not overwrite the
            // Session Bar's own settings when changing the recording interval.
            app=app.join("memory-watch");
            fs::create_dir_all(&app)?;
        } else if !deployed && !app.join("Cargo.toml").exists() {
            app = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        }
        let root = workspace_root(&app)?;
        let temp = root.join("tmp");
        let logs = temp.join("memory-watch-rust");
        anyhow::ensure!(
            logs.parent() == Some(temp.as_path()) && temp.parent() == Some(root.as_path()),
            "Invalid temporary path"
        );
        fs::create_dir_all(&logs)?;
        for key in ["TEMP", "TMP", "TMPDIR"] {
            std::env::set_var(key, &logs);
        }
        Ok(Self { app, logs })
    }
    fn interval(&self) -> u64 {
        fs::metadata(self.app.join("settings.json"))
            .ok()
            .filter(|m| m.len() < 4096)
            .and_then(|_| fs::read(self.app.join("settings.json")).ok())
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|j| j["interval_seconds"].as_u64())
            .filter(|i| INTERVALS.contains(i))
            .unwrap_or(30)
    }
    fn save_interval(&self, n: u64) {
        let pending = self.logs.join("settings.pending");
        if fs::write(&pending, json!({"interval_seconds":n}).to_string()).is_ok() {
            let _ = fs::rename(pending, self.app.join("settings.json"));
        }
    }
}
enum Command {
    Now,
    Pause(bool),
    Interval(u64),
    Quit,
}
enum Event {
    Sample(Arc<Snapshot>),
    Error(String),
    GuardStatus(String),
    GuardAlert(String),
}
fn worker(paths: Paths, interval: u64, rx: mpsc::Receiver<Command>, tx: mpsc::SyncSender<Event>) {
    let recorder = Recorder::new(paths.logs.clone());
    let mut collector = collect::Collector::new();
    let _protection=guard::Runner::start(paths.logs.clone(),tx.clone());
    let (mut seconds, mut paused, mut next, mut force) = (interval, false, Instant::now(), false);
    loop {
        if force || (!paused && Instant::now() >= next) {
            force = false;
            match collector.sample(seconds).and_then(|s| {
                recorder.record(&s)?;
                Ok(s)
            }) {
                Ok(s) => {
                    let _ = tx.try_send(Event::Sample(Arc::new(s)));
                }
                Err(e) => {
                    let msg = format!("{}: {e:#}", chrono::Local::now().to_rfc3339());
                    let _ = fs::write(paths.logs.join("last-error.txt"), &msg);
                    let _ = tx.try_send(Event::Error(msg));
                }
            }
            next = Instant::now() + Duration::from_secs(seconds);
        }
        let wait = if paused {
            Duration::from_secs(3600)
        } else {
            next.saturating_duration_since(Instant::now())
        }
        .min(Duration::from_secs(1));
        match rx.recv_timeout(wait) {
            Ok(Command::Quit) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(Command::Pause(v)) => {
                paused = v;
                if !paused {
                    next = Instant::now();
                }
            }
            Ok(Command::Interval(n)) => {
                seconds = n;
                next = Instant::now();
            }
            Ok(Command::Now) => force = true,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}
#[derive(Clone)]
struct Row {
    key: String,
    name: String,
    pid: Option<(u32, i64)>,
    count: usize,
    private: u64,
    ws: u64,
    delta: i64,
    growth: i64,
    cpu: f64,
    handles: u32,
    threads: u32,
    peak: u64,
    paged: u64,
    nonpaged: u64,
}
struct App {
    paths: Paths,
    hwnd: HWND,
    summary: HWND,
    status: HWND,
    list: HWND,
    details: HWND,
    interval: HWND,
    pause: HWND,
    now: HWND,
    logs: HWND,
    group: HWND,
    filter: HWND,
    font: HFONT,
    icon: HICON,
    tx: mpsc::Sender<Command>,
    rx: mpsc::Receiver<Event>,
    sample: Option<Arc<Snapshot>>,
    rows: Vec<Row>,
    grouped: bool,
    paused: bool,
    seconds: u64,
    sort: usize,
    descending: bool,
    selected: String,
    paths_cache: HashMap<(u32, i64), String>,
    error: String,
    guard_note: String,
}
thread_local! {static APP:RefCell<Option<App>>=const{RefCell::new(None)};}
unsafe fn control(parent: HWND, class: PCWSTR, title: &str, id: isize, extra: u32) -> Result<HWND> {
    CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        PCWSTR(wide(title).as_ptr()),
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(extra),
        0,
        0,
        80,
        25,
        parent,
        HMENU(id as *mut _),
        GetModuleHandleW(None)?,
        None,
    )
    .with_context(|| format!("Create control {id}"))
}
unsafe fn memory_icon() -> Result<HICON> {
    let screen = GetDC(None);
    let dc = CreateCompatibleDC(screen);
    let bitmap = CreateCompatibleBitmap(screen, 32, 32);
    let old = SelectObject(dc, bitmap);
    let brush = CreateSolidBrush(COLORREF(0x754b26));
    FillRect(
        dc,
        &RECT {
            left: 0,
            top: 0,
            right: 32,
            bottom: 32,
        },
        brush,
    );
    let _ = DeleteObject(brush);
    let font = CreateFontW(
        -24,
        0,
        0,
        0,
        700,
        0,
        0,
        0,
        DEFAULT_CHARSET.0 as u32,
        0,
        0,
        CLEARTYPE_QUALITY.0 as u32,
        0,
        w!("Segoe UI"),
    );
    let previous = SelectObject(dc, font);
    SetTextColor(dc, COLORREF(0xffffff));
    SetBkMode(dc, TRANSPARENT);
    let mut r = RECT {
        left: 0,
        top: 0,
        right: 32,
        bottom: 32,
    };
    let mut text = wide("M");
    DrawTextW(
        dc,
        &mut text[..1],
        &mut r,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
    SelectObject(dc, previous);
    let _ = DeleteObject(font);
    let mask = CreateBitmap(32, 32, 1, 1, Some([0u8; 128].as_ptr().cast()));
    let icon = CreateIconIndirect(&ICONINFO {
        fIcon: BOOL(1),
        hbmMask: mask,
        hbmColor: bitmap,
        ..Default::default()
    })?;
    SelectObject(dc, old);
    let _ = DeleteObject(bitmap);
    let _ = DeleteObject(mask);
    let _ = DeleteDC(dc);
    ReleaseDC(None, screen);
    Ok(icon)
}
impl App {
    unsafe fn new(
        paths: Paths,
        tx: mpsc::Sender<Command>,
        rx: mpsc::Receiver<Event>,
    ) -> Result<Self> {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        InitCommonControlsEx(&INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES,
        })
        .ok()
        .context("Initialize common controls")?;
        let instance = GetModuleHandleW(None)?;
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as usize as *mut _),
            lpszClassName: CLASS,
            ..Default::default()
        });
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            w!("Memory Watch · 内存排查"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1170,
            740,
            None,
            None,
            instance,
            None,
        )
        .context("Create monitor window")?;
        let font = CreateFontW(
            -15,
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            0,
            0,
            CLEARTYPE_QUALITY.0 as u32,
            0,
            w!("Microsoft YaHei UI"),
        );
        let summary = control(hwnd, w!("STATIC"), "正在读取系统内存…", 1, 0)?;
        let now = control(hwnd, w!("BUTTON"), "立即采样", 10, BS_PUSHBUTTON as u32)?;
        let pause = control(hwnd, w!("BUTTON"), "暂停记录", 11, BS_PUSHBUTTON as u32)?;
        let logs = control(hwnd, w!("BUTTON"), "打开日志", 12, BS_PUSHBUTTON as u32)?;
        let interval = control(
            hwnd,
            w!("COMBOBOX"),
            "",
            13,
            CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0,
        )?;
        let seconds = paths.interval();
        for n in INTERVALS {
            SendMessageW(
                interval,
                CB_ADDSTRING,
                WPARAM(0),
                LPARAM(wide(&format!("每 {n} 秒更新")).as_ptr() as isize),
            );
        }
        SendMessageW(
            interval,
            CB_SETCURSEL,
            WPARAM(INTERVALS.iter().position(|n| *n == seconds).unwrap_or(1)),
            LPARAM(0),
        );
        let group = control(
            hwnd,
            w!("BUTTON"),
            "按程序名称汇总",
            14,
            BS_AUTOCHECKBOX as u32,
        )?;
        SendMessageW(group, BM_SETCHECK, WPARAM(1), LPARAM(0));
        let filter = control(
            hwnd,
            w!("EDIT"),
            "",
            15,
            ES_AUTOHSCROLL as u32 | WS_BORDER.0,
        )?;
        SendMessageW(
            filter,
            EM_SETCUEBANNER,
            WPARAM(0),
            LPARAM(w!("筛选名称 / PID").as_ptr() as isize),
        );
        let list = control(
            hwnd,
            WC_LISTVIEWW,
            "",
            16,
            LVS_REPORT | LVS_SHOWSELALWAYS | WS_BORDER.0,
        )?;
        SendMessageW(
            list,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            WPARAM(0),
            LPARAM((LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_GRIDLINES) as isize),
        );
        let details = control(
            hwnd,
            w!("STATIC"),
            "选择一行查看详细信息。Private 是进程提交的私有内存；RAM 是当前物理驻留量。",
            17,
            0,
        )?;
        let status = control(
            hwnd,
            w!("STATIC"),
            "日志每次采样写入并刷盘；关闭窗口后仍在托盘记录。",
            18,
            0,
        )?;
        for child in [
            summary, now, pause, logs, interval, group, filter, list, details, status,
        ] {
            SendMessageW(child, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        }
        for (i, (name, width)) in [
            ("程序名称", 230),
            ("PID / 数量", 90),
            ("Private MiB", 110),
            ("RAM MiB", 100),
            ("本次变化 MiB", 115),
            ("观察后增长 MiB", 125),
            ("CPU %", 75),
            ("句柄", 80),
            ("线程", 70),
        ]
        .iter()
        .enumerate()
        {
            let mut text = wide(name);
            let mut c = LVCOLUMNW {
                mask: LVCF_TEXT | LVCF_WIDTH | LVCF_SUBITEM,
                cx: *width,
                pszText: PWSTR(text.as_mut_ptr()),
                iSubItem: i as i32,
                ..Default::default()
            };
            SendMessageW(
                list,
                LVM_INSERTCOLUMNW,
                WPARAM(i),
                LPARAM(&mut c as *mut _ as isize),
            );
        }
        let app = Self {
            paths,
            hwnd,
            summary,
            status,
            list,
            details,
            interval,
            pause,
            now,
            logs,
            group,
            filter,
            font,
            icon: memory_icon()?,
            tx,
            rx,
            sample: None,
            rows: Vec::new(),
            grouped: true,
            paused: false,
            seconds,
            sort: 2,
            descending: true,
            selected: String::new(),
            paths_cache: HashMap::new(),
            error: String::new(),
            guard_note: String::new(),
        };
        app.layout();
        app.tray(NIM_ADD);
        SetTimer(hwnd, 1, 500, None);
        Ok(app)
    }
    unsafe fn layout(&self) {
        let mut r = RECT::default();
        let _ = GetClientRect(self.hwnd, &mut r);
        let w = r.right;
        let h = r.bottom;
        for (hwnd, x, y, cw, ch) in [
            (self.summary, 16, 12, w - 32, 64),
            (self.now, 16, 84, 95, 30),
            (self.pause, 121, 84, 95, 30),
            (self.logs, 226, 84, 95, 30),
            (self.interval, 335, 84, 165, 200),
            (self.group, 518, 84, 170, 30),
            (self.filter, 706, 84, (w - 722).max(120), 29),
            (self.list, 16, 128, w - 32, (h - 265).max(100)),
            (self.details, 16, h - 125, w - 32, 83),
            (self.status, 16, h - 35, w - 32, 30),
        ] {
            let _ = MoveWindow(hwnd, x, y, cw, ch, true);
        }
    }
    unsafe fn tray(&self, action: NOTIFY_ICON_MESSAGE) {
        let mut data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: TRAY,
            hIcon: self.icon,
            ..Default::default()
        };
        let text = if let Some(s) = &self.sample {
            format!(
                "Memory Watch · 提交 {:.1}% · {} 秒{}",
                s.system.commit_total as f64 / s.system.commit_limit.max(1) as f64 * 100.,
                self.seconds,
                if self.paused { " · 暂停" } else { "" }
            )
        } else {
            "Memory Watch · 正在采样".into()
        };
        let t = wide(&text);
        let n = t.len().min(127);
        data.szTip[..n].copy_from_slice(&t[..n]);
        let available = Shell_NotifyIconW(action, &data).as_bool();
        if action != NIM_DELETE {
            let _=fs::write(self.paths.logs.join("ui-state.json"),json!({"pid":std::process::id(),"tray_available":available,"shown":IsWindowVisible(self.hwnd).as_bool(),"paused":self.paused,"interval_seconds":self.seconds,"sample_time":self.sample.as_ref().map(|s|&s.time)}).to_string());
        }
    }
    unsafe fn tick(&mut self) {
        let mut dirty = false;
        while let Ok(e) = self.rx.try_recv() {
            match e {
                Event::Sample(s) => {
                    self.sample = Some(s);
                    self.error.clear();
                    dirty = true;
                }
                Event::Error(e) => {
                    self.error = e;
                    dirty = true;
                }
                Event::GuardStatus(note) => {
                    self.guard_note = note;
                    let _ = SetWindowTextW(self.status, PCWSTR(wide(&self.guard_note).as_ptr()));
                }
                Event::GuardAlert(message) => {
                    self.guard_note = message.clone();
                    self.guard_warning(&message);
                    let _ = SetWindowTextW(self.status, PCWSTR(wide(&message).as_ptr()));
                }
            }
        }
        if dirty {
            self.refresh();
            self.tray(NIM_MODIFY);
        }
    }
    unsafe fn guard_warning(&self, message: &str) {
        static OPEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if OPEN.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        let message = message.to_owned();
        let logs = self.paths.logs.clone();
        thread::spawn(move || {
            let test = message.starts_with("TEST:");
            let title = if test {
                "Codex Git guard notification test"
            } else {
                "Codex Git 内存防护告警"
            };
            let _=fs::write(logs.join("guard-warning-latest.json"),json!({"requested_at":chrono::Local::now().to_rfc3339(),"test":test,"message":message}).to_string());
            // A separate thread keeps sampling/enforcement alive while the user
            // reads the warning, including when Windows suppresses tray toasts.
            unsafe {
                let _ = MessageBoxW(
                    None,
                    PCWSTR(wide(&message).as_ptr()),
                    PCWSTR(wide(title).as_ptr()),
                    MB_OK | MB_ICONWARNING | MB_TOPMOST,
                );
            }
            OPEN.store(false, std::sync::atomic::Ordering::Release);
        });
    }
    unsafe fn refresh(&mut self) {
        let Some(s) = &self.sample else {
            SetWindowTextW(self.status, PCWSTR(wide(&self.error).as_ptr())).ok();
            return;
        };
        let sys = &s.system;
        let ratio = sys.commit_total as f64 / sys.commit_limit.max(1) as f64 * 100.;
        let summary=format!("物理内存：{} / {} GiB（可用 {} GiB）     系统提交：{} / {} GiB（{:.1}%）\r\n分页池：{} MiB     非分页池：{} MiB     开机提交峰值：{} GiB     进程：{}",
   gib(sys.physical_total.saturating_sub(sys.physical_available)),gib(sys.physical_total),gib(sys.physical_available),gib(sys.commit_total),gib(sys.commit_limit),ratio,mib(sys.paged_pool),mib(sys.nonpaged_pool),gib(sys.commit_peak),sys.process_count);
        let _ = SetWindowTextW(self.summary, PCWSTR(wide(&summary).as_ptr()));
        let mut text = [0u16; 256];
        let n = GetWindowTextW(self.filter, &mut text);
        let query = String::from_utf16_lossy(&text[..n as usize]).to_lowercase();
        let mut rows: Vec<Row> = if self.grouped {
            s.groups
                .iter()
                .map(|g| Row {
                    key: g.name.to_lowercase(),
                    name: g.name.clone(),
                    pid: None,
                    count: g.count,
                    private: g.private,
                    ws: g.working_set,
                    delta: g.delta,
                    growth: g.growth,
                    cpu: g.cpu_percent,
                    handles: g.handles,
                    threads: g.threads,
                    peak: g.peak_private,
                    paged: g.paged_pool,
                    nonpaged: g.nonpaged_pool,
                })
                .collect()
        } else {
            s.processes
                .iter()
                .map(|p| Row {
                    key: format!("{}:{}", p.pid, p.created_ticks),
                    name: p.name.clone(),
                    pid: Some((p.pid, p.created_ticks)),
                    count: 1,
                    private: p.private,
                    ws: p.working_set,
                    delta: p.delta,
                    growth: p.growth,
                    cpu: p.cpu_percent,
                    handles: p.handles,
                    threads: p.threads,
                    peak: p.peak_private,
                    paged: p.paged_pool,
                    nonpaged: p.nonpaged_pool,
                })
                .collect()
        };
        rows.retain(|r| {
            query.is_empty()
                || r.name.to_lowercase().contains(&query)
                || r.pid.is_some_and(|(p, _)| p.to_string().contains(&query))
        });
        rows.sort_by(|a, b| {
            let order = match self.sort {
                0 => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                1 => a
                    .pid
                    .map_or(a.count as u32, |p| p.0)
                    .cmp(&b.pid.map_or(b.count as u32, |p| p.0)),
                2 => a.private.cmp(&b.private),
                3 => a.ws.cmp(&b.ws),
                4 => a.delta.cmp(&b.delta),
                5 => a.growth.cmp(&b.growth),
                6 => a.cpu.total_cmp(&b.cpu),
                7 => a.handles.cmp(&b.handles),
                _ => a.threads.cmp(&b.threads),
            };
            if self.descending {
                order.reverse()
            } else {
                order
            }
            .then(a.key.cmp(&b.key))
        });
        self.rows = rows;
        SendMessageW(self.list, WM_SETREDRAW, WPARAM(0), LPARAM(0));
        SendMessageW(self.list, LVM_DELETEALLITEMS, WPARAM(0), LPARAM(0));
        for (i, row) in self.rows.iter().enumerate() {
            let values = [
                row.name.clone(),
                row.pid
                    .map(|p| p.0.to_string())
                    .unwrap_or(format!("{} 个", row.count)),
                mib(row.private),
                mib(row.ws),
                change(row.delta),
                change(row.growth),
                format!("{:.1}", row.cpu),
                row.handles.to_string(),
                row.threads.to_string(),
            ];
            for (j, value) in values.iter().enumerate() {
                let mut text = wide(value);
                let mut item = LVITEMW {
                    mask: LVIF_TEXT,
                    iItem: i as i32,
                    iSubItem: j as i32,
                    pszText: PWSTR(text.as_mut_ptr()),
                    ..Default::default()
                };
                SendMessageW(
                    self.list,
                    if j == 0 {
                        LVM_INSERTITEMW
                    } else {
                        LVM_SETITEMTEXTW
                    },
                    WPARAM(if j == 0 { 0 } else { i }),
                    LPARAM(&mut item as *mut _ as isize),
                );
            }
            if row.key == self.selected {
                let mut item = LVITEMW {
                    state: LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0),
                    stateMask: LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0),
                    ..Default::default()
                };
                SendMessageW(
                    self.list,
                    LVM_SETITEMSTATE,
                    WPARAM(i),
                    LPARAM(&mut item as *mut _ as isize),
                );
            }
        }
        SendMessageW(self.list, WM_SETREDRAW, WPARAM(1), LPARAM(0));
        let _ = InvalidateRect(self.list, None, true);
        let line = if !self.error.is_empty() {
            format!("记录错误：{}", self.error)
        } else {
            format!(
                "{}采样 {} · {} ms · {} 行 · 点击列标题排序{}",
                if self.paused {
                    "已暂停 · 最近"
                } else {
                    "最近"
                },
                s.time,
                s.collection_ms.round(),
                self.rows.len(),
                if ratio >= 85. {
                    " · 注意：提交内存已达到 85%"
                } else {
                    ""
                }
            )
        };
        let line = if self.guard_note.is_empty() {
            line
        } else {
            format!("{} · {}", self.guard_note, line)
        };
        let _ = SetWindowTextW(self.status, PCWSTR(wide(&line).as_ptr()));
        self.show_details();
    }
    unsafe fn show_details(&mut self) {
        let index = SendMessageW(
            self.list,
            LVM_GETNEXTITEM,
            WPARAM(usize::MAX),
            LPARAM(LVNI_SELECTED as isize),
        )
        .0;
        let Some(row) = self.rows.get(index as usize).cloned() else {
            let _ = SetWindowTextW(
                self.details,
                w!("选择一行查看详细信息。Private 是私有提交内存；RAM 是当前物理驻留量。"),
            );
            return;
        };
        self.selected = row.key.clone();
        let extra = if let Some(key) = row.pid {
            if !self.paths_cache.contains_key(&key) {
                if self.paths_cache.len() > 1024 {
                    self.paths_cache.clear();
                }
                self.paths_cache.insert(key, process_path(key));
            }
            let parent = self
                .sample
                .as_ref()
                .and_then(|s| {
                    s.processes
                        .iter()
                        .find(|p| p.pid == key.0 && p.created_ticks == key.1)
                })
                .map_or(0, |p| p.parent_pid);
            let created = chrono::DateTime::from_timestamp(key.1 / 10_000_000 - 11_644_473_600, 0)
                .map(|t| {
                    t.with_timezone(&chrono::Local)
                        .format("%m-%d %H:%M:%S")
                        .to_string()
                })
                .unwrap_or("未知".into());
            format!(
                "{} · 父 PID {} · 创建 {}",
                self.paths_cache[&key], parent, created
            )
        } else {
            let ids = self
                .sample
                .as_ref()
                .map(|s| {
                    s.processes
                        .iter()
                        .filter(|p| p.name.eq_ignore_ascii_case(&row.name))
                        .map(|p| p.pid.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            format!("PID：{ids}（取消“按程序名称汇总”可查看单个进程的路径）")
        };
        let message=format!("{} · 提交峰值 {} MiB · 分页池 {} MiB · 非分页池 {} MiB · 句柄 {} · 线程 {}\r\n{}\r\n增长以首次观察为基准；系统提交与各进程 Private 总和不同，还包含内核等开销。",row.name,mib(row.peak),mib(row.paged),mib(row.nonpaged),row.handles,row.threads,extra);
        let _ = SetWindowTextW(self.details, PCWSTR(wide(&message).as_ptr()));
    }
    unsafe fn check_ui(&mut self) -> Result<serde_json::Value> {
        let grouped = self.grouped;
        let selected = self.selected.clone();
        let mut previous = [0u16; 256];
        let length = GetWindowTextW(self.filter, &mut previous);
        self.grouped = false;
        SendMessageW(self.group, BM_SETCHECK, WPARAM(0), LPARAM(0));
        let _ = SetWindowTextW(self.filter, w!("MEMORY-WATCH.EXE"));
        self.refresh();
        let index = self
            .rows
            .iter()
            .position(|r| r.pid.is_some_and(|p| p.0 == std::process::id()));
        let mut path_ok = false;
        if let Some(i) = index {
            let mut item = LVITEMW {
                state: LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0),
                stateMask: LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0),
                ..Default::default()
            };
            SendMessageW(
                self.list,
                LVM_SETITEMSTATE,
                WPARAM(i),
                LPARAM(&mut item as *mut _ as isize),
            );
            self.show_details();
            let mut text = [0u16; 4096];
            let n = GetWindowTextW(self.details, &mut text);
            let value = String::from_utf16_lossy(&text[..n as usize]);
            path_ok = value.contains("memory-watch.exe") && value.contains("父 PID");
        }
        let filtered = self.rows.len();
        self.grouped = grouped;
        self.selected = selected;
        SendMessageW(self.group, BM_SETCHECK, WPARAM(grouped as usize), LPARAM(0));
        let _ = SetWindowTextW(
            self.filter,
            PCWSTR(wide(&String::from_utf16_lossy(&previous[..length as usize])).as_ptr()),
        );
        self.refresh();
        anyhow::ensure!(
            index.is_some() && path_ok,
            "Process filter, selection or identity-checked path failed"
        );
        capture_window(self.hwnd, &self.paths.logs.join("window.png"))?;
        Ok(
            json!({"passed":true,"case_insensitive_filter":true,"filtered_rows":filtered,"selected_path_verified":path_ok,"grouped_rows":self.rows.len(),"pid":std::process::id()}),
        )
    }
    unsafe fn show(&self) {
        let _ = ShowWindow(self.hwnd, SW_RESTORE);
        let _ = SetForegroundWindow(self.hwnd);
        self.tray(NIM_MODIFY);
    }
    unsafe fn command(&mut self, id: usize) {
        match id {
            10 | 102 => {
                let _ = self.tx.send(Command::Now);
            }
            11 | 101 => {
                self.paused = !self.paused;
                let _ = self.tx.send(Command::Pause(self.paused));
                let _ = SetWindowTextW(
                    self.pause,
                    if self.paused {
                        w!("继续记录")
                    } else {
                        w!("暂停记录")
                    },
                );
                self.refresh();
                self.tray(NIM_MODIFY);
            }
            12 | 103 => {
                ShellExecuteW(
                    self.hwnd,
                    w!("open"),
                    PCWSTR(wide(&self.paths.logs.to_string_lossy()).as_ptr()),
                    None,
                    None,
                    SW_SHOWNORMAL,
                );
            }
            14 => {
                self.grouped = SendMessageW(self.group, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 != 0;
                self.selected.clear();
                self.refresh();
            }
            100 => self.show(),
            104 => {
                let _ = self.tx.send(Command::Quit);
                PostQuitMessage(0);
            }
            _ => {}
        }
    }
}
unsafe fn process_path((pid, created): (u32, i64)) -> String {
    let Ok(p) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
        return "路径不可读取（受保护、已退出或权限不足）".into();
    };
    let (mut start, mut exit, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    let mut buf = [0u16; 4096];
    let mut n = buf.len() as u32;
    let valid = GetProcessTimes(p, &mut start, &mut exit, &mut kernel, &mut user).is_ok()
        && (((start.dwHighDateTime as u64) << 32) | start.dwLowDateTime as u64) == created as u64;
    let result = if valid
        && QueryFullProcessImageNameW(p, PROCESS_NAME_FORMAT(0), PWSTR(buf.as_mut_ptr()), &mut n)
            .is_ok()
    {
        String::from_utf16_lossy(&buf[..n as usize])
    } else {
        "进程已退出或路径不可读取".into()
    };
    let _ = CloseHandle(p);
    result
}
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let result = APP.with(|cell| {
        let Ok(mut a) = cell.try_borrow_mut() else {
            return None;
        };
        let app = a.as_mut()?;
        match msg {
            WM_SIZE => {
                if wparam.0 == SIZE_MINIMIZED as usize {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                } else {
                    app.layout();
                }
                Some(LRESULT(0))
            }
            WM_TIMER => {
                app.tick();
                Some(LRESULT(0))
            }
            WM_CLOSE => {
                let _ = ShowWindow(hwnd, SW_HIDE);
                app.tray(NIM_MODIFY);
                Some(LRESULT(0))
            }
            SHOW => {
                app.show();
                Some(LRESULT(0))
            }
            CAPTURE => {
                if let Err(e) = capture_window(hwnd, &app.paths.logs.join("window.png")) {
                    let _ = fs::write(app.paths.logs.join("capture-error.txt"), format!("{e:#}"));
                }
                Some(LRESULT(0))
            }
            QUIT => {
                app.command(104);
                Some(LRESULT(0))
            }
            PAUSE | RESUME => {
                if app.paused != (msg == PAUSE) {
                    app.command(11);
                }
                Some(LRESULT(0))
            }
            CHECK => {
                let result = app.check_ui();
                let _ = fs::write(
                    app.paths.logs.join("ui-check.json"),
                    match result {
                        Ok(value) => value.to_string(),
                        Err(e) => json!({"passed":false,"error":format!("{e:#}")}).to_string(),
                    },
                );
                Some(LRESULT(0))
            }
            GUARD_WARNING_TEST => {
                app.guard_warning("TEST: Codex Git 防护告警窗口测试。此测试未结束任何程序，监测和防护会继续运行。");
                Some(LRESULT(0))
            }
            WM_COMMAND => {
                let id = wparam.0 & 0xffff;
                let code = (wparam.0 >> 16) & 0xffff;
                if id == 13 && code == CBN_SELCHANGE as usize {
                    let index =
                        SendMessageW(app.interval, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as usize;
                    if let Some(&n) = INTERVALS.get(index) {
                        app.seconds = n;
                        app.paths.save_interval(n);
                        let _ = app.tx.send(Command::Interval(n));
                        app.tray(NIM_MODIFY);
                    }
                } else if id == 15 && code == EN_CHANGE as usize {
                    app.refresh();
                } else {
                    app.command(id);
                }
                Some(LRESULT(0))
            }
            WM_NOTIFY => {
                let hdr = &*(lparam.0 as *const NMHDR);
                if hdr.hwndFrom == app.list {
                    if hdr.code == LVN_COLUMNCLICK {
                        let item = &*(lparam.0 as *const NMLISTVIEW);
                        let col = item.iSubItem as usize;
                        if col == app.sort {
                            app.descending = !app.descending;
                        } else {
                            app.sort = col;
                            app.descending = col != 0;
                        }
                        app.refresh();
                    } else if hdr.code == LVN_ITEMCHANGED {
                        app.show_details();
                    }
                }
                Some(LRESULT(0))
            }
            TRAY => {
                let event = (lparam.0 & 0xffff) as u32;
                if event == WM_LBUTTONUP || event == WM_LBUTTONDBLCLK {
                    app.show();
                } else if event == WM_RBUTTONUP {
                    if let Ok(menu) = CreatePopupMenu() {
                        for (id, text) in [
                            (100, "显示窗口"),
                            (
                                101,
                                if app.paused {
                                    "继续记录"
                                } else {
                                    "暂停记录"
                                },
                            ),
                            (102, "立即采样"),
                            (103, "打开日志目录"),
                            (104, "退出监测"),
                        ] {
                            let _ = AppendMenuW(menu, MF_STRING, id, PCWSTR(wide(text).as_ptr()));
                        }
                        let mut point = POINT::default();
                        let _ = GetCursorPos(&mut point);
                        let _ = SetForegroundWindow(hwnd);
                        let id = TrackPopupMenu(
                            menu,
                            TPM_RETURNCMD | TPM_RIGHTBUTTON,
                            point.x,
                            point.y,
                            0,
                            hwnd,
                            None,
                        );
                        let _ = DestroyMenu(menu);
                        app.command(id.0 as usize);
                    }
                }
                Some(LRESULT(0))
            }
            _ => None,
        }
    });
    result.unwrap_or_else(|| DefWindowProcW(hwnd, msg, wparam, lparam))
}
unsafe fn capture_window(hwnd: HWND, path: &Path) -> Result<()> {
    let mut r = RECT::default();
    GetWindowRect(hwnd, &mut r)?;
    let (width, height) = ((r.right - r.left) as u32, (r.bottom - r.top) as u32);
    let screen = GetDC(None);
    let dc = CreateCompatibleDC(screen);
    let mut info = BITMAPINFO::default();
    info.bmiHeader = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width as i32,
        biHeight: -(height as i32),
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    let bitmap = CreateDIBSection(screen, &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
    let old = SelectObject(dc, bitmap);
    SendMessageW(
        hwnd,
        WM_PRINT,
        WPARAM(dc.0 as usize),
        LPARAM(0x02 | 0x04 | 0x08 | 0x10),
    );
    let raw = std::slice::from_raw_parts(bits as *const u8, (width * height * 4) as usize);
    let mut rgba = Vec::with_capacity(raw.len());
    for p in raw.chunks_exact(4) {
        rgba.extend([p[2], p[1], p[0], 255]);
    }
    image::save_buffer(path, &rgba, width, height, image::ColorType::Rgba8)?;
    SelectObject(dc, old);
    let _ = DeleteObject(bitmap);
    let _ = DeleteDC(dc);
    ReleaseDC(None, screen);
    Ok(())
}
fn run() -> Result<()> {
    let paths = Paths::discover()?;
    let args: Vec<_> = std::env::args().collect();
    if args.iter().any(|s| {
        [
            "--capture",
            "--quit",
            "--hide",
            "--pause",
            "--resume",
            "--ui-check",
            "--guard-warning-test",
        ]
        .contains(&s.as_str())
    }) {
        unsafe {
            let hwnd = FindWindowW(CLASS, None)?;
            PostMessageW(
                hwnd,
                if args.iter().any(|s| s == "--quit") {
                    QUIT
                } else if args.iter().any(|s| s == "--hide") {
                    WM_CLOSE
                } else if args.iter().any(|s| s == "--pause") {
                    PAUSE
                } else if args.iter().any(|s| s == "--resume") {
                    RESUME
                } else if args.iter().any(|s| s == "--guard-warning-test") {
                    GUARD_WARNING_TEST
                } else if args.iter().any(|s| s == "--ui-check") {
                    CHECK
                } else {
                    CAPTURE
                },
                WPARAM(0),
                LPARAM(0),
            )?;
        }
        return Ok(());
    }
    if args.iter().any(|s| s == "--once") {
        let mut c = collect::Collector::new();
        let s = c.sample(paths.interval())?;
        println!(
            "{}",
            json!({"processes":s.processes.len(),"groups":s.groups.len(),"collection_ms":s.collection_ms,"logs":paths.logs})
        );
        return Ok(());
    }
    unsafe {
        let mutex = CreateMutexW(None, false, w!("Local\\RustMemoryWatch20261019"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if !args.iter().any(|s| s == "--background") {
                if let Ok(hwnd) = FindWindowW(CLASS, None) {
                    let _ = PostMessageW(hwnd, SHOW, WPARAM(0), LPARAM(0));
                }
            }
            CloseHandle(mutex)?;
            return Ok(());
        }
        let (tx, commands) = mpsc::channel();
        let (events, rx) = mpsc::sync_channel(16);
        let seconds = paths.interval();
        let p = paths.clone();
        let handle = thread::spawn(move || worker(p, seconds, commands, events));
        let app = App::new(paths.clone(), tx.clone(), rx)?;
        let hwnd = app.hwnd;
        APP.with(|a| *a.borrow_mut() = Some(app));
        let _ = ShowWindow(
            hwnd,
            if args.iter().any(|s| s == "--background") {
                SW_HIDE
            } else {
                SW_SHOW
            },
        );
        let _ = UpdateWindow(hwnd);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = tx.send(Command::Quit);
        let _ = handle.join();
        APP.with(|a| {
            if let Some(app) = a.borrow_mut().take() {
                app.tray(NIM_DELETE);
                let _ = DestroyWindow(app.hwnd);
                let _ = DeleteObject(app.font);
                let _ = DestroyIcon(app.icon);
            }
        });
        CloseHandle(mutex)?;
    }
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        if let Ok(paths) = Paths::discover() {
            let _ = fs::write(paths.logs.join("startup-error.txt"), format!("{e:#}"));
        }
        eprintln!("{e:#}");
        std::process::exit(1);
    }
}
