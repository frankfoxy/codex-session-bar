use crate::{
    body,
    data::{self, Catalog, Row},
    desktop::{Action, Workers},
    guard, host, now, tags,
    search::{ranked_matches, ChatSearch, LabelSearch},
    Paths, Settings, Shortcut, State, Stop,
};
use anyhow::{Context, Result};
use rand::seq::IteratorRandom;
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
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
        System::{LibraryLoader::*, ProcessStatus::*, Threading::*},
        UI::{
            Controls::*, Accessibility::*,
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
const FOCUS_MESSAGE: u32 = WM_APP + 4;
const RECENT_HOTKEY: i32 = 0x5342;
const BG: u32 = 0x23272b;
const PANEL: u32 = 0x242a2e;
const TEXT: u32 = 0xe8eef2;
const MUTED: u32 = 0xa8b8c4;
const ORANGE: u32 = 0xf5a34b;
const MODEL: u32 = 0xffdc91;
const GREEN: u32 = 0x65d69b;
const NEW_RESULT: u32 = 0x70b7ff;
const DETAILS_HEIGHT: i32 = 310;
const PREFERENCES_HEIGHT: i32 = 676;
const RECENT_ROW: i32 = 48;
const LAYOUT_CACHE_LIMIT: usize = 256;
const RECENT_TOP: i32 = 48;
const RECENT_FRAME: i32 = 92;
const SEARCH_CUE_SUBCLASS: usize = 0x5343;
fn search_cue(kind: usize) -> &'static str {
    match (kind,crate::locale::chinese()) {(2,true)=>"搜索标签 / 拼音…",(2,false)=>"Search tags / pinyin…",(_,true)=>"搜索会话 / 拼音…",_=>"Search chats / pinyin…"}
}
unsafe fn paint_search_cue(hwnd: HWND, dc: HDC, kind: usize) {
    if GetWindowTextLengthW(hwnd) != 0 || dc.is_invalid() { return; }
    let saved = SaveDC(dc);
    let font = SendMessageW(hwnd, WM_GETFONT, WPARAM(0), LPARAM(0));
    if font.0 != 0 { SelectObject(dc, HGDIOBJ(font.0 as *mut _)); }
    SetBkMode(dc, TRANSPARENT);
    SetTextColor(dc, COLORREF(0xc4b8a8));
    let mut bounds = RECT::default();
    let _ = GetClientRect(hwnd, &mut bounds);
    bounds.left += 2;
    let mut text: Vec<u16> = search_cue(kind).encode_utf16().collect();
    DrawTextW(dc, &mut text, &mut bounds, DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX);
    let _ = RestoreDC(dc, saved);
}
unsafe extern "system" fn search_cue_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM, id: usize, kind: usize) -> LRESULT {
    let result = DefSubclassProc(hwnd, message, wparam, lparam);
    match message {
        WM_PAINT => {
            let dc = GetDC(hwnd);
            paint_search_cue(hwnd, dc, kind);
            let _ = ReleaseDC(hwnd, dc);
        }
        WM_PRINTCLIENT => paint_search_cue(hwnd, HDC(wparam.0 as *mut _), kind),
        WM_SETTEXT | WM_CHAR | WM_IME_COMPOSITION | WM_PASTE | WM_CUT | WM_CLEAR | WM_UNDO | WM_SETFOCUS | WM_KILLFOCUS | WM_THEMECHANGED => {
            let _ = InvalidateRect(hwnd, None, false);
        }
        WM_NCDESTROY => { let _ = RemoveWindowSubclass(hwnd, Some(search_cue_proc), id); }
        _ => {}
    }
    result
}
unsafe fn install_search_cue(hwnd: HWND, kind: usize) -> Result<()> {
    anyhow::ensure!(SetWindowSubclass(hwnd, Some(search_cue_proc), SEARCH_CUE_SUBCLASS, kind).as_bool(), "Cannot install search placeholder");
    let _ = InvalidateRect(hwnd, None, false);
    Ok(())
}
thread_local! {static APP:RefCell<Option<App>>=const{RefCell::new(None)};}
fn private_bytes()->u64{unsafe{
    let mut counters=PROCESS_MEMORY_COUNTERS_EX::default();counters.cb=size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    if GetProcessMemoryInfo(GetCurrentProcess(),&mut counters as *mut _ as *mut PROCESS_MEMORY_COUNTERS,counters.cb).is_ok(){counters.PrivateUsage as u64}else{0}
}}
fn wide_ui(text:&str)->Vec<u16>{wide(crate::locale::text(text).as_ref())}
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
    Info(String),
    Icon,
    Title,
    NewResults(usize),
    Grip,
    Hide,
    Preferences,
    CheckUpdate,
    ProjectHome,
    UpdateAvailable(String),
    ResetNotice(String),
    ResetNoticeEnabled,
    ResetNoticeInterval(i32),
    CapacityRetryEnabled,
    CapacityRetryInterval(i32),
    CapacityRetryLogs,
    CapacityRetryRefresh,
    CapacityRetryFolder,
    Rows(i32),
    TitleWidth(i32),
    ShortcutEnabled,
    ApplyPreferences,
    GuardSettings,
    GuardLimit(usize,i32),
    RefreshPreferences,
    RefreshBack,
    RefreshInterval(usize, i32),
    ResetPreferences,
    Details,
    Recent(String),
    ChatIcon(String),
    ChatCopy(usize),
    ChatMenuClose,
    ChatMenuBackground,
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
    GuardLogs,
    GuardRefresh,
    GuardFolder,
    BackPreferences,
    Body,
    ClearChatSearch,
    ClearTagSearch,
    ClearEmojiSearch,
    Compact,
    TagSidebar,
    TagFilter(String),
    TagClear,
    TagEdit(String),
    TagOverflow(String),
    TagOverflowClose,
    TagOverflowBackground,
    TagToggle(String),
    TagAdd,
    TagColor(usize),
    TagClose,
    TagBackground,
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
    Preferences,
    GuardLogs,
}

struct Painter {
    target: ID2D1DCRenderTarget,
    text: IDWriteFactory,
    formats: RefCell<HashMap<String, IDWriteTextFormat>>,
    layouts: RefCell<HashMap<(String,u32,u32,u32,bool,bool,u8),(IDWriteTextLayout,u64)>>,
    measurements: RefCell<HashMap<(String,u32),f32>>,
    layout_clock: Cell<u64>,
    brushes: RefCell<HashMap<u32,ID2D1SolidColorBrush>>,
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
            layouts: RefCell::new(HashMap::new()),
            measurements: RefCell::new(HashMap::new()),
            layout_clock: Cell::new(0),
            brushes: RefCell::new(HashMap::new()),
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
    unsafe fn ui_label(&self, text: &str, r: D2D_RECT_F, size: f32, rgb: u32, emoji: bool) {
        self.label(crate::locale::text(text).as_ref(),r,size,rgb,emoji);
    }
    unsafe fn label(&self, text: &str, r: D2D_RECT_F, size: f32, rgb: u32, emoji: bool) {
        let _ = (|| -> Result<()> {
            let layout = self.layout(text,size,emoji,(r.right-r.left).max(1.),(r.bottom-r.top).max(1.),false,0)?;
            let brush = self.brush(rgb)?;
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
            layout.SetWordWrapping(DWRITE_WORD_WRAPPING_CHARACTER)?;
            layout.SetLineSpacing(DWRITE_LINE_SPACING_METHOD_UNIFORM,size+3.,size)?;
            let normal = self.brush(MUTED)?;
            let highlight = self.brush(0xffd27f)?;
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
        let key=(text.to_string(),size.to_bits());
        if let Some(width)=self.measurements.borrow().get(&key){return *width;}
        let width=(|| -> Result<f32> {
            let f=self.format(size,false,text)?;
            let chars:Vec<u16>=text.encode_utf16().collect();
            let layout=self.text.CreateTextLayout(&chars,&f,10000.,40.)?;
            let mut metrics=DWRITE_TEXT_METRICS::default();layout.GetMetrics(&mut metrics)?;
            Ok(metrics.widthIncludingTrailingWhitespace)
        })().unwrap_or(80.);
        let mut measurements=self.measurements.borrow_mut();
        if measurements.len()>=512{measurements.clear();}
        measurements.insert(key,width);width
    }
    fn release_layouts(&self){self.layouts.borrow_mut().clear();self.measurements.borrow_mut().clear();}
    unsafe fn wrapped_label(&self,text:&str,r:D2D_RECT_F,size:f32,rgb:u32){
        if let Ok(layout)=self.layout(text,size,false,(r.right-r.left).max(1.),(r.bottom-r.top).max(1.),true,0){
            if let Ok(brush)=self.brush(rgb){self.target.DrawTextLayout(D2D_POINT_2F{x:r.left,y:r.top},&layout,&brush,D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT|D2D1_DRAW_TEXT_OPTIONS_CLIP);}
        }
    }
    unsafe fn centered_title(&self,text:&str,r:D2D_RECT_F){
        if let Ok(layout)=self.layout(text,13.,false,(r.right-r.left).max(1.),(r.bottom-r.top).max(1.),true,1){
            if let Ok(brush)=self.brush(TEXT){self.target.DrawTextLayout(D2D_POINT_2F{x:r.left,y:r.top},&layout,&brush,D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT|D2D1_DRAW_TEXT_OPTIONS_CLIP);}
        }
    }
    unsafe fn right_label(&self,text:&str,r:D2D_RECT_F,size:f32,rgb:u32){
        if let Ok(layout)=self.layout(text,size,false,(r.right-r.left).max(1.),(r.bottom-r.top).max(1.),false,2){
            if let Ok(brush)=self.brush(rgb){self.target.DrawTextLayout(D2D_POINT_2F{x:r.left,y:r.top},&layout,&brush,D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT|D2D1_DRAW_TEXT_OPTIONS_CLIP);}
        }
    }
    unsafe fn model_label(&self, text: &str, r: D2D_RECT_F, size: f32) {
        if let Ok(layout) = self.layout(text, size, false, (r.right-r.left).max(1.), (r.bottom-r.top).max(1.), false, 0) {
            if let Ok(brush) = self.brush(MODEL) {
                // Monochrome glyphs keep the Fast bolt in the same pale yellow.
                self.target.DrawTextLayout(D2D_POINT_2F { x:r.left, y:r.top }, &layout, &brush, D2D1_DRAW_TEXT_OPTIONS_CLIP);
            }
        }
    }
    unsafe fn fill(&self, r: D2D_RECT_F, rgb: u32, radius: f32) {
        if let Ok(b) = self.brush(rgb) {
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
        if let Ok(b) = self.brush(rgb) {
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
        if let Ok(b) = self.brush(rgb) {
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
        if let Ok(b) = self.brush(rgb) {
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
    unsafe fn status_icon(&self, row: &Row, x: f32, y: f32) {
        if row.state == "Busy" {
            // Only list status cells animate. The title bar keeps its static dot.
            let phase = (now() * 6.) as usize % 8;
            for i in 0..8 {
                let angle = i as f32 * std::f32::consts::TAU / 8. - std::f32::consts::FRAC_PI_2;
                self.circle(x + angle.cos() * 5., y + angle.sin() * 5., 1.1,
                    if (i + 8 - phase) % 8 < 3 { ORANGE } else { 0x70553a });
            }
        } else if row.state == "Done" {
            self.line(x - 4., y, x - 1., y + 3., GREEN, 1.3);
            self.line(x - 1., y + 3., x + 5., y - 4., GREEN, 1.3);
        } else {
            self.label(&row.status_text(), rect(x - 7., y - 12., 18., 24.), 13.,
                if row.state == "Failed" { 0xf87171 } else if row.state == "Waiting" { ORANGE } else { MUTED }, false);
        }
    }
    unsafe fn search_icon(&self, x: f32, y: f32, rgb: u32) {
        if let Ok(brush) = self.brush(rgb) {
            self.target.DrawEllipse(
                &D2D1_ELLIPSE { point: D2D_POINT_2F { x, y }, radiusX: 5.5, radiusY: 5.5 },
                &brush, 1.4, None,
            );
        }
        self.line(x + 4., y + 4., x + 9., y + 9., rgb, 1.4);
    }
    unsafe fn toggle(&self, x: f32, y: f32, on: bool) {
        self.fill(
            rect(x, y, 28., 16.),
            if on { 0x30c777 } else { 0x4b555e },
            8.,
        );
        self.circle(x + if on { 20. } else { 8. }, y + 8., 6., 0xffffff);
    }
    unsafe fn banked_icon(&self,x:f32,y:f32,rgb:u32) {
        self.line(x+1.,y+4.,x+8.,y,rgb,1.1);self.line(x+8.,y,x+15.,y+4.,rgb,1.1);
        self.line(x+1.,y+5.,x+15.,y+5.,rgb,1.1);
        for column in [4.,8.,12.] {self.line(x+column,y+7.,x+column,y+12.,rgb,1.1);}
        self.line(x+2.,y+14.,x+14.,y+14.,rgb,1.1);self.line(x,y+16.,x+16.,y+16.,rgb,1.1);
    }
    unsafe fn notice_bell(&self,x:f32,y:f32,color:u32) {
        let points=[(3.,16.),(5.,13.),(5.,8.),(6.,5.),(8.,3.),(11.,2.),(14.,3.),(16.,5.),(17.,8.),(17.,13.),(19.,16.),(3.,16.)];
        for pair in points.windows(2){self.line(x+pair[0].0,y+pair[0].1,x+pair[1].0,y+pair[1].1,color,1.25);}
        self.line(x+11.,y,x+11.,y+2.,color,1.25);
        self.line(x+9.,y+19.,x+10.,y+20.,color,1.25);self.line(x+10.,y+20.,x+12.,y+20.,color,1.25);self.line(x+12.,y+20.,x+13.,y+19.,color,1.25);
    }
    unsafe fn action_icon(&self, hit: &Hit, x: f32, y: f32) {
        let c = if *hit == Hit::Refresh {
            0xaed8cc
        } else {
            MUTED
        };
        match hit {
            Hit::UpdateAvailable(_) => {
                self.border(rect(x+2.,y+2.,18.,18.),GREEN,9.);
                self.line(x+11.,y+6.,x+11.,y+16.,GREEN,1.5);
                self.line(x+6.5,y+10.5,x+11.,y+6.,GREEN,1.5);
                self.line(x+15.5,y+10.5,x+11.,y+6.,GREEN,1.5);
            }
            Hit::ResetNotice(_) => {
                self.notice_bell(x,y,ORANGE);
            }
            Hit::GuardLogs | Hit::CapacityRetryLogs => {
                self.border(rect(x + 4., y + 2., 15., 19.), c, 2.);
                for dy in [7., 11., 15.] { self.line(x + 8., y + dy, x + 15., y + dy, c, 1.3); }
            }
            Hit::GuardFolder | Hit::CapacityRetryFolder => {
                self.border(rect(x + 2., y + 7., 20., 14.), c, 2.);
                self.line(x + 2., y + 7., x + 2., y + 3., c, 1.4);
                self.line(x + 2., y + 3., x + 10., y + 3., c, 1.4);
                self.line(x + 10., y + 3., x + 14., y + 7., c, 1.4);
            }
            Hit::Preferences => {
                for i in 0..24 {
                    let a = i as f32 * std::f32::consts::TAU / 24.;
                    let b = (i + 1) as f32 * std::f32::consts::TAU / 24.;
                    let radius = |n: i32| if n % 3 == 0 { 9. } else { 7. };
                    self.line(x + 11. + a.cos() * radius(i), y + 11. + a.sin() * radius(i),
                        x + 11. + b.cos() * radius(i + 1), y + 11. + b.sin() * radius(i + 1), c, 1.5);
                }
                self.border(rect(x + 8., y + 8., 6., 6.), c, 3.);
            }
            Hit::ApplyPreferences => {
                self.line(x + 4., y + 11., x + 9., y + 16., GREEN, 1.8);
                self.line(x + 9., y + 16., x + 19., y + 6., GREEN, 1.8);
            }
            Hit::Hide => {
                self.line(x + 11., y + 2., x + 11., y + 13., c, 1.5);
                self.line(x + 7., y + 9., x + 11., y + 13., c, 1.5);
                self.line(x + 15., y + 9., x + 11., y + 13., c, 1.5);
                self.line(x + 3., y + 16., x + 3., y + 20., c, 1.5);
                self.line(x + 3., y + 20., x + 19., y + 20., c, 1.5);
                self.line(x + 19., y + 20., x + 19., y + 16., c, 1.5);
            }
            Hit::Body => {
                self.line(x + 1., y + 3., x + 18., y + 3., c, 1.4);
                self.line(x + 1., y + 8., x + 10., y + 8., c, 1.4);
                self.line(x + 1., y + 13., x + 7., y + 13., c, 1.4);
                self.border(rect(x + 12., y + 9., 7., 7.), TEXT, 3.5);
                self.line(x + 18., y + 15., x + 22., y + 19., TEXT, 1.5);
            }
            Hit::Refresh | Hit::CheckUpdate | Hit::ResetPreferences | Hit::GuardRefresh | Hit::CapacityRetryRefresh => {
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
            Hit::Back | Hit::BackPreferences => {
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
    chat_menu: Option<ChatMenu>,
    chat_copy: Option<std::sync::mpsc::Receiver<std::result::Result<String,String>>>,
    copy_note: String,
    copy_note_until: Instant,
    shortcut_registered: bool,
    shortcut_error: String,
    shortcut_id: i32,
    shortcut_foreground: HWND,
    shortcut_context: bool,
    preferences_draft: Option<Settings>,
    preferences_note: String,
    preferences_advanced: bool,
    preferences_guard: bool,
    guard_limits: guard::Limits,
    update_control: Option<crate::update::Control>,
    notice_control: Option<crate::reset_notice::Control>,
    notice_alert_due: Instant,
    guard_events: Vec<guard::LogEntry>,
    guard_log_error: String,
    capacity_log: bool,
    rows: Vec<Row>,
    recent_rows_at: f64,
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
    custom_tags: tags::Custom,
    tag_filters: HashSet<String>,
    tag_catalog: Vec<(tags::Tag,usize)>,
    tag_regions: Vec<TagRegion>,
    tag_search: LabelSearch,
    tag_query: String,
    tag_search_input: HWND,
    tag_scroll: [f32;3],
    tag_scroll_drag: Option<(usize,f32)>,
    tag_catalog_due: Instant,
    sidebar_open: bool,
    sidebar_animation: Option<SidebarAnimation>,
    tag_editor: Option<String>,
    tag_overflow: Option<String>,
    tag_overflow_y:f32,
    tag_overflow_scroll:f32,
    tag_editor_y: f32,
    tag_editor_scroll: usize,
    tag_input: HWND,
    tag_draft: String,
    tag_color: usize,
    tag_note: String,
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
    list_pointer: Option<(f32,f32)>,
    width: i32,
    height: i32,
    popup_size: [i32; 2],
    popup_layout: Option<([i32; 4], u32)>,
    popup_layout_batch: bool,
    dpi: f32,
    last_draw: Instant,
    last_status_draw: Instant,
    last_diag: Instant,
    last_identity: String,
    ui_language: u16,
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
    anchor: [f32; 3], // Icon right edge, row top and bottom in parent DIP.
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
        Hit::Info(text) | Hit::ResetNotice(text) | Hit::UpdateAvailable(text) => return text.clone(),
        Hit::Icon | Hit::ChatIcon(_) => "Change chat icon",
        Hit::ChatCopy(0) => "Copy a link to this chat",
        Hit::ChatCopy(1) => "Copy the conversation text as Markdown",
        Hit::ChatCopy(_) => "Copy the chat working directory",
        Hit::ChatMenuClose => "Close menu · keep the chat list open",
        Hit::ChatMenuBackground => "Chat copy actions",
        Hit::Title => "Recent chats",
        Hit::NewResults(count) => {
            return format!("{count} newly completed chats · blue dot = unread result · open chat to mark as read")
        }
        Hit::Grip => "Drag session bar",
        Hit::Hide => "Hide to tray · restore from the tray icon or recent-chats shortcut",
        Hit::Preferences => "Settings · list size, shortcut and extensions",
        Hit::CheckUpdate => "Check GitHub for updates now · show an icon when an update is available",
        Hit::ProjectHome=>"Open Codex Session Bar on GitHub",
        Hit::ResetNoticeEnabled => "Follow published Codex reset announcements · selected source only",
        Hit::ResetNoticeInterval(_) => "Announcement check interval · 1–60 minutes · changes apply immediately",
        Hit::CapacityRetryEnabled=>"Automatically append a continuation only after the latest turn fails because the model is at capacity",
        Hit::CapacityRetryInterval(_)=>"Capacity retry delay · 1–300 seconds · default 5 seconds",
        Hit::CapacityRetryLogs=>"View capacity retry log · badge counts actual retry attempts, once per failed turn",
        Hit::CapacityRetryRefresh=>"Refresh recent capacity retry events",
        Hit::CapacityRetryFolder=>"Open retry log folder · includes compressed history",
        Hit::Rows(n) => if *n < 0 { "Show fewer rows" } else { "Show more rows" },
        Hit::ShortcutEnabled => "Enable shortcut · only while Codex or Session Bar has focus",
        Hit::GuardSettings => "Protection limits and check interval",
        Hit::GuardLimit(_,_) => "Adjust protection limit · apply to save",
        Hit::RefreshPreferences => "Refresh settings · source events, rate limits, fallback and diagnostics",
        Hit::RefreshBack => "Back to settings · keep unsaved changes",
        Hit::RefreshInterval(_,delta) => if *delta>0 {"Increase refresh interval · apply to save"} else {"Decrease refresh interval · apply to save"},
        Hit::ApplyPreferences => "Apply and save settings",
        Hit::ResetPreferences => "Restore default list size and shortcut · apply to save",
        Hit::Details => "Session details",
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
        Hit::Guard => "Protect Codex Git · configurable checks · 1s after trigger",
        Hit::GuardLogs => "View protection log · badge counts storm episodes, not processes · new tracking; recovered after 60 stable seconds",
        Hit::GuardRefresh => "Refresh recent protection events",
        Hit::GuardFolder => "Open log folder · includes compressed history",
        Hit::BackPreferences => "Back to settings",
        Hit::Body => "Search message text · off by default",
        Hit::ClearChatSearch => "Clear chat search · keep tag filters",
        Hit::ClearTagSearch => "Clear tag search · keep selected tags",
        Hit::ClearEmojiSearch => "Clear emoji search · keep category",
        Hit::TitleWidth(_) => "Title column maximum · 140–280 DIP · two-line titles and previews",
        Hit::Compact => "Compact view · one line per chat, hide tags and prompt preview",
        Hit::TagSidebar => "Tag filters · expand or collapse · search names or pinyin",
        Hit::TagFilter(_) => "Filter by this tag · selected tags must all match",
        Hit::TagClear => "Clear tag filters",
        Hit::TagOverflow(_) => "Show all tags · scroll for more · keep the chat list open",
        Hit::TagOverflowClose => "Close all tags · keep the chat list open",
        Hit::TagOverflowBackground => "All tags assigned to this chat",
        Hit::TagEdit(_) => "Edit custom tags · project and group tags are read-only",
        Hit::TagToggle(_) => "Add or remove this custom tag from the chat",
        Hit::TagAdd => "Create or attach the tag",
        Hit::TagColor(_) => "Choose custom tag color",
        Hit::TagClose => "Close tag editor · keep the chat list open",
        Hit::TagBackground => "",
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
        mut settings: Settings,
        state: State,
        stop: Stop,
        testing: bool,
    ) -> Result<Self> {
        if !testing {crate::locale::initialize();}
        settings.refresh.clamp();
        state.lock().unwrap().refresh_options=settings.refresh.clone();
        settings.recent_rows = settings.recent_rows.clamp(4, 20);
        settings.recent_title_width=settings.recent_title_width.clamp(140,280);
        if !settings.recent_shortcut.valid() { settings.recent_shortcut = Shortcut::default(); }
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
        let custom_tags = tags::Custom::load(&paths);
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
            chat_menu: None,
            chat_copy: None,
            copy_note: String::new(),
            copy_note_until: Instant::now(),
            shortcut_registered: false,
            shortcut_error: String::new(),
            shortcut_id: RECENT_HOTKEY,
            shortcut_foreground: HWND::default(),
            shortcut_context: false,
            preferences_draft: None,
            preferences_note: String::new(),
            preferences_advanced: false,
            preferences_guard: false,
            guard_limits: guard::Limits::default(),
            update_control: None,
            notice_control: None,
            notice_alert_due:Instant::now(),
            guard_events: Vec::new(),
            guard_log_error: String::new(),
            capacity_log: false,
            popup_layout: None,
            popup_layout_batch: false,
            rows: Vec::new(),
            recent_rows_at: -1.,
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
            custom_tags,
            tag_filters: HashSet::new(),
            tag_catalog: Vec::new(),
            tag_regions: Vec::new(),
            tag_search: LabelSearch::default(),
            tag_query: String::new(),
            tag_search_input: HWND::default(),
            tag_scroll: [0.;3],
            tag_scroll_drag: None,
            tag_catalog_due: Instant::now(),
            sidebar_open: false,
            sidebar_animation: None,
            tag_editor: None,
            tag_overflow:None,tag_overflow_y:100.,tag_overflow_scroll:0.,
            tag_editor_y: 100.,
            tag_editor_scroll: 0,
            tag_input: HWND::default(),
            tag_draft: String::new(),
            tag_color: 0,
            tag_note: String::new(),
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
            list_pointer: None,
            width: 650,
            height: 34,
            popup_size: [640, 460],
            dpi: 1.,
            last_draw: Instant::now(),
            last_status_draw: Instant::now(),
            last_diag: Instant::now(),
            last_identity: String::new(),
            ui_language:crate::locale::status().0,
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
            app.sync_shortcut_scope(true);
            app.tray(true);
            SetTimer(bar, 1, 80, None);
        }
        Ok(app)
    }
    fn snapshot(&self) -> crate::Shared {
        self.state.lock().unwrap().clone()
    }
    unsafe fn register_shortcut(&self, id: i32, shortcut: &Shortcut) -> Result<()> {
        let mut modifiers = MOD_NOREPEAT;
        if shortcut.ctrl { modifiers |= MOD_CONTROL; }
        if shortcut.alt { modifiers |= MOD_ALT; }
        if shortcut.shift { modifiers |= MOD_SHIFT; }
        RegisterHotKey(self.bar, id, modifiers, shortcut.key as u32)?;
        Ok(())
    }
    fn list_page_rows(&self) -> usize {
        if self.popup_kind == Some(Popup::Recent) {
            (((self.popup_size[1] - RECENT_FRAME) / self.recent_row_height()).clamp(1, 20) as usize).min(self.settings.recent_rows.clamp(4,20))
        } else { 8 }
    }
    unsafe fn recent_panel_height(&self) -> i32 {
        let requested = self.settings.recent_rows.clamp(4, 20) as i32;
        let row=self.recent_row_height();
        let minimum=(TAG_CLOUD_TOP+44.+3.*(28.+32.)) as i32;
        if self.testing { return (RECENT_FRAME + requested * row).max(minimum); }
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        let monitor = MonitorFromWindow(self.bar, MONITOR_DEFAULTTONEAREST);
        let available = if GetMonitorInfoW(monitor, &mut info).as_bool() {
            ((info.rcWork.bottom - info.rcWork.top) as f32 / self.dpi) as i32 - 16
        } else { RECENT_FRAME + requested * row };
        (RECENT_FRAME + requested.min(((available - RECENT_FRAME) / row).max(1)) * row).max(minimum).min(available.max(RECENT_FRAME+row))
    }
    fn write_settings(&self, settings: &Settings) -> Result<()> {
        if self.testing { return Ok(()); }
        let mut value = settings.clone();
        let live = self.snapshot();
        value.auto_thread = live.auto_thread;
        value.thread = live.fixed_thread;
        value.position_version = 3;
        let pending = self.paths.runtime.join("rust-settings.pending");
        std::fs::write(&pending, serde_json::to_vec_pretty(&value)?)
            .context("Could not write settings")?;
        std::fs::rename(pending, self.paths.app.join("settings.json"))
            .context("Could not save settings")?;
        Ok(())
    }
    fn set_reset_notice(&mut self,mut config:crate::reset_notice::Settings)->Result<()> {
        config.clamp();
        let mut next=self.settings.clone();next.reset_notice=config.clone();
        self.write_settings(&next)?;
        self.settings=next;
        if let Some(draft)=self.preferences_draft.as_mut(){draft.reset_notice=config.clone();}
        if self.testing {
            let mut live=self.state.lock().unwrap();
            if live.reset_notice.source!=config.source {
                live.reset_notice=Arc::new(crate::reset_notice::View{source:config.source,..Default::default()});
            }
            if !config.enabled {Arc::make_mut(&mut live.reset_notice).current=None;}
        } else if let Some(control)=&self.notice_control {control.configure(config);}
        self.preferences_note="Saved · changes are active".into();
        Ok(())
    }
    fn set_capacity_retry(&mut self,mut config:crate::capacity_retry::Settings)->Result<()> {
        config.clamp();let mut next=self.settings.clone();next.capacity_retry=config.clone();self.write_settings(&next)?;
        self.settings=next;if let Some(draft)=self.preferences_draft.as_mut(){draft.capacity_retry=config.clone();}
        crate::capacity_retry::configure(&self.state,config);self.preferences_note="Saved · changes are active".into();Ok(())
    }
    unsafe fn change_notice_source(&mut self) {
        let index=SendMessageW(self.combo,CB_GETCURSEL,WPARAM(0),LPARAM(0)).0;
        let mut config=self.settings.reset_notice.clone();
        config.source=if index==1{crate::reset_notice::Source::CodexResets}else{crate::reset_notice::Source::Divin};
        if let Err(error)=self.set_reset_notice(config){
            self.preferences_note=format!("Could not save: {error}");
            let previous=usize::from(self.settings.reset_notice.source==crate::reset_notice::Source::CodexResets);
            let _=SendMessageW(self.combo,CB_SETCURSEL,WPARAM(previous),LPARAM(0));
        }
    }
    fn refresh_guard_events(&mut self) {
        if self.testing { return; }
        let result=if self.capacity_log {crate::capacity_retry::recent_events(&self.paths)}else{guard::recent_events(&self.paths)};
        match result {
            Ok(entries) => { self.guard_events = entries; self.guard_log_error.clear(); }
            Err(error) => { self.guard_events.clear();self.guard_log_error = format!("{}: {error}",crate::locale::text(if self.capacity_log{"Cannot read retry log"}else{"Cannot read protection log"})); }
        }
        self.scroll = self.scroll.min(self.guard_events.len().saturating_sub(8));
    }
    unsafe fn apply_preferences(&mut self) -> Result<()> {
        let mut next = self.preferences_draft.clone().context("Settings are not open")?;
        let word = SendMessageW(self.edit, HKM_GETHOTKEY, WPARAM(0), LPARAM(0)).0 as u16;
        next.recent_shortcut = Shortcut::from_control_word(word, next.recent_shortcut.enabled);
        anyhow::ensure!(next.recent_shortcut.valid(), "Use Ctrl or Alt with a letter, number or F1–F24");
        next.refresh.clamp();
        next.recent_rows = next.recent_rows.clamp(4, 20);
        next.recent_title_width=next.recent_title_width.clamp(140,280);
        let changed = next.recent_shortcut != self.settings.recent_shortcut;
        let in_scope = self.shortcut_allowed();
        let new_registration = !self.testing && in_scope && next.recent_shortcut.enabled && (changed || !self.shortcut_registered);
        let next_id = if self.shortcut_id == RECENT_HOTKEY { RECENT_HOTKEY + 1 } else { RECENT_HOTKEY };
        if new_registration {
            self.register_shortcut(next_id, &next.recent_shortcut)
                .context("Shortcut unavailable; the previous shortcut is unchanged")?;
        }
        if let Err(error) = self.write_settings(&next) {
            if new_registration { let _ = UnregisterHotKey(self.bar, next_id); }
            return Err(error);
        }
        if !self.testing && (changed || new_registration) {
            if self.shortcut_registered { let _ = UnregisterHotKey(self.bar, self.shortcut_id); }
            self.shortcut_registered = new_registration;
            if new_registration { self.shortcut_id = next_id; }
            self.shortcut_error.clear();
        }
        { let mut live=self.state.lock().unwrap();live.refresh_options=next.refresh.clone();live.refresh_events.notify(); }
        self.settings = next.clone();
        self.preferences_draft = Some(next);
        self.preferences_note = "Saved · changes are active".into();
        self.diagnostic();
        Ok(())
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
    fn current_row<'a>(&self, live: &'a crate::Shared) -> Option<&'a Row> {
        let rows=if live.recent.all_rows.is_empty(){&live.recent.rows}else{&live.recent.all_rows};
        let id=self.identity(live);
        if !id.is_empty() {return rows.iter().find(|row|row.id==id);}
        if !live.auto_thread {return None;}
        let title=data::visible_chat_title(&live.view_label)?;
        let title=self.catalog.strip(&title).trim();
        let mut matching=rows.iter().filter(|row|self.catalog.strip(&row.label).trim()==title);
        let row=matching.next()?;
        if matching.any(|other|other.id!=row.id){None}else{Some(row)}
    }
    fn bar_usage(&self, live: &crate::Shared)->(Option<u64>,Option<f64>,bool) {
        let current=self.current(live);let row=self.current_row(live);
        if current.is_some() || row.is_some() || !self.identity(live).is_empty()
            || (live.auto_thread && data::visible_chat_title(&live.view_label).is_some()) {
            (current.and_then(|s|s.tokens()).or_else(||row.and_then(|r|r.tokens)),
                current.and_then(|s|s.rate()).or_else(||row.filter(|r|r.state=="Busy").and_then(|r|r.rate())),false)
        } else {live.recent.totals()}
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
                    crate::locale::text("Recent chats").into_owned()
                } else {
                    crate::locale::text("Switching…").into_owned()
                }
            });
        let icon = current
            .map(|s| self.catalog.icon(&s.label))
            .or_else(|| visible.as_ref().map(|s| self.catalog.icon(s)))
            .unwrap_or("◈".into());
        let stale = live.recent.stale || now() - live.recent.loaded_at > self.settings.refresh.fallback_secs as f64 + 10.;
        let busy = live
            .recent
            .busy_count
            .map(|n| {
                format!(
                    "{} {n}{}",
                    if n > 0 { "●" } else { "○" },
                    if stale { "*" } else { "" }
                )
            })
            .unwrap_or("Connecting…".into());
        let (tokens, rate, partial) = self.bar_usage(live);
        let tok = format!(
            "{}{}",
            data::compact(tokens),
            if partial && tokens.is_some() { "*" } else { "" }
        );
        let speed = rate
            .map(|v| format!("{v:.1}{} t/s", if partial { "*" } else { "" }))
            .unwrap_or("— t/s".into());
        (icon, title, crate::locale::text(&busy).into_owned(), tok, speed)
    }
    unsafe fn render(&self, kind: Option<Popup>, width: f32, height: f32) -> Vec<Area> {
        let p = &self.painter;
        let live = self.snapshot();
        let mut hits = Vec::new();
        p.target
            .Clear(Some(&color(if kind.is_none() { BG } else { PANEL })));
        let frame_left=if kind==Some(Popup::Recent)&&!self.sidebar_open{TAG_SIDEBAR}else{0.};
        p.border(
            rect(frame_left+0.5, 0.5, width - frame_left - 1., height - 1.),
            0x47535b,
            if kind.is_none() { 8. } else { 10. },
        );
        let mut area = |hit: Hit, r: D2D_RECT_F| {
            // Informational regions are registered after their foreground is
            // painted. They must never paint an opaque hover background.
            if !matches!(hit, Hit::Info(_) | Hit::ResetNotice(_)) && self.hover.as_ref() == Some(&hit) {
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
                area(Hit::Info(format!("{}: {}{}", crate::locale::text("Busy"), live.recent.busy_count.map(|n|n.to_string()).unwrap_or_else(||"—".into()), if live.recent.stale { if crate::locale::chinese(){" · 数据可能已过期"}else{" · Stale data"} } else {""})), rect(x, 2., sw, 30.));
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
                if let Some(s) = current {
                    let model = s.execution.short();
                    if !model.is_empty() {
                        let w = p.width(&model, 11.) + 2.;
                        p.model_label(&model, rect(x, 2., w, 30.), 11.);
                        area(Hit::Info(s.execution.tooltip()), rect(x, 2., w, 30.));
                        x += w + 8.;
                    }
                }
                let shown: Vec<Option<&data::Quota>> = if live.quota.is_empty() {
                    vec![None]
                } else {
                    live.quota.iter().take(2).map(Some).collect()
                };
                for quota in shown {
                    let quota_left = x;
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
                    let zh = crate::locale::chinese();
                    let tooltip = quota.map(|q| format!("{} · {}\n{} {:.0}% · {} {:.0}%\n{}{}", q.bucket, q.label,
                        if zh { "剩余" } else { "Remaining" }, q.remaining,
                        if zh { "已用" } else { "Used" }, 100. - q.remaining,
                        crate::telemetry::reset_tooltip(if zh { "额度重置" } else { "Quota reset" }, q.reset, now()),
                        if !live.quota_error.is_empty() || now()-live.quota_at>180. { if zh { "\n数据可能已过期 · 正在等待更新" } else { "\nStale data · waiting for update" } } else { "" }))
                        .unwrap_or_else(|| if zh { "额度数据暂不可用".into() } else { "Quota data unavailable".into() });
                    area(Hit::Info(tooltip), rect(quota_left, 2., x - quota_left, 30.));
                    if let Some(reset) = quota.and_then(|q| q.banked_reset).filter(|r| *r > now()) {
                        let text = crate::telemetry::countdown(reset, now());
                        let w = p.width(&text, 11.) + 19.;
                        let color = if reset-now() <= 86400. { ORANGE } else { MUTED };
                        p.banked_icon(x,9.,color);
                        p.label(&text, rect(x+18., 2., w-18., 30.), 11., color, false);
                        area(Hit::Info(quota.map(|q| q.banked_tooltip(now())).unwrap_or_default()), rect(x, 2., w, 30.));
                        x += w + 8.;
                    }
                }
                if self.settings.reset_notice.enabled {
                    let (bell,text)=live.reset_notice.badge(now(),crate::locale::chinese());
                    let color=match bell {crate::reset_notice::Bell::Upcoming=>ORANGE,crate::reset_notice::Bell::Reset=>GREEN,_=>MUTED};
                    let w=if !text.is_empty(){p.width(&text,11.)+30.}else{24.};
                    if bell!=crate::reset_notice::Bell::Neutral {p.fill(rect(x-3.,5.,w+4.,24.),if bell==crate::reset_notice::Bell::Reset {0x254136}else{0x44321f},5.);}
                    area(Hit::ResetNotice(live.reset_notice.tooltip(now(),crate::locale::chinese())),rect(x-3.,2.,w+4.,30.));
                    p.notice_bell(x,6.,color);
                    if !text.is_empty(){p.label(&text,rect(x+25.,2.,w-25.,30.),11.,color,false);}
                    x+=w+9.;
                }
                if live.updater.available() {
                    let hit=Hit::UpdateAvailable(live.updater.badge_tip(crate::locale::chinese()));
                    p.fill(rect(x,5.,25.,24.),0x254136,5.);
                    area(hit.clone(),rect(x,3.,25.,28.));
                    p.action_icon(&hit,x+1.,6.);
                    x+=29.;
                }
                area(Hit::Details, rect(x, 3., 25., 28.));
                p.line(x + 8., 15., x + 12., 19., MUTED, 1.4);
                p.line(x + 12., 19., x + 16., 15., MUTED, 1.4);
                x += 29.;
                area(Hit::Preferences, rect(x, 3., 25., 28.));
                p.action_icon(&Hit::Preferences, x + 2., 6.);
            }
            Some(Popup::Recent) => return self.draw_recent(width, height),
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
                p.ui_label("🎲", rect(17., 9., 30., 32.), 20., TEXT, true);
                area(Hit::Auto, rect(50., 9., 34., 32.));
                p.action_icon(&Hit::Auto, 56., 14.);
                let icon_title = self
                    .icon_target
                    .as_ref()
                    .map(|(_, label)| self.catalog.strip(label))
                    .unwrap_or("Session icon");
                p.label(&if self.icon_target.is_none(){crate::locale::text(icon_title)}else{std::borrow::Cow::Borrowed(icon_title)}, rect(92., 9., 235., 32.), 14., TEXT, false);
                area(Hit::Clear, rect(width - 46., 10., 32., 30.));
                p.action_icon(&Hit::Clear, width - 41., 15.);
                for r in [rect(14., 49., 190., 29.), rect(214., 49., 155., 29.)] {
                    p.fill(r, 0x1c2125, 3.);
                    p.border(r, 0x47535b, 3.);
                }
                p.ui_label(
                    if search.is_empty() {
                        "Search all emoji…"
                    } else {
                        search
                    },
                    rect(21., 49., 148., 29.),
                    13.,
                    MUTED,
                    false,
                );
                if !search.is_empty(){
                    let r=Self::emoji_clear_rect();area(Hit::ClearEmojiSearch,r);
                    p.search_clear(r,self.hover==Some(Hit::ClearEmojiSearch));
                }
                p.ui_label(category, rect(222., 49., 131., 29.), 13., TEXT, false);
                p.ui_label("⌄", rect(350., 49., 16., 29.), 13., MUTED, false);
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
                p.ui_label(
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
                p.ui_label(
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
                    p.ui_label(
                        if self.search.is_empty() {
                            "Search chats…"
                        } else {
                            &self.search
                        },
                        rect(27., 49., width - 86., 35.),
                        13.,
                        MUTED,
                        false,
                    );
                }
                if !self.search.is_empty(){
                    let r=self.chat_clear_rect();area(Hit::ClearChatSearch,r);
                    p.search_clear(r,self.hover==Some(Hit::ClearChatSearch));
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
                    let hovered=self.row_hovered(&row.id);
                    let busy = row.state == "Busy";
                    let r = rect(16., y, width - 32., 40.);
                    p.fill(
                        r,
                        if hovered {
                            if selected{0x354f46}else if busy{0x4a3d2c}else{0x33434d}
                        } else if selected {
                            0x293f37
                        } else if busy {
                            0x352e24
                        } else {
                            0x282f34
                        },
                        7.,
                    );
                    let icon_hit = Hit::ChatIcon(row.id.clone());
                    area(icon_hit.clone(), rect(29., y + 5., 33., 30.));
                    area(Hit::Fixed(row.id.clone()), r);
                    if hovered || selected || self.chat_cursor == self.scroll + slot {
                        p.border(r, if hovered {0x78aac4} else if selected { 0x5ca985 } else { 0x59849c }, 7.);
                    }
                    p.fill(
                        rect(29., y + 5., 30., 30.),
                        if self.hover == Some(icon_hit) {
                            0x496173
                        } else {
                            0x333c43
                        },
                        7.,
                    );
                    p.label(
                        &self.catalog.icon(&row.label),
                        rect(32., y + 4., 28., 32.),
                        20.,
                        TEXT,
                        true,
                    );
                    let with_metrics = row.tokens.is_some() || row.total_seconds.is_some() || !row.execution.model.is_empty();
                    p.label(
                        self.catalog.strip(&row.label),
                        rect(
                            68.,
                            y + 2.,
                            width - 208.,
                            if with_metrics { 21. } else { 36. },
                        ),
                        13.,
                        TEXT,
                        false,
                    );
                    if with_metrics {
                        let model = row.execution.short();
                        let model_width = if model.is_empty() { 0. } else { p.width(&model, 10.) + 10. };
                        if !model.is_empty() { p.model_label(&model, rect(68., y + 22., model_width - 8., 16.), 10.); }
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
                            rect(68. + model_width, y + 22., width - 208. - model_width, 16.),
                            10.,
                            MUTED,
                            false,
                        );
                    }
                    p.status_icon(row, width - 80., y + 20.);
                    if selected {
                        p.ui_label("✓", rect(width - 39., y + 3., 20., 34.), 14., GREEN, false);
                    }
                    if row.newly_completed() {
                        p.circle(20., y + 20., 3., NEW_RESULT);
                    }
                }
                if self.chat_filters.is_empty() {
                    p.ui_label(
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
                p.ui_label(
                    &if !self.copy_note.is_empty(){self.copy_note.clone()}else{format!(
                        "{} chats  ·  Scroll to browse  ·  Enter to select",
                        self.chat_filters.len()
                    )},
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
                    p.ui_label(
                        "Choose a fixed chat…",
                        rect(27., 77., width - 76., 32.),
                        13.,
                        MUTED,
                        false,
                    );
                }
                p.line(width - 38., 90., width - 33., 95., MUTED, 1.4);
                p.line(width - 33., 95., width - 28., 90., MUTED, 1.4);
                p.ui_label(
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
                p.ui_label(
                    "Follow current chat",
                    rect(42., 42., width - 60., 27.),
                    12.,
                    TEXT,
                    false,
                );
                if let Some(s) = self.current(&live) {
                    p.ui_label(
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
                    p.ui_label(
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
                p.ui_label(&quota, rect(16., 167., width - 32., 25.), 12., MUTED, false);
                for (hit, x) in [
                    (Hit::Refresh, 16.),
                    (Hit::Dock, 58.),
                    (Hit::Preferences, 100.),
                    (Hit::Quit, width - 52.),
                ] {
                    if hit == Hit::Refresh {
                        p.fill(rect(x, 202., 36., 32.), 0x2b3a3c, 5.);
                    }
                    area(hit.clone(), rect(x, 202., 36., 32.));
                    p.border(rect(x, 202., 36., 32.), 0x47535b, 5.);
                    p.action_icon(&hit, x + 6., 208.);
                }
                p.ui_label(
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
                p.ui_label(
                    &format!("{}: recent chats · hide from settings or tray", self.settings.recent_shortcut.label()),
                    rect(16., 270., width - 32., 25.),
                    11.,
                    MUTED,
                    false,
                );
            }
            Some(Popup::GuardLogs) => {
                p.ui_label(if self.capacity_log{"Capacity retry log"}else{"Protection log"}, rect(18., 8., width - 130., 30.), 14., TEXT, false);
                for (hit, x) in [(if self.capacity_log{Hit::CapacityRetryRefresh}else{Hit::GuardRefresh}, width - 92.), (Hit::BackPreferences, width - 48.)] {
                    area(hit.clone(), rect(x, 8., 32., 30.));
                    p.action_icon(&hit, x + 5., 12.);
                }
                if self.capacity_log {
                    let view=&live.capacity_retry;let zh=crate::locale::chinese();
                    p.label(&if zh{format!("累计触发 {} 次 · 已提交 {} · 未确认 {}",view.attempts,view.submitted,view.unconfirmed)}else{format!("Retries {} · Submitted {} · Unconfirmed {}",view.attempts,view.submitted,view.unconfirmed)},rect(18.,44.,width-36.,24.),12.,TEXT,false);
                    let since=chrono::DateTime::from_timestamp(view.tracking_since as i64,0).filter(|_|view.tracking_since>0.).map(|time|time.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string());
                    let note=if let Some(since)=since {if zh{format!("自 {since} 起统计 · 同一失败回合只计一次 · 旧版记录不补计")}else{format!("Counting since {since} · once per failed turn · older records excluded")}}
                        else{if zh{"从本版本首次重试开始累计 · 旧版记录仍可查看".into()}else{"Counter starts with the first retry in this version; older logs remain visible".into()}};
                    p.label(&note,rect(18.,72.,width-36.,22.),11.,MUTED,false);
                } else {
                p.ui_label(&format!("Storms {}  ·  {}  ·  Ended {} processes",
                    live.guard.storm_count.map(|v|v.to_string()).unwrap_or_else(|| "—".into()),
                    if !live.guard.available { "Unavailable" } else if !live.guard.enabled { "Protection off" } else if live.guard.storm_active { "Active" } else { "Stable" },
                    live.guard.terminated), rect(18., 44., width - 36., 24.), 12., TEXT, false);
                let since = chrono::DateTime::parse_from_rfc3339(&live.guard.tracking_since).ok()
                    .map(|t| t.format("%m-%d %H:%M").to_string());
                p.ui_label(&since.map(|s|format!("Episode tracking since {s} · earlier alarms are not counted"))
                    .unwrap_or_else(|| "Episode counter is not available yet".into()),
                    rect(18., 72., width - 36., 22.), 11., MUTED, false);
                }
                for (slot, entry) in self.guard_events.iter().skip(self.scroll).take(8).enumerate() {
                    let y = 104. + slot as f32 * 42.;
                    p.fill(rect(14., y, width - 32., 39.), 0x293238, 5.);
                    p.label(&entry.time, rect(22., y + 2., 112., 33.), 10., MUTED, false);
                    p.label(&entry.title, rect(142., y + 1., width - 170., 20.), 12.,
                        if entry.warning { ORANGE } else { TEXT }, false);
                    p.label(&entry.detail, rect(142., y + 20., width - 170., 17.), 10., MUTED, false);
                    area(Hit::Info(format!("{}\n{}\n{}",entry.time,entry.title,entry.detail)),rect(14.,y,width-32.,39.));
                }
                if self.guard_events.is_empty() {
                    p.ui_label(if self.guard_log_error.is_empty() { "No recent events · open the folder for archived logs" } else { &self.guard_log_error },
                        rect(24., 125., width - 48., 45.), 12., MUTED, false);
                }
                if self.guard_events.len() > 8 {
                    let thumb = (333. * 8. / self.guard_events.len() as f32).max(18.);
                    let offset = (333. - thumb) * self.scroll as f32 / (self.guard_events.len() - 8) as f32;
                    p.fill(rect(width - 10., 106., 3., 333.), 0x354148, 1.5);
                    p.fill(rect(width - 10., 106. + offset, 3., thumb), 0x77929f, 1.5);
                }
                p.ui_label(&format!("{} recent events · scroll · latest log tail", self.guard_events.len()), rect(18., height - 39., width - 90., 28.), 11., MUTED, false);
                let folder=if self.capacity_log{Hit::CapacityRetryFolder}else{Hit::GuardFolder};
                area(folder.clone(), rect(width - 54., height - 40., 36., 30.));
                p.action_icon(&folder, width - 48., height - 36.);
            }
            Some(Popup::Preferences) => {
                let draft = self.preferences_draft.as_ref().unwrap_or(&self.settings);
                if self.preferences_guard {return self.draw_guard_preferences(width,height);}
                if self.preferences_advanced {
                    area(Hit::RefreshBack,rect(14.,8.,32.,30.));p.action_icon(&Hit::BackPreferences,18.,13.);
                    p.ui_label("Refresh settings",rect(54.,8.,width-70.,28.),14.,TEXT,false);
                    let watching=self.testing||live.refresh_events.watching.load(Ordering::Relaxed);
                    p.ui_label(if watching{"File-change events · debounced · slow fallback"}else{"File notifications unavailable · fallback active"},rect(20.,45.,width-40.,24.),11.,if watching{GREEN}else{ORANGE},false);
                    for (field,label,step) in [(0,"Current chat minimum",1),(1,"Chat list minimum",1),(2,"Fallback refresh",15),(3,"Cache save interval",15),(4,"Diagnostic status",5),(5,"Window identity check",250)] {
                        let y=78.+field as f32*42.;p.ui_label(label,rect(20.,y,width-180.,30.),12.,TEXT,false);
                        p.fill(rect(width-150.,y,130.,30.),0x1c2125,6.);
                        for(delta,x)in[(-step,width-146.),(step,width-54.)]{
                            area(Hit::RefreshInterval(field,delta),rect(x,y,30.,30.));p.line(x+10.,y+15.,x+20.,y+15.,MUTED,1.5);
                            if delta>0{p.line(x+15.,y+10.,x+15.,y+20.,MUTED,1.5);}
                        }
                        p.ui_label(&draft.refresh.value(field),rect(width-116.,y,62.,30.),11.,TEXT,false);
                    }
                    p.ui_label("Minimums coalesce events; unchanged chats wait for fallback.",rect(20.,338.,width-40.,24.),11.,MUTED,false);
                    p.ui_label("Window following and typing stay responsive.",rect(20.,361.,width-40.,24.),11.,MUTED,false);
                    let button=rect(width-52.,390.,36.,32.);area(Hit::ApplyPreferences,button);p.fill(button,0x293f37,5.);p.action_icon(&Hit::ApplyPreferences,width-46.,396.);
                    p.ui_label(&self.preferences_note,rect(20.,423.,width-40.,25.),11.,if self.preferences_note.starts_with("Saved"){GREEN}else{ORANGE},false);
                    return hits;
                }
                p.ui_label("Settings", rect(16., 8., width - 120., 28.), 14., TEXT, false);
                let home=concat!("Codex Session Bar · v",env!("CARGO_PKG_VERSION"));
                let home_width=p.width(home,11.)+4.;
                let home_left=width-20.-home_width;
                area(Hit::ProjectHome,rect(home_left-4.,8.,home_width+8.,28.));
                p.label(home,rect(home_left,8.,home_width,28.),11.,NEW_RESULT,false);
                if self.hover==Some(Hit::ProjectHome){p.line(home_left,29.,home_left+home_width-4.,29.,NEW_RESULT,0.8);}
                let update=&live.updater;
                let pending=self.update_control.as_ref().is_some_and(|control|control.busy());
                let enabled=!update.busy() && !pending && (self.testing||self.update_control.is_some());
                let button=rect(20.,43.,150.,29.);
                if enabled {area(Hit::CheckUpdate,button);}else{area(Hit::Info(update.tooltip(crate::locale::chinese())),button);}
                p.fill(button,if enabled{0x293f37}else{0x293238},5.);
                p.border(button,if enabled{0x48675b}else{0x3b484f},5.);
                p.action_icon(&Hit::CheckUpdate,26.,46.);
                p.ui_label(if update.phase=="checking" || (pending && !update.busy()) {"Checking…"}else{"Check for updates"},rect(53.,43.,112.,29.),11.,if enabled{TEXT}else{MUTED},false);
                let zh=crate::locale::chinese();
                p.label(&update.time_label(zh),rect(186.,39.,width-206.,18.),11.,MUTED,false);
                p.label(&update.result_label(zh),rect(186.,57.,width-206.,19.),11.,
                    if matches!(update.phase.as_str(),"check_failed"|"error"){ORANGE}else if update.last_result=="up_to_date"{GREEN}else{TEXT},false);
                area(Hit::Info(update.tooltip(zh)),rect(181.,39.,width-201.,37.));
                p.ui_label("Visible rows", rect(20., 78., 260., 30.), 13., TEXT, false);
                p.fill(rect(width - 150., 78., 130., 30.), 0x1c2125, 6.);
                for (delta, x) in [(-1, width - 146.), (1, width - 54.)] {
                    area(Hit::Rows(delta), rect(x, 78., 30., 30.));
                    p.line(x + 10., 93., x + 20., 93., MUTED, 1.5);
                    if delta > 0 { p.line(x + 15., 88., x + 15., 98., MUTED, 1.5); }
                }
                p.label(&draft.recent_rows.to_string(), rect(width - 108., 78., 50., 30.), 13., TEXT, false);
                p.ui_label("Title column width",rect(20.,113.,260.,26.),12.,MUTED,false);
                p.fill(rect(width-150.,113.,130.,26.),0x1c2125,6.);
                for(delta,x)in[(-20,width-146.),(20,width-54.)]{
                    area(Hit::TitleWidth(delta),rect(x,113.,30.,26.));
                    p.line(x+10.,126.,x+20.,126.,MUTED,1.5);
                    if delta>0{p.line(x+15.,121.,x+15.,131.,MUTED,1.5);}
                }
                p.ui_label(&format!("{} px",draft.recent_title_width),rect(width-111.,113.,57.,26.),11.,TEXT,false);
                p.line(20., 145., width - 20., 145., 0x354148, 1.);
                p.ui_label("Global shortcut", rect(20., 156., width - 95., 28.), 13., TEXT, false);
                area(Hit::ShortcutEnabled, rect(width - 72., 155., 52., 30.));
                p.toggle(width - 60., 162., draft.recent_shortcut.enabled);
                p.ui_label("Open recent chats", rect(20., 195., 220., 30.), 12., MUTED, false);
                p.border(rect(247., 193., width - 267., 34.), 0x475b66, 5.);
                if self.testing {
                    p.label(&draft.recent_shortcut.label(), rect(258., 194., width - 289., 32.), 12., TEXT, false);
                }
                p.ui_label("Click the field, then press Ctrl / Alt + a letter, number or F-key", rect(20., 231., width - 40., 24.), 11., MUTED, false);
                p.ui_label(&self.preferences_note, rect(20., 260., width - 40., 30.), 11.,
                    if self.preferences_note.starts_with("Saved") { GREEN } else { ORANGE }, false);
                for (hit, x) in [(Hit::Hide, 16.), (Hit::ResetPreferences, 58.), (Hit::Quit, 100.), (Hit::ApplyPreferences, width - 52.)] {
                    p.fill(rect(x, 300., 36., 32.), if hit == Hit::ApplyPreferences { 0x293f37 } else { 0x293238 }, 5.);
                    area(hit.clone(), rect(x, 300., 36., 32.));
                    p.border(rect(x, 300., 36., 32.), 0x47535b, 5.);
                    p.action_icon(&hit, x + 6., 306.);
                }
                let timing=rect(150.,300.,36.,32.);area(Hit::RefreshPreferences,timing);p.fill(timing,0x293238,5.);p.border(timing,0x47535b,5.);
                p.border(rect(158.,307.,20.,20.),MUTED,10.);p.line(168.,310.,168.,317.,MUTED,1.5);p.line(168.,317.,173.,320.,MUTED,1.5);
                p.line(16., 352., width - 16., 352., 0x3b484f, 1.);
                p.ui_label(
                    "Extensions",
                    rect(20., 358., width - 40., 25.),
                    12.,
                    MUTED,
                    false,
                );
                let guard_row = rect(16., 391., width - 32., 31.);
                p.fill(guard_row, 0x293238, 6.);
                area(Hit::Guard, rect(16., 391., width - 226., 31.));
                area(Hit::Guard, rect(width - 73., 391., 57., 31.));
                p.ui_label(
                    "Codex Git memory protection",
                    rect(27., 391., width - 250., 31.),
                    12.,
                    TEXT,
                    false,
                );
                area(Hit::GuardSettings,rect(width-201.,393.,28.,28.));p.action_icon(&Hit::Preferences,width-198.,396.);
                let badge = rect(width - 162., 395., 40., 23.);
                p.fill(badge, if live.guard.enabled && live.guard.storm_active { 0x62411d } else { 0x314759 }, 10.);
                area(Hit::GuardLogs, badge);
                let badge_text = live.guard.storm_count.map(|v|v.to_string()).unwrap_or_else(|| "—".into());
                let text_width = p.width(&badge_text, 11.).min(32.);
                p.label(&badge_text, rect(width - 142. - text_width / 2., 395., text_width + 1., 23.),
                    11., if live.guard.storm_active { ORANGE } else { NEW_RESULT }, false);
                area(Hit::GuardLogs, rect(width - 113., 391., 32., 31.));
                p.action_icon(&Hit::GuardLogs, width - 108., 396.);
                p.toggle(width - 60., 399., live.guard.enabled);
                p.ui_label(
                    &format!(
                        "{} · {} processes / {:.2} GiB · {}s",
                        live.guard.note,
                        live.guard.max_processes,
                        live.guard.max_private_mib as f64 / 1024.,
                        live.guard.interval_seconds
                    ),
                    rect(20., 426., width - 40., 25.),
                    11.,
                    if live.guard.available { MUTED } else { ORANGE },
                    false,
                );
                p.line(16.,456.,width-16.,456.,0x3b484f,1.);
                p.fill(rect(16.,463.,width-32.,31.),0x293238,6.);
                p.ui_label("Reset announcements",rect(27.,463.,157.,31.),12.,TEXT,false);
                if self.testing {
                    p.fill(rect(190.,465.,288.,27.),0x1c2125,4.);p.border(rect(190.,465.,288.,27.),0x47535b,4.);
                    p.label(self.settings.reset_notice.source.label(),rect(200.,465.,254.,27.),11.,TEXT,false);
                    p.line(458.,476.,463.,481.,MUTED,1.2);p.line(463.,481.,468.,476.,MUTED,1.2);
                }
                area(Hit::ResetNoticeEnabled,rect(width-73.,463.,57.,31.));
                p.toggle(width-60.,471.,self.settings.reset_notice.enabled);
                let api=self.settings.reset_notice.source.api();
                p.label(api,rect(20.,500.,width-180.,27.),10.,MUTED,false);
                area(Hit::Info(format!("{}\n{}\n{}",self.settings.reset_notice.source.label(),api,live.reset_notice.tooltip(now(),crate::locale::chinese()))),rect(20.,498.,width-185.,29.));
                p.fill(rect(width-151.,500.,131.,27.),0x1c2125,5.);
                for(delta,left)in[(-1,width-148.),(1,width-49.)] {
                    area(Hit::ResetNoticeInterval(delta),rect(left,500.,27.,27.));
                    p.line(left+9.,513.,left+18.,513.,MUTED,1.3);
                    if delta>0 {p.line(left+13.5,508.5,left+13.5,517.5,MUTED,1.3);}
                }
                p.label(&format!("{} min",self.settings.reset_notice.check_minutes),rect(width-117.,500.,67.,27.),11.,TEXT,false);
                let check=if self.settings.reset_notice.enabled {live.reset_notice.check_status(crate::locale::chinese())}
                    else {crate::locale::text("Reset announcement checks are disabled").into_owned()};
                p.label(&check,rect(20.,534.,width-40.,23.),10.,if live.reset_notice.error.is_empty(){MUTED}else{ORANGE},false);
                area(Hit::Info(format!("{}\n{}",live.reset_notice.tooltip(now(),crate::locale::chinese()),self.paths.runtime.join("reset-notice-checks.jsonl").display())),rect(18.,532.,width-36.,27.));
                p.line(16.,566.,width-16.,566.,0x3b484f,1.);
                p.fill(rect(16.,575.,width-32.,31.),0x293238,6.);
                area(Hit::CapacityRetryEnabled,rect(16.,575.,width-226.,31.));
                area(Hit::CapacityRetryEnabled,rect(width-73.,575.,57.,31.));
                p.ui_label("Capacity auto-retry",rect(27.,575.,width-250.,31.),12.,TEXT,false);
                let badge=rect(width-162.,579.,40.,23.);
                p.fill(badge,if live.capacity_retry.in_flight{0x62411d}else{0x314759},10.);
                area(Hit::CapacityRetryLogs,badge);
                let count=live.capacity_retry.attempts.to_string();
                let count_width=p.width(&count,11.).min(32.);
                p.label(&count,rect(width-142.-count_width/2.,579.,count_width+1.,23.),11.,if live.capacity_retry.in_flight{ORANGE}else{NEW_RESULT},false);
                area(Hit::CapacityRetryLogs,rect(width-113.,575.,32.,31.));
                p.action_icon(&Hit::CapacityRetryLogs,width-108.,580.);
                p.toggle(width-60.,583.,self.settings.capacity_retry.enabled);
                p.ui_label("Append a continuation after a capacity error",rect(20.,612.,width-180.,27.),10.,MUTED,false);
                p.fill(rect(width-151.,612.,131.,27.),0x1c2125,5.);
                for(delta,left)in[(-1,width-148.),(1,width-49.)]{
                    area(Hit::CapacityRetryInterval(delta),rect(left,612.,27.,27.));
                    p.line(left+9.,625.,left+18.,625.,MUTED,1.3);
                    if delta>0 {p.line(left+13.5,620.5,left+13.5,629.5,MUTED,1.3);}
                }
                p.label(&format!("{} s",self.settings.capacity_retry.delay_secs),rect(width-117.,612.,67.,27.),11.,TEXT,false);
                p.label(&live.capacity_retry.text(self.settings.capacity_retry.enabled,crate::locale::chinese()),rect(20.,646.,width-40.,23.),10.,if live.capacity_retry.last_error.is_empty(){MUTED}else{ORANGE},false);
                let help=if crate::locale::chinese(){format!("仅限最新回合的模型容量不足错误。等待 {} 秒后追加“继续上次任务”，不指定新的模型、推理强度或 Fast。\n已恢复、已停止和旧错误会跳过；相同失败回合只发送一次。\n日志：{}\n{}",self.settings.capacity_retry.delay_secs,self.paths.runtime.join("capacity-retry-events.jsonl").display(),live.capacity_retry.last_error)}
                    else{format!("Append a continuation after {}s only for the latest capacity failure. Keep the chat's model, effort and Fast settings.\nSkip recovered, stopped and old turns; send once per failed turn.\nLog: {}\n{}",self.settings.capacity_retry.delay_secs,self.paths.runtime.join("capacity-retry-events.jsonl").display(),live.capacity_retry.last_error)};
                area(Hit::Info(help),rect(18.,644.,width-36.,27.));
            }
        }
        if kind==Some(Popup::Fixed){self.draw_chat_menu(&mut hits);}
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
            self.painter.target.PushAxisAlignedClip(&rect(paint.rcPaint.left as f32/scale, paint.rcPaint.top as f32/scale,
                (paint.rcPaint.right-paint.rcPaint.left) as f32/scale, (paint.rcPaint.bottom-paint.rcPaint.top) as f32/scale), D2D1_ANTIALIAS_MODE_ALIASED);
            let hits = self.render(
                kind,
                (bounds.right - bounds.left) as f32 / scale,
                (bounds.bottom - bounds.top) as f32 / scale,
            );
            self.painter.target.PopAxisAlignedClip();
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
                    wide_ui(&if a.hit == Hit::Title {
                        format!("Recent chats · {}", self.settings.recent_shortcut.label())
                    } else if let Hit::Recent(id) | Hit::Fixed(id) = &a.hit {
                        let row = self.rows.iter().find(|r| &r.id == id);
                        let mut tip = tip_text(&a.hit, &self.catalog);
                        if let Some(row) = row {
                            tip.push_str(&format!("\n{}", crate::locale::text(&row.state)));
                            if !row.execution.model.is_empty() { tip.push_str(&format!("\n{}", row.execution.tooltip())); }
                        }
                        tip
                    } else if a.hit == Hit::GuardLogs {
                        let guard = self.snapshot().guard;
                        format!("{} recorded storm episodes · view log · continuous storms count once; recovered after 60 stable seconds",
                            guard.storm_count.map(|v|v.to_string()).unwrap_or_else(|| "—".into()))
                    } else if a.hit==Hit::CapacityRetryLogs {
                        let view=self.snapshot().capacity_retry;
                        if crate::locale::chinese(){format!("累计触发 {} 次 · 已提交 {} · 未确认 {}\n点击查看日志 · 同一失败回合只计一次，取消等待不计数 · 重启后保留",view.attempts,view.submitted,view.unconfirmed)}
                        else{format!("{} retry attempts · {} submitted · {} unconfirmed\nView log · once per failed turn, cancelled waits excluded · retained after restart",view.attempts,view.submitted,view.unconfirmed)}
                    } else if let Hit::TagFilter(key)=&a.hit {
                        self.tag_catalog.iter().find(|(tag,_)|&tag.key==key).map(|(tag,count)|format!("{} · {} chats · {} · click to toggle filter",tag.label,count,if tag.kind==tags::Kind::Custom{"custom tag"}else{"synced, read-only"})).unwrap_or_else(||tip_text(&a.hit,&self.catalog))
                    } else { tip_text(&a.hit, &self.catalog) }),
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
        // Telemetry changes must not restart the native tooltip hover delay.
        if let Some(tips) = self.tips.iter_mut().find(|t| t.owner == owner && t.entries.len() == entries.len()) {
            let mut entries = entries;
            for (i, (bounds, text)) in entries.iter_mut().enumerate() {
                let info = TTTOOLINFOW {
                    cbSize: std::mem::offset_of!(TTTOOLINFOW, lpReserved) as u32,
                    uFlags: TTF_SUBCLASS, hwnd: owner, uId: i + 1, rect: *bounds,
                    lpszText: PWSTR(text.as_mut_ptr()), ..Default::default()
                };
                if *bounds != tips.entries[i].0 { let _ = SendMessageW(tips.window, TTM_NEWTOOLRECTW, WPARAM(0), LPARAM(&info as *const _ as isize)); }
                let _ = SendMessageW(tips.window, TTM_UPDATETIPTEXTW, WPARAM(0), LPARAM(&info as *const _ as isize));
            }
            tips.entries = entries;
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
        let _ = SendMessageW(window, TTM_SETMAXTIPWIDTH, WPARAM(0), LPARAM(560));
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
            LPARAM(20000),
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
                if let Some(reset) = q.banked_reset.filter(|r| *r > now()) {
                    width += p.width(&crate::telemetry::countdown(reset, now()), 11.) + 27.;
                }
            }
        }
        if self.settings.reset_notice.enabled {
            let (_,text)=live.reset_notice.badge(now(),crate::locale::chinese());
            width+=if text.is_empty(){33.}else{p.width(&text,11.)+39.};
        }
        if let Some(s) = self.current(&live) {
            let model = s.execution.short();
            if !model.is_empty() { width += p.width(&model, 11.) + 10.; }
        }
        if live.updater.available() {width+=29.;}
        width.ceil() as i32
    }
    unsafe fn round_window(hwnd: HWND, width: i32, height: i32, radius: i32) {
        let region = CreateRoundRectRgn(0, 0, width + 1, height + 1, radius, radius);
        if SetWindowRgn(hwnd, region, true) == 0 {
            let _ = DeleteObject(region);
        }
    }
    unsafe fn tick(&mut self) {
        self.finish_chat_copy();
        self.check_reset_reminders();
        let language=crate::locale::status().0;
        if language!=self.ui_language {
            self.ui_language=language;self.painter.release_layouts();self.tips.clear();
            for (edit,combo) in [(self.edit,self.combo)].into_iter().chain(self.icon_panel.as_ref().map(|p|(p.edit,p.combo))) {
                if !combo.is_invalid() {
                    let selected=SendMessageW(combo,CB_GETCURSEL,WPARAM(0),LPARAM(0));
                    let _=SendMessageW(combo,CB_RESETCONTENT,WPARAM(0),LPARAM(0));
                    if combo==self.combo && self.popup_kind==Some(Popup::Preferences) {
                        for source in [crate::reset_notice::Source::Divin,crate::reset_notice::Source::CodexResets] {
                            let _=SendMessageW(combo,CB_ADDSTRING,WPARAM(0),LPARAM(wide(source.label()).as_ptr() as isize));
                        }
                    } else {for group in &self.groups {let _=SendMessageW(combo,CB_ADDSTRING,WPARAM(0),LPARAM(wide_ui(group).as_ptr() as isize));}}
                    let _=SendMessageW(combo,CB_SETCURSEL,WPARAM(selected.0.max(0) as usize),LPARAM(0));
                    if !edit.is_invalid() && !(combo==self.combo && self.popup_kind==Some(Popup::Preferences)){let _=SendMessageW(edit,EM_SETCUEBANNER,WPARAM(1),LPARAM(wide_ui("Search all emoji…").as_ptr() as isize));}
                }
            }

            let _=InvalidateRect(self.bar,None,false);let _=InvalidateRect(self.popup,None,false);
            for child in [self.edit,self.tag_search_input]{if !child.is_invalid(){let _=InvalidateRect(child,None,true);}}
            if !self.tag_input.is_invalid(){let _=SendMessageW(self.tag_input,EM_SETCUEBANNER,WPARAM(1),LPARAM(wide_ui("Add or select a tag…").as_ptr() as isize));}
        }

        if !self.testing { self.sync_shortcut_scope(false); }
        if !self.event.is_invalid() && WaitForSingleObject(self.event, 0) == WAIT_OBJECT_0 {
            self.hidden = false;
        }
        let live = self.snapshot();
        if self.popup_kind == Some(Popup::Recent) && Instant::now() >= self.tag_catalog_due { self.refresh_tag_catalog(); }
        let identity = self.identity(&live);
        if identity != self.last_identity {
            self.last_identity = identity;
            if self.popup_kind.is_some() && !matches!(self.popup_kind, Some(Popup::Preferences | Popup::GuardLogs)) {
                self.close_popup();
            }
        }
        if self.popup_kind == Some(Popup::Recent) && self.recent_rows_at!=live.recent.loaded_at {
            self.rows = data::stable_rows(&self.rows, &live.recent.rows);
            self.recent_rows_at=live.recent.loaded_at;
        }
        if self.popup_kind == Some(Popup::Fixed) && !live.recent.all_rows.is_empty() && self.recent_rows_at!=live.recent.loaded_at {
            self.rows = data::stable_rows(&self.rows, &live.recent.all_rows);
            self.recent_rows_at=live.recent.loaded_at;
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
                .min(self.chat_filters.len().saturating_sub(self.list_page_rows()));
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
            if self.sidebar_animation.is_none() && matches!(
                self.popup_kind,
                Some(Popup::Recent) | Some(Popup::Details) | Some(Popup::Fixed) | Some(Popup::Preferences) | Some(Popup::GuardLogs)
            ) {
                let _ = InvalidateRect(self.popup, None, false);
            }
            self.last_draw = Instant::now();
        }
        if !self.popup.is_invalid() && self.sidebar_animation.is_none()
            && matches!(self.popup_kind, Some(Popup::Recent | Popup::Fixed))
            && self.last_status_draw.elapsed().as_millis() >= 160
            && self.chat_filters.iter().skip(self.scroll).take(self.list_page_rows()).any(|&i| self.rows[i].state == "Busy") {
            let fixed = self.popup_kind == Some(Popup::Fixed);
            let x = self.popup_size[0] - if fixed { 92 } else { 46 };
            let scale = GetDpiForWindow(self.popup).max(96) as f32 / 96.;
            let bounds = RECT { left: (x as f32 * scale) as i32, top: (if fixed {100.} else {RECENT_TOP as f32} * scale) as i32,
                right: ((x+22) as f32 * scale) as i32, bottom: ((self.popup_size[1]-32) as f32*scale) as i32 };
            let _ = InvalidateRect(self.popup, Some(&bounds), false);
            self.last_status_draw = Instant::now();
        }
        if self.last_diag.elapsed().as_secs() >= self.settings.refresh.diagnostic_secs {
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
    unsafe fn check_reset_reminders(&mut self) {
        if self.testing || !self.settings.reset_notice.enabled || Instant::now()<self.notice_alert_due {return;}
        let Some(control)=self.notice_control.clone() else {return;};
        let Some(alert)=control.next_reminder() else {return;};
        if alert.source!=self.settings.reset_notice.source {control.acknowledge(alert.key,false);return;}
        let (title,body)=alert.text(crate::locale::chinese());
        let mut data=NOTIFYICONDATAW{cbSize:size_of::<NOTIFYICONDATAW>() as u32,hWnd:self.bar,uID:1,
            uFlags:NIF_INFO,dwInfoFlags:NIIF_INFO|NIIF_NOSOUND,..Default::default()};
        for (target,text) in [(&mut data.szInfoTitle[..],title.as_str()),(&mut data.szInfo[..],body.as_str())] {
            let limit=target.len()-1;
            for (index,unit) in text.encode_utf16().take(limit).enumerate(){target[index]=unit;}
        }
        let delivered=Shell_NotifyIconW(NIM_MODIFY,&data).as_bool();
        control.acknowledge(alert.key,delivered);
        self.notice_alert_due=Instant::now()+std::time::Duration::from_secs(20);
    }
    unsafe fn open_icon_panel(&mut self) -> Result<()> {
        let row_height = if self.popup_kind == Some(Popup::Recent) { self.recent_row_height() as f32 } else { 40. };
        let top = if self.popup_kind == Some(Popup::Recent) { RECENT_TOP as f32 } else { 112. };
        let slot = self.icon_target.as_ref().and_then(|(id, _)| self.chat_filters.iter().position(|&i| self.rows[i].id == *id))
            .unwrap_or(self.chat_cursor).saturating_sub(self.scroll).min(self.list_page_rows().saturating_sub(1));
        let row_top = top + slot as f32 * row_height;
        let anchor = [if self.popup_kind == Some(Popup::Recent) { self.recent_left() + 42. } else { 62. }, row_top, row_top + row_height - 4.];
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
                WINDOW_EX_STYLE(0),
                w!("EDIT"),
                w!(""),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                21,
                53,
                146,
                22,
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
                    LPARAM(wide_ui(group).as_ptr() as isize),
                );
            }
            let _ = SendMessageW(combo, CB_SETCURSEL, WPARAM(0), LPARAM(0));
            let _ = SendMessageW(
                edit,
                EM_SETCUEBANNER,
                WPARAM(1),
                LPARAM(wide_ui("Search all emoji…").as_ptr() as isize),
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
            anchor,
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
        // Float beside the clicked icon, over the existing list/sidebar. Never
        // widen or move the parent to make room for this owned popup.
        let x = (parent[0] + (panel.anchor[0] * scale).round() as i32 + gap)
            .max(bounds.left).min((bounds.right - w).max(bounds.left));
        let below = parent[1] + (panel.anchor[2] * scale).round() as i32 + gap;
        let above = parent[1] + (panel.anchor[1] * scale).round() as i32 - gap - h;
        let y = if below + h <= bounds.bottom { below }
            else if above >= bounds.top { above }
            else { below.max(bounds.top).min((bounds.bottom - h).max(bounds.top)) };
        let layout = [x, y, w, h];
        if panel.layout == Some(layout) {
            return;
        }
        panel.layout = Some(layout);
        let _ = SetWindowPos(panel.window, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE);
        Self::round_window(panel.window, w, h, (10. * scale) as i32);
        for (child, x, y, cw, ch) in [
            (panel.edit, 21, 53, 146, 22),
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
            self.list_pointer=None;
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
        self.chat_menu=None;
        self.cancel_sidebar_animation();
        self.close_tag_editor();
        self.close_tag_search();
        if self.tag_scroll_drag.take().is_some(){let _=ReleaseCapture();}
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
        self.painter.release_layouts();
        self.hover = None;
        self.list_pointer=None;
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
        self.chat_menu=None;
        self.cancel_sidebar_animation();
        self.close_tag_editor();
        self.close_tag_search();
        let resume_preferences = kind == Popup::Preferences && self.popup_kind == Some(Popup::GuardLogs);
        if self.popup_kind == Some(Popup::Preferences) && kind == Popup::GuardLogs {
            if let Some(draft) = self.preferences_draft.as_mut() {
                draft.recent_shortcut = Shortcut::from_control_word(
                    SendMessageW(self.edit, HKM_GETHOTKEY, WPARAM(0), LPARAM(0)).0 as u16,
                    draft.recent_shortcut.enabled);
            }
        }
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
        self.list_pointer=None;
        let live = self.snapshot();
        self.popup_size = match kind {
            Popup::Recent => {
                self.rows = live.recent.rows.clone();
                [self.recent_width(), self.recent_panel_height()]
            }
            Popup::Icons => [384, 450],
            Popup::Details => [560, DETAILS_HEIGHT],
            Popup::Fixed => [560, 500],
            Popup::Preferences => [560, PREFERENCES_HEIGHT],
            Popup::GuardLogs => [640, 496],
        };
        if kind == Popup::GuardLogs {
            self.scroll = 0;
            self.refresh_guard_events();
        }
        if kind == Popup::Preferences {
            self.preferences_advanced=false;self.preferences_guard=false;
            if !resume_preferences || self.preferences_draft.is_none() {
                self.preferences_draft = Some(self.settings.clone());
                self.preferences_note = self.shortcut_error.clone();
            }
            self.edit = CreateWindowExW(
                WINDOW_EX_STYLE(0), w!("msctls_hotkey32"), w!(""),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP, 258, 198, 278, 24,
                self.popup, HMENU(104isize as *mut _), GetModuleHandleW(None)?, None,
            )?;
            let shortcut = &self.preferences_draft.as_ref().unwrap().recent_shortcut;
            SendMessageW(self.edit, HKM_SETHOTKEY, WPARAM(shortcut.control_word() as usize), LPARAM(0));
            let _ = EnableWindow(self.edit, shortcut.enabled);
            self.combo=CreateWindowExW(WINDOW_EX_STYLE(0),w!("COMBOBOX"),w!(""),
                WS_CHILD|WS_VISIBLE|WS_TABSTOP|WINDOW_STYLE(CBS_DROPDOWNLIST as u32|WS_VSCROLL.0),
                190,465,288,120,self.popup,HMENU(108isize as *mut _),GetModuleHandleW(None)?,None)?;
            for source in [crate::reset_notice::Source::Divin,crate::reset_notice::Source::CodexResets] {
                SendMessageW(self.combo,CB_ADDSTRING,WPARAM(0),LPARAM(wide(source.label()).as_ptr() as isize));
            }
            let selected=usize::from(self.settings.reset_notice.source==crate::reset_notice::Source::CodexResets);
            SendMessageW(self.combo,CB_SETCURSEL,WPARAM(selected),LPARAM(0));
        }
        if kind == Popup::Icons {
            self.scroll = 0;
            self.search.clear();
            self.category = "默认集合".into();
            self.filters = self.catalog.filter("", "默认集合");
            self.edit = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("EDIT"),
                w!(""),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                21,
                53,
                146,
                22,
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
                    LPARAM(wide_ui(group).as_ptr() as isize),
                );
            }
            let _ = SendMessageW(self.combo, CB_SETCURSEL, WPARAM(0), LPARAM(0));
            let _ = SendMessageW(
                self.edit,
                EM_SETCUEBANNER,
                WPARAM(0),
                LPARAM(wide_ui("Search all emoji…").as_ptr() as isize),
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
            self.refresh_tag_catalog();
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
                if kind == Popup::Recent { 61 } else { 27 },
                if kind == Popup::Recent { 13 } else { 55 },
                if kind == Popup::Recent { 206 } else { 480 },
                24,
                self.popup,
                HMENU(103isize as *mut _),
                GetModuleHandleW(None)?,
                None,
            )?;
            install_search_cue(self.edit, 1)?;
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
        self.ensure_tag_search()?;
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
        if self.sidebar_animation.is_some() && self.popup_layout.is_some() && !self.popup_layout_batch { return; }
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
        let mut x = if self.popup_kind == Some(Popup::Recent) {
            recent_panel_x(bar[0], w, true, scale, bounds.left, bounds.right)
        } else {
            bar[0].max(bounds.left).min((bounds.right - w).max(bounds.left))
        };
        let mut y = if bar[1] - h - 6 >= bounds.top {
            bar[1] - h - 6
        } else {
            (bar[3] + 6).min((bounds.bottom - h).max(bounds.top))
        };
        if self.popup_layout_batch && self.popup_kind==Some(Popup::Recent) {
            if let Some((previous,_))=self.popup_layout {
                x=(previous[0]+previous[2]-w).max(bounds.left).min((bounds.right-w).max(bounds.left));
                y=previous[1].max(bounds.top).min((bounds.bottom-h).max(bounds.top));
            }
        }
        let layout = ([x, y, w, h], scale.to_bits());
        // Moving/resizing a native combo box closes its open drop-down, even
        // when the requested bounds are unchanged. Timer refreshes must not
        // disturb an interaction already in progress.
        if self.popup_layout == Some(layout) {
            return;
        }
        self.popup_layout = Some(layout);
        let flags = if self.popup_layout_batch { SWP_NOACTIVATE | SWP_NOREDRAW } else { SWP_NOACTIVATE };
        let _ = SetWindowPos(self.popup, HWND_TOPMOST, x, y, w, h, flags);
        if self.popup_kind == Some(Popup::Recent) {
            self.apply_recent_region(Instant::now());
        } else {
            Self::round_window(self.popup, w, h, (18. * scale) as i32);
        }
        // Child controls stay in physical pixels, matching the parent DPI.
        for (child, x, y, cw, ch) in [
            if self.popup_kind == Some(Popup::Fixed) {
                (self.edit, 27, 55, 480, 24)
            } else if self.popup_kind == Some(Popup::Recent) {
                (self.edit, self.recent_toolbar_left() as i32 + 83, 14, 184, 22)
            } else if self.popup_kind == Some(Popup::Preferences) {
                (self.edit, 258, 198, 278, 24)
            } else {
                (self.edit, 21, 53, 146, 22)
            },
            if self.popup_kind==Some(Popup::Preferences) {(self.combo,190,465,288,120)}else{(self.combo, 214, 49, 155, 350)},
        ] {
            if !child.is_invalid() {
                let _ = MoveWindow(
                    child,
                    (x as f32 * scale) as i32,
                    (y as f32 * scale) as i32,
                    (cw as f32 * scale) as i32,
                    (ch as f32 * scale) as i32,
                    !self.popup_layout_batch,
                );
            }
        }
        self.place_tag_search();
    }
    fn settings(&self) {
        let _ = self.write_settings(&self.settings);
    }
    fn diagnostic(&self) {
        self.paths.write(if self.testing{"rust-test-memory.json"}else{"memory-status.json"},&json!({"time":now(),"private_bytes":private_bytes(),"layout_entries":self.painter.layouts.borrow().len(),"layout_limit":LAYOUT_CACHE_LIMIT,"width_entries":self.painter.measurements.borrow().len(),"popup_open":self.popup_kind.is_some()}));
        self.paths.write(if self.testing { "rust-test-shortcut-status.json" } else { "shortcut-status.json" },
            &json!({"binding":self.settings.recent_shortcut.label(),"enabled":self.settings.recent_shortcut.enabled,"recent_visible_rows":self.settings.recent_rows,"registered":self.shortcut_registered,"error":self.shortcut_error}));
        self.paths.write(if self.testing { "rust-test-body-status.json" } else { "body-search-status.json" },
            &json!({"enabled":self.search_body,"pending":self.body_pending,"indexing":self.index_pending,"complete":self.body_state.complete,
                "local_only":true,"progress_percent":self.body_state.progress_percent(),
                "indexed":self.body_state.indexed,"available_local":self.body_state.available,"total_chats":self.body_state.total,
                "bytes_read":self.body_state.bytes_read,"skipped_oversized_messages":self.body_state.skipped,"error":self.body_state.error,
                "cache_exists":self.paths.runtime.join("body-search.sqlite").exists(),"on_demand":false}));
        let live = self.snapshot();
        let (_, title, status, _, _) = unsafe { self.bar_text(&live) };
        self.paths.write(if self.testing{"rust-test-status.json"}else{"live-status.json"},&json!({"pid":std::process::id(),"runtime":"rust","ui_language":{"culture":crate::locale::status().0,"source":if crate::locale::status().1{"codex-uia"}else{"windows-default"}},"refresh":{"file_events":live.refresh_events.watching.load(Ordering::Relaxed),"settings":live.refresh_options},"updated_at":now(),"thread":self.identity(&live),"overview":self.identity(&live).is_empty() && data::visible_chat_title(&live.view_label).is_none(),"display_title":title,"display_status":status,"tokens":self.bar_usage(&live).0,"auto_thread":live.auto_thread,"topmost":true,"user_hidden":self.hidden,"host_hidden":self.host_hidden,"host_window":live.host,"host_rect":self.last_host,"host_offset":self.settings.host_offset,"titlebar_slot":live.slot,"quota":live.quota,"quota_error":live.quota_error,"view_error":live.view_error,"snapshot_error":live.snapshot_error,"recent_cache":{"rows":live.recent.rows.len(),"busy_count":live.recent.busy_count,"stale":live.recent.stale,"error":live.recent.error},"active_view":{"thread":live.view,"label":live.view_label,"available":live.view.is_some() || data::visible_chat_title(&live.view_label).is_some(),"identity_pending":live.auto_thread && live.view.is_none() && data::visible_chat_title(&live.view_label).is_some(),"source":"uia-current-page"},"action_note":live.action_note,"codex_git_guard":live.guard,"font":{"chinese":self.painter.chinese,"latin":"Segoe UI","emoji":"Segoe UI Emoji","renderer":"DirectWrite / software Direct2D","antialias":"ClearType","per_monitor_dpi":true}}));
    }
    unsafe fn hit(&mut self, hit: Hit) -> Option<Action> {
        match hit {
            Hit::Info(_) => {}
            Hit::ChatCopy(slot) => {
                if slot==1 {
                    if !self.testing {self.start_chat_copy();}
                    self.chat_menu=None;let _=InvalidateRect(self.popup,None,false);return None;
                }
                if let Some(text)=self.chat_menu.as_ref().and_then(|menu|menu.text(slot)) {
                    if !self.testing {match copy_clipboard(self.popup,&text){Ok(())=>self.set_copy_note("Copied".into()),Err(error)=>self.set_copy_note(format!("Copy failed: {error}"))}}
                }
                self.chat_menu=None;let _=InvalidateRect(self.popup,None,false);
            }
            Hit::ChatMenuClose => {self.chat_menu=None;let _=InvalidateRect(self.popup,None,false);}
            Hit::ChatMenuBackground => {}

            Hit::ClearChatSearch => {
                self.search.clear(); self.filter_fixed();
                let _=SetWindowTextW(self.edit,w!("")); let _=SetFocus(self.edit);
                self.refresh_list_hover();let _=InvalidateRect(self.popup,None,false);
            }
            Hit::ClearTagSearch => {
                self.tag_query.clear(); self.tag_scroll=[0.;3]; self.rebuild_tag_regions();
                let _=SetWindowTextW(self.tag_search_input,w!("")); let _=SetFocus(self.tag_search_input);
                let _=InvalidateRect(self.popup,None,false);
            }
            Hit::ClearEmojiSearch => {
                if let Some(panel)=self.icon_panel.as_mut() {
                    panel.search.clear(); panel.filters=self.catalog.filter("",&panel.category); panel.scroll=0;
                    let _=SetWindowTextW(panel.edit,w!("")); let _=SetFocus(panel.edit);
                    let _=InvalidateRect(panel.window,None,false);
                } else {
                    self.search.clear(); self.filters=self.catalog.filter("",&self.category); self.scroll=0;
                    let _=SetWindowTextW(self.edit,w!("")); let _=SetFocus(self.edit);
                    let _=InvalidateRect(self.popup,None,false);
                }
            }
            Hit::GuardLogs => { self.capacity_log=false;let _ = self.popup(Popup::GuardLogs); }
            Hit::CapacityRetryLogs=>{self.capacity_log=true;let _=self.popup(Popup::GuardLogs);}
            Hit::GuardRefresh | Hit::CapacityRetryRefresh => { self.refresh_guard_events(); }
            Hit::BackPreferences => { let _ = self.popup(Popup::Preferences); }
            Hit::GuardFolder | Hit::CapacityRetryFolder => {
                if !self.testing {
                    let path = if self.capacity_log{self.paths.runtime.clone()}else{guard::log_dir(&self.paths)};
                    let path = path.to_string_lossy().trim_start_matches(r"\\?\").to_string();
                    let result = ShellExecuteW(self.popup, w!("open"), PCWSTR(wide(&path).as_ptr()),
                        PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
                    if result.0 as isize <= 32 { self.guard_log_error = "Cannot open log folder".into(); }
                }
            }
            Hit::Preferences => { let _ = self.popup(Popup::Preferences); }
            Hit::ResetNotice(_) => {
                if !self.testing {
                    let source=self.settings.reset_notice.source;
                    let _=ShellExecuteW(self.bar,w!("open"),PCWSTR(wide(source.url()).as_ptr()),PCWSTR::null(),PCWSTR::null(),SW_SHOWNORMAL);
                }
            }
            Hit::ResetNoticeEnabled => {
                let mut config=self.settings.reset_notice.clone();config.enabled=!config.enabled;
                if let Err(error)=self.set_reset_notice(config){self.preferences_note=format!("Could not save: {error}");}
            }
            Hit::CapacityRetryEnabled=>{
                let mut config=self.settings.capacity_retry.clone();config.enabled=!config.enabled;
                if let Err(error)=self.set_capacity_retry(config){self.preferences_note=format!("Could not save: {error}");}
            }
            Hit::CapacityRetryInterval(delta)=>{
                let mut config=self.settings.capacity_retry.clone();config.delay_secs=(config.delay_secs as i64+delta as i64).clamp(1,300) as u64;
                if let Err(error)=self.set_capacity_retry(config){self.preferences_note=format!("Could not save: {error}");}
            }
            Hit::ResetNoticeInterval(delta) => {
                let mut config=self.settings.reset_notice.clone();config.check_minutes=(config.check_minutes as i64+delta as i64).clamp(1,60) as u64;
                if let Err(error)=self.set_reset_notice(config){self.preferences_note=format!("Could not save: {error}");}
            }
            Hit::CheckUpdate => {
                if self.testing {
                    self.state.lock().unwrap().updater.phase="checking".into();
                } else if let Some(control)=&self.update_control {
                    let _=control.request();
                }
            }
            Hit::UpdateAvailable(_)=>{
                if !self.testing {
                    if let Some(control)=&self.update_control {let _=control.review();}
                }
            }
            Hit::ProjectHome=>{
                if !self.testing {
                    let result=ShellExecuteW(self.popup,w!("open"),PCWSTR(wide("https://github.com/frankfoxy/codex-session-bar").as_ptr()),PCWSTR::null(),PCWSTR::null(),SW_SHOWNORMAL);
                    if result.0 as isize<=32 {self.preferences_note="Cannot open GitHub".into();}
                }
            }
            Hit::TitleWidth(delta)=>{
                if let Some(draft)=self.preferences_draft.as_mut(){draft.recent_title_width=(draft.recent_title_width as i32+delta).clamp(140,280) as u32;self.preferences_note="Apply to save changes".into();}
            }
            Hit::Rows(delta) => {
                if let Some(draft) = self.preferences_draft.as_mut() {
                    draft.recent_rows = (draft.recent_rows as i32 + delta).clamp(4, 20) as usize;
                    self.preferences_note = "Apply to save changes".into();
                }
            }
            Hit::ShortcutEnabled => {
                if let Some(draft) = self.preferences_draft.as_mut() {
                    draft.recent_shortcut.enabled = !draft.recent_shortcut.enabled;
                    let _ = EnableWindow(self.edit, draft.recent_shortcut.enabled);
                    self.preferences_note = "Apply to save changes".into();
                }
            }
            Hit::ResetPreferences => {
                if let Some(draft) = self.preferences_draft.as_mut() {
                    draft.refresh=crate::refresh::Settings::default();
                    draft.recent_rows = 10;
                    draft.recent_title_width=200;
                    draft.recent_shortcut = Shortcut::default();
                    SendMessageW(self.edit, HKM_SETHOTKEY, WPARAM(draft.recent_shortcut.control_word() as usize), LPARAM(0));
                    let _ = EnableWindow(self.edit, true);
                    self.preferences_note = "Defaults restored · apply to save".into();
                }
            }
            Hit::GuardSettings => {
                if let Some(draft)=self.preferences_draft.as_mut(){draft.recent_shortcut=Shortcut::from_control_word(SendMessageW(self.edit,HKM_GETHOTKEY,WPARAM(0),LPARAM(0)).0 as u16,draft.recent_shortcut.enabled);}
                self.guard_limits=guard::Limits::load(&self.paths);self.preferences_guard=true;self.preferences_advanced=false;self.preferences_note.clear();if !self.testing{let _=SetFocus(self.popup);}let _=ShowWindow(self.edit,SW_HIDE);let _=ShowWindow(self.combo,SW_HIDE);
            }
            Hit::GuardLimit(field,delta) => {self.guard_limits.adjust(field,delta);self.preferences_note="Apply to save changes".into();}
            Hit::RefreshPreferences => {
                if let Some(draft)=self.preferences_draft.as_mut(){draft.recent_shortcut=Shortcut::from_control_word(SendMessageW(self.edit,HKM_GETHOTKEY,WPARAM(0),LPARAM(0)).0 as u16,draft.recent_shortcut.enabled);}
                self.preferences_advanced=true;let _=ShowWindow(self.edit,SW_HIDE);let _=ShowWindow(self.combo,SW_HIDE);
            }
            Hit::RefreshBack => {self.preferences_guard=false;self.preferences_advanced=false;let _=ShowWindow(self.edit,SW_SHOW);let _=ShowWindow(self.combo,SW_SHOW);}
            Hit::RefreshInterval(field,delta) => {if let Some(draft)=self.preferences_draft.as_mut(){draft.refresh.adjust(field,delta);self.preferences_note="Apply to save changes".into();}}
            Hit::ApplyPreferences => {
                let result=if self.preferences_guard {self.guard_limits.save(&self.paths)}else{self.apply_preferences()};
                self.preferences_note=match result{Ok(())=>"Saved · changes are active".into(),Err(error)=>format!("{error:#}")};
            }
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
                {let mut live=self.state.lock().unwrap();live.quota_at=-1.;live.refresh_events.notify();}
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
            Hit::TagSidebar => { self.toggle_sidebar(!self.testing); }
            Hit::Compact => {
                if self.sidebar_animation.is_some() { self.advance_sidebar_animation(Instant::now() + std::time::Duration::from_millis(SIDEBAR_DURATION_MS)); }
                self.close_tag_editor();self.close_icon_panel();
                self.settings.compact_recent=!self.settings.compact_recent;
                self.popup_size=[self.recent_width(),self.recent_panel_height()];self.popup_layout=None;
                for i in 0..3{self.tag_scroll[i]=self.tag_scroll[i].min(self.tag_scroll_max(i));}
                self.scroll=self.scroll.min(self.chat_filters.len().saturating_sub(self.list_page_rows()));
                self.settings();self.place_popup();
            }
            Hit::TagFilter(key) => {
                self.close_tag_editor();
                if !self.tag_filters.remove(&key){self.tag_filters.insert(key);}
                self.chat_filters=self.filtered_chats();self.scroll=0;self.chat_cursor=0;
            }
            Hit::TagClear => {self.tag_filters.clear();self.chat_filters=self.filtered_chats();self.scroll=0;self.chat_cursor=0;}
            Hit::TagOverflow(id)=>{self.open_tag_overflow(id);}
            Hit::TagOverflowClose=>{self.tag_overflow=None;let _=InvalidateRect(self.popup,None,false);}
            Hit::TagOverflowBackground=>{}
            Hit::TagEdit(id) => {self.close_icon_panel();let _=self.open_tag_editor(id);}
            Hit::TagToggle(key) => {if let Some(id)=self.tag_editor.clone(){self.custom_tags.toggle(&id,&key);self.save_tag_changes();}}
            Hit::TagColor(color) => {self.tag_color=color;}
            Hit::TagAdd => {if let Some(id)=self.tag_editor.clone(){
                match self.custom_tags.add(&id,&self.tag_draft,self.tag_color){
                    Ok(())=>{self.save_tag_changes();self.tag_draft.clear();if !self.tag_input.is_invalid(){let _=SetWindowTextW(self.tag_input,w!(""));}}
                    Err(e)=>self.tag_note=e.to_string(),
                }
            }}
            Hit::TagClose => {self.close_tag_editor();self.chat_filters=self.filtered_chats();self.scroll=self.scroll.min(self.chat_filters.len().saturating_sub(self.list_page_rows()));self.chat_cursor=self.chat_cursor.min(self.chat_filters.len().saturating_sub(1));}
            Hit::TagBackground => {}
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
        self.chat_menu = None;
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
        let titles = self.chat_search.filter(&self.rows, &self.search, false);
        ranked_matches(
            &self.rows,
            &titles,
            &self.body_hits,
            !self.search.trim().is_empty(),
        ).into_iter().filter(|&i| self.popup_kind != Some(Popup::Recent) || self.custom_tags.matches(&self.rows[i], &self.tag_filters)).collect()
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
        if self.chat_cursor >= self.scroll + self.list_page_rows() {
            self.scroll = self.chat_cursor + 1 - self.list_page_rows();
        }
    }
    fn fixed_hit_at(&self, x: f32, y: f32) -> Option<Hit> {
        if self.chat_menu.is_some(){return self.chat_menu_hit(x,y);}
        let width = self.popup_size[0] as f32;
        if self.popup_kind == Some(Popup::Recent) {
            // Recompute geometry from current scroll/filter state, never stale paint hits.
            if self.tag_overflow.is_some(){
                let r=self.tag_overflow_rect();
                if Self::contains_rect(r,x,y){return self.popup_hits.iter().rev().find(|a|a.contains(x,y)&&matches!(a.hit,Hit::TagOverflowClose|Hit::TagEdit(_)|Hit::TagFilter(_)|Hit::TagOverflowBackground)).map(|a|a.hit.clone()).or(Some(Hit::TagOverflowBackground));}
                return Some(Hit::TagOverflowClose);
            }
            if self.tag_editor.is_some() {
                let r=self.tag_editor_rect();
                if x>=r.left&&x<r.right&&y>=r.top&&y<r.bottom {
                    return self.popup_hits.iter().rev().find(|a|a.contains(x,y)&&matches!(a.hit,Hit::TagClose|Hit::TagAdd|Hit::TagToggle(_)|Hit::TagColor(_)|Hit::TagBackground)).map(|a|a.hit.clone()).or(Some(Hit::TagBackground));
                }
                return Some(Hit::TagClose);
            }
            if !self.search.is_empty() && Self::contains_rect(self.chat_clear_rect(),x,y){return Some(Hit::ClearChatSearch);}
            if self.sidebar_open && !self.tag_query.is_empty() && Self::contains_rect(Self::tag_clear_rect(),x,y){return Some(Hit::ClearTagSearch);}
            if y < RECENT_TOP as f32 {
                let toolbar_x=x-self.recent_toolbar_left();
                if (12. ..42.).contains(&toolbar_x)&&(9. ..39.).contains(&y){return Some(Hit::TagSidebar);}
                if (310. ..370.).contains(&toolbar_x)&&(8. ..40.).contains(&y){return Some(Hit::Body);}
                if (382. ..442.).contains(&toolbar_x)&&(8. ..40.).contains(&y){return Some(Hit::Compact);}
                if (454. ..486.).contains(&toolbar_x)&&(8. ..40.).contains(&y){return Some(Hit::Refresh);}
                return self.popup_hits.iter().rev().find(|a|a.contains(x,y)&&matches!(a.hit,Hit::TagFilter(_)|Hit::TagClear)).map(|a|a.hit.clone());
            }
            if self.sidebar_open && x<TAG_SIDEBAR {
                if !self.tag_query.is_empty() && Self::contains_rect(Self::tag_clear_rect(),x,y){return Some(Hit::ClearTagSearch);}
                return self.tag_cloud_hit(x,y);
            }
            let top=RECENT_TOP as f32;let rh=self.recent_row_height() as f32;let left=self.recent_left();
            if !(left..width-18.).contains(&x)||!(top..top+self.list_page_rows() as f32*rh).contains(&y)||(y-top)%rh>=rh-4.{return None;}
            let slot=self.scroll+((y-top)/rh) as usize;
            let &i=self.chat_filters.get(slot)?;let row=&self.rows[i];
            let local_y=(y-top)%rh;let icon_top=if self.settings.compact_recent{4.}else{7.};
            if (left+12. ..left+42.).contains(&x)&&(icon_top..icon_top+30.).contains(&local_y){return Some(Hit::ChatIcon(row.id.clone()));}
            if !self.settings.compact_recent{
                let layout=unsafe{self.tag_layout(row)};
                if Self::contains_rect(layout.add,x,local_y){return Some(Hit::TagEdit(row.id.clone()));}
                for(tag,r)in layout.chips{if Self::contains_rect(r,x,local_y){return Some(Hit::TagFilter(tag.key));}}
                if layout.hidden>0 && Self::contains_rect(layout.action,x,local_y){return Some(Hit::TagOverflow(row.id.clone()));}
            }
            return Some(Hit::Recent(row.id.clone()));
        }
        if !self.search.is_empty() && Self::contains_rect(self.chat_clear_rect(),x,y){return Some(Hit::ClearChatSearch);}
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
            if (29. ..62.).contains(&x) {
                Hit::ChatIcon(self.rows[index].id.clone())
            } else {
                Hit::Fixed(self.rows[index].id.clone())
            }
        })
    }
    unsafe fn fixed_key(&mut self, key: u32) -> bool {
        if self.chat_menu.is_some(){return self.chat_menu_key(key);}
        if self.tag_overflow.is_some(){
            if key==VK_ESCAPE.0 as u32{let _=self.hit(Hit::TagOverflowClose);return true;}
            if key==VK_RETURN.0 as u32{return true;}
            let delta=match key {
                k if k==VK_UP.0 as u32=>Some(-26.),k if k==VK_DOWN.0 as u32=>Some(26.),
                k if k==VK_PRIOR.0 as u32=>Some(-156.),k if k==VK_NEXT.0 as u32=>Some(156.),
                k if k==VK_HOME.0 as u32=>Some(-1e6),k if k==VK_END.0 as u32=>Some(1e6),_=>None,
            };
            if let Some(delta)=delta{self.scroll_tag_overflow(delta);let _=InvalidateRect(self.popup,None,false);return true;}
        }
        if !self.tag_search_input.is_invalid() && GetFocus()==self.tag_search_input {
            if key==VK_RETURN.0 as u32 {
                if let Some(key)=self.tag_regions.iter().find_map(|region|region.items.first().map(|i|self.tag_catalog[i.index].0.key.clone())){let _=self.hit(Hit::TagFilter(key));}
                let _=InvalidateRect(self.popup,None,false);return true;
            }
            if key!=VK_ESCAPE.0 as u32{return false;}
        }
        if self.tag_editor.is_some(){
            if key==VK_ESCAPE.0 as u32{let _=self.hit(Hit::TagClose);return true;}
            if key==VK_RETURN.0 as u32{let _=self.hit(Hit::TagAdd);let _=InvalidateRect(self.popup,None,false);return true;}
            return false;
        }
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
            k if k == VK_PRIOR.0 as u32 => self.move_fixed_cursor(-(self.list_page_rows() as isize)),
            k if k == VK_NEXT.0 as u32 => self.move_fixed_cursor(self.list_page_rows() as isize),
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
        let page_rows = self.list_page_rows();
        if self.chat_filters.len() <= page_rows {
            return None;
        }
        let (top, height) = match self.popup_kind {
            Some(Popup::Recent) => (RECENT_TOP as f32+2., page_rows as f32 * self.recent_row_height() as f32 - 7.),
            Some(Popup::Fixed) => (100., 350.),
            _ => return None,
        };
        Some((
            top,
            height,
            (height * page_rows as f32 / self.chat_filters.len() as f32).max(20.),
        ))
    }
    unsafe fn begin_chat_scroll(&mut self, x: f32, y: f32) -> bool {
        if self.tag_editor.is_some()||self.tag_overflow.is_some(){return false;}
        let Some((top, height, thumb)) = self.chat_scroll_geometry() else {
            return false;
        };
        let width = self.popup_size[0] as f32;
        if !(width - 17. ..width - 4.).contains(&x) || !(top..top + height).contains(&y) {
            return false;
        }
        let offset = (height - thumb) * self.scroll as f32 / (self.chat_filters.len() - self.list_page_rows()) as f32;
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
                * (self.chat_filters.len() - self.list_page_rows()) as f32)
                .round() as usize;
            self.chat_cursor = self.chat_cursor.clamp(self.scroll, self.scroll + self.list_page_rows() - 1);
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
    // All windows of this class are borderless popups. Preserve their existing
    // right-hand pixels when the tag panel grows/shrinks on the left. Handle
    // this before borrowing App because resize dispatches it synchronously.
    if message==WM_NCCALCSIZE && wparam.0!=0 {
        return LRESULT((WVR_ALIGNRIGHT|WVR_ALIGNTOP) as isize);
    }
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
            WM_HOTKEY if is_bar && wparam.0 == app.shortcut_id as usize => {
                if app.shortcut_allowed() && app.popup_kind != Some(Popup::Preferences) { app.recent_shortcut(); }
                Some(LRESULT(0))
            }
            FOCUS_MESSAGE if is_bar => { app.sync_shortcut_scope(true); Some(LRESULT(0)) }
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
            WM_TIMER if !is_bar && wparam.0 == SIDEBAR_TIMER => {
                app.advance_sidebar_animation(Instant::now());
                Some(LRESULT(0))
            }
            WM_TIMER => {
                app.tick();
                Some(LRESULT(0))
            }
            WM_CONTEXTMENU if !is_bar && matches!(app.popup_kind,Some(Popup::Recent|Popup::Fixed)) => {
                let mut point=POINT{x:lparam.0 as i16 as i32,y:(lparam.0>>16) as i16 as i32};
                if lparam.0 == -1 {
                    let (left,top,row)=if app.popup_kind==Some(Popup::Recent){(app.recent_left(),RECENT_TOP as f32,app.recent_row_height() as f32)}else{(16.,112.,40.)};
                    point=POINT{x:((left+60.)*scale) as i32,y:((top+(app.chat_cursor.saturating_sub(app.scroll)) as f32*row+20.)*scale) as i32};
                } else {let _=ScreenToClient(hwnd,&mut point);}
                app.open_chat_menu(point.x as f32/scale,point.y as f32/scale);Some(LRESULT(0))
            }
            WM_MOUSEWHEEL if !is_bar && app.chat_menu.is_some()=>Some(LRESULT(0)),
            WM_MOUSELEAVE => {
                app.hover=None; app.list_pointer=None; let _=InvalidateRect(hwnd,None,false); Some(LRESULT(0))
            }
            WM_MOUSEMOVE => {
                if !is_bar && matches!(app.popup_kind,Some(Popup::Recent|Popup::Fixed)) && app.set_list_pointer(Some((x,y))){let _=InvalidateRect(hwnd,None,false);}
                let mut tracking=TRACKMOUSEEVENT{cbSize:size_of::<TRACKMOUSEEVENT>() as u32,dwFlags:TME_LEAVE,hwndTrack:hwnd,dwHoverTime:0};
                let _=TrackMouseEvent(&mut tracking);
                if app.tag_scroll_drag.is_some(){
                    app.drag_tag_scroll(y);
                } else if app.chat_scroll_drag.is_some() {
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
                if !is_bar && app.begin_tag_scroll(x,y){return Some(LRESULT(0));}
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
                if app.tag_scroll_drag.take().is_some(){
                    let _=ReleaseCapture();
                } else if app.chat_scroll_drag.take().is_some() {
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
            WM_MOUSEWHEEL if !is_bar && app.popup_kind==Some(Popup::Recent) && app.tag_overflow.is_some()=>{
                let delta=((wparam.0>>16) as i16) as i32;
                app.scroll_tag_overflow(if delta<0{48.}else{-48.});let _=InvalidateRect(app.popup,None,false);Some(LRESULT(0))
            }
            WM_MOUSEWHEEL if !is_bar && app.popup_kind==Some(Popup::Recent) && (app.tag_editor.is_some() || (app.sidebar_open && {
                let mut cursor=POINT::default();let _=GetCursorPos(&mut cursor);let _=ScreenToClient(app.popup,&mut cursor);(cursor.x as f32/scale)<TAG_SIDEBAR
            })) => {
                let delta=((wparam.0>>16) as i16) as i32;
                if app.tag_editor.is_some(){
                    let page=((app.tag_editor_rect().bottom-app.tag_editor_rect().top-204.)/25.).max(1.) as usize;
                    app.tag_editor_scroll=if delta<0{(app.tag_editor_scroll+3).min(app.custom_tags.definitions.len().saturating_sub(page))}else{app.tag_editor_scroll.saturating_sub(3)};
                }else{
                    let mut cursor=POINT::default();let _=GetCursorPos(&mut cursor);let _=ScreenToClient(app.popup,&mut cursor);
                    if let Some(i)=app.tag_region_at(cursor.x as f32/scale,cursor.y as f32/scale){app.scroll_tag_region(i,if delta<0{48.}else{-48.});}
                }
                let _=InvalidateRect(app.popup,None,false);Some(LRESULT(0))
            }
            WM_MOUSEWHEEL
                if !is_bar && matches!(app.popup_kind, Some(Popup::Fixed | Popup::Recent)) =>
            {
                let delta = ((wparam.0 >> 16) as i16) as i32;
                app.scroll = if delta < 0 {
                    (app.scroll + 3).min(app.chat_filters.len().saturating_sub(app.list_page_rows()))
                } else {
                    app.scroll.saturating_sub(3)
                };
                app.chat_cursor = app.chat_cursor.clamp(
                    app.scroll,
                    (app.scroll + app.list_page_rows() - 1).min(app.chat_filters.len().saturating_sub(1)),
                );
                app.refresh_list_hover();
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
            WM_MOUSEWHEEL if !is_bar && app.popup_kind == Some(Popup::GuardLogs) => {
                let delta = ((wparam.0 >> 16) as i16) as i32;
                app.scroll = if delta < 0 { (app.scroll + 3).min(app.guard_events.len().saturating_sub(8)) }
                    else { app.scroll.saturating_sub(3) };
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
                if id==108 && code==CBN_SELCHANGE && app.popup_kind==Some(Popup::Preferences) {
                    app.change_notice_source();
                    let _=InvalidateRect(app.popup,None,false);
                    return Some(LRESULT(0));
                }
                if (id == 100 && code == EN_CHANGE) || (id == 101 && code == CBN_SELCHANGE) {
                    app.update_filter();
                } else if id == 103 && code == EN_CHANGE {
                    let mut text = [0u16; 256];
                    let len = GetWindowTextW(app.edit, &mut text);
                    app.search = String::from_utf16_lossy(&text[..len as usize]);
                    app.filter_fixed();
                    let _ = InvalidateRect(app.popup, None, false);
                } else if id == 106 && code == EN_CHANGE {
                    let mut text=[0u16;256];let len=GetWindowTextW(app.tag_search_input,&mut text);
                    app.tag_query=String::from_utf16_lossy(&text[..len as usize]);
                    app.tag_scroll=[0.;3];app.rebuild_tag_regions();let _=InvalidateRect(app.popup,None,false);
                } else if id == 105 && code == EN_CHANGE {
                    let mut text=[0u16;256];let len=GetWindowTextW(app.tag_input,&mut text);
                    app.tag_draft=String::from_utf16_lossy(&text[..len as usize]);
                } else if id == 104 && code == EN_CHANGE {
                    app.preferences_note = "Apply to save changes".into();
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
                if event == WM_LBUTTONUP || event == WM_LBUTTONDBLCLK || event == NIN_BALLOONUSERCLICK {
                    app.hidden = false;
                    app.tick();
                } else if event == WM_RBUTTONUP {
                    if let Ok(menu) = CreatePopupMenu() {
                        let _ = AppendMenuW(menu, MF_STRING, 1, PCWSTR(wide_ui("Open session bar").as_ptr()));
                        let _ = AppendMenuW(menu, MF_STRING, 3, PCWSTR(wide_ui("Settings").as_ptr()));
                        let _ = AppendMenuW(menu, MF_STRING, 4, PCWSTR(wide_ui("Hide to tray").as_ptr()));
                        let _ = AppendMenuW(menu, MF_STRING, 2, PCWSTR(wide_ui("Quit session bar").as_ptr()));
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
                        } else if chosen.0 == 3 {
                            app.hidden = false;
                            app.tick();
                            let _ = app.popup(Popup::Preferences);
                        } else if chosen.0 == 4 {
                            let _ = app.hit(Hit::Hide);
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
        let mut app = App::new(paths.clone(), settings, state.clone(), stop.clone(), false)?;
        let workers = Workers::start(&paths, &state, &stop, &app.catalog);
        let mut observers = host::start(&paths, &state, &stop);
        observers.push(guard::start(&paths, &state, &stop));
        observers.push(crate::capacity_retry::start(&paths,&state,&stop,workers.actions.clone()));
        let updates = crate::update::Service::start(&paths, &state, &stop, app.bar.0 as isize, QUIT_MESSAGE);
        app.update_control=updates.as_ref().map(|service|service.control());
        let notices=crate::reset_notice::Service::start(&paths,&state,&stop,app.settings.reset_notice.clone());
        app.notice_control=Some(notices.control());
        ACTIONS.with(|a| *a.borrow_mut() = Some(workers.actions.clone()));
        APP.with(|a| *a.borrow_mut() = Some(app));
        let focus_hook=SetWinEventHook(EVENT_SYSTEM_FOREGROUND,EVENT_SYSTEM_FOREGROUND,None,Some(foreground_changed),0,0,WINEVENT_OUTOFCONTEXT);
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
        if !focus_hook.is_invalid(){let _=UnhookWinEvent(focus_hook);}
        stop.store(true, Ordering::Relaxed);
        drop(updates);
        drop(notices);
        drop(workers);
        for observer in observers {
            let _ = observer.join();
        }
        APP.with(|a| {
            if let Some(app) = a.borrow_mut().take() {
                app.settings();
                app.tray(false);
                if app.shortcut_registered {
                    let _ = UnregisterHotKey(app.bar, app.shortcut_id);
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
    capture_render(app, width, height, path, || app.render(kind, width as f32, height as f32))
}
unsafe fn capture_render(app: &App, width: u32, height: u32, path: &std::path::Path, draw: impl FnOnce() -> Vec<Area>) -> Result<Vec<Area>> {
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
    let hits = draw();
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
unsafe fn capture_docking(app: &App, paths: &Paths) -> Result<()> {
    let width = 1100.;
    let bar_width = app.measure_bar() as f32;
    capture_render(app, width as u32, 190, &paths.runtime.join("rust-docking-verification.png"), || {
        let p = &app.painter;
        p.fill(rect(0., 0., width, 190.), 0x181a1c, 0.);
        p.fill(rect(0., 0., width, 44.), 0x202428, 0.);
        p.line(0., 44., width, 44., 0x343c42, 1.);
        p.ui_label("‹    ›    ◫     File    Edit    View    Help", rect(20., 5., 300., 34.), 12., MUTED, false);
        for (x, text) in [(974., "−"), (1020., "□"), (1065., "×")] {
            p.label(text, rect(x, 5., 28., 34.), 14., MUTED, false);
        }
        p.fill(rect(0., 45., 244., 145.), 0x202326, 0.);
        p.line(244., 45., 244., 190., 0x343c42, 1.);
        p.ui_label("Codex", rect(58., 61., 160., 28.), 16., TEXT, false);
        p.ui_label("+  New chat", rect(58., 101., 160., 28.), 12., MUTED, false);
        p.ui_label("Projects", rect(58., 139., 160., 28.), 12., MUTED, false);
        p.ui_label("▱  xxx 会话名称", rect(270., 56., 600., 30.), 13., TEXT, false);
        p.border(rect(273., 129., 796., 44.), 0x3b444c, 14.);
        p.ui_label("Ask anything", rect(290., 134., 600., 30.), 13., MUTED, false);
        let identity = windows::Foundation::Numerics::Matrix3x2 { M11:1., M22:1., ..Default::default() };
        let position = windows::Foundation::Numerics::Matrix3x2 { M31:(320.+964.-bar_width)/2., M32:5., ..identity };
        p.target.SetTransform(&position);
        p.target.PushAxisAlignedClip(&rect(0., 0., bar_width, 34.), D2D1_ANTIALIAS_MODE_ALIASED);
        let hits = app.render(None, bar_width, 34.);
        p.target.PopAxisAlignedClip();
        p.target.SetTransform(&identity);
        hits
    })?;
    Ok(())
}
unsafe fn verify_native_search_cue(hwnd: HWND, kind: usize, path: &std::path::Path) -> Result<()> {
    let mut registered = 0;
    assert!(GetWindowSubclass(hwnd, Some(search_cue_proc), SEARCH_CUE_SUBCLASS, Some(&mut registered)).as_bool());
    assert_eq!(registered, kind);
    let mut bounds = RECT::default();
    GetClientRect(hwnd, &mut bounds)?;
    let (width, height) = (bounds.right.max(1), bounds.bottom.max(1));
    let dc = CreateCompatibleDC(None);
    let mut info = BITMAPINFO::default();
    info.bmiHeader = BITMAPINFOHEADER { biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width, biHeight: -height, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB.0, ..Default::default() };
    let mut bits = std::ptr::null_mut();
    let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
    let previous = SelectObject(dc, bitmap);
    let brush = CreateSolidBrush(COLORREF(0x25211c));
    let original = {
        let mut text = [0u16; 256];
        let count = GetWindowTextW(hwnd, &mut text);
        String::from_utf16_lossy(&text[..count as usize])
    };
    let _ = SetWindowTextW(hwnd, w!(""));
    FillRect(dc, &bounds, brush);
    SendMessageW(hwnd, WM_PRINTCLIENT, WPARAM(dc.0 as usize), LPARAM(PRF_CLIENT as isize));
    let raw = std::slice::from_raw_parts(bits as *const u8, (width * height * 4) as usize);
    let cue_pixels = raw.chunks_exact(4).filter(|p| (p[0] as i32-196).abs()<8 && (p[1] as i32-184).abs()<8 && (p[2] as i32-168).abs()<8).count();
    assert!(cue_pixels>20, "Native empty search field did not paint its cue: {cue_pixels}");
    let mut rgba = Vec::with_capacity(raw.len());
    for p in raw.chunks_exact(4) { rgba.extend([p[2],p[1],p[0],255]); }
    image::save_buffer(path, &rgba, width as u32, height as u32, image::ColorType::Rgba8)?;
    let _ = SetWindowTextW(hwnd, w!("typed"));
    FillRect(dc, &bounds, brush);
    paint_search_cue(hwnd, dc, kind);
    let raw = std::slice::from_raw_parts(bits as *const u8, (width * height * 4) as usize);
    assert!(raw.chunks_exact(4).all(|p| p[0]==37 && p[1]==33 && p[2]==28), "Cue must disappear after typing");
    let _ = SetWindowTextW(hwnd, PCWSTR(wide(&original).as_ptr()));
    SelectObject(dc, previous);
    let _ = DeleteObject(bitmap);
    let _ = DeleteObject(brush);
    let _ = DeleteDC(dc);
    Ok(())
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
                recent_rows: 8,
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
            banked_reset: Some(now() + 8300.),
            banked_available: Some(3),
            banked_expiring: Some(1),
            banked_expiries: vec![Some(now()+8300.), Some(now()+90000.), Some(now()+170000.)],
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
                prompt: "请检查图像采集链路，并补充连续帧仿真。".into(),
                tags: vec![tags::Tag{key:"project:demo".into(),label:"Demo project".into(),kind:tags::Kind::Project,color:0},tags::Tag{key:"group:IP".into(),label:"IP".into(),kind:tags::Kind::Group,color:0}],
                state: if i < 3 { "Busy".into() } else { "Done".into() },
                execution: crate::telemetry::Execution { model: "gpt-6.1-sol".into(), effort: "high".into(), service_tier: (i == 0).then(|| "priority".into()) },
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
        }.into();
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
        assert_eq!(app.fixed_hit_at(TAG_SIDEBAR + 340., 25.), Some(Hit::Body));
        let _ = app.hit(Hit::Body);
        assert!(app.search_body);
        assert!(app.body_service.is_none());
        let _ = app.hit(Hit::Body);
        assert!(!app.search_body);
        assert_eq!(app.rows.len(), 40);
        assert_eq!(app.popup_size, [1276, RECENT_FRAME + 8 * RECENT_ROW]);
        app.search = "huihuamingcheng 25".into();
        app.filter_fixed();
        assert_eq!(app.chat_filters, [24]);
        app.search.clear();
        app.filter_fixed();
        app.move_fixed_cursor(24);
        assert_eq!(app.scroll, 17);
        assert_eq!(
            app.fixed_hit_at(TAG_SIDEBAR + 80., (RECENT_TOP + 7 * RECENT_ROW + 10) as f32),
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
        Arc::make_mut(&mut state.lock().unwrap().recent).rows.reverse();
        for _ in 0..100 {
            app.tick();
        }
        assert_eq!(app.popup_kind, Some(Popup::Recent));
        assert_eq!(app.scroll, 17);
        assert_eq!(
            app.rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
            original
        );
        assert!(app.begin_chat_scroll(app.popup_size[0] as f32-10., 382.));
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
        Arc::make_mut(&mut state.lock().unwrap().recent).rows = rows.clone();
        app.close_popup();
        let snap = data::Snapshot {
            id: "fixture-0".into(),
            label: "🧪 xxx 会话名称 · 中文清晰显示".into(),
            tokens_used: Some(27190000),
            model: "gpt-6.1-sol".into(),
            execution: crate::telemetry::Execution { model: "gpt-6.1-sol".into(), effort: "high".into(), service_tier: Some("priority".into()) },
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
        assert_eq!(app.bar_text(&app.snapshot()).2, "● 3");
        let saved_update=state.lock().unwrap().updater.clone();
        assert!(!bar_hits.iter().any(|area|matches!(area.hit,Hit::UpdateAvailable(_))));
        state.lock().unwrap().updater=crate::update::View{available_version:"99.1.0".into(),..Default::default()};
        let update_width=app.measure_bar() as u32;
        assert_eq!(update_width,w+29,"An update adds only one compact icon slot");
        let update_hits=capture(&app,None,update_width,34,&paths.runtime.join("rust-update-available-verification.png"))?;
        let update_area=update_hits.iter().find(|area|matches!(&area.hit,Hit::UpdateAvailable(text) if text.contains("99.1.0"))).expect("A discovered update must have a clickable icon");
        assert!(update_area.r.right<=update_hits.iter().find(|area|area.hit==Hit::Details).unwrap().r.left);
        app.hover=Some(update_area.hit.clone());
        capture(&app,None,update_width,34,&paths.runtime.join("rust-update-available-hover-verification.png"))?;
        app.hover=None;
        state.lock().unwrap().updater=saved_update;
        let original=app.snapshot();
        for status in ["Done","Stopped"] {
            let mut idle=original.clone();
            idle.recent=Arc::new(data::Recent{rows:vec![Row{id:"fixture-0".into(),label:"🧪 xxx 会话名称 · 中文清晰显示".into(),state:status.into(),tokens:Some(241_700_000),..Default::default()}],..Default::default()});
            idle.snapshot=None;
            assert_eq!(app.bar_usage(&idle).0,Some(241_700_000));
            assert_eq!(app.bar_text(&idle).3,data::compact(Some(241_700_000)));
            idle.snapshot=original.snapshot.clone();
            idle.snapshot.as_mut().unwrap().tokens_used=None;
            assert_eq!(app.bar_usage(&idle).0,Some(241_700_000));
            idle.view=Some("different-chat".into());
            assert_eq!(app.bar_usage(&idle).0,None,"Never show another chat's token count");
            idle.view=None;idle.view_label="xxx 会话名称 · 中文清晰显示".into();
            assert_eq!(app.bar_usage(&idle).0,Some(241_700_000));
            Arc::make_mut(&mut idle.recent).rows.push(Row{id:"duplicate-title".into(),label:"xxx 会话名称 · 中文清晰显示".into(),tokens:Some(1),..Default::default()});
            assert_eq!(app.bar_usage(&idle).0,None,"Ambiguous visible titles cannot select a token total");
        }
        assert!(bar_hits.iter().any(|area| matches!(&area.hit, Hit::Info(text) if text.contains("gpt-6.1-sol") && text.contains("Fast: On"))));
        assert!(bar_hits.iter().any(|area| matches!(&area.hit, Hit::Info(text) if text.contains("Quota reset:") && text.contains("Time remaining:"))));
        assert!(bar_hits.iter().any(|area| matches!(&area.hit, Hit::Info(text) if text.contains("Banked reset"))));
        for (name, marker) in [("model", "gpt-6.1-sol"), ("quota", "Quota reset:")] {
            let info = bar_hits.iter().find(|area| matches!(&area.hit, Hit::Info(text) if text.contains(marker))).unwrap();
            app.hover = Some(info.hit.clone());
            let path = paths.runtime.join(format!("rust-bar-hover-{name}-verification.png"));
            capture(&app, None, w, 34, &path)?;
            let before = image::open(paths.runtime.join("rust-bar-verification.png"))?.to_rgba8();
            let after = image::open(&path)?.to_rgba8();
            for y in info.r.top.ceil() as u32..info.r.bottom.floor() as u32 {
                for x in info.r.left.ceil() as u32..info.r.right.floor() as u32 {
                    assert_eq!(before.get_pixel(x,y), after.get_pixel(x,y), "Hover must preserve {name} foreground");
                }
            }
        }
        app.hover = None;
        app.sync_tips(app.bar, &bar_hits, 1.);
        let tip_window = app.tips.iter().find(|t| t.owner == app.bar).unwrap().window;
        let mut updated_hits = bar_hits.clone();
        for area in &mut updated_hits { if let Hit::Info(text) = &mut area.hit { text.push_str("\nupdated"); } }
        app.sync_tips(app.bar, &updated_hits, 1.);
        assert_eq!(app.tips.iter().find(|t| t.owner == app.bar).unwrap().window, tip_window, "Telemetry must not recreate tooltips");
        state.lock().unwrap().quota[0].banked_reset = None;
        assert!(app.measure_bar() < w as i32);
        let no_banked_hits = capture(&app, None, app.measure_bar() as u32, 34, &paths.runtime.join("rust-bar-no-banked-verification.png"))?;
        assert!(!no_banked_hits.iter().any(|area| matches!(&area.hit, Hit::Info(text) if text.contains("Banked reset"))));
        state.lock().unwrap().quota[0].banked_reset = Some(now() + 8300.);
        let saved_notice=state.lock().unwrap().reset_notice.clone();
        let fixture_notice=crate::reset_notice::View{
            checked_at:now(),source:crate::reset_notice::Source::Divin,
            current:Some(crate::reset_notice::Notice{id:"fixture-reset".into(),at:now()+5400.,announced_at:now()-900.,
                bound:crate::reset_notice::Bound::Before,expression:"Before the announced deadline".into(),
                posts:vec![crate::reset_notice::Post{author:"Tibo (example)".into(),original:"A reset is scheduled before the published deadline.".into(),
                    translation:"示例：额度将在公布的截止时间前重置。".into(),url:"https://x.com/thsottiaux/status/123456789".into(),..Default::default()}],..Default::default()}),
            history:vec![crate::reset_notice::Notice{id:"previous-fixture".into(),announced_at:now()-86400.,summary:"Previous reset example".into(),completed:true,..Default::default()}],..Default::default()};
        state.lock().unwrap().reset_notice=Arc::new(fixture_notice.clone());
        let notice_width=app.measure_bar() as u32;
        let notice_hits=capture(&app,None,notice_width,34,&paths.runtime.join("rust-reset-notice-verification.png"))?;
        let notice_area=notice_hits.iter().find(|area|matches!(area.hit,Hit::ResetNotice(_))).expect("Explicit forecast must have a clickable alarm badge");
        let bank_area=notice_hits.iter().find(|area|matches!(&area.hit,Hit::Info(text) if text.contains("Banked reset"))).unwrap();
        assert!(notice_area.r.left>=bank_area.r.right,"Announcement belongs after the banked-expiry area");
        assert!(matches!(&notice_area.hit,Hit::ResetNotice(text) if text.contains("Original")&&text.contains("示例")&&text.contains("Previous reset example")));
        app.hover=Some(notice_area.hit.clone());
        capture(&app,None,notice_width,34,&paths.runtime.join("rust-reset-notice-hover-verification.png"))?;
        let plain_notice=image::open(paths.runtime.join("rust-reset-notice-verification.png"))?.to_rgba8();
        let hover_notice=image::open(paths.runtime.join("rust-reset-notice-hover-verification.png"))?.to_rgba8();
        for y in 6..28 {for x in (notice_area.r.left as u32+3)..(notice_area.r.left as u32+26) {
            assert_eq!(plain_notice.get_pixel(x,y),hover_notice.get_pixel(x,y),"Alarm foreground must remain visible on hover");
        }}
        app.hit(notice_area.hit.clone());
        assert_eq!(app.snapshot().reset_notice.current.as_ref().unwrap().id,"fixture-reset");
        app.hover=None;
        let notice_before=fixture_notice.current.as_ref().unwrap().at;
        assert!(fixture_notice.current.as_ref().unwrap().short(notice_before+1.,true).contains("待确认"));
        {
            let mut shared=state.lock().unwrap();let notice=Arc::make_mut(&mut shared.reset_notice);
            let mut completed=notice.current.take().unwrap();completed.completed=true;completed.at=now()-120.;
            completed.posts[0].observed=true;notice.history.insert(0,completed);
        }
        let completed_notice_hits=capture(&app,None,app.measure_bar() as u32,34,&paths.runtime.join("rust-reset-notice-completed-verification.png"))?;
        let green=completed_notice_hits.iter().find(|area|matches!(area.hit,Hit::ResetNotice(_))).expect("Recent confirmed reset must remain visible");
        assert!(green.r.right-green.r.left>35.);
        assert_eq!(app.snapshot().reset_notice.badge(now(),true).0,crate::reset_notice::Bell::Reset);
        state.lock().unwrap().reset_notice=Arc::new(fixture_notice.clone());
        Arc::make_mut(&mut state.lock().unwrap().reset_notice).current.as_mut().unwrap().at=now()-1.;
        let expired_notice_hits=capture(&app,None,app.measure_bar() as u32,34,&paths.runtime.join("rust-reset-notice-expired-verification.png"))?;
        let expired=expired_notice_hits.iter().find(|area|matches!(area.hit,Hit::ResetNotice(_))).unwrap();
        assert!(expired.r.right-expired.r.left<35.,"Expired predictions show no countdown label");
        state.lock().unwrap().reset_notice=saved_notice;
        capture_docking(&app, paths)?;
        app.popup_kind = Some(Popup::Recent);
        app.rows = rows.clone();
        app.search.clear();
        app.filter_fixed();
        capture(
            &app,
            Some(Popup::Recent),
            1276,
            (RECENT_FRAME + 8 * RECENT_ROW) as u32,
            &paths.runtime.join("rust-recent-verification.png"),
        )?;
        assert_eq!(
            app.fixed_hit_at(TAG_SIDEBAR + 30., 81.),
            Some(Hit::ChatIcon(rows[0].id.clone()))
        );
        assert_eq!(
            app.fixed_hit_at(TAG_SIDEBAR + 80., 81.),
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
            1276,
            (RECENT_FRAME + 8 * RECENT_ROW) as u32,
            &paths.runtime.join("rust-background-index-verification.png"),
        )?;
        app.index_pending = false;
        app.body_state = body::Status::default();
        app.search = "mingcheng 2".into();
        app.filter_fixed();
        capture(
            &app,
            Some(Popup::Recent),
            1276,
            (RECENT_FRAME + 8 * RECENT_ROW) as u32,
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
            1276,
            (RECENT_FRAME + 8 * RECENT_ROW) as u32,
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
            live.guard.note = "0 Git · 0 MiB · ended 128".into();
            live.guard.terminated = 128;
            live.guard.storm_count = Some(2);
            live.guard.tracking_since = "2026-10-20T19:00:00+08:00".into();
        }
        assert!(app.snapshot().guard.enabled);
        assert!(app.hit(Hit::Guard).is_none());
        assert!(!app.snapshot().guard.enabled);
        assert!(app.hit(Hit::Guard).is_none());
        assert!(app.snapshot().guard.enabled);
        {
            let mut live = state.lock().unwrap();
            live.guard.available = true;
            live.guard.note = "0 Git · 0 MiB · ended 128".into();
        }
        let detail_hits = capture(
            &app,
            Some(Popup::Details),
            560,
            DETAILS_HEIGHT as u32,
            &paths.runtime.join("rust-details-verification.png")
        )?;
        assert!(detail_hits.iter().any(|a| a.hit == Hit::ChooseFixed));
        assert!(!detail_hits.iter().any(|a| a.hit == Hit::Guard));
        let before = app.snapshot();
        app.last_identity = app.identity(&before);
        app.popup(Popup::Fixed)?;
        let order = app.rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>();
        Arc::make_mut(&mut state.lock().unwrap().recent).all_rows.reverse();
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
        Arc::make_mut(&mut state.lock().unwrap().recent).all_rows = rows.clone();
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
        assert!(app.hit(Hit::Preferences).is_none());
        assert_eq!(app.popup_kind, Some(Popup::Preferences));
        let original_rows = app.settings.recent_rows;
        app.hit(Hit::Rows(2));
        assert_eq!(app.settings.recent_rows, original_rows);
        app.close_popup();
        app.hit(Hit::Preferences);
        assert_eq!(app.preferences_draft.as_ref().unwrap().recent_rows, original_rows);
        app.hit(Hit::ResetPreferences);
        SendMessageW(app.edit, HKM_SETHOTKEY, WPARAM(b'K' as usize), LPARAM(0));
        app.hit(Hit::ApplyPreferences);
        assert!(!app.preferences_note.starts_with("Saved"));
        assert_eq!(app.settings.recent_rows, original_rows);
        let new_shortcut = Shortcut { key: b'K' as u16, ..Shortcut::default() };
        SendMessageW(app.edit, HKM_SETHOTKEY, WPARAM(new_shortcut.control_word() as usize), LPARAM(0));
        app.hit(Hit::ApplyPreferences);
        assert!(app.preferences_note.starts_with("Saved"));
        assert_eq!(app.settings.recent_rows, 10);
        assert_eq!(app.settings.recent_shortcut.label(), "Ctrl+Alt+K");
        app.popup(Popup::Recent)?;
        assert_eq!(app.popup_size, [1276, RECENT_FRAME + 10 * RECENT_ROW]);
        assert_eq!(app.list_page_rows(), 10);
        assert_eq!(app.fixed_hit_at(TAG_SIDEBAR + 80., (RECENT_TOP + 9 * RECENT_ROW + 10) as f32), Some(Hit::Recent(rows[9].id.clone())));
        assert!(app.begin_chat_scroll(app.popup_size[0] as f32-10., 450.));
        app.drag_chat_scroll(10000.);
        assert_eq!(app.scroll, 30);
        app.chat_scroll_drag = None;
        let _ = ReleaseCapture();
        app.scroll = 0;
        app.chat_cursor = 0;
        capture(&app, Some(Popup::Recent), 1276, (RECENT_FRAME + 10 * RECENT_ROW) as u32, &paths.runtime.join("rust-recent-configured-verification.png"))?;
        app.popup(Popup::Preferences)?;
        app.hit(Hit::ShortcutEnabled);
        app.hit(Hit::ApplyPreferences);
        assert!(!app.settings.recent_shortcut.enabled);
        app.hit(Hit::ResetPreferences);
        app.hit(Hit::ApplyPreferences);
        assert_eq!(app.settings.recent_shortcut, Shortcut::default());
        let preferences_hits = capture(&app, Some(Popup::Preferences), 560, PREFERENCES_HEIGHT as u32,
            &paths.runtime.join("rust-preferences-verification.png"))?;
        assert!(preferences_hits.iter().any(|a| a.hit == Hit::Guard));
        assert!(preferences_hits.iter().any(|a|a.hit==Hit::ResetNoticeEnabled));
        assert!(preferences_hits.iter().any(|a|a.hit==Hit::CapacityRetryEnabled));
        assert_eq!(preferences_hits.iter().find(|a|a.contains(418.,590.)).map(|a|&a.hit),Some(&Hit::CapacityRetryLogs));
        assert_eq!(preferences_hits.iter().find(|a|a.contains(460.,590.)).map(|a|&a.hit),Some(&Hit::CapacityRetryLogs));
        let home=preferences_hits.iter().find(|a|a.hit==Hit::ProjectHome).expect("Settings must link the full product name and version to GitHub");
        assert!(home.r.top<35. && home.r.left>200. && home.r.right<=560.);
        let saved_capacity=app.settings.capacity_retry.clone();
        assert_eq!(saved_capacity.delay_secs,5);
        app.hit(Hit::CapacityRetryEnabled);assert!(app.settings.capacity_retry.enabled);
        app.hit(Hit::CapacityRetryInterval(1));assert_eq!(app.settings.capacity_retry.delay_secs,6);
        assert_eq!(app.snapshot().capacity_options.delay_secs,6);
        let saved_capacity_culture=crate::locale::status().0;crate::locale::follow(0x804);
        capture(&app,Some(Popup::Preferences),560,PREFERENCES_HEIGHT as u32,&paths.runtime.join("rust-capacity-retry-settings-verification.png"))?;
        app.hover=Some(Hit::CapacityRetryEnabled);
        capture(&app,Some(Popup::Preferences),560,PREFERENCES_HEIGHT as u32,&paths.runtime.join("rust-capacity-retry-hover-verification.png"))?;
        app.hover=None;
        let normal=image::open(paths.runtime.join("rust-capacity-retry-settings-verification.png"))?.to_rgb8();
        let hovered=image::open(paths.runtime.join("rust-capacity-retry-hover-verification.png"))?.to_rgb8();
        let bright=|image:&image::RgbImage|->usize{(578..603).flat_map(|y|(27..430).map(move|x|(x,y))).filter(|&(x,y)|image.get_pixel(x,y).0.iter().all(|channel|*channel>180)).count()};
        let foreground=bright(&normal);
        assert!(foreground>30 && bright(&hovered)*100>=foreground*90,"Hover background must preserve the extension title foreground");
        let saved_retry_view=state.lock().unwrap().capacity_retry.clone();
        state.lock().unwrap().capacity_retry=crate::capacity_retry::View{attempts:3,submitted:2,unconfirmed:1,tracking_since:now()-3600.,available:true,..Default::default()};
        capture(&app,Some(Popup::Preferences),560,PREFERENCES_HEIGHT as u32,&paths.runtime.join("rust-capacity-retry-counter-verification.png"))?;
        let saved_log_entries=app.guard_events.clone();
        app.guard_events=vec![guard::LogEntry{time:"10-26 19:00:00".into(),title:"已提交继续消息 #3 · 示例会话".into(),detail:"会话 demo-chat · 失败回合 demo-turn · 等待 5 秒".into(),warning:false}];
        let enabled_before=app.settings.capacity_retry.enabled;
        app.hit(Hit::CapacityRetryLogs);
        assert!(app.capacity_log&&app.popup_kind==Some(Popup::GuardLogs));
        assert_eq!(app.settings.capacity_retry.enabled,enabled_before,"Log buttons must not toggle the extension");
        let log_hits=capture(&app,Some(Popup::GuardLogs),640,496,&paths.runtime.join("rust-capacity-retry-log-verification.png"))?;
        assert!(log_hits.iter().any(|area|area.hit==Hit::CapacityRetryRefresh));
        assert!(log_hits.iter().any(|area|matches!(&area.hit,Hit::Info(text) if text.contains("demo-chat")&&text.contains("demo-turn"))));
        app.hit(Hit::BackPreferences);
        assert_eq!(app.popup_kind,Some(Popup::Preferences));
        assert_eq!(app.settings.capacity_retry.enabled,enabled_before);
        app.guard_events=saved_log_entries;
        state.lock().unwrap().capacity_retry=saved_retry_view;
        crate::locale::follow(saved_capacity_culture);app.set_capacity_retry(saved_capacity)?;
        assert_eq!(SendMessageW(app.combo,CB_GETCOUNT,WPARAM(0),LPARAM(0)).0,2);
        let saved_notice_config=app.settings.reset_notice.clone();
        SendMessageW(app.combo,CB_SETCURSEL,WPARAM(0),LPARAM(0));app.change_notice_source();
        assert_eq!(app.settings.reset_notice.source,crate::reset_notice::Source::Divin);
        SendMessageW(app.combo,CB_SETCURSEL,WPARAM(1),LPARAM(0));app.change_notice_source();
        assert_eq!(app.settings.reset_notice.source,crate::reset_notice::Source::CodexResets);
        assert_eq!(app.snapshot().reset_notice.source,crate::reset_notice::Source::CodexResets);
        let saved_culture=crate::locale::status().0;
        crate::locale::follow(0x804);app.tick();
        assert_eq!(SendMessageW(app.combo,CB_GETCOUNT,WPARAM(0),LPARAM(0)).0,2,"Language refresh must not fill source combo with Emoji categories");
        assert_eq!(SendMessageW(app.combo,CB_GETCURSEL,WPARAM(0),LPARAM(0)).0,1);
        capture(&app,Some(Popup::Preferences),560,PREFERENCES_HEIGHT as u32,&paths.runtime.join("rust-reset-source-settings-zh-verification.png"))?;
        crate::locale::follow(saved_culture);app.tick();
        app.hit(Hit::ResetNoticeEnabled);assert!(!app.settings.reset_notice.enabled);
        app.hit(Hit::ResetNoticeInterval(1));assert_eq!(app.settings.reset_notice.check_minutes,6);
        app.hit(Hit::GuardSettings);
        assert_eq!(GetWindowLongPtrW(app.combo,GWL_STYLE)&WS_VISIBLE.0 as isize,0);
        app.hit(Hit::RefreshBack);
        assert_ne!(GetWindowLongPtrW(app.combo,GWL_STYLE)&WS_VISIBLE.0 as isize,0);
        app.set_reset_notice(saved_notice_config)?;
        SendMessageW(app.combo,CB_SETCURSEL,WPARAM(usize::from(app.settings.reset_notice.source==crate::reset_notice::Source::CodexResets)),LPARAM(0));
        let update_button=preferences_hits.iter().find(|a|a.hit==Hit::CheckUpdate).expect("Manual update check must be visible in settings");
        assert!(update_button.contains(95.,57.));
        let previous_update=state.lock().unwrap().updater.clone();
        state.lock().unwrap().updater=crate::update::View{last_check_at:Some(now()),last_result:"up_to_date".into(),last_detail:"v0.2.1".into(),..Default::default()};
        app.hit(Hit::CheckUpdate);
        assert_eq!(state.lock().unwrap().updater.phase,"checking");
        let checking_hits=capture(&app,Some(Popup::Preferences),560,PREFERENCES_HEIGHT as u32,&paths.runtime.join("rust-update-checking-verification.png"))?;
        assert!(!checking_hits.iter().any(|a|a.hit==Hit::CheckUpdate));
        state.lock().unwrap().updater.phase="up_to_date".into();
        let completed_hits=capture(&app,Some(Popup::Preferences),560,PREFERENCES_HEIGHT as u32,&paths.runtime.join("rust-update-result-verification.png"))?;
        assert!(completed_hits.iter().any(|a|a.hit==Hit::CheckUpdate));
        assert!(completed_hits.iter().any(|a|matches!(&a.hit,Hit::Info(text) if text.contains("v0.2.1"))));
        state.lock().unwrap().updater=previous_update;
        assert!(preferences_hits.iter().any(|a| a.hit == Hit::ApplyPreferences));
        assert!(preferences_hits.iter().any(|a| a.hit == Hit::GuardLogs && a.contains(451., 405.)));
        assert!(!preferences_hits.iter().any(|a| a.hit == Hit::Guard && a.contains(451., 405.)));
        app.guard_events = vec![
            guard::LogEntry { time:"10-20 20:55:40".into(), title:"Storm recovered".into(), detail:"Below limits for 60 seconds".into(), warning:false },
            guard::LogEntry { time:"10-20 20:54:40".into(), title:"Git ended · PID 12340".into(), detail:"terminated".into(), warning:false },
            guard::LogEntry { time:"10-20 20:54:40".into(), title:"Ending read-only Git · PID 12340".into(), detail:"diff · 35 MiB · Codex PID 12300".into(), warning:false },
            guard::LogEntry { time:"10-20 20:46:41".into(), title:"Git storm #2 detected".into(), detail:"100 Git · 3400 MiB · protection activated".into(), warning:true },
        ];
        app.hit(Hit::Rows(1));
        let guard_enabled = app.snapshot().guard.enabled;
        app.hit(Hit::GuardLogs);
        assert_eq!(app.popup_kind, Some(Popup::GuardLogs));
        assert_eq!(app.snapshot().guard.enabled, guard_enabled);
        capture(&app, Some(Popup::GuardLogs), 640, 496, &paths.runtime.join("rust-guard-log-verification.png"))?;
        app.hit(Hit::BackPreferences);
        assert_eq!(app.popup_kind, Some(Popup::Preferences));
        assert_eq!(app.preferences_draft.as_ref().unwrap().recent_rows, 11);
        assert!(app.hits.iter().any(|a| a.hit == Hit::Preferences));
        assert!(!app.hits.iter().any(|a| a.hit == Hit::Hide));
        // Exercise real Windows registration and persistence only in an isolated
        // fixture. Existing app shortcuts and user settings remain untouched.
        let original_paths = app.paths.clone();
        let original_settings = app.settings.clone();
        let fixture = paths.runtime.join(format!("preferences-native-{}", std::process::id()));
        anyhow::ensure!(fixture.starts_with(&paths.runtime), "Invalid preference fixture");
        std::fs::create_dir_all(&fixture)?;
        app.paths.app = fixture.clone();
        app.paths.runtime = fixture.clone();
        app.testing = false;
        state.lock().unwrap().host=GetForegroundWindow().0 as isize;
        app.shortcut_registered = false;
        app.settings.recent_shortcut.enabled = false;
        let native_shortcut = Shortcut { key: 0x87, shift: true, ..Shortcut::default() };
        app.preferences_draft = Some(original_settings.clone());
        SendMessageW(app.edit, HKM_SETHOTKEY, WPARAM(native_shortcut.control_word() as usize), LPARAM(0));
        app.apply_preferences()?;
        assert!(app.shortcut_registered);
        assert_eq!(app.settings.recent_shortcut, native_shortcut);
        let blocked = Shortcut { key: 0x86, ..native_shortcut.clone() };
        app.register_shortcut(RECENT_HOTKEY + 2, &blocked)?;
        SendMessageW(app.edit, HKM_SETHOTKEY, WPARAM(blocked.control_word() as usize), LPARAM(0));
        assert!(app.apply_preferences().is_err());
        assert_eq!(app.settings.recent_shortcut, native_shortcut);
        assert!(app.register_shortcut(RECENT_HOTKEY + 3, &native_shortcut).is_err());
        let persisted: Settings = serde_json::from_value(crate::read_json(&fixture.join("settings.json"), 16384))?;
        assert_eq!(persisted.recent_shortcut, native_shortcut);
        assert_eq!(persisted.recent_rows, 10);
        let _ = UnregisterHotKey(app.bar, RECENT_HOTKEY + 2);
        app.preferences_draft.as_mut().unwrap().recent_shortcut.enabled = false;
        app.apply_preferences()?;
        assert!(!app.shortcut_registered);
        app.testing = true;
        app.paths = original_paths;
        app.settings = original_settings;
        app.shortcut_id = RECENT_HOTKEY;
        paths.write("rust-preferences-test.json", &json!({"apply_and_discard":true,"invalid_shortcut_preserves_settings":true,
            "shortcut_enable_disable":true,"native_hotkey_conflict_preserves_previous_registration_and_file":true,
            "default_recent_rows":10,"configured_scroll_last_row":true,"extensions_only_in_settings":true,
            "guard_log_button_does_not_toggle_protection":true,"guard_log_preserves_unsaved_settings":true}));
        paths.write("rust-popover-shortcut-test.json", &json!({"list_window_preserved":true,"list_search_scroll_selection_preserved":true,"native_icon_click_routed":true,"icon_combo_survives_100_refreshes":true,"recent_shortcut_toggle":true,"index_percent_local_only":true,"body_snippet_highlight":true}));
        verify_recent_tags(&mut app,paths)?;
        verify_filter_controls(&mut app,paths)?;
        verify_memory_cache(&mut app,paths)?;
        verify_two_columns(&mut app,paths)?;
        verify_tag_overflow(&mut app,paths)?;
        verify_sidebar_animation(&mut app,paths)?;
        verify_refresh_preferences(&mut app,paths)?;
        verify_chat_menu_and_language(&mut app,paths)?;
        verify_guard_preferences(&mut app,paths)?;
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

include!("recent.rs");

include!("context_menu.rs");

include!("guard_preferences.rs");
