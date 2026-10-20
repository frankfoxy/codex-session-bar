use crate::collect::Snapshot;
use anyhow::{Context, Result};
use flate2::{read::GzDecoder, Compression, GzBuilder};
use serde_json::json;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufReader, Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub struct Recorder {
    root: PathBuf,
    max_bytes: u64,
    keep_archives: usize,
}

impl Recorder {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            max_bytes: 16 * 1024 * 1024,
            keep_archives: 7,
        }
    }

    pub fn record(&self, s: &Snapshot) -> Result<()> {
        let target = self.root.join("samples.jsonl");
        let mut bytes = Vec::with_capacity(256 * 1024);
        serde_json::to_writer(&mut bytes, s)?;
        anyhow::ensure!(bytes.len() <= 8 * 1024 * 1024, "Sample exceeds 8 MiB bound");
        bytes.push(b'\n');
        let mut file = OpenOptions::new().create(true).append(true).open(target)?;
        file.write_all(&bytes)?;
        file.sync_data()?;
        drop(file);
        let pending = self.root.join("latest.pending");
        let mut f = File::create(&pending)?;
        serde_json::to_writer_pretty(
            &mut f,
            &json!({"monitor_pid":std::process::id(),"time":s.time,"epoch":s.epoch,"interval_seconds":s.interval_seconds,"collection_ms":s.collection_ms,"system":s.system,"process_count":s.processes.len(),"top_processes":s.processes.iter().take(20).collect::<Vec<_>>(),"top_groups":s.groups.iter().take(20).collect::<Vec<_>>()}),
        )?;
        f.sync_data()?;
        drop(f);
        fs::rename(pending, self.root.join("latest.json"))?;
        // Persist the fresh sample before any archive work. The UI stays on its own thread.
        self.maintain()
    }

    fn maintain(&self) -> Result<()> {
        let active = self.root.join("samples.jsonl");
        if fs::metadata(&active).is_ok_and(|m| m.len() >= self.max_bytes) {
            let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.9fZ");
            let closed = self
                .root
                .join(format!("samples.{stamp}-{}.jsonl", std::process::id()));
            anyhow::ensure!(!closed.exists(), "Archive name already exists");
            fs::rename(&active, closed)?;
        }
        // Also migrate the old numbered files, and retry any closed file left by an
        // interrupted compression. Never touch the active file or unrelated logs.
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type()?.is_file() && closed_name(&name, ".jsonl") {
                self.compress(&entry.path())?;
            }
        }
        let mut archives = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type()?.is_file() && closed_name(&name, ".jsonl.gz") {
                let decoder = GzDecoder::new(File::open(entry.path())?);
                let header = decoder.header().context("Invalid gzip archive header")?;
                archives.push((header.mtime(), name, entry.path()));
            }
        }
        archives.sort();
        for (_, _, path) in archives
            .iter()
            .take(archives.len().saturating_sub(self.keep_archives))
        {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    pub(crate) fn compress(&self, source: &Path) -> Result<()> {
        let archive = source.with_extension("jsonl.gz");
        if !archive.exists() {
            let pending = source.with_extension("jsonl.gz.pending");
            // A previous interrupted attempt may leave only this disposable output.
            // Keep the source untouched until the completed archive passes validation.
            let output = File::create(&pending)?;
            let modified = fs::metadata(source)?
                .modified()
                .unwrap_or(SystemTime::now());
            let mtime = modified.duration_since(UNIX_EPOCH)?.as_secs() as u32;
            let mut encoder = GzBuilder::new()
                .mtime(mtime)
                .write(output, Compression::default());
            io::copy(&mut BufReader::new(File::open(source)?), &mut encoder)?;
            encoder.finish()?.sync_all()?;
            verify_archive(source, &pending)?;
            fs::rename(&pending, &archive)?;
        } else {
            // Recover safely after a crash between publishing gzip and deleting source.
            verify_archive(source, &archive)?;
        }
        fs::remove_file(source)?;
        Ok(())
    }
}

fn closed_name(name: &str, suffix: &str) -> bool {
    let Some(id) = name
        .strip_prefix("samples.")
        .and_then(|s| s.strip_suffix(suffix))
    else {
        return false;
    };
    // Only our numbered legacy files and timestamped rotations are managed here.
    matches!(id, "1" | "2" | "3" | "4" | "5" | "6" | "7")
        || (id.len() > 25
            && id.contains('T')
            && id.contains('Z')
            && id
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, 'T' | 'Z' | '.' | '-')))
}

fn verify_archive(source: &Path, archive: &Path) -> Result<()> {
    let mut original = BufReader::new(File::open(source)?);
    let mut decoded = GzDecoder::new(BufReader::new(File::open(archive)?));
    let mut a = [0u8; 64 * 1024];
    let mut b = [0u8; 64 * 1024];
    loop {
        let n = original.read(&mut a)?;
        if n == 0 {
            anyhow::ensure!(decoded.read(&mut b[..1])? == 0, "Archive has extra data");
            break;
        }
        decoded.read_exact(&mut b[..n])?;
        anyhow::ensure!(a[..n] == b[..n], "Archive does not match source");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let workspace = crate::workspace_root(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
            let temp = workspace.join("tmp");
            let root = temp.join(format!(
                "memory-watch-compression-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            assert_eq!(root.parent(), Some(temp.as_path()));
            fs::create_dir(&root).unwrap();
            Self(root)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn migrates_legacy_and_keeps_active_sample() {
        let fixture = Fixture::new();
        let source = fixture.0.join("samples.1.jsonl");
        let data = b"{\"pid\":123,\"private_bytes\":456}\n".repeat(8192);
        fs::write(&source, &data).unwrap();
        fs::write(fixture.0.join("samples.jsonl"), b"active\n").unwrap();
        fs::write(fixture.0.join("unrelated.jsonl"), b"untouched").unwrap();
        Recorder::new(fixture.0.clone()).maintain().unwrap();
        assert!(!source.exists());
        let archive = fixture.0.join("samples.1.jsonl.gz");
        assert!(fs::metadata(&archive).unwrap().len() < data.len() as u64 / 10);
        let mut restored = Vec::new();
        GzDecoder::new(File::open(archive).unwrap())
            .read_to_end(&mut restored)
            .unwrap();
        assert_eq!(restored, data);
        assert_eq!(
            fs::read(fixture.0.join("samples.jsonl")).unwrap(),
            b"active\n"
        );
        assert!(fixture.0.join("unrelated.jsonl").exists());
    }

    #[test]
    fn rotation_retention_and_interrupted_output_recovery() {
        let fixture = Fixture::new();
        let recorder = Recorder {
            root: fixture.0.clone(),
            max_bytes: 32,
            keep_archives: 2,
        };
        for i in 0..4 {
            fs::write(fixture.0.join("samples.jsonl"), vec![i; 128]).unwrap();
            recorder.maintain().unwrap();
        }
        let count = fs::read_dir(&fixture.0)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|s| s == "gz"))
            .count();
        assert_eq!(count, 2);
        let source = fixture.0.join("samples.1.jsonl");
        fs::write(&source, b"complete source").unwrap();
        fs::write(source.with_extension("jsonl.gz.pending"), b"interrupted").unwrap();
        recorder.compress(&source).unwrap();
        // Simulate a restart after gzip publication but before source removal.
        fs::write(&source, b"complete source").unwrap();
        recorder.compress(&source).unwrap();
        assert!(!source.exists());
    }

    #[test]
    fn corrupt_archive_never_deletes_source() {
        let fixture = Fixture::new();
        let source = fixture.0.join("samples.1.jsonl");
        fs::write(&source, b"evidence").unwrap();
        fs::write(source.with_extension("jsonl.gz"), b"bad gzip").unwrap();
        assert!(Recorder::new(fixture.0.clone()).compress(&source).is_err());
        assert_eq!(fs::read(&source).unwrap(), b"evidence");
    }
}
