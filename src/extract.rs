//! Per-block pipeline: read compressed range -> decode stages -> split into files -> writer pool.
//! Blocks (across all archives) are scheduled biggest-first on `jobs` parallel workers.
//! Progress is reported as `Event`s to a `Sink`; this module draws nothing itself.

use crate::config::Config;
use crate::events::{self, CodecCpu, Event, Level, Sink};
use crate::format::{Archive, Checksum, DataBlock, Entry};
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
    /// At most this many memory-heavy jobs at once (0 = no limit). The limit is machine-wide:
    /// slots are shared with every other arc-unpack process.
    pub heavy_jobs: usize,
    /// Resume support: a marker per finished job is written here, and jobs that already have
    /// one are skipped.
    pub done_dir: Option<PathBuf>,
    /// The repack's own list of installed-file MD5s (e.g. `_Redist\fitgirl.md5`). Listed files
    /// are hashed while they are written, so no separate verification pass is needed.
    pub md5: Option<Arc<Md5List>>,
    /// Where `out_dir` sits inside the game folder (e.g. `temp\`), for MD5 list lookups.
    pub md5_prefix: String,
    /// Set once this run has picked its first job (or has none): lets a caller start several
    /// runs in a chosen order, each getting its memory-heavy slot before the next asks.
    pub first_job_started: Option<Arc<AtomicBool>>,
}

/// Expected MD5 of one installed file.
pub struct ExpectedMd5 {
    pub md5: [u8; 16],
    /// Install-relative path as the list spells it (for messages).
    pub path: String,
    /// Set once the file was hashed while being written...
    pub checked: AtomicBool,
    /// ...and whether it matched.
    pub matched: AtomicBool,
}

impl ExpectedMd5 {
    pub fn new(md5: [u8; 16], path: String) -> Self {
        ExpectedMd5 { md5, path, checked: AtomicBool::new(false), matched: AtomicBool::new(false) }
    }
}

/// Keyed by [`md5_key`] of the path relative to the install folder.
pub type Md5List = std::collections::HashMap<String, ExpectedMd5>;

/// Normalized lookup key for an install-relative path.
pub fn md5_key(rel: &str) -> String {
    rel.replace('/', "\\").trim_start_matches('\\').to_lowercase()
}

pub fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Default)]
pub struct Summary {
    /// Files whose CRC matched.
    pub files_ok: u64,
    /// Files decoded completely but not CRC-checked (the archive's checksum kind is unknown).
    pub files_unverified: u64,
    pub bytes: u64,
    pub problems: Vec<String>,
    pub notes: Vec<String>,
    pub cancelled: bool,
    /// Jobs skipped because an earlier run already finished them (resume).
    pub skipped: u64,
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
    /// Resume marker file name for this job (archive name, block, archive size).
    fn marker(&self) -> String {
        let part = match self.kind {
            JobKind::Block(i) => i.to_string(),
            JobKind::Unarc => "all".into(),
        };
        format!("{}-{}-{}.done", self.archive.name(), part, self.archive.len)
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
    if opts.tune {
        crate::helpers::place_xtool_timer(&cfg.codec_dir(), &opts.host_dir);
    }

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

    // Without administrator rights these codecs can't create their shared memory: they fail, and
    // their helper processes wait forever. Stop before starting any.
    if !crate::guard::is_elevated() {
        let needs = codecs_needing_admin(&jobs, cfg);
        ensure!(
            needs.is_empty(),
            "these codecs need administrator rights (they create Global\\ shared memory): {}. Run arc-unpack as \
             administrator (without --no-elevate).",
            needs.join(", ")
        );
    }

    let mut summary = Summary::default();
    let via_unarc: Vec<String> =
        jobs.iter().filter(|j| matches!(j.kind, JobKind::Unarc)).map(|j| j.archive.name()).collect();
    for a in archives.iter().filter(|a| !a.checksum.verifies() && !via_unarc.contains(&a.name())) {
        summary.notes.push(format!(
            "{}: block checksums are neither standard nor FitGirl CRC-32 (another modified FreeArc), \
             so file CRCs cannot be verified; sizes and codec exit codes are still checked",
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
        jobs.into_iter().map(|j| { let heavy = is_heavy(&j, cfg); (next_job_id(), j, heavy) }).collect();
    let queue = Mutex::new(Queue { pending, heavy_running: 0, told_waiting: false });
    let wake = std::sync::Condvar::new();

    let all_done = AtomicBool::new(false);
    let dialogs: Mutex<Vec<String>> = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        // Codecs that report errors with a MessageBox would otherwise block until someone
        // clicks OK. Record the text and close the dialog, so their error path continues.
        s.spawn(|| {
            let mut tuned = std::collections::HashSet::new();
            let tune = opts.tune && std::env::var_os("ARC_UNPACK_NO_DESCENDANT_TUNE").is_none();
            while !all_done.load(Ordering::Relaxed) {
                if tune {
                    crate::guard::tune_descendants(&mut tuned);
                }
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
                while let Some((id, job, heavy, slot)) = next_job(queue, wake, opts, sink) {
                    if let Some(f) = &opts.first_job_started {
                        f.store(true, Ordering::Relaxed);
                    }
                    let label = job.label();
                    let marker = opts.done_dir.as_ref().map(|d| d.join(job.marker()));
                    if marker.as_ref().is_some_and(|m| m.is_file()) {
                        events::info(sink, format!("{label}: already extracted by an earlier run, skipping"));
                        summary.lock().unwrap().skipped += 1;
                        drop(slot);
                        if heavy {
                            queue.lock().unwrap().heavy_running -= 1;
                            wake.notify_all();
                        }
                        continue;
                    }
                    sink(Event::JobStarted {
                        id,
                        label: label.clone(),
                        method: job.method(),
                        compressed: job.compsize(),
                        output: job.origsize(),
                    });
                    let proxies = ProxyUse::begin(proxy_codecs(&job, cfg), matches!(job.kind, JobKind::Unarc), &opts.cancel);
                    let started = Instant::now();
                    let res = match job.kind {
                        JobKind::Block(i) => run_block(&job.archive, &job.archive.blocks[i], cfg, opts, id, sink),
                        JobKind::Unarc => run_unarc(&job.archive, cfg, opts, id, sink),
                    };
                    drop(proxies);
                    let secs = started.elapsed().as_secs_f64();
                    let mut sm = summary.lock().unwrap();
                    match res {
                        Ok(r) => {
                            if let Some(m) = &marker
                                && r.problems.is_empty()
                            {
                                let _ = std::fs::create_dir_all(m.parent().unwrap());
                                let _ = std::fs::write(m, format!("{} files
", r.files_ok + r.files_unverified));
                            }
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
                    drop(slot);
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
    told_waiting: bool,
}

/// Next job this worker may start: the biggest pending one, except that heavy jobs wait while
/// `heavy_jobs` of them are already running (light ones may overtake them). None = all done.
/// A heavy job also needs one of the machine-wide slots (guard::try_heavy_slot), so separate
/// arc-unpack processes share the limit.
fn next_job(
    queue: &Mutex<Queue>,
    wake: &std::sync::Condvar,
    opts: &Options,
    sink: &Sink,
) -> Option<(usize, Job, bool, Option<crate::guard::HeavySlot>)> {
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
        let mut slot = None;
        let mut blocked_elsewhere = false;
        let pick = q.pending.iter().position(|(_, _, heavy)| {
            if !heavy || limit == 0 {
                return true;
            }
            if running >= limit {
                return false;
            }
            if slot.is_none() {
                slot = crate::guard::try_heavy_slot(limit);
                blocked_elsewhere |= slot.is_none();
            }
            slot.is_some()
        });
        if let Some(pos) = pick {
            let (id, job, heavy) = q.pending.remove(pos);
            if heavy {
                q.heavy_running += 1;
            }
            return Some((id, job, heavy, slot));
        }
        if blocked_elsewhere && !q.told_waiting {
            q.told_waiting = true;
            events::warn(sink, "another arc-unpack job is running a memory-heavy step; waiting for it to finish");
        }
        q = wake.wait_timeout(q, Duration::from_millis(250)).unwrap().0;
    }
}

/// Whether a job needs a memory-heavy slot (whole-archive unarc runs included, which load the
/// same codecs). What decides is the codec, not how it is run: the multi-threaded lolz
/// decoders (magic*, lolly*) size their thread count by a share of RAM, and srep keeps the data
/// it may need again in memory. The single-threaded lolzi/lolzx/lolz take what the stream says.
fn is_heavy(job: &Job, cfg: &Config) -> bool {
    let block = |b: &DataBlock| method_is_heavy(&b.method, b.origsize, cfg);
    match job.kind {
        JobKind::Unarc => job.archive.blocks.iter().any(block),
        JobKind::Block(i) => block(&job.archive.blocks[i]),
    }
}

fn method_is_heavy(chain: &str, block_size: u64, cfg: &Config) -> bool {
    method_is_heavy_with(chain, block_size, cfg, crate::memlimit::get())
}

/// `method_is_heavy` with the RAM limit (`limit`, bytes) given.
fn method_is_heavy_with(chain: &str, block_size: u64, cfg: &Config, limit: Option<u64>) -> bool {
    // magic*, lolly*: they size their threads from the free RAM (and sit inside 4x4 as "lolz").
    const HUNGRY: [&str; 2] = ["magic", "lolly"];
    let hungry = |s: &str| HUNGRY.iter().any(|h| s.to_ascii_lowercase().starts_with(h));
    let lolz_inside = |s: &str| s.to_ascii_lowercase().starts_with("lolz");
    // lolzi, lolzx, lolz: single-threaded decoders whose memory comes from the stream (about
    // 0.5 GB for d512; reverse-engineered), so several can run beside the heavy jobs.
    const SINGLE: [&str; 3] = ["lolzi", "lolzx", "lolz"];
    crate::method::parse_chain(chain).iter().any(|m| {
        let name = m.name.to_ascii_lowercase();
        if hungry(&name) || m.params.iter().any(|p| hungry(p) || lolz_inside(p)) {
            return true;
        }
        if SINGLE.contains(&name.as_str()) {
            // "d512": the dictionary in MB.
            let dict_mb = m.params.iter().find_map(|p| {
                let rest = p.strip_prefix('d')?;
                if rest.chars().all(|c| c.is_ascii_digit()) { rest.parse::<u64>().ok() } else { crate::method::parse_size(rest).map(|b| b >> 20) }
            });
            return limit.is_some() || dict_mb.is_some_and(|d| d >= 1024);
        }
        if name.starts_with("lolz") {
            return true;
        }
        if name == "srep" {
            // "mem228mb": how much the compressor says decompression needs. Without it, go by size.
            let declared = m.params.iter().find_map(|p| p.strip_prefix("mem").and_then(crate::method::parse_size));
            // With a RAM limit srep gets less memory and spills to one fixed-name temp file
            // (srep-virtual-memory.tmp) in a shared folder: two spilling at once corrupt each
            // other, so such blocks are heavy (and a limit runs heavy jobs one at a time).
            if let (Some(limit), Some(need)) = (limit, declared)
                && need > crate::memlimit::srep_mb(limit) << 20
            {
                return true;
            }
            return declared.map(|need| need > 1 << 30).unwrap_or(block_size > 2 << 30);
        }
        // Any other CLS codec has an unknown appetite: heavy when the block is big.
        let cls = cfg.resolve(&m.full()).is_ok_and(|st| Config::flatten(&st).iter().any(|s| matches!(s.codec, crate::config::Codec::Cls { .. })));
        cls && block_size > 1 << 30
    })
}

/// CLS codecs (or their helper executables) that hard-code "Global\" object names, which only
/// works with administrator rights.
fn codecs_needing_admin(jobs: &[Job], cfg: &Config) -> Vec<String> {
    let mut dlls = std::collections::BTreeSet::new();
    for job in jobs {
        match job.kind {
            JobKind::Block(i) => {
                let stages = cfg.resolve(&job.archive.blocks[i].method).unwrap_or_default();
                for st in Config::flatten(&stages) {
                    if let crate::config::Codec::Cls { dll, .. } = &st.codec {
                        dlls.insert(dll.clone());
                    }
                }
            }
            JobKind::Unarc => {
                // unarc.dll loads cls-<name>.dll for a method <name>; any word of the method
                // string can be one ("4x4:b128mb:mpzz+srep").
                for b in &job.archive.blocks {
                    for word in b.method.split(['+', ':']).map(str::trim).filter(|w| !w.is_empty()) {
                        let dll = cfg.codec_dir().join(format!("cls-{}.dll", word.to_ascii_lowercase()));
                        if dll.is_file() {
                            dlls.insert(dll);
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

/// CLS codecs that run their decoder in a helper program (cls-srep.dll + cls-srep_x64.exe) talk
/// to it through events and shared memory numbered 00-99, each instance taking the first free
/// number: two that start at the same moment can take the same one and mix their data.
/// arc-unpack's codec host starts them one at a time (guard::InitLock), but unarc.dll, which
/// loads them itself for whole-archive jobs, doesn't. So a whole-archive job has such a codec to
/// itself: it waits until no other job uses it, and jobs using it wait for it. (Assetto Corsa:
/// setup-fitgirl-08's srep stages started while 09 and 12 ran srep through unarc.dll, and 08's
/// output ended early, at a different file each time.) Per process: early extractions of later
/// archives run in the same process.
static PROXY_USE: Mutex<Vec<(String, usize, bool)>> = Mutex::new(Vec::new());
static PROXY_FREED: std::sync::Condvar = std::sync::Condvar::new();

/// Helper-program codecs that spill to fixed-name temp files in one shared folder: magic2l and
/// magicl write `<ldmfTempPath>temp_<worker>.ldmf` (fopen "w+b"); a second instance shows
/// "Can't create vm temp file!" and crashes. So one runs at a time.
const SPILLING_PROXIES: [&str; 2] = ["cls-magic2l", "cls-magicl"];

struct ProxyUse {
    /// Codec names, and whether this job holds each one exclusively.
    names: Vec<(String, bool)>,
}

impl ProxyUse {
    /// Waits until the codecs `names` are free for this job (`exclusive`: a whole-archive job,
    /// which holds them all exclusively; the spilling ones are always held exclusively).
    fn begin(names: Vec<String>, exclusive: bool, cancel: &AtomicBool) -> ProxyUse {
        let names: Vec<(String, bool)> =
            names.into_iter().map(|n| { let excl = exclusive || SPILLING_PROXIES.contains(&n.as_str()); (n, excl) }).collect();
        if names.is_empty() {
            return ProxyUse { names };
        }
        let mut using = PROXY_USE.lock().unwrap();
        loop {
            let free = names.iter().all(|(n, want_excl)| match using.iter().find(|u| &u.0 == n) {
                None => true,
                Some((_, shared, excl)) => !*excl && (!*want_excl || *shared == 0),
            });
            if free || cancel.load(Ordering::Relaxed) {
                break;
            }
            using = PROXY_FREED.wait_timeout(using, Duration::from_millis(250)).unwrap().0;
        }
        for (n, excl) in &names {
            if !using.iter().any(|u| &u.0 == n) {
                using.push((n.clone(), 0, false));
            }
            let u = using.iter_mut().find(|u| &u.0 == n).unwrap();
            if *excl {
                u.2 = true;
            } else {
                u.1 += 1;
            }
        }
        ProxyUse { names }
    }
}

impl Drop for ProxyUse {
    fn drop(&mut self) {
        if self.names.is_empty() {
            return;
        }
        let mut using = PROXY_USE.lock().unwrap();
        for (n, excl) in &self.names {
            if let Some(u) = using.iter_mut().find(|u| &u.0 == n) {
                if *excl {
                    u.2 = false;
                } else {
                    u.1 = u.1.saturating_sub(1);
                }
            }
        }
        using.retain(|u| u.1 > 0 || u.2);
        PROXY_FREED.notify_all();
    }
}

/// The helper-program CLS codecs a job uses (see `ProxyUse`), by lowercase DLL name.
fn proxy_codecs(job: &Job, cfg: &Config) -> Vec<String> {
    let mut dlls = Vec::new();
    match job.kind {
        JobKind::Block(i) => {
            let stages = cfg.resolve(&job.archive.blocks[i].method).unwrap_or_default();
            for st in Config::flatten(&stages) {
                if let crate::config::Codec::Cls { dll, .. } = &st.codec {
                    dlls.push(dll.clone());
                }
            }
        }
        JobKind::Unarc => {
            for b in &job.archive.blocks {
                for word in b.method.split(['+', ':']).map(str::trim).filter(|w| !w.is_empty()) {
                    let dll = cfg.codec_dir().join(format!("cls-{}.dll", word.to_ascii_lowercase()));
                    if dll.is_file() {
                        dlls.push(dll);
                    }
                }
            }
        }
    }
    let mut names: Vec<String> = dlls
        .iter()
        .filter(|dll| {
            let stem = dll.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            ["x64", "x86"].iter().any(|a| dll.with_file_name(format!("{stem}_{a}.exe")).is_file())
        })
        .filter_map(|dll| dll.file_stem().map(|s| s.to_string_lossy().to_lowercase()))
        .collect();
    names.sort();
    names.dedup();
    names
}

struct BlockResult {
    files_ok: u64,
    files_unverified: u64,
    problems: Vec<String>,
    codecs: Vec<CodecCpu>,
}

/// Counts decoded bytes coming out of the last stage (for the stall watchdog), and reports
/// them as output progress every few MB.
struct CountOut {
    inner: Stream,
    n: Arc<AtomicU64>,
    id: usize,
    sink: Sink,
    reported: u64,
}
impl Read for CountOut {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let k = self.inner.read(buf)?;
        let n = self.n.fetch_add(k as u64, Ordering::Relaxed) + k as u64;
        if n >= self.reported + (4 << 20) || (k == 0 && n > self.reported) {
            self.reported = n;
            (self.sink)(Event::JobOutput { id: self.id, done: n });
        }
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
    let output: Stream = Box::new(CountOut { inner: output, n: produced.clone(), id, sink: sink.clone(), reported: 0 });

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
    let split = split(output, entries, opts, archive.checksum);
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
/// Job ids are unique per process, so the events of extractions running side by side (the
/// install script's early extractions) never mix.
pub fn next_job_id() -> usize {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// MD5 of a file on disk.
pub fn md5_of_file(path: &Path) -> std::io::Result<[u8; 16]> {
    let mut f = File::open(path)?;
    let mut ctx = md5::Context::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(ctx.finalize().0);
        }
        ctx.consume(&buf[..n]);
    }
}

/// Hashes the files of `entries` under `out` that the MD5 list names and that haven't been
/// checked yet, on a few threads, and records the outcome in the list.
fn hash_written(list: &Md5List, out: &Path, prefix: &str, entries: &[Entry], cancel: &AtomicBool) {
    let todo: Vec<(&ExpectedMd5, PathBuf)> = entries
        .iter()
        .filter(|e| !e.is_dir)
        .filter_map(|e| {
            let x = list.get(&md5_key(&format!("{prefix}{}", e.path)))?;
            (!x.checked.load(Ordering::Relaxed)).then_some(())?;
            Some((x, safe_join(out, &e.path).ok()?))
        })
        .collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 4);
    std::thread::scope(|s| {
        for _ in 0..threads.min(todo.len()) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= todo.len() || cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    let (x, path) = &todo[i];
                    // A file that can't be read stays unchecked: the final check reports it.
                    if let Ok(got) = md5_of_file(path) {
                        x.matched.store(got == x.md5, Ordering::Relaxed);
                        x.checked.store(true, Ordering::Relaxed);
                    }
                }
            });
        }
    });
}

struct RemoveDirOnDrop(PathBuf);

impl Drop for RemoveDirOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Mirrors `src` into `dst` with hard links (copies where linking fails), for a private working
/// folder. Gives up on folders with more than a few thousand files (the shared one is used then).
fn mirror_codecs(src: &Path, dst: &Path) -> Result<PathBuf> {
    fn walk(src: &Path, dst: &Path, budget: &mut usize) -> Result<()> {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            anyhow::ensure!(*budget > 0, "too many files to mirror");
            *budget -= 1;
            let to = dst.join(e.file_name());
            if e.file_type()?.is_dir() {
                walk(&e.path(), &to, budget)?;
            } else if std::fs::hard_link(e.path(), &to).is_err() {
                std::fs::copy(e.path(), &to)?;
            }
        }
        Ok(())
    }
    let mut budget = 4000;
    walk(src, dst, &mut budget)?;
    Ok(dst.to_path_buf())
}

fn run_unarc(archive: &Archive, cfg: &Config, opts: &Options, id: usize, sink: &Sink) -> Result<BlockResult> {
    use std::io::BufRead;
    let dll = cfg.unarc_dll().context("no unarc configured")?;
    let host = stage::helper_exe(&["unarc-host-x86.exe"], &opts.host_dir)?;
    let abs = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    // A work folder per job: FreeArc's external codecs use fixed temp-file names in it, so two
    // unarc jobs sharing one would overwrite each other's files.
    let work = opts.temp_dir.join(format!("unarc-{}-{}", std::process::id(), stage::unique()));
    std::fs::create_dir_all(&work).with_context(|| format!("create {}", work.display()))?;
    let _work_cleanup = RemoveDirOnDrop(work.clone());
    // unarc.dll starts its external codecs (rz, precomp, ...) in its current folder, and the CLS
    // proxies it loads start their helpers (cls-srep_x64.exe, ...) in the proxy's own folder. Some
    // keep fixed-name temp files there (the Razor rz-1.03.7-stdio build, precomp's ~temp files):
    // two unarc jobs sharing the codec folder overwrite each other's (a CRC error in a file). So
    // each job runs from a private copy of the codec folder made of hard links: unarc.dll, the
    // proxies, their helpers and the codecs. CreateProcess finds unarc's external codecs (rzw.exe,
    // xtool\...) relative to the current folder.
    let (codecs, dll) = match mirror_codecs(&cfg.codec_dir(), &work.join("codecs")) {
        Ok(private) => {
            let dll = dll.file_name().map(|n| private.join(n)).filter(|p| p.is_file()).unwrap_or(dll);
            (private, dll)
        }
        Err(_) => (cfg.codec_dir(), dll),
    };
    let mut cmd = std::process::Command::new(host);
    cmd.arg(&dll).arg(if opts.out_dir.is_some() { "x" } else { "t" }).arg(abs(&archive.path));
    cmd.arg(if opts.overwrite { "-o+" } else { "-o-" });
    if let Some(ini) = cfg.arc_ini_path() {
        cmd.arg(format!("-cfg{}", abs(&ini).display()));
    }
    if let Some(out) = &opts.out_dir {
        cmd.arg(format!("-dp{}", abs(out).display()));
    }
    cmd.arg(format!("-w{}", abs(&work).display()));
    cmd.current_dir(&codecs);
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    stage::apply_child_flags(&mut cmd, opts.tune);
    let child = cmd.spawn().context("start unarc-host-x86.exe")?;
    let job = crate::perf::CpuJob::track(&child);
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
    // Also stopped when nothing moves on disk for the stall timeout (a hung external codec).
    let finished = Arc::new(AtomicBool::new(false));
    let stalled = Arc::new(Mutex::new(None::<String>));
    let canceller = {
        let (child, finished, cancel, stalled) = (child.clone(), finished.clone(), opts.cancel.clone(), stalled.clone());
        let mut stall = crate::guard::StallCheck::new(opts.stall_timeout);
        std::thread::spawn(move || {
            while !finished.load(Ordering::Relaxed) {
                if cancel.load(Ordering::Relaxed) {
                    let _ = child.lock().unwrap().kill();
                    return;
                }
                if stall.stalled() {
                    let running = crate::guard::running_programs().join(", ");
                    *stalled.lock().unwrap() = Some(format!(
                        "unarc.dll made no progress for {} s (nothing read or written; still running: {running}): stopped it",
                        stall.limit_secs()
                    ));
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
        if let Some(mb) = line.strip_prefix("write ").and_then(|v| v.trim().parse::<u64>().ok()) {
            sink(Event::JobOutput { id, done: mb << 20 });
        }
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
    let cpu = job.as_ref().and_then(|j| j.cpu_seconds()).or_else(|| crate::perf::cpu_seconds(&child.lock().unwrap()));
    if opts.cancel.load(Ordering::Relaxed) {
        bail!("cancelled");
    }
    if let Some(why) = stalled.lock().unwrap().take() {
        bail!("{why}");
    }
    if !status.success() {
        bail!("unarc.dll failed ({status}): {}", errors.trim());
    }
    sink(Event::JobProgress { id, done: total });
    let mut problems = Vec::new();
    let reported = unarc_errors(&errors);
    if !reported.is_empty() {
        problems.push(format!("unarc.dll reported: {}", reported.join("; ")));
    }
    // unarc.dll wrote these files itself, so they weren't hashed on the way to disk: check the
    // listed ones now, while other archives are still decoding, instead of in a pass at the end.
    if let (Some(list), Some(out)) = (&opts.md5, &opts.out_dir) {
        hash_written(list, out, &opts.md5_prefix, &archive.entries, &opts.cancel);
    }
    let files = archive.entries.iter().filter(|e| !e.is_dir).count() as u64;
    let codecs = vec![CodecCpu { label: "unarc.dll".into(), cpu_secs: cpu, wall_secs: wall }];
    Ok(BlockResult { files_ok: files, files_unverified: 0, problems, codecs })
}

/// The errors unarc.dll itself reported. External codecs it runs (bcm, rz, ...) share the
/// helper's stderr and print progress there ("Decompressing $$arcpackedfile$$.tmp", "CRC = ...");
/// that chatter isn't an error when unarc finished successfully.
fn unarc_errors(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("unarc error") || l.starts_with("unarc-host:"))
        .collect()
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
fn split(mut stream: Stream, entries: &[Entry], opts: &Options, checksum: Checksum) -> Result<BlockResult> {
    let verify = checksum.verifies();
    let mut problems = Vec::new();
    let mut files_ok = 0u64;
    let mut files_unverified = 0u64;

    let Some(out_dir) = &opts.out_dir else {
        let mut buf = vec![0u8; CHUNK];
        for e in entries.iter().filter(|e| !e.is_dir) {
            let mut h = checksum.hasher();
            let expected = expected_md5(opts, &e.path);
            let mut m = expected.map(|_| md5::Context::new());
            let mut left = e.size;
            while left > 0 {
                let n = (left as usize).min(CHUNK);
                stream.read_exact(&mut buf[..n]).with_context(|| short_msg(e, left))?;
                h.update(&buf[..n]);
                if let Some(m) = &mut m {
                    m.consume(&buf[..n]);
                }
                left -= n as u64;
            }
            let crc = h.finalize();
            if let (Some(x), Some(m)) = (expected, m) {
                check_md5(x, m);
            }
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
                    match write_file(&job, overwrite, checksum, expected_md5(opts, &job.rel)) {
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

fn expected_md5<'a>(opts: &'a Options, rel: &str) -> Option<&'a ExpectedMd5> {
    opts.md5.as_ref()?.get(&md5_key(&format!("{}{rel}", opts.md5_prefix)))
}

/// Records the hash of a file as written. A mismatch is not an error here: a later step (a
/// later archive, a repack tool) may still replace the file, so mismatches are re-checked
/// from disk once everything is done.
fn check_md5(expected: &ExpectedMd5, ctx: md5::Context) {
    let got = ctx.finalize().0;
    expected.matched.store(got == expected.md5, Ordering::Relaxed);
    expected.checked.store(true, Ordering::Relaxed);
}

fn write_file(job: &FileJob, overwrite: bool, checksum: Checksum, expected: Option<&ExpectedMd5>) -> Result<()> {
    let verify = checksum.verifies();
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true);
    if overwrite { opts.create(true).truncate(true) } else { opts.create_new(true) };
    let file = opts.open(&job.path).with_context(|| format!("create {}", job.path.display()))?;
    if job.size > 0 {
        file.set_len(job.size)?; // reserve space up front: fewer fragments, early "disk full"
    }
    let mut w = std::io::BufWriter::with_capacity(1 << 20, file);
    let mut h = checksum.hasher();
    let mut m = expected.map(|_| md5::Context::new());
    let mut written = 0u64;
    for chunk in job.chunks.iter() {
        h.update(&chunk);
        if let Some(m) = &mut m {
            m.consume(&chunk);
        }
        w.write_all(&chunk)?;
        written += chunk.len() as u64;
    }
    ensure!(written == job.size, "only {written} of {} bytes arrived", job.size);
    let file = w.into_inner().map_err(|e| e.into_error())?;
    let _ = file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(job.mtime as u64));
    let crc = h.finalize();
    ensure!(!verify || crc == job.crc, "CRC mismatch (got {crc:08x}, want {:08x})", job.crc);
    if let (Some(x), Some(m)) = (expected, m) {
        check_md5(x, m);
    }
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
    fn codec_chatter_is_not_an_unarc_error() {
        let bcm = "Decompressing $$arcpackedfile$$.tmp:\n518979242 -> 519055360 in 11.2s\nCRC = 3f1c0a9e\n";
        assert!(unarc_errors(bcm).is_empty());
        let both = format!("{bcm}unarc error -2: CRC error in data\n");
        assert_eq!(unarc_errors(&both), vec!["unarc error -2: CRC error in data"]);
    }

    #[test]
    fn heavy_is_decided_by_codec() {
        let cfg = Config::default();
        let gb = 1u64 << 30;
        // The multi-threaded lolz decoders are always heavy, also as the inner method of 4x4.
        assert!(method_is_heavy("xt3u+srep:m3yf+magic2", 6 * gb, &cfg));
        assert!(method_is_heavy("precomp+lollypop2", 100 << 20, &cfg));
        assert!(method_is_heavy("4x4:b64mb:lolz", gb, &cfg));
        // The single-threaded ones only with a big dictionary or a RAM limit (Assetto Corsa's
        // lolzi blocks run beside the heavy jobs).
        assert!(!method_is_heavy_with("srep:m3f:mem228mb:l256+lolzi:d512:al1:mc1023:tt8", 6 * gb, &cfg, None));
        assert!(!method_is_heavy_with("lolz:d512:al1:mc1023:tt2:cm1:pa4", gb, &cfg, None));
        assert!(method_is_heavy_with("lolzi:d1024:al1", gb, &cfg, None));
        assert!(method_is_heavy_with("lolzi:d512:al1:mc1023:tt8", gb, &cfg, Some(8 * gb)));
        // With a RAM limit, srep blocks that would spill to its one temp file are heavy.
        assert!(method_is_heavy_with("srep:m3f:mem900mb+lzma", gb / 2, &cfg, Some(2 * gb)));
        assert!(!method_is_heavy_with("srep:m3f:mem228mb+lzma", gb / 2, &cfg, Some(2 * gb)));
        // srep says what it needs; without that, only big blocks count.
        assert!(!method_is_heavy("4x4:b128mb:mpzz+srep:m3f:mem228mb+4x4:tor:16mb:c3", gb / 2, &cfg));
        assert!(method_is_heavy("srep:m3f:mem3gb+lzma", gb / 2, &cfg));
        assert!(!method_is_heavy("srep:m3zf+dispack070+delta+rzwb", 111 << 20, &cfg));
        assert!(method_is_heavy("srep:m3f+rzs", 5 * gb, &cfg));
        // Plain FreeArc methods never are.
        assert!(!method_is_heavy("dispack070+delta+lzma:96mb", 8 * gb, &cfg));
        assert!(!method_is_heavy("storing", 50 * gb, &cfg));
    }

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
