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
use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
    /// Scan, prepare codecs and check space, but extract nothing.
    pub dry_run: bool,
    /// Run the installer's own install script (falls back to plain extraction if it can't).
    pub script: bool,
    /// Components to install (None: default selection plus downloaded optional parts).
    pub components: Option<Vec<String>>,
}

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
    let text = std::fs::read_to_string(path).unwrap_or_default();
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

fn verify_md5(scan: &Scan, cancel: &AtomicBool, sink: &Sink) -> Result<usize> {
    let todo: Vec<&ScannedArchive> = scan.archives.iter().filter(|a| a.md5.is_some()).collect();
    if todo.is_empty() {
        events::warn(sink, "no MD5 list found next to the archives; skipping verification");
        return Ok(0);
    }
    let total: u64 = todo.iter().map(|a| a.archive.len).sum();
    sink(Event::Plan { jobs: todo.len(), compressed: total, output: 0 });
    let results: Vec<Result<()>> = std::thread::scope(|s| {
        let handles: Vec<_> = todo
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let id = 100_000 + i;
                s.spawn(move || -> Result<()> {
                    let name = a.archive.name();
                    sink(Event::JobStarted { id, label: name.clone(), method: "md5".into(), compressed: a.archive.len, output: 0 });
                    let started = std::time::Instant::now();
                    let mut f = std::fs::File::open(&a.archive.path)?;
                    let mut ctx = md5::Context::new();
                    let mut buf = vec![0u8; 8 << 20];
                    let mut done = 0u64;
                    loop {
                        if cancel.load(Ordering::Relaxed) {
                            bail!("cancelled");
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
                    Ok(())
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(anyhow::anyhow!("verify thread panicked")))).collect()
    });
    for r in results {
        r?;
    }
    Ok(todo.len())
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
    };
    std::fs::create_dir_all(&eopts.temp_dir)?;
    extract::run(reopened, cfg, &eopts, sink)
}

pub fn install(opts: &InstallOptions, sink: &Sink) -> Result<InstallReport> {
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
        });
    }

    let mut md5_checked = 0;
    if opts.verify_md5 {
        sink(Event::Phase { name: "Verifying archives".into() });
        md5_checked = verify_md5(&scan, &opts.cancel, sink)?;
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
        .and_then(|i| match md5_list_from_setup(i) {
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
        match install_setup_files(inno, &opts.target_dir, sink) {
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
    Ok(InstallReport { summary, codec_dir, scripts, scripts_dir, md5_checked, setup_files, files_md5, script_steps: 0 })
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
    use crate::setup_script::{self as ss, Folders, InnoHost};
    let abs = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let target = abs(&opts.target_dir);
    let selected: std::collections::BTreeSet<String> = match &opts.components {
        Some(list) => list.iter().map(|c| c.trim().to_ascii_lowercase()).filter(|c| !c.is_empty()).collect(),
        None => ss::default_components(inno, &ss::repack_files(&opts.repack_dir)),
    };
    events::info(sink, format!("running the installer's own install script; components: {}", selected.iter().cloned().collect::<Vec<_>>().join(", ")));

    // Upper bound: everything every archive holds (inner archives are extracted and deleted).
    let need: u64 = scan.archives.iter().map(|a| a.archive.total_size()).sum();
    if let Some(free) = crate::guard::free_space(&target) {
        ensure!(free > need + (1 << 30), "not enough space on the target drive: need {}, {} free", human(need), human(free));
    }
    if opts.dry_run {
        let plan = ss::plan(setup_exe, &opts.repack_dir, &target)?;
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

    let mut md5_checked = 0;
    if opts.verify_md5 {
        sink(Event::Phase { name: "Verifying archives".into() });
        md5_checked = verify_md5(scan, &opts.cancel, sink)?;
    }

    // A fresh copy of the installer's temporary files per run: the script edits some of them.
    let target_key = sha1_smol::Sha1::from(target.to_string_lossy().to_lowercase()).digest().to_string();
    let run_dir = opts.cache_dir.join("run").join(&target_key[..12]);
    let _ = std::fs::remove_dir_all(&run_dir);
    let tmp = run_dir.join("tmp");
    ss::prepare_tmp(inno, &tmp).context("unpacking the installer's temporary files")?;
    std::fs::create_dir_all(&target)?;

    let md5 = match md5_list_from_setup(inno) {
        Ok(l) => l.map(Arc::new),
        Err(e) => {
            events::warn(sink, format!("installed-file MD5 list: {e:#}"));
            None
        }
    };

    sink(Event::Phase { name: "Installing".into() });
    let mut runner = ScriptRunner { opts, sink, md5: md5.clone(), app: target.clone(), summary: Summary::default(), configs: Vec::new(), n: 0 };
    let folders = Folders { app: target.clone(), tmp: tmp.clone(), src: abs(&opts.repack_dir), setup_exe: abs(setup_exe) };
    let mut host = InnoHost::new(folders, inno, selected, &mut runner);
    let mut vm = crate::pascal::Vm::new(script);
    let mut script_error = None;
    let mut setup_files = 0;
    // Inno's order: ssInstall, then its [Files], then ssPostInstall (where repacks unpack), ssDone.
    for step in [ss::SS_INSTALL, ss::SS_POST_INSTALL, ss::SS_DONE] {
        if step == ss::SS_POST_INSTALL {
            match install_setup_files(inno, &target, sink) {
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
    let mut summary = runner.summary;
    summary.notes.extend(messages);
    if opts.cancel.load(Ordering::Relaxed) {
        summary.cancelled = true;
    } else if let Some(e) = script_error {
        summary.problems.push(format!("the installer's script stopped: {e}"));
    }

    let mut files_md5 = 0;
    if !summary.cancelled
        && let Some(list) = &md5
    {
        sink(Event::Phase { name: "Checking files".into() });
        // Check what the installed list (after the script merged optional parts) names.
        let wanted = md5_keys_on_disk(inno, &target);
        let entries: Vec<(&String, &ExpectedMd5)> = list.iter().filter(|(k, _)| wanted.as_ref().is_none_or(|w| w.contains(*k))).collect();
        let check = check_md5_list(&entries, &target, None, &opts.cancel);
        files_md5 = check.ok;
        summary.problems.extend(check.problems);
        summary.notes.extend(check.notes);
    }
    let _ = std::fs::remove_dir_all(&run_dir);
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
    })
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
        parse_installed_md5(&String::from_utf8_lossy(&text), &base, &mut list);
    }
    (!list.is_empty()).then(|| list.into_keys().collect())
}

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

    fn extract(&mut self, archive: &Path, out: &Path, arc_ini: &Path, base: Option<&str>) -> Result<bool> {
        if !archive.is_file() {
            events::warn(self.sink, format!("{}: not found", archive.display()));
            return Ok(false);
        }
        let cfg = self.config(arc_ini)?;
        let a = Archive::open(archive)?;
        // A part of the archive: extract everything next to the target, then move that part in.
        let stage = base.map(|_| out.join(format!(".arc-unpack-part-{}", self.n)));
        let dest = stage.clone().unwrap_or_else(|| out.to_path_buf());
        // MD5 list paths are relative to the game folder.
        let prefix = match (&stage, out.strip_prefix(&self.app)) {
            (None, Ok(rel)) => Some(if rel.as_os_str().is_empty() { String::new() } else { format!("{}\\", rel.display()) }),
            _ => None,
        };
        let eopts = Options {
            out_dir: Some(dest),
            jobs: self.opts.jobs,
            writers: self.opts.writers,
            overwrite: true,
            skip_missing: false,
            tune: self.opts.tune,
            temp_dir: self.opts.cache_dir.join("tmp"),
            host_dir: self.opts.host_dir.clone(),
            stall_timeout: self.opts.stall_timeout,
            cancel: self.opts.cancel.clone(),
            heavy_jobs: self.opts.heavy_jobs,
            done_dir: None,
            md5: prefix.as_ref().and(self.md5.clone()),
            md5_prefix: prefix.unwrap_or_default(),
        };
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
        Ok(ok)
    }

    fn exec(&mut self, program: &Path, args: &str, dir: &Path) -> Result<bool> {
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
        crate::stage::apply_child_flags(&mut cmd, self.opts.tune);
        let mut child = cmd.spawn().with_context(|| format!("start {}", program.display()))?;
        let status = loop {
            if let Some(st) = child.try_wait()? {
                break st;
            }
            if self.opts.cancel.load(Ordering::Relaxed) {
                let _ = child.kill();
                bail!("cancelled");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        if !status.success() {
            let name = program.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let note = format!("{name}{args}: exited with code {}", status.code().unwrap_or(-1));
            events::warn(self.sink, note.clone());
            self.summary.notes.push(note);
        }
        // Like ISDone: a program that ran counts as done; the final MD5 check catches failures.
        Ok(true)
    }
}

impl crate::setup_script::Runner for ScriptRunner<'_> {
    fn run(&mut self, step: &crate::setup_script::Step) -> Result<bool> {
        use crate::setup_script::Step;
        ensure!(!self.opts.cancel.load(Ordering::Relaxed), "cancelled");
        self.n += 1;
        let started = std::time::Instant::now();
        let ok = match step {
            Step::Extract { archive, out, cfg, base } => self.extract(archive, out, cfg, base.as_deref())?,
            Step::Exec { program, args, dir } => self.exec(program, args, dir)?,
        };
        let secs = started.elapsed().as_secs_f64();
        events::info(self.sink, format!("step {}: {step} ({secs:.1}s){}", self.n, if ok { "" } else { " FAILED" }));
        Ok(ok)
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
/// own folder).
fn md5_list_from_setup(inno: &crate::inno::Setup) -> Result<Option<Md5List>> {
    let mut list = Md5List::new();
    for f in inno.files.iter().filter(|f| f.has_data() && english_or_neutral(&f.languages)) {
        let Some(rel) = f.app_relative() else { continue };
        // FitGirl ships optional parts' entries as `*.addon` next to the list (the installer
        // appends them when the part is installed).
        let lower = rel.to_ascii_lowercase();
        if !lower.ends_with(".md5") && !lower.ends_with(".addon") {
            continue;
        }
        let mut base: Vec<&str> = rel.split(['\\', '/']).filter(|p| !p.is_empty()).collect();
        base.pop();
        parse_installed_md5(&String::from_utf8_lossy(&inno.read(f)?), &base, &mut list);
    }
    Ok((!list.is_empty()).then_some(list))
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
fn install_setup_files(inno: &crate::inno::Setup, target: &Path, sink: &Sink) -> Result<usize> {
    let mut skipped = Vec::new();
    let written = inno.extract_with(|f| {
        let Some(rel) = f.app_relative() else { return Ok(None) };
        if !english_or_neutral(&f.languages) {
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
}
