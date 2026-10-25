//! Inert process fixture compiled only by Update-Installer.ps1.
use std::{path::Path, time::Duration};
fn main() {
    let version=env!("FIXTURE_VERSION");
    let arg=std::env::args().nth(1).unwrap_or_default();
    if arg=="--version" {println!("Codex Session Bar {version}");return;}
    if arg=="--parent" {
        let parent=std::env::current_exe().unwrap();let directory=parent.parent().unwrap();
        while !directory.join("exit-parent").exists() {std::thread::sleep(Duration::from_millis(30));}
        return;
    }
    let exe=std::env::current_exe().unwrap();
    std::fs::write(exe.parent().unwrap_or(Path::new(".")).join(format!("started-{version}.txt")),b"started").unwrap();
}
