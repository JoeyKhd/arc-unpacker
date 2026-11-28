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
//! Every install, with or without a limit, also gets `make_safe`: at most 16 decode threads for
//! the old lolz helpers, which hang with more (see `OLD_LOLZ`).

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
pub(crate) fn srep_mb(limit: u64) -> u64 {
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

/// The lolz-family sections that take MaxMemoryUsage.
const LOLZ_SECTIONS: [&str; 7] = ["magic", "magicl", "magic2", "magic2l", "lolly", "lollypop", "lollypop2"];

/// CLS.ini with the decoders' memory settings lowered to fit `limit` (never raised). A lolz
/// section without MaxMemoryUsage gets one (A Way Out's [magic] has none, and the helper would
/// plan with 75% of the RAM).
pub fn cls_ini(text: &str, limit: u64, physical: u64) -> String {
    let lolz = |section: &str| if section.starts_with("lollypop") { lolz_mb(limit) * 5 / 4 } else { lolz_mb(limit) };
    let mut missing: Vec<String> = Vec::new();
    let mut section = String::new();
    for line in text.lines() {
        let t = line.trim();
        if let Some(name) = t.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            section = name.trim().to_ascii_lowercase();
            if LOLZ_SECTIONS.contains(&section.as_str()) && !missing.contains(&section) {
                missing.push(section.clone());
            }
        } else if t.split_once('=').is_some_and(|(k, _)| k.trim().eq_ignore_ascii_case("MaxMemoryUsage")) {
            missing.retain(|s| *s != section);
        }
    }
    let mut section = String::new();
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        let ending = &line[body.len()..];
        let t = body.trim();
        if let Some(name) = t.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            section = name.trim().to_ascii_lowercase();
            if let Some(i) = missing.iter().position(|s| *s == section) {
                missing.remove(i);
                out.push_str(line);
                let sep = if ending.is_empty() { "\r\n" } else { "" };
                out.push_str(&format!("{sep}MaxMemoryUsage={}m{}", lolz(&section), if ending.is_empty() { "" } else { ending }));
                continue;
            }
        } else if let Some((key, value)) = t.split_once('=') {
            let key = key.trim();
            let ours = if key.eq_ignore_ascii_case("MaxMemoryUsage") {
                // (As in FitGirl's preset: lollypop gets a quarter more than the others.)
                Some(lolz(&section))
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
        // A line without -dm uses xtool's default of 75% of RAM for its deduplication store
        // (rtool's xt3d9 lines, the 0.7.9 xt3u ones): give it the limit's share, right after
        // "decode" (always as a percentage; an absolute size would follow xtool's working set).
        let mut words = words;
        if !words.iter().any(|w| w.starts_with("-dm"))
            && let Some(i) = words.iter().position(|w| w.eq_ignore_ascii_case("decode"))
        {
            words.insert(i + 1, format!("-dm{dm}p"));
        }
        out.push_str(&words.join(" "));
        out.push_str(ending);
    }
    out
}

/// arc.ini with fogg.exe's `-m` (MiB) set by `size(current MiB)`. fogg (balrogg's Ogg Vorbis
/// recompressor with FitGirl's Wwise/FSB5 layers) decodes as many blocks at once as fit in
/// -m by its own estimate (4 x block + 72 MiB per pool, reverse-engineered): FitGirl's `-m2g`
/// holds it to about ten 30 MB blocks, and one 0.5 GB block at a time. -m and -t only gate
/// admission, and blocks are written in stream order, so the output is the same. Only lines
/// that set -m are changed (fogg's own default is unknown).
fn fogg_ini(text: &str, size: impl Fn(u64) -> u64) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        let ending = &line[body.len()..];
        if !body.trim_start().to_ascii_lowercase().starts_with("unpackcmd") {
            out.push_str(line);
            continue;
        }
        let mut words: Vec<String> = body.split(' ').map(str::to_string).collect();
        let is_fogg = |w: &str| w.trim_matches('"').rsplit(['\\', '/']).next().is_some_and(|n| n.eq_ignore_ascii_case("fogg.exe"));
        let Some(prog) = words.iter().position(|w| is_fogg(w)) else {
            out.push_str(line);
            continue;
        };
        // "-m2g": k, m or g; a bare number is bytes.
        let mib = |w: &str| -> Option<u64> {
            let rest = w.strip_prefix("-m")?;
            let (num, unit) = rest.split_at(rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len()));
            let n: u64 = num.parse().ok()?;
            match unit.to_ascii_lowercase().as_str() {
                "" => Some(n >> 20),
                "k" => Some(n >> 10),
                "m" => Some(n),
                "g" => Some(n << 10),
                _ => None,
            }
        };
        if let Some((i, current)) = words.iter().enumerate().skip(prog + 1).find_map(|(i, w)| mib(w).map(|m| (i, m))) {
            let new = size(current);
            if new != current {
                words[i] = format!("-m{new}m");
            }
        }
        out.push_str(&words.join(" "));
        out.push_str(ending);
    }
    out
}

/// fogg's -m for this install: a quarter of the free RAM, at most 8 GiB, never lower than the
/// repack's (fogg's estimate counts each of its two stages separately, so up to half the free
/// RAM). Returns a line for the log when it changed arc.ini.
pub fn size_fogg(tmp: &Path, free: u64) -> Option<String> {
    let want = (free / 4).min(8 << 30) >> 20;
    let changed = edit_files(tmp, |name, text| (name == "arc.ini").then(|| fogg_ini(text, |cur| cur.max(want))));
    changed.then(|| format!("fogg: -m raised to {want} MB, so more audio blocks decode at once"))
}

/// The old lolz helpers that hang with 17 or more decode threads: cls-magic_x64.exe (= magicl),
/// cls-lolly_x64.exe, cls-lollypop_x64.exe and cls-lollypop2_x64.exe. Each reads the CLS.ini
/// section named like itself (the helper's name without "cls-" and "_x64").
///
/// Reverse-engineered: for a multi-threaded ("mtt") stream they decode with
/// T = min(MaxThreadsUsage, MaxMemoryUsage / memory per thread) threads (magic: also at most the
/// CPU count), but they keep only 16 thread slots: a 17th runs into a lock nothing ever
/// releases, so the decoder waits forever with no CPU. MaxThreadsUsage defaults to 75% of the
/// CPUs (24 on a 32-thread PC), and FitGirl's scripts set no MaxMemoryUsage unless "limit RAM"
/// is ticked; FitGirl's own installer hangs the same way (A Way Out on 32 threads). magic2 and
/// magic2l cap themselves at 16. The thread count doesn't change the output.
pub const OLD_LOLZ: [&str; 5] = ["magic", "magicl", "lolly", "lollypop", "lollypop2"];

/// The most decode threads the old lolz helpers are given.
pub const OLD_LOLZ_THREADS: u32 = 16;

/// A MaxThreadsUsage value ("100%", "75p", "12", "100%-2") as a thread count on a PC with
/// `cpus` logical CPUs, as the helpers read it: N% or Np is min(N, 100)% of the CPUs, a bare N
/// is N threads, terms joined by '-' are subtracted. None for anything else (sizes, text).
fn threads(expr: &str, cpus: u32) -> Option<u32> {
    let mut total: i64 = 0;
    for (i, term) in expr.trim().split('-').enumerate() {
        let t = term.trim().to_ascii_lowercase();
        let (num, unit) = t.split_at(t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len()));
        let n: i64 = num.parse().ok()?;
        let v = match unit.trim() {
            "%" | "p" => n.min(100) * cpus as i64 / 100,
            "" => n,
            _ => return None,
        };
        total += if i == 0 { v } else { -v };
    }
    Some(total.max(0) as u32)
}

/// CLS.ini with what every install needs on this PC: each old lolz section (`OLD_LOLZ`) present,
/// or missing but with its helper in the folder (`helpers`, lowercase section names), gets at
/// most `OLD_LOLZ_THREADS` decode threads. A lower setting stays; memory settings are left alone.
pub fn cls_ini_safe(text: &str, cpus: u32, helpers: &[&str]) -> String {
    let cap = OLD_LOLZ_THREADS;
    let old = |s: &str| OLD_LOLZ.contains(&s);
    // Which old sections exist, and whether each has an acceptable MaxThreadsUsage.
    let mut sections: Vec<(String, bool)> = Vec::new();
    let mut section = String::new();
    for line in text.lines() {
        let t = line.trim();
        if let Some(name) = t.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            section = name.trim().to_ascii_lowercase();
            if old(&section) && !sections.iter().any(|(s, _)| *s == section) {
                sections.push((section.clone(), false));
            }
        } else if old(&section)
            && let Some((k, _)) = t.split_once('=')
            && k.trim().eq_ignore_ascii_case("MaxThreadsUsage")
            && let Some(entry) = sections.iter_mut().find(|(s, _)| *s == section)
        {
            entry.1 = true;
        }
    }
    let mut out = String::with_capacity(text.len() + 64);
    section.clear();
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        let ending = &line[body.len()..];
        let t = body.trim();
        if let Some(name) = t.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            section = name.trim().to_ascii_lowercase();
            out.push_str(line);
            if old(&section) && sections.iter().any(|(s, has)| *s == section && !has) {
                if ending.is_empty() {
                    out.push_str(&format!("\r\nMaxThreadsUsage={cap}"));
                } else {
                    out.push_str(&format!("MaxThreadsUsage={cap}{ending}"));
                }
                // (Only once, should the section appear twice.)
                if let Some(e) = sections.iter_mut().find(|(s, _)| *s == section) {
                    e.1 = true;
                }
            }
            continue;
        }
        if old(&section)
            && let Some((key, value)) = t.split_once('=')
            && key.trim().eq_ignore_ascii_case("MaxThreadsUsage")
            && threads(value, cpus).is_none_or(|n| n > cap)
        {
            out.push_str(&format!("{}={cap}{ending}", key.trim()));
            continue;
        }
        out.push_str(line);
    }
    for name in OLD_LOLZ {
        if helpers.contains(&name) && !sections.iter().any(|(s, _)| s == name) {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push_str("\r\n");
            }
            out.push_str(&format!("[{name}]\r\nMaxThreadsUsage={cap}\r\n"));
        }
    }
    out
}

/// The old lolz helpers in `dir` (`cls-<name>_x64.exe`), as CLS.ini section names.
fn old_lolz_helpers(dir: &Path) -> Vec<&'static str> {
    OLD_LOLZ.into_iter().filter(|name| dir.join(format!("cls-{name}_x64.exe")).is_file()).collect()
}

/// Logical CPUs as the helpers count them (GetSystemInfo's dwNumberOfProcessors).
fn logical_cpus() -> u32 {
    #[cfg(windows)]
    unsafe {
        let mut info: windows_sys::Win32::System::SystemInformation::SYSTEM_INFO = std::mem::zeroed();
        windows_sys::Win32::System::SystemInformation::GetSystemInfo(&mut info);
        if info.dwNumberOfProcessors > 0 {
            return info.dwNumberOfProcessors;
        }
    }
    std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(4)
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
pub fn make_safe(dir: &Path, _physical: u64) -> Option<String> {
    let cpus = logical_cpus();
    let helpers = old_lolz_helpers(dir);
    let changed = edit_files(dir, |name, text| (name == "cls.ini").then(|| cls_ini_safe(text, cpus, &helpers)));
    changed.then(|| {
        format!(
            "old lolz decoders ({}): at most {OLD_LOLZ_THREADS} threads (they hang with 17 or more)",
            OLD_LOLZ.join(", ")
        )
    })
}

/// Applies the limit to the installer's CLS.ini and arc.ini in `tmp`. Returns what it set, for
/// the log.
pub fn apply(tmp: &Path, limit: u64, physical: u64) -> String {
    edit_files(tmp, |name, text| match name {
        "cls.ini" => Some(cls_ini(text, limit, physical)),
        // fogg gets a quarter of the limit, at least 512 MiB, never more than the repack's.
        "arc.ini" => Some(fogg_ini(&arc_ini(text, limit, physical), |cur| ((limit >> 20) / 4).max(512).min(cur))),
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
    fn fogg_memory() {
        let ini = "[External compressor:fogg]\r\nheader = 0\r\nunpackcmd = fogg.exe -m2g <stdin> <stdout>\r\n[External compressor:fogg2]\r\nunpackcmd = fogg.exe -t100p -m2g <stdin> <stdout>\r\n[External compressor:x]\r\nunpackcmd = xfogg.exe -m2g <stdin> <stdout>\r\nunpackcmd = fogg.exe <stdin> <stdout>";
        // Raised (8 GiB), with -t and other programs and lines left alone.
        let up = fogg_ini(ini, |cur| cur.max(8192));
        assert_eq!(up, ini.replacen("fogg.exe -m2g", "fogg.exe -m8192m", 1).replacen("-t100p -m2g", "-t100p -m8192m", 1));
        // Never lowered by the raise, and a line without -m keeps fogg's default.
        assert_eq!(fogg_ini(ini, |cur| cur.max(1024)), ini);
        // Under a 2 GB limit: 512 MiB.
        assert!(fogg_ini(ini, |cur| ((2u64 << 10) / 4).max(512).min(cur)).contains("unpackcmd = fogg.exe -m512m <stdin>"));
        // Units: a bare number is bytes.
        assert_eq!(fogg_ini("unpackcmd = fogg.exe -m1073741824 <stdin> <stdout>\n", |c| c * 2), "unpackcmd = fogg.exe -m2048m <stdin> <stdout>\n");
    }

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
        // A line without -dm gets one (xtool's default would be 75% of RAM).
        assert!(got.contains("xtool3\\xtool.exe decode -dm5p -t100p - -"));
        // 4 GB: two threads per GB, and other programs' options are left alone.
        let got = arc_ini(ini, 4 * GB, 32 * GB);
        assert!(got.contains("xtool.exe decode -t8 -dm2p - -") && got.contains("xtool3\\xtool.exe decode -dm2p -t8 - -"));
        let rtool = "unpackcmd = rtool.exe decode -t100p - - <stdin> <stdout>\n";
        assert_eq!(arc_ini(rtool, 4 * GB, 32 * GB), "unpackcmd = rtool.exe decode -dm2p -t8 - - <stdin> <stdout>\n");
        assert!(got.contains("rz.exe d -t100p - -"));
    }

    #[test]
    fn old_lolz_helpers_get_at_most_16_threads() {
        // A Way Out's CLS.ini on 32 threads: [magic] and [magicl] get MaxThreadsUsage=16 (100% would
        // be 32); memory is left alone; magic2 caps itself and stays as it is.
        let ini = "[lollypop2]\r\nMaxMemoryUsage=75%-10m\r\n\r\n[magic]\r\nBufsize=512k\r\nMaxThreadsUsage=100%\r\n\r\n[magicl]\r\nldmfMaxMemoryUsage=1024\r\n[magic2]\r\nMaxThreadsUsage=100%\r\n";
        let got = cls_ini_safe(ini, 32, &[]);
        assert_eq!(
            got,
            "[lollypop2]\r\nMaxThreadsUsage=16\r\nMaxMemoryUsage=75%-10m\r\n\r\n[magic]\r\nBufsize=512k\r\nMaxThreadsUsage=16\r\n\r\n[magicl]\r\nMaxThreadsUsage=16\r\nldmfMaxMemoryUsage=1024\r\n[magic2]\r\nMaxThreadsUsage=100%\r\n"
        );
        assert_eq!(cls_ini_safe(&got, 32, &[]), got, "running it again changes nothing");
        // 16 CPUs: 100% is 16, kept. A bare 12 is kept; 75% of 32 (24) is lowered.
        assert_eq!(cls_ini_safe("[magic]\r\nMaxThreadsUsage=100%\r\n", 16, &[]), "[magic]\r\nMaxThreadsUsage=100%\r\n");
        assert_eq!(cls_ini_safe("[lolly]\r\nMaxThreadsUsage=12\r\n", 32, &[]), "[lolly]\r\nMaxThreadsUsage=12\r\n");
        assert_eq!(cls_ini_safe("[lollypop]\r\nMaxThreadsUsage=75%\r\n", 32, &[]), "[lollypop]\r\nMaxThreadsUsage=16\r\n");
        assert_eq!(cls_ini_safe("[lollypop]\r\nMaxThreadsUsage=lots\r\n", 32, &[]), "[lollypop]\r\nMaxThreadsUsage=16\r\n");
        // No section, but the helper is there: it would use its default (75%), so a section is added.
        assert_eq!(cls_ini_safe("[Srep]\r\nMemory=60%\r\n", 32, &["lollypop"]), "[Srep]\r\nMemory=60%\r\n[lollypop]\r\nMaxThreadsUsage=16\r\n");
        assert_eq!(cls_ini_safe("[Srep]", 32, &["magic"]), "[Srep]\r\n[magic]\r\nMaxThreadsUsage=16\r\n");
        // A section header on the last line, without a line ending.
        assert_eq!(cls_ini_safe("[magic]", 32, &[]), "[magic]\r\nMaxThreadsUsage=16");
    }

    #[test]
    fn thread_settings() {
        assert_eq!(threads("100%", 32), Some(32));
        assert_eq!(threads("75p", 32), Some(24));
        assert_eq!(threads("150%", 8), Some(8));
        assert_eq!(threads("12", 32), Some(12));
        assert_eq!(threads("100%-2", 32), Some(30));
        assert_eq!(threads("1024m", 32), None);
    }

    #[test]
    fn a_limit_reaches_lolz_sections_without_a_memory_key() {
        let ini = "[magic]\r\nMaxThreadsUsage=16\r\n[lollypop]\r\nMaxThreadsUsage=16\r\n[magic2]\r\nMaxMemoryUsage=75%-10m\r\n";
        // 8 GB: lolz 4096m (lollypop 5120m); an existing key is lowered as before.
        assert_eq!(
            cls_ini(ini, 8 * GB, 32 * GB),
            "[magic]\r\nMaxMemoryUsage=4096m\r\nMaxThreadsUsage=16\r\n[lollypop]\r\nMaxMemoryUsage=5120m\r\nMaxThreadsUsage=16\r\n[magic2]\r\nMaxMemoryUsage=4096m\r\n"
        );
    }

    #[test]
    fn sizes() {
        assert_eq!(eval_mb("75%-10m", 16 * GB), Some(12278));
        assert_eq!(eval_mb("2g", GB), Some(2048));
        assert_eq!(eval_mb("lots", GB), None);
    }
}
