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
