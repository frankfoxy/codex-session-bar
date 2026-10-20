// Recent-list layout and tag interactions. Included in ui so all native hit tests
// share the same geometry as the renderer.
const TAG_SIDEBAR: f32 = 236.;
const TAG_CLOUD_TOP: f32 = RECENT_TOP as f32 + 64.;
const TAG_REGION_MAX: f32 = 128.;
struct TagCloudItem {
    index: usize,
    r: D2D_RECT_F,
}
struct TagRegion {
    items: Vec<TagCloudItem>,
    height: f32,
}

impl App {
    unsafe fn shortcut_allowed(&self) -> bool {
        self.shortcut_allowed_in(GetForegroundWindow())
    }
    unsafe fn shortcut_allowed_in(&self, fg: HWND) -> bool {
        if fg.is_invalid() {
            return false;
        }
        let root = GetAncestor(fg, GA_ROOT);
        if root == self.bar || self.owns_popup_window(fg) {
            return true;
        }
        let host = self.state.lock().unwrap().host;
        if host != 0 && root.0 as isize == host {
            return true;
        }
        // Only queried after foreground changes, not on every key or paint.
        host::visible_windows().contains(&(root.0 as isize))
    }
    unsafe fn sync_shortcut_scope(&mut self, force: bool) {
        let fg = GetForegroundWindow();
        self.sync_shortcut_for(fg, force);
    }
    unsafe fn sync_shortcut_for(&mut self, fg: HWND, force: bool) {
        if !force && fg == self.shortcut_foreground {
            return;
        }
        self.shortcut_foreground = fg;
        self.shortcut_context = self.shortcut_allowed_in(fg);
        if self.shortcut_context && self.settings.recent_shortcut.enabled {
            if !self.shortcut_registered {
                match self.register_shortcut(self.shortcut_id, &self.settings.recent_shortcut) {
                    Ok(()) => {
                        self.shortcut_registered = true;
                        self.shortcut_error.clear();
                    }
                    Err(e) => self.shortcut_error = e.to_string(),
                }
            }
        } else if self.shortcut_registered {
            let _ = UnregisterHotKey(self.bar, self.shortcut_id);
            self.shortcut_registered = false;
        }
    }
    fn recent_left(&self) -> f32 {
        if self.sidebar_open {
            TAG_SIDEBAR + 12.
        } else {
            12.
        }
    }
    fn recent_row_height(&self) -> i32 {
        if self.settings.compact_recent {
            43
        } else {
            RECENT_ROW
        }
    }
    unsafe fn recent_width(&self) -> i32 {
        let base = if self.settings.compact_recent {
            640
        } else {
            820
        };
        let requested = base
            + if self.sidebar_open {
                TAG_SIDEBAR as i32
            } else {
                0
            };
        if self.testing {
            return requested;
        }
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(
            MonitorFromWindow(self.bar, MONITOR_DEFAULTTONEAREST),
            &mut info,
        )
        .as_bool()
        {
            requested
                .min(((info.rcWork.right - info.rcWork.left) as f32 / self.dpi) as i32 - 16)
                .max(480)
        } else {
            requested
        }
    }
    fn refresh_tag_catalog(&mut self) {
        let next = self.custom_tags.catalog(&self.rows);
        if next != self.tag_catalog || self.tag_regions.len() != 3 {
            self.tag_catalog = next;
            self.tag_search.sync(
                self.tag_catalog
                    .iter()
                    .map(|(t, _)| (t.key.as_str(), t.label.as_str())),
            );
            unsafe {
                self.rebuild_tag_regions();
            }
        }
        self.tag_catalog_due = Instant::now() + std::time::Duration::from_secs(1);
    }
    fn tag_region_viewport(&self, index: usize) -> D2D_RECT_F {
        let available = (self.popup_size[1] as f32 - 44. - TAG_CLOUD_TOP).max(120.);
        let mut heights = [32.; 3];
        for (i, h) in heights.iter_mut().enumerate() {
            *h = self
                .tag_regions
                .get(i)
                .map(|r| r.height)
                .unwrap_or(0.)
                .clamp(32., TAG_REGION_MAX);
        }
        let budget = (available - 3. * 28.).max(36.);
        let sum: f32 = heights.iter().sum();
        if sum > budget {
            let floor = (budget / 3.).min(32.);
            let extra: f32 = heights.iter().map(|h| h - floor).sum();
            let ratio = ((budget - 3. * floor) / extra.max(1.)).clamp(0., 1.);
            for h in &mut heights {
                *h = floor + (*h - floor) * ratio;
            }
        }
        rect(
            12.,
            TAG_CLOUD_TOP + heights.iter().take(index).map(|h| h + 28.).sum::<f32>() + 24.,
            TAG_SIDEBAR - 24.,
            heights[index],
        )
    }
    fn tag_scroll_max(&self, index: usize) -> f32 {
        let r = self.tag_region_viewport(index);
        (self.tag_regions.get(index).map(|r| r.height).unwrap_or(0.) - (r.bottom - r.top)).max(0.)
    }
    unsafe fn rebuild_tag_regions(&mut self) {
        self.tag_regions.clear();
        for kind in [tags::Kind::Project, tags::Kind::Group, tags::Kind::Custom] {
            let mut items = Vec::new();
            let (mut x, mut y) = (12., 0.);
            for (index, (tag, count)) in self.tag_catalog.iter().enumerate() {
                if tag.kind != kind || !self.tag_search.matches(&tag.key, &self.tag_query) {
                    continue;
                }
                let w = (self.painter.width(&tag.label, 11.)
                    + self.painter.width(&count.to_string(), 9.)
                    + if tag.kind == tags::Kind::Custom {
                        34.
                    } else {
                        46.
                    })
                .clamp(48., TAG_SIDEBAR - 32.);
                if x + w > TAG_SIDEBAR - 12. && x > 12. {
                    x = 12.;
                    y += 34.;
                }
                items.push(TagCloudItem {
                    index,
                    r: rect(x, y, w, 28.),
                });
                x += w + 6.;
            }
            self.tag_regions.push(TagRegion {
                height: if items.is_empty() { 0. } else { y + 30. },
                items,
            });
        }
        for i in 0..3 {
            self.tag_scroll[i] = self.tag_scroll[i].min(self.tag_scroll_max(i));
        }
    }
    fn tag_region_at(&self, x: f32, y: f32) -> Option<usize> {
        if !self.sidebar_open || !(8. ..TAG_SIDEBAR).contains(&x) {
            return None;
        }
        (0..3).find(|&i| {
            let r = self.tag_region_viewport(i);
            y >= r.top - 24. && y < r.bottom
        })
    }
    fn tag_cloud_hit(&self, x: f32, y: f32) -> Option<Hit> {
        let i = self.tag_region_at(x, y)?;
        let viewport = self.tag_region_viewport(i);
        if y < viewport.top || x >= TAG_SIDEBAR - 10. {
            return None;
        }
        self.tag_regions
            .get(i)?
            .items
            .iter()
            .find(|item| {
                let top = viewport.top + item.r.top - self.tag_scroll[i];
                top >= viewport.top
                    && top + 28. <= viewport.bottom
                    && x >= item.r.left
                    && x < item.r.right
                    && y >= top
                    && y < top + 28.
            })
            .map(|item| Hit::TagFilter(self.tag_catalog[item.index].0.key.clone()))
    }
    fn scroll_tag_region(&mut self, index: usize, amount: f32) {
        self.tag_scroll[index] =
            (self.tag_scroll[index] + amount).clamp(0., self.tag_scroll_max(index));
    }
    fn tag_track_geometry(&self, index: usize) -> Option<(D2D_RECT_F, f32, f32)> {
        if self.tag_scroll_max(index) <= 0. {
            return None;
        }
        let r = self.tag_region_viewport(index);
        let h = r.bottom - r.top;
        let thumb = (h * h / self.tag_regions[index].height).max(14.).min(h);
        let offset = (h - thumb) * self.tag_scroll[index] / self.tag_scroll_max(index);
        Some((r, thumb, offset))
    }
    unsafe fn begin_tag_scroll(&mut self, x: f32, y: f32) -> bool {
        if self.tag_editor.is_some() || !(TAG_SIDEBAR - 10. ..TAG_SIDEBAR).contains(&x) {
            return false;
        }
        let Some(i) = self.tag_region_at(x, y) else {
            return false;
        };
        let Some((r, thumb, offset)) = self.tag_track_geometry(i) else {
            return false;
        };
        self.tag_scroll_drag = Some((
            i,
            if y >= r.top + offset && y <= r.top + offset + thumb {
                y - r.top - offset
            } else {
                thumb / 2.
            },
        ));
        let _ = SetCapture(self.popup);
        self.drag_tag_scroll(y);
        true
    }
    unsafe fn drag_tag_scroll(&mut self, y: f32) {
        let Some((i, grab)) = self.tag_scroll_drag else {
            return;
        };
        if let Some((r, thumb, _)) = self.tag_track_geometry(i) {
            let offset = (y - r.top - grab).clamp(0., (r.bottom - r.top - thumb).max(0.));
            self.tag_scroll[i] = if r.bottom - r.top > thumb {
                offset * self.tag_scroll_max(i) / (r.bottom - r.top - thumb)
            } else {
                0.
            };
            let _ = InvalidateRect(self.popup, None, false);
        }
    }
    unsafe fn close_tag_search(&mut self) {
        if !self.tag_search_input.is_invalid() {
            let _ = DestroyWindow(self.tag_search_input);
            self.tag_search_input = HWND::default();
        }
    }
    unsafe fn place_tag_search(&self) {
        if self.tag_search_input.is_invalid() {
            return;
        }
        let _ = MoveWindow(
            self.tag_search_input,
            (46. * self.dpi) as i32,
            ((RECENT_TOP as f32 + 9.) * self.dpi) as i32,
            ((TAG_SIDEBAR - 97.) * self.dpi) as i32,
            (22. * self.dpi) as i32,
            true,
        );
    }
    unsafe fn ensure_tag_search(&mut self) -> Result<()> {
        if self.popup_kind != Some(Popup::Recent) || !self.sidebar_open {
            if !self.tag_search_input.is_invalid() {
                if GetFocus() == self.tag_search_input {
                    let _ = SetFocus(self.edit);
                }
                let _ = ShowWindow(self.tag_search_input, SW_HIDE);
            }
            return Ok(());
        }
        if !self.testing && self.tag_search_input.is_invalid() {
            self.tag_search_input = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("EDIT"),
                PCWSTR(wide(&self.tag_query).as_ptr()),
                WS_CHILD | WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                0,
                0,
                0,
                0,
                self.popup,
                HMENU(106isize as *mut _),
                GetModuleHandleW(None)?,
                None,
            )?;
            SendMessageW(
                self.tag_search_input,
                WM_SETFONT,
                WPARAM(self.edit_font.0 as usize),
                LPARAM(1),
            );
            SendMessageW(
                self.tag_search_input,
                EM_SETCUEBANNER,
                WPARAM(1),
                LPARAM(w!("Tags / pinyin…").as_ptr() as isize),
            );
            let _ = SetWindowTheme(self.tag_search_input, w!("DarkMode_CFD"), PCWSTR::null());
        }
        self.place_tag_search();
        if !self.tag_search_input.is_invalid() {
            let _ = ShowWindow(self.tag_search_input, SW_SHOW);
        }
        Ok(())
    }
    unsafe fn tag_layout(&self, row: &Row) -> (f32, Vec<(tags::Tag, f32, f32)>, usize) {
        let width = self.popup_size[0] as f32;
        let tags = self.custom_tags.for_row(row);
        let budget = ((width - self.recent_left() - 65.) * 0.48).clamp(120., 380.);
        let mut used = 0.;
        let mut result = Vec::new();
        for tag in tags.iter().take(3) {
            let w = (self.painter.width(&tag.label, 10.)
                + if tag.kind == tags::Kind::Custom {
                    14.
                } else {
                    26.
                })
            .min(130.);
            if used + w > budget - 34. {
                break;
            }
            result.push((tag.clone(), 0., w));
            used += w + 5.;
        }
        let hidden = tags.len() - result.len();
        let extra = if hidden > 0 { 28. } else { 0. };
        let start = width - 51. - used - extra;
        let mut x = start;
        for (_, tx, w) in &mut result {
            *tx = x;
            x += *w + 5.;
        }
        (start, result, hidden)
    }
    unsafe fn paint_tag(&self, tag: &tags::Tag, x: f32, y: f32, w: f32, selected: bool) {
        let p = &self.painter;
        let (bg, fg) = tag.colors();
        p.fill(rect(x, y, w, 20.), bg, 4.);
        if selected {
            p.border(rect(x, y, w, 20.), fg, 4.);
        }
        let pad = if tag.kind != tags::Kind::Custom {
            p.border(rect(x + 6., y + 9., 6., 6.), fg, 1.);
            p.border(rect(x + 7., y + 5., 4., 6.), fg, 2.);
            18.
        } else {
            7.
        };
        p.label(
            &tag.label,
            rect(x + pad, y + 1., w - pad - 5., 18.),
            10.,
            fg,
            false,
        );
    }
    fn tag_editor_rect(&self) -> D2D_RECT_F {
        let w = (self.popup_size[0] as f32 - self.recent_left() - 16.).min(360.);
        let h = (self.popup_size[1] as f32 - RECENT_TOP as f32 - 45.).min(348.);
        rect(
            self.popup_size[0] as f32 - w - 18.,
            self.tag_editor_y
                .min(self.popup_size[1] as f32 - h - 42.)
                .max(RECENT_TOP as f32),
            w,
            h,
        )
    }
    fn contains_rect(r: D2D_RECT_F, x: f32, y: f32) -> bool {
        x >= r.left && x < r.right && y >= r.top && y < r.bottom
    }
    fn chat_clear_rect(&self) -> D2D_RECT_F {
        if self.popup_kind == Some(Popup::Recent) {
            rect(273., 13., 24., 24.)
        } else {
            rect(self.popup_size[0] as f32 - 47., 54., 24., 24.)
        }
    }
    fn tag_clear_rect() -> D2D_RECT_F {
        rect(TAG_SIDEBAR - 43., RECENT_TOP as f32 + 8., 24., 24.)
    }
    fn emoji_clear_rect() -> D2D_RECT_F {
        rect(176., 51.5, 24., 24.)
    }
    fn pointed_row(&self) -> Option<usize> {
        let (x, y) = self.list_pointer?;
        let (top, rh, left, right, page) = match self.popup_kind {
            Some(Popup::Recent) => (
                RECENT_TOP as f32,
                self.recent_row_height() as f32,
                self.recent_left(),
                self.popup_size[0] as f32 - 18.,
                self.list_page_rows(),
            ),
            Some(Popup::Fixed) => (100., 44., 16., self.popup_size[0] as f32 - 16., 8),
            _ => return None,
        };
        if (left..right).contains(&x)
            && (top..top + page as f32 * rh).contains(&y)
            && (y - top) % rh < rh - 4.
        {
            self.chat_filters
                .get(self.scroll + ((y - top) / rh) as usize)
                .copied()
        } else {
            None
        }
    }
    fn set_list_pointer(&mut self, p: Option<(f32, f32)>) -> bool {
        let old = self.pointed_row();
        self.list_pointer = p;
        old != self.pointed_row()
    }
    fn row_hovered(&self, id: &str) -> bool {
        if self.list_pointer.is_some() {
            return self.pointed_row().is_some_and(|i| self.rows[i].id == id);
        }
        matches!(&self.hover,Some(Hit::Recent(key)|Hit::Fixed(key)|Hit::ChatIcon(key)|Hit::TagEdit(key)) if key==id)
    }
    unsafe fn refresh_list_hover(&mut self) {
        let mut point = POINT::default();
        let _ = GetCursorPos(&mut point);
        let _ = ScreenToClient(self.popup, &mut point);
        let (x, y) = (point.x as f32 / self.dpi, point.y as f32 / self.dpi);
        self.list_pointer = Some((x, y));
        self.hover = self.fixed_hit_at(x, y);
    }
    unsafe fn draw_recent(&self, width: f32, height: f32) -> Vec<Area> {
        let p = &self.painter;
        let live = self.snapshot();
        let mut hits = Vec::new();
        let mut area = |hit, r| hits.push(Area { hit, r });
        area(Hit::TagSidebar, rect(12., 9., 30., 30.));
        if self.sidebar_open {
            p.fill(rect(12., 9., 30., 30.), 0x334750, 6.);
        }
        p.tag_icon(
            16.,
            12.,
            if self.sidebar_open { 0x92c8f3 } else { MUTED },
            0.85,
        );
        p.fill(rect(51., 9., 250., 32.), 0x1c2125, 6.);
        p.border(rect(51., 9., 250., 32.), 0x43515a, 6.);
        if self.testing {
            p.label(
                if self.search.is_empty() {
                    "Search chats / pinyin…"
                } else {
                    &self.search
                },
                rect(61., 9., 206., 32.),
                12.,
                MUTED,
                false,
            );
        }
        if !self.search.is_empty() {
            let r = self.chat_clear_rect();
            area(Hit::ClearChatSearch, r);
            p.search_clear(r, self.hover == Some(Hit::ClearChatSearch));
        }
        area(Hit::Body, rect(310., 8., 60., 32.));
        p.action_icon(&Hit::Body, 312., 14.);
        p.toggle(339., 16., self.search_body);
        area(Hit::Compact, rect(382., 8., 60., 32.));
        for offset in [0., 6., 12.] {
            p.line(385., 17. + offset, 401., 17. + offset, MUTED, 1.3);
        }
        p.toggle(411., 16., self.settings.compact_recent);
        area(Hit::Refresh, rect(454., 8., 32., 32.));
        p.fill(rect(454., 8., 32., 32.), 0x2b3a3c, 8.);
        p.action_icon(&Hit::Refresh, 459., 13.);
        let fresh = self
            .chat_filters
            .iter()
            .filter(|&&i| self.rows[i].newly_completed())
            .count();
        p.label(
            &format!(
                "{} chats{}{}",
                self.chat_filters.len(),
                if self.tag_filters.is_empty() {
                    String::new()
                } else {
                    format!(" · {} tags", self.tag_filters.len())
                },
                if fresh > 0 {
                    format!(" · {fresh} new")
                } else {
                    String::new()
                }
            ),
            rect(
                499.,
                9.,
                width
                    - if self.tag_filters.is_empty() {
                        512.
                    } else {
                        546.
                    },
                30.,
            ),
            11.,
            MUTED,
            false,
        );
        if !self.tag_filters.is_empty() {
            let r = rect(width - 39., 13., 24., 24.);
            area(Hit::TagClear, r);
            p.search_clear(r, self.hover == Some(Hit::TagClear));
        }
        let left = self.recent_left();
        if self.sidebar_open {
            p.line(
                TAG_SIDEBAR,
                RECENT_TOP as f32,
                TAG_SIDEBAR,
                height - 40.,
                0x3a484f,
                1.,
            );
            p.fill(
                rect(16., RECENT_TOP as f32 + 4., TAG_SIDEBAR - 32., 32.),
                0x1c2125,
                5.,
            );
            p.border(
                rect(16., RECENT_TOP as f32 + 4., TAG_SIDEBAR - 32., 32.),
                0x43515a,
                5.,
            );
            p.tag_icon(23., RECENT_TOP as f32 + 11., MUTED, 0.66);
            if self.testing {
                p.label(
                    if self.tag_query.is_empty() {
                        "Tags / pinyin…"
                    } else {
                        &self.tag_query
                    },
                    rect(46., RECENT_TOP as f32 + 5., TAG_SIDEBAR - 97., 29.),
                    11.,
                    TEXT,
                    false,
                );
            }
            if !self.tag_query.is_empty() {
                let r = Self::tag_clear_rect();
                area(Hit::ClearTagSearch, r);
                p.search_clear(r, self.hover == Some(Hit::ClearTagSearch));
            }
            p.label(
                "Name · pinyin · initials",
                rect(18., RECENT_TOP as f32 + 39., TAG_SIDEBAR - 34., 19.),
                9.,
                MUTED,
                false,
            );
            for (region_index, region) in self.tag_regions.iter().enumerate() {
                let viewport = self.tag_region_viewport(region_index);
                p.label(
                    ["Projects", "Groups", "Custom tags"][region_index],
                    rect(16., viewport.top - 25., TAG_SIDEBAR - 40., 22.),
                    10.,
                    MUTED,
                    false,
                );
                if region.items.is_empty() {
                    p.label(
                        "—",
                        rect(20., viewport.top + 1., TAG_SIDEBAR - 40., 24.),
                        11.,
                        MUTED,
                        false,
                    );
                }
                for item in &region.items {
                    let (tag, count) = &self.tag_catalog[item.index];
                    let y = viewport.top + item.r.top - self.tag_scroll[region_index];
                    if y < viewport.top || y + 28. > viewport.bottom {
                        continue;
                    }
                    let x = item.r.left;
                    let w = item.r.right - x;
                    let (bg, fg) = tag.colors();
                    let selected = self.tag_filters.contains(&tag.key);
                    p.fill(rect(x, y, w, 28.), bg, 5.);
                    if selected {
                        p.border(rect(x, y, w, 28.), fg, 5.);
                    }
                    let pad = if tag.kind == tags::Kind::Custom {
                        8.
                    } else {
                        p.border(rect(x + 7., y + 13., 6., 6.), fg, 1.);
                        p.border(rect(x + 8., y + 9., 4., 6.), fg, 2.);
                        20.
                    };
                    let count_w = p.width(&count.to_string(), 9.);
                    p.label(
                        &tag.label,
                        rect(x + pad, y + 1., w - pad - count_w - 26., 26.),
                        11.,
                        fg,
                        false,
                    );
                    p.label(
                        &count.to_string(),
                        rect(x + w - count_w - 20., y + 2., count_w + 1., 25.),
                        9.,
                        MUTED,
                        false,
                    );
                    if selected {
                        p.label("✓", rect(x + w - 16., y + 2., 13., 24.), 10., fg, false);
                    }
                    area(Hit::TagFilter(tag.key.clone()), rect(x, y, w, 28.));
                }
                if let Some((r, thumb, offset)) = self.tag_track_geometry(region_index) {
                    p.fill(
                        rect(TAG_SIDEBAR - 7., r.top, 3., r.bottom - r.top),
                        0x303c44,
                        1.5,
                    );
                    p.fill(
                        rect(TAG_SIDEBAR - 7., r.top + offset, 3., thumb),
                        0x718b9b,
                        1.5,
                    );
                }
                if region_index < 2 {
                    p.line(
                        16.,
                        viewport.bottom + 2.,
                        TAG_SIDEBAR - 15.,
                        viewport.bottom + 2.,
                        0x354148,
                        1.,
                    );
                }
            }
        }
        for (slot, &i) in self
            .chat_filters
            .iter()
            .skip(self.scroll)
            .take(self.list_page_rows())
            .enumerate()
        {
            let row = &self.rows[i];
            let y = RECENT_TOP as f32 + slot as f32 * self.recent_row_height() as f32;
            let r = rect(
                left,
                y,
                width - left - 18.,
                self.recent_row_height() as f32 - 4.,
            );
            let current = row.id == self.identity(&live);
            let busy = row.state == "Busy";
            let hovered = self.row_hovered(&row.id);
            p.fill(
                r,
                if hovered {
                    if current {
                        0x354f46
                    } else if busy {
                        0x4a3d2c
                    } else {
                        0x33434d
                    }
                } else if current {
                    0x293d36
                } else if busy {
                    0x342c23
                } else {
                    PANEL
                },
                7.,
            );
            if hovered || current || self.chat_cursor == self.scroll + slot {
                p.border(
                    r,
                    if hovered {
                        0x78aac4
                    } else if current {
                        0x527b69
                    } else {
                        0x59849c
                    },
                    7.,
                );
            }
            area(Hit::Recent(row.id.clone()), r);
            let icon_r = rect(left + 6., y + 4., 30., 30.);
            area(Hit::ChatIcon(row.id.clone()), icon_r);
            p.fill(icon_r, 0x303b43, 7.);
            p.label(
                &self.catalog.icon(&row.label),
                rect(left + 10., y + 2., 25., 34.),
                17.,
                TEXT,
                true,
            );
            let metric_width = if busy { 225. } else { 165. };
            let metric_x = width - 91. - metric_width;
            let line_h = if self.settings.compact_recent {
                34.
            } else {
                24.
            };
            let title_width = self.settings.recent_title_width.clamp(140, 280) as f32;
            if self.settings.compact_recent {
                p.label(
                    self.catalog.strip(&row.label),
                    rect(left + 42., y + 2., (metric_x - left - 51.).max(40.), line_h),
                    13.,
                    TEXT,
                    false,
                );
            } else {
                p.wrapped_label(
                    self.catalog.strip(&row.label),
                    rect(left + 42., y + 5., title_width, 32.),
                    13.,
                    TEXT,
                );
            }
            let metrics = if busy {
                format!(
                    "{} t/s · {} TOK · {}",
                    row.rate().map(|v| format!("{v:.1}")).unwrap_or("—".into()),
                    data::compact(row.tokens),
                    row.started_at
                        .map(|t| data::duration(now() - t))
                        .unwrap_or("—".into())
                )
            } else {
                format!(
                    "{} TOK · {}",
                    data::compact(row.tokens),
                    row.total_seconds.map(data::duration).unwrap_or("—".into())
                )
            };
            p.label(
                &metrics,
                if self.settings.compact_recent {
                    rect(metric_x, y + 2., metric_width, line_h)
                } else {
                    rect(left + 42., y + 43., 225., 22.)
                },
                10.,
                MUTED,
                false,
            );
            p.label(
                &row.status_text(),
                rect(width - 85., y + 2., 70., line_h),
                11.,
                match row.state.as_str() {
                    "Busy" | "Waiting" => ORANGE,
                    "Done" => GREEN,
                    "Failed" => 0xf87171,
                    _ => MUTED,
                },
                false,
            );
            if row.newly_completed() {
                p.circle(
                    width - 27.,
                    y + if self.settings.compact_recent {
                        19.
                    } else {
                        14.
                    },
                    3.3,
                    NEW_RESULT,
                );
            }
            if self.settings.compact_recent {
                continue;
            }
            let (_, tags, hidden) = self.tag_layout(row);
            let snippet = self.body_previews.get(&row.id).filter(|_| {
                self.search_body
                    && !self.search.trim().is_empty()
                    && self.body_only.contains(&row.id)
            });
            let preview = snippet.unwrap_or(&row.prompt);
            let preview_x = left + 42. + title_width + 18.;
            let preview_r = rect(preview_x, y + 6., (width - 105. - preview_x).max(40.), 30.);
            if snippet.is_some() {
                p.highlighted_label(preview, preview_r, 12., &self.search, false);
            } else {
                p.wrapped_label(
                    if preview.is_empty() { "—" } else { preview },
                    preview_r,
                    12.,
                    MUTED,
                );
            }
            for (tag, x, w) in tags {
                self.paint_tag(&tag, x, y + 45., w, self.tag_filters.contains(&tag.key));
                area(Hit::TagFilter(tag.key), rect(x, y + 45., w, 20.));
            }
            if hidden > 0 {
                p.label(
                    &format!("+{hidden}"),
                    rect(width - 79., y + 45., 26., 20.),
                    10.,
                    MUTED,
                    false,
                );
                area(
                    Hit::TagEdit(row.id.clone()),
                    rect(width - 79., y + 45., 26., 20.),
                );
            }
            area(
                Hit::TagEdit(row.id.clone()),
                rect(width - 49., y + 43., 30., 25.),
            );
            p.border(rect(width - 43., y + 49., 12., 10.), 0x91a8b5, 2.);
            p.line(width - 40., y + 54., width - 34., y + 54., MUTED, 1.);
            p.line(width - 37., y + 51., width - 37., y + 57., MUTED, 1.);
        }
        if self.chat_filters.is_empty() {
            p.label(
                if self.body_pending {
                    "Searching chat bodies…"
                } else {
                    "No matching chats"
                },
                rect(left + 18., 105., width - left - 36., 40.),
                13.,
                MUTED,
                false,
            );
        }
        if let Some((top, track, thumb)) = self.chat_scroll_geometry() {
            let off = (track - thumb) * self.scroll as f32
                / self
                    .chat_filters
                    .len()
                    .saturating_sub(self.list_page_rows())
                    .max(1) as f32;
            p.fill(rect(width - 11., top + off, 4., thumb), 0x6f8b9d, 2.);
        }
        p.line(16., height - 38., width - 16., height - 38., 0x354148, 1.);
        let legend = [
            ("Project", tags::Kind::Project),
            ("Group", tags::Kind::Group),
            ("Custom", tags::Kind::Custom),
        ];
        let lw: f32 = legend
            .iter()
            .map(|(text, _)| p.width(text, 10.) + 31.)
            .sum();
        let mut lx = width - 16. - lw;
        let (tok, rate, partial) = live.recent.totals();
        let total = format!(
            "Total {}{} t/s · {} TOK",
            rate.map(|v| format!("{v:.1}")).unwrap_or("—".into()),
            if partial { "*" } else { "" },
            data::compact(tok)
        );
        let tw = p.width(&total, 10.);
        let tx = lx - 14. - tw;
        p.label(
            &format!(
                "{} / {} chats · Index {:.0}%{}",
                self.chat_filters.len(),
                self.rows.len(),
                self.body_state.progress_percent(),
                if self.body_state.error.is_empty() {
                    ""
                } else {
                    " · retry pending"
                }
            ),
            rect(16., height - 29., (tx - 28.).max(30.), 24.),
            10.,
            MUTED,
            false,
        );
        p.label(
            &total,
            rect(tx, height - 29., tw + 1., 24.),
            10.,
            MUTED,
            false,
        );
        for (text, kind) in legend {
            let w = p.width(text, 10.) + 26.;
            self.paint_tag(
                &tags::Tag {
                    label: text.into(),
                    kind,
                    ..Default::default()
                },
                lx,
                height - 27.,
                w,
                false,
            );
            lx += w + 5.;
        }
        if let Some(id) = &self.tag_editor {
            let r = self.tag_editor_rect();
            let x = r.left;
            let y = r.top;
            let w = r.right - x;
            let h = r.bottom - y;
            // Background is a hit target, so clicks never leak to a chat underneath.
            area(Hit::TagBackground, r);
            p.fill(r, 0x2a3237, 9.);
            p.border(r, 0x52646f, 9.);
            p.label(
                "Chat tags",
                rect(x + 14., y + 7., w - 55., 28.),
                14.,
                TEXT,
                false,
            );
            area(Hit::TagClose, rect(x + w - 39., y + 6., 30., 28.));
            p.action_icon(&Hit::Clear, x + w - 34., y + 9.);
            p.label(
                "Project / group · synced, read-only",
                rect(x + 14., y + 36., w - 28., 22.),
                10.,
                MUTED,
                false,
            );
            let mut tx = x + 14.;
            if let Some(row) = self.rows.iter().find(|r| &r.id == id) {
                for tag in &row.tags {
                    let tw = (p.width(&tag.label, 10.) + 26.).min((x + w - 14. - tx).max(30.));
                    self.paint_tag(tag, tx, y + 61., tw, false);
                    tx += tw + 5.;
                    if tx > x + w - 50. {
                        break;
                    }
                }
            }
            p.label(
                "Custom tags",
                rect(x + 14., y + 87., w - 28., 22.),
                11.,
                MUTED,
                false,
            );
            let page = ((h - 204.) / 25.).max(1.) as usize;
            for (slot, tag) in self
                .custom_tags
                .definitions
                .iter()
                .skip(self.tag_editor_scroll)
                .take(page)
                .enumerate()
            {
                let ty = y + 113. + slot as f32 * 25.;
                let selected = self
                    .custom_tags
                    .chats
                    .get(id)
                    .is_some_and(|keys| keys.contains(&tag.key));
                let tw = (p.width(&tag.label, 11.) + 40.).min(w - 30.);
                p.label(
                    if selected { "✓" } else { "+" },
                    rect(x + 16., ty, 20., 22.),
                    12.,
                    tag.colors().1,
                    false,
                );
                p.label(
                    &tag.label,
                    rect(x + 40., ty, w - 60., 22.),
                    11.,
                    tag.colors().1,
                    false,
                );
                area(
                    Hit::TagToggle(tag.key.clone()),
                    rect(x + 12., ty, w - 24., 23.),
                );
                let _ = tw;
            }
            if self.custom_tags.definitions.is_empty() {
                p.label(
                    "Create your first custom tag below",
                    rect(x + 15., y + 115., w - 30., 24.),
                    11.,
                    MUTED,
                    false,
                );
            }
            for (i, (_, fg)) in tags::PALETTE.iter().enumerate() {
                let cx = x + 21. + i as f32 * 25.;
                p.circle(cx, y + h - 69., 6., *fg);
                if self.tag_color == i {
                    p.border(rect(cx - 9., y + h - 78., 18., 18.), TEXT, 9.);
                }
                area(Hit::TagColor(i), rect(cx - 11., y + h - 80., 23., 23.));
            }
            p.fill(rect(x + 14., y + h - 53., w - 62., 29.), 0x1c2125, 5.);
            p.border(rect(x + 14., y + h - 53., w - 62., 29.), 0x52646f, 5.);
            if self.testing {
                p.label(
                    if self.tag_draft.is_empty() {
                        "Add or select a tag…"
                    } else {
                        &self.tag_draft
                    },
                    rect(x + 22., y + h - 53., w - 80., 29.),
                    11.,
                    MUTED,
                    false,
                );
            }
            area(Hit::TagAdd, rect(x + w - 41., y + h - 53., 28., 29.));
            p.label(
                "+",
                rect(x + w - 35., y + h - 53., 24., 29.),
                20.,
                TEXT,
                false,
            );
            p.label(
                &self.tag_note,
                rect(x + 14., y + h - 24., w - 28., 19.),
                10.,
                MUTED,
                false,
            );
        }
        hits
    }
    unsafe fn close_tag_editor(&mut self) {
        self.tag_editor = None;
        if !self.tag_input.is_invalid() {
            let _ = DestroyWindow(self.tag_input);
            self.tag_input = HWND::default();
        }
    }
    unsafe fn open_tag_editor(&mut self, id: String) -> Result<()> {
        self.close_tag_editor();
        self.tag_editor_y = self
            .rows
            .iter()
            .position(|r| r.id == id)
            .and_then(|i| self.chat_filters.iter().position(|v| *v == i))
            .map(|slot| {
                RECENT_TOP as f32
                    + slot.saturating_sub(self.scroll) as f32 * RECENT_ROW as f32
                    + 28.
            })
            .unwrap_or(100.);
        self.tag_editor = Some(id);
        self.tag_editor_scroll = 0;
        self.tag_draft.clear();
        self.tag_note = "Changes are saved locally".into();
        if !self.testing {
            let r = self.tag_editor_rect();
            let scale = self.dpi;
            self.tag_input = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("EDIT"),
                w!(""),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                ((r.left + 22.) * scale) as i32,
                ((r.bottom - 49.) * scale) as i32,
                ((r.right - r.left - 80.) * scale) as i32,
                (22. * scale) as i32,
                self.popup,
                HMENU(105isize as *mut _),
                GetModuleHandleW(None)?,
                None,
            )?;
            SendMessageW(
                self.tag_input,
                WM_SETFONT,
                WPARAM(self.edit_font.0 as usize),
                LPARAM(1),
            );
            let _ = SetWindowTheme(self.tag_input, w!("DarkMode_CFD"), PCWSTR::null());
            let _ = SetFocus(self.tag_input);
        }
        Ok(())
    }
    unsafe fn save_tag_changes(&mut self) {
        self.tag_note = if self.testing {
            "Saved".into()
        } else {
            match self.custom_tags.save(&self.paths) {
                Ok(()) => "Saved locally".into(),
                Err(e) => format!("Could not save: {e}"),
            }
        };
        self.refresh_tag_catalog();
        // Preserve list order/anchor while editing, even if a selected tag was removed.
        // The new membership is applied when filtering changes or the editor closes.
    }
}

impl Painter {
    unsafe fn search_clear(&self, r: D2D_RECT_F, hover: bool) {
        if hover {
            self.fill(r, 0x40535e, 5.);
        }
        let x = (r.left + r.right) * 0.5;
        let y = (r.top + r.bottom) * 0.5;
        self.line(
            x - 3.5,
            y - 3.5,
            x + 3.5,
            y + 3.5,
            if hover { TEXT } else { MUTED },
            1.4,
        );
        self.line(
            x + 3.5,
            y - 3.5,
            x - 3.5,
            y + 3.5,
            if hover { TEXT } else { MUTED },
            1.4,
        );
    }
    unsafe fn tag_icon(&self, x: f32, y: f32, rgb: u32, scale: f32) {
        for (a, b, c, d) in [
            (3., 3., 12., 3.),
            (12., 3., 23., 14.),
            (23., 14., 14., 23.),
            (14., 23., 3., 12.),
            (3., 12., 3., 3.),
        ] {
            self.line(
                x + a * scale,
                y + b * scale,
                x + c * scale,
                y + d * scale,
                rgb,
                1.4,
            );
        }
        self.circle(x + 7.5 * scale, y + 7.5 * scale, 1.5 * scale, rgb);
    }

    unsafe fn brush(&self, rgb: u32) -> Result<ID2D1SolidColorBrush> {
        if let Some(b) = self.brushes.borrow().get(&rgb) {
            return Ok(b.clone());
        }
        let brush = self.target.CreateSolidColorBrush(&color(rgb), None)?;
        let mut cache = self.brushes.borrow_mut();
        if cache.len() >= 128 {
            cache.clear();
        }
        cache.insert(rgb, brush.clone());
        Ok(brush)
    }
    unsafe fn layout(
        &self,
        text: &str,
        size: f32,
        emoji: bool,
        w: f32,
        h: f32,
        wrap: bool,
    ) -> Result<IDWriteTextLayout> {
        let key = (
            text.to_string(),
            size.to_bits(),
            w.to_bits(),
            h.to_bits(),
            emoji,
            wrap,
        );
        let clock = self.layout_clock.get().wrapping_add(1);
        self.layout_clock.set(clock);
        if let Some((layout, used)) = self.layouts.borrow_mut().get_mut(&key) {
            *used = clock;
            return Ok(layout.clone());
        }
        let f = self.format(size, emoji, text)?;
        let chars: Vec<u16> = text.encode_utf16().collect();
        let layout = self.text.CreateTextLayout(&chars, &f, w, h)?;
        if wrap {
            layout.SetWordWrapping(DWRITE_WORD_WRAPPING_CHARACTER)?;
            layout.SetLineSpacing(DWRITE_LINE_SPACING_METHOD_UNIFORM, size + 3., size)?;
        }
        let mut cache = self.layouts.borrow_mut();
        // Evict only the least recently used native layout; changing counters
        // never retain thousands of COM/font objects or flush every hot row.
        if cache.len() >= LAYOUT_CACHE_LIMIT {
            if let Some(old) = cache
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(key, _)| key.clone())
            {
                cache.remove(&old);
            }
        }
        cache.insert(key, (layout.clone(), clock));
        Ok(layout)
    }
}

unsafe extern "system" fn foreground_changed(
    _: HWINEVENTHOOK,
    _: u32,
    _: HWND,
    _: i32,
    _: i32,
    _: u32,
    _: u32,
) {
    APP.with(|slot| {
        if let Ok(value) = slot.try_borrow() {
            if let Some(app) = value.as_ref() {
                let _ = PostMessageW(app.bar, FOCUS_MESSAGE, WPARAM(0), LPARAM(0));
            }
        }
    });
}

unsafe fn verify_recent_tags(app: &mut App, paths: &Paths) -> Result<()> {
    app.close_popup();
    app.sidebar_open = false;
    app.custom_tags = tags::Custom::default();
    app.tag_filters.clear();
    app.custom_tags.add("fixture-0", "排查", 0)?;
    app.custom_tags.add("fixture-3", "文档", 1)?;
    app.popup(Popup::Recent)?;
    assert!(!app.sidebar_open);
    let list_window = app.popup;
    let input = app.edit;
    app.chat_cursor = 3;
    app.scroll = 2;
    let order = app.chat_filters.clone();
    let _ = app.hit(Hit::TagSidebar);
    assert!(app.sidebar_open);
    assert_eq!(app.popup_size[0], 1056);
    assert_eq!(app.chat_filters, order);
    assert_eq!(app.scroll, 2);
    assert_eq!(app.chat_cursor, 3);
    app.scroll = 0;
    app.chat_cursor = 0;
    let h = app.popup_size[1] as u32;
    app.popup_hits = capture(
        app,
        Some(Popup::Recent),
        1056,
        h,
        &paths.runtime.join("rust-tags-expanded-verification.png"),
    )?;
    assert!(app
        .popup_hits
        .iter()
        .any(|a| matches!(a.hit, Hit::TagEdit(_))));
    assert!(app.tag_region_viewport(0).bottom - app.tag_region_viewport(0).top < TAG_REGION_MAX);
    let side = app.tag_regions[0]
        .items
        .iter()
        .find(|item| app.tag_catalog[item.index].0.key == "project:demo")
        .unwrap();
    assert_eq!(
        app.fixed_hit_at(
            side.r.left + 5.,
            app.tag_region_viewport(0).top + side.r.top + 12.
        ),
        Some(Hit::TagFilter("project:demo".into()))
    );
    let _ = app.hit(Hit::TagFilter("project:demo".into()));
    let _ = app.hit(Hit::TagFilter("custom:排查".into()));
    assert_eq!(app.chat_filters.len(), 1);
    assert_eq!(app.rows[app.chat_filters[0]].id, "fixture-0");
    let _ = app.hit(Hit::TagClear);
    assert_eq!(app.chat_filters.len(), 40);
    let _ = app.hit(Hit::TagEdit("fixture-0".into()));
    assert!(app.tag_editor.is_some());
    assert_eq!(app.popup, list_window);
    assert_eq!(app.edit, input);
    let before = app.custom_tags.for_row(&app.rows[0]);
    let _ = app.hit(Hit::TagToggle("project:demo".into()));
    assert_eq!(app.custom_tags.for_row(&app.rows[0]), before);
    app.tag_draft = "待验证".into();
    let _ = app.hit(Hit::TagColor(2));
    assert!(app.fixed_key(VK_RETURN.0 as u32));
    assert!(app
        .custom_tags
        .for_row(&app.rows[0])
        .iter()
        .any(|t| t.label == "待验证" && t.color == 2));
    app.popup_hits = capture(
        app,
        Some(Popup::Recent),
        1056,
        h,
        &paths.runtime.join("rust-tags-editor-verification.png"),
    )?;
    let r = app.tag_editor_rect();
    assert_eq!(
        app.fixed_hit_at(r.left + 5., r.top + 80.),
        Some(Hit::TagBackground)
    );
    let _ = app.hit(Hit::TagClose);
    assert_eq!(app.popup_kind, Some(Popup::Recent));
    assert!(app.tag_editor.is_none());
    let _ = app.hit(Hit::TagSidebar);
    assert!(!app.sidebar_open);
    assert_eq!(app.popup_size[0], 820);
    app.popup_hits = capture(
        app,
        Some(Popup::Recent),
        820,
        h,
        &paths.runtime.join("rust-tags-collapsed-verification.png"),
    )?;
    let before_compact = (
        app.search.clone(),
        app.chat_filters.clone(),
        app.scroll,
        app.chat_cursor,
    );
    let _ = app.hit(Hit::Compact);
    assert!(app.settings.compact_recent);
    assert_eq!(app.popup_size, [640, RECENT_FRAME + 10 * 43]);
    assert_eq!(
        (
            app.search.clone(),
            app.chat_filters.clone(),
            app.scroll,
            app.chat_cursor
        ),
        before_compact
    );
    let compact_hits = capture(
        app,
        Some(Popup::Recent),
        640,
        app.popup_size[1] as u32,
        &paths.runtime.join("rust-recent-compact-verification.png"),
    )?;
    assert!(!compact_hits
        .iter()
        .any(|a| matches!(a.hit, Hit::TagEdit(_))));
    assert_eq!(
        app.fixed_hit_at(80., RECENT_TOP as f32 + 43. + 10.),
        Some(Hit::Recent(app.rows[app.chat_filters[1]].id.clone()))
    );
    let _ = app.hit(Hit::Compact);
    assert!(!app.settings.compact_recent);
    assert_eq!(app.popup_size, [820, RECENT_FRAME + 10 * RECENT_ROW]);
    app.search = "mingcheng 2".into();
    app.filter_fixed();
    let title_hits = app.chat_filters.clone();
    app.body_pending = true;
    assert_eq!(app.filtered_chats(), title_hits);
    app.body_pending = false;
    app.search.clear();
    app.filter_fixed();
    // Each source category owns its own capped viewport, hit geometry and scroll.
    let _ = app.hit(Hit::TagSidebar);
    for (i, row) in app.rows.iter_mut().enumerate() {
        row.tags.push(tags::Tag {
            key: format!("project:p{i}"),
            label: format!("工程 {i}"),
            kind: tags::Kind::Project,
            color: 0,
        });
        row.tags.push(tags::Tag {
            key: format!("group:g{i}"),
            label: format!("分组 {i}"),
            kind: tags::Kind::Group,
            color: 0,
        });
    }
    for i in 0..60 {
        app.custom_tags
            .add("fixture-0", &format!("排查事项 {i}"), i)?;
    }
    app.refresh_tag_catalog();
    let regions = (0..3)
        .map(|i| app.tag_region_viewport(i))
        .collect::<Vec<_>>();
    assert!((0..3).all(|i| app.tag_scroll_max(i) > 0.));
    app.scroll_tag_region(0, 48.);
    assert!(app.tag_scroll[0] > 0.);
    assert_eq!(app.tag_scroll[1], 0.);
    assert_eq!(app.tag_scroll[2], 0.);
    app.scroll_tag_region(1, 48.);
    assert_eq!(app.tag_scroll[0], 48.);
    assert_eq!(app.tag_scroll[2], 0.);
    app.scroll_tag_region(2, 10000.);
    assert_eq!(app.tag_scroll[2], app.tag_scroll_max(2));
    assert!((0..3).all(|i| {
        let r = app.tag_region_viewport(i);
        r.top == regions[i].top
            && r.bottom == regions[i].bottom
            && r.bottom - r.top <= TAG_REGION_MAX
    }));
    let r = app.tag_region_viewport(0);
    assert!(app.begin_tag_scroll(TAG_SIDEBAR - 6., r.top + 10.));
    app.drag_tag_scroll(r.bottom + 100.);
    assert_eq!(app.tag_scroll[0], app.tag_scroll_max(0));
    app.tag_scroll_drag = None;
    let _ = ReleaseCapture();
    app.tag_scroll = [0.; 3];
    capture(
        app,
        Some(Popup::Recent),
        1056,
        h,
        &paths.runtime.join("rust-tags-regions-verification.png"),
    )?;
    let _ = app.hit(Hit::TagFilter("custom:排查".into()));
    let selected = app.tag_filters.clone();
    let chats = app.chat_filters.clone();
    let mut matching = Vec::new();
    for query in ["排查", "paicha", "pc"] {
        app.tag_query = query.into();
        app.rebuild_tag_regions();
        matching.push(
            app.tag_regions[2]
                .items
                .iter()
                .map(|i| app.tag_catalog[i.index].0.key.clone())
                .collect::<Vec<_>>(),
        );
        assert_eq!(app.tag_filters, selected);
        assert_eq!(app.chat_filters, chats);
    }
    assert_eq!(matching[0], matching[1]);
    assert_eq!(matching[1], matching[2]);
    assert!(matching[0].len() > 50);
    capture(
        app,
        Some(Popup::Recent),
        1056,
        h,
        &paths.runtime.join("rust-tags-pinyin-verification.png"),
    )?;
    app.testing = false;
    app.ensure_tag_search()?;
    assert!(!app.tag_search_input.is_invalid());
    assert_eq!(GetDlgCtrlID(app.tag_search_input), 106);
    app.close_tag_search();
    app.testing = true;
    app.tag_query.clear();
    app.rebuild_tag_regions();
    let _ = app.hit(Hit::TagClear);
    let _ = app.hit(Hit::TagSidebar);
    // The minimum four-row compact list must still fit all three tag regions.
    let saved_rows = app.settings.recent_rows;
    app.settings.recent_rows = 4;
    let _ = app.hit(Hit::Compact);
    let _ = app.hit(Hit::TagSidebar);
    assert_eq!(app.list_page_rows(), 4);
    assert!((0..3).all(|i| {
        let r = app.tag_region_viewport(i);
        r.bottom - r.top >= 28. && r.bottom <= app.popup_size[1] as f32 - 44.
    }));
    let _ = app.hit(Hit::TagSidebar);
    let _ = app.hit(Hit::Compact);
    app.settings.recent_rows = saved_rows;
    app.popup_size = [app.recent_width(), app.recent_panel_height()];
    // Verify the actual Windows registration is released outside our windows.
    let old_host = app.state.lock().unwrap().host;
    let shortcut = app.settings.recent_shortcut.clone();
    app.settings.recent_shortcut = Shortcut {
        key: 0x87,
        shift: true,
        ..Shortcut::default()
    };
    app.state.lock().unwrap().host = 0;
    // Inject foreground handles into the same production scope resolver. This
    // exercises real registration without stealing the user's foreground focus.
    app.sync_shortcut_for(app.popup, true);
    assert!(app.shortcut_registered);
    let outside = CreateWindowExW(
        WS_EX_TOOLWINDOW,
        CLASS,
        w!("Shortcut scope verification"),
        WS_POPUP,
        0,
        0,
        1,
        1,
        None,
        None,
        GetModuleHandleW(None)?,
        None,
    )?;
    app.sync_shortcut_for(outside, true);
    assert!(!app.shortcut_registered);
    assert!(!app.shortcut_allowed_in(outside));
    assert!(RegisterHotKey(outside, 9, MOD_CONTROL | MOD_ALT | MOD_SHIFT, 0x87).is_ok());
    let _ = UnregisterHotKey(outside, 9);
    let _ = DestroyWindow(outside);
    app.shortcut_foreground = HWND::default();
    app.settings.recent_shortcut = shortcut;
    app.state.lock().unwrap().host = old_host;
    paths.write("rust-tags-ui-test.json",&json!({"collapsed_by_default":true,"sidebar_scroll_and_multitag_filter":true,"source_tags_locked":true,"editor_keeps_list_window_and_scroll":true,
        "custom_add_color_toggle_and_enter":true,"three_capped_regions_scroll_independently":true,"tag_scrollbars_drag_to_end":true,"tag_name_pinyin_initials_match":true,"tag_search_preserves_selections":true,"native_tag_search_control":true,
        "compact_hides_tags_and_preview_preserves_filter_scroll":true,"title_results_immediate_during_body_search":true,"shortcut_released_and_available_to_other_apps":true,"render_cache_limit":LAYOUT_CACHE_LIMIT}));
    Ok(())
}

unsafe fn verify_filter_controls(app: &mut App, paths: &Paths) -> Result<()> {
    app.close_popup();
    app.sidebar_open = false;
    app.popup(Popup::Recent)?;
    app.search = "xxx".into();
    app.filter_fixed();
    let _ = SetWindowTextW(app.edit, w!("xxx"));
    app.tag_filters.insert("project:demo".into());
    let selected = app.tag_filters.clone();
    assert_eq!(app.fixed_hit_at(285., 25.), Some(Hit::ClearChatSearch));
    let _ = app.hit(Hit::ClearChatSearch);
    assert!(app.search.is_empty());
    assert_eq!(GetWindowTextLengthW(app.edit), 0);
    assert_eq!(app.tag_filters, selected);
    let _ = app.hit(Hit::TagSidebar);
    app.testing = false;
    app.ensure_tag_search()?;
    app.testing = true;
    app.tag_query = "pc".into();
    app.rebuild_tag_regions();
    let _ = SetWindowTextW(app.tag_search_input, w!("pc"));
    let r = App::tag_clear_rect();
    assert_eq!(
        app.fixed_hit_at(r.left + 12., r.top + 12.),
        Some(Hit::ClearTagSearch)
    );
    let _ = app.hit(Hit::ClearTagSearch);
    assert!(app.tag_query.is_empty());
    assert_eq!(GetWindowTextLengthW(app.tag_search_input), 0);
    assert_eq!(app.tag_filters, selected);
    let _ = app.hit(Hit::TagClear);
    let _ = app.hit(Hit::TagSidebar);
    // Pointer position controls the whole row, including tag chips, independently of the keyboard cursor.
    app.chat_cursor = 0;
    app.scroll = 0;
    assert!(app.set_list_pointer(Some((80., RECENT_TOP as f32 + RECENT_ROW as f32 + 15.))));
    assert!(!app.set_list_pointer(Some((82., RECENT_TOP as f32 + RECENT_ROW as f32 + 17.))));
    let hover_id = app.rows[app.chat_filters[1]].id.clone();
    assert!(app.row_hovered(&hover_id));
    assert!(!app.row_hovered(&app.rows[app.chat_filters[0]].id));
    app.scroll = 1;
    assert!(!app.row_hovered(&hover_id));
    app.scroll = 0;
    app.hover = Some(Hit::Recent(hover_id));
    capture(
        app,
        Some(Popup::Recent),
        820,
        app.popup_size[1] as u32,
        &paths.runtime.join("rust-list-hover-verification.png"),
    )?;
    app.list_pointer = None;
    app.hover = None;
    assert!(app.rows.iter().all(|r| !app.row_hovered(&r.id)));
    app.search = "xxx".into();
    app.filter_fixed();
    let _ = SetWindowTextW(app.edit, w!("xxx"));
    capture(
        app,
        Some(Popup::Recent),
        820,
        app.popup_size[1] as u32,
        &paths.runtime.join("rust-search-clear-verification.png"),
    )?;
    let _ = app.hit(Hit::ChatIcon("fixture-0".into()));
    let parent_search = app.search.clone();
    let parent_filters = app.chat_filters.clone();
    let parent_scroll = app.scroll;
    {
        let panel = app.icon_panel.as_mut().unwrap();
        panel.search = "camera".into();
        let _ = SetWindowTextW(panel.edit, w!("camera"));
    }
    let cat = app.icon_panel.as_ref().unwrap().category.clone();
    let _ = app.hit(Hit::ClearEmojiSearch);
    let panel = app.icon_panel.as_ref().unwrap();
    assert!(panel.search.is_empty());
    assert_eq!(panel.category, cat);
    assert_eq!(GetWindowTextLengthW(panel.edit), 0);
    assert_eq!(app.search, parent_search);
    assert_eq!(app.chat_filters, parent_filters);
    assert_eq!(app.scroll, parent_scroll);
    app.close_popup();
    app.popup(Popup::Fixed)?;
    app.search = "test".into();
    app.filter_fixed();
    assert_eq!(
        app.fixed_hit_at(app.popup_size[0] as f32 - 35., 66.),
        Some(Hit::ClearChatSearch)
    );
    let _ = app.hit(Hit::ClearChatSearch);
    assert!(app.search.is_empty());
    assert_eq!(GetWindowTextLengthW(app.edit), 0);
    app.close_popup();
    app.popup(Popup::Icons)?;
    app.search = "camera".into();
    let _ = SetWindowTextW(app.edit, w!("camera"));
    let category = app.category.clone();
    let _ = app.hit(Hit::ClearEmojiSearch);
    assert!(app.search.is_empty());
    assert_eq!(app.category, category);
    assert_eq!(GetWindowTextLengthW(app.edit), 0);
    paths.write("rust-filter-ui-test.json",&json!({"chat_clear":true,"fixed_chat_clear":true,"tag_clear_keeps_selection":true,"emoji_clear_keeps_category":true,"popover_clear_keeps_parent":true,"hover_tracks_pointer_and_scroll":true,"footer_legend":true}));
    Ok(())
}

unsafe fn verify_memory_cache(app: &mut App, paths: &Paths) -> Result<()> {
    app.close_popup();
    let before = private_bytes();
    let hot = "常用会话名称";
    let hot_layout = app.painter.layout(hot, 13., false, 200., 40., true)?;
    for i in 0..3000 {
        let text = format!("变化中的统计 {i} TOK · 12:{:02}", i % 60);
        let _ = app.painter.layout(&text, 11., false, 250., 24., false)?;
        let _ = app.painter.width(&text, 11.);
        let same = app.painter.layout(hot, 13., false, 200., 40., true)?;
        assert_eq!(same, hot_layout);
        assert!(app.painter.layouts.borrow().len() <= LAYOUT_CACHE_LIMIT);
    }
    let populated = private_bytes();
    let entries = app.painter.layouts.borrow().len();
    app.painter.release_layouts();
    assert!(app.painter.layouts.borrow().is_empty());
    assert!(app.painter.measurements.borrow().is_empty());
    paths.write("rust-memory-cache-test.json",&json!({"changing_counters":3000,"layout_entries":entries,"layout_limit":LAYOUT_CACHE_LIMIT,"width_limit":512,"hot_layout_retained":true,"released_after_close":true,"private_before":before,"private_populated":populated,"private_after_clear":private_bytes()}));
    Ok(())
}

unsafe fn verify_two_columns(app: &mut App, paths: &Paths) -> Result<()> {
    app.close_popup();
    app.sidebar_open = false;
    app.settings.compact_recent = false;
    app.settings.recent_title_width = 200;
    app.popup(Popup::Recent)?;
    let first = app.chat_filters[0];
    app.rows[first].label = "🚧 条码智能相机纯 RTL：图像采集、数据链路与帧缓存验证".into();
    app.rows[first].prompt="继续完成图像采集链路，检查帧缓存与跨时钟域数据传输，保持四路并行，覆盖连续采集时的背压、丢帧和异常恢复。".into();
    let second = app.chat_filters[1];
    app.rows[second].label = "💾 LPDDR2C 控制器调试优化与多通道读写性能验证".into();
    app.rows[second].prompt =
        "先修改设计，然后运行仿真回归以及板级测试，核对采样时序、突发传输和数据校验的结果。".into();
    let h = app.popup_size[1] as u32;
    app.popup_hits = capture(
        app,
        Some(Popup::Recent),
        820,
        h,
        &paths.runtime.join("rust-two-column-verification.png"),
    )?;
    assert_eq!(app.recent_row_height(), 72);
    let (_, tags, _) = app.tag_layout(&app.rows[first]);
    if let Some((tag, x, _)) = tags.first() {
        assert_eq!(
            app.fixed_hit_at(x + 5., RECENT_TOP as f32 + 52.),
            Some(Hit::TagFilter(tag.key.clone()))
        );
    }
    app.search = "帧缓存".into();
    app.search_body = true;
    app.body_only.insert(app.rows[first].id.clone());
    app.body_previews
        .insert(app.rows[first].id.clone(), app.rows[first].prompt.clone());
    capture(
        app,
        Some(Popup::Recent),
        820,
        h,
        &paths
            .runtime
            .join("rust-two-column-search-verification.png"),
    )?;
    app.close_popup();
    app.popup(Popup::Preferences)?;
    let _ = app.hit(Hit::TitleWidth(20));
    assert_eq!(
        app.preferences_draft.as_ref().unwrap().recent_title_width,
        220
    );
    for _ in 0..20 {
        let _ = app.hit(Hit::TitleWidth(20));
    }
    assert_eq!(
        app.preferences_draft.as_ref().unwrap().recent_title_width,
        280
    );
    for _ in 0..20 {
        let _ = app.hit(Hit::TitleWidth(-20));
    }
    assert_eq!(
        app.preferences_draft.as_ref().unwrap().recent_title_width,
        140
    );
    capture(
        app,
        Some(Popup::Preferences),
        560,
        PREFERENCES_HEIGHT as u32,
        &paths.runtime.join("rust-column-settings-verification.png"),
    )?;
    let saved: Settings = serde_json::from_value(serde_json::to_value(&app.settings)?)?;
    assert_eq!(saved.recent_title_width, 200);
    paths.write("rust-two-column-test.json",&json!({"two_line_title_and_prompt":true,"body_highlight":true,"tag_hit_positions":true,"width_setting_range":[140,280],"settings_roundtrip":true,"compact_unchanged":true}));
    Ok(())
}
