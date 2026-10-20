// Local copy actions. No navigation or network request is made by this menu.
struct ChatMenu { id:String, title:String, directory:String, x:f32, y:f32, cursor:usize }
impl ChatMenu {
    fn enabled(&self,item:usize)->bool {item<2||(item==2&&!self.directory.is_empty())}
    fn text(&self, item:usize)->Option<String> {
        let link=format!("codex://threads/{}",self.id);
        match item {
            0=>Some(link),
            2 if !self.directory.is_empty()=>Some(self.directory.clone()),
            _=>None,
        }
    }
}
unsafe fn copy_clipboard(owner:HWND,text:&str)->Result<()> {
    use windows::Win32::System::{DataExchange::*,Memory::*};
    let text=wide(text);
    let memory=GlobalAlloc(GMEM_MOVEABLE,text.len()*2)?;
    let pointer=GlobalLock(memory);
    if pointer.is_null(){let _=GlobalFree(memory);anyhow::bail!("Cannot allocate clipboard text");}
    std::ptr::copy_nonoverlapping(text.as_ptr(),pointer as *mut u16,text.len());
    let _=GlobalUnlock(memory);
    if let Err(error)=OpenClipboard(owner){let _=GlobalFree(memory);return Err(error.into());}
    let result=(||->Result<()> {EmptyClipboard()?;SetClipboardData(13,HANDLE(memory.0))?;Ok(())})();
    let _=CloseClipboard();
    if result.is_err(){let _=GlobalFree(memory);}
    result
}
impl App {
    unsafe fn set_copy_note(&mut self,note:String){
        self.copy_note=note;self.copy_note_until=Instant::now()+std::time::Duration::from_secs(10);
        let _=InvalidateRect(self.popup,None,false);
    }
    unsafe fn start_chat_copy(&mut self){
        if self.chat_copy.is_some(){return;}
        let Some(menu)=&self.chat_menu else{return;};
        let (paths,id,title)=(self.paths.clone(),menu.id.clone(),menu.title.clone());
        let (tx,rx)=std::sync::mpsc::sync_channel(1);self.chat_copy=Some(rx);
        self.set_copy_note("Preparing Markdown…".into());
        std::thread::spawn(move||{let result=data::Store::new(&paths).markdown(&id,&title).map_err(|e|format!("{e:#}"));let _=tx.send(result);});
    }
    unsafe fn finish_chat_copy(&mut self){
        let result=self.chat_copy.as_ref().and_then(|rx|match rx.try_recv(){Ok(value)=>Some(value),Err(std::sync::mpsc::TryRecvError::Disconnected)=>Some(Err("Markdown worker stopped".into())),Err(_)=>None});
        if let Some(result)=result {
            self.chat_copy=None;
            let result=result.and_then(|text|copy_clipboard(self.bar,&text).map_err(|e|e.to_string()));
            self.set_copy_note(match result{Ok(())=>"Copied Markdown".into(),Err(e)=>format!("Copy failed: {e}")});
        }
        if self.chat_copy.is_none()&&!self.copy_note.is_empty()&&Instant::now()>=self.copy_note_until {
            self.copy_note.clear();let _=InvalidateRect(self.popup,None,false);
        }
    }
    fn chat_menu_rect(&self)->Option<D2D_RECT_F> {self.chat_menu.as_ref().map(|menu|rect(menu.x,menu.y,310.,116.))}
    unsafe fn open_chat_menu(&mut self,x:f32,y:f32) {
        let (left,top,row_height)=match self.popup_kind {
            Some(Popup::Recent)=>(self.recent_left(),RECENT_TOP as f32,self.recent_row_height() as f32),
            Some(Popup::Fixed)=>(16.,112.,40.),_=>return,
        };
        if x<left||x>self.popup_size[0] as f32-18.||y<top||y>=top+self.list_page_rows() as f32*row_height{return;}
        let Some(&index)=self.chat_filters.get(self.scroll+((y-top)/row_height) as usize) else{return;};
        let row=&self.rows[index];let (id,title)=(row.id.clone(),row.label.clone());
        let directory=data::Store::new(&self.paths).directory(&id).unwrap_or_default();
        self.close_icon_panel();self.close_tag_editor();
        self.chat_menu=Some(ChatMenu {id,title,directory,x:x.min(self.popup_size[0] as f32-320.).max(10.),y:y.min(self.popup_size[1] as f32-126.).max(8.),cursor:0});
        self.list_pointer=None;self.hover=None;
        if !self.testing {let _=SetFocus(self.popup);}
        let _=InvalidateRect(self.popup,None,false);
    }
    fn chat_menu_hit(&self,x:f32,y:f32)->Option<Hit> {
        let r=self.chat_menu_rect()?;
        if !Self::contains_rect(r,x,y){return Some(Hit::ChatMenuClose);}
        let slot=((y-r.top-7.)/34.).floor();
        if slot>=0. && slot<3. && self.chat_menu.as_ref()?.enabled(slot as usize){Some(Hit::ChatCopy(slot as usize))}
        else{Some(Hit::ChatMenuBackground)}
    }
    unsafe fn draw_chat_menu(&self,hits:&mut Vec<Area>) {
        let Some(menu)=&self.chat_menu else{return;};let r=self.chat_menu_rect().unwrap();let p=&self.painter;
        p.fill(rect(r.left+3.,r.top+4.,310.,116.),0x15191c,8.);p.fill(r,0x20262b,8.);p.border(r,0x4a5861,8.);
        hits.push(Area{hit:Hit::ChatMenuBackground,r});
        for (slot,title) in ["Copy deeplink","Copy as Markdown","Copy working directory"].iter().enumerate(){
            let y=r.top+7.+slot as f32*34.;let enabled=menu.enabled(slot);let hit=Hit::ChatCopy(slot);
            if enabled {
                hits.push(Area{hit:hit.clone(),r:rect(r.left+5.,y,300.,32.)});
                let selected=match self.hover {Some(Hit::ChatCopy(index))=>index,_=>menu.cursor};
                if selected==slot {p.fill(rect(r.left+5.,y,300.,32.),0x35434d,5.);}
            }
            let color=if enabled{TEXT}else{0x74818a};
            p.border(rect(r.left+16.,y+10.,11.,12.),color,2.);p.border(rect(r.left+20.,y+6.,11.,12.),color,2.);
            p.ui_label(title,rect(r.left+43.,y,258.,32.),12.,color,false);
        }
    }
    unsafe fn chat_menu_key(&mut self,key:u32)->bool {
        let Some(menu)=self.chat_menu.as_mut() else{return false;};
        match key {
            k if k==VK_ESCAPE.0 as u32=>{self.chat_menu=None;}
            k if k==VK_UP.0 as u32||k==VK_DOWN.0 as u32=>{
                let count=if menu.directory.is_empty(){2}else{3};
                menu.cursor=if k==VK_UP.0 as u32{(menu.cursor+count-1)%count}else{(menu.cursor+1)%count};
            }
            k if k==VK_RETURN.0 as u32=>{let slot=menu.cursor;let _=self.hit(Hit::ChatCopy(slot));}
            _=>return true,
        }
        let _=InvalidateRect(self.popup,None,false);true
    }
}
unsafe fn verify_chat_menu_and_language(app:&mut App,paths:&Paths)->Result<()> {
    app.search.clear();app.search_body=false;app.tag_filters.clear();app.settings.compact_recent=false;
    app.popup(Popup::Recent)?;app.filter_fixed();
    let first=*app.chat_filters.first().ok_or_else(||anyhow::anyhow!("No fixture chats"))?;
    let id=app.rows[first].id.clone();
    let (scroll,cursor,window)=(app.scroll,app.chat_cursor,app.popup);
    app.open_chat_menu(app.recent_left()+60.,RECENT_TOP as f32+20.);
    let menu=app.chat_menu.as_ref().unwrap();assert_eq!(menu.id,id);
    let r=app.chat_menu_rect().unwrap();
    assert_eq!(app.chat_menu_hit(r.left+50.,r.top+20.),Some(Hit::ChatCopy(0)));
    assert_eq!(app.chat_menu_hit(r.left+50.,r.top+54.),Some(Hit::ChatCopy(1)));
    assert_eq!(app.chat_menu_hit(r.left+50.,r.top+88.),Some(Hit::ChatMenuBackground));
    let hits=capture(app,Some(Popup::Recent),app.popup_size[0] as u32,app.popup_size[1] as u32,&paths.runtime.join("rust-chat-menu-verification.png"))?;
    assert!(hits.iter().any(|a|a.hit==Hit::ChatCopy(0)),"Copy menu not rendered");
    app.chat_menu_key(VK_DOWN.0 as u32);assert_eq!(app.chat_menu.as_ref().unwrap().cursor,1);
    // testing=true explicitly skips the real clipboard operation.
    app.chat_menu_key(VK_RETURN.0 as u32);
    assert!(app.chat_menu.is_none());assert_eq!(app.popup,window);assert_eq!(app.popup_kind,Some(Popup::Recent));
    assert_eq!((app.scroll,app.chat_cursor),(scroll,cursor));
    let original_titles:Vec<_>=app.rows.iter().map(|r|r.label.clone()).collect();
    crate::locale::follow(0x804);app.tick();assert_eq!(app.ui_language,0x804);
    assert_eq!(search_cue(1),"搜索会话 / 拼音…");assert_eq!(search_cue(2),"搜索标签 / 拼音…");
    assert_eq!(wide_ui("Copy deeplink"),wide("复制会话链接"));
    app.popup(Popup::Recent)?;
    app.open_chat_menu(app.recent_left()+60.,RECENT_TOP as f32+20.);
    assert!(app.chat_menu.is_some());
    capture(app,Some(Popup::Recent),app.popup_size[0] as u32,app.popup_size[1] as u32,&paths.runtime.join("rust-chat-menu-zh-verification.png"))?;
    app.chat_menu=None;app.popup(Popup::Preferences)?;
    capture(app,Some(Popup::Preferences),app.popup_size[0] as u32,app.popup_size[1] as u32,&paths.runtime.join("rust-preferences-zh-verification.png"))?;
    assert_eq!(original_titles,app.rows.iter().map(|r|r.label.clone()).collect::<Vec<_>>());
    crate::locale::follow(0x409);app.tick();assert_eq!(app.ui_language,0x409);
    assert_eq!(search_cue(1),"Search chats / pinyin…");
    paths.write("rust-context-language-test.json",&json!({"right_click_row_identity":true,"three_copy_formats":true,"missing_directory_disabled":true,"keyboard_navigation":true,"list_scroll_and_selection_preserved":true,"clipboard_not_modified_in_test":true,"live_zh_en_switch":true,"titles_preserved":true}));
    Ok(())
}
#[cfg(test)] mod chat_copy_tests {
    use super::*;
    #[test] fn copy_formats_preserve_chat_identity_title_and_directory(){
        let menu=ChatMenu{id:"chat-id".into(),title:"相机 [联调]".into(),directory:"D:\\project\\相机".into(),x:0.,y:0.,cursor:0};
        assert_eq!(menu.text(0).unwrap(),"codex://threads/chat-id");
        assert!(menu.enabled(1));
        assert_eq!(menu.text(2).unwrap(),"D:\\project\\相机");
        assert!(ChatMenu{directory:String::new(),..menu}.text(2).is_none());
    }
}
