//! Per-block pipeline: read compressed range -> decode stages -> split into files -> writer pool.
//! Blocks (across all archives) are scheduled biggest-first on `jobs` parallel workers.
//! Progress is reported as `Event`s to a `Sink`; this module draws nothing itself.

use crate::config::Config;
use crate::events::{self, CodecCpu, Event, Level, Sink};
use crate::format::{Archive, DataBlock, Entry};
use crate::stage::{self, Ctx, Registry, Stream};
use anyhow::{Context, Result, anyhow, bail, ensure};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

const CHUNK: usize = 4 << 20;

pub struct Options {
    /// None = test mode (decode + verify CRCs, write nothing).
    pub out_dir: Option<PathBuf>,
    pub jobs: usize,
    pub writers: usize,
    pub overwrite: bool,
    pub skip_missing: bool,
    pub tune: bool,
    pub temp_dir: PathBuf,
    pub host_dir: PathBuf,
    /// Seconds without any progress before a block is declared hung and stopped (0 = never).
    pub stall_timeout: u64,
    /// Set to stop: running blocks are killed, queued ones are skipped.
    pub cancel: Arc<AtomicBool>,
    /// At most this many memory-heavy jobs at once (0 = no limit).
    pub heavy_jobs: usize,
}

#[derive(Default)]
pub struct Summary {
    /// Files whose CRC matched.
    pub files_ok: u64,
    /// Files decoded completely but not CRC-checked (archive uses a nonstandard checksum).
    pub files_unverified: u64,
    pub bytes: u64,
    pub problems: Vec<String>,
    pub notes: Vec<String>,
    pub cancelled: bool,
}

enum JobKind {
    /// One data block through arc-unpack's own stage pipeline.
    Block(usize),
    /// The whole archive through FreeArc's unarc.dll (for FreeArc built-in methods).
    Unarc,
}

struct Job {
    archive: Arc<Archive>,
    kind: JobKind,
}

impl Job {
    fn compsize(&self) -> u64 {
        match self.kind {
            JobKind::Block(i) => self.archive.blocks[i].compsize,
            JobKind::Unarc => self.archive.blocks.iter().map(|b| b.compsize).sum(),
        }
    }
    fn origsize(&self) -> u64 {
        match self.kind {
            JobKind::Block(i) => self.archive.blocks[i].origsize,
            JobKind::Unarc => self.archive.total_size(),
        }
    }
    fn method(&self) -> String {
        match self.kind {
            JobKind::Block(i) => self.archive.blocks[i].method.clone(),
            JobKind::Unarc => "unarc.dll (whole archive)".into(),
        }
    }
    fn label(&self) -> String {
        match self.kind {
            JobKind::Block(i) => format!("{} #{i}", self.archive.name()),
            JobKind::Unarc => format!("{} (unarc)", self.archive.name()),
        }
    }
}

pub fn run(archives: Vec<Archive>, cfg: &Config, opts: &Options, sink: &Sink) -> Result<Summary> {
    let archives: Vec<Arc<Archive>> = archives.into_iter().map(Arc::new).collect();

    // Validate everything up front: a missing codec should fail in one second, not one hour.
    let mut missing = Vec::new();
    let mut jobs = Vec::new();
    let unarc = cfg.unarc_dll().filter(|p| p.is_file());
    for a in &archives {
        let real = |b: &&DataBlock| !(b.origsize == 0 && b.compsize == 0); // skip directory-only blocks
        let any_missing = a.blocks.iter().filter(real).any(|b| !cfg.missing(&b.method).is_empty());
        if any_missing && unarc.is_some() {
            jobs.push(Job { archive: a.clone(), kind: JobKind::Unarc });
            continue;
        }
        for (i, b) in a.blocks.iter().enumerate().filter(|(_, b)| real(b)) {
            let m = cfg.missing(&b.method);
            if m.is_empty() {
                jobs.push(Job { archive: a.clone(), kind: JobKind::Block(i) });
            } else if opts.skip_missing {
                events::warn(sink, format!("skipping {} block {i} ({}): no decoder for {}", a.name(), b.method, m.join(", ")));
            } else {
                missing.extend(m.into_iter().map(|m| format!("{} block {i}: {m}", a.name())));
            }
        }
    }
    if !missing.is_empty() {
        bail!("missing decoders (use --skip-missing to extract the rest):\n  {}", missing.join("\n  "));
    }

    if !crate::guard::is_elevated() {
        let needs = codecs_needing_admin(&jobs, cfg);
        if !needs.is_empty() {
            events::warn(
                sink,
                format!(
                    "not running as administrator, but these codecs create Global\\ shared memory and will fail \
                     without it: {}. Run arc-unpack as administrator.",
                    needs.join(", ")
                ),
            );
        }
    }

    let mut summary = Summary::default();
    let via_unarc: Vec<String> =
        jobs.iter().filter(|j| matches!(j.kind, JobKind::Unarc)).map(|j| j.archive.name()).collect();
    for a in archives.iter().filter(|a| !a.standard_crc && !via_unarc.contains(&a.name())) {
        summary.notes.push(format!(
            "{}: block checksums are not standard CRC-32 (modified FreeArc), so file CRCs cannot be \
             verified; sizes and codec exit codes are still checked",
            a.name()
        ));
    }
    if let Some(out) = &opts.out_dir {
        create_dirs(&archives, out, opts.overwrite, &mut summary)?;
    }

    jobs.sort_by_key(|j| std::cmp::Reverse(j.compsize()));
    sink(Event::Plan {
        jobs: jobs.len(),
        compressed: jobs.iter().map(Job::compsize).sum(),
        output: jobs.iter().map(Job::origsize).sum(),
    });

    let summary = Arc::new(Mutex::new(summary));
    let pending: Vec<(usize, Job, bool)> =
        jobs.into_iter().enumerate().map(|(id, j)| { let heavy = is_heavy(&j, cfg); (id, j, heavy) }).collect();
    let queue = Mutex::new(Queue { pending, heavy_running: 0 });
    let wake = std::sync::Condvar::new();

    let all_done = AtomicBool::new(false);
    let dialogs: Mutex<Vec<String>> = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        // Codecs that report errors with a MessageBox would otherwise block until someone
        // clicks OK. Record the text and close the dialog, so their error path continues.
        s.spawn(|| {
            while !all_done.load(Ordering::Relaxed) {
                for m in crate::guard::dismiss_codec_dialogs() {
                    sink(Event::Message { level: Level::Warn, text: format!("codec error dialog (closed automatically): {m}") });
                    dialogs.lock().unwrap().push(m);
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        });
        let mut workers = Vec::new();
        for _ in 0..opts.jobs.max(1) {
            let summary = summary.clone();
            let (queue, wake) = (&queue, &wake);
            workers.push(s.spawn(move || {
                while let Some((id, job, heavy)) = next_job(queue, wake, opts) {
                    let label = job.label();
                    sink(Event::JobStarted {
                        id,
                        label: label.clone(),
                        method: job.method(),
                        compressed: job.compsize(),
                        output: job.origsize(),
                    });
                    let started = Instant::now();
                    let res = match job.kind {
                        JobKind::Block(i) => run_block(&job.archive, &job.archive.blocks[i], cfg, opts, id, sink),
                        JobKind::Unarc => run_unarc(&job.archive, cfg, opts, id, sink),
                    };
                    let secs = started.elapsed().as_secs_f64();
                    let mut sm = summary.lock().unwrap();
                    match res {
                        Ok(r) => {
                            sm.files_ok += r.files_ok;
                            sm.files_unverified += r.files_unverified;
                            sm.bytes += job.origsize();
                            sm.problems.extend(r.problems.into_iter().map(|p| format!("{label}: {p}")));
                            sink(Event::JobFinished {
                                id,
                                label,
                                files: r.files_ok + r.files_unverified,
                                output: job.origsize(),
                                secs,
                                codecs: r.codecs,
                                error: None,
                            });
                        }
                        Err(e) => {
                            let msg = format!("{e:#}");
                            sm.problems.push(format!("{label}: {msg}"));
                            sink(Event::JobFinished { id, label, files: 0, output: 0, secs, codecs: Vec::new(), error: Some(msg) });
                        }
                    }
                    drop(sm);
                    if heavy {
                        queue.lock().unwrap().heavy_running -= 1;
                        wake.notify_all();
                    }
                }
            }));
        }
        for w in workers {
            let _ = w.join();
        }
        all_done.store(true, Ordering::Relaxed);
    });
    let mut summary = Arc::try_unwrap(summary).ok().unwrap().into_inner().unwrap();
    summary.problems.extend(dialogs.into_inner().unwrap().into_iter().map(|m| format!("codec error dialog: {m}")));
    summary.cancelled = opts.cancel.load(Ordering::Relaxed);
    Ok(summary)
}

struct Queue {
    /// (id, job, heavy), biggest first.
    pending: Vec<(usize, Job, bool)>,
    heavy_running: usize,
}

/// Next job this worker may start: the biggest pending one, except that heavy jobs wait while
/// `heavy_jobs` of them are already running (light ones may overtake them). None = all done.
fn next_job(queue: &Mutex<Queue>, wake: &std::sync::Condvar, opts: &Options) -> Option<(usize, Job, bool)> {
    let mut q = queue.lock().unwrap();
    loop {
        if opts.cancel.load(Ordering::Relaxed) {
            q.pending.clear();
            wake.notify_all();
        }
        if q.pending.is_empty() {
            return None;
        }
        let limit = opts.heavy_jobs;
        let running = q.heavy_running;
        if let Some(pos) = q.pending.iter().position(|(_, _, heavy)| !heavy || limit == 0 || running < limit) {
            let item = q.pending.remove(pos);
            if item.2 {
                q.heavy_running += 1;
            }
            return Some(item);
        }
        q = wake.wait_timeout(q, Duration::from_millis(250)).unwrap().0;
    }
}

/// Memory-hungry jobs: CLS codecs (lolz-class decoders size their buffers by a share of RAM)
/// and whole-archive unarc runs (which load them too).
fn is_heavy(job: &Job, cfg: &Config) -> bool {
    match job.kind {
        JobKind::Unarc => true,
        JobKind::Block(i) => cfg
            .resolve(&job.archive.blocks[i].method)
            .map(|st| st.iter().any(|s| matches!(s.codec, crate::config::Codec::Cls { .. })))
            .unwrap_or(false),
    }
}

/// CLS codecs (or their helper executables) that hard-code "Global\" object names, which only
/// works with administrator rights.
fn codecs_needing_admin(jobs: &[Job], cfg: &Config) -> Vec<String> {
    let mut dlls = std::collections::BTreeSet::new();
    for job in jobs {
        match job.kind {
            JobKind::Block(i) => {
                for st in cfg.resolve(&job.archive.blocks[i].method).unwrap_or_default() {
                    if let crate::config::Codec::Cls { dll, .. } = st.codec {
                        dlls.insert(dll);
                    }
                }
            }
            JobKind::Unarc => {
                if let Ok(rd) = std::fs::read_dir(cfg.codec_dir()) {
                    for e in rd.flatten() {
                        let n = e.file_name().to_string_lossy().to_ascii_lowercase();
                        if n.starts_with("cls-") && n.ends_with(".dll") {
                            dlls.insert(e.path());
                        }
                    }
                }
            }
        }
    }
    let has_global = |p: &Path| std::fs::read(p).map(|b| b.windows(7).any(|w| w == b"Global\\")).unwrap_or(false);
    dlls.into_iter()
        .filter(|dll| {
            let stem = dll.with_extension("");
            [dll.clone(), stem.with_file_name(format!("{}_x64.exe", stem.file_name().unwrap_or_default().to_string_lossy()))]
                .iter()
                .any(|p| has_global(p))
        })
        .map(|d| d.file_name().unwrap_or_default().to_string_lossy().into_owned())
        .collect()
}

struct BlockResult {
    files_ok: u64,
    files_unverified: u64,
    problems: Vec<String>,
    codecs: Vec<CodecCpu>,
}

/// Counts decoded bytes coming out of the last stage (for the stall watchdog).
struct CountOut {
    inner: Stream,
    n: Arc<AtomicU64>,
}
impl Read for CountOut {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let k = self.inner.read(buf)?;
        self.n.fetch_add(k as u64, Ordering::Relaxed);
        Ok(k)
    }
}

/// Counts compressed bytes read, and reports them as progress.
struct Counting<R> {
    inner: R,
    done: Arc<AtomicU64>,
    id: usize,
    sink: Sink,
}
impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        let done = self.done.fetch_add(n as u64, Ordering::Relaxed) + n as u64;
        (self.sink)(Event::JobProgress { id: self.id, done });
        Ok(n)
    }
}

fn run_block(archive: &Archive, block: &DataBlock, cfg: &Config, opts: &Options, id: usize, sink: &Sink) -> Result<BlockResult> {
    let stages = cfg.resolve(&block.method)?;
    let mut f = File::open(&archive.path)?;
    f.seek(SeekFrom::Start(block.pos))?;
    let consumed = Arc::new(AtomicU64::new(0));
    let input: Stream = Box::new(Counting {
        inner: BufReader::with_capacity(CHUNK, f.take(block.compsize)),
        done: consumed.clone(),
        id,
        sink: sink.clone(),
    });

    let reg = Registry::default();
    let ctx = Ctx { temp_dir: opts.temp_dir.clone(), host_dir: opts.host_dir.clone(), tune: opts.tune, reg: reg.clone() };
    let output = stage::build(&stages, input, block.origsize, &ctx)?;
    let produced = Arc::new(AtomicU64::new(0));
    let output: Stream = Box::new(CountOut { inner: output, n: produced.clone() });

    // Watchdog: stops the block when cancelled, or when no compressed input is consumed and no
    // output produced for a long time (a hung codec, or one waiting on something it can't get).
    let done = Arc::new(AtomicBool::new(false));
    let stalled = Arc::new(AtomicBool::new(false));
    let watchdog = {
        let (done, stalled, produced, consumed, reg, cancel) =
            (done.clone(), stalled.clone(), produced.clone(), consumed.clone(), reg.clone(), opts.cancel.clone());
        let limit = (opts.stall_timeout > 0).then(|| Duration::from_secs(opts.stall_timeout));
        std::thread::spawn(move || {
            let mut last = (u64::MAX, u64::MAX);
            let mut since = Instant::now();
            while !done.load(Ordering::Relaxed) {
                if cancel.load(Ordering::Relaxed) {
                    reg.kill_all();
                    return;
                }
                let now = (consumed.load(Ordering::Relaxed), produced.load(Ordering::Relaxed));
                if now != last {
                    last = now;
                    since = Instant::now();
                } else if limit.is_some_and(|l| since.elapsed() > l) {
                    stalled.store(true, Ordering::Relaxed);
                    reg.kill_all();
                    return;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        })
    };

    let entries = &archive.entries[block.entries()];
    let split = split(output, entries, opts, archive.standard_crc);
    done.store(true, Ordering::Relaxed);
    let _ = watchdog.join();
    if split.is_err() {
        reg.kill_all();
    }
    let split = if opts.cancel.load(Ordering::Relaxed) {
        Err(anyhow!("cancelled"))
    } else if stalled.load(Ordering::Relaxed) {
        Err(anyhow!(
            "no progress for {} s: a codec hung (or is waiting on something it can't get); stopped it",
            opts.stall_timeout
        ))
    } else {
        split
    };
    let stage_errs = reg.join();
    let codecs = codec_report(&reg);

    match split {
        Ok(mut r) => {
            // Stage errors after a fully verified split (e.g. a codec complaining about trailing
            // bytes) are reported but don't invalidate the files.
            r.problems.extend(stage_errs.iter().map(|e| format!("{e:#}")));
            r.codecs = codecs;
            Ok(r)
        }
        Err(e) if !stage_errs.is_empty() && !opts.cancel.load(Ordering::Relaxed) => {
            let detail: Vec<String> = stage_errs.iter().map(|e| format!("{e:#}")).collect();
            Err(anyhow!("{e:#}\n    stage errors:\n    {}", detail.join("\n    ")))
        }
        Err(e) => Err(e),
    }
}

/// Extracts (or tests) a whole archive through unarc.dll via unarc-host-x86.exe. unarc checks
/// file CRCs with its own (possibly modified) algorithm, so success means the files verified.
fn run_unarc(archive: &Archive, cfg: &Config, opts: &Options, id: usize, sink: &Sink) -> Result<BlockResult> {
    use std::io::BufRead;
    let dll = cfg.unarc_dll().context("no unarc configured")?;
    let host = stage::helper_exe(&["unarc-host-x86.exe"], &opts.host_dir)?;
    let abs = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let mut cmd = std::process::Command::new(host);
    cmd.arg(&dll).arg(if opts.out_dir.is_some() { "x" } else { "t" }).arg(abs(&archive.path));
    cmd.arg(if opts.overwrite { "-o+" } else { "-o-" });
    if let Some(ini) = cfg.arc_ini_path() {
        cmd.arg(format!("-cfg{}", abs(&ini).display()));
    }
    if let Some(out) = &opts.out_dir {
        cmd.arg(format!("-dp{}", abs(out).display()));
    }
    cmd.arg(format!("-w{}", abs(&opts.temp_dir).display()));
    // CreateProcess finds unarc's external codecs (rzw.exe, xtool\...) relative to the current dir.
    cmd.current_dir(cfg.codec_dir());
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    stage::apply_child_flags(&mut cmd, opts.tune);
    let child = cmd.spawn().context("start unarc-host-x86.exe")?;
    if opts.tune {
        crate::perf::tune_child(&child, false);
    }
    let started = Instant::now();
    let child = Arc::new(Mutex::new(child));
    let stderr = child.lock().unwrap().stderr.take().unwrap();
    let stdout = child.lock().unwrap().stdout.take().unwrap();
    let err_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = BufReader::new(stderr).read_to_string(&mut s);
        s
    });
    // Cancel support: unarc reports progress in coarse steps, so watch the flag separately.
    let finished = Arc::new(AtomicBool::new(false));
    let canceller = {
        let (child, finished, cancel) = (child.clone(), finished.clone(), opts.cancel.clone());
        std::thread::spawn(move || {
            while !finished.load(Ordering::Relaxed) {
                if cancel.load(Ordering::Relaxed) {
                    let _ = child.lock().unwrap().kill();
                    return;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        })
    };
    let total = archive.blocks.iter().map(|b| b.compsize).sum::<u64>();
    let mut last = 0u64;
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        if let Some(mb) = line.strip_prefix("read ").and_then(|v| v.trim().parse::<u64>().ok()) {
            let pos = (mb << 20).min(total);
            if pos > last {
                sink(Event::JobProgress { id, done: pos });
                last = pos;
            }
        }
    }
    // Poll instead of wait(): holding the lock in a blocking wait would stop the canceller.
    let status = loop {
        if let Some(st) = child.lock().unwrap().try_wait()? {
            break st;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    finished.store(true, Ordering::Relaxed);
    let _ = canceller.join();
    let errors = err_thread.join().unwrap_or_default();
    let wall = started.elapsed().as_secs_f64();
    let cpu = crate::perf::cpu_seconds(&child.lock().unwrap());
    if opts.cancel.load(Ordering::Relaxed) {
        bail!("cancelled");
    }
    if !status.success() {
        bail!("unarc.dll failed ({status}): {}", errors.trim());
    }
    sink(Event::JobProgress { id, done: total });
    let mut problems = Vec::new();
    if !errors.trim().is_empty() {
        problems.push(format!("unarc.dll reported: {}", errors.trim()));
    }
    let files = archive.entries.iter().filter(|e| !e.is_dir).count() as u64;
    let codecs = vec![CodecCpu { label: "unarc.dll".into(), cpu_secs: cpu, wall_secs: wall }];
    Ok(BlockResult { files_ok: files, files_unverified: 0, problems, codecs })
}

fn codec_report(reg: &Registry) -> Vec<CodecCpu> {
    reg.cpu_report()
        .into_iter()
        .map(|(label, cpu_secs, wall_secs)| CodecCpu { label, cpu_secs, wall_secs })
        .collect()
}

struct FileJob {
    path: PathBuf,
    rel: String,
    size: u64,
    mtime: u32,
    crc: u32,
    chunks: Receiver<Vec<u8>>,
}

/// Cuts the decoded stream into files. In extract mode, files go to a pool of writer threads
/// (file creation/close is the slow part on Windows, so it pays to overlap it); in test mode
/// CRCs are checked inline.
fn split(mut stream: Stream, entries: &[Entry], opts: &Options, verify: bool) -> Result<BlockResult> {
    let mut problems = Vec::new();
    let mut files_ok = 0u64;
    let mut files_unverified = 0u64;

    let Some(out_dir) = &opts.out_dir else {
        let mut buf = vec![0u8; CHUNK];
        for e in entries.iter().filter(|e| !e.is_dir) {
            let mut h = crc32fast::Hasher::new();
            let mut left = e.size;
            while left > 0 {
                let n = (left as usize).min(CHUNK);
                stream.read_exact(&mut buf[..n]).with_context(|| short_msg(e, left))?;
                h.update(&buf[..n]);
                left -= n as u64;
            }
            let crc = h.finalize();
            if !verify {
                files_unverified += 1;
            } else if crc == e.crc {
                files_ok += 1;
            } else {
                problems.push(format!("CRC mismatch: {} (got {crc:08x}, want {:08x})", e.path, e.crc));
            }
        }
        check_trailing(&mut stream, &mut problems);
        return Ok(BlockResult { files_ok, files_unverified, problems, codecs: Vec::new() });
    };

    // Small queue: each queued file can hold up to 4 chunks, so this bounds memory.
    let (job_tx, job_rx) = bounded::<FileJob>(opts.writers.max(1) * 2);
    let (res_tx, res_rx) = unbounded::<Result<String, String>>();
    let ok_counter = Arc::new(AtomicU64::new(0));
    let split_result = std::thread::scope(|s| -> Result<()> {
        for _ in 0..opts.writers.max(1) {
            let job_rx = job_rx.clone();
            let res_tx = res_tx.clone();
            let ok_counter = ok_counter.clone();
            let overwrite = opts.overwrite;
            s.spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    match write_file(&job, overwrite, verify) {
                        Ok(()) => {
                            ok_counter.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => {
                            // Keep draining so the splitter never blocks on a dead file.
                            while job.chunks.recv().is_ok() {}
                            let _ = res_tx.send(Err(format!("{}: {e:#}", job.rel)));
                        }
                    }
                }
            });
        }
        drop(job_rx);
        let r = feed_files(&mut stream, entries, out_dir, &job_tx);
        drop(job_tx);
        r
    });
    drop(res_tx);
    problems.extend(res_rx.iter().filter_map(|r| r.err()));
    split_result?;
    check_trailing(&mut stream, &mut problems);
    let written = ok_counter.load(Ordering::Relaxed);
    if verify {
        files_ok += written;
    } else {
        files_unverified += written;
    }
    Ok(BlockResult { files_ok, files_unverified, problems, codecs: Vec::new() })
}

fn feed_files(stream: &mut Stream, entries: &[Entry], out_dir: &Path, job_tx: &Sender<FileJob>) -> Result<()> {
    for e in entries.iter().filter(|e| !e.is_dir) {
        let path = safe_join(out_dir, &e.path)?;
        let (tx, rx) = bounded::<Vec<u8>>(4);
        job_tx
            .send(FileJob { path, rel: e.path.clone(), size: e.size, mtime: e.mtime, crc: e.crc, chunks: rx })
            .map_err(|_| anyhow!("writer pool stopped"))?;
        let mut left = e.size;
        while left > 0 {
            let n = (left as usize).min(CHUNK);
            let mut buf = vec![0u8; n];
            stream.read_exact(&mut buf).with_context(|| short_msg(e, left))?;
            tx.send(buf).map_err(|_| anyhow!("writer for {} stopped", e.path))?;
            left -= n as u64;
        }
    }
    Ok(())
}

fn short_msg(e: &Entry, left: u64) -> String {
    format!("decoded stream ended early in {} ({} bytes missing)", e.path, left)
}

fn check_trailing(stream: &mut Stream, problems: &mut Vec<String>) {
    let mut probe = [0u8; 1];
    match stream.read(&mut probe) {
        Ok(0) => {}
        Ok(_) => {
            let mut extra = 1u64;
            let mut sink = vec![0u8; CHUNK];
            while let Ok(n) = stream.read(&mut sink) {
                if n == 0 {
                    break;
                }
                extra += n as u64;
            }
            problems.push(format!("decoded stream has {extra} unexpected trailing bytes"));
        }
        Err(e) => problems.push(format!("error after last file: {e}")),
    }
}

fn write_file(job: &FileJob, overwrite: bool, verify: bool) -> Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true);
    if overwrite { opts.create(true).truncate(true) } else { opts.create_new(true) };
    let file = opts.open(&job.path).with_context(|| format!("create {}", job.path.display()))?;
    if job.size > 0 {
        file.set_len(job.size)?; // reserve space up front: fewer fragments, early "disk full"
    }
    let mut w = std::io::BufWriter::with_capacity(1 << 20, file);
    let mut h = crc32fast::Hasher::new();
    let mut written = 0u64;
    for chunk in job.chunks.iter() {
        h.update(&chunk);
        w.write_all(&chunk)?;
        written += chunk.len() as u64;
    }
    ensure!(written == job.size, "only {written} of {} bytes arrived", job.size);
    let file = w.into_inner().map_err(|e| e.into_error())?;
    let _ = file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(job.mtime as u64));
    let crc = h.finalize();
    ensure!(!verify || crc == job.crc, "CRC mismatch (got {crc:08x}, want {:08x})", job.crc);
    Ok(())
}

fn create_dirs(archives: &[Arc<Archive>], out: &Path, _overwrite: bool, summary: &mut Summary) -> Result<()> {
    std::fs::create_dir_all(out).with_context(|| format!("create {}", out.display()))?;
    let mut dirs = std::collections::BTreeSet::new();
    for a in archives {
        for e in &a.entries {
            let p = match safe_join(out, &e.path) {
                Ok(p) => p,
                Err(err) => {
                    summary.problems.push(format!("{}: {err:#}", a.name()));
                    continue;
                }
            };
            if e.is_dir {
                dirs.insert(p);
            } else if let Some(parent) = p.parent() {
                dirs.insert(parent.to_path_buf());
            }
        }
    }
    for d in dirs {
        std::fs::create_dir_all(&d).with_context(|| format!("create {}", d.display()))?;
    }
    Ok(())
}

/// Joins an archive path onto the output dir, refusing anything that could escape it
/// (`..`, absolute paths, drive letters, alternate data streams, device names).
pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    let mut out = root.to_path_buf();
    let mut any = false;
    for part in rel.split(['/', '\\']) {
        if part.is_empty() || part == "." {
            continue;
        }
        ensure!(part != "..", "refusing path with '..': {rel}");
        ensure!(!part.contains(':'), "refusing path with ':': {rel}");
        let stem = part.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
        let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit());
        ensure!(!reserved, "refusing reserved device name: {rel}");
        let mut comps = Path::new(part).components();
        ensure!(matches!((comps.next(), comps.next()), (Some(Component::Normal(_)), None)), "refusing odd path component in {rel}");
        out.push(part);
        any = true;
    }
    ensure!(any, "empty path in archive");
    Ok(out)
}

pub fn human(n: u64) -> String {
    let n = n as f64;
    if n >= 1e9 { format!("{:.2} GB", n / 1e9) } else if n >= 1e6 { format!("{:.1} MB", n / 1e6) } else { format!("{:.0} KB", n / 1e3) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_rules() {
        let r = Path::new("C:\\out");
        assert_eq!(safe_join(r, "a/b.txt").unwrap(), Path::new("C:\\out\\a\\b.txt"));
        assert_eq!(safe_join(r, "./a//b").unwrap(), Path::new("C:\\out\\a\\b"));
        for bad in ["../x", "a/../../x", "C:/Windows/x", "a/b:stream", "con", "a/NUL.txt", "LPT1", ""] {
            assert!(safe_join(r, bad).is_err(), "{bad} should be refused");
        }
        assert!(safe_join(r, "console.txt").is_ok());
        assert!(safe_join(r, "COM10.txt").is_ok());
    }
}
