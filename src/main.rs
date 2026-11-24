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
    /// Convert a FreeArc arc.ini (from the installer's tools folder) into codecs.toml.
    ImportArcini {
        arc_ini: PathBuf,
        /// Write here instead of printing.
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
        Cmd::Test(run) => go(run, None, false, 1),
        Cmd::Extract { run, out, overwrite, writers } => go(run, Some(out), overwrite, writers),
    }
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
    let crc = if a.standard_crc { "standard CRC-32" } else { "nonstandard checksums" };
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
