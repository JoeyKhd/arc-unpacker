//! A RAM limit for an install, set by the user.
//!
//! FitGirl's installers have a "limit RAM usage to 2 GB" switch. Its script then changes the
//! decoders' memory settings: srep `Memory=512m`, the lolz family `MaxMemoryUsage=1024m`
//! (lollypop2 1280m), xtool `-t4 -dm10p`. arc-unpack scales that preset to any limit: lolz gets
//! half of it, srep a quarter, xtool's deduplication memory a fifth (as a share of this PC's
//! RAM, which is how xtool takes it), and below 8 GB xtool gets two threads per GB. Besides
//! that, one memory-hungry decoder runs at a time and arc-unpack's own buffers shrink.
//!
//! The decoders take these as targets: data packed with a large dictionary still needs it.
//!
//! Every install, with or without a limit, also gets `make_safe`: a memory cap for the old lolz
//! decoder of 2018 repacks, which hangs when it is allowed too much (see `magic_cap_mb`).

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static LIMIT: AtomicU64 = AtomicU64::new(0);

/// Sets (or clears) the limit, in bytes, for this process.
pub fn set(bytes: Option<u64>) {
    LIMIT.store(bytes.unwrap_or(0), Ordering::Relaxed);
}

/// The limit in bytes, if one is set.
pub fn get() -> Option<u64> {
    Some(LIMIT.load(Ordering::Relaxed)).filter(|&b| b > 0)
}

const MB: u64 = 1 << 20;

/// MaxMemoryUsage for the lolz family, in MB.
fn lolz_mb(limit: u64) -> u64 {
    (limit / MB / 2).max(1024)
}

/// srep's Memory, in MB.
fn srep_mb(limit: u64) -> u64 {
    (limit / MB / 4).max(512)
}

/// A CLS.ini size ("75%-10m", "60%-2048m", "1024m", "2g") in MB on a PC with `physical` bytes.
fn eval_mb(expr: &str, physical: u64) -> Option<u64> {
    let mut total: i64 = 0;
    for (i, term) in expr.trim().split('-').enumerate() {
        let t = term.trim().to_ascii_lowercase();
        let (num, unit) = t.split_at(t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len()));
        let n: i64 = num.parse().ok()?;
        let mb = match unit.trim() {
            "%" | "p" => (physical / MB) as i64 * n / 100,
            "" | "m" | "mb" => n,
            "g" | "gb" => n * 1024,
            "k" | "kb" => n / 1024,
            _ => return None,
        };
        total += if i == 0 { mb } else { -mb };
    }
    Some(total.max(0) as u64)
}

/// CLS.ini with the decoders' memory settings lowered to fit `limit` (never raised).
pub fn cls_ini(text: &str, limit: u64, physical: u64) -> String {
    let mut section = String::new();
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        let ending = &line[body.len()..];
        let t = body.trim();
        if let Some(name) = t.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            section = name.trim().to_ascii_lowercase();
        } else if let Some((key, value)) = t.split_once('=') {
            let key = key.trim();
            let ours = if key.eq_ignore_ascii_case("MaxMemoryUsage") {
                // (As in FitGirl's preset: lollypop gets a quarter more than the others.)
                Some(if section.starts_with("lollypop") { lolz_mb(limit) * 5 / 4 } else { lolz_mb(limit) })
            } else if key.eq_ignore_ascii_case("Memory") && section == "srep" {
                Some(srep_mb(limit))
            } else {
                None
            };
            if let Some(ours) = ours {
                let new = eval_mb(value, physical).map_or(ours, |theirs| theirs.min(ours));
                out.push_str(&format!("{key}={new}m{ending}"));
                continue;
            }
        }
        out.push_str(line);
    }
    out
}

/// arc.ini with xtool's deduplication memory (and, for small limits, its threads) lowered.
pub fn arc_ini(text: &str, limit: u64, physical: u64) -> String {
    let dm = (100 * limit / (5 * physical.max(1))).max(1);
    let threads = (limit < 8 << 30).then(|| (2 * limit / (1 << 30)).max(4));
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let lower = line.to_ascii_lowercase();
        let xtool = lower.trim_start().starts_with("unpackcmd") && lower.contains("tool.exe") && lower.contains(" decode ");
        if !xtool {
            out.push_str(line);
            continue;
        }
        let body = line.trim_end_matches(['\r', '\n']);
        let ending = &line[body.len()..];
        let words: Vec<String> = body
            .split(' ')
            .map(|w| {
                let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
                if let Some(n) = w.strip_prefix("-dm").and_then(|r| r.strip_suffix('p')).filter(|n| digits(n)) {
                    return format!("-dm{}p", n.parse::<u64>().unwrap_or(dm).min(dm));
                }
                if let (Some(t), Some(rest)) = (threads, w.strip_prefix("-t"))
                    && digits(rest.strip_suffix('p').unwrap_or(rest))
                {
                    return format!("-t{t}");
                }
                w.to_string()
            })
            .collect();
        out.push_str(&words.join(" "));
        out.push_str(ending);
    }
    out
}

/// The most memory the old lolz decoder of 2018 repacks ("magic", `cls-magic.dll`) is given, in
/// MB: 12 GB, or 40% of this PC's RAM if that is less.
///
/// That decoder never starts decoding when it is allowed about 16 GB or more: it uses no CPU and
/// waits forever, whatever memory is free. FitGirl's scripts give it no MaxMemoryUsage unless
/// "limit RAM" is ticked, so it plans about 0.85 GB per CPU thread and hangs on PCs with 20 or
/// more threads; FitGirl's own installer hangs the same way. Measured on A Way Out, 32 threads,
/// 32 GB of RAM: no limit hangs, 16 GB hangs, 12 GB and 8 GB work, 16 threads (13.4 GB) works.
fn magic_cap_mb(physical: u64) -> u64 {
    (physical / MB * 2 / 5).min(12 * 1024)
}

/// CLS.ini with what every install needs on this PC: the old "magic" decoder's MaxMemoryUsage
/// added if it is missing, or lowered if it is above `magic_cap_mb` (never raised).
pub fn cls_ini_safe(text: &str, physical: u64) -> String {
    let cap = magic_cap_mb(physical);
    let mut section = String::new();
    let (mut has_magic, mut has_key) = (false, false);
    for line in text.lines() {
        let t = line.trim();
        if let Some(name) = t.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            section = name.trim().to_ascii_lowercase();
            has_magic |= section == "magic";
        } else if section == "magic" && t.split_once('=').is_some_and(|(k, _)| k.trim().eq_ignore_ascii_case("MaxMemoryUsage")) {
            has_key = true;
        }
    }
    if !has_magic {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 32);
    section.clear();
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        let ending = &line[body.len()..];
        let t = body.trim();
        if let Some(name) = t.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            section = name.trim().to_ascii_lowercase();
            out.push_str(line);
            if section == "magic" && !has_key {
                if ending.is_empty() {
                    out.push_str(&format!("\r\nMaxMemoryUsage={cap}m"));
                } else {
                    out.push_str(&format!("MaxMemoryUsage={cap}m{ending}"));
                }
            }
            continue;
        }
        if section == "magic"
            && let Some((key, value)) = t.split_once('=')
            && key.trim().eq_ignore_ascii_case("MaxMemoryUsage")
        {
            let new = eval_mb(value, physical).map_or(cap, |theirs| theirs.min(cap));
            out.push_str(&format!("{}={new}m{ending}", key.trim()));
            continue;
        }
        out.push_str(line);
    }
    out
}

/// Runs `edit` on the files in `dir` it picks by lowercase name (CLS.ini, arc.ini). Returns
/// whether any changed. (Both are plain ASCII/ANSI settings files; bytes outside ASCII pass
/// through.)
fn edit_files(dir: &Path, edit: impl Fn(&str, &str) -> Option<String>) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else { return false };
    let mut changed = false;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_ascii_lowercase();
        let Ok(bytes) = std::fs::read(e.path()) else { continue };
        let text: String = bytes.iter().map(|&b| b as char).collect();
        let Some(new) = edit(&name, &text) else { continue };
        let new: Vec<u8> = new.chars().map(|c| c as u8).collect();
        if new != bytes && std::fs::write(e.path(), new).is_ok() {
            changed = true;
        }
    }
    changed
}

/// Applies `cls_ini_safe` to the installer's CLS.ini in `dir`, for every install. Returns a
/// line for the log when it changed something.
pub fn make_safe(dir: &Path, physical: u64) -> Option<String> {
    let changed = edit_files(dir, |name, text| (name == "cls.ini").then(|| cls_ini_safe(text, physical)));
    changed.then(|| format!("old lolz decoder (magic): memory capped at {} MB (it hangs when allowed more)", magic_cap_mb(physical)))
}

/// Applies the limit to the installer's CLS.ini and arc.ini in `tmp`. Returns what it set, for
/// the log.
pub fn apply(tmp: &Path, limit: u64, physical: u64) -> String {
    edit_files(tmp, |name, text| match name {
        "cls.ini" => Some(cls_ini(text, limit, physical)),
        "arc.ini" => Some(arc_ini(text, limit, physical)),
        _ => None,
    });
    format!(
        "RAM limit {:.0} GB: lolz decoders {} MB, srep {} MB, one memory-hungry decoder at a time",
        limit as f64 / (1u64 << 30) as f64,
        lolz_mb(limit),
        srep_mb(limit)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1 << 30;

    #[test]
    fn cls_ini_follows_the_limit() {
        let ini = "[Srep]\r\nMemory=60%-2048m\r\nTempPath=C:\\x\r\n[Precomp]\r\nMemory=128\r\n[lollypop2]\r\nMaxThreadsUsage=100%\r\nMaxMemoryUsage=75%-10m\r\n[magic2]\r\nMaxMemoryUsage=75%-10m\r\n[magicl]\r\nldmfMaxMemoryUsage=1024m\r\nMaxMemoryUsage = 900m\r\n";
        // 8 GB on a 32 GB PC: lolz 4096m (lollypop 5120m), srep 2048m. Other keys untouched.
        let got = cls_ini(ini, 8 * GB, 32 * GB);
        assert_eq!(
            got,
            "[Srep]\r\nMemory=2048m\r\nTempPath=C:\\x\r\n[Precomp]\r\nMemory=128\r\n[lollypop2]\r\nMaxThreadsUsage=100%\r\nMaxMemoryUsage=5120m\r\n[magic2]\r\nMaxMemoryUsage=4096m\r\n[magicl]\r\nldmfMaxMemoryUsage=1024m\r\nMaxMemoryUsage=900m\r\n"
        );
        // FitGirl's own 2 GB preset: srep 512m, lolz 1024m, lollypop 1280m.
        let two = cls_ini(ini, 2 * GB, 16 * GB);
        assert!(two.contains("[Srep]\r\nMemory=512m") && two.contains("[magic2]\r\nMaxMemoryUsage=1024m") && two.contains("MaxMemoryUsage=1280m"));
        // Never raised: on a 4 GB PC "60%-2048m" is 409 MB, less than a 16 GB limit would give.
        assert!(cls_ini(ini, 16 * GB, 4 * GB).contains("[Srep]\r\nMemory=409m"));
    }

    #[test]
    fn arc_ini_lowers_xtool() {
        let ini = "[External compressor:xt3dd]\nheader = 0\nunpackcmd = xtool.exe decode -t100p -dm50p - - <stdin> <stdout>\n[External compressor:xt3]\nunpackcmd = xtool3\\xtool.exe decode -t100p - - <stdin> <stdout>\n[External compressor:rzs]\nunpackcmd = rz.exe d -t100p - -\n";
        // 8 GB of 32: dedup memory a fifth of the limit = 5% of this PC; threads stay.
        let got = arc_ini(ini, 8 * GB, 32 * GB);
        assert!(got.contains("xtool.exe decode -t100p -dm5p - - <stdin> <stdout>\n"));
        assert!(got.contains("xtool3\\xtool.exe decode -t100p - -"));
        // 4 GB: two threads per GB, and other programs' options are left alone.
        let got = arc_ini(ini, 4 * GB, 32 * GB);
        assert!(got.contains("xtool.exe decode -t8 -dm2p - -") && got.contains("xtool3\\xtool.exe decode -t8 - -"));
        assert!(got.contains("rz.exe d -t100p - -"));
    }

    #[test]
    fn the_old_magic_decoder_gets_a_safe_memory_cap() {
        // A Way Out's CLS.ini: [magic] has no MaxMemoryUsage. 32 GB PC: 40% = 13107 MB, so 12 GB.
        let ini = "[lollypop2]\r\nMaxMemoryUsage=75%-10m\r\n\r\n[magic]\r\nBufsize=512k\r\nMaxThreadsUsage=100%\r\n\r\n[magicl]\r\nldmfMaxMemoryUsage=1024\r\n";
        let got = cls_ini_safe(ini, 32 * GB);
        assert_eq!(
            got,
            "[lollypop2]\r\nMaxMemoryUsage=75%-10m\r\n\r\n[magic]\r\nMaxMemoryUsage=12288m\r\nBufsize=512k\r\nMaxThreadsUsage=100%\r\n\r\n[magicl]\r\nldmfMaxMemoryUsage=1024\r\n"
        );
        // 16 GB PC: 40% of it. A lower setting (FitGirl's "limit RAM" 1024m) stays; a higher one
        // is lowered; other decoders' sections are left alone.
        assert!(cls_ini_safe(ini, 16 * GB).contains("[magic]\r\nMaxMemoryUsage=6553m\r\n"));
        let low = "[magic]\r\nMaxMemoryUsage=1024m\r\n";
        assert_eq!(cls_ini_safe(low, 32 * GB), "[magic]\r\nMaxMemoryUsage=1024m\r\n");
        assert_eq!(cls_ini_safe("[magic]\r\nMaxMemoryUsage=75%-10m\r\n", 32 * GB), "[magic]\r\nMaxMemoryUsage=12288m\r\n");
        assert_eq!(cls_ini_safe("[magic2]\r\nMaxThreadsUsage=100%\r\n", 32 * GB), "[magic2]\r\nMaxThreadsUsage=100%\r\n");
        // A section header on the last line, without a line ending.
        assert_eq!(cls_ini_safe("[magic]", 32 * GB), "[magic]\r\nMaxMemoryUsage=12288m");
    }

    #[test]
    fn sizes() {
        assert_eq!(eval_mb("75%-10m", 16 * GB), Some(12278));
        assert_eq!(eval_mb("2g", GB), Some(2048));
        assert_eq!(eval_mb("lots", GB), None);
    }
}
