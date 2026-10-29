//! One-time, non-destructive migration from the old workspace runtime.
use anyhow::Result;
use std::{fs, path::Path};

pub fn temporary_root()->std::path::PathBuf {
    let inherited=std::env::temp_dir();
    // Older update helpers launched the app with TEMP set to the update folder.
    // Do not keep nesting runtime directories inside that disposable folder.
    if inherited.file_name().is_some_and(|name|name.to_string_lossy().starts_with("session-bar-update-"))
        && inherited.join("plan.json").is_file() {
        let plan=crate::read_json(&inherited.join("plan.json"),65536);
        if let Some(saved)=plan["system_tmp"].as_str().map(std::path::PathBuf::from).filter(|p|p.is_absolute()) {return saved;}
        if let Some(local)=std::env::var_os("LOCALAPPDATA") {return std::path::PathBuf::from(local).join("Temp");}
    }
    inherited
}

pub fn migrate(legacy:&Path,destination:&Path)->Result<()> {
    if legacy==destination || destination.join("legacy-migration.json").exists() {return Ok(());}
    fs::create_dir_all(destination)?;
    let mut names:Vec<String>=["capacity-retry-state.json","reset-notice-reminders.json","reset-notice-cache.json","recent-cache.json","update-status.json"].iter().map(|s|s.to_string()).collect();
    for prefix in ["capacity-retry-events","reset-notice-checks"] {
        names.push(format!("{prefix}.jsonl"));
        for i in 1..=4 {names.push(format!("{prefix}.{i}.gz"));}
    }
    for name in names {
        let source=legacy.join(&name);let target=destination.join(&name);
        if !fs::metadata(&source).is_ok_and(|m|m.is_file()&&m.len()<=1024*1024) {continue;}
        match fs::OpenOptions::new().write(true).create_new(true).open(&target) {
            Ok(mut output)=>{let mut input=fs::File::open(source)?;std::io::copy(&mut input,&mut output)?;},
            Err(error) if error.kind()==std::io::ErrorKind::AlreadyExists=>{},
            Err(error)=>return Err(error.into()),
        }
    }
    fs::write(destination.join("legacy-migration.json"),br#"{"version":1}"#)?;
    Ok(())
}

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn migration_preserves_retry_claims_and_does_not_overwrite_or_repeat() {
        let root=super::super::guard::test_workspace().join("tmp").canonicalize().unwrap();
        let fixture=root.join(format!("runtime-migration-{}",std::process::id()));
        let old=fixture.join("old");let new=fixture.join("new");fs::create_dir_all(&old).unwrap();
        fs::write(old.join("capacity-retry-state.json"),b"original claim").unwrap();
        migrate(&old,&new).unwrap();assert_eq!(fs::read(new.join("capacity-retry-state.json")).unwrap(),b"original claim");
        fs::write(old.join("capacity-retry-state.json"),b"stale source").unwrap();
        migrate(&old,&new).unwrap();assert_eq!(fs::read(new.join("capacity-retry-state.json")).unwrap(),b"original claim");
        assert!(old.join("capacity-retry-state.json").is_file());fs::remove_dir_all(fixture).unwrap();
    }
}
