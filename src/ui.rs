use crate::{
    body,
    data::{self, Catalog, Row},
    desktop::{Action, Workers},
    guard, host, now,
    search::{ranked_matches, ChatSearch},
    Paths, Settings, State, Stop,
};
use anyhow::Result;
use rand::seq::IteratorRandom;
use serde_json::json;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    mem::size_of,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};
use windows::{
    core::{w, PCWSTR, PWSTR},
    Win32::{
        Foundation::*,
        Graphics::{Direct2D::Common::*, Direct2D::*, DirectWrite::*, Dxgi::Common::*, Gdi::*},
        System::{LibraryLoader::*, Threading::*},
        UI::{
            Controls::*,
            HiDpi::*,
            Input::{Ime::*, KeyboardAndMouse::*},
            Shell::*,
            WindowsAndMessaging::*,
        },
    },
};

const CLASS: PCWSTR = w!("CodexSessionBarRust");
const TRAY_MESSAGE: u32 = WM_APP + 1;
const QUIT_MESSAGE: u32 = WM_APP + 2;
const HIDE_MESSAGE: u32 = WM_APP + 3;
const RECENT_HOTKEY: i32 = 0x5342;
const BG: u32 = 0x23272b;
const PANEL: u32 = 0x242a2e;
const TEXT: u32 = 0xe8eef2;
const MUTED: u32 = 0xa8b8c4;
const ORANGE: u32 = 0xf5a34b;
const GREEN: u32 = 0x65d69b;
const NEW_RESULT: u32 = 0x70b7ff;
const DETAILS_HEIGHT: i32 = 408;
thread_local! {static APP:RefCell<Option<App>>=const{RefCell::new(None)};}
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
fn rect(x: f32, y: f32, width: f32, height: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left: x,
        top: y,
        right: x + width,
        bottom: y + height,
    }
}
fn color(rgb: u32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((rgb >> 16) & 255) as f32 / 255.,
        g: ((rgb >> 8) & 255) as f32 / 255.,
        b: (rgb & 255) as f32 / 255.,
        a: 1.,
    }
}
// The original tray pictogram: a rounded session bar with two short text strokes.
// Rasterize at the tray's native size with supersampling, retaining transparent edges.
fn tray_pixels(size: usize) -> Vec<u32> {
    let mut pixels = vec![0; size * size];
    for y in 0..size {
        for x in 0..size {
            let mut coverage = 0u32;
            for sy in 0..4 {
                for sx in 0..4 {
                    let px = (x as f32 + (sx as f32 + 0.5) / 4.) * 24. / size as f32;
                    let py = (y as f32 + (sy as f32 + 0.5) / 4.) * 24. / size as f32;
                    let qx = (px - 12.).abs() - 7.;
                    let qy = (py - 12.).abs() - 3.;
                    let outline =
                        (qx.max(0.).hypot(qy.max(0.)) + qx.max(qy).min(0.) - 3.).abs() <= 0.85;
                    let text = (px - px.clamp(6., 9.)).hypot(py - 12.) <= 0.85
                        || (px - px.clamp(12., 18.)).hypot(py - 12.) <= 0.85;
                    coverage += u32::from(outline || text);
                }
            }
            let a = coverage * 255 / 16;
            pixels[y * size + x] =
                (a << 24) | ((0xb7 * a / 255) << 16) | ((0xc2 * a / 255) << 8) | (0xcf * a / 255);
        }
    }
    pixels
}
unsafe fn create_tray_icon() -> Result<HICON> {
    let size = GetSystemMetrics(SM_CXSMICON).max(16) as usize;
    let pixels = tray_pixels(size);
    let bitmap = CreateBitmap(
        size as i32,
        size as i32,
        1,
        32,
        Some(pixels.as_ptr().cast()),
    );
    let mask_bytes = vec![0u8; size * size.div_ceil(16) * 2];
    let mask = CreateBitmap(
        size as i32,
        size as i32,
        1,
        1,
        Some(mask_bytes.as_ptr().cast()),
    );
    if bitmap.is_invalid() || mask.is_invalid() {
        let _ = DeleteObject(bitmap);
        let _ = DeleteObject(mask);
        anyhow::bail!("Cannot create Session Bar tray bitmap");
    }
    let icon = CreateIconIndirect(&ICONINFO {
        fIcon: BOOL(1),
        hbmMask: mask,
        hbmColor: bitmap,
        ..Default::default()
    });
    let _ = DeleteObject(bitmap);
    let _ = DeleteObject(mask);
    Ok(icon?)
}
#[derive(Clone, Debug, PartialEq)]
enum Hit {
    Icon,
    Title,
    NewResults(usize),
    Grip,
    Hide,
    Details,
    Recent(String),
    ChatIcon(String),
    ChooseFixed,
    Fixed(String),
    Back,
    Random,
    Auto,
    Clear,
    Emoji(usize),
    Refresh,
    Dock,
    Follow,
    Guard,
    Body,
    Quit,
}
#[derive(Clone)]
struct Area {
    r: D2D_RECT_F,
    hit: Hit,
}
impl Area {
    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.r.left && x < self.r.right && y >= self.r.top && y < self.r.bottom
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
enum Popup {
    Recent,
    Icons,
    Details,
    Fixed,
}

struct Painter {
    target: ID2D1DCRenderTarget,
    text: IDWriteFactory,
    formats: RefCell<HashMap<String, IDWriteTextFormat>>,
    chinese: String,
}
impl Painter {
    unsafe fn new() -> Result<Self> {
        let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_IGNORE,
            },
            dpiX: 96.,
            dpiY: 96.,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let target = factory.CreateDCRenderTarget(&props)?;
        target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE);
        let text: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let mut collection = None;
        text.GetSystemFontCollection(&mut collection, false)?;
        let mut chinese = "Microsoft YaHei".to_string();
        if let Some(collection) = collection {
            let (mut index, mut exists) = (0, BOOL(0));
            collection.FindFamilyName(w!("Microsoft YaHei UI"), &mut index, &mut exists)?;
            if exists.as_bool() {
                chinese = "Microsoft YaHei UI".into();
            }
        }
        Ok(Self {
            target,
            text,
            formats: RefCell::new(HashMap::new()),
            chinese,
        })
    }
    unsafe fn format(&self, size: f32, emoji: bool, text: &str) -> Result<IDWriteTextFormat> {
        let font = if emoji {
            "Segoe UI Emoji"
        } else if text.chars().any(|c| c >= '\u{2e80}' && c <= '\u{ffff}') {
            &self.chinese
        } else {
            "Segoe UI"
        };
        let key = format!("{font}:{size}");
        if let Some(f) = self.formats.borrow().get(&key) {
            return Ok(f.clone());
        }
        let f = self.text.CreateTextFormat(
            PCWSTR(wide(font).as_ptr()),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size,
            w!("zh-CN"),
        )?;
        f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
        f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
        let sign = self.text.CreateEllipsisTrimmingSign(&f)?;
        f.SetTrimming(
            &DWRITE_TRIMMING {
                granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                delimiter: 0,
                delimiterCount: 0,
            },
            &sign,
        )?;
        self.formats.borrow_mut().insert(key, f.clone());
        Ok(f)
    }
    unsafe fn label(&self, text: &str, r: D2D_RECT_F, size: f32, rgb: u32, emoji: bool) {
        let _ = (|| -> Result<()> {
            let f = self.format(size, emoji, text)?;
            let chars: Vec<u16> = text.encode_utf16().collect();
            let layout = self.text.CreateTextLayout(
                &chars,
                &f,
                (r.right - r.left).max(1.),
                (r.bottom - r.top).max(1.),
            )?;
            let brush = self.target.CreateSolidColorBrush(&color(rgb), None)?;
            self.target.DrawTextLayout(
                D2D_POINT_2F {
                    x: r.left,
                    y: r.top,
                },
                &layout,
                &brush,
                D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT | D2D1_DRAW_TEXT_OPTIONS_CLIP,
            );
            Ok(())
        })();
    }
    unsafe fn highlighted_label(
        &self,
        text: &str,
        r: D2D_RECT_F,
        size: f32,
        query: &str,
        exact: bool,
    ) {
        let _ = (|| -> Result<()> {
            let f = self.format(size, false, text)?;
            let chars: Vec<u16> = text.encode_utf16().collect();
            let layout = self.text.CreateTextLayout(
                &chars,
                &f,
                (r.right - r.left).max(1.),
                (r.bottom - r.top).max(1.),
            )?;
            let normal = self.target.CreateSolidColorBrush(&color(MUTED), None)?;
            let highlight = self.target.CreateSolidColorBrush(&color(0xffd27f), None)?;
            for (start, end) in body::match_ranges(text, query, exact) {
                let range = DWRITE_TEXT_RANGE {
                    startPosition: text[..start].encode_utf16().count() as u32,
                    length: text[start..end].encode_utf16().count() as u32,
                };
                layout.SetDrawingEffect(&highlight, range)?;
                layout.SetFontWeight(DWRITE_FONT_WEIGHT_SEMI_BOLD, range)?;
            }
            self.target.DrawTextLayout(
                D2D_POINT_2F {
                    x: r.left,
                    y: r.top,
                },
                &layout,
                &normal,
                D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT | D2D1_DRAW_TEXT_OPTIONS_CLIP,
            );
            Ok(())
        })();
    }
    unsafe fn width(&self, text: &str, size: f32) -> f32 {
        (|| -> Result<f32> {
            let f = self.format(size, false, text)?;
            let chars: Vec<u16> = text.encode_utf16().collect();
            let layout = self.text.CreateTextLayout(&chars, &f, 10000., 40.)?;
            let mut metrics = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut metrics)?;
            Ok(metrics.widthIncludingTrailingWhitespace)
        })()
        .unwrap_or(80.)
    }
    unsafe fn fill(&self, r: D2D_RECT_F, rgb: u32, radius: f32) {
        if let Ok(b) = self.target.CreateSolidColorBrush(&color(rgb), None) {
            self.target.FillRoundedRectangle(
                &D2D1_ROUNDED_RECT {
                    rect: r,
                    radiusX: radius,
                    radiusY: radius,
                },
                &b,
            );
        }
    }
    unsafe fn border(&self, r: D2D_RECT_F, rgb: u32, radius: f32) {
        if let Ok(b) = self.target.CreateSolidColorBrush(&color(rgb), None) {
            self.target.DrawRoundedRectangle(
                &D2D1_ROUNDED_RECT {
                    rect: r,
                    radiusX: radius,
                    radiusY: radius,
                },
                &b,
                1.,
                None,
            );
        }
    }
    unsafe fn line(&self, x: f32, y: f32, x2: f32, y2: f32, rgb: u32, width: f32) {
        if let Ok(b) = self.target.CreateSolidColorBrush(&color(rgb), None) {
            self.target.DrawLine(
                D2D_POINT_2F { x, y },
                D2D_POINT_2F { x: x2, y: y2 },
                &b,
                width,
                None,
            );
        }
    }
    unsafe fn circle(&self, x: f32, y: f32, radius: f32, rgb: u32) {
        if let Ok(b) = self.target.CreateSolidColorBrush(&color(rgb), None) {
            self.target.FillEllipse(
                &D2D1_ELLIPSE {
                    point: D2D_POINT_2F { x, y },
                    radiusX: radius,
                    radiusY: radius,
                },
                &b,
            );
        }
    }
    unsafe fn toggle(&self, x: f32, y: f32, on: bool) {
        self.fill(
            rect(x, y, 28., 16.),
            if on { 0x30c777 } else { 0x4b555e },
            8.,
        );
        self.circle(x + if on { 20. } else { 8. }, y + 8., 6., 0xffffff);
    }
    unsafe fn action_icon(&self, hit: &Hit, x: f32, y: f32) {
        let c = if *hit == Hit::Refresh {
            0xaed8cc
        } else {
            MUTED
        };
        match hit {
            Hit::Body => {
                self.line(x + 1., y + 3., x + 18., y + 3., c, 1.4);
                self.line(x + 1., y + 8., x + 10., y + 8., c, 1.4);
                self.line(x + 1., y + 13., x + 7., y + 13., c, 1.4);
                self.border(rect(x + 12., y + 9., 7., 7.), TEXT, 3.5);
                self.line(x + 18., y + 15., x + 22., y + 19., TEXT, 1.5);
            }
            Hit::Refresh => {
                // Two opposing arcs and aligned arrowheads form a clear refresh
                // glyph; the old single arc resembled a power switch.
                for start in [-165_f32, 15.] {
                    for i in 0..30 {
                        let a = (start + i as f32 * 5.).to_radians();
                        let b = (start + (i + 1) as f32 * 5.).to_radians();
                        self.line(
                            x + 11. + 8. * a.cos(),
                            y + 11. + 8. * a.sin(),
                            x + 11. + 8. * b.cos(),
                            y + 11. + 8. * b.sin(),
                            c,
                            1.7,
                        );
                    }
                }
                for (ax, ay, bx, by) in [
                    (19., 4., 19., 9.),
                    (19., 9., 14., 9.),
                    (3., 18., 3., 13.),
                    (3., 13., 8., 13.),
                ] {
                    self.line(x + ax, y + ay, x + bx, y + by, c, 1.7);
                    self.circle(x + ax, y + ay, 0.85, c);
                    self.circle(x + bx, y + by, 0.85, c);
                }
            }
            Hit::Auto => {
                self.line(x + 3., y + 18., x + 17., y + 4., TEXT, 2.);
                self.line(x + 10., y + 11., x + 13., y + 14., TEXT, 2.);
                for (cx, cy) in [(4., 4.), (20., 14.)] {
                    self.line(x + cx - 3., y + cy, x + cx + 3., y + cy, ORANGE, 1.4);
                    self.line(x + cx, y + cy - 3., x + cx, y + cy + 3., ORANGE, 1.4);
                }
            }
            Hit::Clear => {
                for (a, b, cx, d) in [
                    (3., 12., 12., 3.),
                    (12., 3., 20., 11.),
                    (20., 11., 12., 19.),
                    (12., 19., 3., 12.),
                    (8., 7., 16., 15.),
                ] {
                    self.line(x + a, y + b, x + cx, y + d, c, 1.5);
                }
                self.line(x + 12., y + 19., x + 22., y + 19., c, 1.5);
            }
            Hit::Back => {
                self.line(x + 14., y + 3., x + 6., y + 10., c, 1.5);
                self.line(x + 6., y + 10., x + 14., y + 17., c, 1.5);
            }
            Hit::Dock => {
                self.border(rect(x + 1., y + 2., 21., 17.), c, 2.);
                self.line(x + 1., y + 7., x + 22., y + 7., c, 1.4);
                self.fill(rect(x + 12., y + 3., 8., 3.), GREEN, 1.);
            }
            Hit::Quit => {
                self.border(rect(x + 2., y + 3., 13., 17.), c, 2.);
                self.line(x + 9., y + 11., x + 23., y + 11., c, 1.5);
                self.line(x + 19., y + 7., x + 23., y + 11., c, 1.5);
                self.line(x + 19., y + 15., x + 23., y + 11., c, 1.5);
            }
            _ => {}
        }
    }
}

struct App {
    paths: Paths,
    settings: Settings,
    state: State,
    stop: Stop,
    catalog: Catalog,
    painter: Painter,
    bar: HWND,
    popup: HWND,
    edit: HWND,
    combo: HWND,
    edit_font: HFONT,
    child_brush: HBRUSH,
    tray_icon: HICON,
    tips: Vec<Tips>,
    popup_kind: Option<Popup>,
    icon_target: Option<(String, String)>,
    icon_panel: Option<IconPanel>,
    shortcut_registered: bool,
    shortcut_error: String,
    rows: Vec<Row>,
    hits: Vec<Area>,
    popup_hits: Vec<Area>,
    filters: Vec<usize>,
    chat_filters: Vec<usize>,
    chat_search: ChatSearch,
    search_body: bool,
    body_service: Option<body::Service>,
    body_generation: u64,
    body_pending: bool,
    index_pending: bool,
    index_due: Instant,
    body_state: body::Status,
    body_hits: HashSet<String>,
    body_previews: HashMap<String, String>,
    body_only: HashSet<String>,
    chat_scroll_drag: Option<f32>,
    chat_cursor: usize,
    groups: Vec<String>,
    category: String,
    search: String,
    scroll: usize,
    hidden: bool,
    host_hidden: bool,
    drag: Option<(POINT, [i32; 4])>,
    hover: Option<Hit>,
    width: i32,
    height: i32,
    popup_size: [i32; 2],
    popup_layout: Option<([i32; 4], u32)>,
    dpi: f32,
    last_draw: Instant,
    last_diag: Instant,
    last_identity: String,
    last_signature: String,
    last_host: Option<[i32; 4]>,
    event: HANDLE,
    testing: bool,
    quit: bool,
}
// The list retains its window, controls, filters and scroll under the popover.
struct IconPanel {
    window: HWND,
    edit: HWND,
    combo: HWND,
    search: String,
    category: String,
    filters: Vec<usize>,
    scroll: usize,
    hits: Vec<Area>,
    layout: Option<[i32; 4]>,
}
// Stable tool rectangles/text let hover delays finish despite frequent repaints.
struct Tips {
    owner: HWND,
    window: HWND,
    entries: Vec<(RECT, Vec<u16>)>,
}
impl Drop for Tips {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.window);
        }
    }
}
fn tip_text(hit: &Hit, catalog: &Catalog) -> String {
    match hit {
        Hit::Icon | Hit::ChatIcon(_) => "Change chat icon",
        Hit::Title => "Recent chats · Ctrl+Alt+J",
        Hit::NewResults(count) => {
            return format!("{count} newly completed chats · blue dot = unread result · open chat to mark as read")
        }
        Hit::Grip => "Drag session bar",
        Hit::Hide => "Hide to tray · Ctrl+Alt+J to reopen",
        Hit::Details => "Session settings",
        Hit::Recent(_) => "Open chat in Codex · blue dot = newly completed, unread result",
        Hit::Fixed(_) => "Use this fixed chat · blue dot = newly completed, unread result",
        Hit::ChooseFixed => "Choose a fixed chat",
        Hit::Back => "Back to session settings",
        Hit::Random => "Random icon",
        Hit::Auto => "Suggest icon from chat title · local matching",
        Hit::Clear => "Remove chat icon",
        Hit::Refresh => "Refresh account limits and chat list",
        Hit::Dock => "Dock in the blank title bar area",
        Hit::Follow => "Follow the current Codex chat",
        Hit::Guard => "Protect Codex Git · every 5 seconds · alerts and automatic cleanup",
        Hit::Body => "Search message text · off by default",
        Hit::Quit => "Quit Session Bar",
        Hit::Emoji(i) => {
            return catalog
                .entries
                .get(*i)
                .map(|e| e.name.clone())
                .unwrap_or_default()
        }
    }
    .into()
}
impl App {
    unsafe fn new(
        paths: Paths,
        settings: Settings,
        state: State,
        stop: Stop,
        testing: bool,
    ) -> Result<Self> {
        let _ = InitCommonControlsEx(&INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_WIN95_CLASSES,
        });
        let catalog = Catalog::load(&paths)?;
        let painter = Painter::new()?;
        let mut groups = vec!["默认集合".into(), "All emoji".into()];
        for e in &catalog.entries {
            if !groups.contains(&e.group) {
                groups.push(e.group.clone());
            }
        }
        let filters = catalog.filter("", "默认集合");
        let bar = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            CLASS,
            w!("Codex Session Bar"),
            WS_POPUP,
            0,
            0,
            650,
            34,
            None,
            None,
            GetModuleHandleW(None)?,
            None,
        )?;
        let popup = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            CLASS,
            w!("Codex Session Bar Menu"),
            WS_POPUP | WS_CLIPCHILDREN,
            0,
            0,
            640,
            460,
            bar,
            None,
            GetModuleHandleW(None)?,
            None,
        )?;
        let event = if testing {
            HANDLE::default()
        } else {
            CreateEventW(
                None,
                false,
                false,
                PCWSTR(wide(&format!("Local\\{}-show", paths.instance)).as_ptr()),
            )?
        };
        let mut app = Self {
            paths,
            settings,
            state,
            stop,
            catalog,
            painter,
            bar,
            popup,
            edit: HWND::default(),
            combo: HWND::default(),
            edit_font: HFONT::default(),
            child_brush: CreateSolidBrush(COLORREF(0x25211c)),
            tray_icon: create_tray_icon()?,
            tips: Vec::new(),
            popup_kind: None,
            icon_target: None,
            icon_panel: None,
            shortcut_registered: false,
            shortcut_error: String::new(),
            popup_layout: None,
            rows: Vec::new(),
            hits: Vec::new(),
            popup_hits: Vec::new(),
            filters,
            chat_filters: Vec::new(),
            chat_search: ChatSearch::default(),
            search_body: false,
            body_service: None,
            body_generation: 0,
            body_pending: false,
            index_pending: false,
            index_due: Instant::now() + std::time::Duration::from_secs(3),
            body_state: body::Status::default(),
            body_hits: HashSet::new(),
            body_previews: HashMap::new(),
            body_only: HashSet::new(),
            chat_scroll_drag: None,
            chat_cursor: 0,
            groups,
            category: "默认集合".into(),
            search: String::new(),
            scroll: 0,
            hidden: false,
            host_hidden: false,
            drag: None,
            hover: None,
            width: 650,
            height: 34,
            popup_size: [640, 460],
            dpi: 1.,
            last_draw: Instant::now() - std::time::Duration::from_secs(3),
            last_diag: Instant::now() - std::time::Duration::from_secs(3),
            last_identity: String::new(),
            last_signature: String::new(),
            last_host: None,
            event,
            testing,
            quit: false,
        };
        app.edit_font = CreateFontW(
            -14,
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            DEFAULT_PITCH.0 as u32,
            w!("Microsoft YaHei UI"),
        );
        if !testing {
            match RegisterHotKey(
                bar,
                RECENT_HOTKEY,
                MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
                b'J' as u32,
            ) {
                Ok(()) => app.shortcut_registered = true,
                Err(error) => app.shortcut_error = error.to_string(),
            }
            app.tray(true);
            SetTimer(bar, 1, 80, None);
        }
        Ok(app)
    }
    fn snapshot(&self) -> crate::Shared {
        self.state.lock().unwrap().clone()
    }
    fn identity(&self, live: &crate::Shared) -> String {
        if live.auto_thread {
            live.view.clone().unwrap_or_default()
        } else {
            live.fixed_thread.clone()
        }
    }
    fn current<'a>(&self, live: &'a crate::Shared) -> Option<&'a data::Snapshot> {
        live.snapshot
            .as_ref()
            .filter(|s| s.id == self.identity(live))
    }
    unsafe fn bar_text(&self, live: &crate::Shared) -> (String, String, String, String, String) {
        let current = self.current(live);
        let visible = live
            .auto_thread
            .then(|| data::visible_chat_title(&live.view_label))
            .flatten();
        let title = current
            .map(|s| self.catalog.strip(&s.label).to_string())
            .unwrap_or_else(|| {
                if let Some(label) = &visible {
                    self.catalog.strip(label).into()
                } else if self.identity(live).is_empty() {
                    "Recent chats".into()
                } else {
                    "Switching…".into()
                }
            });
        let icon = current
            .map(|s| self.catalog.icon(&s.label))
            .or_else(|| visible.as_ref().map(|s| self.catalog.icon(s)))
            .unwrap_or("◈".into());
        let stale = live.recent.stale || now() - live.recent.loaded_at > 10.;
        let busy = live
            .recent
            .busy_count
            .map(|n| {
                format!(
                    "{} Busy ({n}){}",
                    if n > 0 { "●" } else { "○" },
                    if stale { "*" } else { "" }
                )
            })
            .unwrap_or("Connecting…".into());
        let (tokens, rate, partial) = if let Some(s) = current {
            (s.tokens(), s.rate(), false)
        } else if visible.is_some() || !self.identity(live).is_empty() {
            (None, None, false)
        } else {
            live.recent.totals()
        };
        let tok = format!(
            "{}{}",
            data::compact(tokens),
            if partial && tokens.is_some() { "*" } else { "" }
        );
        let speed = rate
            .map(|v| format!("{v:.1}{} t/s", if partial { "*" } else { "" }))
            .unwrap_or("— t/s".into());
        (icon, title, busy, tok, speed)
    }
    unsafe fn render(&self, kind: Option<Popup>, width: f32, height: f32) -> Vec<Area> {
        let p = &self.painter;
        let live = self.snapshot();
        let mut hits = Vec::new();
        p.target
            .Clear(Some(&color(if kind.is_none() { BG } else { PANEL })));
        p.border(
            rect(0.5, 0.5, width - 1., height - 1.),
            0x47535b,
            if kind.is_none() { 8. } else { 10. },
        );
        let mut area = |hit: Hit, r: D2D_RECT_F| {
            if self.hover.as_ref() == Some(&hit) {
                p.fill(r, 0x35434a, 5.);
            }
            hits.push(Area { hit, r });
        };
        match kind {
            None => {
                let (icon, title, busy, tok, speed) = self.bar_text(&live);
                let current = self.current(&live);
                let mut x = 6.;
                area(Hit::Icon, rect(x, 3., 28., 28.));
                p.label(&icon, rect(x + 2., 3., 28., 27.), 19., TEXT, true);
                x += 34.;
                let tw = (p.width(&title, 12.) + 4.).clamp(64., 190.);
                area(Hit::Title, rect(x, 2., tw, 30.));
                p.label(&title, rect(x, 2., tw, 30.), 12., TEXT, false);
                x += tw;
                let new_results = live.recent.new_result_count();
                if new_results > 0 {
                    area(Hit::NewResults(new_results), rect(x, 2., 14., 30.));
                    p.circle(x + 7., 17., 3.3, NEW_RESULT);
                    x += 14.;
                }
                x += 6.;
                let sw = p.width(&busy, 12.) + 2.;
                p.label(
                    &busy,
                    rect(x, 2., sw, 30.),
                    12.,
                    if live.recent.busy_count.is_some_and(|n| n > 0) {
                        ORANGE
                    } else {
                        MUTED
                    },
                    false,
                );
                x += sw + 8.;
                let elapsed = current
                    .and_then(|s| {
                        s.turn.started_at.map(|start| {
                            data::duration(s.turn.completed_at.unwrap_or(now()) - start)
                        })
                    })
                    .unwrap_or("—".into());
                let ew = p.width(&elapsed, 11.).max(35.);
                p.label(&elapsed, rect(x, 2., ew, 30.), 11., MUTED, false);
                x += ew + 4.;
                area(Hit::Grip, rect(x, 2., 18., 30.));
                for dx in [6., 11.] {
                    for dy in [12., 17., 22.] {
                        p.circle(x + dx, dy, 1., MUTED);
                    }
                }
                x += 22.;
                for text in [&tok, &speed] {
                    let w = p.width(text, 11.) + 2.;
                    p.label(text, rect(x, 2., w, 30.), 11., MUTED, false);
                    x += w + 8.;
                }
                let shown: Vec<Option<&data::Quota>> = if live.quota.is_empty() {
                    vec![None]
                } else {
                    live.quota.iter().take(2).map(Some).collect()
                };
                for quota in shown {
                    let label = quota.map(|q| q.label.as_str()).unwrap_or("Limit");
                    let w = p.width(label, 11.) + 3.;
                    p.label(label, rect(x, 2., w, 30.), 11., MUTED, false);
                    x += w + 2.;
                    let rgb = quota
                        .map(|q| {
                            if q.remaining < 25. {
                                0xf87171
                            } else if q.remaining < 50. {
                                ORANGE
                            } else if q.remaining < 75. {
                                0x60a5fa
                            } else {
                                0x27ce79
                            }
                        })
                        .unwrap_or(0x87929d);
                    p.border(rect(x, 12., 24., 11.), rgb, 2.);
                    p.fill(rect(x + 24., 15., 2., 5.), rgb, 1.);
                    if let Some(q) = quota {
                        p.fill(
                            rect(x + 2., 14., (q.remaining as f32 / 100. * 20.).max(0.5), 7.),
                            rgb,
                            1.,
                        );
                    }
                    x += 30.;
                    // Ring = fraction of the reset window remaining.
                    if let Some(q) = quota {
                        if let (Some(reset), Some(minutes)) = (q.reset, q.minutes) {
                            let share = ((reset - now()) / (minutes * 60.)).clamp(0., 1.);
                            for i in 0..28 {
                                let angle = (i as f32 / 28.) * std::f32::consts::TAU
                                    - std::f32::consts::FRAC_PI_2;
                                p.circle(
                                    x + 4. + angle.cos() * 4.,
                                    17. + angle.sin() * 4.,
                                    0.8,
                                    if (i as f64 / 28.) < share {
                                        0x80adff
                                    } else {
                                        0x46525d
                                    },
                                );
                            }
                        }
                    }
                    x += 12.;
                    let percent = quota
                        .map(|q| {
                            format!(
                                "{:.0}%{}",
                                q.remaining,
                                if !live.quota_error.is_empty() || now() - live.quota_at > 180. {
                                    "*"
                                } else {
                                    ""
                                }
                            )
                        })
                        .unwrap_or("—".into());
                    let w = p.width(&percent, 11.) + 2.;
                    p.label(&percent, rect(x, 2., w, 30.), 11., TEXT, false);
                    x += w + 7.;
                }
                area(Hit::Details, rect(x, 3., 25., 28.));
                p.line(x + 8., 15., x + 12., 19., MUTED, 1.4);
                p.line(x + 12., 19., x + 16., 15., MUTED, 1.4);
                x += 29.;
                area(Hit::Hide, rect(x, 3., 25., 28.));
                p.line(x + 8., 13., x + 16., 21., MUTED, 1.3);
                p.line(x + 16., 13., x + 8., 21., MUTED, 1.3);
            }
            Some(Popup::Recent) => {
                p.fill(rect(12., 9., 244., 30.), 0x1c2125, 6.);
                p.border(rect(12., 9., 244., 30.), 0x43515a, 6.);
                if self.testing {
                    p.label(
                        if self.search.is_empty() {
                            "Search chats / pinyin…"
                        } else {
                            &self.search
                        },
                        rect(23., 9., 222., 30.),
                        12.,
                        MUTED,
                        false,
                    );
                }
                area(Hit::Body, rect(264., 8., 60., 32.));
                p.action_icon(&Hit::Body, 266., 14.);
                p.toggle(293., 16., self.search_body);
                let (tokens, rate, partial) = live.recent.totals();
                let total = format!(
                    "Total {}{} t/s  ·  {}{} TOK",
                    rate.map(|r| format!("{r:.1}")).unwrap_or("—".into()),
                    if partial && rate.is_some() { "*" } else { "" },
                    data::compact(tokens),
                    if partial && tokens.is_some() { "*" } else { "" }
                );
                p.fill(rect(332., 8., 32., 32.), 0x2b3a3c, 8.);
                p.border(rect(332., 8., 32., 32.), 0x445c5c, 8.);
                area(Hit::Refresh, rect(332., 8., 32., 32.));
                p.action_icon(&Hit::Refresh, 337., 13.);
                let new_results = self.chat_filters.iter()
                    .filter(|&&index| self.rows[index].newly_completed()).count();
                p.label(
                    &format!(
                        "{} chats{}",
                        self.chat_filters.len(),
                        if new_results > 0 {
                            format!(" · {new_results} new")
                        } else {
                            String::new()
                        }
                    ),
                    rect(379., 9., width - 391., 30.),
                    11.,
                    MUTED,
                    false,
                );
                for (slot, &index) in self
                    .chat_filters
                    .iter()
                    .skip(self.scroll)
                    .take(8)
                    .enumerate()
                {
                    let row = &self.rows[index];
                    let y = 48. + slot as f32 * 43.;
                    let busy = row.state == "Busy";
                    let current = row.id == self.identity(&live);
                    let r = rect(12., y, width - 30., 39.);
                    p.fill(
                        r,
                        if current {
                            0x293d36
                        } else if busy {
                            0x342c23
                        } else {
                            PANEL
                        },
                        7.,
                    );
                    if current || self.chat_cursor == self.scroll + slot {
                        p.border(r, if current { 0x527b69 } else { 0x59849c }, 7.);
                    }
                    let icon_hit = Hit::ChatIcon(row.id.clone());
                    area(icon_hit.clone(), rect(18., y + 4., 30., 30.));
                    area(Hit::Recent(row.id.clone()), r);
                    p.fill(
                        rect(18., y + 4., 30., 30.),
                        if self.hover == Some(icon_hit) {
                            0x496173
                        } else {
                            0x303b43
                        },
                        7.,
                    );
                    p.label(
                        &self.catalog.icon(&row.label),
                        rect(22., y + 2., 25., 34.),
                        16.,
                        TEXT,
                        true,
                    );
                    let label_width = if busy { width - 365. } else { width - 305. };
                    let body_only = self.body_only.contains(&row.id);
                    let snippet = self
                        .body_previews
                        .get(&row.id)
                        .filter(|_| self.search_body && !self.search.trim().is_empty());
                    let line_height = if snippet.is_some() { 18. } else { 34. };
                    p.label(
                        self.catalog.strip(&row.label),
                        rect(
                            49.,
                            y + 2.,
                            label_width - if body_only { 39. } else { 0. },
                            line_height,
                        ),
                        13.,
                        TEXT,
                        false,
                    );
                    if body_only {
                        p.label(
                            "Body",
                            rect(49. + label_width - 34., y + 2., 34., line_height),
                            10.,
                            0x81b5d1,
                            false,
                        );
                    }
                    if busy {
                        let speed = row.rate().map(|r| format!("{r:.1}")).unwrap_or("—".into());
                        let metrics = format!(
                            "{speed} t/s   ·   {} TOK   ·   {}",
                            data::compact(row.tokens),
                            row.started_at
                                .map(|t| data::duration(now() - t))
                                .unwrap_or("—".into())
                        );
                        p.label(
                            &metrics,
                            rect(width - 310., y + 2., 220., line_height),
                            11.,
                            MUTED,
                            false,
                        );
                    } else {
                        p.label(
                            &format!(
                                "{} TOK   ·   {}",
                                data::compact(row.tokens),
                                row.total_seconds.map(data::duration).unwrap_or("—".into())
                            ),
                            rect(width - 250., y + 2., 160., line_height),
                            11.,
                            MUTED,
                            false,
                        );
                    }
                    if let Some(snippet) = snippet {
                        p.highlighted_label(
                            snippet,
                            rect(49., y + 20., width - 145., 17.),
                            11.,
                            &self.search,
                            false,
                        );
                    }
                    p.label(
                        &row.status_text(),
                        rect(width - 85., y + 2., 70., 34.),
                        12.,
                        if busy {
                            ORANGE
                        } else if row.state == "Done" {
                            GREEN
                        } else if row.state == "Failed" {
                            0xf87171
                        } else if row.state == "Waiting" {
                            ORANGE
                        } else {
                            MUTED
                        },
                        false,
                    );
                    if row.newly_completed() {
                        p.circle(width - 29., y + 19., 3.3, NEW_RESULT);
                    }
                }
                if self.chat_filters.is_empty() {
                    p.label(
                        if self.body_pending {
                            "Searching chat bodies…"
                        } else if !self.rows.is_empty() {
                            "No matching chats"
                        } else if live.recent.error.is_empty() {
                            "Loading recent chats…"
                        } else {
                            "Cached list unavailable · refresh pending"
                        },
                        rect(20., 86., width - 40., 45.),
                        13.,
                        MUTED,
                        false,
                    );
                }
                if self.chat_filters.len() > 8 {
                    let thumb = (337. * 8. / self.chat_filters.len() as f32).max(20.);
                    let offset =
                        (337. - thumb) * self.scroll as f32 / (self.chat_filters.len() - 8) as f32;
                    p.fill(rect(width - 11., 50., 4., 337.), 0x303c44, 2.);
                    p.fill(rect(width - 11., 50. + offset, 4., thumb), 0x6f8b9d, 2.);
                }
                let hint = if live.recent.stale || now() - live.recent.loaded_at > 10. {
                    " · Cached"
                } else {
                    ""
                };
                p.line(16., height - 38., width - 16., height - 38., 0x354148, 1.);
                let count = if self.index_pending || self.body_pending {
                    format!(
                        "Index {:.0}% · {} / {} · {:.0} MiB",
                        self.body_state.progress_percent(),
                        self.body_state.indexed,
                        self.body_state.available,
                        self.body_state.bytes_read as f64 / 1048576.
                    )
                } else if !self.body_state.error.is_empty() {
                    "Index unavailable · retry pending".into()
                } else if self.body_state.complete {
                    format!(
                        "{} / {} chats · Index {:.0}%{}",
                        self.chat_filters.len(),
                        self.rows.len(),
                        self.body_state.progress_percent(),
                        if self.body_state.skipped > 0 {
                            " · partial"
                        } else {
                            ""
                        }
                    )
                } else {
                    format!(
                        "{} / {} chats{hint}",
                        self.chat_filters.len(),
                        self.rows.len()
                    )
                };
                p.label(
                    &count,
                    rect(16., height - 29., 235., 24.),
                    11.,
                    MUTED,
                    false,
                );
                let tw = p.width(&total, 11.).min(width - 265.);
                p.label(
                    &total,
                    rect(width - 16. - tw, height - 29., tw + 1., 24.),
                    11.,
                    MUTED,
                    false,
                );
            }
            Some(Popup::Icons) => {
                let (search, category, filters, scroll) = self
                    .icon_panel
                    .as_ref()
                    .map(|panel| {
                        (
                            panel.search.as_str(),
                            panel.category.as_str(),
                            &panel.filters,
                            panel.scroll,
                        )
                    })
                    .unwrap_or((&self.search, &self.category, &self.filters, self.scroll));
                area(Hit::Random, rect(12., 9., 36., 32.));
                p.label("🎲", rect(17., 9., 30., 32.), 20., TEXT, true);
                area(Hit::Auto, rect(50., 9., 34., 32.));
                p.action_icon(&Hit::Auto, 56., 14.);
                let icon_title = self
                    .icon_target
                    .as_ref()
                    .map(|(_, label)| self.catalog.strip(label))
                    .unwrap_or("Session icon");
                p.label(icon_title, rect(92., 9., 235., 32.), 14., TEXT, false);
                area(Hit::Clear, rect(width - 46., 10., 32., 30.));
                p.action_icon(&Hit::Clear, width - 41., 15.);
                for r in [rect(14., 49., 190., 29.), rect(214., 49., 155., 29.)] {
                    p.fill(r, 0x1c2125, 3.);
                    p.border(r, 0x47535b, 3.);
                }
                p.label(
                    if search.is_empty() {
                        "Search all emoji…"
                    } else {
                        search
                    },
                    rect(21., 49., 180., 29.),
                    13.,
                    MUTED,
                    false,
                );
                p.label(category, rect(222., 49., 131., 29.), 13., TEXT, false);
                p.label("⌄", rect(350., 49., 16., 29.), 13., MUTED, false);
                let columns = 10;
                let cell = 36.;
                let start = scroll * columns;
                for (slot, index) in filters.iter().skip(start).take(90).enumerate() {
                    let x = 12. + (slot % columns) as f32 * cell;
                    let y = 98. + (slot / columns) as f32 * cell;
                    area(Hit::Emoji(*index), rect(x, y, cell - 2., cell - 2.));
                    p.label(
                        &self.catalog.entries[*index].emoji,
                        rect(x + 5., y + 1., 30., 32.),
                        22.,
                        TEXT,
                        true,
                    );
                }
                p.label(
                    &if search.trim().is_empty() {
                        format!("{} icons · scroll for more", filters.len())
                    } else {
                        format!("{} results · all emoji", filters.len())
                    },
                    rect(16., height - 26., width - 32., 22.),
                    11.,
                    MUTED,
                    false,
                );
            }
            Some(Popup::Fixed) => {
                p.label(
                    "Choose a fixed chat",
                    rect(16., 9., 350., 30.),
                    14.,
                    TEXT,
                    false,
                );
                area(Hit::Back, rect(width - 74., 9., 58., 30.));
                p.action_icon(&Hit::Back, width - 48., 14.);
                p.fill(rect(16., 49., width - 32., 35.), 0x1c2125, 6.);
                p.border(rect(16., 49., width - 32., 35.), 0x43515a, 6.);
                if self.testing {
                    p.label(
                        if self.search.is_empty() {
                            "Search chats…"
                        } else {
                            &self.search
                        },
                        rect(27., 49., width - 54., 35.),
                        13.,
                        MUTED,
                        false,
                    );
                }
                for (slot, &index) in self
                    .chat_filters
                    .iter()
                    .skip(self.scroll)
                    .take(8)
                    .enumerate()
                {
                    let row = &self.rows[index];
                    let y = 100. + slot as f32 * 44.;
                    let selected = !live.auto_thread && row.id == live.fixed_thread;
                    let busy = row.state == "Busy";
                    let r = rect(16., y, width - 32., 40.);
                    p.fill(
                        r,
                        if selected {
                            0x293f37
                        } else if busy {
                            0x352e24
                        } else {
                            0x282f34
                        },
                        7.,
                    );
                    let icon_hit = Hit::ChatIcon(row.id.clone());
                    area(icon_hit.clone(), rect(23., y + 5., 33., 30.));
                    area(Hit::Fixed(row.id.clone()), r);
                    if selected || self.chat_cursor == self.scroll + slot {
                        p.border(r, if selected { 0x5ca985 } else { 0x59849c }, 7.);
                    }
                    p.fill(
                        rect(23., y + 5., 30., 30.),
                        if self.hover == Some(icon_hit) {
                            0x496173
                        } else {
                            0x333c43
                        },
                        7.,
                    );
                    p.label(
                        &self.catalog.icon(&row.label),
                        rect(26., y + 4., 28., 32.),
                        20.,
                        TEXT,
                        true,
                    );
                    let with_metrics = row.tokens.is_some() || row.total_seconds.is_some();
                    p.label(
                        self.catalog.strip(&row.label),
                        rect(
                            62.,
                            y + 2.,
                            width - 202.,
                            if with_metrics { 21. } else { 36. },
                        ),
                        13.,
                        TEXT,
                        false,
                    );
                    if with_metrics {
                        p.label(
                            &if busy {
                                format!(
                                    "{} TOK  ·  {} t/s",
                                    data::compact(row.tokens),
                                    row.rate().map(|v| format!("{v:.1}")).unwrap_or("—".into())
                                )
                            } else {
                                format!(
                                    "{} TOK  ·  {} total",
                                    data::compact(row.tokens),
                                    row.total_seconds.map(data::duration).unwrap_or("—".into())
                                )
                            },
                            rect(62., y + 22., width - 202., 16.),
                            10.,
                            MUTED,
                            false,
                        );
                    }
                    p.label(
                        &row.status_text(),
                        rect(width - 126., y + 3., 85., 34.),
                        11.,
                        match row.state.as_str() {
                            "Busy" | "Waiting" => ORANGE,
                            "Done" => GREEN,
                            "Failed" => 0xf87171,
                            _ => MUTED,
                        },
                        false,
                    );
                    if selected {
                        p.label("✓", rect(width - 39., y + 3., 20., 34.), 14., GREEN, false);
                    }
                    if row.newly_completed() {
                        p.circle(width - 57., y + 20., 3.3, NEW_RESULT);
                    }
                }
                if self.chat_filters.is_empty() {
                    p.label(
                        "No matching chats",
                        rect(27., 120., width - 54., 44.),
                        13.,
                        MUTED,
                        false,
                    );
                }
                if self.chat_filters.len() > 8 {
                    p.fill(rect(width - 9., 101., 3., 350.), 0x35414a, 1.5);
                    let thumb = (350. * 8. / self.chat_filters.len() as f32).max(20.);
                    let offset =
                        (350. - thumb) * self.scroll as f32 / (self.chat_filters.len() - 8) as f32;
                    p.fill(rect(width - 9., 101. + offset, 3., thumb), 0x77929f, 1.5);
                }
                p.label(
                    &format!(
                        "{} chats  ·  Scroll to browse  ·  Enter to select",
                        self.chat_filters.len()
                    ),
                    rect(18., height - 35., width - 36., 26.),
                    11.,
                    MUTED,
                    false,
                );
            }
            Some(Popup::Details) => {
                let field = rect(16., 77., width - 32., 32.);
                p.fill(field, 0x1c2125, 6.);
                area(Hit::ChooseFixed, field);
                p.border(field, 0x475b66, 6.);
                let fixed = (!live.auto_thread)
                    .then(|| {
                        live.recent
                            .all_rows
                            .iter()
                            .chain(&live.recent.rows)
                            .find(|r| r.id == live.fixed_thread)
                    })
                    .flatten();
                let label = fixed.map(|r| r.label.as_str()).or_else(|| {
                    self.current(&live)
                        .filter(|_| !live.auto_thread)
                        .map(|s| s.label.as_str())
                });
                if let Some(label) = label {
                    p.label(
                        &self.catalog.icon(label),
                        rect(23., 79., 26., 28.),
                        18.,
                        TEXT,
                        true,
                    );
                    p.label(
                        self.catalog.strip(label),
                        rect(55., 77., width - 104., 32.),
                        13.,
                        TEXT,
                        false,
                    );
                } else {
                    p.label(
                        "Choose a fixed chat…",
                        rect(27., 77., width - 76., 32.),
                        13.,
                        MUTED,
                        false,
                    );
                }
                p.line(width - 38., 90., width - 33., 95., MUTED, 1.4);
                p.line(width - 33., 95., width - 28., 90., MUTED, 1.4);
                p.label(
                    "Session details",
                    rect(16., 8., width - 80., 28.),
                    14.,
                    TEXT,
                    false,
                );
                let version = concat!("v", env!("CARGO_PKG_VERSION"));
                let version_width = p.width(version, 11.) + 18.;
                let version_x = width - 16. - version_width;
                p.fill(rect(version_x, 12., version_width, 23.), 0x303b43, 6.);
                p.label(
                    version,
                    rect(version_x + 9., 12., version_width - 18., 23.),
                    11.,
                    MUTED,
                    false,
                );
                area(Hit::Follow, rect(16., 42., width - 32., 27.));
                p.border(rect(19., 49., 14., 14.), MUTED, 3.);
                if live.auto_thread {
                    p.line(22., 55., 26., 59., GREEN, 1.5);
                    p.line(26., 59., 31., 51., GREEN, 1.5);
                }
                p.label(
                    "Follow current chat",
                    rect(42., 42., width - 60., 27.),
                    12.,
                    TEXT,
                    false,
                );
                if let Some(s) = self.current(&live) {
                    p.label(
                        &format!(
                            "Session tokens: {}   ·   Output: {}",
                            data::compact(s.tokens()),
                            data::compact(
                                s.usage
                                    .pointer("/total_token_usage/output_tokens")
                                    .and_then(|v| v.as_u64())
                            )
                        ),
                        rect(16., 112., width - 32., 25.),
                        12.,
                        MUTED,
                        false,
                    );
                    p.label(
                        &format!("{} · {}", s.model, s.cwd),
                        rect(16., 139., width - 32., 25.),
                        11.,
                        MUTED,
                        false,
                    );
                } else {
                    p.label(
                        if self.identity(&live).is_empty() {
                            "No chat selected · showing all Busy chats"
                        } else {
                            "Loading chat metrics…"
                        },
                        rect(16., 112., width - 32., 25.),
                        12.,
                        MUTED,
                        false,
                    );
                }
                let quota = if live.quota.is_empty() {
                    format!(
                        "Account limits: {}",
                        if live.quota_error.is_empty() {
                            "connecting…"
                        } else {
                            &live.quota_error
                        }
                    )
                } else {
                    live.quota
                        .iter()
                        .map(|q| format!("{} · {}: {:.0}% left", q.bucket, q.label, q.remaining))
                        .collect::<Vec<_>>()
                        .join("   ")
                };
                p.label(&quota, rect(16., 167., width - 32., 25.), 12., MUTED, false);
                for (hit, x) in [
                    (Hit::Refresh, 16.),
                    (Hit::Dock, 58.),
                    (Hit::Quit, width - 52.),
                ] {
                    if hit == Hit::Refresh {
                        p.fill(rect(x, 202., 36., 32.), 0x2b3a3c, 5.);
                    }
                    area(hit.clone(), rect(x, 202., 36., 32.));
                    p.border(rect(x, 202., 36., 32.), 0x47535b, 5.);
                    p.action_icon(&hit, x + 6., 208.);
                }
                p.label(
                    &live.action_note,
                    rect(16., 239., width - 32., 25.),
                    11.,
                    if live.action_note.starts_with("Action failed") {
                        0xf87171
                    } else {
                        MUTED
                    },
                    false,
                );
                p.label(
                    "Ctrl+Alt+J: recent chats · × hides to tray",
                    rect(16., 270., width - 32., 25.),
                    11.,
                    MUTED,
                    false,
                );
                p.line(16., 305., width - 16., 305., 0x3b484f, 1.);
                p.label(
                    "Extensions",
                    rect(20., 310., width - 40., 25.),
                    12.,
                    MUTED,
                    false,
                );
                let guard_row = rect(16., 340., width - 32., 31.);
                p.fill(guard_row, 0x293238, 6.);
                area(Hit::Guard, guard_row);
                p.label(
                    "Codex Git memory protection",
                    rect(27., 340., width - 100., 31.),
                    12.,
                    TEXT,
                    false,
                );
                p.toggle(width - 60., 348., live.guard.enabled);
                p.label(
                    &format!(
                        "{} · {} processes / {:.0} GiB · 5s",
                        live.guard.note,
                        live.guard.max_processes,
                        live.guard.max_private_mib as f64 / 1024.
                    ),
                    rect(20., 373., width - 40., 25.),
                    11.,
                    if live.guard.available { MUTED } else { ORANGE },
                    false,
                );
            }
        }
        hits
    }
    unsafe fn paint(&mut self, hwnd: HWND) {
        let mut paint = PAINTSTRUCT::default();
        let dc = BeginPaint(hwnd, &mut paint);
        let mut bounds = RECT::default();
        let _ = GetClientRect(hwnd, &mut bounds);
        let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.;
        let kind = if hwnd == self.bar {
            None
        } else if self.icon_panel.as_ref().is_some_and(|p| p.window == hwnd) {
            Some(Popup::Icons)
        } else {
            self.popup_kind
        };
        if self.painter.target.BindDC(dc, &bounds).is_ok() {
            self.painter.target.SetDpi(96. * scale, 96. * scale);
            self.painter.target.BeginDraw();
            let hits = self.render(
                kind,
                (bounds.right - bounds.left) as f32 / scale,
                (bounds.bottom - bounds.top) as f32 / scale,
            );
            let _ = self.painter.target.EndDraw(None, None);
            self.sync_tips(hwnd, &hits, scale);
            if hwnd == self.bar {
                self.hits = hits;
            } else if let Some(panel) = self.icon_panel.as_mut().filter(|p| p.window == hwnd) {
                panel.hits = hits;
            } else {
                self.popup_hits = hits;
            }
        }
        let _ = EndPaint(hwnd, &paint);
    }
    unsafe fn sync_tips(&mut self, owner: HWND, hits: &[Area], scale: f32) {
        let entries: Vec<_> = hits
            .iter()
            .map(|a| {
                (
                    RECT {
                        left: (a.r.left * scale).round() as i32,
                        top: (a.r.top * scale).round() as i32,
                        right: (a.r.right * scale).round() as i32,
                        bottom: (a.r.bottom * scale).round() as i32,
                    },
                    wide(&tip_text(&a.hit, &self.catalog)),
                )
            })
            .collect();
        if self
            .tips
            .iter()
            .any(|t| t.owner == owner && t.entries == entries)
        {
            return;
        }
        self.tips.retain(|t| t.owner != owner);
        let Ok(window) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            TOOLTIPS_CLASSW,
            PCWSTR::null(),
            WS_POPUP | WINDOW_STYLE(TTS_ALWAYSTIP | TTS_NOPREFIX),
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            owner,
            None,
            GetModuleHandleW(None).unwrap_or_default(),
            None,
        ) else {
            return;
        };
        let mut tips = Tips {
            owner,
            window,
            entries,
        };
        let _ = SetWindowTheme(window, w!("DarkMode_Explorer"), PCWSTR::null());
        let _ = SendMessageW(window, TTM_SETMAXTIPWIDTH, WPARAM(0), LPARAM(360));
        let _ = SendMessageW(
            window,
            TTM_SETDELAYTIME,
            WPARAM(TTDT_INITIAL as usize),
            LPARAM(400),
        );
        let _ = SendMessageW(
            window,
            TTM_SETDELAYTIME,
            WPARAM(TTDT_AUTOPOP as usize),
            LPARAM(6000),
        );
        let _ = SendMessageW(
            window,
            WM_SETFONT,
            WPARAM(self.edit_font.0 as usize),
            LPARAM(0),
        );
        for (i, (bounds, text)) in tips.entries.iter_mut().enumerate() {
            let info = TTTOOLINFOW {
                // V2 size also works without a common-controls v6 manifest.
                cbSize: std::mem::offset_of!(TTTOOLINFOW, lpReserved) as u32,
                uFlags: TTF_SUBCLASS,
                hwnd: owner,
                uId: i + 1,
                rect: *bounds,
                lpszText: PWSTR(text.as_mut_ptr()),
                ..Default::default()
            };
            let _ = SendMessageW(
                window,
                TTM_ADDTOOLW,
                WPARAM(0),
                LPARAM(&info as *const _ as isize),
            );
        }
        self.tips.push(tips);
    }
    unsafe fn measure_bar(&self) -> i32 {
        let live = self.snapshot();
        let (_, title, busy, tok, speed) = self.bar_text(&live);
        let p = &self.painter;
        let elapsed = self
            .current(&live)
            .and_then(|s| {
                s.turn
                    .started_at
                    .map(|start| data::duration(s.turn.completed_at.unwrap_or(now()) - start))
            })
            .unwrap_or("—".into());
        let mut width = 6.
            + 34.
            + (p.width(&title, 12.) + 4.).clamp(64., 190.)
            + if live.recent.new_result_count() > 0 { 14. } else { 0. }
            + 6.
            + p.width(&busy, 12.)
            + 10.
            + p.width(&elapsed, 11.).max(35.)
            + 4.
            + 22.
            + p.width(&tok, 11.)
            + 10.
            + p.width(&speed, 11.)
            + 10.
            + 58.;
        if live.quota.is_empty() {
            width += p.width("Limit", 11.) + 5. + 42. + p.width("—", 11.) + 9.;
        } else {
            for q in live.quota.iter().take(2) {
                let percent = format!(
                    "{:.0}%{}",
                    q.remaining,
                    if !live.quota_error.is_empty() || now() - live.quota_at > 180. {
                        "*"
                    } else {
                        ""
                    }
                );
                width += p.width(&q.label, 11.) + 5. + 42. + p.width(&percent, 11.) + 9.;
            }
        }
        width.ceil() as i32
    }
    unsafe fn round_window(hwnd: HWND, width: i32, height: i32, radius: i32) {
        let region = CreateRoundRectRgn(0, 0, width + 1, height + 1, radius, radius);
        if SetWindowRgn(hwnd, region, true) == 0 {
            let _ = DeleteObject(region);
        }
    }
    unsafe fn tick(&mut self) {
        if !self.event.is_invalid() && WaitForSingleObject(self.event, 0) == WAIT_OBJECT_0 {
            self.hidden = false;
        }
        let live = self.snapshot();
        let identity = self.identity(&live);
        if identity != self.last_identity {
            self.last_identity = identity;
            if self.popup_kind.is_some() {
                self.close_popup();
            }
        }
        if self.popup_kind == Some(Popup::Recent) {
            self.rows = data::stable_rows(&self.rows, &live.recent.rows);
        }
        if self.popup_kind == Some(Popup::Fixed) && !live.recent.all_rows.is_empty() {
            self.rows = data::stable_rows(&self.rows, &live.recent.all_rows);
        }
        if matches!(self.popup_kind, Some(Popup::Recent | Popup::Fixed))
            && self.chat_search.sync(&self.rows, &self.catalog)
        {
            // A renamed title may change a filter match; keep the visible anchor
            // and selection rather than resetting the user's scroll position.
            let top = self.chat_filters.get(self.scroll).copied();
            let selected = self.chat_filters.get(self.chat_cursor).copied();
            self.update_body_only();
            let next = self.filtered_chats();
            let allowed: HashSet<_> = next.iter().copied().collect();
            let previous: HashSet<_> = self.chat_filters.iter().copied().collect();
            self.chat_filters.retain(|i| allowed.contains(i));
            self.chat_filters
                .extend(next.into_iter().filter(|i| !previous.contains(i)));
            self.scroll = top
                .and_then(|i| self.chat_filters.iter().position(|&v| v == i))
                .unwrap_or(self.scroll)
                .min(self.chat_filters.len().saturating_sub(8));
            self.chat_cursor = selected
                .and_then(|i| self.chat_filters.iter().position(|&v| v == i))
                .unwrap_or(self.chat_cursor)
                .min(self.chat_filters.len().saturating_sub(1));
        }
        if self.index_pending || self.body_pending {
            if let Some(service) = &self.body_service {
                let result = service.status();
                if result.generation == self.body_generation {
                    self.body_state = result;
                    if self.body_state.complete {
                        self.index_pending = false;
                        self.index_due = Instant::now() + std::time::Duration::from_secs(60);
                        if self.body_pending {
                            self.body_pending = false;
                            self.body_hits = self.body_state.hits.clone();
                            self.body_previews = self.body_state.previews.clone();
                            self.update_body_only();
                            self.chat_filters = self.filtered_chats();
                            self.chat_cursor = 0;
                            self.scroll = 0;
                        }
                    }
                }
            }
        }
        // Prewarm independently of the Body switch and panel visibility. A live
        // query takes priority; ordinary title typing never cancels this work.
        if !self.testing
            && !self.index_pending
            && !self.body_pending
            && Instant::now() >= self.index_due
            && !live.recent.rows.is_empty()
        {
            let service = self
                .body_service
                .get_or_insert_with(|| body::Service::new(self.paths.clone(), self.stop.clone()));
            self.body_generation =
                service.submit("", false, live.recent.rows.iter().map(|r| r.id.clone()));
            self.index_pending = true;
            self.body_state = service.status();
        }
        let host = host::bounds(live.host);
        if !self.testing {
            if self.hidden || !host::shown(live.host) {
                let _ = ShowWindow(self.bar, SW_HIDE);
                if self.popup_kind.is_some() {
                    self.close_popup();
                }
                self.host_hidden = !self.hidden;
            } else if let Some(h) = host {
                self.dpi = GetDpiForWindow(HWND(live.host as *mut _)).max(96) as f32 / 96.;
                let width = self.measure_bar();
                let (w, ht) = (
                    (width as f32 * self.dpi).round() as i32,
                    (self.height as f32 * self.dpi).round() as i32,
                );
                let slot = live
                    .slot
                    .filter(|_| live.slot_host == live.host)
                    .unwrap_or_else(|| {
                        host::titlebar_slot(((h[2] - h[0]) as f32 / self.dpi) as i32, &[])
                    });
                let (mut x, mut y);
                if let Some(offset) = self.settings.host_offset {
                    x = h[0] + (offset[0] as f32 * self.dpi) as i32;
                    y = h[1] + (offset[1] as f32 * self.dpi) as i32;
                } else if ((slot[2] - slot[0]) as f32 * self.dpi) as i32 >= w {
                    x = h[0] + ((slot[0] + slot[2]) as f32 * self.dpi) as i32 / 2 - w / 2;
                    y = h[1]
                        + (slot[1] as f32 * self.dpi) as i32
                        + ((slot[3] - slot[1]) as f32 * self.dpi) as i32 / 2
                        - ht / 2;
                } else {
                    x = (h[0] + h[2] - w) / 2;
                    y = h[1] + (64. * self.dpi) as i32;
                }
                let in_header =
                    x < h[2] && x + w > h[0] && y < h[1] + (44. * self.dpi) as i32 && y + ht > h[1];
                let safe = x >= h[0] + (slot[0] as f32 * self.dpi) as i32
                    && x + w <= h[0] + (slot[2] as f32 * self.dpi) as i32
                    && y >= h[1] + (slot[1] as f32 * self.dpi) as i32
                    && y + ht <= h[1] + (slot[3] as f32 * self.dpi) as i32;
                if self.drag.is_none() && in_header && !safe {
                    self.settings.host_offset = None;
                    x = (h[0] + h[2] - w) / 2;
                    y = h[1] + (64. * self.dpi) as i32;
                }
                let monitor =
                    MonitorFromWindow(HWND(live.host as *mut _), MONITOR_DEFAULTTONEAREST);
                let mut info = MONITORINFO {
                    cbSize: size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                if GetMonitorInfoW(monitor, &mut info).as_bool() {
                    x = x
                        .max(info.rcWork.left)
                        .min((info.rcWork.right - w).max(info.rcWork.left));
                    y = y
                        .max(info.rcWork.top)
                        .min((info.rcWork.bottom - ht).max(info.rcWork.top));
                }
                let current = host::bounds(self.bar.0 as isize);
                if self.drag.is_none()
                    && (current != Some([x, y, x + w, y + ht]) || self.width != width)
                {
                    let _ = SetWindowPos(self.bar, HWND_TOPMOST, x, y, w, ht, SWP_NOACTIVATE);
                    Self::round_window(self.bar, w, ht, (14. * self.dpi) as i32);
                }
                self.width = width;
                self.last_host = Some(h);
                let _ = ShowWindow(self.bar, SW_SHOWNOACTIVATE);
                self.host_hidden = false;
                if self.popup_kind.is_some() {
                    self.place_popup();
                    self.place_icon_panel();
                }
            }
        }
        let signature = format!(
            "{:?}|{}|{}|{}|{:?}|{}",
            self.bar_text(&live),
            live.quota_at,
            live.recent.loaded_at,
            live.action_note,
            live.snapshot.as_ref().map(|s| s.turn.status.as_str()),
            self.last_identity
        );
        if signature != self.last_signature || self.last_draw.elapsed().as_millis() >= 500 {
            self.last_signature = signature;
            let _ = InvalidateRect(self.bar, None, false);
            if matches!(
                self.popup_kind,
                Some(Popup::Recent) | Some(Popup::Details) | Some(Popup::Fixed)
            ) {
                let _ = InvalidateRect(self.popup, None, false);
            }
            self.last_draw = Instant::now();
        }
        if self.last_diag.elapsed().as_secs_f64() >= 1. {
            self.diagnostic();
            self.last_diag = Instant::now();
        }
    }
    unsafe fn tray(&self, add: bool) {
        let mut data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.bar,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: TRAY_MESSAGE,
            hIcon: self.tray_icon,
            ..Default::default()
        };
        let tip = wide("Codex Session Bar · Rust");
        data.szTip[..tip.len()].copy_from_slice(&tip);
        let _ = Shell_NotifyIconW(if add { NIM_ADD } else { NIM_DELETE }, &data);
    }
    unsafe fn open_icon_panel(&mut self) -> Result<()> {
        let window = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            CLASS,
            w!("Codex Session Icon"),
            WS_POPUP | WS_CLIPCHILDREN,
            0,
            0,
            384,
            450,
            self.popup,
            None,
            GetModuleHandleW(None)?,
            None,
        )?;
        let controls = (|| -> Result<(HWND, HWND)> {
            let edit = CreateWindowExW(
                WS_EX_CLIENTEDGE,
                w!("EDIT"),
                w!(""),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                14,
                49,
                190,
                29,
                window,
                HMENU(100isize as *mut _),
                GetModuleHandleW(None)?,
                None,
            )?;
            let combo = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("COMBOBOX"),
                w!(""),
                WS_CHILD
                    | WS_VISIBLE
                    | WS_TABSTOP
                    | WINDOW_STYLE(CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0),
                214,
                49,
                155,
                350,
                window,
                HMENU(101isize as *mut _),
                GetModuleHandleW(None)?,
                None,
            )?;
            for group in &self.groups {
                let _ = SendMessageW(
                    combo,
                    CB_ADDSTRING,
                    WPARAM(0),
                    LPARAM(wide(group).as_ptr() as isize),
                );
            }
            let _ = SendMessageW(combo, CB_SETCURSEL, WPARAM(0), LPARAM(0));
            let _ = SendMessageW(
                edit,
                EM_SETCUEBANNER,
                WPARAM(1),
                LPARAM(w!("Search all emoji…").as_ptr() as isize),
            );
            for child in [edit, combo] {
                let _ = SendMessageW(
                    child,
                    WM_SETFONT,
                    WPARAM(self.edit_font.0 as usize),
                    LPARAM(1),
                );
                let _ = SetWindowTheme(child, w!("DarkMode_CFD"), PCWSTR::null());
            }
            Ok((edit, combo))
        })();
        let (edit, combo) = match controls {
            Ok(value) => value,
            Err(error) => {
                let _ = DestroyWindow(window);
                return Err(error);
            }
        };
        self.icon_panel = Some(IconPanel {
            window,
            edit,
            combo,
            search: String::new(),
            category: "默认集合".into(),
            filters: self.catalog.filter("", "默认集合"),
            scroll: 0,
            hits: Vec::new(),
            layout: None,
        });
        self.place_icon_panel();
        if !self.testing {
            let _ = ShowWindow(window, SW_SHOW);
            let _ = SetForegroundWindow(window);
            let _ = SetFocus(edit);
        }
        Ok(())
    }
    unsafe fn place_icon_panel(&mut self) {
        let Some(panel) = self.icon_panel.as_mut() else {
            return;
        };
        if SendMessageW(panel.combo, CB_GETDROPPEDSTATE, WPARAM(0), LPARAM(0)).0 != 0 {
            return;
        }
        let Some(parent) = host::bounds(self.popup.0 as isize) else {
            return;
        };
        let scale = self.dpi;
        let (w, h, gap) = (
            (384. * scale).round() as i32,
            (450. * scale).round() as i32,
            (6. * scale).round() as i32,
        );
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(
            MonitorFromWindow(self.popup, MONITOR_DEFAULTTONEAREST),
            &mut info,
        );
        let bounds = info.rcWork;
        let x = if parent[0] - gap - w >= bounds.left {
            parent[0] - gap - w
        } else if parent[2] + gap + w <= bounds.right {
            parent[2] + gap
        } else {
            (parent[2] - w)
                .max(bounds.left)
                .min((bounds.right - w).max(bounds.left))
        };
        let y = parent[1]
            .max(bounds.top)
            .min((bounds.bottom - h).max(bounds.top));
        let layout = [x, y, w, h];
        if panel.layout == Some(layout) {
            return;
        }
        panel.layout = Some(layout);
        let _ = SetWindowPos(panel.window, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE);
        Self::round_window(panel.window, w, h, (10. * scale) as i32);
        for (child, x, y, cw, ch) in [
            (panel.edit, 14, 49, 190, 29),
            (panel.combo, 214, 49, 155, 350),
        ] {
            let _ = MoveWindow(
                child,
                (x as f32 * scale) as i32,
                (y as f32 * scale) as i32,
                (cw as f32 * scale) as i32,
                (ch as f32 * scale) as i32,
                true,
            );
        }
    }
    unsafe fn close_icon_panel(&mut self) {
        if let Some(panel) = self.icon_panel.take() {
            self.tips.retain(|t| t.owner != panel.window);
            let _ = DestroyWindow(panel.window);
            self.icon_target = None;
            self.hover = None;
            let _ = InvalidateRect(self.popup, None, false);
            if !self.testing {
                let _ = SetFocus(self.edit);
            }
        }
    }
    unsafe fn update_icon_panel_filter(&mut self) {
        let Some(panel) = self.icon_panel.as_mut() else {
            return;
        };
        let mut text = [0u16; 256];
        let len = GetWindowTextW(panel.edit, &mut text);
        panel.search = String::from_utf16_lossy(&text[..len as usize]);
        let index = SendMessageW(panel.combo, CB_GETCURSEL, WPARAM(0), LPARAM(0))
            .0
            .max(0) as usize;
        panel.category = self.groups.get(index).cloned().unwrap_or("默认集合".into());
        panel.filters = self.catalog.filter(&panel.search, &panel.category);
        panel.scroll = 0;
        let _ = InvalidateRect(panel.window, None, false);
    }
    unsafe fn icon_owns(&self, hwnd: HWND) -> bool {
        let Some(panel) = &self.icon_panel else {
            return false;
        };
        if hwnd == panel.window || IsChild(panel.window, hwnd).as_bool() {
            return true;
        }
        let mut info = COMBOBOXINFO {
            cbSize: size_of::<COMBOBOXINFO>() as u32,
            ..Default::default()
        };
        GetComboBoxInfo(panel.combo, &mut info).is_ok()
            && (hwnd == info.hwndList || IsChild(info.hwndList, hwnd).as_bool())
    }
    unsafe fn icon_message(
        &mut self,
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<LRESULT> {
        let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.;
        let (x, y) = (
            (lparam.0 as i16) as f32 / scale,
            ((lparam.0 >> 16) as i16) as f32 / scale,
        );
        match message {
            WM_PAINT => {
                self.paint(hwnd);
            }
            WM_ERASEBKGND => return Some(LRESULT(1)),
            WM_MOUSEMOVE | WM_LBUTTONUP => {
                let hit = self
                    .icon_panel
                    .as_ref()?
                    .hits
                    .iter()
                    .find(|a| a.contains(x, y))
                    .map(|a| a.hit.clone());
                if message == WM_MOUSEMOVE {
                    if self.hover != hit {
                        self.hover = hit;
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                } else if let Some(action) = hit.and_then(|hit| self.hit(hit)) {
                    ACTIONS.with(|sender| {
                        if let Some(sender) = sender.borrow().as_ref() {
                            let _ = sender.send(action);
                        }
                    });
                }
            }
            WM_SETCURSOR => {
                SetCursor(
                    LoadCursorW(
                        None,
                        if self.hover.is_some() {
                            IDC_HAND
                        } else {
                            IDC_ARROW
                        },
                    )
                    .unwrap_or_default(),
                );
                return Some(LRESULT(1));
            }
            WM_MOUSEWHEEL => {
                let panel = self.icon_panel.as_mut()?;
                let max = panel.filters.len().div_ceil(10).saturating_sub(9);
                panel.scroll = if ((wparam.0 >> 16) as i16) < 0 {
                    (panel.scroll + 3).min(max)
                } else {
                    panel.scroll.saturating_sub(3)
                };
                let _ = InvalidateRect(hwnd, None, false);
            }
            WM_COMMAND => {
                let (id, code) = (
                    (wparam.0 & 0xffff) as u32,
                    ((wparam.0 >> 16) & 0xffff) as u32,
                );
                if (id == 100 && code == EN_CHANGE) || (id == 101 && code == CBN_SELCHANGE) {
                    self.update_icon_panel_filter();
                }
            }
            WM_CTLCOLOREDIT | WM_CTLCOLORSTATIC | WM_CTLCOLORLISTBOX => {
                let dc = HDC(wparam.0 as *mut _);
                SetTextColor(dc, COLORREF(0xf2eee8));
                SetBkColor(dc, COLORREF(0x25211c));
                return Some(LRESULT(self.child_brush.0 as isize));
            }
            WM_ACTIVATE if (wparam.0 & 0xffff) == WA_INACTIVE as usize => {
                let target = HWND(lparam.0 as *mut _);
                if !self.icon_owns(target) {
                    if self.owns_popup_window(target) || target == self.bar {
                        self.close_icon_panel();
                    } else {
                        self.close_popup();
                    }
                }
            }
            WM_CLOSE => self.close_icon_panel(),
            _ => return None,
        }
        Some(LRESULT(0))
    }
    unsafe fn close_popup(&mut self) {
        self.close_icon_panel();
        self.tips.retain(|t| t.owner != self.popup);
        if self.body_pending {
            if let Some(service) = &self.body_service {
                service.cancel();
            }
            self.body_generation = 0;
            self.index_pending = false;
            self.index_due = Instant::now();
        }
        self.body_pending = false;
        self.icon_target = None;
        if self.chat_scroll_drag.take().is_some() {
            let _ = ReleaseCapture();
        }
        let _ = ShowWindow(self.popup, SW_HIDE);
        self.popup_kind = None;
        self.hover = None;
    }
    unsafe fn owns_popup_window(&self, hwnd: HWND) -> bool {
        if hwnd.is_invalid() {
            return false;
        }
        if hwnd == self.popup || IsChild(self.popup, hwnd).as_bool() {
            return true;
        }
        if self
            .icon_panel
            .as_ref()
            .is_some_and(|p| hwnd == p.window || IsChild(p.window, hwnd).as_bool())
        {
            return true;
        }
        [
            self.combo,
            self.icon_panel
                .as_ref()
                .map(|p| p.combo)
                .unwrap_or_default(),
        ]
        .iter()
        .any(|&combo| {
            let mut info = COMBOBOXINFO {
                cbSize: size_of::<COMBOBOXINFO>() as u32,
                ..Default::default()
            };
            !combo.is_invalid()
                && GetComboBoxInfo(combo, &mut info).is_ok()
                && (hwnd == info.hwndList || IsChild(info.hwndList, hwnd).as_bool())
        })
    }
    fn choose_fixed_chat(&self, id: &str) -> bool {
        let Some(row) = self.rows.iter().find(|r| r.id == id) else {
            return false;
        };
        let mut live = self.state.lock().unwrap();
        live.fixed_thread = row.id.clone();
        live.auto_thread = false;
        live.snapshot = None;
        true
    }
    unsafe fn popup(&mut self, kind: Popup) -> Result<()> {
        self.close_icon_panel();
        self.tips.retain(|t| t.owner != self.popup);
        if self.popup_kind == Some(kind) && IsWindowVisible(self.popup).as_bool() {
            self.close_popup();
            return Ok(());
        }
        for hwnd in [self.edit, self.combo] {
            if !hwnd.is_invalid() {
                let _ = DestroyWindow(hwnd);
            }
        }
        self.edit = HWND::default();
        self.combo = HWND::default();
        // A newly created child needs one layout even if the panel bounds match.
        self.popup_layout = None;
        self.popup_kind = Some(kind);
        if kind != Popup::Recent && self.body_pending {
            if let Some(service) = &self.body_service {
                service.cancel();
            }
            self.body_pending = false;
            self.index_pending = false;
            self.body_generation = 0;
            self.index_due = Instant::now();
        }
        if kind != Popup::Icons {
            self.icon_target = None;
        }
        self.hover = None;
        let live = self.snapshot();
        self.popup_size = match kind {
            Popup::Recent => {
                self.rows = live.recent.rows.clone();
                [560, 436]
            }
            Popup::Icons => [384, 450],
            Popup::Details => [560, DETAILS_HEIGHT],
            Popup::Fixed => [560, 500],
        };
        if kind == Popup::Icons {
            self.scroll = 0;
            self.search.clear();
            self.category = "默认集合".into();
            self.filters = self.catalog.filter("", "默认集合");
            self.edit = CreateWindowExW(
                WS_EX_CLIENTEDGE,
                w!("EDIT"),
                w!(""),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                14,
                49,
                190,
                29,
                self.popup,
                HMENU(100isize as *mut _),
                GetModuleHandleW(None)?,
                None,
            )?;
            self.combo = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("COMBOBOX"),
                w!(""),
                WS_CHILD
                    | WS_VISIBLE
                    | WS_TABSTOP
                    | WINDOW_STYLE(CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0),
                214,
                49,
                155,
                350,
                self.popup,
                HMENU(101isize as *mut _),
                GetModuleHandleW(None)?,
                None,
            )?;
            for group in &self.groups {
                let _ = SendMessageW(
                    self.combo,
                    CB_ADDSTRING,
                    WPARAM(0),
                    LPARAM(wide(group).as_ptr() as isize),
                );
            }
            let _ = SendMessageW(self.combo, CB_SETCURSEL, WPARAM(0), LPARAM(0));
            let _ = SendMessageW(
                self.edit,
                EM_SETCUEBANNER,
                WPARAM(0),
                LPARAM(w!("Search all emoji…").as_ptr() as isize),
            );
        }
        if matches!(kind, Popup::Recent | Popup::Fixed) {
            if kind == Popup::Fixed {
                self.rows = if !live.recent.all_rows.is_empty() {
                    live.recent.all_rows.clone()
                } else if self.testing {
                    self.rows.clone()
                } else {
                    data::stable_rows(
                        &data::Store::new(&self.paths).sessions().unwrap_or_default(),
                        &live.recent.rows,
                    )
                };
                self.rows.sort_by_key(|r| r.state != "Busy");
            }
            self.search.clear();
            self.scroll = 0;
            self.filter_fixed();
            if kind == Popup::Fixed && !live.auto_thread {
                self.chat_cursor = self
                    .chat_filters
                    .iter()
                    .position(|&i| self.rows[i].id == live.fixed_thread)
                    .unwrap_or(0);
                self.scroll = self.chat_cursor.saturating_sub(7);
            }
            self.edit = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("EDIT"),
                w!(""),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                27,
                if kind == Popup::Recent { 13 } else { 55 },
                if kind == Popup::Recent { 218 } else { 506 },
                24,
                self.popup,
                HMENU(103isize as *mut _),
                GetModuleHandleW(None)?,
                None,
            )?;
            let _ = SendMessageW(
                self.edit,
                EM_SETCUEBANNER,
                WPARAM(1),
                LPARAM(w!("Search chats / pinyin…").as_ptr() as isize),
            );
        }
        let previous_font = self.edit_font;
        self.tips.clear();
        self.edit_font = CreateFontW(
            (-14.0 * self.dpi).round() as i32,
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            DEFAULT_PITCH.0 as u32,
            w!("Microsoft YaHei UI"),
        );
        for child in [self.edit, self.combo] {
            if !child.is_invalid() {
                let _ = SendMessageW(
                    child,
                    WM_SETFONT,
                    WPARAM(self.edit_font.0 as usize),
                    LPARAM(1),
                );
                let _ = SetWindowTheme(child, w!("DarkMode_CFD"), PCWSTR::null());
            }
        }
        let _ = DeleteObject(previous_font);
        self.place_popup();
        let _ = InvalidateRect(self.popup, None, false);
        if !self.testing {
            let _ = ShowWindow(self.popup, SW_SHOW);
            let _ = SetForegroundWindow(self.popup);
            if matches!(kind, Popup::Icons | Popup::Fixed | Popup::Recent) {
                let _ = SetFocus(self.edit);
            }
        }
        Ok(())
    }
    unsafe fn place_popup(&mut self) {
        // Token/TPS text can change the bar's width and anchor while the user
        // is choosing. Let native combo tracking finish before moving its panel.
        if [
            self.combo,
            self.icon_panel
                .as_ref()
                .map(|p| p.combo)
                .unwrap_or_default(),
        ]
        .iter()
        .any(|&combo| {
            !combo.is_invalid()
                && SendMessageW(combo, CB_GETDROPPEDSTATE, WPARAM(0), LPARAM(0)).0 != 0
        }) {
            return;
        }
        let Some(bar) = host::bounds(self.bar.0 as isize) else {
            return;
        };
        let scale = self.dpi;
        let (w, h) = (
            (self.popup_size[0] as f32 * scale).round() as i32,
            (self.popup_size[1] as f32 * scale).round() as i32,
        );
        let monitor = MonitorFromWindow(self.bar, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(monitor, &mut info);
        let bounds = info.rcWork;
        let x = bar[0]
            .max(bounds.left)
            .min((bounds.right - w).max(bounds.left));
        let y = if bar[1] - h - 6 >= bounds.top {
            bar[1] - h - 6
        } else {
            (bar[3] + 6).min((bounds.bottom - h).max(bounds.top))
        };
        let layout = ([x, y, w, h], scale.to_bits());
        // Moving/resizing a native combo box closes its open drop-down, even
        // when the requested bounds are unchanged. Timer refreshes must not
        // disturb an interaction already in progress.
        if self.popup_layout == Some(layout) {
            return;
        }
        self.popup_layout = Some(layout);
        let _ = SetWindowPos(self.popup, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE);
        Self::round_window(self.popup, w, h, (18. * scale) as i32);
        // Child controls stay in physical pixels, matching the parent DPI.
        for (child, x, y, cw, ch) in [
            if self.popup_kind == Some(Popup::Fixed) {
                (self.edit, 27, 55, 506, 24)
            } else if self.popup_kind == Some(Popup::Recent) {
                (self.edit, 23, 13, 218, 22)
            } else {
                (self.edit, 14, 49, 190, 29)
            },
            (self.combo, 214, 49, 155, 350),
        ] {
            if !child.is_invalid() {
                let _ = MoveWindow(
                    child,
                    (x as f32 * scale) as i32,
                    (y as f32 * scale) as i32,
                    (cw as f32 * scale) as i32,
                    (ch as f32 * scale) as i32,
                    true,
                );
            }
        }
    }
    fn settings(&self) {
        if self.testing {
            return;
        }
        let mut value = self.settings.clone();
        let live = self.snapshot();
        value.auto_thread = live.auto_thread;
        value.thread = live.fixed_thread;
        value.position_version = 3;
        let pending = self.paths.runtime.join("rust-settings.pending");
        if let Ok(b) = serde_json::to_vec_pretty(&value) {
            if std::fs::write(&pending, b).is_ok() {
                let _ = std::fs::rename(pending, self.paths.app.join("settings.json"));
            }
        }
    }
    fn diagnostic(&self) {
        self.paths.write(if self.testing { "rust-test-shortcut-status.json" } else { "shortcut-status.json" },
            &json!({"binding":"Ctrl+Alt+J","registered":self.shortcut_registered,"error":self.shortcut_error}));
        self.paths.write(if self.testing { "rust-test-body-status.json" } else { "body-search-status.json" },
            &json!({"enabled":self.search_body,"pending":self.body_pending,"indexing":self.index_pending,"complete":self.body_state.complete,
                "local_only":true,"progress_percent":self.body_state.progress_percent(),
                "indexed":self.body_state.indexed,"available_local":self.body_state.available,"total_chats":self.body_state.total,
                "bytes_read":self.body_state.bytes_read,"skipped_oversized_messages":self.body_state.skipped,"error":self.body_state.error,
                "cache_exists":self.paths.runtime.join("body-search.sqlite").exists(),"on_demand":false}));
        let live = self.snapshot();
        let current = self.current(&live);
        let (_, title, status, _, _) = unsafe { self.bar_text(&live) };
        self.paths.write(if self.testing{"rust-test-status.json"}else{"live-status.json"},&json!({"pid":std::process::id(),"runtime":"rust","updated_at":now(),"thread":self.identity(&live),"overview":self.identity(&live).is_empty() && data::visible_chat_title(&live.view_label).is_none(),"display_title":title,"display_status":status,"tokens":current.and_then(|s|s.tokens()),"auto_thread":live.auto_thread,"topmost":true,"user_hidden":self.hidden,"host_hidden":self.host_hidden,"host_window":live.host,"host_rect":self.last_host,"host_offset":self.settings.host_offset,"titlebar_slot":live.slot,"quota":live.quota,"quota_error":live.quota_error,"recent_cache":{"rows":live.recent.rows.len(),"busy_count":live.recent.busy_count,"stale":live.recent.stale,"error":live.recent.error},"active_view":{"thread":live.view,"label":live.view_label,"available":live.view.is_some() || data::visible_chat_title(&live.view_label).is_some(),"identity_pending":live.auto_thread && live.view.is_none() && data::visible_chat_title(&live.view_label).is_some(),"source":"uia-current-page"},"action_note":live.action_note,"codex_git_guard":live.guard,"font":{"chinese":self.painter.chinese,"latin":"Segoe UI","emoji":"Segoe UI Emoji","renderer":"DirectWrite / software Direct2D","antialias":"ClearType","per_monitor_dpi":true}}));
    }
    unsafe fn hit(&mut self, hit: Hit) -> Option<Action> {
        match hit {
            Hit::Title | Hit::NewResults(_) => {
                let _ = self.popup(Popup::Recent);
            }
            Hit::Icon => {
                self.close_icon_panel();
                let live = self.snapshot();
                if let Some(current) = self.current(&live) {
                    self.icon_target = Some((current.id.clone(), current.label.clone()));
                    let _ = self.popup(Popup::Icons);
                }
            }
            Hit::ChatIcon(id) => {
                if let Some(row) = self.rows.iter().find(|r| r.id == id) {
                    let target = (row.id.clone(), row.label.clone());
                    self.close_icon_panel();
                    self.icon_target = Some(target);
                    let _ = self.open_icon_panel();
                }
            }
            Hit::Details => {
                let _ = self.popup(Popup::Details);
            }
            Hit::ChooseFixed => {
                let _ = self.popup(Popup::Fixed);
            }
            Hit::Back => {
                let _ = self.popup(Popup::Details);
            }
            Hit::Fixed(id) => {
                if self.choose_fixed_chat(&id) {
                    self.settings();
                    self.last_identity = id;
                    let _ = self.popup(Popup::Details);
                }
            }
            Hit::Hide => {
                self.hidden = true;
                self.close_popup();
                let _ = ShowWindow(self.bar, SW_HIDE);
                self.diagnostic();
            }
            Hit::Recent(id) => {
                let mut live = self.state.lock().unwrap();
                live.auto_thread = true;
                live.action_pending = true;
                drop(live);
                self.settings();
                self.close_popup();
                return Some(Action::Navigate(id));
            }
            Hit::Random | Hit::Auto | Hit::Clear | Hit::Emoji(_) => {
                let live = self.snapshot();
                let Some((id, label)) = self
                    .icon_target
                    .clone()
                    .or_else(|| self.current(&live).map(|s| (s.id.clone(), s.label.clone())))
                else {
                    return None;
                };
                let icon = match hit {
                    Hit::Clear => String::new(),
                    Hit::Auto => self.catalog.suggest(&label),
                    Hit::Emoji(index) => self.catalog.entries[index].emoji.clone(),
                    _ => {
                        let current_icon = self.catalog.icon(&label);
                        self.catalog
                            .entries
                            .iter()
                            .filter(|e| e.emoji != current_icon)
                            .choose(&mut rand::thread_rng())
                            .map(|e| e.emoji.clone())
                            .unwrap_or_default()
                    }
                };
                self.state.lock().unwrap().action_pending = true;
                if self.icon_panel.is_some() {
                    self.close_icon_panel();
                } else {
                    self.close_popup();
                }
                return Some(Action::Icon(id, icon));
            }
            Hit::Refresh => {
                self.state.lock().unwrap().quota_at = -1.;
                return Some(Action::Refresh);
            }
            Hit::Dock => {
                self.settings.host_offset = None;
                self.settings();
                self.tick();
            }
            Hit::Follow => {
                let mut live = self.state.lock().unwrap();
                live.auto_thread = !live.auto_thread;
                if !live.auto_thread {
                    live.fixed_thread = live
                        .view
                        .clone()
                        .unwrap_or_else(|| live.fixed_thread.clone());
                }
                live.snapshot = None;
                drop(live);
                self.settings();
                let _ = InvalidateRect(self.popup, None, false);
            }
            Hit::Guard => {
                let enabled = !self.snapshot().guard.enabled;
                let result = if self.testing {
                    Ok(())
                } else {
                    guard::set(&self.paths, enabled)
                };
                let mut live = self.state.lock().unwrap();
                match result {
                    Ok(()) => {
                        live.guard.enabled = enabled;
                        live.guard.available = false;
                        live.guard.note = "Applying change…".into();
                    }
                    Err(error) => {
                        live.guard.available = false;
                        live.guard.note = format!("Cannot change protection: {error}");
                    }
                }
                let _ = InvalidateRect(self.popup, None, false);
            }
            Hit::Body => {
                self.search_body = !self.search_body;
                self.filter_fixed();
                let _ = InvalidateRect(self.popup, None, false);
                let _ = SetFocus(self.edit);
            }
            Hit::Quit => {
                self.quit = true;
                self.stop.store(true, Ordering::Relaxed);
                PostQuitMessage(0);
            }
            Hit::Grip => {}
        }
        None
    }
    unsafe fn recent_shortcut(&mut self) {
        self.hidden = false;
        if !self.testing {
            let host = HWND(self.snapshot().host as *mut _);
            if !host.is_invalid() && IsWindow(host).as_bool() {
                if IsIconic(host).as_bool() {
                    let _ = ShowWindow(host, SW_RESTORE);
                }
                let _ = SetForegroundWindow(host);
                self.tick();
            }
        }
        if self.popup_kind == Some(Popup::Recent) {
            self.close_popup();
        } else {
            let _ = self.hit(Hit::Title);
        }
    }
    unsafe fn update_filter(&mut self) {
        let mut text = [0u16; 256];
        let len = GetWindowTextW(self.edit, &mut text);
        self.search = String::from_utf16_lossy(&text[..len as usize]);
        let index = SendMessageW(self.combo, CB_GETCURSEL, WPARAM(0), LPARAM(0))
            .0
            .max(0) as usize;
        self.category = self.groups.get(index).cloned().unwrap_or("默认集合".into());
        self.filters = self.catalog.filter(&self.search, &self.category);
        self.scroll = 0;
        let _ = InvalidateRect(self.popup, None, false);
    }
    fn filter_fixed(&mut self) {
        self.chat_search.sync(&self.rows, &self.catalog);
        if self.body_pending {
            if let Some(service) = &self.body_service {
                service.cancel();
            }
            self.body_generation = 0;
            self.index_pending = false;
            self.index_due = Instant::now();
        }
        self.body_hits.clear();
        self.body_previews.clear();
        self.body_only.clear();
        self.body_pending = false;
        if self.search_body
            && self.popup_kind == Some(Popup::Recent)
            && !self.search.trim().is_empty()
            && !self.testing
        {
            let service = self
                .body_service
                .get_or_insert_with(|| body::Service::new(self.paths.clone(), self.stop.clone()));
            self.body_generation =
                service.submit(&self.search, false, self.rows.iter().map(|r| r.id.clone()));
            self.body_pending = true;
            self.index_pending = true;
            self.body_state = service.status();
        }
        self.chat_filters = self.filtered_chats();
        self.chat_cursor = 0;
        self.scroll = 0;
    }
    fn filtered_chats(&self) -> Vec<usize> {
        if self.body_pending {
            return Vec::new();
        }
        let titles = self.chat_search.filter(&self.rows, &self.search, false);
        ranked_matches(
            &self.rows,
            &titles,
            &self.body_hits,
            !self.search.trim().is_empty(),
        )
    }
    fn update_body_only(&mut self) {
        let titles: HashSet<_> = self
            .chat_search
            .filter(&self.rows, &self.search, false)
            .into_iter()
            .map(|i| self.rows[i].id.clone())
            .collect();
        self.body_only = self.body_hits.difference(&titles).cloned().collect();
    }
    fn move_fixed_cursor(&mut self, delta: isize) {
        self.chat_cursor = self
            .chat_cursor
            .saturating_add_signed(delta)
            .min(self.chat_filters.len().saturating_sub(1));
        if self.chat_cursor < self.scroll {
            self.scroll = self.chat_cursor;
        }
        if self.chat_cursor >= self.scroll + 8 {
            self.scroll = self.chat_cursor - 7;
        }
    }
    fn fixed_hit_at(&self, x: f32, y: f32) -> Option<Hit> {
        let width = self.popup_size[0] as f32;
        if self.popup_kind == Some(Popup::Recent) {
            if (264. ..324.).contains(&x) && (8. ..40.).contains(&y) {
                return Some(Hit::Body);
            }
            if (332. ..364.).contains(&x) && (8. ..40.).contains(&y) {
                return Some(Hit::Refresh);
            }
            if !(12. ..width - 18.).contains(&x)
                || !(48. ..392.).contains(&y)
                || (y - 48.) % 43. >= 39.
            {
                return None;
            }
            let slot = self.scroll + ((y - 48.) / 43.) as usize;
            return self.chat_filters.get(slot).map(|&index| {
                if (18. ..48.).contains(&x) {
                    Hit::ChatIcon(self.rows[index].id.clone())
                } else {
                    Hit::Recent(self.rows[index].id.clone())
                }
            });
        }
        if (width - 74. ..width - 16.).contains(&x) && (9. ..39.).contains(&y) {
            return Some(Hit::Back);
        }
        if !(16. ..width - 16.).contains(&x)
            || !(100. ..452.).contains(&y)
            || (y - 100.) % 44. >= 40.
        {
            return None;
        }
        let slot = self.scroll + ((y - 100.) / 44.) as usize;
        self.chat_filters.get(slot).map(|&index| {
            if (23. ..56.).contains(&x) {
                Hit::ChatIcon(self.rows[index].id.clone())
            } else {
                Hit::Fixed(self.rows[index].id.clone())
            }
        })
    }
    unsafe fn fixed_key(&mut self, key: u32) -> bool {
        if self.icon_panel.is_some() {
            return false;
        }
        if !matches!(self.popup_kind, Some(Popup::Fixed | Popup::Recent)) {
            return false;
        }
        // Let the Chinese IME use Enter/arrows to select and commit candidates.
        let ime = ImmGetContext(self.edit);
        if !ime.is_invalid() {
            let composing = ImmGetCompositionStringW(ime, GCS_COMPSTR, None, 0) > 0;
            let _ = ImmReleaseContext(self.edit, ime);
            if composing {
                return false;
            }
        }
        match key {
            k if k == VK_UP.0 as u32 => self.move_fixed_cursor(-1),
            k if k == VK_DOWN.0 as u32 => self.move_fixed_cursor(1),
            k if k == VK_PRIOR.0 as u32 => self.move_fixed_cursor(-8),
            k if k == VK_NEXT.0 as u32 => self.move_fixed_cursor(8),
            k if k == VK_RETURN.0 as u32 => {
                if let Some(&index) = self.chat_filters.get(self.chat_cursor) {
                    let hit = if self.popup_kind == Some(Popup::Recent) {
                        Hit::Recent(self.rows[index].id.clone())
                    } else {
                        Hit::Fixed(self.rows[index].id.clone())
                    };
                    if let Some(action) = self.hit(hit) {
                        ACTIONS.with(|sender| {
                            if let Some(sender) = sender.borrow().as_ref() {
                                let _ = sender.send(action);
                            }
                        });
                    }
                }
            }
            k if k == VK_ESCAPE.0 as u32 => {
                if self.popup_kind == Some(Popup::Recent) {
                    self.close_popup();
                } else {
                    let _ = self.popup(Popup::Details);
                }
            }
            _ => return false,
        }
        let _ = InvalidateRect(self.popup, None, false);
        true
    }
    fn chat_scroll_geometry(&self) -> Option<(f32, f32, f32)> {
        if self.chat_filters.len() <= 8 {
            return None;
        }
        let (top, height) = match self.popup_kind {
            Some(Popup::Recent) => (50., 337.),
            Some(Popup::Fixed) => (100., 350.),
            _ => return None,
        };
        Some((
            top,
            height,
            (height * 8. / self.chat_filters.len() as f32).max(20.),
        ))
    }
    unsafe fn begin_chat_scroll(&mut self, x: f32, y: f32) -> bool {
        let Some((top, height, thumb)) = self.chat_scroll_geometry() else {
            return false;
        };
        let width = self.popup_size[0] as f32;
        if !(width - 17. ..width - 4.).contains(&x) || !(top..top + height).contains(&y) {
            return false;
        }
        let offset = (height - thumb) * self.scroll as f32 / (self.chat_filters.len() - 8) as f32;
        self.chat_scroll_drag = Some(if (top + offset..top + offset + thumb).contains(&y) {
            y - top - offset
        } else {
            thumb / 2.
        });
        let _ = SetCapture(self.popup);
        self.drag_chat_scroll(y);
        true
    }
    unsafe fn drag_chat_scroll(&mut self, y: f32) {
        if let (Some(grab), Some((top, height, thumb))) =
            (self.chat_scroll_drag, self.chat_scroll_geometry())
        {
            self.scroll = (((y - top - grab) / (height - thumb)).clamp(0., 1.)
                * (self.chat_filters.len() - 8) as f32)
                .round() as usize;
            self.chat_cursor = self.chat_cursor.clamp(self.scroll, self.scroll + 7);
            let _ = InvalidateRect(self.popup, None, false);
        }
    }
}

thread_local! {static ACTIONS:RefCell<Option<std::sync::mpsc::Sender<Action>>>=const{RefCell::new(None)};}
unsafe extern "system" fn wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let handled = APP.with(|slot| {
        let Ok(mut state) = slot.try_borrow_mut() else {
            return None;
        };
        let Some(app) = state.as_mut() else {
            return None;
        };
        if app
            .icon_panel
            .as_ref()
            .is_some_and(|panel| panel.window == hwnd)
        {
            return app.icon_message(hwnd, message, wparam, lparam);
        }
        let is_bar = hwnd == app.bar;
        let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.;
        let x = (lparam.0 as i16) as f32 / scale;
        let y = ((lparam.0 >> 16) as i16) as f32 / scale;
        match message {
            WM_HOTKEY if is_bar && wparam.0 == RECENT_HOTKEY as usize => {
                app.recent_shortcut();
                Some(LRESULT(0))
            }
            WM_MOUSEACTIVATE if is_bar => Some(LRESULT(MA_NOACTIVATE as isize)),
            WM_SETCURSOR => {
                let cursor = match app.hover {
                    Some(Hit::Grip) => IDC_SIZEALL,
                    Some(_) => IDC_HAND,
                    None => IDC_ARROW,
                };
                SetCursor(LoadCursorW(None, cursor).unwrap_or_default());
                Some(LRESULT(1))
            }
            WM_PAINT => {
                app.paint(hwnd);
                Some(LRESULT(0))
            }
            WM_ERASEBKGND => Some(LRESULT(1)),
            WM_TIMER => {
                app.tick();
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE => {
                if app.chat_scroll_drag.is_some() {
                    app.drag_chat_scroll(y);
                } else if let Some((start, original)) = app.drag {
                    let mut point = POINT::default();
                    let _ = GetCursorPos(&mut point);
                    let _ = SetWindowPos(
                        app.bar,
                        HWND_TOPMOST,
                        original[0] + point.x - start.x,
                        original[1] + point.y - start.y,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                } else {
                    let areas = if is_bar { &app.hits } else { &app.popup_hits };
                    let hover = if !is_bar
                        && matches!(app.popup_kind, Some(Popup::Recent | Popup::Fixed))
                    {
                        app.fixed_hit_at(x, y)
                    } else {
                        areas
                            .iter()
                            .find(|a| a.contains(x, y))
                            .map(|a| a.hit.clone())
                    };
                    if app.hover != hover {
                        app.hover = hover;
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN => {
                if !is_bar && app.begin_chat_scroll(x, y) {
                    return Some(LRESULT(0));
                }
                if is_bar
                    && app
                        .hits
                        .iter()
                        .any(|a| a.hit == Hit::Grip && a.contains(x, y))
                {
                    let mut start = POINT::default();
                    let _ = GetCursorPos(&mut start);
                    if let Some(original) = host::bounds(hwnd.0 as isize) {
                        app.drag = Some((start, original));
                        let _ = SetCapture(hwnd);
                    }
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONUP => {
                if app.chat_scroll_drag.take().is_some() {
                    let _ = ReleaseCapture();
                } else if app.drag.take().is_some() {
                    let _ = ReleaseCapture();
                    if let (Some(current), Some(host)) =
                        (host::bounds(app.bar.0 as isize), app.last_host)
                    {
                        app.settings.host_offset = Some([
                            ((current[0] - host[0]) as f32 / app.dpi).round() as i32,
                            ((current[1] - host[1]) as f32 / app.dpi).round() as i32,
                        ]);
                        app.settings();
                    }
                } else {
                    let areas = if is_bar { &app.hits } else { &app.popup_hits };
                    let hit = if !is_bar
                        && matches!(app.popup_kind, Some(Popup::Fixed | Popup::Recent))
                    {
                        app.fixed_hit_at(x, y)
                    } else {
                        areas
                            .iter()
                            .find(|a| a.contains(x, y))
                            .map(|a| a.hit.clone())
                    };
                    if let Some(hit) = hit {
                        if let Some(action) = app.hit(hit) {
                            ACTIONS.with(|sender| {
                                if let Some(sender) = sender.borrow().as_ref() {
                                    let _ = sender.send(action);
                                }
                            });
                        }
                    }
                }
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL
                if !is_bar && matches!(app.popup_kind, Some(Popup::Fixed | Popup::Recent)) =>
            {
                let delta = ((wparam.0 >> 16) as i16) as i32;
                app.scroll = if delta < 0 {
                    (app.scroll + 3).min(app.chat_filters.len().saturating_sub(8))
                } else {
                    app.scroll.saturating_sub(3)
                };
                app.chat_cursor = app.chat_cursor.clamp(
                    app.scroll,
                    (app.scroll + 7).min(app.chat_filters.len().saturating_sub(1)),
                );
                let _ = InvalidateRect(app.popup, None, false);
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL if !is_bar && app.popup_kind == Some(Popup::Icons) => {
                let delta = ((wparam.0 >> 16) as i16) as i32;
                let max = app.filters.len().div_ceil(10).saturating_sub(9);
                app.scroll = if delta < 0 {
                    (app.scroll + 3).min(max)
                } else {
                    app.scroll.saturating_sub(3)
                };
                let _ = InvalidateRect(app.popup, None, false);
                Some(LRESULT(0))
            }
            WM_CTLCOLOREDIT | WM_CTLCOLORSTATIC | WM_CTLCOLORLISTBOX => {
                let dc = HDC(wparam.0 as *mut _);
                SetTextColor(dc, COLORREF(0xf2eee8));
                SetBkColor(dc, COLORREF(0x25211c));
                Some(LRESULT(app.child_brush.0 as isize))
            }
            WM_COMMAND if !is_bar => {
                let id = (wparam.0 & 0xffff) as u32;
                let code = ((wparam.0 >> 16) & 0xffff) as u32;
                if (id == 100 && code == EN_CHANGE) || (id == 101 && code == CBN_SELCHANGE) {
                    app.update_filter();
                } else if id == 103 && code == EN_CHANGE {
                    let mut text = [0u16; 256];
                    let len = GetWindowTextW(app.edit, &mut text);
                    app.search = String::from_utf16_lossy(&text[..len as usize]);
                    app.filter_fixed();
                    let _ = InvalidateRect(app.popup, None, false);
                }
                Some(LRESULT(0))
            }
            WM_ACTIVATE if !is_bar && (wparam.0 & 0xffff) == WA_INACTIVE as usize => {
                // Native drop-down lists are separate windows owned by their
                // combo, so activating one is still interaction in this panel.
                if !app.owns_popup_window(HWND(lparam.0 as *mut _)) {
                    app.close_popup();
                }
                Some(LRESULT(0))
            }
            WM_CLOSE if is_bar => {
                let _ = app.hit(Hit::Hide);
                Some(LRESULT(0))
            }
            WM_CLOSE => {
                app.close_popup();
                Some(LRESULT(0))
            }
            QUIT_MESSAGE => {
                let _ = app.hit(Hit::Quit);
                Some(LRESULT(0))
            }
            HIDE_MESSAGE => {
                let _ = app.hit(Hit::Hide);
                Some(LRESULT(0))
            }
            TRAY_MESSAGE => {
                let event = (lparam.0 & 0xffff) as u32;
                if event == WM_LBUTTONUP || event == WM_LBUTTONDBLCLK {
                    app.hidden = false;
                    app.tick();
                } else if event == WM_RBUTTONUP {
                    if let Ok(menu) = CreatePopupMenu() {
                        let _ = AppendMenuW(menu, MF_STRING, 1, w!("Open session bar"));
                        let _ = AppendMenuW(menu, MF_STRING, 2, w!("Quit session bar"));
                        let mut point = POINT::default();
                        let _ = GetCursorPos(&mut point);
                        let _ = SetForegroundWindow(app.bar);
                        let chosen = TrackPopupMenu(
                            menu,
                            TPM_RETURNCMD | TPM_RIGHTBUTTON,
                            point.x,
                            point.y,
                            0,
                            app.bar,
                            None,
                        );
                        let _ = DestroyMenu(menu);
                        if chosen.0 == 1 {
                            app.hidden = false;
                            app.tick();
                        } else if chosen.0 == 2 {
                            let _ = app.hit(Hit::Quit);
                        }
                    }
                }
                Some(LRESULT(0))
            }
            WM_DPICHANGED => {
                app.dpi = (wparam.0 & 0xffff) as f32 / 96.;
                app.tick();
                Some(LRESULT(0))
            }
            _ => None,
        }
    });
    handled.unwrap_or_else(|| DefWindowProcW(hwnd, message, wparam, lparam))
}

unsafe fn register() -> Result<()> {
    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wndproc),
        hInstance: GetModuleHandleW(None)?.into(),
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        lpszClassName: CLASS,
        ..Default::default()
    };
    anyhow::ensure!(
        RegisterClassW(&class) != 0,
        "Could not register native window class"
    );
    Ok(())
}
pub fn request_quit(_paths: &Paths) -> Result<()> {
    unsafe {
        if let Ok(hwnd) = FindWindowW(CLASS, w!("Codex Session Bar")) {
            let _ = PostMessageW(hwnd, QUIT_MESSAGE, WPARAM(0), LPARAM(0));
        }
    }
    Ok(())
}
pub fn request_hide(_paths: &Paths) -> Result<()> {
    unsafe {
        if let Ok(hwnd) = FindWindowW(CLASS, w!("Codex Session Bar")) {
            let _ = PostMessageW(hwnd, HIDE_MESSAGE, WPARAM(0), LPARAM(0));
        }
    }
    Ok(())
}
pub fn run(paths: Paths, settings: Settings, state: State, stop: Stop) -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let name = wide(&format!("Local\\{}-rust-instance", paths.instance));
        let mutex = CreateMutexW(None, false, PCWSTR(name.as_ptr()))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let event = OpenEventW(
                EVENT_MODIFY_STATE,
                false,
                PCWSTR(wide(&format!("Local\\{}-show", paths.instance)).as_ptr()),
            )?;
            SetEvent(event)?;
            let _ = CloseHandle(event);
            let _ = CloseHandle(mutex);
            return Ok(());
        }
        register()?;
        let app = App::new(paths.clone(), settings, state.clone(), stop.clone(), false)?;
        let workers = Workers::start(&paths, &state, &stop, &app.catalog);
        let mut observers = host::start(&paths, &state, &stop);
        observers.push(guard::start(&paths, &state, &stop));
        ACTIONS.with(|a| *a.borrow_mut() = Some(workers.actions.clone()));
        APP.with(|a| *a.borrow_mut() = Some(app));
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            // The search edit owns focus; wheel scrolling still belongs to its list.
            if msg.message == WM_MOUSEWHEEL {
                if let Some(popup) = APP.with(|slot| {
                    slot.try_borrow().ok().and_then(|a| {
                        a.as_ref()
                            .filter(|app| app.owns_popup_window(msg.hwnd))
                            .map(|app| {
                                if app.icon_owns(msg.hwnd) {
                                    app.icon_panel.as_ref().unwrap().window
                                } else {
                                    app.popup
                                }
                            })
                    })
                }) {
                    msg.hwnd = popup;
                }
            }
            if msg.message == WM_KEYDOWN
                && APP.with(|slot| {
                    let Ok(mut value) = slot.try_borrow_mut() else {
                        return false;
                    };
                    value.as_mut().is_some_and(|app| {
                        app.owns_popup_window(msg.hwnd) && app.fixed_key(msg.wParam.0 as u32)
                    })
                })
            {
                continue;
            }
            if msg.message == WM_KEYDOWN && msg.wParam.0 == VK_ESCAPE.0 as usize {
                APP.with(|slot| {
                    if let Ok(mut app) = slot.try_borrow_mut() {
                        if let Some(app) = app.as_mut() {
                            if app.icon_panel.is_some() {
                                app.close_icon_panel();
                            } else {
                                app.close_popup();
                            }
                        }
                    }
                });
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        stop.store(true, Ordering::Relaxed);
        drop(workers);
        for observer in observers {
            let _ = observer.join();
        }
        APP.with(|a| {
            if let Some(app) = a.borrow_mut().take() {
                app.settings();
                app.tray(false);
                if app.shortcut_registered {
                    let _ = UnregisterHotKey(app.bar, RECENT_HOTKEY);
                }
                let _ = DestroyWindow(app.popup);
                let _ = DestroyWindow(app.bar);
                let _ = DeleteObject(app.edit_font);
                let _ = DeleteObject(app.child_brush);
                let _ = DestroyIcon(app.tray_icon);
                let _ = CloseHandle(app.event);
            }
        });
        let _ = CloseHandle(mutex);
        Ok(())
    }
}

unsafe fn capture(
    app: &App,
    kind: Option<Popup>,
    width: u32,
    height: u32,
    path: &std::path::Path,
) -> Result<Vec<Area>> {
    let dc = CreateCompatibleDC(None);
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
    let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
    let previous = SelectObject(dc, bitmap);
    app.painter.target.BindDC(
        dc,
        &RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        },
    )?;
    app.painter.target.SetDpi(96., 96.);
    app.painter.target.BeginDraw();
    let hits = app.render(kind, width as f32, height as f32);
    app.painter.target.EndDraw(None, None)?;
    let raw = std::slice::from_raw_parts(bits as *const u8, (width * height * 4) as usize);
    let mut rgba = Vec::with_capacity(raw.len());
    for pixel in raw.chunks_exact(4) {
        rgba.extend([pixel[2], pixel[1], pixel[0], 255]);
    }
    image::save_buffer(path, &rgba, width, height, image::ColorType::Rgba8)?;
    SelectObject(dc, previous);
    let _ = DeleteObject(bitmap);
    let _ = DeleteDC(dc);
    Ok(hits)
}
pub fn self_test(paths: &Paths) -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        register()?;
        let state = Arc::new(Mutex::new(crate::Shared {
            auto_thread: true,
            ..Default::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let mut app = App::new(
            paths.clone(),
            Settings {
                auto_thread: true,
                ..Default::default()
            },
            state.clone(),
            stop,
            true,
        )?;
        let mut icon_info = ICONINFO::default();
        GetIconInfo(app.tray_icon, &mut icon_info)?;
        assert!(!icon_info.hbmColor.is_invalid());
        let _ = DeleteObject(icon_info.hbmColor);
        let _ = DeleteObject(icon_info.hbmMask);
        let pixels = tray_pixels(48);
        let preview = image::RgbaImage::from_fn(48, 48, |x, y| {
            let pixel = pixels[(y * 48 + x) as usize];
            let a = pixel >> 24;
            image::Rgba([
                (((pixel >> 16) & 255) + 35 * (255 - a) / 255) as u8,
                (((pixel >> 8) & 255) + 39 * (255 - a) / 255) as u8,
                ((pixel & 255) + 43 * (255 - a) / 255) as u8,
                255,
            ])
        });
        preview.save(paths.runtime.join("rust-tray-icon.png"))?;
        let quota = data::Quota {
            bucket: "codex".into(),
            label: "7d".into(),
            remaining: 54.,
            minutes: Some(10080.),
            reset: Some(now() + 350000.),
        };
        state.lock().unwrap().quota = vec![quota];
        state.lock().unwrap().quota_at = now();
        let rows: Vec<Row> = (0..40)
            .map(|i| Row {
                id: format!("fixture-{i}"),
                label: format!(
                    "{} xxx 会话名称 {}",
                    ["🧪", "🚧", "🚀", "💬", "🎯", "📦", "💻", "🧠", "🌈", "📷"][i % 10],
                    i + 1
                ),
                state: if i < 3 { "Busy".into() } else { "Done".into() },
                is_unread: Some(i == 3 || i == 5),
                tokens: Some(27190000),
                tps: Some(42.3),
                rate_at: Some(now()),
                started_at: Some(now() - 754.),
                total_seconds: Some(8365. + i as f64 * 113.),
                updated_at: now() - i as f64 * 60.,
                ..Default::default()
            })
            .collect();
        state.lock().unwrap().recent = data::Recent {
            all_rows: rows.clone(),
            rows: rows.clone(),
            busy_count: Some(3),
            busy_metrics: rows[..3].to_vec(),
            loaded_at: now(),
            ..Default::default()
        };
        assert_eq!(app.bar_text(&app.snapshot()).1, "Recent chats");
        assert!(app.current(&app.snapshot()).is_none());
        {
            let mut live = state.lock().unwrap();
            live.view_label = "📷 xxx 新会话".into();
            // A visible page can precede its database identity or usage snapshot.
            let text = app.bar_text(&live);
            assert_eq!(text.0, "📷");
            assert_eq!(text.1, "xxx 新会话");
            assert_eq!(text.3, "—");
            assert_eq!(text.4, "— t/s");
            live.view = Some("not-yet-loaded".into());
            assert_eq!(app.bar_text(&live).1, "xxx 新会话");
            live.view = None;
            live.view_label = "New chat".into();
            assert_eq!(app.bar_text(&live).1, "New chat");
            live.view_label = "Codex".into();
            assert_eq!(app.bar_text(&live).1, "Recent chats");
            assert_ne!(app.bar_text(&live).3, "—");
            live.view_label.clear();
        }
        assert_eq!(app.catalog.defaults.len(), 90);
        assert!(app.catalog.entries.len() > 3900);
        let avocado = app
            .catalog
            .entries
            .iter()
            .position(|e| e.emoji == "🥑")
            .unwrap();
        assert!(app.catalog.filter("avocado", "默认集合").contains(&avocado));
        assert!(app
            .catalog
            .filter("avocado", "Smileys & Emotion")
            .contains(&avocado));
        assert_eq!(app.catalog.filter("", "默认集合").len(), 90);
        assert_eq!(app.catalog.suggest("🚧 条码智能相机纯RTL"), "📷");
        assert_eq!(app.catalog.suggest("codex session bar"), "🎛️");
        assert_eq!(app.catalog.suggest("系统问题修复。"), "🛠️");
        assert_eq!(app.catalog.suggest("调研无人机 AI 模块"), "🚁");
        app.popup(Popup::Recent)?;
        let tip_areas: Vec<_> = [Hit::Title, Hit::Body, Hit::Refresh, Hit::Auto]
            .into_iter()
            .enumerate()
            .map(|(i, hit)| Area {
                hit,
                r: rect(i as f32 * 40., 8., 32., 32.),
            })
            .collect();
        app.sync_tips(app.popup, &tip_areas, 1.);
        let tip_window = app
            .tips
            .iter()
            .find(|t| t.owner == app.popup)
            .unwrap()
            .window;
        assert!(SendMessageW(tip_window, TTM_GETTOOLCOUNT, WPARAM(0), LPARAM(0)).0 > 3);
        for _ in 0..100 {
            app.sync_tips(app.popup, &tip_areas, 1.);
        }
        assert_eq!(
            app.tips
                .iter()
                .find(|t| t.owner == app.popup)
                .unwrap()
                .window,
            tip_window
        );
        assert!(!app.search_body);
        assert!(app.body_service.is_none());
        assert_eq!(app.fixed_hit_at(290., 25.), Some(Hit::Body));
        let _ = app.hit(Hit::Body);
        assert!(app.search_body);
        assert!(app.body_service.is_none());
        let _ = app.hit(Hit::Body);
        assert!(!app.search_body);
        assert_eq!(app.rows.len(), 40);
        assert_eq!(app.popup_size, [560, 436]);
        app.search = "huihuamingcheng 25".into();
        app.filter_fixed();
        assert_eq!(app.chat_filters, [24]);
        app.search.clear();
        app.filter_fixed();
        app.move_fixed_cursor(24);
        assert_eq!(app.scroll, 17);
        assert_eq!(
            app.fixed_hit_at(80., 381.),
            Some(Hit::Recent(rows[24].id.clone()))
        );
        let original = app.rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>();
        let mut changed = rows.clone();
        changed.reverse();
        app.rows = data::stable_rows(&app.rows, &changed);
        assert_eq!(
            app.rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
            original
        );
        state.lock().unwrap().recent.rows.reverse();
        for _ in 0..100 {
            app.tick();
        }
        assert_eq!(app.popup_kind, Some(Popup::Recent));
        assert_eq!(app.scroll, 17);
        assert_eq!(
            app.rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
            original
        );
        assert!(app.begin_chat_scroll(550., 382.));
        app.drag_chat_scroll(1000.);
        assert_eq!(app.scroll, 32);
        app.chat_scroll_drag = None;
        let _ = ReleaseCapture();
        app.search = "mingcheng 25".into();
        app.filter_fixed();
        let (actions, received) = std::sync::mpsc::channel();
        ACTIONS.with(|a| *a.borrow_mut() = Some(actions));
        assert!(app.fixed_key(VK_RETURN.0 as u32));
        assert!(matches!(received.try_recv().unwrap(), Action::Navigate(id) if id == rows[24].id));
        ACTIONS.with(|a| *a.borrow_mut() = None);
        state.lock().unwrap().recent.rows = rows.clone();
        app.close_popup();
        let snap = data::Snapshot {
            id: "fixture-0".into(),
            label: "🧪 xxx 会话名称 · 中文清晰显示".into(),
            tokens_used: Some(27190000),
            model: "gpt-6.1-sol".into(),
            cwd: "C:\\Projects\\demo".into(),
            turn: data::Turn {
                status: "inProgress".into(),
                started_at: Some(now() - 754.),
                completed_at: None,
            },
            output_rate: Some(data::Rate {
                value: 42.3,
                start: now() - 20.,
                end: now() - 10.,
                output_tokens: 423,
            }),
            ..Default::default()
        };
        {
            let mut live = state.lock().unwrap();
            live.view = Some(snap.id.clone());
            live.snapshot = Some(snap);
        }
        let w = app.measure_bar() as u32;
        let bar_hits = capture(
            &app,
            None,
            w,
            34,
            &paths.runtime.join("rust-bar-verification.png"),
        )?;
        assert!(bar_hits.iter().any(|area| area.hit == Hit::NewResults(2)));
        app.popup_kind = Some(Popup::Recent);
        app.rows = rows.clone();
        app.search.clear();
        app.filter_fixed();
        capture(
            &app,
            Some(Popup::Recent),
            560,
            436,
            &paths.runtime.join("rust-recent-verification.png"),
        )?;
        assert_eq!(
            app.fixed_hit_at(30., 81.),
            Some(Hit::ChatIcon(rows[0].id.clone()))
        );
        assert_eq!(
            app.fixed_hit_at(80., 81.),
            Some(Hit::Recent(rows[0].id.clone()))
        );
        let identity_before_icon = app.identity(&app.snapshot());
        let follow_before_icon = app.snapshot().auto_thread;
        app.last_identity = identity_before_icon.clone();
        app.search = "mingcheng".into();
        app.filter_fixed();
        app.move_fixed_cursor(12);
        let list_before_icon = (
            app.popup,
            app.edit,
            app.search.clone(),
            app.scroll,
            app.chat_cursor,
            app.chat_filters.clone(),
        );
        assert!(app.hit(Hit::ChatIcon(rows[7].id.clone())).is_none());
        assert_eq!(app.popup_kind, Some(Popup::Recent));
        assert!(app.icon_panel.is_some());
        assert_eq!(app.identity(&app.snapshot()), identity_before_icon);
        assert_eq!(app.snapshot().auto_thread, follow_before_icon);
        assert_eq!(app.icon_target.as_ref().unwrap().0, rows[7].id);
        let panel_hits = capture(
            &app,
            Some(Popup::Icons),
            384,
            450,
            &paths.runtime.join("rust-chat-icon-verification.png"),
        )?;
        app.icon_panel.as_mut().unwrap().hits = panel_hits;
        let chosen = app.icon_panel.as_ref().unwrap().filters[0];
        let panel_window = app.icon_panel.as_ref().unwrap().window;
        let panel_combo = app.icon_panel.as_ref().unwrap().combo;
        assert!(app.owns_popup_window(panel_window));
        assert!(app.icon_owns(app.icon_panel.as_ref().unwrap().edit));
        let _ = SendMessageW(panel_combo, CB_SHOWDROPDOWN, WPARAM(1), LPARAM(0));
        for _ in 0..100 {
            app.tick();
            app.place_icon_panel();
        }
        assert_ne!(
            SendMessageW(panel_combo, CB_GETDROPPEDSTATE, WPARAM(0), LPARAM(0)).0,
            0
        );
        let _ = SendMessageW(panel_combo, CB_SHOWDROPDOWN, WPARAM(0), LPARAM(0));
        let chosen_area = app
            .icon_panel
            .as_ref()
            .unwrap()
            .hits
            .iter()
            .find(|a| a.hit == Hit::Emoji(chosen))
            .unwrap()
            .r;
        let scale = GetDpiForWindow(panel_window).max(96) as f32 / 96.;
        let (px, py) = (
            ((chosen_area.left + 8.) * scale) as u16,
            ((chosen_area.top + 8.) * scale) as u16,
        );
        let (sender, receiver) = std::sync::mpsc::channel();
        ACTIONS.with(|actions| *actions.borrow_mut() = Some(sender));
        app.icon_message(
            panel_window,
            WM_LBUTTONUP,
            WPARAM(0),
            LPARAM((px as u32 | ((py as u32) << 16)) as isize),
        );
        assert!(
            matches!(receiver.try_recv().unwrap(), Action::Icon(id, icon) if id == rows[7].id && icon == app.catalog.entries[chosen].emoji)
        );
        ACTIONS.with(|actions| *actions.borrow_mut() = None);
        assert!(app.icon_panel.is_none());
        assert_eq!(app.popup_kind, Some(Popup::Recent));
        assert_eq!(
            (
                app.popup,
                app.edit,
                app.search.clone(),
                app.scroll,
                app.chat_cursor,
                app.chat_filters.clone()
            ),
            list_before_icon
        );
        assert!(app.hit(Hit::ChatIcon(rows[7].id.clone())).is_none());
        assert!(
            matches!(app.hit(Hit::Random), Some(Action::Icon(id, icon)) if id == rows[7].id && icon != app.catalog.icon(&rows[7].label))
        );
        assert_eq!(app.popup_kind, Some(Popup::Recent));
        app.search.clear();
        assert!(app.hit(Hit::ChatIcon(rows[7].id.clone())).is_none());
        let expected_icon = app.catalog.suggest(&rows[7].label);
        assert!(
            matches!(app.hit(Hit::Auto), Some(Action::Icon(id, icon)) if id == rows[7].id && icon == expected_icon)
        );
        assert_eq!(app.popup_kind, Some(Popup::Recent));
        app.filter_fixed();
        app.index_pending = true;
        app.body_state = body::Status {
            indexed: 17,
            available: 40,
            total: 40,
            bytes_read: 12 * 1048576,
            ..Default::default()
        };
        assert!(!app.search_body);
        capture(
            &app,
            Some(Popup::Recent),
            560,
            436,
            &paths.runtime.join("rust-background-index-verification.png"),
        )?;
        app.index_pending = false;
        app.body_state = body::Status::default();
        app.search = "mingcheng 2".into();
        app.filter_fixed();
        capture(
            &app,
            Some(Popup::Recent),
            560,
            436,
            &paths.runtime.join("rust-recent-search-verification.png"),
        )?;
        app.search_body = true;
        app.body_hits = [rows[0].id.clone(), rows[39].id.clone()]
            .into_iter()
            .collect();
        app.body_previews = [
            (
                rows[0].id.clone(),
                "支持 Mingcheng 2 匹配，并在会话名称下方高亮正文。".into(),
            ),
            (
                rows[39].id.clone(),
                "讨论过 mingcheng 2 的正文索引和相机采集。".into(),
            ),
        ]
        .into_iter()
        .collect();
        app.body_state = body::Status {
            complete: true,
            available: 40,
            total: 40,
            indexed: 40,
            ..body::Status::default()
        };
        app.update_body_only();
        app.chat_filters = app.filtered_chats();
        assert_eq!(app.chat_filters[0], 1); // Busy + title match.
        assert_eq!(app.chat_filters[1], 0); // Busy + body match precedes idle title matches.
        assert_eq!(*app.chat_filters.last().unwrap(), 39);
        capture(
            &app,
            Some(Popup::Recent),
            560,
            436,
            &paths.runtime.join("rust-recent-body-verification.png"),
        )?;
        app.paths.write("rust-body-ui-test.json",&json!({"default_off":true,"background_prewarm":true,"ios_switches":true,"busy_title_body_ranking":true,"body_match_badge":true}));
        app.search_body = false;
        app.search.clear();
        app.filter_fixed();
        app.popup_kind = Some(Popup::Icons);
        capture(
            &app,
            Some(Popup::Icons),
            384,
            450,
            &paths.runtime.join("rust-icons-verification.png"),
        )?;
        app.rows = rows.clone();
        app.popup(Popup::Details)?;
        {
            let mut live = state.lock().unwrap();
            live.guard.available = true;
            live.guard.note = "0 Git · 0 MiB · blocked 0".into();
        }
        assert!(app.snapshot().guard.enabled);
        assert!(app.hit(Hit::Guard).is_none());
        assert!(!app.snapshot().guard.enabled);
        assert!(app.hit(Hit::Guard).is_none());
        assert!(app.snapshot().guard.enabled);
        {
            let mut live = state.lock().unwrap();
            live.guard.available = true;
            live.guard.note = "0 Git · 0 MiB · blocked 0".into();
        }
        assert!(capture(
            &app,
            Some(Popup::Details),
            560,
            DETAILS_HEIGHT as u32,
            &paths.runtime.join("rust-details-verification.png")
        )?
        .iter()
        .any(|a| a.hit == Hit::ChooseFixed));
        let before = app.snapshot();
        app.last_identity = app.identity(&before);
        app.popup(Popup::Fixed)?;
        let order = app.rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>();
        state.lock().unwrap().recent.all_rows.reverse();
        for _ in 0..100 {
            app.tick();
        }
        assert_eq!(app.popup_kind, Some(Popup::Fixed));
        assert_eq!(
            app.rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
            order
        );
        app.search = "名称 2".into();
        app.filter_fixed();
        assert_eq!(app.chat_filters.len(), 13);
        assert_eq!(app.rows[app.chat_filters[0]].id, rows[1].id);
        assert_eq!(
            app.fixed_hit_at(80., 120.),
            Some(Hit::Fixed(rows[1].id.clone()))
        );
        app.search.clear();
        app.filter_fixed();
        app.move_fixed_cursor(9);
        assert_eq!(app.chat_cursor, 9);
        assert_eq!(app.scroll, 2);
        assert!(!app.choose_fixed_chat("missing-chat"));
        app.move_fixed_cursor(-8);
        assert!(app.fixed_key(VK_RETURN.0 as u32));
        assert_eq!(app.snapshot().fixed_thread, rows[1].id);
        assert!(!app.snapshot().auto_thread);
        assert_eq!(app.popup_kind, Some(Popup::Details));
        state.lock().unwrap().recent.all_rows = rows.clone();
        app.popup(Popup::Fixed)?;
        let picker_hits = capture(
            &app,
            Some(Popup::Fixed),
            560,
            500,
            &paths.runtime.join("rust-fixed-verification.png"),
        )?;
        assert!(picker_hits
            .iter()
            .any(|a| a.hit == Hit::Fixed(rows[1].id.clone())));
        assert_eq!(
            app.fixed_hit_at(35., 252.),
            Some(Hit::ChatIcon(rows[3].id.clone()))
        );
        let pinned_before_icon = app.snapshot().fixed_thread;
        assert!(app.hit(Hit::ChatIcon(rows[3].id.clone())).is_none());
        assert_eq!(app.popup_kind, Some(Popup::Fixed));
        assert!(app.icon_panel.is_some());
        assert_eq!(app.snapshot().fixed_thread, pinned_before_icon);
        assert!(!app.snapshot().auto_thread);
        assert!(
            matches!(app.hit(Hit::Clear), Some(Action::Icon(id, icon)) if id == rows[3].id && icon.is_empty())
        );
        assert_eq!(app.popup_kind, Some(Popup::Fixed));
        assert!(app.icon_panel.is_none());
        app.popup(Popup::Details)?;
        capture(
            &app,
            Some(Popup::Details),
            560,
            DETAILS_HEIGHT as u32,
            &paths.runtime.join("rust-details-fixed-verification.png"),
        )?;
        *state.lock().unwrap() = before;
        let _ = app.hit(Hit::Icon);
        assert_eq!(app.icon_target.as_ref().unwrap().0, "fixture-0");
        let _ = SendMessageW(app.combo, CB_SHOWDROPDOWN, WPARAM(1), LPARAM(0));
        for _ in 0..100 {
            app.place_popup();
        }
        assert_ne!(
            SendMessageW(app.combo, CB_GETDROPPEDSTATE, WPARAM(0), LPARAM(0)).0,
            0,
            "Timer placement closed the emoji category dropdown"
        );
        let _ = SendMessageW(app.combo, CB_SHOWDROPDOWN, WPARAM(0), LPARAM(0));
        assert_eq!(app.catalog.rename("👩🏽‍💻 中文标题", "🚀"), "🚀 中文标题");
        assert_eq!(app.catalog.filter("rocket", "All emoji").len(), 1);
        let hits = bar_hits;
        let title = hits.iter().find(|a| a.hit == Hit::Title).unwrap();
        let grip = hits.iter().find(|a| a.hit == Hit::Grip).unwrap();
        assert!(title.r.right < grip.r.left);
        app.hits = hits;
        let current = state.lock().unwrap().snapshot.as_ref().unwrap().id.clone();
        match app.hit(Hit::Random) {
            Some(Action::Icon(id, icon)) => {
                assert_eq!(id, current);
                assert_ne!(icon, "🧪");
            }
            _ => anyhow::bail!("Random action did not bind its chat"),
        }
        app.hidden = true;
        app.recent_shortcut();
        assert!(!app.hidden);
        assert_eq!(app.popup_kind, Some(Popup::Recent));
        app.recent_shortcut();
        assert!(app.popup_kind.is_none());
        paths.write("rust-popover-shortcut-test.json", &json!({"list_window_preserved":true,"list_search_scroll_selection_preserved":true,"native_icon_click_routed":true,"icon_combo_survives_100_refreshes":true,"recent_shortcut_toggle":true,"index_percent_local_only":true,"body_snippet_highlight":true}));
        let _ = app.hit(Hit::Hide);
        assert!(app.hidden);
        app.hidden = false;
        app.diagnostic();
        paths.write("rust-ui-test.json",&json!({"ui":"pass","runtime":"rust","emoji_count":app.catalog.entries.len(),"default_collection":90,"recent_rows":40,"recent_search_pinyin":true,"recent_scroll_drag":true,"recent_scroll_survives_100_refreshes":true,"recent_keyboard_navigation":true,"startup_home_quota":true,"open_list_order_fixed":true,"random_icon_bound":true,"drag_grip_separate":true,"fixed_chat_picker_survives_100_refreshes":true,"fixed_chat_search":true,"fixed_chat_keyboard_scroll":true,"fixed_chat_color_icons":true,"emoji_dropdown_survives_100_refreshes":true,"fixed_chat_selection":true,"font_chinese":app.painter.chinese,"renderer":"DirectWrite + software Direct2D"}));
        println!(
            "{}",
            json!({"ui":"pass","emoji":app.catalog.entries.len(),"font":app.painter.chinese,"screenshots":paths.runtime.to_string_lossy()})
        );
        let _ = DestroyWindow(app.popup);
        let _ = DestroyWindow(app.bar);
        let _ = DeleteObject(app.edit_font);
        let _ = DeleteObject(app.child_brush);
        let _ = DestroyIcon(app.tray_icon);
        Ok(())
    }
}
