use crate::{
    data::{self, Catalog, Row},
    desktop::{Action, Workers},
    host, now, Paths, Settings, State, Stop,
};
use anyhow::Result;
use rand::seq::IteratorRandom;
use serde_json::json;
use std::{
    cell::RefCell,
    collections::HashMap,
    mem::size_of,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};
use windows::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::*,
        Graphics::{Direct2D::Common::*, Direct2D::*, DirectWrite::*, Dxgi::Common::*, Gdi::*},
        System::{LibraryLoader::*, Threading::*},
        UI::{Controls::*, HiDpi::*, Input::KeyboardAndMouse::*, Shell::*, WindowsAndMessaging::*},
    },
};

const CLASS: PCWSTR = w!("CodexSessionBarRust");
const TRAY_MESSAGE: u32 = WM_APP + 1;
const QUIT_MESSAGE: u32 = WM_APP + 2;
const HIDE_MESSAGE: u32 = WM_APP + 3;
const BG: u32 = 0x23272b;
const PANEL: u32 = 0x242a2e;
const TEXT: u32 = 0xe8eef2;
const MUTED: u32 = 0xa8b8c4;
const ORANGE: u32 = 0xf5a34b;
const GREEN: u32 = 0x65d69b;
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
    Grip,
    Hide,
    Details,
    Recent(String),
    ChooseFixed,
    Fixed(String),
    Back,
    Random,
    Clear,
    Emoji(usize),
    Refresh,
    Dock,
    Follow,
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
    popup_kind: Option<Popup>,
    rows: Vec<Row>,
    hits: Vec<Area>,
    popup_hits: Vec<Area>,
    filters: Vec<usize>,
    chat_filters: Vec<usize>,
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
impl App {
    unsafe fn new(
        paths: Paths,
        settings: Settings,
        state: State,
        stop: Stop,
        testing: bool,
    ) -> Result<Self> {
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
            popup_kind: None,
            popup_layout: None,
            rows: Vec::new(),
            hits: Vec::new(),
            popup_hits: Vec::new(),
            filters,
            chat_filters: Vec::new(),
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
        let title = current
            .map(|s| self.catalog.strip(&s.label).to_string())
            .unwrap_or_else(|| {
                if self.identity(live).is_empty() {
                    "Recent chats".into()
                } else {
                    "Switching…".into()
                }
            });
        let icon = current
            .map(|s| self.catalog.icon(&s.label))
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
                x += tw + 6.;
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
                p.label("Recent chats", rect(14., 8., 112., 30.), 13., TEXT, false);
                let (tokens, rate, partial) = live.recent.totals();
                let total = format!(
                    "Total {}{} t/s  ·  {}{} TOK",
                    rate.map(|r| format!("{r:.1}")).unwrap_or("—".into()),
                    if partial && rate.is_some() { "*" } else { "" },
                    data::compact(tokens),
                    if partial && tokens.is_some() { "*" } else { "" }
                );
                p.label(&total, rect(130., 8., 400., 30.), 12., MUTED, false);
                area(Hit::Refresh, rect(width - 78., 9., 64., 28.));
                p.label("Refresh", rect(width - 74., 9., 60., 28.), 11., TEXT, false);
                for (index, row) in self.rows.iter().enumerate() {
                    let y = 46. + index as f32 * 43.;
                    let busy = row.state == "Busy";
                    let current = row.id == self.identity(&live);
                    let r = rect(12., y, width - 24., 39.);
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
                    if current {
                        p.border(r, 0x527b69, 7.);
                    }
                    area(Hit::Recent(row.id.clone()), r);
                    p.label(
                        &self.catalog.icon(&row.label),
                        rect(22., y + 2., 25., 34.),
                        16.,
                        TEXT,
                        true,
                    );
                    let label_width = if busy { width - 365. } else { width - 142. };
                    p.label(
                        self.catalog.strip(&row.label),
                        rect(49., y + 2., label_width, 34.),
                        13.,
                        TEXT,
                        false,
                    );
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
                            rect(width - 310., y + 2., 220., 34.),
                            11.,
                            MUTED,
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
                        } else {
                            MUTED
                        },
                        false,
                    );
                }
                if self.rows.is_empty() {
                    p.label(
                        if live.recent.error.is_empty() {
                            "Loading recent chats…"
                        } else {
                            "Cached list unavailable · refresh pending"
                        },
                        rect(16., 50., width - 32., 45.),
                        13.,
                        MUTED,
                        false,
                    );
                }
                p.label(
                    if live.recent.stale || now() - live.recent.loaded_at > 10. {
                        "Cached · Refresh pending"
                    } else {
                        "Order stays fixed while open · Busy first on reopen"
                    },
                    rect(16., height - 29., width - 32., 24.),
                    11.,
                    MUTED,
                    false,
                );
            }
            Some(Popup::Icons) => {
                area(Hit::Random, rect(12., 9., 36., 32.));
                p.label("🎲", rect(17., 9., 30., 32.), 20., TEXT, true);
                p.label("Session icon", rect(57., 9., 250., 32.), 14., TEXT, false);
                area(Hit::Clear, rect(width - 68., 10., 54., 30.));
                p.label("Clear", rect(width - 62., 10., 48., 30.), 12., MUTED, false);
                for r in [rect(14., 49., 190., 29.), rect(214., 49., 155., 29.)] {
                    p.fill(r, 0x1c2125, 3.);
                    p.border(r, 0x47535b, 3.);
                }
                p.label(
                    if self.search.is_empty() {
                        "Search emoji…"
                    } else {
                        &self.search
                    },
                    rect(21., 49., 180., 29.),
                    13.,
                    MUTED,
                    false,
                );
                p.label(&self.category, rect(222., 49., 131., 29.), 13., TEXT, false);
                p.label("⌄", rect(350., 49., 16., 29.), 13., MUTED, false);
                let columns = 10;
                let cell = 36.;
                let start = self.scroll * columns;
                for (slot, index) in self.filters.iter().skip(start).take(90).enumerate() {
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
                    &format!("{} icons · scroll for more", self.filters.len()),
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
                p.label(
                    "‹  Back",
                    rect(width - 68., 9., 54., 30.),
                    12.,
                    MUTED,
                    false,
                );
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
                    area(Hit::Fixed(row.id.clone()), r);
                    if selected || self.chat_cursor == self.scroll + slot {
                        p.border(r, if selected { 0x5ca985 } else { 0x59849c }, 7.);
                    }
                    p.fill(rect(23., y + 5., 30., 30.), 0x333c43, 7.);
                    p.label(
                        &self.catalog.icon(&row.label),
                        rect(26., y + 4., 28., 32.),
                        20.,
                        TEXT,
                        true,
                    );
                    let with_metrics = busy && row.tokens.is_some();
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
                            &format!(
                                "{} TOK  ·  {} t/s",
                                data::compact(row.tokens),
                                row.rate().map(|v| format!("{v:.1}")).unwrap_or("—".into())
                            ),
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
                    rect(16., 8., width - 32., 28.),
                    14.,
                    TEXT,
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
                for (hit, text, x, w) in [
                    (Hit::Refresh, "Refresh limits", 16., 122.),
                    (Hit::Dock, "Dock in title bar", 148., 140.),
                    (Hit::Quit, "Quit session bar", width - 151., 134.),
                ] {
                    area(hit, rect(x, 204., w, 30.));
                    p.border(rect(x, 204., w, 30.), 0x47535b, 5.);
                    p.label(text, rect(x + 8., 204., w - 12., 30.), 12., TEXT, false);
                }
                p.label(
                    &live.action_note,
                    rect(16., 241., width - 32., 28.),
                    11.,
                    if live.action_note.starts_with("Action failed") {
                        0xf87171
                    } else {
                        MUTED
                    },
                    false,
                );
                p.label(
                    "× hides to tray · Ask Codex: 打开会话条",
                    rect(16., 274., width - 32., 25.),
                    11.,
                    MUTED,
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
            if hwnd == self.bar {
                self.hits = hits;
            } else {
                self.popup_hits = hits;
            }
        }
        let _ = EndPaint(hwnd, &paint);
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
    unsafe fn close_popup(&mut self) {
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
        [self.combo].iter().any(|&combo| {
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
        self.hover = None;
        let live = self.snapshot();
        self.popup_size = match kind {
            Popup::Recent => {
                self.rows = live.recent.rows.clone();
                [650, ((self.rows.len().max(1) * 43) as i32) + 80]
            }
            Popup::Icons => [384, 450],
            Popup::Details => [560, 310],
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
                LPARAM(w!("Search emoji…").as_ptr() as isize),
            );
        }
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
            self.search.clear();
            self.scroll = 0;
            self.filter_fixed();
            if !live.auto_thread {
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
                55,
                506,
                24,
                self.popup,
                HMENU(103isize as *mut _),
                GetModuleHandleW(None)?,
                None,
            )?;
            let _ = SendMessageW(
                self.edit,
                EM_SETCUEBANNER,
                WPARAM(0),
                LPARAM(w!("Search chats…").as_ptr() as isize),
            );
        }
        let previous_font = self.edit_font;
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
            if matches!(kind, Popup::Icons | Popup::Fixed) {
                let _ = SetFocus(self.edit);
            }
        }
        Ok(())
    }
    unsafe fn place_popup(&mut self) {
        // Token/TPS text can change the bar's width and anchor while the user
        // is choosing. Let native combo tracking finish before moving its panel.
        if [self.combo].iter().any(|&combo| {
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
        let live = self.snapshot();
        let current = self.current(&live);
        let (_, title, status, _, _) = unsafe { self.bar_text(&live) };
        self.paths.write(if self.testing{"rust-test-status.json"}else{"live-status.json"},&json!({"pid":std::process::id(),"runtime":"rust","updated_at":now(),"thread":self.identity(&live),"overview":self.identity(&live).is_empty(),"display_title":title,"display_status":status,"tokens":current.and_then(|s|s.tokens()),"auto_thread":live.auto_thread,"topmost":true,"user_hidden":self.hidden,"host_hidden":self.host_hidden,"host_window":live.host,"host_rect":self.last_host,"host_offset":self.settings.host_offset,"titlebar_slot":live.slot,"quota":live.quota,"quota_error":live.quota_error,"recent_cache":{"rows":live.recent.rows.len(),"busy_count":live.recent.busy_count,"stale":live.recent.stale,"error":live.recent.error},"active_view":{"thread":live.view,"label":live.view_label,"available":live.view.is_some(),"source":"uia-current-page"},"action_note":live.action_note,"font":{"chinese":self.painter.chinese,"latin":"Segoe UI","emoji":"Segoe UI Emoji","renderer":"DirectWrite / software Direct2D","antialias":"ClearType","per_monitor_dpi":true}}));
    }
    unsafe fn hit(&mut self, hit: Hit) -> Option<Action> {
        match hit {
            Hit::Title => {
                let _ = self.popup(Popup::Recent);
            }
            Hit::Icon => {
                if self.current(&self.snapshot()).is_some() {
                    let _ = self.popup(Popup::Icons);
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
            Hit::Random | Hit::Clear | Hit::Emoji(_) => {
                let live = self.snapshot();
                let Some(current) = self.current(&live) else {
                    return None;
                };
                let id = current.id.clone();
                let icon = match hit {
                    Hit::Clear => String::new(),
                    Hit::Emoji(index) => self.catalog.entries[index].emoji.clone(),
                    _ => {
                        let current_icon = self.catalog.icon(&current.label);
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
                self.close_popup();
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
            Hit::Quit => {
                self.quit = true;
                self.stop.store(true, Ordering::Relaxed);
                PostQuitMessage(0);
            }
            Hit::Grip => {}
        }
        None
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
        let query = self.search.trim().to_lowercase();
        self.chat_filters = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| query.is_empty() || row.label.to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect();
        self.chat_cursor = 0;
        self.scroll = 0;
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
        self.chat_filters
            .get(slot)
            .map(|&index| Hit::Fixed(self.rows[index].id.clone()))
    }
    unsafe fn fixed_key(&mut self, key: u32) -> bool {
        if self.popup_kind != Some(Popup::Fixed) {
            return false;
        }
        match key {
            k if k == VK_UP.0 as u32 => self.move_fixed_cursor(-1),
            k if k == VK_DOWN.0 as u32 => self.move_fixed_cursor(1),
            k if k == VK_RETURN.0 as u32 => {
                if let Some(&index) = self.chat_filters.get(self.chat_cursor) {
                    let _ = self.hit(Hit::Fixed(self.rows[index].id.clone()));
                }
            }
            k if k == VK_ESCAPE.0 as u32 => {
                let _ = self.popup(Popup::Details);
            }
            _ => return false,
        }
        let _ = InvalidateRect(self.popup, None, false);
        true
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
        let is_bar = hwnd == app.bar;
        let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.;
        let x = (lparam.0 as i16) as f32 / scale;
        let y = ((lparam.0 >> 16) as i16) as f32 / scale;
        match message {
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
                if let Some((start, original)) = app.drag {
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
                    let hover = areas
                        .iter()
                        .find(|a| a.contains(x, y))
                        .map(|a| a.hit.clone());
                    if app.hover != hover {
                        app.hover = hover;
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN => {
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
                if app.drag.take().is_some() {
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
                    let hit = if !is_bar && app.popup_kind == Some(Popup::Fixed) {
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
            WM_MOUSEWHEEL if !is_bar && app.popup_kind == Some(Popup::Fixed) => {
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
        let observers = host::start(&paths, &state, &stop);
        ACTIONS.with(|a| *a.borrow_mut() = Some(workers.actions.clone()));
        APP.with(|a| *a.borrow_mut() = Some(app));
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
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
                            app.close_popup();
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
        let rows: Vec<Row> = (0..10)
            .map(|i| Row {
                id: format!("fixture-{i}"),
                label: format!(
                    "{} xxx 会话名称 {}",
                    ["🧪", "🚧", "🚀", "💬", "🎯", "📦", "💻", "🧠", "🌈", "📷"][i],
                    i + 1
                ),
                state: if i < 3 { "Busy".into() } else { "Done".into() },
                tokens: Some(27190000),
                tps: Some(42.3),
                rate_at: Some(now()),
                started_at: Some(now() - 754.),
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
        assert_eq!(app.catalog.defaults.len(), 90);
        assert!(app.catalog.entries.len() > 3900);
        app.popup(Popup::Recent)?;
        let original = app.rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>();
        let mut changed = rows.clone();
        changed.reverse();
        app.rows = data::stable_rows(&app.rows, &changed);
        assert_eq!(
            app.rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
            original
        );
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
        app.popup_kind = Some(Popup::Recent);
        app.rows = rows.clone();
        capture(
            &app,
            Some(Popup::Recent),
            650,
            510,
            &paths.runtime.join("rust-recent-verification.png"),
        )?;
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
        assert!(capture(
            &app,
            Some(Popup::Details),
            560,
            310,
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
        assert_eq!(app.chat_filters.len(), 1);
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
        app.popup(Popup::Details)?;
        capture(
            &app,
            Some(Popup::Details),
            560,
            310,
            &paths.runtime.join("rust-details-verification.png"),
        )?;
        *state.lock().unwrap() = before;
        app.popup(Popup::Icons)?;
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
        let _ = app.hit(Hit::Hide);
        assert!(app.hidden);
        app.hidden = false;
        app.diagnostic();
        paths.write("rust-ui-test.json",&json!({"ui":"pass","runtime":"rust","emoji_count":app.catalog.entries.len(),"default_collection":90,"recent_rows":10,"startup_home_quota":true,"open_list_order_fixed":true,"random_icon_bound":true,"drag_grip_separate":true,"fixed_chat_picker_survives_100_refreshes":true,"fixed_chat_search":true,"fixed_chat_keyboard_scroll":true,"fixed_chat_color_icons":true,"emoji_dropdown_survives_100_refreshes":true,"fixed_chat_selection":true,"font_chinese":app.painter.chinese,"renderer":"DirectWrite + software Direct2D"}));
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
