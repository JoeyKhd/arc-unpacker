//! 7z archives, unpacked the way ISDone's IS7zipExtract does it. ISDone 0.6 loads the 7zxa.dll
//! 9.17 it carries inside itself (not the installer's 7z.dll) and writes every file of the
//! archive into the output folder, in archive order:
//! - folder entries create nothing (no empty folders);
//! - files are opened with CREATE_ALWAYS and get the extraction time, no stored times or
//!   attributes (ISDone imports neither SetFileTime nor SetFileAttributes);
//! - names go through the ANSI code page (CreateFileA), so a character it can't represent makes
//!   the file fail;
//! - a CRC error doesn't stop it (its SetOperationResult always answers S_OK): ISDone returns
//!   TRUE with the damaged file. Here that is a problem in the install's report.
//!
//! Farming Simulator 17 rebuilds `{app}\inner.pack` with builder.exe and unpacks it this way.

use anyhow::{Context, Result, bail};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// What one archive gave.
#[derive(Debug, Default)]
pub struct Report {
    pub files: usize,
    pub bytes: u64,
    /// Files whose data failed the archive's CRC (the rest of their block was not unpacked).
    pub problems: Vec<String>,
}

/// Unpacks `archive` into `out`. An error means ISDone would have returned FALSE (archive missing
/// or unreadable, a method it doesn't know, a file it couldn't create).
pub fn extract(archive: &Path, out: &Path, password: &str, cancel: &AtomicBool) -> Result<Report> {
    if !archive.is_file() {
        bail!("{} is missing", archive.display());
    }
    let mut reader = sevenz_rust2::ArchiveReader::open(archive, sevenz_rust2::Password::from(password))
        .map_err(|e| anyhow::anyhow!("{}", describe(&e)))
        .with_context(|| format!("open {}", archive.display()))?;
    let mut report = Report::default();
    let mut failure: Option<anyhow::Error> = None;
    let result = reader.for_each_entries_with_decision(|entry, data| {
        use sevenz_rust2::EntryDecision;
        if cancel.load(Ordering::Relaxed) {
            failure = Some(anyhow::anyhow!("cancelled"));
            return Ok(EntryDecision::Stop);
        }
        if entry.is_directory() {
            return Ok(EntryDecision::Continue);
        }
        let path = match target(out, entry.name()) {
            Ok(p) => p,
            Err(e) => {
                failure = Some(e);
                return Ok(EntryDecision::Stop);
            }
        };
        let written = write_file(&path, data);
        match written {
            Ok(n) => {
                report.files += 1;
                report.bytes += n;
                Ok(EntryDecision::Continue)
            }
            Err(WriteError::Crc) => {
                // The data is all written; the crate can't go on in this block after a CRC error.
                report.files += 1;
                report.problems.push(format!("{}: {} failed its 7z CRC check", name_of(archive), entry.name()));
                Ok(EntryDecision::SkipBlock)
            }
            Err(WriteError::Other(e)) => {
                failure = Some(e.context(format!("{}: {}", name_of(archive), entry.name())));
                Ok(EntryDecision::Stop)
            }
        }
    });
    if let Some(e) = failure {
        return Err(e);
    }
    result.map_err(|e| anyhow::anyhow!("{}: {}", name_of(archive), describe(&e)))?;
    Ok(report)
}

enum WriteError {
    Crc,
    Other(anyhow::Error),
}

fn write_file(path: &Path, data: &mut dyn Read) -> std::result::Result<u64, WriteError> {
    let other = |e: std::io::Error, what: &str| WriteError::Other(anyhow::Error::new(e).context(format!("{what} {}", path.display())));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| other(e, "create folder for"))?;
    }
    // CREATE_ALWAYS: an existing file is truncated (a read-only one fails, as in ISDone).
    let file = std::fs::File::create(path).map_err(|e| other(e, "create"))?;
    let mut w = BufWriter::with_capacity(1 << 20, file);
    let n = match std::io::copy(data, &mut w) {
        Ok(n) => n,
        Err(e) if is_crc(&e) => {
            w.flush().map_err(|e| other(e, "write"))?;
            return Err(WriteError::Crc);
        }
        Err(e) => return Err(WriteError::Other(anyhow::anyhow!("unpack {}: {}", path.display(), describe_io(&e)))),
    };
    w.flush().map_err(|e| other(e, "write"))?;
    Ok(n)
}

fn is_crc(e: &std::io::Error) -> bool {
    e.get_ref()
        .and_then(|inner| inner.downcast_ref::<sevenz_rust2::Error>())
        .is_some_and(|inner| matches!(inner, sevenz_rust2::Error::ChecksumVerificationFailed))
}

fn describe_io(e: &std::io::Error) -> String {
    match e.get_ref().and_then(|inner| inner.downcast_ref::<sevenz_rust2::Error>()) {
        Some(inner) => describe(inner),
        None => e.to_string(),
    }
}

fn describe(e: &sevenz_rust2::Error) -> String {
    match e {
        sevenz_rust2::Error::UnsupportedCompressionMethod(m) => format!("unsupported 7z method {m}"),
        sevenz_rust2::Error::PasswordRequired => "the archive needs a password".into(),
        other => other.to_string(),
    }
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// `out` + the entry's path, as ISDone builds it (OutPath + '\' + name), but never outside `out`.
fn target(out: &Path, name: &str) -> Result<PathBuf> {
    let name = ansi(name);
    let mut p = out.to_path_buf();
    let mut any = false;
    for part in name.split(['\\', '/']) {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." || part.contains(':') {
            bail!("unsafe path in the archive: {name}");
        }
        // A character the ANSI code page doesn't have became '?': CreateFileA fails on it.
        if part.contains(['?', '*', '"', '<', '>', '|']) {
            bail!("ISDone can't create {name} (not representable in the ANSI code page)");
        }
        p.push(part);
        any = true;
    }
    if !any {
        bail!("empty path in the archive");
    }
    Ok(p)
}

/// The name as ISDone's CreateFileA sees it: through the ANSI code page and back (best fit, so
/// e.g. some accented letters lose their accent and others become '?').
#[cfg(windows)]
fn ansi(name: &str) -> String {
    use windows_sys::Win32::Globalization::{CP_ACP, MultiByteToWideChar, WideCharToMultiByte};
    let wide: Vec<u16> = name.encode_utf16().collect();
    if wide.is_empty() || name.is_ascii() {
        return name.to_string();
    }
    unsafe {
        let n = WideCharToMultiByte(CP_ACP, 0, wide.as_ptr(), wide.len() as i32, std::ptr::null_mut(), 0, std::ptr::null(), std::ptr::null_mut());
        if n <= 0 {
            return "?".into();
        }
        let mut bytes = vec![0u8; n as usize];
        WideCharToMultiByte(CP_ACP, 0, wide.as_ptr(), wide.len() as i32, bytes.as_mut_ptr(), n, std::ptr::null(), std::ptr::null_mut());
        let m = MultiByteToWideChar(CP_ACP, 0, bytes.as_ptr(), n, std::ptr::null_mut(), 0);
        if m <= 0 {
            return "?".into();
        }
        let mut back = vec![0u16; m as usize];
        MultiByteToWideChar(CP_ACP, 0, bytes.as_ptr(), n, back.as_mut_ptr(), m);
        String::from_utf16_lossy(&back)
    }
}

#[cfg(not(windows))]
fn ansi(name: &str) -> String {
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(dir: &Path) -> PathBuf {
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("sub").join("deeper")).unwrap();
        std::fs::create_dir_all(src.join("empty")).unwrap();
        std::fs::write(src.join("a.txt"), b"hello 7z").unwrap();
        std::fs::write(src.join("zero.bin"), b"").unwrap();
        std::fs::write(src.join("sub").join("deeper").join("b.bin"), vec![7u8; 300_000]).unwrap();
        let archive = dir.join("inner.pack");
        sevenz_rust2::compress_to_path(&src, &archive).unwrap();
        archive
    }

    #[test]
    fn unpacks_files_but_not_empty_folders() {
        let dir = tempfile::tempdir().unwrap();
        let archive = sample(dir.path());
        let out = dir.path().join("out");
        let r = extract(&archive, &out, "", &AtomicBool::new(false)).unwrap();
        assert_eq!(r.files, 3, "{r:?}");
        assert!(r.problems.is_empty());
        assert_eq!(std::fs::read(out.join("a.txt")).unwrap(), b"hello 7z");
        assert_eq!(std::fs::read(out.join("zero.bin")).unwrap(), b"");
        assert_eq!(std::fs::read(out.join("sub").join("deeper").join("b.bin")).unwrap(), vec![7u8; 300_000]);
        assert!(!out.join("empty").exists(), "ISDone creates no empty folders");
    }

    #[test]
    fn overwrites_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let archive = sample(dir.path());
        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("a.txt"), b"an older and longer file").unwrap();
        extract(&archive, &out, "", &AtomicBool::new(false)).unwrap();
        assert_eq!(std::fs::read(out.join("a.txt")).unwrap(), b"hello 7z");
    }

    #[test]
    fn a_missing_or_broken_archive_fails() {
        let dir = tempfile::tempdir().unwrap();
        assert!(extract(&dir.path().join("none.7z"), dir.path(), "", &AtomicBool::new(false)).is_err());
        let bad = dir.path().join("bad.7z");
        std::fs::write(&bad, b"not a 7z archive at all").unwrap();
        assert!(extract(&bad, dir.path(), "", &AtomicBool::new(false)).is_err());
    }

    #[test]
    fn paths_stay_inside_the_output_folder() {
        let out = Path::new(r"C:\Games\Game");
        assert_eq!(target(out, "pdlc/a.zip").unwrap(), out.join("pdlc").join("a.zip"));
        assert_eq!(target(out, r"\sdk\x.zip").unwrap(), out.join("sdk").join("x.zip"));
        assert!(target(out, "../evil.dll").is_err());
        assert!(target(out, "C:/evil.dll").is_err());
        assert!(target(out, "").is_err());
    }
}
