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
use crate::extract::{self, Options, Summary, human};
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
}

pub struct InstallReport {
    pub summary: Summary,
    pub codec_dir: PathBuf,
    /// Batch scripts from installer-script archives, for the user to review.
    pub scripts: Vec<PathBuf>,
    pub scripts_dir: Option<PathBuf>,
    pub md5_checked: usize,
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
    };
    std::fs::create_dir_all(&eopts.temp_dir)?;
    extract::run(reopened, cfg, &eopts, sink)
}

pub fn install(opts: &InstallOptions, sink: &Sink) -> Result<InstallReport> {
    sink(Event::Phase { name: "Scanning".into() });
    let first = scan(&opts.repack_dir, None)?;
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
        return Ok(InstallReport { summary: Summary::default(), codec_dir: cfg.codec_dir(), scripts: Vec::new(), scripts_dir: None, md5_checked: 0 });
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
        let s = run_role(&tools, &cfg, &codec_dir, true, opts, sink)?;
        ensure!(s.problems.is_empty() && !s.cancelled, "codec tools failed: {}", s.problems.join("; "));
    }

    let mut scripts = Vec::new();
    let mut scripts_dir = None;
    let script_archives = by_role(Role::Scripts);
    if !script_archives.is_empty() {
        sink(Event::Phase { name: "Setting aside installer scripts".into() });
        let dir = opts.cache_dir.join("scripts").join(opts.repack_dir.file_name().unwrap_or_default());
        let s = run_role(&script_archives, &cfg, &dir, true, opts, sink)?;
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
    let summary = run_role(&by_role(Role::Game), &cfg, &opts.target_dir, opts.overwrite, opts, sink)?;
    sink(Event::Phase { name: if summary.cancelled { "Cancelled".into() } else { "Done".into() } });
    Ok(InstallReport { summary, codec_dir, scripts, scripts_dir, md5_checked })
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
