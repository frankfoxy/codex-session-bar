//! Compressed rotation for Git protection events only.
use anyhow::Result;
use flate2::{read::GzDecoder, Compression, GzBuilder};
use std::{fs::{self, File}, io::{self, BufReader, Read}, path::Path, time::{SystemTime, UNIX_EPOCH}};
pub fn compress(source: &Path) -> Result<()> {
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


#[cfg(test)] mod tests {
    use super::*;
    #[test] fn compressed_events_are_verified_before_removing_source() {
        let temp = super::super::test_workspace().join("tmp").canonicalize().unwrap();
        let folder = temp.join(format!("guard-archive-test-{}", std::process::id()));
        fs::create_dir_all(&folder).unwrap();
        let source = folder.join("events.1.jsonl");
        fs::write(&source, b"{\"type\":\"storm_started\"}\n").unwrap();
        compress(&source).unwrap();
        assert!(!source.exists());
        assert!(source.with_extension("jsonl.gz").exists());
        fs::write(&source, b"different").unwrap();
        assert!(compress(&source).is_err());
        assert!(source.exists());
        fs::remove_dir_all(folder).unwrap();
    }
}
