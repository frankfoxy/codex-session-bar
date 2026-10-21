impl App {
    unsafe fn draw_guard_preferences(&self,width:f32,height:f32)->Vec<Area> {
        let p=&self.painter;let mut hits=Vec::new();
        hits.push(Area{hit:Hit::RefreshBack,r:rect(14.,8.,32.,30.)});p.action_icon(&Hit::BackPreferences,18.,13.);
        p.ui_label("Protection limits",rect(54.,8.,width-70.,28.),14.,TEXT,false);
        for (field,label,step) in [(0,"Process limit",1),(1,"Total private memory",256),(2,"Normal check interval",1)] {
            let y=67.+field as f32*51.;p.ui_label(label,rect(20.,y,width-185.,32.),12.,TEXT,false);
            p.fill(rect(width-166.,y,146.,32.),0x1c2125,6.);
            for(delta,x)in[(-step,width-162.),(step,width-54.)]{
                hits.push(Area{hit:Hit::GuardLimit(field,delta),r:rect(x,y,30.,32.)});
                p.line(x+10.,y+16.,x+20.,y+16.,MUTED,1.5);if delta>0{p.line(x+15.,y+11.,x+15.,y+21.,MUTED,1.5);}
            }
            let text=match field{0=>self.guard_limits.max_processes.to_string(),1=>format!("{} MiB",self.guard_limits.max_private_mib),_=>format!("{} s",self.guard_limits.interval_seconds)};
            p.label(&text,rect(width-128.,y,81.,32.),11.,TEXT,false);
        }
        for (i,text) in ["After a trigger: 1s checks until 60s stable.","Alerts and recording cannot pause enforcement.","Only verified Codex read-only Git queries are ended."].iter().enumerate(){
            p.ui_label(text,rect(20.,239.+i as f32*27.,width-40.,25.),11.,MUTED,false);
        }
        let button=rect(width-52.,height-75.,36.,32.);hits.push(Area{hit:Hit::ApplyPreferences,r:button});
        p.fill(button,0x293f37,5.);p.action_icon(&Hit::ApplyPreferences,width-46.,height-69.);
        p.ui_label(&self.preferences_note,rect(20.,height-34.,width-40.,25.),11.,if self.preferences_note.starts_with("Saved"){GREEN}else{ORANGE},false);
        hits
    }
}
unsafe fn verify_guard_preferences(app:&mut App,paths:&Paths)->Result<()> {
    app.close_popup();
    let original=app.paths.clone();
    let fixture=paths.runtime.join(format!("guard-preferences-test-{}",std::process::id()));
    std::fs::create_dir_all(fixture.join("runtime"))?;
    app.paths.runtime=fixture.join("runtime");
    let mut config=serde_json::json!({"enabled":false,"max_processes":64,"max_private_mib":4096});
    let folder=fixture.join("memory-watch-rust");std::fs::create_dir_all(&folder)?;
    std::fs::write(folder.join("codex-git-guard-settings.json"),serde_json::to_vec(&config)?)?;
    app.popup(Popup::Preferences)?;app.hit(Hit::GuardSettings);
    assert_eq!(app.guard_limits.max_processes,64);assert!(app.preferences_guard);
    app.guard_limits=guard::Limits::default();app.hit(Hit::GuardLimit(0,1));assert_eq!(app.guard_limits.max_processes,17);
    app.hit(Hit::GuardLimit(0,-1));app.hit(Hit::ApplyPreferences);
    config=crate::read_json(&folder.join("codex-git-guard-settings.json"),4096);
    assert_eq!(config["enabled"],false);assert_eq!(config["max_processes"],16);assert_eq!(config["max_private_mib"],1024);assert_eq!(config["interval_seconds"],5);
    capture(app,Some(Popup::Preferences),560,464,&paths.runtime.join("rust-guard-settings-verification.png"))?;
    app.hit(Hit::RefreshBack);assert!(!app.preferences_guard);
    app.close_popup();app.paths=original;
    paths.write("rust-guard-settings-test.json",&json!({"configurable_limits":true,"defaults":"16 / 1024 MiB / 5s","apply_preserves_enabled_state":true,"effective_interval_display":true}));
    Ok(())
}
