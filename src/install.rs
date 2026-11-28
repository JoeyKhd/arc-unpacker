//! One-step install of a FreeArc-based repack folder:
//!   1. find the installer (`setup.exe`) and the FreeArc archives (by magic, not by name);
//!   2. optionally verify the archives against the repack's MD5 list;
//!   3. prepare the codecs once per installer (extract, fix CLS.ini, import arc.ini) in a cache;
//!   4. classify archives: codec tools (contain programs arc.ini needs) go into the codec
//!      folder, installer scripts are set aside for review, everything else is the game;
//!   5. extract the game into the target folder.
//! Nothing from the installer's own install script is run.

use crate::config::Config;
use crate::events::{self, Event, Sink};
use crate::extract::{self, ExpectedMd5, Md5List, Options, Summary, human};
use crate::format::Archive;
use anyhow::{Context, Result, bail, ensure};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Game files: extracted into the target folder.
    Game,
    /// Programs the codecs need (e.g. xtool folders): extracted into the codec folder first.
    CodecTools,
    /// Small archives of batch scripts the installer runs afterwards: set aside for review.
    Scripts,
}

impl Role {
    pub fn describe(self) -> &'static str {
        match self {
            Role::Game => "game files",
            Role::CodecTools => "codec tools",
            Role::Scripts => "installer scripts",
        }
    }
}

pub struct ScannedArchive {
    pub archive: Archive,
    pub role: Role,
    pub md5: Option<String>,
}

/// What's in a repack folder.
pub struct Scan {
    pub setup: Option<PathBuf>,
    pub archives: Vec<ScannedArchive>,
    pub md5_file: Option<PathBuf>,
    pub codec_dir: Option<PathBuf>,
}

impl Scan {
    pub fn game_bytes(&self) -> u64 {
        self.archives.iter().filter(|a| a.role == Role::Game).map(|a| a.archive.total_size()).sum()
    }
    pub fn compressed_bytes(&self) -> u64 {
        self.archives.iter().map(|a| a.archive.len).sum()
    }
}

pub struct InstallOptions {
    pub repack_dir: PathBuf,
    pub target_dir: PathBuf,
    /// Where prepared codecs and set-aside scripts are kept (default %LOCALAPPDATA%\arc-unpack).
    pub cache_dir: PathBuf,
    /// Use this codecs.toml instead of preparing codecs from setup.exe.
    pub config: Option<PathBuf>,
    pub verify_md5: bool,
    pub jobs: usize,
    pub heavy_jobs: usize,
    pub writers: usize,
    pub overwrite: bool,
    pub stall_timeout: u64,
    pub tune: bool,
    pub host_dir: PathBuf,
    pub cancel: Arc<AtomicBool>,
    /// Set while the archive check runs to skip the rest of it; the install continues.
    pub skip_verify: Arc<AtomicBool>,
    /// Scan, prepare codecs and check space, but extract nothing.
    pub dry_run: bool,
    /// Run the installer's own install script (falls back to plain extraction if it can't).
    pub script: bool,
    /// Components to install (None: default selection plus downloaded optional parts).
    pub components: Option<Vec<String>>,
    /// After the game: install the runtimes it needs that this PC is missing (DirectX, Visual
    /// C++, ...), downloaded from the links the installer uses. See `runtimes`.
    pub runtimes: bool,
    /// Keep RAM use to about this many bytes (see `memlimit`). Callers also set `heavy_jobs` to 1.
    pub memory_limit: Option<u64>,
    /// Make the uninstaller: `uninstall.exe` in the game folder and an entry in Windows' list of
    /// installed apps (see `uninstall`).
    pub uninstaller: bool,
    /// After a successful install, create the installer's desktop shortcuts and Start menu
    /// entries.
    pub desktop_shortcut: bool,
    pub start_menu: bool,
}

#[derive(Default)]
pub struct InstallReport {
    pub summary: Summary,
    pub codec_dir: PathBuf,
    /// Batch scripts from installer-script archives, for the user to review.
    pub scripts: Vec<PathBuf>,
    pub scripts_dir: Option<PathBuf>,
    pub md5_checked: usize,
    /// Files the installer itself puts in the game folder (launcher stubs, `_Redist`, MD5 list).
    pub setup_files: usize,
    /// Installed files that matched the repack's own MD5 list.
    pub files_md5: usize,
    /// Steps the installer's own script took (0 when it wasn't used).
    pub script_steps: usize,
    /// Desktop shortcuts the original installer would create (not created yet: see
    /// `setup_script::create_shortcuts`).
    pub shortcuts: Vec<crate::setup_script::Shortcut>,
    /// The installer's Start menu entries for this selection (made if `start_menu` is set).
    pub start_menu: Vec<crate::setup_script::Shortcut>,
    /// The shortcuts made (desktop and Start menu).
    pub shortcuts_made: Vec<PathBuf>,
    /// The uninstaller put in the game folder.
    pub uninstaller: Option<PathBuf>,
    /// The game's name and Start menu folder, as the installer gives them.
    pub app: Option<crate::inno::AppInfo>,
    /// Only parts were added to a game that was already installed.
    pub added_parts: bool,
    /// What happened to each runtime installed after the game (one line each).
    pub runtimes: Vec<String>,
}

pub fn default_cache_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("arc-unpack")
}

fn has_prefix(path: &Path, magic: &[u8]) -> bool {
    let mut buf = vec![0u8; magic.len()];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut buf)).is_ok() && buf == magic
}

/// The repack's installer: `setup.exe`, or else the first Inno Setup program in the folder (older
/// repacks call it `setup-multi6.exe` and the like).
pub fn find_setup(repack_dir: &Path) -> Option<PathBuf> {
    let plain = repack_dir.join("setup.exe");
    if plain.is_file() {
        return Some(plain);
    }
    let mut exes: Vec<PathBuf> = std::fs::read_dir(repack_dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("exe")))
        .collect();
    exes.sort();
    exes.into_iter().find(|p| is_inno_setup(p))
}

fn is_inno_setup(path: &Path) -> bool {
    // The offset-table magic sits in the loader's resources, well inside the first megabytes.
    std::fs::read(path).map(|b| b.windows(12).any(|w| w == b"rDlPtS\xcd\xe6\xd7{\x0b*")).unwrap_or(false)
}

/// Lowercased, '/'-separated relative program paths that the codec config runs from a
/// subfolder, e.g. `xtool3/xtool.exe` (those folders come from codec-tool archives).
fn referenced_tool_paths(cfg: &Config) -> HashSet<String> {
    let mut out = HashSet::new();
    for spec in cfg.methods.values() {
        if let Some(cmd) = &spec.cmd
            && let Some(prog) = crate::config::split_cmdline(cmd).into_iter().next()
        {
            let p = prog.replace('\\', "/").to_ascii_lowercase();
            if p.contains('/') && !Path::new(&prog).is_absolute() {
                out.insert(p.trim_start_matches("./").to_string());
            }
        }
    }
    out
}

fn classify(a: &Archive, tools: &HashSet<String>) -> Role {
    let files = || a.entries.iter().filter(|e| !e.is_dir);
    if files().any(|e| tools.contains(&e.path.replace('\\', "/").to_ascii_lowercase())) {
        return Role::CodecTools;
    }
    let scripts = files().any(|e| {
        let p = e.path.to_ascii_lowercase();
        p.ends_with(".bat") || p.ends_with(".cmd")
    });
    if scripts && a.total_size() < 64 << 20 {
        return Role::Scripts;
    }
    Role::Game
}

fn read_md5_list(path: &Path) -> BTreeMap<String, String> {
    let text = std::fs::read(path).map(|b| md5_list_text(&b)).unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.starts_with(';') || l.len() < 34 {
                return None;
            }
            let (hash, rest) = l.split_at(32);
            let name = rest.trim().trim_start_matches('*').trim();
            let name = name.rsplit(['\\', '/']).next().unwrap_or(name);
            hash.chars().all(|c| c.is_ascii_hexdigit()).then(|| (name.to_ascii_lowercase(), hash.to_ascii_lowercase()))
        })
        .collect()
}

/// Looks at a repack folder without changing anything. `cfg` (if prepared already) refines
/// the codec-tool classification.
pub fn scan(repack_dir: &Path, cfg: Option<&Config>) -> Result<Scan> {
    let mut setup = None;
    let mut archives = Vec::new();
    let mut md5_file = None;
    let mut entries: Vec<PathBuf> = std::fs::read_dir(repack_dir)
        .with_context(|| format!("read {}", repack_dir.display()))?
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    for p in &entries {
        if p.is_dir() {
            if md5_file.is_none()
                && let Ok(rd) = std::fs::read_dir(p)
            {
                md5_file = rd.flatten().map(|e| e.path()).find(|f| f.extension().is_some_and(|x| x.eq_ignore_ascii_case("md5")));
            }
            continue;
        }
        let ext = p.extension().map(|x| x.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        if ext == "md5" && md5_file.is_none() {
            md5_file = Some(p.clone());
        } else if ext == "exe" && setup.is_none() && is_inno_setup(p) {
            setup = Some(p.clone());
        } else if has_prefix(p, b"ArC\x01") {
            archives.push(Archive::open(p)?);
        }
    }
    ensure!(!archives.is_empty(), "no FreeArc archives found in {}", repack_dir.display());
    let md5s = md5_file.as_deref().map(read_md5_list).unwrap_or_default();
    let tools = cfg.map(referenced_tool_paths).unwrap_or_default();
    let archives = archives
        .into_iter()
        .map(|a| {
            let role = classify(&a, &tools);
            let md5 = md5s.get(&a.name().to_ascii_lowercase()).cloned();
            ScannedArchive { archive: a, role, md5 }
        })
        .collect();
    Ok(Scan { setup, archives, md5_file, codec_dir: cfg.map(|c| c.codec_dir()) })
}

/// `scan` after preparing the codecs (cached, so cheap after the first time), so that
/// codec-tool archives are recognized. Falls back to a plain scan without an installer.
pub fn scan_prepared(repack_dir: &Path, cache_dir: &Path) -> Result<Scan> {
    let first = scan(repack_dir, None)?;
    let Some(setup) = first.setup.clone() else { return Ok(first) };
    let cfg = Config::load(&prepare_codecs(&setup, cache_dir, &events::null_sink())?)?;
    scan(repack_dir, Some(&cfg))
}

/// Extracts the installer's codecs into the cache (once per installer) and returns the
/// generated config path.
pub fn prepare_codecs(setup: &Path, cache_dir: &Path, sink: &Sink) -> Result<PathBuf> {
    let bytes = std::fs::read(setup)?;
    let key = sha1_smol::Sha1::from(&bytes).digest().to_string()[..12].to_string();
    let root = cache_dir.join("codecs").join(&key);
    let config = root.join("codecs.toml");
    if root.join(".ready").is_file() && config.is_file() {
        events::info(sink, format!("using prepared codecs in {}", root.display()));
        // (Caches made before `make_safe` existed get it too; it changes nothing twice.)
        let unpacked = root.join("setup");
        let codec_dir = if unpacked.join("tmp").is_dir() { unpacked.join("tmp") } else { unpacked };
        let (_, physical) = crate::guard::memory();
        if let Some(note) = crate::memlimit::make_safe(&codec_dir, physical) {
            events::info(sink, note);
        }
        return Ok(config);
    }
    events::info(sink, format!("extracting codecs from {}", setup.display()));
    let inno = crate::inno::Setup::open(setup)?;
    let unpacked = root.join("setup");
    let _ = std::fs::remove_dir_all(&unpacked);
    let files = inno.extract_all(&unpacked)?;
    events::info(sink, format!("{} installer files extracted (SHA-1 verified)", files.len()));
    // Codecs are installed to Inno's {tmp}; fall back to the root for other layouts.
    let codec_dir = if unpacked.join("tmp").is_dir() { unpacked.join("tmp") } else { unpacked.clone() };
    let work = root.join("work");
    std::fs::create_dir_all(&work)?;
    // CLS.ini carries "{app}" placeholders the installer fills in (temp folders for codecs).
    for name in ["CLS.ini", "cls.ini"] {
        let p = codec_dir.join(name);
        if let Ok(b) = std::fs::read(&p) {
            let text = String::from_utf8_lossy(&b).replace("{app}", &work.to_string_lossy());
            std::fs::write(&p, text)?;
            break;
        }
    }
    let (_, physical) = crate::guard::memory();
    if let Some(note) = crate::memlimit::make_safe(&codec_dir, physical) {
        events::info(sink, note);
    }
    let toml = if codec_dir.join("arc.ini").is_file() {
        crate::arcini::to_toml(&codec_dir.join("arc.ini"))?
    } else {
        let mut t = format!("codec_dir = '{}'\n", codec_dir.display());
        if codec_dir.join("unarc.dll").is_file() {
            t.push_str("unarc = 'unarc.dll'\n");
        }
        t
    };
    std::fs::write(&config, toml)?;
    std::fs::write(root.join(".ready"), "")?;
    Ok(config)
}

/// Checks the archives against the repack's MD5 list. Returns how many were fully checked
/// (fewer if the rest of the check was skipped through `skip`).
/// Checks the downloaded archives against the repack's MD5 list: all of them, or with `only`,
/// the ones the install uses (see `norm_path`).
fn verify_md5(scan: &Scan, only: Option<&HashSet<String>>, cancel: &AtomicBool, skip: &AtomicBool, sink: &Sink) -> Result<usize> {
    let todo: Vec<&ScannedArchive> =
        scan.archives.iter().filter(|a| a.md5.is_some() && only.is_none_or(|o| o.contains(&norm_path(&a.archive.path)))).collect();
    if todo.is_empty() {
        events::warn(sink, "no MD5 list found next to the archives; skipping verification");
        return Ok(0);
    }
    let total: u64 = todo.iter().map(|a| a.archive.len).sum();
    sink(Event::Plan { jobs: todo.len(), compressed: total, output: 0 });
    let results: Vec<Result<bool>> = std::thread::scope(|s| {
        let handles: Vec<_> = todo
            .iter()
            .map(|a| {
                let id = extract::next_job_id();
                s.spawn(move || -> Result<bool> {
                    let name = a.archive.name();
                    let label = format!("checking {name}");
                    sink(Event::JobStarted { id, label: label.clone(), method: "md5".into(), compressed: a.archive.len, output: 0 });
                    let started = std::time::Instant::now();
                    let mut f = std::fs::File::open(&a.archive.path)?;
                    let mut ctx = md5::Context::new();
                    let mut buf = vec![0u8; 8 << 20];
                    let mut done = 0u64;
                    loop {
                        if cancel.load(Ordering::Relaxed) {
                            bail!("cancelled");
                        }
                        if skip.load(Ordering::Relaxed) {
                            sink(Event::JobFinished {
                                id,
                                label: name.clone(),
                                files: 0,
                                output: 0,
                                secs: started.elapsed().as_secs_f64(),
                                codecs: Vec::new(),
                                error: None,
                            });
                            return Ok(false);
                        }
                        let n = f.read(&mut buf)?;
                        if n == 0 {
                            break;
                        }
                        ctx.consume(&buf[..n]);
                        done += n as u64;
                        sink(Event::JobProgress { id, done });
                    }
                    let got = format!("{:x}", ctx.finalize());
                    let ok = Some(&got) == a.md5.as_ref();
                    sink(Event::JobFinished {
                        id,
                        label: name.clone(),
                        files: 1,
                        output: 0,
                        secs: started.elapsed().as_secs_f64(),
                        codecs: Vec::new(),
                        error: (!ok).then(|| format!("MD5 mismatch: got {got}, expected {}", a.md5.as_deref().unwrap_or("?"))),
                    });
                    ensure!(ok, "{name} is damaged (MD5 mismatch); download it again");
                    Ok(true)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(anyhow::anyhow!("verify thread panicked")))).collect()
    });
    let mut checked = 0;
    for r in results {
        checked += r? as usize;
    }
    if checked < todo.len() {
        events::info(sink, "archive check skipped; installed files are still checked against the repack's MD5 list");
    }
    Ok(checked)
}

fn run_role(
    archives: &[&ScannedArchive],
    cfg: &Config,
    out: &Path,
    overwrite: bool,
    opts: &InstallOptions,
    done_dir: Option<PathBuf>,
    md5: Option<Arc<Md5List>>,
    sink: &Sink,
) -> Result<Summary> {
    if archives.is_empty() {
        return Ok(Summary::default());
    }
    let reopened: Vec<Archive> = archives.iter().map(|a| Archive::open(&a.archive.path)).collect::<Result<_>>()?;
    let eopts = Options {
        out_dir: Some(out.to_path_buf()),
        jobs: opts.jobs,
        writers: opts.writers,
        overwrite,
        skip_missing: false,
        tune: opts.tune,
        temp_dir: opts.cache_dir.join("tmp"),
        host_dir: opts.host_dir.clone(),
        stall_timeout: opts.stall_timeout,
        cancel: opts.cancel.clone(),
        heavy_jobs: opts.heavy_jobs,
        done_dir,
        md5,
        md5_prefix: String::new(),
        first_job_started: None,
    };
    std::fs::create_dir_all(&eopts.temp_dir)?;
    extract::run(reopened, cfg, &eopts, sink)
}

/// Installs a repack into `opts.target_dir` (see `install_game`), then makes the shortcuts and
/// the uninstaller.
pub fn install(opts: &InstallOptions, sink: &Sink) -> Result<InstallReport> {
    let target = std::path::absolute(&opts.target_dir).unwrap_or_else(|_| opts.target_dir.clone());
    // What the folder holds already: the uninstaller removes only what the install adds.
    let before = (opts.uninstaller && !opts.dry_run).then(|| crate::uninstall::before_install(&target, &opts.cache_dir));
    let mut report = install_game(opts, sink)?;
    if opts.dry_run || report.summary.cancelled {
        return Ok(report);
    }
    let app = report.app.clone().unwrap_or_default();
    let name = app.display_name().unwrap_or_else(|| target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Game".into()));
    let mut made = Vec::new();
    let mut made_dirs = Vec::new();
    if report.summary.problems.is_empty() && !report.added_parts {
        if opts.desktop_shortcut && !report.shortcuts.is_empty() {
            match crate::setup_script::create_desktop_shortcuts(&report.shortcuts) {
                Ok(links) => made.extend(links),
                Err(e) => report.summary.problems.push(format!("the desktop shortcut: {e:#}")),
            }
        }
        if opts.start_menu && !report.start_menu.is_empty() {
            let group = Some(app.group.trim()).filter(|g| !g.is_empty() && !g.contains('{')).map_or_else(|| name.clone(), str::to_string);
            match crate::setup_script::create_start_menu_shortcuts(&group, &report.start_menu) {
                Ok((links, dirs)) => {
                    made.extend(links);
                    made_dirs = dirs;
                }
                Err(e) => report.summary.problems.push(format!("the Start menu entry: {e:#}")),
            }
        }
    }
    report.shortcuts_made = made.clone();
    // When parts are added to a game installed some other way (FitGirl's own installer), that
    // game's own uninstaller is left as it is.
    if let Some(before) = before
        && (!report.added_parts || target.join(crate::uninstall::LIST).is_file())
    {
        let icon = app
            .uninstall_icon
            .strip_prefix("{app}")
            .map(|rest| target.join(rest.trim_start_matches('\\')))
            .filter(|p| p.is_file())
            .or_else(|| report.shortcuts.iter().chain(&report.start_menu).map(|s| s.target.clone()).next());
        let details = crate::uninstall::Details { name, publisher: app.publisher.clone(), version: app.version.clone(), icon };
        let made_uninstaller = crate::helpers::find(&[crate::uninstall::PROGRAM], &opts.host_dir)
            .and_then(|program| crate::uninstall::create(&target, &before, &details, &made, &made_dirs, &program, true));
        match made_uninstaller {
            Ok(notes) => {
                report.summary.notes.extend(notes);
                report.uninstaller = Some(target.join(crate::uninstall::EXE));
                crate::uninstall::forget_before(&target, &opts.cache_dir);
            }
            Err(e) => report.summary.problems.push(format!("the uninstaller: {e:#}")),
        }
    }
    Ok(report)
}

/// The install itself.
fn install_game(opts: &InstallOptions, sink: &Sink) -> Result<InstallReport> {
    sink(Event::Phase { name: "Scanning".into() });
    let first = scan(&opts.repack_dir, None)?;
    // Preferred: run the installer's own install script (exact result, any post-processing).
    if opts.script
        && opts.config.is_none()
        && let Some(setup) = &first.setup
    {
        let usable = crate::inno::Setup::open(setup).and_then(|inno| {
            ensure!(!inno.code.is_empty(), "the installer has no install script");
            let script = crate::pascal::Script::load(&inno.code)?;
            ensure!(script.find("CURSTEPCHANGED").is_some(), "the installer's script has no install procedure");
            Ok((inno, script))
        });
        match usable {
            Ok((inno, script)) => return install_scripted(opts, &first, setup, &inno, &script, sink),
            Err(e) => events::warn(sink, format!("can't run the installer's own script ({e:#}); extracting the archives directly")),
        }
    }
    let config_path = match &opts.config {
        Some(c) => c.clone(),
        None => {
            sink(Event::Phase { name: "Preparing codecs".into() });
            let setup = first.setup.as_deref().context("no Inno Setup installer (setup.exe) found in the repack folder")?;
            prepare_codecs(setup, &opts.cache_dir, sink)?
        }
    };
    let cfg = Config::load(&config_path)?;
    let scan = scan(&opts.repack_dir, Some(&cfg))?;
    for a in &scan.archives {
        events::info(
            sink,
            format!("{}: {} → {}, {}", a.archive.name(), human(a.archive.len), human(a.archive.total_size()), a.role.describe()),
        );
    }

    let need = scan.game_bytes();
    if let Some(free) = crate::guard::free_space(&opts.target_dir) {
        ensure!(
            free > need + (1 << 30),
            "not enough space on the target drive: need {}, {} free",
            human(need),
            human(free)
        );
    }

    if opts.dry_run {
        events::info(sink, format!("would install {} of game files into {}", human(need), opts.target_dir.display()));
        return Ok(InstallReport {
            summary: Summary::default(),
            codec_dir: cfg.codec_dir(),
            scripts: Vec::new(),
            scripts_dir: None,
            md5_checked: 0,
            setup_files: 0,
            files_md5: 0,
            script_steps: 0,
            shortcuts: Vec::new(),
            ..Default::default()
        });
    }

    let mut md5_checked = 0;
    if opts.verify_md5 {
        sink(Event::Phase { name: "Verifying archives".into() });
        md5_checked = verify_md5(&scan, None, &opts.cancel, &opts.skip_verify, sink)?;
    }

    let by_role = |r: Role| scan.archives.iter().filter(|a| a.role == r).collect::<Vec<_>>();
    let codec_dir = cfg.codec_dir();

    let tools = by_role(Role::CodecTools);
    if !tools.is_empty() {
        sink(Event::Phase { name: "Extracting codec tools".into() });
        let s = run_role(&tools, &cfg, &codec_dir, true, opts, None, None, sink)?;
        ensure!(s.problems.is_empty() && !s.cancelled, "codec tools failed: {}", s.problems.join("; "));
    }

    let mut scripts = Vec::new();
    let mut scripts_dir = None;
    let script_archives = by_role(Role::Scripts);
    if !script_archives.is_empty() {
        sink(Event::Phase { name: "Setting aside installer scripts".into() });
        let dir = opts.cache_dir.join("scripts").join(opts.repack_dir.file_name().unwrap_or_default());
        let s = run_role(&script_archives, &cfg, &dir, true, opts, None, None, sink)?;
        if !s.problems.is_empty() {
            events::warn(sink, format!("installer scripts: {}", s.problems.join("; ")));
        }
        for a in &script_archives {
            for e in a.archive.entries.iter().filter(|e| !e.is_dir) {
                let p = e.path.to_ascii_lowercase();
                if p.ends_with(".bat") || p.ends_with(".cmd") {
                    scripts.push(dir.join(&e.path));
                }
            }
        }
        scripts_dir = Some(dir);
    }

    sink(Event::Phase { name: "Installing game files".into() });
    std::fs::create_dir_all(&opts.target_dir)?;
    // Resume: finished archives leave a marker per target folder; a retry skips them and
    // overwrites whatever the failed one left half-written.
    let target_key = sha1_smol::Sha1::from(opts.target_dir.to_string_lossy().to_lowercase()).digest().to_string();
    let done_dir = opts.cache_dir.join("progress").join(&target_key[..12]);
    let resuming = std::fs::read_dir(&done_dir).map(|mut d| d.next().is_some()).unwrap_or(false);
    if resuming {
        events::info(sink, "resuming an earlier install into this folder: finished archives are skipped");
    }
    let overwrite = opts.overwrite || resuming;

    // setup.exe carries a few files of its own for the game folder, and usually the repack's
    // list of installed-file MD5s: with it, every listed file is checked while it is written,
    // which replaces the installer's separate "verify files" pass.
    let inno = first.setup.as_ref().and_then(|s| match crate::inno::Setup::open(s) {
        Ok(i) => Some(i),
        Err(e) => {
            events::warn(sink, format!("can't read {}: {e:#}", s.display()));
            None
        }
    });
    let md5 = inno
        .as_ref()
        .and_then(|i| match md5_list_from_setup(i, None) {
            Ok(l) => l,
            Err(e) => {
                events::warn(sink, format!("installed-file MD5 list: {e:#}"));
                None
            }
        })
        .map(Arc::new);
    if let Some(l) = &md5 {
        events::info(sink, format!("{} files will be checked against the repack's MD5 list as they are written", l.len()));
    }

    let game = by_role(Role::Game);
    let mut summary = run_role(&game, &cfg, &opts.target_dir, overwrite, opts, Some(done_dir), md5.clone(), sink)?;
    let mut setup_files = 0;
    if !summary.cancelled
        && let Some(inno) = &inno
    {
        match install_setup_files(inno, &opts.target_dir, sink, None, false) {
            Ok(n) => setup_files = n,
            Err(e) => summary.problems.push(format!("files from setup.exe: {e:#}")),
        }
    }
    let mut files_md5 = 0;
    if !summary.cancelled
        && let Some(list) = &md5
    {
        sink(Event::Phase { name: "Checking files".into() });
        let in_archives: HashSet<String> = game
            .iter()
            .flat_map(|a| a.archive.entries.iter().filter(|e| !e.is_dir).map(|e| extract::md5_key(&e.path)))
            .collect();
        let entries: Vec<(&String, &ExpectedMd5)> = list.iter().collect();
        let check = check_md5_list(&entries, &opts.target_dir, Some(&in_archives), &opts.cancel);
        if check.after > 0 {
            events::info(sink, format!("{} files written by unarc.dll or an earlier run were hashed afterwards", check.after));
        }
        files_md5 = check.ok;
        summary.problems.extend(check.problems);
        summary.notes.extend(check.notes);
    }
    sink(Event::Phase { name: if summary.cancelled { "Cancelled".into() } else { "Done".into() } });
    Ok(InstallReport {
        summary,
        codec_dir,
        scripts,
        scripts_dir,
        md5_checked,
        setup_files,
        files_md5,
        script_steps: 0,
        app: inno.as_ref().map(|i| i.app.clone()),
        ..Default::default()
    })
}

/// Installs by running the installer's own install script (see `setup_script`): every archive
/// extraction goes through arc-unpack's engine, the repack's own post-processing runs as the
/// script says, and the result is checked against the repack's MD5 list.
fn install_scripted(
    opts: &InstallOptions,
    scan: &Scan,
    setup_exe: &Path,
    inno: &crate::inno::Setup,
    script: &crate::pascal::Script,
    sink: &Sink,
) -> Result<InstallReport> {
    use crate::setup_script as ss;
    let abs = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let target = abs(&opts.target_dir);
    let selected: std::collections::BTreeSet<String> = match &opts.components {
        Some(list) => ss::with_exclusive_choices(inno, list.iter().map(|c| c.trim().to_ascii_lowercase()).filter(|c| !c.is_empty()).collect()),
        None => ss::apply_wizard_checks(
            inno,
            ss::default_components(inno, &ss::repack_files(&opts.repack_dir)),
            &ss::wizard_checks(inno, script, setup_exe, &opts.repack_dir),
        ),
    };
    events::info(sink, format!("running the installer's own install script; components: {}", selected.iter().cloned().collect::<Vec<_>>().join(", ")));
    // Optional parts the selection leaves out, and whether this only adds parts to a game that
    // is already installed (no base-game component selected).
    let parts = ss::parts_for(inno, &ss::repack_files(&opts.repack_dir), &selected);
    if parts.only_optional {
        let game = find_installed(inno, &target)?;
        events::info(sink, format!("adding parts to the game installed in {} ({} files)", target.display(), game.files));
    }
    if !parts.skip.is_empty() {
        events::info(sink, format!("not installing (unticked): {}", parts.skip.iter().cloned().collect::<Vec<_>>().join(", ")));
    }

    // What the script will do with this selection: which repack archives it needs (all must be
    // here), how much space they take, and what can be extracted early.
    let plan = match ss::plan_with(inno, script, setup_exe, &opts.repack_dir, &target, Some(selected.clone())) {
        Ok(p) => Some(p),
        Err(e) => {
            events::warn(sink, format!("couldn't plan the install ahead: {e:#}"));
            None
        }
    };
    if parts.only_optional
        && let Some(p) = &plan
    {
        ensure!(
            p.steps.iter().any(|s| matches!(s, ss::Step::Extract { .. })),
            "nothing to add: the selected parts' archives aren't in the repack folder"
        );
    }
    let needed = plan.as_ref().map(|p| ss::preflight(p, &opts.repack_dir));
    if let Some(pf) = &needed {
        ensure!(
            pf.missing.is_empty(),
            "the selected parts need {} which {} not in the repack folder: download {}, or don't select the parts that need {}",
            pf.missing.join(", "),
            if pf.missing.len() == 1 { "is" } else { "are" },
            if pf.missing.len() == 1 { "it" } else { "them" },
            if pf.missing.len() == 1 { "it" } else { "them" }
        );
    }
    let (need, packed) = needed_size(scan, needed.as_ref());
    if let Some(free) = crate::guard::free_space(&target) {
        ensure!(free > need + (1 << 30), "not enough space on the target drive: need {}, {} free", human(need), human(free));
    }
    if opts.dry_run {
        let plan = plan.context("the install script couldn't be planned")?;
        for (i, s) in plan.steps.iter().enumerate() {
            events::info(sink, format!("step {}: {s}", i + 1));
        }
        if let Some(e) = plan.error {
            events::warn(sink, format!("the script would stop: {e}"));
        }
        events::info(sink, format!("would install up to {} into {}", human(need), target.display()));
        let mut report = empty_report(PathBuf::new());
        report.script_steps = plan.steps.len();
        return Ok(report);
    }

    // The downloaded archives are checked against the repack's MD5 list while the install runs
    // (the same bytes are being read anyway). A damaged archive stops the install.
    sink(Event::Phase { name: "Installing".into() });
    sink(Event::Total { compressed: packed, output: need });
    let damaged: Mutex<Option<String>> = Mutex::new(None);
    // Only the archives this install uses are checked (not left-out parts, nor the base game's
    // archives when adding parts).
    let uses: Option<HashSet<String>> = needed.as_ref().map(|pf| pf.needed.iter().map(|p| norm_path(p)).collect());
    std::thread::scope(|threads| {
        let check = opts.verify_md5.then(|| {
            threads.spawn(|| match verify_md5(scan, uses.as_ref(), &opts.cancel, &opts.skip_verify, sink) {
                Ok(n) => n,
                Err(e) => {
                    if !opts.cancel.load(Ordering::Relaxed) {
                        *damaged.lock().unwrap() = Some(format!("{e:#}"));
                        opts.cancel.store(true, Ordering::Relaxed);
                    }
                    0
                }
            })
        });
        let report = run_scripted(opts, setup_exe, inno, script, sink, &target, selected, &parts, plan.as_ref());
        let md5_checked = check.map(|c| c.join().unwrap_or(0)).unwrap_or(0);
        let damaged = damaged.lock().unwrap().take();
        match (report, damaged) {
            // The archive differs from the list, yet every installed file was verified.
            (Ok(mut r), Some(why)) if r.files_md5 > 0 && r.summary.problems.is_empty() && !r.summary.cancelled => {
                r.summary.notes.push(format!("{why} (every installed file still matched the repack's MD5 list)"));
                r.md5_checked = md5_checked;
                Ok(r)
            }
            (_, Some(why)) => bail!("{why}. The install was stopped; the partly installed game folder can be deleted."),
            (report, None) => report.map(|mut r| {
                r.md5_checked = md5_checked;
                r
            }),
        }
    })
    .map(|mut r| {
        // Runtimes the game needs, once the game itself is in place.
        if opts.runtimes && r.summary.problems.is_empty() && !r.summary.cancelled {
            let list = crate::runtimes::of_repack(inno, Some(script), &ss::repack_files(&opts.repack_dir));
            let missing = crate::runtimes::missing(&list);
            if missing.is_empty() {
                r.runtimes.push("All the runtimes this game needs are already installed".into());
            } else {
                sink(Event::Phase { name: "Installing runtimes".into() });
                let (done, problems) = crate::runtimes::install(inno, &missing, &opts.cache_dir.join("runtimes"), sink);
                r.runtimes = done;
                r.summary.problems.extend(problems.into_iter().map(|p| format!("runtime {p}")));
                sink(Event::Phase { name: "Done".into() });
            }
        }
        r
    })
}

/// The install itself: the script's steps, the installer's own files, the final file check.
#[allow(clippy::too_many_arguments)]
fn run_scripted(
    opts: &InstallOptions,
    setup_exe: &Path,
    inno: &crate::inno::Setup,
    script: &crate::pascal::Script,
    sink: &Sink,
    target: &Path,
    selected: std::collections::BTreeSet<String>,
    parts: &crate::setup_script::Parts,
    plan: Option<&crate::setup_script::Plan>,
) -> Result<InstallReport> {
    use crate::setup_script::{self as ss, Folders, InnoHost};
    let abs = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let target = target.to_path_buf();
    let md5_checked = 0;

    // A fresh copy of the installer's temporary files per run: the script edits some of them.
    let target_key = sha1_smol::Sha1::from(target.to_string_lossy().to_lowercase()).digest().to_string();
    let run_dir = opts.cache_dir.join("run").join(&target_key[..12]);
    let _ = std::fs::remove_dir_all(&run_dir);
    let tmp = run_dir.join("tmp");
    ss::prepare_tmp(inno, &tmp).context("unpacking the installer's temporary files")?;
    // CLS.ini carries "{app}" placeholders (codecs' temp folders) the installer fills in.
    for name in ["CLS.ini", "cls.ini"] {
        let p = tmp.join(name);
        if let Ok(b) = std::fs::read(&p) {
            let text = String::from_utf8_lossy(&b);
            if text.contains("{app}") {
                std::fs::write(&p, text.replace("{app}", &target.to_string_lossy()))?;
            }
            break;
        }
    }
    crate::memlimit::set(opts.memory_limit);
    let (_, physical) = crate::guard::memory();
    if let Some(note) = crate::memlimit::make_safe(&tmp, physical) {
        events::info(sink, note);
    }
    if let Some(limit) = opts.memory_limit {
        events::info(sink, crate::memlimit::apply(&tmp, limit, physical));
    }
    std::fs::create_dir_all(&target)?;

    let md5 = match md5_list_from_setup(inno, Some(&selected)) {
        Ok(l) => l.map(Arc::new),
        Err(e) => {
            events::warn(sink, format!("installed-file MD5 list: {e:#}"));
            None
        }
    };

    // Repack archives the script extracts later start early, into a staging folder next to the
    // game folder, while earlier steps run; at their step they are moved into place.
    let staging = staging_dir(&target);
    let _ = std::fs::remove_dir_all(&staging);
    // (Not when adding parts: their archives replace game files one at a time, with backups.)
    let prefetch = match plan {
        Some(p) if !parts.only_optional && std::env::var_os("ARC_UNPACK_NO_PREFETCH").is_none() => {
            plan_prefetches(p, &abs(&opts.repack_dir), &target, &staging)
        }
        _ => Vec::new(),
    };

    let mut runner = ScriptRunner {
        opts,
        sink,
        md5: md5.clone(),
        app: target.clone(),
        tmp: tmp.clone(),
        summary: Summary::default(),
        configs: Vec::new(),
        n: 0,
        prefetch,
        changes: HashMap::new(),
        steps_total: plan.map(|p| p.steps.len()).unwrap_or(0),
        backup: parts.only_optional.then(|| target.join(BACKUP_DIR)),
        deferred: Vec::new(),
        background: Vec::new(),
        tuned: HashSet::new(),
        standins: (std::env::var_os("ARC_UNPACK_NO_TOOL_STANDINS").is_none())
            .then(|| crate::helpers::find(&[crate::tools::PROGRAM], &opts.host_dir).ok())
            .flatten()
            .map(|p| (p, run_dir.join(crate::tools::MAP_FILE), run_dir.join("tool-originals"))),
        overlapped: Vec::new(),
        safe_tools: HashMap::new(),
    };
    let folders = Folders { app: target.clone(), tmp: tmp.clone(), src: abs(&opts.repack_dir), setup_exe: abs(setup_exe) };
    let mut host = InnoHost::new(folders, inno, selected.clone(), &mut runner);
    host.parts = parts.clone();
    let mut vm = crate::pascal::Vm::new(script);
    let mut script_error = None;
    let mut setup_files = 0;
    // Inno's order: ssInstall, then its [Files], then ssPostInstall (where repacks unpack), ssDone.
    for step in [ss::SS_INSTALL, ss::SS_POST_INSTALL, ss::SS_DONE] {
        if step == ss::SS_POST_INSTALL {
            match install_setup_files(inno, &target, sink, Some(&selected), parts.only_optional) {
                Ok(n) => setup_files = n,
                Err(e) => script_error = Some(format!("files from setup.exe: {e:#}")),
            }
        }
        if script_error.is_some() {
            break;
        }
        if let Err(e) = ss::run_step(script, &mut vm, &mut host, step) {
            script_error = Some(format!("{e:#}"));
        }
    }
    let messages = std::mem::take(&mut host.messages);
    let script_steps = host.steps.len();
    drop(host);
    runner.finish_background(script_error.is_some());
    runner.report_standins();
    runner.discard_prefetches();
    let _ = std::fs::remove_dir_all(&staging);
    let mut summary = runner.summary;
    summary.notes.extend(messages);
    if opts.cancel.load(Ordering::Relaxed) {
        summary.cancelled = true;
    } else if let Some(e) = script_error {
        summary.problems.push(format!("the installer's script stopped: {e}"));
    }

    // Adding parts: the merge of the parts' file lists into the game's was left out with the
    // base game's archives; done here. Only the parts' files are checked then.
    let mut part_keys = None;
    if parts.only_optional && !summary.cancelled && summary.problems.is_empty() {
        match merge_part_lists(inno, &target, &selected) {
            Ok(k) => part_keys = Some(k),
            Err(e) => summary.problems.push(format!("adding the parts' files to the game's file list: {e:#}")),
        }
    }
    let mut files_md5 = 0;
    if !summary.cancelled
        && let Some(list) = &md5
    {
        sink(Event::Phase { name: "Checking files".into() });
        // Check what the installed list (after the script merged optional parts) names.
        let wanted = match part_keys {
            Some(k) => Some(k),
            None => md5_keys_on_disk(inno, &target),
        };
        let entries: Vec<(&String, &ExpectedMd5)> = list.iter().filter(|(k, _)| wanted.as_ref().is_none_or(|w| w.contains(*k))).collect();
        let check = check_md5_list(&entries, &target, None, &opts.cancel);
        files_md5 = check.ok;
        summary.problems.extend(check.problems);
        summary.notes.extend(check.notes);
    }
    let _ = std::fs::remove_dir_all(&run_dir);
    let shortcuts = ss::desktop_shortcuts(inno, &selected, &target).into_iter().filter(|s| s.target.is_file()).collect();
    let start_menu = ss::start_menu_shortcuts(inno, &selected, &target).into_iter().filter(|s| s.target.is_file()).collect();
    sink(Event::Phase { name: if summary.cancelled { "Cancelled".into() } else { "Done".into() } });
    Ok(InstallReport {
        summary,
        codec_dir: tmp,
        scripts: Vec::new(),
        scripts_dir: None,
        md5_checked,
        setup_files,
        files_md5,
        script_steps,
        shortcuts,
        start_menu,
        app: Some(inno.app.clone()),
        added_parts: parts.only_optional,
        ..Default::default()
    })
}

/// Space the install needs (everything the needed archives hold; inner archives are extracted
/// and deleted again) and the compressed bytes it reads. Without a plan: every archive.
pub fn needed_size(scan: &Scan, needed: Option<&crate::setup_script::Preflight>) -> (u64, u64) {
    // Script paths can carry doubled separators ("R:\\fg-01.bin" from "{src}\\..." with
    // {src} = "R:\\"): compare normalized, case-insensitively.
    let needed: Option<Vec<String>> = needed.map(|n| n.needed.iter().map(|x| norm_path(x)).collect());
    let wanted = |a: &&ScannedArchive| needed.as_ref().is_none_or(|n| n.contains(&norm_path(&a.archive.path)));
    scan.archives.iter().filter(wanted).fold((0, 0), |(t, c), a| (t + a.archive.total_size(), c + a.archive.len))
}

/// A path for comparing: absolute, normalized (script paths can carry doubled separators) and
/// lower case.
fn norm_path(p: &Path) -> String {
    let p = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    p.components().collect::<PathBuf>().to_string_lossy().to_lowercase()
}

fn empty_report(codec_dir: PathBuf) -> InstallReport {
    InstallReport {
        summary: Summary::default(),
        codec_dir,
        scripts: Vec::new(),
        scripts_dir: None,
        md5_checked: 0,
        setup_files: 0,
        files_md5: 0,
        script_steps: 0,
        ..Default::default()
    }
}

/// Keys of the MD5 lists as they are in the game folder after the script ran (it may have
/// merged optional parts' lists into the main one), or None if there are none.
fn md5_keys_on_disk(inno: &crate::inno::Setup, target: &Path) -> Option<HashSet<String>> {
    let mut list = Md5List::new();
    for f in &inno.files {
        let Some(rel) = f.app_relative() else { continue };
        if !rel.to_ascii_lowercase().ends_with(".md5") {
            continue;
        }
        let Ok(text) = std::fs::read(target.join(rel)) else { continue };
        let mut base: Vec<&str> = rel.split(['\\', '/']).filter(|p| !p.is_empty()).collect();
        base.pop();
        parse_installed_md5(&md5_list_text(&text), &base, &mut list);
    }
    (!list.is_empty()).then(|| list.into_keys().collect())
}

/// Adding parts: where game files a part replaces are kept (inside the game folder).
pub const BACKUP_DIR: &str = "_arc-unpack-backup";

/// Carries out the script's steps with arc-unpack's engine.
struct ScriptRunner<'a> {
    opts: &'a InstallOptions,
    sink: &'a Sink,
    md5: Option<Arc<Md5List>>,
    app: PathBuf,
    summary: Summary,
    /// Codec configs imported from the script's arc.ini, by (path, content hash).
    configs: Vec<(PathBuf, String, Config)>,
    n: usize,
    tmp: PathBuf,
    prefetch: Vec<Prefetch>,
    /// Changes the script has made so far to each codec config, by lowercase file name.
    changes: HashMap<String, usize>,
    /// Steps in the plan (0 if unknown).
    steps_total: usize,
    /// Adding parts: game files an archive replaces are moved under here first.
    backup: Option<PathBuf>,
    /// Programs the script started without waiting for them: not started yet, and running (by
    /// name).
    deferred: Vec<(PathBuf, String, PathBuf)>,
    background: Vec<(String, std::process::Child)>,
    /// Processes already tuned (see `exec`).
    tuned: HashSet<u32>,
    /// Stand-ins for repack tools we know exactly (src/tools.rs): the stand-in program, the
    /// list of stand-ins and where the originals go. None: disabled or not found.
    standins: Option<(PathBuf, PathBuf, PathBuf)>,
    /// Tool steps running beside the next steps (see `overlap_paths`), and which programs
    /// qualify, by path.
    overlapped: Vec<Overlapped>,
    safe_tools: HashMap<PathBuf, Option<usize>>,
}

/// A tool step running beside the next steps.
struct Overlapped {
    n: usize,
    program: PathBuf,
    args: String,
    child: std::process::Child,
    cpu: Option<crate::perf::CpuJob>,
    started: std::time::Instant,
    /// The files and folders its command line names (see `path_key`).
    paths: Vec<String>,
}

/// How long programs the script started without waiting may run on after the script ended.
const BACKGROUND_GRACE: std::time::Duration = std::time::Duration::from_secs(300);

/// A repack archive the script will extract later, extracted early into a staging folder.
struct Prefetch {
    archive: PathBuf,
    out: PathBuf,
    base: Option<String>,
    /// The codec config it uses, relative to `{tmp}`, and its lowercase file name.
    cfg_rel: PathBuf,
    cfg_name: String,
    /// How many changes the script makes to that config before this step: the early
    /// extraction starts once the real run has made them all, so it uses the same config.
    changes_before: usize,
    stage: PathBuf,
    cancel: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<Result<Summary>>>,
    used: bool,
    /// The programs its codecs run (worked out once its config is final).
    programs: Option<Vec<PathBuf>>,
}

/// The codec programs an archive's blocks run, given by a path (`xt\xtool.exe`, or absolute).
/// Scripts often unpack these into their temp folder in an early step; an early extraction
/// waits until they are there. (Bare names may be found on PATH, so they aren't waited for.)
fn codec_programs(cfg: &Config, archive: &Path) -> Vec<PathBuf> {
    let Ok(a) = Archive::open(archive) else { return Vec::new() };
    let mut out: Vec<PathBuf> = Vec::new();
    for b in &a.blocks {
        let Ok(stages) = cfg.resolve(&b.method) else { continue }; // unarc.dll's job, not ours
        for s in Config::flatten(&stages) {
            if let crate::config::Codec::External(e) = &s.codec {
                // (A program that isn't there yet keeps its relative path: inside codec_dir.)
                let p = PathBuf::from(&e.argv[0]);
                let p = if p.is_absolute() {
                    p
                } else if p.components().count() > 1 {
                    cfg.codec_dir().join(p)
                } else {
                    continue;
                };
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// Staging folder for early extractions: next to the game folder (same drive, so moving the
/// files in is a rename) and outside it (the script never sees it).
fn staging_dir(target: &Path) -> PathBuf {
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "game".into());
    target.with_file_name(format!("{name}.arc-unpack-staging"))
}

fn is_batch(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("bat") || e.eq_ignore_ascii_case("cmd"))
}

/// Exit codes that don't mean failure:
/// - fart.exe (FART 1.99b, FitGirl's find-and-replace) returns how many replacements it made;
///   only -1 to -4 are errors;
/// - FitGirl's buildNN.bat files end by deleting themselves, so cmd then fails to read the next
///   line and returns 1.
fn benign_exit(program: &Path, code: i32) -> bool {
    let fart = program.file_name().is_some_and(|n| n.eq_ignore_ascii_case("fart.exe"));
    (fart && code >= 0) || (is_batch(program) && code == 1 && !program.exists())
}

/// Can the batch file `bat` (run in `dir`) change what an early extraction with codec config
/// `cfg_name` depends on: the config itself, or anything else in the temp folder `tmp` (codec
/// programs, CLS.ini)? Read with the run.exe lists it passes on. FitGirl's buildNN.bat files
/// only use `{tmp}\work\` (their tools and lists) and the game folder. Anything unclear counts
/// as yes: an unreadable file, another batch file, the config's name as a word, a path into
/// `{tmp}` outside `work\`, or running in `{tmp}` itself.
fn batch_touches(bat: &Path, dir: &Path, cfg_name: &str, tmp: &Path) -> bool {
    let lower = |p: &Path| p.to_string_lossy().to_lowercase().trim_end_matches('\\').to_string();
    let tmp_l = format!("{}\\", lower(tmp));
    let work = format!("{tmp_l}work\\");
    let dir_l = format!("{}\\", lower(dir));
    if dir_l.starts_with(&tmp_l) && !dir_l.starts_with(&work) {
        return true;
    }
    let read = |p: &Path| std::fs::read(p).ok().map(|b| b.iter().map(|&c| (c as char).to_ascii_lowercase()).collect::<String>());
    let Some(text) = read(bat) else { return true };
    let mut texts = vec![text.clone()];
    for line in text.lines().filter(|l| l.contains("run.exe")) {
        // "<tmp>\work\run.exe" " " <url> "<tmp>\work\fitgirl01.txt" 0: the list is the third
        // word after run.exe.
        let words = cmd_words(line);
        let Some(list) = words.iter().position(|w| w.ends_with("run.exe")).and_then(|i| words.get(i + 3)) else { return true };
        let list = Path::new(list);
        let list = if list.is_absolute() { list.to_path_buf() } else { dir.join(list) };
        match read(&list) {
            Some(t) => texts.push(t),
            None => return true,
        }
    }
    let own = bat.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    texts.iter().any(|t| {
        has_word(t, cfg_name)
            || t.match_indices(&tmp_l).any(|(i, _)| !t[i + tmp_l.len()..].starts_with("work\\"))
            || t.split(|c: char| c.is_whitespace() || "\"&|<>()".contains(c)).any(|w| {
                let name = w.rsplit(['\\', '/']).next().unwrap_or("");
                (name.ends_with(".bat") || name.ends_with(".cmd")) && name != own
            })
    })
}

/// arc-unpack's command-line program: this one, or the one next to the GUI.
fn cli_exe() -> Option<PathBuf> {
    let me = std::env::current_exe().ok()?;
    if me.file_name().is_some_and(|n| n.eq_ignore_ascii_case("arc-unpack.exe")) {
        return Some(me);
    }
    Some(me.with_file_name("arc-unpack.exe")).filter(|p| p.is_file())
}

/// A cmd command line's `&&`-chained commands (quotes respected); None if it uses `&` alone.
fn split_and_and(line: &str) -> Option<Vec<&str>> {
    let (mut out, mut start, mut quoted) = (Vec::new(), 0, false);
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => quoted = !quoted,
            b'&' if !quoted => {
                if b.get(i + 1) != Some(&b'&') {
                    return None;
                }
                out.push(line[start..i].trim());
                i += 2;
                start = i;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(line[start..].trim());
    (!quoted && out.iter().all(|s| !s.is_empty())).then_some(out)
}

/// `word` as a path, relative to `dir` unless absolute.
fn resolve_in(dir: &Path, word: &str) -> PathBuf {
    let p = Path::new(word);
    if p.is_absolute() { p.to_path_buf() } else { dir.join(p) }
}

/// A path for comparing: absolute, `.`/`..` resolved, lowercase, no trailing separator.
fn path_key(p: &Path) -> Option<String> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            std::path::Component::CurDir => {}
            c => out.push(c),
        }
    }
    Some(out.to_string_lossy().to_lowercase().replace('/', "\\").trim_end_matches('\\').to_string())
}

/// Whether two steps' paths meet: the same file, or one inside the other's folder.
fn paths_overlap(a: &[String], b: &[String]) -> bool {
    a.iter().any(|x| b.iter().any(|y| x == y || x.starts_with(&format!("{y}\\")) || y.starts_with(&format!("{x}\\"))))
}

/// The words of a command line, quotes removed ("a b" c -> [a b, c]).
fn cmd_words(line: &str) -> Vec<String> {
    let (mut words, mut cur, mut quoted, mut any) = (Vec::new(), String::new(), false, false);
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any || !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
                any = false;
            }
            c => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any || !cur.is_empty() {
        words.push(cur);
    }
    words
}

/// `word` in `text` as a whole word (a file name: not part of a longer name).
fn has_word(text: &str, word: &str) -> bool {
    let name_char = |c: char| c.is_ascii_alphanumeric() || "._-".contains(c);
    text.match_indices(word).any(|(i, _)| {
        let before = text[..i].chars().next_back();
        let after = text[i + word.len()..].chars().next();
        before.is_none_or(|c| "\\/\" \t\r\n".contains(c)) && after.is_none_or(|c| !name_char(c))
    })
}

/// Does an Exec step (possibly) change `cfg_name`? Batch files are opaque, so they count.
fn exec_touches(program: &Path, args: &str, cfg_name: &str) -> bool {
    is_batch(program) || args.to_lowercase().contains(cfg_name)
}

fn path_inside(p: &Path, root: &Path) -> bool {
    let (p, r) = (p.to_string_lossy().to_lowercase(), root.to_string_lossy().to_lowercase());
    p.starts_with(&format!("{}\\", r.trim_end_matches('\\')))
}

/// Every extraction of a repack archive (from the repack folder into the game folder) the plan
/// contains, with what must happen before it may start.
fn plan_prefetches(plan: &crate::setup_script::Plan, src: &Path, app: &Path, staging: &Path) -> Vec<Prefetch> {
    use crate::setup_script::Step;
    let mut out = Vec::new();
    for (j, step) in plan.steps.iter().enumerate() {
        let Step::Extract { archive, out: dest, cfg, base } = step else { continue };
        if !path_inside(archive, src) || !(path_inside(dest, app) || dest == app) {
            continue;
        }
        let cfg_rel = cfg.strip_prefix(&plan.tmp).map(Path::to_path_buf).unwrap_or_else(|_| cfg.clone());
        let cfg_name = cfg.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        let same = |p: &Path| p.file_name().is_some_and(|f| f.to_string_lossy().to_lowercase() == cfg_name);
        let writes = plan.writes.iter().filter(|(n, p)| *n <= j && same(p)).count();
        let execs = plan.steps[..j]
            .iter()
            .filter(|s| matches!(s, Step::Exec { program, args, .. } if exec_touches(program, args, &cfg_name)))
            .count();
        out.push(Prefetch {
            archive: archive.clone(),
            out: dest.clone(),
            base: base.clone(),
            cfg_rel,
            cfg_name,
            changes_before: writes + execs,
            stage: staging.join(j.to_string()),
            cancel: Arc::new(AtomicBool::new(false)),
            handle: None,
            used: false,
            programs: None,
        });
    }
    out
}

impl ScriptRunner<'_> {
    fn config(&mut self, arc_ini: &Path) -> Result<Config> {
        let bytes = std::fs::read(arc_ini).with_context(|| format!("read {}", arc_ini.display()))?;
        let hash = sha1_smol::Sha1::from(&bytes).digest().to_string();
        if let Some((_, _, c)) = self.configs.iter().find(|(p, h, _)| p == arc_ini && *h == hash) {
            return Ok(c.clone());
        }
        let toml = crate::arcini::to_toml(arc_ini)?;
        let path = arc_ini.with_extension("arc-unpack.toml");
        std::fs::write(&path, toml)?;
        let cfg = Config::load(&path)?;
        self.configs.push((arc_ini.to_path_buf(), hash, cfg.clone()));
        Ok(cfg)
    }

    /// Extraction options for writing into `dest`; `final_out` is where the files end up (for
    /// MD5 list lookups, which are relative to the game folder).
    fn options(&self, dest: PathBuf, final_out: Option<&Path>, cancel: Arc<AtomicBool>) -> Options {
        let prefix = final_out.and_then(|o| o.strip_prefix(&self.app).ok()).map(|rel| {
            if rel.as_os_str().is_empty() { String::new() } else { format!("{}\\", rel.display()) }
        });
        Options {
            out_dir: Some(dest),
            jobs: self.opts.jobs,
            writers: self.opts.writers,
            overwrite: true,
            skip_missing: false,
            tune: self.opts.tune,
            temp_dir: self.opts.cache_dir.join("tmp"),
            host_dir: self.opts.host_dir.clone(),
            stall_timeout: self.opts.stall_timeout,
            cancel,
            heavy_jobs: self.opts.heavy_jobs,
            done_dir: None,
            md5: prefix.as_ref().and(self.md5.clone()),
            md5_prefix: prefix.unwrap_or_default(),
            first_job_started: None,
        }
    }

    /// Starts every early extraction whose codec config is final by now, largest archive
    /// first: the longest one decides when the install ends, so it must get its memory-heavy
    /// slot before smaller archives take the slots.
    fn start_prefetches(&mut self) {
        let mut ready: Vec<usize> = (0..self.prefetch.len())
            .filter(|&i| {
                let p = &self.prefetch[i];
                !(p.used || p.handle.is_some() || self.changes.get(&p.cfg_name).copied().unwrap_or(0) < p.changes_before)
            })
            .collect();
        let size = |p: &Prefetch| std::fs::metadata(&p.archive).map(|m| m.len()).unwrap_or(0);
        ready.sort_by_key(|&i| std::cmp::Reverse(size(&self.prefetch[i])));
        for i in ready {
            let p = &self.prefetch[i];
            if self.opts.cancel.load(Ordering::Relaxed) {
                return;
            }
            let (archive, stage, cfg_path) = (p.archive.clone(), p.stage.clone(), self.tmp.join(&p.cfg_rel));
            let (out, whole) = (p.out.clone(), p.base.is_none());
            let cfg = match self.config(&cfg_path) {
                Ok(c) => c,
                Err(e) => {
                    events::warn(self.sink, format!("no early extraction of {}: {e:#}", archive.display()));
                    self.prefetch[i].used = true;
                    continue;
                }
            };
            // Its codec programs may still be unpacked by an earlier step: then try again later.
            let programs = self.prefetch[i].programs.get_or_insert_with(|| codec_programs(&cfg, &archive));
            if !programs.iter().all(|p| p.is_file()) {
                continue;
            }
            let mut eopts = self.options(stage.clone(), whole.then_some(out.as_path()), self.prefetch[i].cancel.clone());
            let started = Arc::new(AtomicBool::new(false));
            eopts.first_job_started = Some(started.clone());
            let sink = self.sink.clone();
            let name = archive.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            events::info(self.sink, format!("{name}: extracting early (the script needs it later)"));
            let handle = std::thread::spawn(move || {
                std::fs::create_dir_all(&stage)?;
                std::fs::create_dir_all(&eopts.temp_dir)?;
                extract::run(vec![Archive::open(&archive)?], &cfg, &eopts, &sink)
            });
            // Let it pick its first job (and slot) before the next, smaller one starts.
            let waited = std::time::Instant::now();
            while !started.load(Ordering::Relaxed) && !handle.is_finished() && waited.elapsed() < std::time::Duration::from_secs(3) {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            self.prefetch[i].handle = Some(handle);
        }
    }

    /// The early extraction for this step, if there is one: waits for it and moves its files in.
    /// Returns None if there was none (or it failed), so the step extracts normally.
    fn take_prefetch(&mut self, archive: &Path, out: &Path, base: Option<&str>) -> Result<Option<bool>> {
        let Some(i) = self.prefetch.iter().position(|p| !p.used && p.archive == archive && p.out == out && p.base.as_deref() == base)
        else {
            return Ok(None);
        };
        self.prefetch[i].used = true;
        let Some(handle) = self.prefetch[i].handle.take() else { return Ok(None) };
        let stage = self.prefetch[i].stage.clone();
        while !handle.is_finished() {
            if self.opts.cancel.load(Ordering::Relaxed) {
                self.prefetch[i].cancel.store(true, Ordering::Relaxed);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let result = handle.join().map_err(|_| anyhow::anyhow!("early extraction crashed"))?;
        match result {
            Ok(s) if s.problems.is_empty() && !s.cancelled => {
                let part = match base {
                    Some(b) => stage.join(b.replace('/', "\\")),
                    None => stage.clone(),
                };
                if part.is_dir() {
                    move_tree(&part, out)?;
                }
                let _ = std::fs::remove_dir_all(&stage);
                self.merge(s);
                Ok(Some(true))
            }
            Ok(_) if self.opts.cancel.load(Ordering::Relaxed) => {
                let _ = std::fs::remove_dir_all(&stage);
                bail!("cancelled")
            }
            other => {
                let why = match other {
                    Ok(s) => s.problems.join("; "),
                    Err(e) => format!("{e:#}"),
                };
                let _ = std::fs::remove_dir_all(&stage);
                // A codec that made no progress for the whole stall timeout would hang the same
                // way again: don't make the user wait for it twice.
                if why.contains("no progress for") {
                    bail!("{}: {why}", archive.display());
                }
                events::warn(self.sink, format!("early extraction of {} failed ({why}); extracting again", archive.display()));
                Ok(None)
            }
        }
    }

    /// Stops early extractions the script never asked for and waits for them.
    fn discard_prefetches(&mut self) {
        for p in &mut self.prefetch {
            p.cancel.store(true, Ordering::Relaxed);
        }
        for p in &mut self.prefetch {
            if let Some(h) = p.handle.take() {
                let _ = h.join();
            }
        }
    }

    /// Programs the script started without waiting: Setup leaves them running, but they work
    /// on the game's files (the script usually waits for them itself), so the install waits
    /// for them before the file check. Stopped at once when the script failed or the install
    /// was cancelled, and after `BACKGROUND_GRACE` otherwise.
    fn finish_background(&mut self, failed: bool) {
        // Tool steps still running beside the script's last steps.
        if failed || self.opts.cancel.load(Ordering::Relaxed) {
            self.kill_overlapped();
        } else if let Err(e) = self.join_overlapped() {
            self.summary.problems.push(format!("{e:#}"));
        }
        if !failed
            && !self.opts.cancel.load(Ordering::Relaxed)
            && let Err(e) = self.start_deferred()
        {
            self.summary.problems.push(format!("{e:#}"));
        }
        let started = std::time::Instant::now();
        for (name, mut child) in std::mem::take(&mut self.background) {
            loop {
                if !matches!(child.try_wait(), Ok(None)) {
                    break;
                }
                let cancelled = self.opts.cancel.load(Ordering::Relaxed);
                if failed || cancelled || started.elapsed() > BACKGROUND_GRACE {
                    let _ = child.kill();
                    let _ = child.wait();
                    if !failed && !cancelled {
                        let note = format!("{name} (started without waiting) still ran {} s after the script ended: stopped it", BACKGROUND_GRACE.as_secs());
                        events::warn(self.sink, note.clone());
                        self.summary.notes.push(note);
                    }
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }

    fn merge(&mut self, s: Summary) {
        let sm = &mut self.summary;
        sm.files_ok += s.files_ok;
        sm.files_unverified += s.files_unverified;
        sm.bytes += s.bytes;
        sm.cancelled |= s.cancelled;
        sm.problems.extend(s.problems);
        for n in s.notes {
            if !sm.notes.contains(&n) {
                sm.notes.push(n);
            }
        }
    }

    fn extract(&mut self, archive: &Path, out: &Path, arc_ini: &Path, base: Option<&str>) -> Result<bool> {
        if let Some(ok) = self.take_prefetch(archive, out, base)? {
            return Ok(ok);
        }
        if !archive.is_file() {
            events::warn(self.sink, format!("{}: not found", archive.display()));
            return Ok(false);
        }
        let cfg = self.config(arc_ini)?;
        let a = Archive::open(archive)?;
        if let Some(root) = &self.backup {
            let stem = archive.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "part".into());
            let mut dir = root.join(&stem);
            for i in 2.. {
                if !dir.exists() {
                    break;
                }
                dir = root.join(format!("{stem}-{i}"));
            }
            let moved = back_up_replaced(&a, out, base, &self.app, &dir)?;
            if moved > 0 {
                self.summary.notes.push(format!("{moved} game files {stem} replaces were moved to {}", dir.display()));
            }
        }
        // A part of the archive: extract everything next to the target, then move that part in.
        let stage = base.map(|_| out.join(format!(".arc-unpack-part-{}", self.n)));
        let dest = stage.clone().unwrap_or_else(|| out.to_path_buf());
        let eopts = self.options(dest, stage.is_none().then_some(out), self.opts.cancel.clone());
        std::fs::create_dir_all(&eopts.temp_dir)?;
        let s = extract::run(vec![a], &cfg, &eopts, self.sink)?;
        if let (Some(stage), Some(base)) = (&stage, base) {
            let part = stage.join(base.replace('/', "\\"));
            if part.is_dir() {
                move_tree(&part, out)?;
            }
            let _ = std::fs::remove_dir_all(stage);
        }
        let ok = s.problems.is_empty() && !s.cancelled;
        self.merge(s);
        Ok(ok)
    }

    /// ISRarExtract: unpacks a RAR archive with the installer's Unrar.dll, through the 32-bit
    /// helper (Unrar.dll is 32-bit, like unarc.dll).
    fn unrar(&mut self, archive: &Path, out: &Path, unrar: &Path, password: &str) -> Result<bool> {
        if !unrar.is_file() {
            events::warn(self.sink, format!("{}: the installer has no Unrar.dll", archive.display()));
            return Ok(false);
        }
        let host = crate::stage::helper_exe(&["unarc-host-x86.exe"], &self.opts.host_dir)?;
        std::fs::create_dir_all(out)?;
        // (A trailing separator: Unrar.dll treats the destination as a folder.)
        let dest = format!("{}\\", out.display().to_string().trim_end_matches(['\\', '/']));
        let mut cmd = std::process::Command::new(&host);
        cmd.arg("--rar").arg(unrar).arg(archive).arg(&dest);
        if !password.is_empty() {
            cmd.arg(password);
        }
        cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
        crate::stage::apply_child_flags(&mut cmd, self.opts.tune);
        let child = cmd.spawn().with_context(|| format!("start {}", host.display()))?;
        let output = child.wait_with_output()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let files = stdout.lines().filter(|l| l.starts_with("file ")).count();
        if !output.status.success() {
            let why = String::from_utf8_lossy(&output.stderr).trim().to_string();
            events::warn(self.sink, format!("unrar {}: {why}", archive.display()));
            return Ok(false);
        }
        let name = archive.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        events::info(self.sink, format!("{name} (RAR): {files} files"));
        Ok(true)
    }

    /// IS7zipExtract: unpacks a 7z archive the way ISDone does (src/sevenz.rs). A failure is
    /// the call returning FALSE; the script decides what happens then.
    fn seven_zip(&mut self, archive: &Path, out: &Path, password: &str) -> bool {
        let name = archive.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let started = std::time::Instant::now();
        match crate::sevenz::extract(archive, out, password, &self.opts.cancel) {
            Ok(r) => {
                let mb = r.bytes as f64 / 1e6;
                events::info(self.sink, format!("{name} (7z): {} files, {mb:.1} MB in {:.0}s", r.files, started.elapsed().as_secs_f64()));
                self.summary.problems.extend(r.problems);
                true
            }
            Err(e) => {
                events::warn(self.sink, format!("{name}: {e:#}"));
                false
            }
        }
    }

    fn exec(&mut self, program: &Path, args: &str, dir: &Path, wait: bool) -> Result<bool> {
        if !wait {
            // Started once the archives up to the next other step are fully unpacked: such a
            // program waits for a file an archive unpacks and then works on the files before
            // it (007 First Light's w01.bat: chunk0_t.fgpack, then chunk0_b.fgpack). FreeArc
            // writes them in order; the parallel extraction here doesn't.
            self.deferred.push((program.to_path_buf(), args.to_string(), dir.to_path_buf()));
            return Ok(true);
        }
        // Archives that only waited for this step to settle their codec config start now, and
        // unpack while it runs (FitGirl's buildNN.bat files never touch it). Before it starts,
        // so they see only files earlier steps finished.
        let (now, after) = self.config_changes(program, args, dir);
        self.count_changes(now);
        self.start_prefetches();
        let mut child = self.spawn(program, args, dir)?;
        // CPU time of the program and everything it starts (cmd.exe -> run.exe -> 32 tools):
        // shows whether a step is CPU-bound, single-threaded or waiting.
        let cpu = crate::perf::CpuJob::track(&child);
        let started = std::time::Instant::now();
        let mut stall = crate::guard::StallCheck::new(self.opts.stall_timeout);
        let mut next_look = started;
        let status = loop {
            if let Some(st) = child.try_wait()? {
                break st;
            }
            if std::time::Instant::now() >= next_look {
                next_look = std::time::Instant::now() + std::time::Duration::from_millis(300);
                self.look_after_children();
            }
            if self.opts.cancel.load(Ordering::Relaxed) {
                let _ = child.kill();
                bail!("cancelled");
            }
            if stall.stalled() {
                // Named before stopping it: what still runs is the best hint at why.
                let running = crate::guard::running_programs().join(", ");
                let _ = child.kill();
                let name = program.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                bail!(
                    "{name}{args} made no progress for {} s (nothing read or written; still running: {running}): stopped it",
                    stall.limit_secs()
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        self.count_changes(after);
        self.report_exit(program, args, status, cpu.as_ref(), started, None);
        // Like ISDone: a program that ran counts as done; the final MD5 check catches failures.
        Ok(true)
    }

    /// While programs run: the tools they start (through cmd.exe or the repack's own runner)
    /// get the codecs' tuning (above-normal priority, no power throttling, which would put a
    /// single-threaded tool on the slow cores), and error dialogs are closed. As during
    /// extraction.
    fn look_after_children(&mut self) {
        if self.opts.tune && std::env::var_os("ARC_UNPACK_NO_DESCENDANT_TUNE").is_none() {
            crate::guard::tune_descendants(&mut self.tuned);
        }
        for m in crate::guard::dismiss_codec_dialogs() {
            let note = format!("error dialog (closed automatically): {m}");
            events::warn(self.sink, note.clone());
            self.summary.notes.push(note);
        }
    }

    /// Logs how a step's program did: its CPU use, and an exit code that means trouble.
    fn report_exit(
        &mut self,
        program: &Path,
        args: &str,
        status: std::process::ExitStatus,
        cpu: Option<&crate::perf::CpuJob>,
        started: std::time::Instant,
        step: Option<usize>,
    ) {
        let name = program.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let wall = started.elapsed().as_secs_f64();
        if let Some(n) = step {
            // (The step's own line came when it started.)
            events::info(self.sink, format!("step {n} finished ({wall:.1}s)"));
        }
        if let Some(cpu) = cpu.and_then(|j| j.cpu_seconds())
            && wall >= 1.0
        {
            events::info(self.sink, format!("    {name}: {cpu:.0}s cpu / {wall:.0}s wall ({:.1} cores)", cpu / wall));
        }
        if !status.success() && !benign_exit(program, status.code().unwrap_or(-1)) {
            let note = format!("{name}{args}: exited with code {}", status.code().unwrap_or(-1));
            events::warn(self.sink, note.clone());
            self.summary.notes.push(note);
        }
    }

    /// The files and folders a waiting Exec step touches, if it may run beside the next steps:
    /// `cmd /C` with a chain of `&&` commands, each one a tool that touches only the files its
    /// command line names (`tools::named_files_tool`) or a plain del/move/copy/ren/rd/md,
    /// with no wildcards, redirection, pipes or variables, and nothing that can change a codec
    /// config. ISExec and Exec report success once a program has run, whatever its exit code
    /// (arc-unpack's Exec gives ResultCode 0), so the script can't tell whether it waited:
    /// running such steps side by side only changes when they finish. FS17 runs 217 raw2hif
    /// steps in a row, one core each. Every other step runs alone, after those in flight.
    fn overlap_paths(&mut self, program: &Path, args: &str, dir: &Path) -> Option<Vec<String>> {
        if std::env::var_os("ARC_UNPACK_NO_OVERLAP").is_some() || !program.file_name().is_some_and(|n| n.eq_ignore_ascii_case("cmd.exe")) {
            return None;
        }
        let rest = args.trim_start();
        let line = rest.strip_prefix("/C ").or_else(|| rest.strip_prefix("/c "))?.trim();
        if line.is_empty() || line.contains(['|', '<', '>', '%', '^', '(', ')', '*', '?', '!']) {
            return None;
        }
        let (now, after) = self.config_changes(program, args, dir);
        if !now.is_empty() || !after.is_empty() {
            return None;
        }
        let map = self.standins.as_ref().map(|s| s.1.clone());
        let mut paths = Vec::new();
        for seg in split_and_and(line)? {
            let words = cmd_words(seg);
            let (cmd, rest) = words.split_first()?;
            const BUILTINS: [&str; 10] = ["del", "erase", "move", "copy", "ren", "rename", "rd", "rmdir", "md", "mkdir"];
            let mut skip = 0;
            if !BUILTINS.contains(&cmd.to_ascii_lowercase().as_str()) {
                let tool = resolve_in(dir, cmd);
                let known = match self.safe_tools.get(&tool) {
                    Some(&k) => k,
                    None => {
                        let k = crate::tools::named_files_tool(&tool, map.as_deref());
                        self.safe_tools.insert(tool.clone(), k);
                        k
                    }
                };
                skip = known?;
                // (Another step deleting or replacing the tool meanwhile would meet it here.)
                paths.push(path_key(&tool)?);
            }
            // Every other word that isn't a switch counts as a file or folder: a folder can be
            // named "04" (Sleeping Dogs' `rd /q /s 04`).
            for w in rest.iter().skip(skip) {
                let switch = w.starts_with('-') || (w.starts_with('/') && w.len() <= 3);
                if switch || w.eq_ignore_ascii_case("nul") {
                    continue;
                }
                for part in w.split('+').filter(|p| !p.is_empty()) {
                    paths.push(path_key(&resolve_in(dir, part))?);
                }
            }
        }
        Some(paths)
    }

    /// Starts a step that runs beside the next ones, once no step in flight shares a file or
    /// folder with it and fewer than one per CPU are running.
    fn start_overlapped(&mut self, program: &Path, args: &str, dir: &Path, paths: Vec<String>) -> Result<()> {
        let limit = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        self.wait_overlapped(|r| r.overlapped.len() < limit && r.overlapped.iter().all(|o| !paths_overlap(&o.paths, &paths)))?;
        let child = self.spawn(program, args, dir)?;
        let cpu = crate::perf::CpuJob::track(&child);
        self.overlapped.push(Overlapped {
            n: self.n,
            program: program.to_path_buf(),
            args: args.to_string(),
            child,
            cpu,
            started: std::time::Instant::now(),
            paths,
        });
        Ok(())
    }

    /// Waits for every step in flight.
    fn join_overlapped(&mut self) -> Result<()> {
        self.wait_overlapped(|r| r.overlapped.is_empty())
    }

    /// Reaps (and reports) finished steps in flight until `done` holds, looking after the running
    /// ones as `exec` does: tuning, error dialogs, cancel, and the stall check.
    fn wait_overlapped(&mut self, done: impl Fn(&Self) -> bool) -> Result<()> {
        let mut stall = crate::guard::StallCheck::new(self.opts.stall_timeout);
        let mut next_look = std::time::Instant::now();
        loop {
            let mut i = 0;
            while i < self.overlapped.len() {
                match self.overlapped[i].child.try_wait()? {
                    Some(status) => {
                        let o = self.overlapped.remove(i);
                        self.report_exit(&o.program, &o.args, status, o.cpu.as_ref(), o.started, Some(o.n));
                    }
                    None => i += 1,
                }
            }
            if done(self) {
                return Ok(());
            }
            if std::time::Instant::now() >= next_look {
                next_look = std::time::Instant::now() + std::time::Duration::from_millis(300);
                self.look_after_children();
            }
            if self.opts.cancel.load(Ordering::Relaxed) {
                self.kill_overlapped();
                bail!("cancelled");
            }
            if stall.stalled() {
                let running = crate::guard::running_programs().join(", ");
                let steps: Vec<String> = self.overlapped.iter().map(|o| format!("step {}", o.n)).collect();
                self.kill_overlapped();
                bail!("{} made no progress for {} s (nothing read or written; still running: {running}): stopped them", steps.join(", "), stall.limit_secs());
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn kill_overlapped(&mut self) {
        for mut o in std::mem::take(&mut self.overlapped) {
            let _ = o.child.kill();
            let _ = o.child.wait();
        }
    }

    /// Puts stand-ins in place of the known tools in the temporary folder (see src/tools.rs):
    /// before each program the script runs, as its archives may have unpacked new tools.
    fn substitute_tools(&mut self) {
        let Some((program, map, originals)) = &self.standins else { return };
        match crate::tools::substitute(&self.tmp, program, originals, map) {
            Ok(done) => {
                for d in done {
                    events::info(self.sink, format!("using arc-unpack's own version of {d}"));
                }
            }
            Err(e) => events::warn(self.sink, format!("stand-in for a repack tool: {e:#}")),
        }
    }

    /// What the stand-ins did: how often they did the work themselves, and (with
    /// ARC_UNPACK_TOOL_VERIFY=1) how their output compared with the original tools'.
    fn report_standins(&mut self) {
        let Some((_, map, _)) = &self.standins else { return };
        let log = std::fs::read_to_string(map.with_extension("log")).unwrap_or_default();
        let mut counts: BTreeMap<String, (usize, f64)> = BTreeMap::new();
        for l in log.lines() {
            let mut w = l.split_whitespace();
            if let (Some(role), Some("native"), Some(secs)) = (w.next(), w.next(), w.next().and_then(|x| x.parse::<f64>().ok())) {
                let c = counts.entry(role.to_string()).or_default();
                c.0 += 1;
                c.1 += secs;
            }
            if l.contains(" failed (") {
                events::warn(self.sink, format!("stand-in: {l}"));
            }
        }
        for (role, (n, secs)) in counts {
            events::info(self.sink, format!("arc-unpack's own {role}: {n} runs, {secs:.0} s in all"));
        }
        let verify = std::fs::read_to_string(map.with_extension("verify.log")).unwrap_or_default();
        let (mut same, mut different) = (0, 0);
        // One result per line normally; count every result on a line anyway (lines written by
        // several processes at once could merge).
        const VERDICTS: [&str; 5] = [": same (", ": DIFFERENT", ": ours FAILED", ": original wrote nothing", ": ours wrote nothing"];
        for l in verify.lines().filter(|l| !l.trim().is_empty()) {
            let results = VERDICTS.iter().map(|v| l.matches(v).count()).sum::<usize>().max(1);
            let ok = l.matches(VERDICTS[0]).count();
            same += ok;
            if ok < results {
                different += results - ok;
                events::warn(self.sink, format!("tool check: {l}"));
                self.summary.problems.push(format!("a stand-in's output differs from the original tool's: {l}"));
            }
        }
        if same + different > 0 {
            events::info(self.sink, format!("tool check: {same} of {} outputs identical to the original tools'", same + different));
        }
    }

    /// Starts the programs the script started without waiting (see `exec`).
    fn start_deferred(&mut self) -> Result<()> {
        if !self.deferred.is_empty() {
            self.substitute_tools();
        }
        for (program, args, dir) in std::mem::take(&mut self.deferred) {
            // (A program that runs on its own while the script goes on counts at once.)
            let (now, after) = self.config_changes(&program, &args, &dir);
            self.count_changes(now);
            self.count_changes(after);
            let child = self.spawn(&program, &args, &dir)?;
            let name = program.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            self.background.push((name, child));
        }
        Ok(())
    }

    fn spawn(&mut self, program: &Path, args: &str, dir: &Path) -> Result<std::process::Child> {
        let mut cmd;
        let is_batch = program.extension().is_some_and(|e| e.eq_ignore_ascii_case("bat") || e.eq_ignore_ascii_case("cmd"));
        if is_batch {
            // As Inno's Exec does: batch files run through cmd.exe.
            cmd = std::process::Command::new(std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".into()));
            raw_arg(&mut cmd, &format!("/C \"\"{}\"{args}\"", program.display()));
        } else {
            cmd = std::process::Command::new(program);
            raw_arg(&mut cmd, args.trim_start());
        }
        cmd.current_dir(dir).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        if let Some((_, map, _)) = &self.standins {
            cmd.env(crate::tools::MAP_ENV, map);
            if let Some(cli) = cli_exe() {
                cmd.env(crate::tools::SELF_ENV, cli);
            }
        }
        crate::stage::apply_child_flags(&mut cmd, self.opts.tune);
        cmd.spawn().with_context(|| format!("start {}", program.display()))
    }

    /// The codec configs (of early extractions) a step may change, split into those it can't
    /// really touch (counted as changed at once, so archives waiting for it can start while it
    /// runs) and those it may (counted once it has finished). Batch files are read to decide
    /// (`batch_touches`); any other program touches a config its command line names.
    fn config_changes(&self, program: &Path, args: &str, dir: &Path) -> (Vec<String>, Vec<String>) {
        let names: HashSet<String> = self.prefetch.iter().map(|p| p.cfg_name.clone()).collect();
        let (mut now, mut after) = (Vec::new(), Vec::new());
        for n in names {
            if !exec_touches(program, args, &n) {
                continue;
            }
            if is_batch(program) && !batch_touches(program, dir, &n, &self.tmp) {
                now.push(n);
            } else {
                after.push(n);
            }
        }
        (now, after)
    }

    fn count_changes(&mut self, names: Vec<String>) {
        for n in names {
            *self.changes.entry(n).or_default() += 1;
        }
    }
}

impl crate::setup_script::Runner for ScriptRunner<'_> {
    fn run(&mut self, step: &crate::setup_script::Step) -> Result<bool> {
        use crate::setup_script::Step;
        ensure!(!self.opts.cancel.load(Ordering::Relaxed), "cancelled");
        self.start_prefetches();
        self.n += 1;
        let total = self.steps_total.max(self.n);
        (self.sink)(Event::Step { index: self.n, total, label: step_label(step) });
        if !matches!(step, Step::Extract { .. }) {
            self.start_deferred()?;
        }
        if matches!(step, Step::Exec { .. }) {
            self.substitute_tools();
        }
        if let Step::Exec { program, args, dir, wait: true } = step
            && let Some(paths) = self.overlap_paths(program, args, dir)
        {
            self.start_overlapped(program, args, dir, paths)?;
            events::info(self.sink, format!("step {}: {step} (runs beside the next steps)", self.n));
            self.start_prefetches();
            return Ok(true);
        }
        // Everything else runs alone, after the tool steps still running.
        self.join_overlapped()?;
        let started = std::time::Instant::now();
        let ok = match step {
            Step::Extract { archive, out, cfg, base } => self.extract(archive, out, cfg, base.as_deref())?,
            Step::Exec { program, args, dir, wait } => self.exec(program, args, dir, *wait)?,
            Step::Rar { archive, out, unrar, password } => self.unrar(archive, out, unrar, password)?,
            Step::SevenZip { archive, out, password } => self.seven_zip(archive, out, password),
        };
        let secs = started.elapsed().as_secs_f64();
        events::info(self.sink, format!("step {}: {step} ({secs:.1}s){}", self.n, if ok { "" } else { " FAILED" }));
        self.start_prefetches();
        Ok(ok)
    }

    fn file_written(&mut self, path: &Path) {
        if let Some(name) = path.file_name() {
            *self.changes.entry(name.to_string_lossy().to_lowercase()).or_default() += 1;
        }
    }

    fn barrier(&mut self) -> Result<()> {
        self.join_overlapped()
    }
}

/// A step in plain words, for the window.
fn step_label(step: &crate::setup_script::Step) -> String {
    use crate::setup_script::Step;
    let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match step {
        Step::Extract { archive, .. } | Step::Rar { archive, .. } | Step::SevenZip { archive, .. } => {
            format!("Unpacking {}", name(archive))
        }
        Step::Exec { program, args, .. } => {
            // `cmd /C tool.exe ...`: name the tool, not cmd.
            let tool = args
                .split(|c: char| c.is_whitespace() || c == '"')
                .find(|t| {
                    let t = t.to_ascii_lowercase();
                    t.ends_with(".exe") || t.ends_with(".bat") || t.ends_with(".cmd")
                })
                .map(|t| name(Path::new(t)))
                .filter(|_| name(program).eq_ignore_ascii_case("cmd.exe"))
                .unwrap_or_else(|| name(program));
            if tool.eq_ignore_ascii_case("cmd.exe") {
                "Finishing up files".to_string()
            } else {
                format!("Running the repack's tools ({tool})")
            }
        }
    }
}

#[cfg(windows)]
fn raw_arg(cmd: &mut std::process::Command, args: &str) {
    use std::os::windows::process::CommandExt;
    if !args.is_empty() {
        cmd.raw_arg(args);
    }
}

#[cfg(not(windows))]
fn raw_arg(cmd: &mut std::process::Command, args: &str) {
    cmd.args(args.split_whitespace());
}

/// Moves every file under `from` into `to` (same volume: renames), replacing existing ones.
fn move_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let dst = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            move_tree(&e.path(), &dst)?;
        } else {
            std::fs::rename(e.path(), &dst).with_context(|| format!("move {} to {}", e.path().display(), dst.display()))?;
        }
    }
    Ok(())
}

/// The repack's list of installed-file MD5s shipped inside setup.exe (FitGirl:
/// `{app}\_Redist\fitgirl.md5`, lines like `<md5> *..\Game\file.dat` relative to the list's
/// own folder). With a selection, only the lists of selected components: a repack with a
/// language choice ships one list per language, all written to the same file.
fn md5_list_from_setup(inno: &crate::inno::Setup, selected: Option<&std::collections::BTreeSet<String>>) -> Result<Option<Md5List>> {
    let mut list = Md5List::new();
    for f in inno.files.iter().filter(|f| f.has_data() && english_or_neutral(&f.languages)) {
        let Some(rel) = f.app_relative() else { continue };
        if let Some(sel) = selected
            && !f.components.is_empty()
            && !crate::setup_script::eval_components(&f.components, sel).unwrap_or(true)
        {
            continue;
        }
        // FitGirl ships optional parts' entries as `*.addon` next to the list (the installer
        // appends them when the part is installed).
        let lower = rel.to_ascii_lowercase();
        if !lower.ends_with(".md5") && !lower.ends_with(".addon") {
            continue;
        }
        let mut base: Vec<&str> = rel.split(['\\', '/']).filter(|p| !p.is_empty()).collect();
        base.pop();
        parse_installed_md5(&md5_list_text(&inno.read(f)?), &base, &mut list);
    }
    Ok((!list.is_empty()).then_some(list))
}

/// The text of an MD5 list. FitGirl's lists are made on a Russian Windows: names that aren't
/// plain ASCII are in its ANSI code page, 1251, not UTF-8 (RimWorld's "Russian (Русский).tar").
/// Lines that are valid UTF-8 are taken as they are, the others are read as code page 1251.
pub(crate) fn md5_list_text(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_owned();
    }
    bytes
        .split_inclusive(|&b| b == b'\n')
        .map(|line| std::str::from_utf8(line).map(str::to_owned).unwrap_or_else(|_| cp1251(line)))
        .collect()
}

fn cp1251(bytes: &[u8]) -> String {
    use windows_sys::Win32::Globalization::MultiByteToWideChar;
    let Ok(len) = i32::try_from(bytes.len()) else { return String::from_utf8_lossy(bytes).into_owned() };
    // SAFETY: `bytes` is `len` bytes long; the second call writes at most `n` units into `wide`.
    let n = unsafe { MultiByteToWideChar(1251, 0, bytes.as_ptr(), len, std::ptr::null_mut(), 0) };
    if n <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut wide = vec![0u16; n as usize];
    let n = unsafe { MultiByteToWideChar(1251, 0, bytes.as_ptr(), len, wide.as_mut_ptr(), n) };
    String::from_utf16_lossy(&wide[..n.max(0) as usize])
}

fn parse_installed_md5(text: &str, base: &[&str], list: &mut Md5List) {
    for line in text.lines().map(str::trim) {
        if line.starts_with(';') {
            continue;
        }
        let (Some(hash), Some(rest)) = (line.get(..32), line.get(32..)) else { continue };
        let Some(md5) = parse_hex16(hash) else { continue };
        let mut parts: Vec<&str> = base.to_vec();
        let mut inside = true;
        for p in rest.trim().trim_start_matches('*').trim().split(['\\', '/']) {
            match p {
                "" | "." => {}
                ".." => inside &= parts.pop().is_some(),
                p => parts.push(p),
            }
        }
        if inside && !parts.is_empty() && parts.iter().all(|p| !p.contains(':')) {
            let path = parts.join("\\");
            list.insert(extract::md5_key(&path), ExpectedMd5::new(md5, path));
        }
    }
}

/// The checksum lines of an MD5 list, normalised (lower case, trimmed), for comparing lists.
fn hash_lines(text: &str) -> HashSet<String> {
    text.lines()
        .map(str::trim)
        .filter_map(|l| {
            let (hash, rest) = (l.get(..32)?, l.get(32..)?);
            parse_hex16(hash)?;
            Some(format!("{} {}", hash.to_ascii_lowercase(), rest.trim().to_ascii_lowercase()))
        })
        .collect()
}

/// The component names in an Inno component expression ("bonus", "voice\french or text").
fn component_words(expr: &str) -> impl Iterator<Item = String> + '_ {
    expr.split(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .map(str::to_ascii_lowercase)
        .filter(|w| !w.is_empty() && !matches!(w.as_str(), "and" | "or" | "not"))
}

/// A game folder with this repack's game already installed.
#[derive(Debug, Clone)]
pub struct InstalledGame {
    /// Files the game's own list names (all present).
    pub files: usize,
    /// Optional components whose file lists are already merged into the game's list.
    pub parts: std::collections::BTreeSet<String>,
    /// Whether the game folder still has its file list (`_Redist\fitgirl.md5`).
    pub list: bool,
}

/// Checks that `target` has this repack's game installed, by its own installer or by
/// arc-unpack (both leave the repack's file list, `_Redist\fitgirl.md5`): the list is there,
/// matches this repack's, and every file it names exists. Adding optional parts needs that.
pub fn find_installed(inno: &crate::inno::Setup, target: &Path) -> Result<InstalledGame> {
    let is_list = |f: &&crate::inno::SetupFile| {
        f.has_data() && english_or_neutral(&f.languages) && f.app_relative().is_some_and(|r| r.to_ascii_lowercase().ends_with(".md5"))
    };
    let lists: Vec<&crate::inno::SetupFile> = inno.files.iter().filter(is_list).collect();
    ensure!(!lists.is_empty(), "this repack has no file list, so an installed copy of its game can't be recognised");
    let (mut files, mut have, mut list_found) = (0, HashSet::new(), true);
    for f in lists {
        let rel = f.app_relative().unwrap_or_default();
        let ours = md5_list_text(&inno.read(f)?);
        // The game's own copy of the list; people often delete `_Redist` after installing.
        let on_disk = std::fs::read(target.join(crate::inno::safe_dest(rel)?)).ok().map(|b| hash_lines(&md5_list_text(&b)));
        if let Some(on_disk) = &on_disk {
            ensure!(
                hash_lines(&ours).is_subset(on_disk),
                "{} has a different version of this game: its file list doesn't match this repack's",
                target.display()
            );
        }
        let mut base: Vec<&str> = rel.split(['\\', '/']).filter(|p| !p.is_empty()).collect();
        base.pop();
        let mut list = Md5List::new();
        parse_installed_md5(&ours, &base, &mut list);
        let mut missing: Vec<&str> = list.values().filter(|e| !target.join(&e.path).is_file()).map(|e| e.path.as_str()).collect();
        missing.sort();
        ensure!(missing.len() < list.len(), "the game isn't installed in {}", target.display());
        ensure!(
            missing.is_empty(),
            "{} of the game's {} files are missing from {} (e.g. {}): install the base game first",
            missing.len(),
            list.len(),
            target.display(),
            missing.first().unwrap_or(&"")
        );
        if on_disk.is_none() {
            // No list to compare: check a few of the game's smallest files instead (quick), so a
            // different version of the game isn't mistaken for this one.
            let mut sized: Vec<(u64, &ExpectedMd5)> =
                list.values().map(|e| (std::fs::metadata(target.join(&e.path)).map(|m| m.len()).unwrap_or(0), e)).collect();
            sized.sort_by_key(|(len, e)| (*len, e.path.clone()));
            for (_, e) in sized.iter().filter(|(len, _)| *len > 0).take(8) {
                let got = extract::md5_of_file(&target.join(&e.path))?;
                ensure!(
                    got == e.md5,
                    "{} has a different version of this game: {} doesn't match this repack's file list",
                    target.display(),
                    e.path
                );
            }
            list_found = false;
        }
        files += list.len();
        have.extend(on_disk.unwrap_or_default());
    }
    let mut parts = std::collections::BTreeSet::new();
    for f in inno.files.iter().filter(|f| f.has_data() && !f.components.is_empty()) {
        if f.app_relative().is_some_and(|r| r.to_ascii_lowercase().ends_with(".addon")) {
            let lines = hash_lines(&md5_list_text(&inno.read(f)?));
            if !lines.is_empty() && lines.is_subset(&have) {
                parts.extend(component_words(&f.components));
            }
        }
    }
    Ok(InstalledGame { files, parts, list: list_found })
}

/// Adding parts: FitGirl's installer merges each installed part's file list (`*.addon`) into
/// the game's (`copy /b fitgirl.md5+*.addon`) as its very last step, which belongs to the base
/// game's archives and is left out. The same merge, byte for byte (a list already merged is
/// left alone). Returns the merged lists' entries, to check the part's files against.
fn merge_part_lists(inno: &crate::inno::Setup, target: &Path, selected: &std::collections::BTreeSet<String>) -> Result<HashSet<String>> {
    let mut keys = HashSet::new();
    let dir_of = |rel: &str| rel.rsplit_once(['\\', '/']).map(|(d, _)| d.to_ascii_lowercase()).unwrap_or_default();
    for f in inno.files.iter().filter(|f| f.has_data() && english_or_neutral(&f.languages) && !f.components.is_empty()) {
        let Some(rel) = f.app_relative() else { continue };
        if !rel.to_ascii_lowercase().ends_with(".addon") || !crate::setup_script::eval_components(&f.components, selected).unwrap_or(false) {
            continue;
        }
        let addon = inno.read(f)?;
        let mut base: Vec<&str> = rel.split(['\\', '/']).filter(|p| !p.is_empty()).collect();
        base.pop();
        let mut list = Md5List::new();
        parse_installed_md5(&md5_list_text(&addon), &base, &mut list);
        keys.extend(list.into_keys());
        let game_list = inno.files.iter().filter_map(|g| g.app_relative()).find(|r| r.to_ascii_lowercase().ends_with(".md5") && dir_of(r) == dir_of(rel));
        // (No list when `_Redist` was deleted after installing: nothing to merge into.)
        if let Some(list_rel) = game_list
            && let Ok(mut text) = std::fs::read(target.join(crate::inno::safe_dest(list_rel)?))
        {
            let path = target.join(crate::inno::safe_dest(list_rel)?);
            if !hash_lines(&md5_list_text(&addon)).is_subset(&hash_lines(&md5_list_text(&text))) {
                text.extend_from_slice(&addon);
                std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
            }
        }
        let _ = std::fs::remove_file(target.join(crate::inno::safe_dest(rel)?));
    }
    Ok(keys)
}

/// Adding parts: game files an archive would replace are moved into `backup` first (same paths
/// relative to the game folder), so they can be put back by hand. Returns how many were moved.
fn back_up_replaced(archive: &Archive, out: &Path, base: Option<&str>, game: &Path, backup: &Path) -> Result<usize> {
    let mut moved = 0;
    let prefix = base.map(|b| format!("{}/", b.replace('\\', "/").trim_matches('/').to_ascii_lowercase()));
    for e in archive.entries.iter().filter(|e| !e.is_dir) {
        let rel = match &prefix {
            // Only this part of the archive is extracted, without the prefix.
            Some(p) if e.path.to_ascii_lowercase().starts_with(p.as_str()) => &e.path[p.len()..],
            Some(_) => continue,
            None => e.path.as_str(),
        };
        let dest = extract::safe_join(out, rel)?;
        if !dest.is_file() {
            continue;
        }
        let inside = dest.strip_prefix(game).map(Path::to_path_buf).unwrap_or_else(|_| PathBuf::from(rel));
        let to = backup.join(inside);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&dest, &to).with_context(|| format!("move {} to {}", dest.display(), to.display()))?;
        moved += 1;
    }
    Ok(moved)
}

fn parse_hex16(s: &str) -> Option<[u8; 16]> {
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

struct Md5Check {
    ok: usize,
    /// Of `ok`: hashed after extraction instead of while being written.
    after: usize,
    problems: Vec<String>,
    notes: Vec<String>,
}

/// Completes the MD5 check. Files whose hash matched while they were written only need to
/// still exist; everything else (written by unarc.dll, skipped by a resumed run, or
/// mismatched at write time and possibly replaced later) is hashed from disk now. Missing
/// files are problems, except, with `in_archives`, those no archive here contains (optional
/// parts not downloaded), which are only noted.
fn check_md5_list(
    entries: &[(&String, &ExpectedMd5)],
    target: &Path,
    in_archives: Option<&HashSet<String>>,
    cancel: &AtomicBool,
) -> Md5Check {
    let mut check = Md5Check { ok: 0, after: 0, problems: Vec::new(), notes: Vec::new() };
    let path_of = |x: &ExpectedMd5| target.join(x.path.replace('/', "\\"));
    let mut todo = Vec::new();
    let mut gone = Vec::new();
    for &(key, x) in entries {
        if x.checked.load(Ordering::Relaxed) && x.matched.load(Ordering::Relaxed) {
            if path_of(x).is_file() {
                check.ok += 1;
            } else {
                gone.push((key, x));
            }
        } else {
            todo.push((key, x));
        }
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8);
    let todo_ref = &todo;
    let mut results: Vec<(usize, Result<bool, String>)> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut out = Vec::new();
                    let mut buf = vec![0u8; 1 << 20];
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= todo_ref.len() || cancel.load(Ordering::Relaxed) {
                            break;
                        }
                        let x = todo_ref[i].1;
                        let r = (|| -> std::io::Result<bool> {
                            let mut f = std::fs::File::open(path_of(x))?;
                            let mut ctx = md5::Context::new();
                            loop {
                                let n = f.read(&mut buf)?;
                                if n == 0 {
                                    break;
                                }
                                ctx.consume(&buf[..n]);
                            }
                            Ok(ctx.finalize().0 == x.md5)
                        })();
                        out.push((i, r.map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { String::new() } else { e.to_string() })));
                    }
                    out
                })
            })
            .collect();
        workers.into_iter().flat_map(|w| w.join().unwrap_or_default()).collect()
    });
    let base = todo.len();
    todo.extend(gone);
    results.extend((base..todo.len()).map(|i| (i, Err(String::new()))));
    let (mut missing, mut absent, mut bad) = (Vec::new(), Vec::new(), Vec::new());
    for (i, r) in results {
        let (key, x) = todo[i];
        match r {
            Ok(true) => {
                check.ok += 1;
                check.after += 1;
            }
            Ok(false) => bad.push(format!("{} (MD5 mismatch with the repack's list)", x.path)),
            Err(e) if e.is_empty() && in_archives.is_none_or(|a| a.contains(key.as_str())) => missing.push(x.path.clone()),
            Err(e) if e.is_empty() => absent.push(x.path.clone()),
            Err(e) => bad.push(format!("{}: {e}", x.path)),
        }
    }
    let sample = |v: &[String]| v.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
    check.problems.extend(bad);
    if !missing.is_empty() {
        check.problems.push(format!("{} files from the archives are missing: {}", missing.len(), sample(&missing)));
    }
    if !absent.is_empty() {
        check.notes.push(format!(
            "{} files in the repack's MD5 list are in none of the archives here (made by the installer's own \
             post-processing, or optional parts not downloaded): {}",
            absent.len(),
            sample(&absent)
        ));
    }
    check
}

/// Whether an Inno `Languages:` expression includes English (or is empty = every language).
/// The silent default is the installer's first language; repacks list English as "en"/"english".
pub fn english_or_neutral(languages: &str) -> bool {
    let words: Vec<String> = languages
        .split(|c: char| c.is_whitespace() || c == ',' || c == '(' || c == ')')
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    words.is_empty() || words.iter().any(|w| w == "en" || w == "eng" || w.starts_with("english"))
}

/// Writes the files the installer itself puts in the game folder (`{app}\...`): launcher
/// stubs, `_Redist` tools, the MD5 list. Same choice as a default silent install: English or
/// language-neutral entries of every component (all archives get installed too). Entries that
/// depend on a task or a script check are skipped and reported. Files the archives already
/// produced are left alone (the installer writes these first, then the archives overwrite).
fn install_setup_files(
    inno: &crate::inno::Setup,
    target: &Path,
    sink: &Sink,
    selected: Option<&std::collections::BTreeSet<String>>,
    parts_only: bool,
) -> Result<usize> {
    let mut skipped = Vec::new();
    let written = inno.extract_with(|f| {
        let Some(rel) = f.app_relative() else { return Ok(None) };
        if !english_or_neutral(&f.languages) {
            return Ok(None);
        }
        // Adding parts: only the parts' own files. Their file lists (`*.addon`) are merged into
        // the game's list afterwards, and the base game's files are left as they are (also when
        // the player deleted some, such as `_Redist`).
        if parts_only && (f.components.is_empty() || rel.to_ascii_lowercase().ends_with(".addon")) {
            return Ok(None);
        }
        if let Some(sel) = selected
            && !f.components.is_empty()
            && !crate::setup_script::eval_components(&f.components, sel).unwrap_or(true)
        {
            return Ok(None);
        }
        if !f.tasks.is_empty() || !f.check.is_empty() {
            skipped.push(format!("{rel} (depends on {})", if f.check.is_empty() { &f.tasks } else { &f.check }));
            return Ok(None);
        }
        let path = target.join(crate::inno::safe_dest(rel)?);
        Ok((!path.exists()).then_some(path))
    })?;
    for s in skipped {
        events::warn(sink, format!("not installed from setup.exe: {s}"));
    }
    if !written.is_empty() {
        events::info(sink, format!("{} files from setup.exe written to the game folder", written.len()));
    }
    Ok(written.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_list_parsing() {
        let dir = std::env::temp_dir().join(format!("arcunp-md5-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.md5");
        std::fs::write(&p, "; comment\r\n0123456789abcdef0123456789ABCDEF *data-01.bin\r\nffffffffffffffffffffffffffffffff  sub\\data-02.bin\r\n").unwrap();
        let m = read_md5_list(&p);
        assert_eq!(m.get("data-01.bin").unwrap(), "0123456789abcdef0123456789abcdef");
        assert_eq!(m.get("data-02.bin").unwrap(), "ffffffffffffffffffffffffffffffff");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn md5_lists_in_code_page_1251() {
        // RimWorld's fitgirl.md5: ASCII lines, and Cyrillic names in code page 1251.
        let mut bytes = b"\xef\xbb\xbf0123456789abcdef0123456789abcdef *..\\Data\\Core\\About.xml\r\n".to_vec();
        bytes.extend_from_slice(b"2791a01a58fc95edd73f1812840669d2 *..\\Data\\Core\\Languages\\Russian (\xd0\xf3\xf1\xf1\xea\xe8\xe9).tar\r\n");
        bytes.extend_from_slice(b"7a1e8599571ee9838bf3be2f41273f86 *..\\Data\\Biotech\\Languages\\Ukrainian (\xd3\xea\xf0\xe0\xbf\xed\xf1\xfc\xea\xe0).tar\r\n");
        let text = md5_list_text(&bytes);
        assert!(text.starts_with("0123456789abcdef"));
        assert!(text.contains(r"Languages\Russian (Русский).tar"));
        assert!(text.contains(r"Languages\Ukrainian (Українська).tar"));
        let mut list = Md5List::new();
        parse_installed_md5(&text, &["_Redist"], &mut list);
        assert!(list.contains_key(&extract::md5_key(r"Data\Biotech\Languages\Ukrainian (Українська).tar")));
        // Cities: Skylines: a dash in code page 1251 (0x96).
        let dash = md5_list_text(b"00000000000000000000000000000000 *..\\Files\\Radio\\Music\\GoldFM\\Etta James \x96 At Last.ogg\n");
        assert!(dash.ends_with("Etta James – At Last.ogg\n"));
        // UTF-8 lists are unchanged.
        assert_eq!(md5_list_text("a *Español.txt\n".as_bytes()), "a *Español.txt\n");
    }

    #[test]
    fn file_lists_compare_by_line() {
        let a = hash_lines("; header\r\n0123456789ABCDEF0123456789ABCDEF *..\\Game\\A.dat\r\nnot a line\r\n");
        let b = hash_lines("0123456789abcdef0123456789abcdef *..\\game\\a.dat\nffffffffffffffffffffffffffffffff *..\\x\n");
        assert_eq!(a.len(), 1);
        assert!(a.is_subset(&b));
        assert!(!b.is_subset(&a));
    }

    #[test]
    fn replaced_files_are_backed_up() {
        use crate::format::Entry;
        let root = std::env::temp_dir().join(format!("arcunp-backup-{}", std::process::id()));
        let (game, backup) = (root.join("game"), root.join("game").join(BACKUP_DIR).join("fg-selective-french"));
        std::fs::create_dir_all(game.join("Data")).unwrap();
        std::fs::write(game.join("Data").join("voice.pck"), b"english").unwrap();
        std::fs::write(game.join("keep.txt"), b"untouched").unwrap();
        let entry = |p: &str, is_dir: bool| Entry { path: p.into(), size: 1, mtime: 0, is_dir, crc: 0 };
        let archive = |entries| Archive { path: PathBuf::new(), len: 0, blocks: Vec::new(), entries, checksum: crate::format::Checksum::Standard };
        // Only files the archive holds and the game already has are moved, keeping their paths.
        let a = archive(vec![entry("Data", true), entry("Data/voice.pck", false), entry("new.txt", false)]);
        assert_eq!(back_up_replaced(&a, &game, None, &game, &backup).unwrap(), 1);
        assert!(!game.join("Data").join("voice.pck").exists());
        assert_eq!(std::fs::read(backup.join("Data").join("voice.pck")).unwrap(), b"english");
        assert!(game.join("keep.txt").is_file());
        // A part of an archive (ExtractedPath): its prefix is dropped, other paths are ignored.
        let a = archive(vec![entry("Sub/keep.txt", false), entry("Other/keep.txt", false)]);
        assert_eq!(back_up_replaced(&a, &game, Some("Sub"), &game, &backup).unwrap(), 1);
        assert_eq!(std::fs::read(backup.join("keep.txt")).unwrap(), b"untouched");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn installed_md5_list_paths() {
        let mut list = Md5List::new();
        let text = "; header\r\n\
            0123456789abcdef0123456789ABCDEF *..\\Game_Data\\level0\r\n\
            ffffffffffffffffffffffffffffffff *local.txt\r\n\
            00000000000000000000000000000000 *..\\..\\outside.txt\r\n\
            nothex0123456789abcdef0123456789 *..\\x\r\n";
        parse_installed_md5(text, &["_Redist"], &mut list);
        assert_eq!(list.len(), 2);
        let e = &list[&extract::md5_key("game_data/LEVEL0")];
        assert_eq!(e.path, r"Game_Data\level0");
        assert_eq!(extract::hex(&e.md5), "0123456789abcdef0123456789abcdef");
        assert!(list.contains_key(&extract::md5_key(r"_Redist\local.txt")));
    }

    #[test]
    fn md5_check_after_extraction() {
        let dir = std::env::temp_dir().join(format!("arcunp-md5chk-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub").join("a.txt"), b"hello").unwrap();
        std::fs::write(dir.join("b.txt"), b"tampered").unwrap();
        let mut list = Md5List::new();
        for (p, content) in [(r"sub\a.txt", &b"hello"[..]), ("b.txt", b"original"), ("gone.txt", b"x"), ("optional.txt", b"y")] {
            list.insert(extract::md5_key(p), ExpectedMd5::new(md5::compute(content).0, p.to_string()));
        }
        let in_archives: HashSet<String> = ["sub/a.txt", "b.txt", "gone.txt"].iter().map(|p| extract::md5_key(p)).collect();
        let entries: Vec<(&String, &ExpectedMd5)> = list.iter().collect();
        let c = check_md5_list(&entries, &dir, Some(&in_archives), &AtomicBool::new(false));
        assert_eq!((c.ok, c.after), (1, 1));
        assert_eq!(c.problems.len(), 2, "{:?}", c.problems);
        assert!(c.problems.iter().any(|p| p.starts_with("b.txt (MD5 mismatch")));
        assert!(c.problems.iter().any(|p| p.contains("gone.txt")));
        assert_eq!(c.notes.len(), 1);
        assert!(c.notes[0].contains("optional.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn setup_file_languages() {
        assert!(english_or_neutral(""));
        assert!(english_or_neutral("en"));
        assert!(english_or_neutral("russian or english"));
        assert!(!english_or_neutral("russian"));
        assert!(!english_or_neutral("de"));
    }

    #[test]
    fn tool_paths_from_config() {
        let cfg: Config = toml::from_str(
            "[methods.xt]\ncmd = 'xtool3\\xtool.exe decode -t100p - -'\n[methods.p]\ncmd = 'precomp.exe -r'\n",
        )
        .unwrap();
        let t = referenced_tool_paths(&cfg);
        assert!(t.contains("xtool3/xtool.exe"));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn benign_exit_codes() {
        let dir = tempfile::tempdir().unwrap();
        let fart = dir.path().join("fart.exe");
        assert!(benign_exit(&fart, 8), "fart: 8 replacements");
        assert!(benign_exit(&fart, 0));
        assert!(!benign_exit(&fart, -2), "fart: an error");
        // A bat that deleted itself; one that is still there failed for real.
        let gone = dir.path().join("build01.bat");
        assert!(benign_exit(&gone, 1));
        assert!(!benign_exit(&gone, 2));
        let kept = dir.path().join("build02.bat");
        std::fs::write(&kept, "@echo off").unwrap();
        assert!(!benign_exit(&kept, 1));
        assert!(!benign_exit(&dir.path().join("x5.exe"), 2));
    }

    #[test]
    fn fitgirl_batch_files_dont_touch_the_codec_config() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("Run").join("tmp");
        let work = tmp.join("work");
        let app = dir.path().join("Game");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&app).unwrap();
        let t = tmp.display().to_string();
        // Shapes of the saved bats and lists, after FitGirl's fart.exe put the real {tmp} in.
        let bat = work.join("build01.bat");
        std::fs::write(
            &bat,
            format!("\"{t}\\work\\run.exe\" \" \" http://bit.ly/fitgirl-repacks-new-list \"{t}\\work\\fitgirl01.txt\" 0\r\ndel \"{t}\\work\\fitgirl01.txt\"\r\ndel \"{t}\\work\\build01.bat\""),
        )
        .unwrap();
        let list = work.join("fitgirl01.txt");
        // Cricket 19, and The Sims 3 (whose files are named like 160-1-ini.ini).
        std::fs::write(
            &list,
            format!(
                "cmd /c \"\"{t}\\work\\fgpack.exe\" e 22.fgpack 22.fgpack_ >nul 2>&1&&del 22.fgpack&&\"{t}\\work\\x5.exe\" 22.fgpack_ 22.fgpack.x5 22.fgpack\"\r\ncmd /c \"\"{t}\\work\\fgpack.exe\" c 160-1-ini.ini 160-1-ini.ini_ \"c1\" nul nul&&move 160-1-ini.ini_ 160-1-ini.ini\"\r\n"
            ),
        )
        .unwrap();
        assert!(!batch_touches(&bat, &app, "arc.ini", &tmp));
        assert!(!batch_touches(&bat, &app, "ini.ini", &tmp), "names are whole words");
        assert!(batch_touches(&bat, &app, "160-1-ini.ini", &tmp), "a name in the list");
        // Run in {tmp} itself: its relative names could be anything there.
        assert!(batch_touches(&bat, &tmp, "arc.ini", &tmp));
        assert!(!batch_touches(&bat, &work, "arc.ini", &tmp));

        let touching = |body: String| {
            let b = work.join("build02.bat");
            std::fs::write(&b, body).unwrap();
            batch_touches(&b, &app, "arc.ini", &tmp)
        };
        assert!(touching(format!("copy x \"{t}\\arc.ini\"")), "the config itself");
        assert!(touching("copy x arc.ini".into()), "the config by name");
        assert!(!touching("copy x xarc.ini".into()), "another name that ends like it");
        assert!(touching(format!("del \"{t}\\cls-srep.dll\"")), "a codec in {{tmp}}");
        assert!(touching("call other.bat".into()), "another batch file");
        assert!(touching(format!("\"{t}\\work\\run.exe\" \" \" url \"{t}\\work\\missing.txt\" 0")), "an unreadable list");
        assert!(batch_touches(&work.join("none.bat"), &app, "arc.ini", &tmp), "an unreadable bat");
    }

    #[test]
    fn command_words() {
        assert_eq!(cmd_words(r#""C:\t\run.exe" " " http://x "C:\t\l.txt" 0"#), [r"C:\t\run.exe", " ", "http://x", r"C:\t\l.txt", "0"]);
        assert!(has_word("del arc.ini\r\n", "arc.ini") && has_word(r"x\arc.ini", "arc.ini") && !has_word("xarc.ini", "arc.ini") && !has_word("arc.ini_", "arc.ini"));
    }

    #[test]
    fn and_chains_and_paths() {
        assert_eq!(split_and_and(r#"C:\t\nz.exe d9 a.unp a.hif9 a.raw&&del a.unp"#), Some(vec![r"C:\t\nz.exe d9 a.unp a.hif9 a.raw", "del a.unp"]));
        assert_eq!(split_and_and(r#""C:\a & b\x.exe" in out"#), Some(vec![r#""C:\a & b\x.exe" in out"#]), "& inside quotes");
        assert_eq!(split_and_and("a & b"), None, "a lone & is not a chain");
        assert_eq!(split_and_and("a&&"), None);
        assert_eq!(path_key(Path::new(r"C:\Games\X\..\Y\File.TXT\")).as_deref(), Some(r"c:\games\y\file.txt"));
        let a = vec![r"c:\g\inner_".to_string()];
        assert!(paths_overlap(&a, &[r"c:\g\inner_\00000137.raw".to_string()]), "a file inside a folder another step removes");
        assert!(paths_overlap(&a, &[r"c:\g\inner_".to_string()]));
        assert!(!paths_overlap(&a, &[r"c:\g\inner_2".to_string(), r"c:\g\inner".to_string()]));
    }
}
