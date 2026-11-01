//! Non-activating, scrollable hover card for a single visible chat.
use super::*;
use std::time::Duration;
#[path = "hover_markdown.rs"]
mod markdown;

const WIDTH:f32=620.;
const HEIGHT:f32=260.;
const REPLY_TOP:f32=53.;
fn wheel_scroll(current:f32,delta:i16,maximum:f32)->f32{
    (current-delta as f32/120.*54.).clamp(0.,maximum.max(0.))
}
fn vertical_placement(top:i32,bottom:i32,work_top:i32,work_bottom:i32,desired:i32,minimum:i32)->Option<(i32,i32)> {
    let below_space=(work_bottom-bottom-4).max(0);let above_space=(top-work_top-4).max(0);
    let below=below_space>=desired||below_space>=above_space;let h=desired.min(if below{below_space}else{above_space});
    if h<minimum{return None}Some((if below{bottom+4}else{top-h-4},h))
}

pub(super) struct HoverCard {
    pub window:HWND,
    pub visible:bool,
    id:Option<String>,
    candidate:Option<String>,
    since:Instant,
    last_inside:Instant,
    scroll:f32,
    max_scroll:f32,
    height:f32,
    drag:Option<f32>,
}
impl HoverCard {
    pub unsafe fn new(owner:HWND)->Result<Self>{
        let window=CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE,CLASS,w!("Codex Session Bar Hover"),WS_POPUP,0,0,WIDTH as i32,HEIGHT as i32,owner,None,GetModuleHandleW(None)?,None)?;
        Ok(Self{window,visible:false,id:None,candidate:None,since:Instant::now(),last_inside:Instant::now(),scroll:0.,max_scroll:0.,height:HEIGHT,drag:None})
    }
    pub fn active_id(&self)->Option<String>{if self.visible{self.id.clone()}else{None}}
    pub unsafe fn hide(&mut self){if self.drag.take().is_some(){let _=ReleaseCapture();}if self.visible{let _=ShowWindow(self.window,SW_HIDE);}self.visible=false;self.id=None;self.candidate=None;self.scroll=0.;self.max_scroll=0.;}
    pub(super) unsafe fn contains_cursor(&self)->bool{
        if !self.visible{return false}let mut p=POINT::default();let mut r=RECT::default();GetCursorPos(&mut p).is_ok()&&GetWindowRect(self.window,&mut r).is_ok()&&p.x>=r.left&&p.x<r.right&&p.y>=r.top&&p.y<r.bottom
    }
    unsafe fn show(&mut self,id:&str,owner:HWND,row:D2D_RECT_F,manager:bool){
        let mut point=POINT::default();if GetCursorPos(&mut point).is_err(){return}
        let scale=GetDpiForWindow(owner).max(96) as f32/96.;let w=(WIDTH*scale).round() as i32;
        let mut info=MONITORINFO{cbSize:size_of::<MONITORINFO>() as u32,..Default::default()};let _=GetMonitorInfoW(MonitorFromWindow(owner,MONITOR_DEFAULTTONEAREST),&mut info);let work=info.rcWork;
        let mut origin=POINT::default();if !ClientToScreen(owner,&mut origin).as_bool(){return}
        let top=origin.y+(row.top*scale).round() as i32;let bottom=origin.y+(row.bottom*scale).round() as i32;
        let x=(point.x+14).clamp(work.left,(work.right-w).max(work.left));
        let desired=if manager{390.}else{HEIGHT};
        let Some((y,h))=vertical_placement(top,bottom,work.top,work.bottom,(desired*scale).round() as i32,(90.*scale) as i32)else{return};self.height=h as f32/scale;
        App::round_window(self.window,w,h,(12.*scale) as i32);
        if SetWindowPos(self.window,HWND_TOP,x,y,w,h,SWP_NOACTIVATE|SWP_SHOWWINDOW).is_ok(){self.id=Some(id.into());self.visible=true;self.scroll=0.;self.max_scroll=0.;self.last_inside=Instant::now();let _=InvalidateRect(self.window,None,false);}
    }
}

impl App {
    pub(super) unsafe fn update_hover_card(&mut self){
        if !matches!(self.popup_kind,Some(Popup::Recent|Popup::Fixed|Popup::Repair))||self.icon_panel.is_some()||self.chat_menu.is_some()||self.tag_editor.is_some()||self.tag_overflow.is_some()||self.hidden||self.host_hidden{self.hover_card.hide();return}
        let now=Instant::now();
        if self.hover_card.contains_cursor(){self.hover_card.last_inside=now;return}
        let target=match &self.hover{Some(Hit::Recent(id)|Hit::Fixed(id)|Hit::ManagerRow(id))=>Some(id.clone()),_=>None};
        if let Some(id)=target {
            self.hover_card.last_inside=now;
            if self.hover_card.candidate.as_deref()!=Some(&id){self.hover_card.candidate=Some(id.clone());self.hover_card.since=now;if self.hover_card.visible&&self.hover_card.id.as_deref()!=Some(&id){let _=ShowWindow(self.hover_card.window,SW_HIDE);self.hover_card.visible=false;self.hover_card.id=None;}}
            if !self.hover_card.visible&&now.duration_since(self.hover_card.since)>=Duration::from_millis(450){if let Some(row)=self.popup_hits.iter().find(|a|matches!(&a.hit,Hit::Recent(v)|Hit::Fixed(v)|Hit::ManagerRow(v) if v==&id)).map(|a|a.r){self.hover_card.show(&id,self.popup,row,self.popup_kind==Some(Popup::Repair));}}
        }else if self.hover_card.visible&&now.duration_since(self.hover_card.last_inside)<Duration::from_millis(450){}else{self.hover_card.hide();}
    }
    unsafe fn card_reply(&self,id:&str)->String{
        match self.reply_service.as_ref().and_then(|s|s.peek(id)){
            Some(crate::replies::Preview::Text(text))=>text,
            Some(other)=>other.text(crate::locale::chinese()),
            None=>if crate::locale::chinese(){"正在读取最新回复…"}else{"Loading latest reply…"}.into(),
        }
    }
    /// Returns the maximum reply scroll offset; content is clipped to its pane.
    pub(super) unsafe fn draw_hover_card(&self,id:&str,scroll:f32)->f32 {
        let p=&self.painter;let zh=crate::locale::chinese();let manager=self.popup_kind==Some(Popup::Repair);
        let entry=if manager{self.repair_service.view.lock().unwrap().rows.iter().find(|r|r.id==id).cloned()}else{None};
        let fallback=entry.as_ref().map(|e|crate::data::Row{id:e.id.clone(),label:e.title.clone(),disk_bytes:Some(e.bytes),state:if e.note=="running"||self.repair_activity.contains(id){"Busy".into()}else if matches!(e.last_status.as_str(),"completed"|"failed"|"interrupted"|"cancelled"|"canceled"){"Done".into()}else{"Unknown".into()},..Default::default()});
        let Some(row)=self.rows.iter().find(|r|r.id==id).or(fallback.as_ref())else{return 0.};let height=self.hover_card.height;let reply_bottom=height-14.;
        p.fill(rect(0.,0.,WIDTH,height),0x273237,11.);p.border(rect(0.5,0.5,WIDTH-1.,height-1.),0x4a626b,11.);
        let icon=self.catalog.icon(&row.label);if !icon.is_empty(){p.fill(rect(12.,8.,26.,26.),0x394e57,6.);p.label(&icon,rect(14.,7.,24.,28.),16.,TEXT,true);}
        let (status,color,bg)=match row.state.as_str(){"Busy"=>(if zh{"运行中"}else{"Busy"},ORANGE,0x514033),"Done"=>(if zh{"已完成"}else{"Done"},GREEN,0x29493d),"Stopped"=>(if zh{"已停止"}else{"Stopped"},MUTED,0x364047),"Failed"=>(if zh{"失败"}else{"Failed"},0xe67f83,0x4a3437),_=>(if zh{"未知"}else{"Unknown"},MUTED,0x364047)};
        let model=crate::telemetry::Execution{model:row.execution.model.clone(),effort:String::new(),service_tier:None}.short();
        let model=if model.is_empty(){if zh{"未知"}else{"Unknown"}}else{&model};
        let effort=match row.execution.effort.as_str(){"none"=>"Off","minimal"=>"Min","low"=>"L","medium"=>"M","high"=>"High","xhigh"=>"XH","max"=>"Max","ultra"=>"Ultra",""=>if zh{"未知"}else{"Unknown"},other=>other};
        let size=row.disk_bytes.map(|n|format!("{:.2} MiB",n as f64/1048576.)).unwrap_or_else(||"—".into());
        // Reserve space from the right so a long title cannot cover telemetry.
        let status_w=(p.width(status,10.)+23.).clamp(54.,78.);let status_x=WIDTH-12.-status_w;
        let model_w=(p.width(model,10.)+18.).clamp(48.,100.);let effort_w=(p.width(effort,10.)+17.).clamp(38.,66.);
        let size_w=(p.width(&size,10.)+17.).clamp(48.,93.);
        let telemetry_w=model_w+effort_w+size_w+12.;let telemetry_x=status_x-8.-telemetry_w;
        p.label(self.catalog.strip(&row.label),rect(45.,7.,(telemetry_x-51.).max(65.),28.),12.,TEXT,false);
        let mut x=telemetry_x;
        p.border(rect(x,14.,11.,12.),0x9db7e6,2.);p.fill(rect(x+3.,17.,5.,6.),0x9db7e6,1.);p.label(model,rect(x+15.,7.,model_w-15.,28.),10.,MODEL,false);x+=model_w;
        p.line(x+2.,13.,x+2.,28.,0x40535a,1.);x+=6.;p.line(x+6.,14.,x+6.,26.,MODEL,1.3);p.line(x,20.,x+12.,20.,MODEL,1.3);p.line(x+2.,16.,x+10.,24.,MODEL,1.);p.line(x+10.,16.,x+2.,24.,MODEL,1.);p.label(effort,rect(x+15.,7.,effort_w-15.,28.),10.,MODEL,false);x+=effort_w;
        p.line(x+2.,13.,x+2.,28.,0x40535a,1.);x+=6.;p.border(rect(x+1.,14.,10.,12.),0x75c7cf,2.);p.line(x+4.,18.,x+8.,18.,0x75c7cf,1.);p.line(x+4.,22.,x+8.,22.,0x75c7cf,1.);p.label(&size,rect(x+15.,7.,size_w-15.,28.),10.,TEXT,false);
        p.fill(rect(status_x,9.,status_w,24.),bg,12.);p.circle(status_x+10.,21.,3.,color);p.label(status,rect(status_x+17.,9.,status_w-20.,24.),10.,color,false);
        p.line(12.,43.,WIDTH-12.,43.,0x40535a,1.);
        // The reply icon stays at the top right; the text uses the full pane.
        p.fill(rect(WIDTH-39.,REPLY_TOP,22.,22.),0x304e4a,5.);p.border(rect(WIDTH-35.,REPLY_TOP+4.,14.,10.),GREEN,3.);p.line(WIDTH-31.,REPLY_TOP+14.,WIDTH-33.,REPLY_TOP+17.,GREEN,1.);
        let reply=self.card_reply(id);
        let blocks=markdown::parse(&reply);let pane=reply_bottom-REPLY_TOP;let mut y=REPLY_TOP-scroll;
        p.target.PushAxisAlignedClip(&rect(14.,REPLY_TOP,WIDTH-62.,pane),D2D1_ANTIALIAS_MODE_ALIASED);
        if manager {
            if let Some(e)=entry.as_ref(){
                let when=chrono::DateTime::from_timestamp(e.updated as i64,0).filter(|_|e.updated>0.).map(|v|v.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string()).unwrap_or_else(||"—".into());
                let meta=format!("{}  ·  {}  ·  {:.1} h{}",if e.archived{if zh{"已归档"}else{"Archived"}}else if zh{"未归档"}else{"Local"},when,e.seconds/3600.,if e.compressed{"  ·  ZIP"}else{""});
                p.label(&meta,rect(14.,y,WIDTH-70.,24.),10.,MUTED,false);y+=29.;
                if e.issue {let note=crate::repair::note(&e.note,zh);p.label(note,rect(14.,y,WIDTH-70.,23.),10.,ORANGE,false);y+=25.;}
                if !e.result.is_empty()&&!e.result.starts_with("cleaned:")&&!e.result.starts_with("reduced:")&&e.result!="repaired"&&e.result!="decompressed"{let msg=format!("{}: {}",if zh{"操作结果"}else{"Operation result"},e.result);if let Ok(layout)=p.layout(&msg,11.,false,WIDTH-72.,12000.,true,0){let _=layout.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);let mut m=DWRITE_TEXT_METRICS::default();let _=layout.GetMetrics(&mut m);if let Ok(brush)=p.brush(ORANGE){p.target.DrawTextLayout(D2D_POINT_2F{x:14.,y},&layout,&brush,D2D1_DRAW_TEXT_OPTIONS_CLIP);}y+=m.height.max(18.)+6.;}}
            }
            p.circle(20.,y+10.,3.,0x79bfe8);p.label(if zh{"最近的 Prompt"}else{"Latest prompt"},rect(29.,y,WIDTH-90.,21.),11.,0x9ed7f0,false);y+=23.;
            let prompt=self.reply_service.as_ref().and_then(|s|s.prompt(id)).filter(|s|!s.trim().is_empty()).unwrap_or_else(||if zh{"暂无可显示的提问"}else{"No prompt available"}.into());
            if let Ok(layout)=p.layout(&prompt,12.,false,WIDTH-78.,12000.,true,0){let _=layout.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);let mut m=DWRITE_TEXT_METRICS::default();let _=layout.GetMetrics(&mut m);let h=m.height.max(18.)+12.;p.fill(rect(14.,y,WIDTH-62.,h),0x202d32,4.);if let Ok(brush)=p.brush(TEXT){p.target.DrawTextLayout(D2D_POINT_2F{x:21.,y:y+6.},&layout,&brush,D2D1_DRAW_TEXT_OPTIONS_CLIP);}y+=h+8.;}
            p.circle(20.,y+10.,3.,GREEN);p.label(if zh{"最新回复"}else{"Latest reply"},rect(29.,y,WIDTH-90.,21.),11.,GREEN,false);y+=25.;
        }
        for block in blocks {
            match block {
                markdown::Block::Text(value,heading)=>{
                    if value.text.is_empty(){y+=8.;continue}
                    let size=if heading{14.}else{13.};
                    if let Ok(layout)=p.layout(&value.text,size,false,WIDTH-62.,12000.,true,0){
                        let _=layout.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);let _=layout.SetLineSpacing(DWRITE_LINE_SPACING_METHOD_UNIFORM,18.,14.);
                        let whole=DWRITE_TEXT_RANGE{startPosition:0,length:value.text.encode_utf16().count() as u32};let _=layout.SetFontWeight(DWRITE_FONT_WEIGHT_NORMAL,whole);if let Ok(brush)=p.brush(TEXT){let _=layout.SetDrawingEffect(&brush,whole);}
                        if heading{let _=layout.SetFontWeight(DWRITE_FONT_WEIGHT_SEMI_BOLD,DWRITE_TEXT_RANGE{startPosition:0,length:value.text.encode_utf16().count() as u32});}
                        for (start,length) in value.bold {let _=layout.SetFontWeight(DWRITE_FONT_WEIGHT_SEMI_BOLD,DWRITE_TEXT_RANGE{startPosition:start,length});}
                        for (start,length) in value.code {if let Ok(brush)=p.brush(0x93d4dc){let _=layout.SetDrawingEffect(&brush,DWRITE_TEXT_RANGE{startPosition:start,length});}}
                        let mut metrics=DWRITE_TEXT_METRICS::default();let _=layout.GetMetrics(&mut metrics);
                        if y<reply_bottom && y+metrics.height>=REPLY_TOP {if let Ok(brush)=p.brush(TEXT){p.target.DrawTextLayout(D2D_POINT_2F{x:14.,y},&layout,&brush,D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT|D2D1_DRAW_TEXT_OPTIONS_CLIP);}}
                        y+=metrics.height.max(18.)+3.;
                    }
                },
                markdown::Block::Code(code)=>{
                    if let Ok(layout)=p.layout(&code,12.,false,WIDTH-78.,12000.,true,0){let _=layout.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);let mut m=DWRITE_TEXT_METRICS::default();let _=layout.GetMetrics(&mut m);let h=m.height.max(18.)+12.;if y<reply_bottom&&y+h>=REPLY_TOP {p.fill(rect(14.,y,WIDTH-62.,h),0x1f292d,4.);if let Ok(brush)=p.brush(0xb6d3d9){p.target.DrawTextLayout(D2D_POINT_2F{x:21.,y:y+6.},&layout,&brush,D2D1_DRAW_TEXT_OPTIONS_CLIP);}}y+=h+6.;}
                },
                markdown::Block::Table(rows)=>{
                    let columns=rows.iter().map(|r|r.len()).max().unwrap_or(0).min(12);if columns==0 {continue}let full=WIDTH-62.;let cw=full/columns as f32;
                    for (index,row) in rows.iter().enumerate(){
                        let mut layouts=Vec::new();let mut height=25_f32;
                        for i in 0..columns {let text=row.get(i).map(|v|v.text.as_str()).unwrap_or("");let layout=p.layout(text,11.,false,(cw-10.).max(1.),12000.,true,0).ok();if let Some(l)=&layout {let _=l.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);let mut m=DWRITE_TEXT_METRICS::default();let _=l.GetMetrics(&mut m);height=height.max(m.height+10.);}layouts.push(layout);}
                        if y<reply_bottom&&y+height>=REPLY_TOP {p.fill(rect(14.,y,full,height),if index==0{0x304249}else{0x202a2e},0.);for (i,layout) in layouts.iter().enumerate(){let x=14.+i as f32*cw;p.border(rect(x,y,cw,height),0x40565c,0.);if let Some(layout)=layout {let len=row.get(i).map(|v|v.text.encode_utf16().count()).unwrap_or(0) as u32;let _=layout.SetFontWeight(if index==0{DWRITE_FONT_WEIGHT_SEMI_BOLD}else{DWRITE_FONT_WEIGHT_NORMAL},DWRITE_TEXT_RANGE{startPosition:0,length:len});if let Ok(brush)=p.brush(if index==0{0xa8d5dc}else{TEXT}){p.target.DrawTextLayout(D2D_POINT_2F{x:x+5.,y:y+5.},layout,&brush,D2D1_DRAW_TEXT_OPTIONS_CLIP);}}}}
                        y+=height;
                    }y+=6.;
                }
            }
        }
        p.target.PopAxisAlignedClip();let maximum=(y+scroll-REPLY_TOP-pane).max(0.);let scroll=scroll.clamp(0.,maximum);
        if maximum>0.{let x=WIDTH-13.;let thumb=(pane*pane/(pane+maximum)).max(22.).min(pane);let y=REPLY_TOP+(pane-thumb)*scroll/maximum;p.fill(rect(x,REPLY_TOP,4.,pane),0x334249,2.);p.fill(rect(x,y,4.,thumb),0x819ca6,2.);}
        maximum
    }
    pub(super) unsafe fn card_message(&mut self,hwnd:HWND,msg:u32,w:WPARAM,l:LPARAM)->Option<LRESULT>{
        let reply_bottom=self.hover_card.height-14.;let scale=GetDpiForWindow(hwnd).max(96) as f32/96.;let x=(l.0 as i16) as f32/scale;let y=((l.0>>16) as i16) as f32/scale;
        match msg {
            WM_MOUSEACTIVATE=>Some(LRESULT(MA_NOACTIVATE as isize)),
            WM_ERASEBKGND=>Some(LRESULT(1)),
            WM_PAINT=>{let mut ps=PAINTSTRUCT::default();let dc=BeginPaint(hwnd,&mut ps);let mut bounds=RECT::default();let _=GetClientRect(hwnd,&mut bounds);if self.painter.target.BindDC(dc,&bounds).is_ok(){self.painter.target.SetDpi(96.*scale,96.*scale);self.painter.target.BeginDraw();let max=self.hover_card.id.as_deref().map(|id|self.draw_hover_card(id,self.hover_card.scroll)).unwrap_or(0.);self.hover_card.max_scroll=max;self.hover_card.scroll=self.hover_card.scroll.clamp(0.,max);let _=self.painter.target.EndDraw(None,None);}let _=EndPaint(hwnd,&ps);Some(LRESULT(0))},
            WM_MOUSEMOVE=>{self.hover_card.last_inside=Instant::now();if let Some(grab)=self.hover_card.drag {let pane=reply_bottom-REPLY_TOP;let max=self.hover_card.max_scroll;let thumb=(pane*pane/(pane+max)).max(22.).min(pane);if max>0. {self.hover_card.scroll=((y-REPLY_TOP-grab)/(pane-thumb)).clamp(0.,1.)*max;let _=InvalidateRect(hwnd,None,false);}}Some(LRESULT(0))},
            WM_MOUSEWHEEL=>{let delta=(w.0>>16) as i16;self.hover_card.scroll=wheel_scroll(self.hover_card.scroll,delta,self.hover_card.max_scroll);self.hover_card.last_inside=Instant::now();let _=InvalidateRect(hwnd,None,false);Some(LRESULT(0))},
            WM_LBUTTONDOWN if x>=WIDTH-24. && y>=REPLY_TOP && y<reply_bottom && self.hover_card.max_scroll>0.=>{let pane=reply_bottom-REPLY_TOP;let max=self.hover_card.max_scroll;let thumb=(pane*pane/(pane+max)).max(22.).min(pane);let top=REPLY_TOP+(pane-thumb)*self.hover_card.scroll/max;self.hover_card.drag=Some(if y>=top&&y<=top+thumb{y-top}else{thumb/2.});let _=SetCapture(hwnd);Some(LRESULT(0))},
            WM_LBUTTONUP=>{if self.hover_card.drag.take().is_some(){let _=ReleaseCapture();}Some(LRESULT(0))},
            _=>None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{wheel_scroll,vertical_placement};
    #[test]
    fn wheel_scrolling_stays_inside_reply_bounds(){
        assert_eq!(wheel_scroll(0.,-120,130.),54.);
        assert_eq!(wheel_scroll(120.,-120,130.),130.);
        assert_eq!(wheel_scroll(10.,120,130.),0.);
        assert_eq!(wheel_scroll(0.,-120,0.),0.);
    }
    #[test]
    fn placement_never_overlaps_hovered_row() {
        for (top,bottom,work_bottom) in [(80,128,1000),(700,748,1000),(210,258,480),(110,158,260)] {
            if let Some((y,h))=vertical_placement(top,bottom,0,work_bottom,260,90) {assert!(y>=bottom||y+h<=top);assert!(y>=0&&y+h<=work_bottom);}
        }
    }
}
