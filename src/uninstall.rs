//! The uninstaller every install gets: `uninstall.exe` with its list `uninstall.dat` in the game
//! folder, and an entry in Windows' list of installed apps (Settings > Apps).
//!
//! The list names what the install made: the files and folders that weren't in the game folder
//! before it (a snapshot is taken first), the shortcuts and the apps entry. Uninstalling removes
//! exactly those. Files that were there before, and files the game makes itself later (saves,
//! settings), stay; folders go once they are empty.

use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};

/// The uninstaller's file name in the game folder.
pub const EXE: &str = "uninstall.exe";
/// Its list of what to remove.
pub const LIST: &str = "uninstall.dat";
/// The uninstaller program arc-unpack carries (see `helpers`).
pub const PROGRAM: &str = "arc-unpack-uninstall.exe";
const HEADER: &str = "arc-unpack uninstall list 1";

/// What a folder holds: paths relative to it, lower case.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub existed: bool,
    files: HashSet<String>,
    dirs: HashSet<String>,
}

/// The files and folders in `dir`: relative paths (with `\`), whether a folder, size. Links are
/// listed, not followed.
fn walk(dir: &Path) -> Vec<(String, bool, u64)> {
    let mut out = Vec::new();
    let mut todo = vec![PathBuf::new()];
    while let Some(rel) = todo.pop() {
        let Ok(entries) = std::fs::read_dir(dir.join(&rel)) else { continue };
        for e in entries.flatten() {
            let Ok(kind) = e.file_type() else { continue };
            let r = rel.join(e.file_name());
            let s = r.to_string_lossy().replace('/', "\\");
            if kind.is_dir() {
                out.push((s, true, 0));
                todo.push(r);
            } else {
                out.push((s, false, e.metadata().map(|m| m.len()).unwrap_or(0)));
            }
        }
    }
    out
}

pub fn snapshot(dir: &Path) -> Snapshot {
    let mut s = Snapshot { existed: dir.is_dir(), ..Default::default() };
    for (rel, is_dir, _) in walk(dir) {
        if is_dir {
            s.dirs.insert(rel.to_lowercase());
        } else {
            s.files.insert(rel.to_lowercase());
        }
    }
    s
}

/// A short key for a game folder (names its apps entry and its saved snapshot).
fn key(dir: &Path) -> String {
    let abs = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
    sha1_smol::Sha1::from(abs.to_string_lossy().to_lowercase()).digest().to_string()[..12].to_string()
}

fn saved_snapshot(dir: &Path, cache_dir: &Path) -> PathBuf {
    cache_dir.join("uninstall").join(format!("{}.before", key(dir)))
}

/// The game folder as it was before installing into it. The snapshot is kept until an
/// uninstaller was made for the folder, so retrying after a stopped or failed attempt counts
/// what that attempt left as installed too.
pub fn before_install(dir: &Path, cache_dir: &Path) -> Snapshot {
    let path = saved_snapshot(dir, cache_dir);
    let now = snapshot(dir);
    if !now.files.is_empty()
        && let Some(saved) = load_snapshot(&path)
    {
        return saved;
    }
    let _ = save_snapshot(&path, &now);
    now
}

/// Drops the snapshot of a game folder once its uninstaller was made.
pub fn forget_before(dir: &Path, cache_dir: &Path) {
    let _ = std::fs::remove_file(saved_snapshot(dir, cache_dir));
}

fn save_snapshot(path: &Path, s: &Snapshot) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("no folder")?)?;
    let mut text = String::from(if s.existed { "existed\n" } else { "new\n" });
    for f in &s.files {
        text.push_str(&format!("f\t{f}\n"));
    }
    for d in &s.dirs {
        text.push_str(&format!("d\t{d}\n"));
    }
    std::fs::write(path, text)?;
    Ok(())
}

fn load_snapshot(path: &Path) -> Option<Snapshot> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let existed = match lines.next()? {
        "existed" => true,
        "new" => false,
        _ => return None,
    };
    let mut s = Snapshot { existed, ..Default::default() };
    for line in lines {
        match line.split_once('\t') {
            Some(("f", p)) => s.files.insert(p.to_string()),
            Some(("d", p)) => s.dirs.insert(p.to_string()),
            _ => false,
        };
    }
    Some(s)
}

/// The entry in Windows' list of installed apps: for all users (HKLM) or this user (HKCU).
#[derive(Debug, Clone, PartialEq)]
pub struct AppsEntry {
    pub machine: bool,
    pub key: String,
}

/// What an uninstall removes (the contents of `uninstall.dat`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Record {
    pub name: String,
    /// The install made the game folder itself: it goes too once it is empty.
    pub made_folder: bool,
    pub apps_entry: Option<AppsEntry>,
    pub shortcuts: Vec<PathBuf>,
    /// Start menu folders made for the shortcuts (removed once empty).
    pub shortcut_dirs: Vec<PathBuf>,
    /// Folders and files the install made, relative to the game folder.
    pub dirs: Vec<String>,
    pub files: Vec<String>,
    pub bytes: u64,
}

impl Record {
    fn to_text(&self) -> String {
        let mut t = format!("{HEADER}\nname\t{}\n", self.name);
        if self.made_folder {
            t.push_str("made-folder\n");
        }
        if let Some(e) = &self.apps_entry {
            t.push_str(&format!("apps\t{}\t{}\n", if e.machine { "HKLM" } else { "HKCU" }, e.key));
        }
        t.push_str(&format!("bytes\t{}\n", self.bytes));
        for s in &self.shortcuts {
            t.push_str(&format!("shortcut\t{}\n", s.display()));
        }
        for d in &self.shortcut_dirs {
            t.push_str(&format!("shortcut-dir\t{}\n", d.display()));
        }
        for d in &self.dirs {
            t.push_str(&format!("dir\t{d}\n"));
        }
        for f in &self.files {
            t.push_str(&format!("file\t{f}\n"));
        }
        t
    }

    fn parse(text: &str) -> Result<Record> {
        let mut lines = text.lines();
        ensure!(lines.next().map(str::trim) == Some(HEADER), "not an arc-unpack uninstall list");
        let mut r = Record::default();
        for line in lines {
            let (k, v) = line.split_once('\t').unwrap_or((line, ""));
            match k {
                "name" => r.name = v.to_string(),
                "made-folder" => r.made_folder = true,
                "apps" => {
                    if let Some((root, key)) = v.split_once('\t') {
                        r.apps_entry = Some(AppsEntry { machine: root == "HKLM", key: key.to_string() });
                    }
                }
                "bytes" => r.bytes = v.parse().unwrap_or(0),
                "shortcut" => r.shortcuts.push(PathBuf::from(v)),
                "shortcut-dir" => r.shortcut_dirs.push(PathBuf::from(v)),
                "dir" => r.dirs.push(v.to_string()),
                "file" => r.files.push(v.to_string()),
                _ => {}
            }
        }
        Ok(r)
    }

    /// Whether removing it needs administrator rights (an entry or shortcuts for all users).
    pub fn needs_admin(&self) -> bool {
        let common = std::env::var_os("ProgramData").map(|p| p.to_string_lossy().to_lowercase());
        self.apps_entry.as_ref().is_some_and(|e| e.machine)
            || self.shortcuts.iter().chain(&self.shortcut_dirs).any(|s| common.as_ref().is_some_and(|c| s.to_string_lossy().to_lowercase().starts_with(c.as_str())))
    }
}

/// The uninstall list in a game folder.
pub fn read(dir: &Path) -> Result<Record> {
    let path = dir.join(LIST);
    let text = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    Record::parse(&text)
}

fn write(dir: &Path, r: &Record) -> Result<()> {
    let tmp = dir.join(format!("{LIST}.tmp"));
    std::fs::write(&tmp, r.to_text()).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, dir.join(LIST)).with_context(|| format!("write {}", dir.join(LIST).display()))
}

/// What the apps entry shows.
#[derive(Debug, Clone, Default)]
pub struct Details {
    pub name: String,
    pub publisher: String,
    pub version: String,
    pub icon: Option<PathBuf>,
}

/// After an install: puts the uninstaller (`program`) in the game folder with the list of what
/// the install made (added to the list already there, if any), and registers it in Windows'
/// list of installed apps. Returns notes on what couldn't be done.
pub fn create(dir: &Path, before: &Snapshot, details: &Details, shortcuts: &[PathBuf], shortcut_dirs: &[PathBuf], program: &Path, register: bool) -> Result<Vec<String>> {
    let mut notes = Vec::new();
    std::fs::copy(program, dir.join(EXE)).with_context(|| format!("copy the uninstaller into {}", dir.display()))?;
    let mut rec = if dir.join(LIST).is_file() { read(dir).unwrap_or_default() } else { Record::default() };
    let mut files: BTreeMap<String, String> = rec.files.iter().map(|f| (f.to_lowercase(), f.clone())).collect();
    let mut dirs: BTreeMap<String, String> = rec.dirs.iter().map(|d| (d.to_lowercase(), d.clone())).collect();
    for (rel, is_dir, _) in walk(dir) {
        let k = rel.to_lowercase();
        if k == LIST.to_lowercase() || k == format!("{LIST}.tmp") {
            continue;
        }
        if is_dir {
            if !before.dirs.contains(&k) {
                dirs.entry(k).or_insert(rel);
            }
        } else if !before.files.contains(&k) {
            files.entry(k).or_insert(rel);
        }
    }
    rec.bytes = files.values().filter_map(|f| std::fs::metadata(dir.join(f)).ok()).map(|m| m.len()).sum();
    rec.files = files.into_values().collect();
    rec.dirs = dirs.into_values().collect();
    rec.made_folder |= !before.existed;
    rec.name = details.name.clone();
    for s in shortcuts {
        if !rec.shortcuts.contains(s) {
            rec.shortcuts.push(s.clone());
        }
    }
    for d in shortcut_dirs {
        if !rec.shortcut_dirs.contains(d) {
            rec.shortcut_dirs.push(d.clone());
        }
    }
    if register {
        match apps::register(dir, &rec, details) {
            Ok(e) => rec.apps_entry = Some(e),
            Err(e) => notes.push(format!("couldn't add the game to Windows' list of installed apps: {e:#}")),
        }
    }
    write(dir, &rec)?;
    Ok(notes)
}

/// Adds shortcuts made after the install to the game folder's uninstall list.
pub fn add_shortcuts(dir: &Path, shortcuts: &[PathBuf]) -> Result<()> {
    let mut rec = read(dir)?;
    for s in shortcuts {
        if !rec.shortcuts.contains(s) {
            rec.shortcuts.push(s.clone());
        }
    }
    write(dir, &rec)
}

/// The outcome of an uninstall.
#[derive(Debug, Default)]
pub struct Removal {
    pub removed: usize,
    /// Listed files that couldn't be removed, with why.
    pub failed: Vec<String>,
    /// Files left in the game folder that the install didn't make (saves, settings, ...).
    pub kept: Vec<String>,
}

/// A listed path as a path inside `dir`; None if it would point anywhere else.
fn inside(dir: &Path, rel: &str) -> Option<PathBuf> {
    let p = Path::new(rel);
    let plain = !rel.is_empty() && !rel.contains(':') && p.components().all(|c| matches!(c, Component::Normal(_)));
    plain.then(|| dir.join(p))
}

/// Removes a file; also read-only ones and links to folders. Ok if it was already gone.
fn remove_file(p: &Path) -> std::io::Result<()> {
    let e = match std::fs::remove_file(p) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => e,
    };
    let Ok(m) = std::fs::symlink_metadata(p) else { return Err(e) };
    if m.is_dir() || m.file_type().is_symlink() {
        return std::fs::remove_dir(p);
    }
    let mut perm = m.permissions();
    if perm.readonly() {
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(false);
        std::fs::set_permissions(p, perm)?;
        return std::fs::remove_file(p);
    }
    Err(e)
}

/// Removes what the list names from `dir` (its files, then its folders once empty), the
/// shortcuts and the list itself. `uninstall.exe` stays (it may be running): the caller removes
/// it, and the game folder if the install made it and it is empty. The apps entry is removed
/// with `apps::unregister`. `progress` gets (done, total) files.
pub fn remove(dir: &Path, rec: &Record, progress: &mut dyn FnMut(usize, usize)) -> Removal {
    let mut out = Removal::default();
    let total = rec.files.len();
    for (i, rel) in rec.files.iter().enumerate() {
        if !rel.eq_ignore_ascii_case(EXE)
            && let Some(p) = inside(dir, rel)
        {
            match remove_file(&p) {
                Ok(()) => out.removed += 1,
                Err(e) => out.failed.push(format!("{rel}: {e}")),
            }
        }
        progress(i + 1, total);
    }
    for s in rec.shortcuts.iter().filter(|s| s.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk"))) {
        if let Err(e) = remove_file(s) {
            out.failed.push(format!("{}: {e}", s.display()));
        }
    }
    let mut made: Vec<&PathBuf> = rec.shortcut_dirs.iter().collect();
    made.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for d in made {
        let _ = std::fs::remove_dir(d);
    }
    let mut dirs: Vec<&String> = rec.dirs.iter().collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.matches('\\').count()));
    for d in dirs {
        if let Some(p) = inside(dir, d) {
            let _ = std::fs::remove_dir(&p);
        }
    }
    let _ = std::fs::remove_file(dir.join(LIST));
    let listed: HashSet<String> = rec.files.iter().map(|f| f.to_lowercase()).collect();
    out.kept = walk(dir)
        .into_iter()
        .filter(|(r, is_dir, _)| !is_dir && !r.eq_ignore_ascii_case(EXE) && !listed.contains(&r.to_lowercase()))
        .map(|(r, ..)| r)
        .collect();
    out
}

/// Windows' list of installed apps (`...\CurrentVersion\Uninstall`).
pub mod apps {
    use super::{AppsEntry, Details, EXE, Record};
    use anyhow::Result;
    use std::path::Path;

    const UNINSTALL: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall";

    enum Value {
        Text(String),
        Number(u32),
    }

    /// Registers the game (for all users when running as administrator, else for this user).
    pub fn register(dir: &Path, rec: &Record, d: &Details) -> Result<AppsEntry> {
        let machine = crate::guard::is_elevated();
        let entry = AppsEntry { machine, key: format!(r"{UNINSTALL}\arc-unpack-{}", super::key(dir)) };
        if let Some(old) = &rec.apps_entry
            && *old != entry
        {
            let _ = unregister(old);
        }
        let exe = dir.join(EXE);
        let mut values = vec![
            ("DisplayName", Value::Text(d.name.clone())),
            ("InstallLocation", Value::Text(dir.display().to_string())),
            ("UninstallString", Value::Text(format!("\"{}\"", exe.display()))),
            ("QuietUninstallString", Value::Text(format!("\"{}\" /S", exe.display()))),
            ("EstimatedSize", Value::Number((rec.bytes / 1024).min(u32::MAX as u64) as u32)),
            ("NoModify", Value::Number(1)),
            ("NoRepair", Value::Number(1)),
            ("InstallDate", Value::Text(today())),
            ("Comments", Value::Text("Installed with arc-unpack".into())),
        ];
        if let Some(icon) = &d.icon {
            values.push(("DisplayIcon", Value::Text(icon.display().to_string())));
        }
        let plain = |s: &str| !s.trim().is_empty() && !s.contains('{');
        if plain(&d.publisher) {
            values.push(("Publisher", Value::Text(d.publisher.trim().to_string())));
        }
        if plain(&d.version) {
            values.push(("DisplayVersion", Value::Text(d.version.trim().to_string())));
        }
        write_key(machine, &entry.key, &values)?;
        Ok(entry)
    }

    #[cfg(windows)]
    fn today() -> String {
        let mut t = unsafe { std::mem::zeroed::<windows_sys::Win32::Foundation::SYSTEMTIME>() };
        unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t) };
        format!("{:04}{:02}{:02}", t.wYear, t.wMonth, t.wDay)
    }

    #[cfg(windows)]
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    #[cfg(windows)]
    fn write_key(machine: bool, key: &str, values: &[(&str, Value)]) -> Result<()> {
        use windows_sys::Win32::System::Registry::*;
        let root = if machine { HKEY_LOCAL_MACHINE } else { HKEY_CURRENT_USER };
        let mut h: HKEY = std::ptr::null_mut();
        let k = wide(key);
        // SAFETY: plain registry calls with NUL-terminated strings that outlive them.
        let r = unsafe { RegCreateKeyExW(root, k.as_ptr(), 0, std::ptr::null(), REG_OPTION_NON_VOLATILE, KEY_WRITE | KEY_WOW64_64KEY, std::ptr::null(), &mut h, std::ptr::null_mut()) };
        anyhow::ensure!(r == 0, "can't create the registry key (error {r})");
        let mut result = Ok(());
        for (name, value) in values {
            let n = wide(name);
            let r = match value {
                Value::Text(s) => {
                    let w = wide(s);
                    unsafe { RegSetValueExW(h, n.as_ptr(), 0, REG_SZ, w.as_ptr().cast(), (w.len() * 2) as u32) }
                }
                Value::Number(v) => unsafe { RegSetValueExW(h, n.as_ptr(), 0, REG_DWORD, (v as *const u32).cast(), 4) },
            };
            if r != 0 {
                result = Err(anyhow::anyhow!("can't write the registry value {name} (error {r})"));
                break;
            }
        }
        unsafe { RegCloseKey(h) };
        result
    }

    /// Removes the game from the list. Ok if it isn't there.
    #[cfg(windows)]
    pub fn unregister(e: &AppsEntry) -> Result<()> {
        use windows_sys::Win32::System::Registry::*;
        let root = if e.machine { HKEY_LOCAL_MACHINE } else { HKEY_CURRENT_USER };
        let k = wide(&e.key);
        let r = unsafe { RegDeleteKeyExW(root, k.as_ptr(), KEY_WOW64_64KEY, 0) };
        anyhow::ensure!(r == 0 || r == 2, "can't remove the registry key (error {r})");
        Ok(())
    }

    #[cfg(not(windows))]
    fn today() -> String {
        String::new()
    }
    #[cfg(not(windows))]
    fn write_key(_: bool, _: &str, _: &[(&str, Value)]) -> Result<()> {
        anyhow::bail!("only on Windows")
    }
    #[cfg(not(windows))]
    pub fn unregister(_: &AppsEntry) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(dir: &Path, rel: &str, data: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }

    #[test]
    fn the_list_names_what_the_install_made() {
        let tmp = tempfile::tempdir().unwrap();
        let game = tmp.path().join("Game");
        std::fs::create_dir_all(&game).unwrap();
        put(&game, "mine.txt", "the user's file, there before");
        let before = snapshot(&game);
        put(&game, "game.exe", "exe");
        put(&game, "data\\a\\b.pak", "pak");
        let program = tmp.path().join("prog.exe");
        std::fs::write(&program, "uninstaller").unwrap();
        let details = Details { name: "Some Game".into(), ..Default::default() };
        let notes = create(&game, &before, &details, &[], &[], &program, false).unwrap();
        assert!(notes.is_empty());
        let rec = read(&game).unwrap();
        assert_eq!(rec.name, "Some Game");
        assert!(!rec.made_folder, "the folder was there before");
        let mut files = rec.files.clone();
        files.sort();
        assert_eq!(files, ["data\\a\\b.pak", "game.exe", "uninstall.exe"]);
        assert_eq!(rec.dirs.len(), 2, "data and data\\a: {:?}", rec.dirs);
        assert_eq!(rec.bytes, 3 + 3 + "uninstaller".len() as u64);

        // The game makes a save later; uninstalling keeps it and the user's file.
        put(&game, "data\\save.dat", "save");
        let mut seen = 0;
        let r = remove(&game, &rec, &mut |done, total| {
            assert!(done <= total);
            seen = done;
        });
        assert_eq!(seen, 3);
        assert!(r.failed.is_empty(), "{:?}", r.failed);
        let mut kept = r.kept.clone();
        kept.sort();
        assert_eq!(kept, ["data\\save.dat", "mine.txt"]);
        assert!(!game.join("game.exe").exists() && !game.join(LIST).exists());
        assert!(!game.join("data\\a").exists(), "empty folders go");
        assert!(game.join("data\\save.dat").exists());
        assert!(game.join(EXE).exists(), "uninstall.exe is left to the caller");
    }

    #[test]
    fn a_new_folder_and_a_second_install_into_it() {
        let tmp = tempfile::tempdir().unwrap();
        let game = tmp.path().join("New");
        let program = tmp.path().join("prog.exe");
        std::fs::write(&program, "u").unwrap();
        let before = snapshot(&game);
        assert!(!before.existed);
        put(&game, "base.bin", "1");
        create(&game, &before, &Details { name: "G".into(), ..Default::default() }, &[], &[], &program, false).unwrap();
        // Adding parts later: the list grows.
        let before = snapshot(&game);
        put(&game, "dlc\\part.bin", "2");
        create(&game, &before, &Details { name: "G".into(), ..Default::default() }, &[], &[], &program, false).unwrap();
        let rec = read(&game).unwrap();
        assert!(rec.made_folder);
        let mut files = rec.files.clone();
        files.sort();
        assert_eq!(files, ["base.bin", "dlc\\part.bin", "uninstall.exe"]);
    }

    #[test]
    fn a_retry_counts_what_the_stopped_attempt_left() {
        let tmp = tempfile::tempdir().unwrap();
        let (game, cache) = (tmp.path().join("G"), tmp.path().join("cache"));
        let first = before_install(&game, &cache);
        put(&game, "half.bin", "x"); // the stopped attempt
        let second = before_install(&game, &cache);
        assert_eq!(first, second, "the snapshot from before the first attempt");
        put(&game, "rest.bin", "y");
        let program = tmp.path().join("prog.exe");
        std::fs::write(&program, "u").unwrap();
        create(&game, &second, &Details::default(), &[], &[], &program, false).unwrap();
        assert!(read(&game).unwrap().files.iter().any(|f| f == "half.bin"));
    }

    #[test]
    fn only_paths_inside_the_game_folder() {
        let d = Path::new(r"C:\Games\G");
        assert_eq!(inside(d, r"a\b.txt"), Some(d.join(r"a\b.txt")));
        for bad in ["", r"..\x", r"a\..\..\x", r"C:\Windows\x", r"\x", "c:x"] {
            assert_eq!(inside(d, bad), None, "{bad}");
        }
        let rec = Record {
            name: "N".into(),
            made_folder: true,
            apps_entry: Some(AppsEntry { machine: true, key: r"Software\X\arc-unpack-1".into() }),
            shortcuts: vec![PathBuf::from(r"C:\Users\u\Desktop\G.lnk")],
            shortcut_dirs: vec![PathBuf::from(r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs\Games\G")],
            dirs: vec!["d".into()],
            files: vec![r"d\f.txt".into()],
            bytes: 5,
        };
        assert_eq!(Record::parse(&rec.to_text()).unwrap(), rec);
        assert!(Record::parse("something else").is_err());
    }
}
