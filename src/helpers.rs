//! Helper programs: cls-host-x86/x64.exe (CLS codec DLLs) and unarc-host-x86.exe (unarc.dll).
//!
//! Release builds (`--features embed-helpers`, see build.ps1) carry them inside the main .exe,
//! so a single downloaded arc-unpack-gui.exe works on its own; they are written to the cache
//! on first use. Development builds find them next to the executable.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[cfg(feature = "embed-helpers")]
const EMBEDDED: &[(&str, &[u8])] = &[
    ("cls-host-x86.exe", include_bytes!(concat!(env!("ARC_UNPACK_EMBED_DIR"), "/cls-host-x86.exe"))),
    ("cls-host-x64.exe", include_bytes!(concat!(env!("ARC_UNPACK_EMBED_DIR"), "/cls-host-x64.exe"))),
    ("unarc-host-x86.exe", include_bytes!(concat!(env!("ARC_UNPACK_EMBED_DIR"), "/unarc-host-x86.exe"))),
    (XTOOL_TIMER, include_bytes!(concat!(env!("ARC_UNPACK_EMBED_DIR"), "/zz-arc-unpack-timer.dll"))),
];
#[cfg(not(feature = "embed-helpers"))]
const EMBEDDED: &[(&str, &[u8])] = &[];

/// Finds the first of `names`: in ARC_UNPACK_HOST_DIR, next to the executable, or (release
/// builds) unpacked from the executable itself.
pub fn find(names: &[&str], host_dir: &Path) -> Result<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = std::env::var_os("ARC_UNPACK_HOST_DIR") {
        dirs.push(PathBuf::from(d));
    }
    dirs.push(host_dir.to_path_buf());
    for d in &dirs {
        if let Some(p) = names.iter().map(|n| d.join(n)).find(|p| p.is_file()) {
            return Ok(p);
        }
    }
    for name in names {
        if let Some((_, bytes)) = EMBEDDED.iter().find(|(n, _)| n == name) {
            return unpack(name, bytes);
        }
    }
    anyhow::bail!("{} not found next to arc-unpack (build it with build.ps1)", names[0])
}

fn unpack(name: &str, bytes: &[u8]) -> Result<PathBuf> {
    // Keyed by content, so a changed helper never reuses a stale copy from another build.
    let digest = sha1_smol::Sha1::from(bytes).digest().to_string();
    unpack_into(&crate::install::default_cache_dir().join("helpers").join(&digest[..12]), name, bytes, 50)
}

fn unpack_into(dir: &Path, name: &str, bytes: &[u8], tries: u32) -> Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = dir.join(name);
    if path.is_file() {
        return Ok(path);
    }
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    // Write next to the target and rename, so a half-written helper is never executed. The
    // temporary name is unique per call: several extractions in one process can unpack the
    // same helper at once.
    let tmp = dir.join(format!("{name}.{}-{}.tmp", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    // A virus scanner checking the new program can hold it open for a moment, which makes the
    // rename fail: try again for a few seconds.
    let mut error = None;
    for _ in 0..tries {
        match std::fs::rename(&tmp, &path) {
            Ok(()) => {
                error = None;
                break;
            }
            // Another extraction got there first; its copy is identical.
            Err(_) if path.is_file() => {
                error = None;
                break;
            }
            Err(e) => error = Some(e),
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = std::fs::remove_file(&tmp);
    if let Some(e) = error {
        return Err(blocked(&path, &format!("it could not be put in place ({e})")));
    }
    if !path.is_file() {
        return Err(blocked(&path, "it was removed right after it was written"));
    }
    Ok(path)
}

/// The error for a helper program that could not be written or was removed.
fn blocked(path: &Path, what: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "arc-unpack's helper {} is missing: {what}. A virus scanner has probably blocked or removed it. Allow \
         the folder {} in your virus scanner (or restore the file from its quarantine) and try again.",
        path.display(),
        path.parent().unwrap_or(path).display()
    )
}

/// For a program that failed to start because it isn't there: if it is one of arc-unpack's
/// own helpers, the reason (a virus scanner removing it after it was written).
pub fn missing_hint(program: &Path) -> Option<String> {
    let helpers = crate::install::default_cache_dir().join("helpers");
    (program.starts_with(&helpers) && !program.is_file()).then(|| format!("{:#}", blocked(program, "it was removed after it was written")))
}

/// The xtool plugin from `xtool-timer/`. Named to sort last among the plugins xtool finds.
pub const XTOOL_TIMER: &str = "zz-arc-unpack-timer.dll";

/// xtool (the precompressor most repacks use) coordinates its threads with `Sleep(1)`, which
/// lasts ~15.6 ms at Windows' default timer resolution; a tiny plugin next to xtool.exe makes
/// xtool ask for 1 ms, and keeps xtool's view of free memory steady, which avoids an xtool bug
/// that restores large deduplicated files wrong when free memory rises during a decode (see
/// `xtool-timer/src/lib.rs`). This puts the plugin next to every 64-bit
/// `xtool*.exe` in `codec_dir` and its subfolders, or removes it when
/// `ARC_UNPACK_NO_XTOOL_TIMER` is set. Failures are ignored (as without the plugin).
pub fn place_xtool_timer(codec_dir: &Path, host_dir: &Path) {
    let enabled = std::env::var_os("ARC_UNPACK_NO_XTOOL_TIMER").is_none();
    let mut dirs = vec![codec_dir.to_path_buf()];
    dirs.extend(std::fs::read_dir(codec_dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()));
    let mut plugin: Option<Option<PathBuf>> = None;
    for dir in dirs {
        let has_xtool = std::fs::read_dir(&dir).into_iter().flatten().flatten().any(|e| {
            let name = e.file_name().to_string_lossy().to_ascii_lowercase();
            name.starts_with("xtool") && name.ends_with(".exe") && crate::stage::pe_machine(&e.path()).is_ok_and(|m| m == 0x8664)
        });
        let dest = dir.join(XTOOL_TIMER);
        if !enabled || !has_xtool {
            if dest.is_file() {
                let _ = std::fs::remove_file(&dest);
            }
            continue;
        }
        let Some(src) = plugin.get_or_insert_with(|| find(&[XTOOL_TIMER], host_dir).ok()).clone() else { return };
        let same = |a: &Path, b: &Path| std::fs::metadata(a).ok().map(|m| m.len()) == std::fs::metadata(b).ok().map(|m| m.len());
        if !dest.is_file() || !same(&src, &dest) {
            let _ = std::fs::copy(&src, &dest);
        }
    }
}

/// True when this build carries its helpers inside the executable.
pub fn embedded() -> bool {
    !EMBEDDED.is_empty()
}

#[cfg(test)]
mod tests {
    #[test]
    fn unpacks_once_even_from_many_threads() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = vec![7u8; 1 << 20];
        let paths: Vec<_> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..8).map(|_| s.spawn(|| super::unpack_into(dir.path(), "h.exe", &bytes, 50).unwrap())).collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(paths.iter().all(|p| p == &dir.path().join("h.exe")));
        assert_eq!(std::fs::read(dir.path().join("h.exe")).unwrap(), bytes);
        // Nothing else is left behind.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    /// A helper that can't be put in place is an error that says why, never a path to a file
    /// that isn't there.
    #[test]
    fn a_blocked_helper_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("h.exe")).unwrap(); // the rename can't replace this
        let e = super::unpack_into(dir.path(), "h.exe", b"MZ", 2).unwrap_err().to_string();
        assert!(e.contains("could not be put in place") && e.contains("virus scanner"), "{e}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "temporary file left behind");
    }

    /// With `--features embed-helpers`, a helper that is not next to the executable is
    /// unpacked from the binary itself and is a valid PE file of the right architecture.
    #[cfg(feature = "embed-helpers")]
    #[test]
    fn embedded_helpers_unpack() {
        let p = super::find(&["cls-host-x86.exe"], std::path::Path::new("Z:/nonexistent")).unwrap();
        assert!(p.starts_with(crate::install::default_cache_dir()));
        assert_eq!(crate::stage::pe_machine(&p).unwrap(), 0x14c);
        let p = super::find(&["unarc-host-x86.exe"], std::path::Path::new("Z:/nonexistent")).unwrap();
        assert_eq!(crate::stage::pe_machine(&p).unwrap(), 0x14c);
    }
}
