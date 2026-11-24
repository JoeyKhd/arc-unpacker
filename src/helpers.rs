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
    let dir = crate::install::default_cache_dir().join("helpers").join(&digest[..12]);
    let path = dir.join(name);
    if !path.is_file() {
        std::fs::create_dir_all(&dir)?;
        // Write next to the target and rename, so a half-written helper is never executed.
        let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
        if std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp); // another process won the race; theirs is identical
        }
    }
    Ok(path)
}

/// The xtool plugin from `xtool-timer/`. Named to sort last among the plugins xtool finds.
pub const XTOOL_TIMER: &str = "zz-arc-unpack-timer.dll";

/// xtool (the precompressor most repacks use) coordinates its threads with `Sleep(1)`, which
/// lasts ~15.6 ms at Windows' default timer resolution; a tiny plugin next to xtool.exe makes
/// xtool ask for 1 ms (see `xtool-timer/src/lib.rs`). This puts the plugin next to every 64-bit
/// `xtool*.exe` in `codec_dir` and its subfolders, or removes it when
/// `ARC_UNPACK_NO_XTOOL_TIMER` is set. Failures are ignored: it is only a speed-up.
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
