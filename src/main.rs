use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use arc_unpack::config::Config;
use arc_unpack::extract::{self, human};
use arc_unpack::format::Archive;
use arc_unpack::perf;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

/// Fast, parallel unpacker for FreeArc archives (e.g. repack .bin files).
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show blocks, compression chains and sizes.
    List {
        archives: Vec<PathBuf>,
        /// Also list every file.
        #[arg(long)]
        files: bool,
    },
    /// Check that every compression method in the archives has a decoder configured.
    Check {
        archives: Vec<PathBuf>,
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Benchmarking: decode a file through a method chain from a config, e.g. a codec's input
    /// saved with ARC_UNPACK_TEE_DIR through that codec alone. Prints time, size and MD5.
    #[command(hide = true)]
    Replay {
        /// codecs.toml with the method
        #[arg(short, long)]
        config: PathBuf,
        /// Method chain in compression order, e.g. "xt2png"
        #[arg(short, long)]
        method: String,
        /// Input file
        input: PathBuf,
        /// Also write the output here
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Convert a FreeArc arc.ini (from the installer's tools folder) into codecs.toml.
    ImportArcini {
        arc_ini: PathBuf,
        /// Write here instead of printing.
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Install a repack folder in one step: prepare codecs from its setup.exe, verify, extract.
    Install(InstallArgs),
    /// Show what a repack's own install script does: components and steps (changes nothing).
    Script {
        /// The repack folder (with setup.exe)
        repack: PathBuf,
        /// Game folder to use in the listing
        #[arg(long, default_value = r"C:\Games\Game")]
        target: PathBuf,
        /// Print the bytecode of this script procedure instead (e.g. CurStepChanged)
        #[arg(long)]
        disasm: Option<String>,
    },
    /// List the runtimes a repack's game needs (DirectX, Visual C++, ...) and whether this PC has
    /// them; with --install, install the missing ones (as `install --runtimes` does).
    Runtimes {
        /// The repack folder (with setup.exe)
        repack: PathBuf,
        /// Download and install the missing ones (needs an administrator terminal).
        #[arg(long)]
        install: bool,
    },
    /// Extract the files embedded in an Inno Setup 5.5 installer (without running it).
    ExtractSetup {
        setup: PathBuf,
        /// Output folder (installer paths such as {tmp}/x land in <out>/tmp/x). Omit to list.
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Decode everything and verify CRCs without writing files.
    Test(RunArgs),
    /// Extract files into a folder.
    Extract {
        #[command(flatten)]
        run: RunArgs,
        /// Output folder.
        #[arg(short, long)]
        out: PathBuf,
        /// Replace existing files instead of failing on them.
        #[arg(long)]
        overwrite: bool,
        /// Parallel file writers per block.
        #[arg(long, default_value_t = 8)]
        writers: usize,
    },
}

#[derive(Args)]
struct RunArgs {
    archives: Vec<PathBuf>,
    /// codecs.toml describing how to run each compression method.
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// Blocks decoded at the same time (each runs its full codec chain; watch your RAM).
    #[arg(short, long, default_value_t = 2)]
    jobs: usize,
    /// Folder for codecs that need temp files (default: system temp).
    #[arg(long)]
    temp_dir: Option<PathBuf>,
    /// Don't adjust process priority / power throttling.
    #[arg(long)]
    no_tune: bool,
    /// At most this many memory-heavy jobs (CLS codecs, unarc) at once; beyond the first, only
    /// while plenty of RAM is free. 0 = no limit.
    #[arg(long, default_value_t = 2)]
    heavy_jobs: usize,
    /// Stop a block after this many seconds without any progress (a hung codec). 0 = never.
    #[arg(long, default_value_t = 600)]
    stall_timeout: u64,
    /// Skip blocks whose methods have no decoder instead of refusing to start.
    #[arg(long)]
    skip_missing: bool,
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    // Every codec process we start (and its children) dies with us: no orphans after errors.
    arc_unpack::guard::kill_descendants_on_exit();
    match cli.cmd {
        Cmd::List { archives, files } => {
            for a in open_all(&archives)? {
                print_archive(&a, files);
            }
            Ok(())
        }
        Cmd::Check { archives, config } => {
            let cfg = load_config(config.as_ref())?;
            let archives = open_all(&archives)?;
            let mut chains: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for a in &archives {
                for b in a.blocks.iter().filter(|b| b.compsize > 0) {
                    chains.entry(b.method.clone()).or_default().push(a.name());
                }
            }
            let mut bad = 0;
            for (chain, used_by) in &chains {
                let missing = cfg.missing(chain);
                let unarc = cfg.unarc_dll().filter(|p| p.is_file());
                let status = match (missing.is_empty(), &unarc) {
                    (true, _) => "ok".to_string(),
                    (false, Some(dll)) => format!(
                        "whole archive via {} (no stage decoder for: {})",
                        dll.display(),
                        missing.iter().map(|m| m.split(':').next().unwrap_or(m)).collect::<Vec<_>>().join(", ")
                    ),
                    (false, None) => format!("MISSING: {}", missing.join("; ")),
                };
                println!("{chain}\n    used by {}\n    {status}", used_by.join(", "));
                if unarc.is_none() {
                    bad += missing.len();
                }
            }
            anyhow::ensure!(bad == 0, "{bad} method(s) have no decoder");
            println!("all methods have decoders");
            Ok(())
        }
        Cmd::Replay { config, method, input, out } => {
            use std::io::{Read, Write};
            let cfg = Config::load(&config)?;
            let stages = cfg.resolve(&method)?;
            let reg = arc_unpack::stage::Registry::default();
            let ctx = arc_unpack::stage::Ctx {
                temp_dir: std::env::temp_dir(),
                host_dir: std::env::current_exe()?.parent().map(PathBuf::from).unwrap_or_default(),
                tune: true,
                reg: reg.clone(),
            };
            let file = std::fs::File::open(&input).with_context(|| format!("open {}", input.display()))?;
            let started = Instant::now();
            let mut output = arc_unpack::stage::build(&stages, Box::new(std::io::BufReader::with_capacity(1 << 20, file)), 0, &ctx)?;
            let mut sink = match &out {
                Some(p) => Some(std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(p)?)),
                None => None,
            };
            let (mut md5, mut total, mut buf) = (md5::Context::new(), 0u64, vec![0u8; 1 << 20]);
            let read = loop {
                match output.read(&mut buf) {
                    Ok(0) => break Ok(()),
                    Ok(n) => {
                        md5.consume(&buf[..n]);
                        total += n as u64;
                        if let Some(w) = sink.as_mut() {
                            w.write_all(&buf[..n])?;
                        }
                    }
                    Err(e) => break Err(e),
                }
            };
            drop(output);
            let errors = reg.join();
            let secs = started.elapsed().as_secs_f64();
            for (label, cpu, wall) in reg.cpu_report() {
                println!("  {label}: {:.0}s cpu / {wall:.0}s wall", cpu.unwrap_or(0.0));
            }
            read.context("reading the decoded output")?;
            for e in &errors {
                eprintln!("stage error: {e:#}");
            }
            println!("{total} bytes in {secs:.1}s, md5 {:x}", md5.finalize());
            anyhow::ensure!(errors.is_empty(), "{} stage error(s)", errors.len());
            Ok(())
        }
        Cmd::ImportArcini { arc_ini, out } => {
            let toml = arc_unpack::arcini::to_toml(&arc_ini)?;
            match out {
                Some(p) => {
                    std::fs::write(&p, toml)?;
                    println!("wrote {}", p.display());
                }
                None => print!("{toml}"),
            }
            Ok(())
        }
        Cmd::Runtimes { repack, install } => {
            // (Only the installer is needed: the .bin files may be gone after installing.)
            let setup_exe = arc_unpack::install::find_setup(&repack).with_context(|| format!("no installer (setup.exe) in {}", repack.display()))?;
            let setup = arc_unpack::inno::Setup::open(&setup_exe)?;
            let script = arc_unpack::pascal::Script::load(&setup.code).ok();
            let list = arc_unpack::runtimes::of_repack(&setup, script.as_ref(), &arc_unpack::setup_script::repack_files(&repack));
            if list.is_empty() {
                println!("This repack doesn't install any runtimes.");
                return Ok(());
            }
            for r in &list {
                let state = match (r.installed, r.available) {
                    (true, _) => "installed on this PC",
                    (false, true) => "missing",
                    (false, false) => "missing, and the installer has no way to get it",
                };
                println!("  {:28} {state}", r.label);
            }
            let missing = arc_unpack::runtimes::missing(&list);
            if !install || missing.is_empty() {
                return Ok(());
            }
            anyhow::ensure!(arc_unpack::guard::is_elevated(), "installing runtimes needs an administrator terminal");
            let (sink, term) = arc_unpack::progress::terminal();
            let (done, problems) = arc_unpack::runtimes::install(&setup, &missing, &arc_unpack::install::default_cache_dir().join("runtimes"), &sink);
            term.finish();
            for d in &done {
                println!("{d}");
            }
            for p in &problems {
                eprintln!("problem: {p}");
            }
            if !problems.is_empty() {
                std::process::exit(2);
            }
            Ok(())
        }
        Cmd::Script { repack, target, disasm } => {
            let setup_exe = if repack.is_file() {
                repack.clone()
            } else {
                arc_unpack::install::find_setup(&repack).with_context(|| format!("no installer (setup.exe) in {}", repack.display()))?
            };
            let repack = if repack.is_file() { repack.parent().map(PathBuf::from).unwrap_or_default() } else { repack };
            if let Some(name) = disasm {
                let setup = arc_unpack::inno::Setup::open(&setup_exe)?;
                let script = arc_unpack::pascal::Script::load(&setup.code)?;
                let p = script.find(&name).with_context(|| format!("no procedure {name} in the script"))?;
                for line in arc_unpack::pascal::disassemble(&script, p)? {
                    println!("{line}");
                }
                return Ok(());
            }
            let plan = arc_unpack::setup_script::plan(&setup_exe, &repack, &target, None)?;
            if let Ok(setup) = arc_unpack::inno::Setup::open(&setup_exe)
                && let Some(name) = setup.app.display_name()
            {
                println!("Game: {name}\n");
            }
            println!("Components (default install + downloaded optional parts):");
            for (name, desc, on) in &plan.components {
                println!("  [{}] {name}  {desc}", if *on { "x" } else { " " });
            }
            println!("\nSteps:");
            for (i, s) in plan.steps.iter().enumerate() {
                println!("  {:3}. {s}", i + 1);
            }
            for m in &plan.messages {
                println!("note: {m}");
            }
            if let Some(e) = &plan.error {
                println!("\nThe script stopped: {e}");
            }
            let setup = arc_unpack::inno::Setup::open(&setup_exe)?;
            let script = arc_unpack::pascal::Script::load(&setup.code).ok();
            let runtimes = arc_unpack::runtimes::of_repack(&setup, script.as_ref(), &arc_unpack::setup_script::repack_files(&repack));
            if !runtimes.is_empty() {
                println!("\nRuntimes the game needs (`install --runtimes` installs the missing ones):");
                for r in &runtimes {
                    let state = match (r.installed, r.available) {
                        (true, _) => "installed on this PC",
                        (false, true) => "missing",
                        (false, false) => "missing, and the installer has no way to get it",
                    };
                    println!("  {:28} {state}", r.label);
                }
            }
            println!("\n(dry run: extraction is only simulated, so steps that check for extracted files may be missing)");
            Ok(())
        }
        Cmd::ExtractSetup { setup, out } => {
            let s = arc_unpack::inno::Setup::open(&setup)?;
            println!("{}: {} files", s.version, s.files.len());
            match out {
                None => {
                    for f in &s.files {
                        let mut cond = Vec::new();
                        for (k, v) in [("components", &f.components), ("tasks", &f.tasks), ("languages", &f.languages), ("check", &f.check)] {
                            if !v.is_empty() {
                                cond.push(format!("{k}: {v}"));
                            }
                        }
                        let cond = if cond.is_empty() { String::new() } else { format!("  [{}]", cond.join("; ")) };
                        let dest = if f.dest.is_empty() { "(the installer's own uninstaller)" } else { &f.dest };
                        println!("  {:>10}  {dest}{cond}", f.size);
                    }
                }
                Some(dir) => {
                    let written = s.extract_all(&dir)?;
                    println!("extracted {} files to {} (SHA-1 verified)", written.len(), dir.display());
                }
            }
            Ok(())
        }
        Cmd::Install(args) => install(args),
        Cmd::Test(run) => go(run, None, false, 1),
        Cmd::Extract { run, out, overwrite, writers } => go(run, Some(out), overwrite, writers),
    }
}

#[derive(Args)]
struct InstallArgs {
    /// The repack folder (setup.exe + archives).
    repack: PathBuf,
    /// Where to install the game files.
    target: PathBuf,
    /// Use this codecs.toml instead of preparing codecs from setup.exe.
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// Skip checking the archives against the repack's MD5 list.
    #[arg(long)]
    no_verify: bool,
    /// Jobs running at once (light ones fill in around heavy ones).
    #[arg(short, long, default_value_t = 3)]
    jobs: usize,
    /// At most this many memory-heavy jobs at once (beyond the first, only while plenty of RAM
    /// is free).
    #[arg(long, default_value_t = 2)]
    heavy_jobs: usize,
    /// Keep RAM use to about this many GB: the decoders are told to stay within it and one
    /// memory-hungry decoder runs at a time. Slower.
    #[arg(long, value_name = "GB")]
    memory_limit: Option<u64>,
    #[arg(long, default_value_t = 8)]
    writers: usize,
    #[arg(long)]
    overwrite: bool,
    #[arg(long, default_value_t = 600)]
    stall_timeout: u64,
    #[arg(long)]
    no_tune: bool,
    /// Don't ask for administrator rights (some codecs then fail).
    #[arg(long)]
    no_elevate: bool,
    /// Where prepared codecs and set-aside scripts are kept.
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Scan, prepare codecs and check disk space, but don't extract anything.
    #[arg(long)]
    dry_run: bool,
    /// Don't run the installer's own install script; just extract every archive into the
    /// target (the older mode: repack post-processing steps are skipped).
    #[arg(long)]
    no_script: bool,
    /// Components to install, comma-separated (like setup.exe's /COMPONENTS). Default: what a
    /// default install selects, plus optional parts whose archives were downloaded. Optional
    /// parts left out aren't installed. Without the base game's component, the listed parts are
    /// added to the game already installed in TARGET.
    #[arg(long, value_delimiter = ',')]
    components: Option<Vec<String>>,
    /// After the game, install the runtimes it needs that this PC is missing (DirectX, Visual
    /// C++, .NET, PhysX): downloaded from the links the installer uses (DirectX's installer is
    /// bundled), checked for a Microsoft or NVIDIA signature, run silently.
    #[arg(long)]
    runtimes: bool,
    /// Don't put an uninstaller in the game folder (uninstall.exe, also listed in Settings >
    /// Apps). It removes exactly what the install added; saves and settings stay.
    #[arg(long)]
    no_uninstaller: bool,
    /// Create the installer's desktop shortcut and Start menu entry after a successful install.
    #[arg(long)]
    shortcuts: bool,
    /// Wait for Enter before exiting (used when relaunched in its own elevated window).
    #[arg(long, hide = true)]
    keep_open: bool,
}

fn install(a: InstallArgs) -> Result<()> {
    let abs = |p: &PathBuf| std::path::absolute(p).unwrap_or_else(|_| p.clone());
    let (repack, target) = (abs(&a.repack), abs(&a.target));
    if !a.no_elevate && !a.dry_run && !arc_unpack::guard::is_elevated() {
        // The repack's codecs commonly need admin rights (Global\ shared memory), like setup.exe.
        println!("Requesting administrator rights (the repack's codecs need them)...");
        let mut args = vec!["install".to_string(), repack.display().to_string(), target.display().to_string()];
        if let Some(c) = &a.config {
            args.extend(["--config".into(), abs(c).display().to_string()]);
        }
        if let Some(c) = &a.cache_dir {
            args.extend(["--cache-dir".into(), abs(c).display().to_string()]);
        }
        if let Some(c) = &a.components {
            args.extend(["--components".into(), c.join(",")]);
        }
        if let Some(gb) = a.memory_limit {
            args.extend(["--memory-limit".into(), gb.to_string()]);
        }
        for (flag, on) in
            [("--no-verify", a.no_verify), ("--overwrite", a.overwrite), ("--no-tune", a.no_tune), ("--no-script", a.no_script), ("--runtimes", a.runtimes), ("--no-uninstaller", a.no_uninstaller), ("--shortcuts", a.shortcuts)]
        {
            if on {
                args.push(flag.into());
            }
        }
        args.extend([
            "--jobs".into(),
            a.jobs.to_string(),
            "--heavy-jobs".into(),
            a.heavy_jobs.to_string(),
            "--writers".into(),
            a.writers.to_string(),
            "--stall-timeout".into(),
            a.stall_timeout.to_string(),
            "--keep-open".into(),
        ]);
        let code = arc_unpack::guard::relaunch_elevated(&args)?;
        std::process::exit(code);
    }
    if !a.no_tune {
        perf::tune_self();
    }
    let opts = arc_unpack::install::InstallOptions {
        repack_dir: repack,
        target_dir: target,
        cache_dir: a.cache_dir.map(|c| abs(&c)).unwrap_or_else(arc_unpack::install::default_cache_dir),
        config: a.config.map(|c| abs(&c)),
        verify_md5: !a.no_verify,
        jobs: a.jobs,
        heavy_jobs: if a.memory_limit.is_some() { 1 } else { a.heavy_jobs },
        writers: a.writers,
        overwrite: a.overwrite,
        stall_timeout: a.stall_timeout,
        tune: !a.no_tune,
        host_dir: std::env::current_exe()?.parent().map(PathBuf::from).unwrap_or_default(),
        cancel: Default::default(),
        skip_verify: Default::default(),
        dry_run: a.dry_run,
        script: !a.no_script,
        components: a.components,
        runtimes: a.runtimes,
        memory_limit: a.memory_limit.map(|gb| gb.max(1) << 30),
        uninstaller: !a.no_uninstaller,
        desktop_shortcut: a.shortcuts,
        start_menu: a.shortcuts,
    };
    let started = Instant::now();
    let (sink, term) = arc_unpack::progress::terminal();
    let result = arc_unpack::install::install(&opts, &sink);
    term.finish();
    let code = match result {
        Ok(r) => {
            let s = &r.summary;
            let secs = started.elapsed().as_secs_f64();
            for n in &s.notes {
                eprintln!("note: {n}");
            }
            if a.dry_run {
                println!("dry run: nothing was extracted");
            } else {
                println!(
                    "installed {} files ({} CRC-verified), {} in {:.0}s ({}/s)",
                    s.files_ok + s.files_unverified,
                    s.files_ok,
                    human(s.bytes),
                    secs,
                    human((s.bytes as f64 / secs.max(0.001)) as u64)
                );
            }
            if r.files_md5 > 0 {
                println!("{} files match the repack's own MD5 list", r.files_md5);
            }
            if r.setup_files > 0 {
                println!("{} files from setup.exe itself (launcher, _Redist, MD5 list)", r.setup_files);
            }
            if r.md5_checked > 0 {
                println!("{} archives matched the repack's MD5 list", r.md5_checked);
            }
            for line in &r.runtimes {
                println!("runtime {line}");
            }
            for link in &r.shortcuts_made {
                println!("shortcut: {}", link.display());
            }
            if let Some(u) = &r.uninstaller {
                println!("uninstaller: {} (also in Settings > Apps)", u.display());
            }
            if !r.scripts.is_empty() {
                println!("\nThe installer would run these scripts afterwards (not run; review them first):");
                for p in &r.scripts {
                    println!("  {}", p.display());
                }
            }
            if s.problems.is_empty() {
                0
            } else {
                eprintln!("{} problem(s):", s.problems.len());
                for p in &s.problems {
                    eprintln!("  {p}");
                }
                2
            }
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            1
        }
    };
    if a.keep_open {
        println!("\nPress Enter to close this window.");
        let mut s = String::new();
        let _ = std::io::stdin().read_line(&mut s);
    }
    std::process::exit(code);
}

fn go(run: RunArgs, out: Option<PathBuf>, overwrite: bool, writers: usize) -> Result<()> {
    let cfg = load_config(run.config.as_ref())?;
    let archives = open_all(&run.archives)?;
    if !run.no_tune {
        perf::tune_self();
    }
    let host_dir = std::env::current_exe()?.parent().map(PathBuf::from).unwrap_or_default();
    let opts = extract::Options {
        out_dir: out,
        jobs: run.jobs,
        writers,
        overwrite,
        skip_missing: run.skip_missing,
        stall_timeout: run.stall_timeout,
        tune: !run.no_tune,
        temp_dir: run.temp_dir.unwrap_or_else(std::env::temp_dir),
        host_dir,
        cancel: Default::default(),
        heavy_jobs: run.heavy_jobs,
        done_dir: None,
        md5: None,
        md5_prefix: String::new(),
        first_job_started: None,
    };
    let started = Instant::now();
    let (sink, term) = arc_unpack::progress::terminal();
    let s = extract::run(archives, &cfg, &opts, &sink);
    term.finish();
    let s = s?;
    let secs = started.elapsed().as_secs_f64();
    for n in &s.notes {
        eprintln!("note: {n}");
    }
    let unverified = if s.files_unverified > 0 {
        format!(", {} complete but not CRC-checked", s.files_unverified)
    } else {
        String::new()
    };
    println!(
        "{} files CRC OK{unverified}, {} in {:.0}s ({}/s)",
        s.files_ok,
        human(s.bytes),
        secs,
        human((s.bytes as f64 / secs.max(0.001)) as u64)
    );
    if !s.problems.is_empty() {
        eprintln!("{} problem(s):", s.problems.len());
        for p in &s.problems {
            eprintln!("  {p}");
        }
        std::process::exit(2);
    }
    Ok(())
}

fn load_config(path: Option<&PathBuf>) -> Result<Config> {
    match path {
        Some(p) => Config::load(p),
        None => {
            let default = PathBuf::from("codecs.toml");
            if default.is_file() { Config::load(&default) } else { Ok(Config::default()) }
        }
    }
}

fn open_all(paths: &[PathBuf]) -> Result<Vec<Archive>> {
    anyhow::ensure!(!paths.is_empty(), "no archives given");
    paths.iter().map(|p| Archive::open(p).with_context(|| format!("reading {}", p.display()))).collect()
}

fn print_archive(a: &Archive, files: bool) {
    let nfiles = a.entries.iter().filter(|e| !e.is_dir).count();
    let ndirs = a.entries.len() - nfiles;
    let crc = a.checksum.describe();
    println!("{}  ({} on disk, {} files + {} dirs, {} unpacked, {crc})", a.name(), human(a.len), nfiles, ndirs, human(a.total_size()));
    for (i, b) in a.blocks.iter().enumerate() {
        let n = a.entries[b.entries()].iter().filter(|e| !e.is_dir).count();
        println!(
            "  block {i}: {:>10} -> {:>10}  {:>6} files  @{}  {}",
            human(b.compsize),
            human(b.origsize),
            n,
            b.pos,
            b.method
        );
        if files {
            for e in &a.entries[b.entries()] {
                if e.is_dir {
                    println!("      {}/", e.path);
                } else {
                    println!("      {}  {}  crc={:08x}", e.path, e.size, e.crc);
                }
            }
        }
    }
}
