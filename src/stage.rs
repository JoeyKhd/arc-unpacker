//! Turns a resolved method chain into running decode stages connected by pipes.
//! Every stage runs concurrently: built-ins on their own thread, codecs as child processes.

use crate::config::{Codec, Stage};
use crate::pipe::{PipeWriter, pipe};
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

pub type Stream = Box<dyn Read + Send>;

const IO_BUF: usize = 1 << 20;

pub struct ChildInfo {
    pub label: String,
    pub child: Arc<Mutex<Child>>,
    pub started: Instant,
    /// CPU accounting for the process and its descendants.
    pub job: Option<crate::perf::CpuJob>,
}

type Workers = Vec<(String, JoinHandle<Result<()>>)>;

#[derive(Default, Clone)]
pub struct Registry {
    pub children: Arc<Mutex<Vec<ChildInfo>>>,
    pub workers: Arc<Mutex<Workers>>,
    /// Set once the block is being torn down; later failures are consequences, not causes.
    pub aborted: Arc<std::sync::atomic::AtomicBool>,
}

impl Registry {
    /// Runs a stage thread. If it fails, every codec process of this block is killed: otherwise
    /// a dead stage leaves its neighbours blocked on full or never-closing pipes forever.
    fn worker(&self, label: String, f: impl FnOnce() -> Result<()> + Send + 'static) {
        let reg = self.clone();
        let run = move || {
            let r = f();
            if r.is_err() {
                if reg.is_aborted() {
                    return r.map_err(|e| e.context("stopped after another stage failed"));
                }
                reg.kill_all();
            }
            r
        };
        let h = std::thread::Builder::new().name(label.clone()).spawn(run).expect("spawn thread");
        self.workers.lock().unwrap().push((label, h));
    }

    pub fn kill_all(&self) {
        self.aborted.store(true, std::sync::atomic::Ordering::Relaxed);
        for c in self.children.lock().unwrap().iter() {
            if let Ok(mut ch) = c.child.try_lock() {
                let _ = ch.kill();
            }
        }
    }

    pub fn is_aborted(&self) -> bool {
        self.aborted.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Joins every stage thread (including ones started by other stage threads) and returns their errors.
    pub fn join(&self) -> Vec<anyhow::Error> {
        let mut errs = Vec::new();
        loop {
            let workers = std::mem::take(&mut *self.workers.lock().unwrap());
            if workers.is_empty() {
                return errs;
            }
            for (label, h) in workers {
                match h.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => errs.push(e.context(label)),
                    Err(_) => errs.push(anyhow!("{label}: thread panicked")),
                }
            }
        }
    }

    /// (label, cpu seconds, wall seconds) per codec process.
    /// Processes with the same label (one per chunk of a split xtool) are added up.
    pub fn cpu_report(&self) -> Vec<(String, Option<f64>, f64)> {
        let mut out: Vec<(String, Option<f64>, f64)> = Vec::new();
        for c in self.children.lock().unwrap().iter() {
            let ch = c.child.lock().unwrap();
            let cpu = c.job.as_ref().and_then(|j| j.cpu_seconds()).or_else(|| crate::perf::cpu_seconds(&ch));
            let wall = c.started.elapsed().as_secs_f64();
            match out.iter_mut().find(|o| o.0 == c.label) {
                Some(o) => {
                    o.1 = match (o.1, cpu) {
                        (Some(a), Some(b)) => Some(a + b),
                        (a, b) => a.or(b),
                    };
                    o.2 = o.2.max(wall);
                }
                None => out.push((c.label.clone(), cpu, wall)),
            }
        }
        out
    }
}

#[derive(Clone)]
pub struct Ctx {
    pub temp_dir: PathBuf,
    pub host_dir: PathBuf,
    pub tune: bool,
    pub reg: Registry,
}

/// Builds the decode pipeline. `stages` are in compression order; `final_size` is the size of
/// the fully decoded block (known from the directory).
pub fn build(stages: &[Stage], input: Stream, final_size: u64, ctx: &Ctx) -> Result<Stream> {
    let mut stream = input;
    let mut rank = 0;
    for (i, st) in stages.iter().enumerate().rev() {
        let label = st.method.full();
        stream = match &st.codec {
            Codec::Storing => stream,
            Codec::Lzma(params) => {
                let params = *params;
                let (mut w, r) = pipe(8);
                let size = (i == 0).then_some(final_size);
                ctx.reg.worker(format!("lzma ({label})"), move || {
                    let mut input = BufReader::with_capacity(IO_BUF, stream);
                    crate::lzma::decode(&params, &mut input, &mut w, size)?;
                    w.finish()?;
                    Ok(())
                });
                Box::new(r)
            }
            Codec::External(e) => {
                let spec = ExtSpec {
                    label,
                    argv: e.argv.clone(),
                    workdir: e.workdir.clone(),
                    input_file: e.input_file,
                    output_file: e.output_file,
                    header: e.header,
                    performance_cores: e.performance_cores,
                    rank,
                    datafile: e.datafile.clone(),
                };
                rank += 1;
                external(spec, stream, ctx)?
            }
            Codec::FourByFour(inner) => four_by_four(inner.clone(), stream, ctx, label)?,
            Codec::Cls { dll, method, performance_cores } => {
                let host = cls_host_for(dll, &ctx.host_dir)?;
                let spec = ExtSpec {
                    label: format!("{label} [{}]", dll.file_name().unwrap_or_default().to_string_lossy()),
                    argv: vec![host.to_string_lossy().into_owned(), dll.to_string_lossy().into_owned(), method.clone()],
                    workdir: dll.parent().map(Path::to_path_buf),
                    input_file: false,
                    output_file: false,
                    header: false,
                    performance_cores: *performance_cores,
                    rank,
                    datafile: None,
                };
                rank += 1;
                external(spec, stream, ctx)?
            }
        };
    }
    Ok(stream)
}

/// FreeArc's 4x4 (C_4x4.cpp): an int32 version (0), then blocks of [original size int32 (-1 =
/// stored as is), packed size int32, packed data], each compressed on its own with the inner
/// method. Blocks are decoded through the inner chain several at a time and written in order;
/// each must come out at its recorded size.
fn four_by_four(inner: Vec<Stage>, input: Stream, ctx: &Ctx, label: String) -> Result<Stream> {
    use std::collections::BTreeMap;
    let (mut w, r) = pipe(8);
    let ctx2 = ctx.clone();
    ctx.reg.worker(label.clone(), move || {
        let mut input = input;
        let mut v = [0u8; 4];
        if !read_full(&mut input, &mut v)? {
            w.finish()?;
            return Ok(());
        }
        ensure!(i32::from_le_bytes(v) == 0, "4x4: unknown format version {}", i32::from_le_bytes(v));
        type Done = (Mutex<BTreeMap<u64, std::result::Result<Vec<u8>, String>>>, std::sync::Condvar);
        let done: Arc<Done> = Arc::new((Mutex::new(BTreeMap::new()), std::sync::Condvar::new()));
        let mut workers = Vec::new();
        let mut jobs: Option<crossbeam_channel::Sender<(u64, i32, Vec<u8>)>> = None;
        let (mut next_out, mut index, mut ahead) = (0u64, 0u64, 1usize);
        let mut reserved = ReservedMemory::take(0);
        // Writes finished blocks in order; with `wait`, until block `upto` has been written.
        let emit = |next_out: &mut u64, upto: u64, w: &mut crate::pipe::PipeWriter| -> Result<()> {
            loop {
                let block = {
                    let mut d = done.0.lock().unwrap();
                    loop {
                        if let Some(b) = d.remove(next_out) {
                            break Some(b);
                        }
                        if *next_out >= upto {
                            break None;
                        }
                        d = done.1.wait(d).unwrap();
                    }
                };
                match block {
                    Some(Ok(data)) => {
                        w.write_all(&data).context("4x4 output")?;
                        *next_out += 1;
                    }
                    Some(Err(e)) => bail!("4x4 block {next_out}: {e}"),
                    None => return Ok(()),
                }
            }
        };
        let mut head = [0u8; 8];
        while read_full(&mut input, &mut head)? {
            let orig = i32::from_le_bytes(head[..4].try_into().unwrap());
            let packed = i32::from_le_bytes(head[4..].try_into().unwrap());
            ensure!(packed >= 0 && orig >= -1, "4x4: bad block header");
            let mut data = vec![0u8; packed as usize];
            ensure!(read_full(&mut input, &mut data)?, "4x4: stream cut short");
            if jobs.is_none() {
                // Blocks in flight: as many as cores, as far as free RAM allows (see
                // `four_by_four_workers`), reserved so that 4x4 streams starting together
                // don't all count the same free memory.
                let block = (orig.max(packed) as u64).max(1 << 20);
                let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
                let (available, _) = crate::guard::memory();
                let others = FOUR_BY_FOUR_RESERVED.load(std::sync::atomic::Ordering::Relaxed);
                ahead = four_by_four_workers(block, &inner, available.saturating_sub(others), crate::memlimit::get(), threads);
                reserved = ReservedMemory::take(ahead as u64 * per_block_memory(block, &inner));
                let (tx, rx) = crossbeam_channel::bounded::<(u64, i32, Vec<u8>)>(ahead);
                for _ in 0..ahead {
                    let (rx, done, inner, ctx) = (rx.clone(), done.clone(), inner.clone(), ctx2.clone());
                    workers.push(std::thread::spawn(move || {
                        for (i, orig, data) in rx.iter() {
                            let r = if orig == -1 {
                                Ok(data)
                            } else {
                                decode_block(&inner, data, orig as usize, &ctx).map_err(|e| format!("{e:#}"))
                            };
                            done.0.lock().unwrap().insert(i, r);
                            done.1.notify_all();
                        }
                    }));
                }
                jobs = Some(tx);
            }
            // Keep at most `ahead` blocks beyond the one being written.
            if index >= next_out + 2 * ahead as u64 {
                emit(&mut next_out, index - 2 * ahead as u64 + 1, &mut w)?;
            }
            jobs.as_ref().unwrap().send((index, orig, data)).map_err(|_| anyhow!("4x4 workers stopped"))?;
            index += 1;
            emit(&mut next_out, 0, &mut w)?;
        }
        drop(jobs);
        let r = emit(&mut next_out, index, &mut w);
        for h in workers {
            let _ = h.join();
        }
        drop(reserved);
        r?;
        w.finish()?;
        Ok(())
    });
    Ok(Box::new(r))
}

/// Memory reserved by the running 4x4 streams for their blocks in flight.
static FOUR_BY_FOUR_RESERVED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A share of `FOUR_BY_FOUR_RESERVED`, given back when dropped.
struct ReservedMemory(u64);

impl ReservedMemory {
    fn take(bytes: u64) -> ReservedMemory {
        FOUR_BY_FOUR_RESERVED.fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
        ReservedMemory(bytes)
    }
}

impl Drop for ReservedMemory {
    fn drop(&mut self) {
        FOUR_BY_FOUR_RESERVED.fetch_sub(self.0, std::sync::atomic::Ordering::Relaxed);
    }
}

/// What one 4x4 block in flight takes: its packed and decoded buffers here, plus what the
/// inner codec needs for it. The built-in LZMA needs its dictionary (at most the block). A codec
/// program or CLS DLL's needs aren't declared for decoding, so it gets four times the block (bcm,
/// a BWT decoder, takes about five times its block, counting our input buffer).
fn per_block_memory(block: u64, inner: &[Stage]) -> u64 {
    let foreign = inner.iter().any(|s| matches!(s.codec, Codec::External(_) | Codec::Cls { .. } | Codec::FourByFour(_)));
    2 * block + if foreign { 4 * block } else { block }
}

/// How many 4x4 blocks to decode at once: one per logical CPU (as FreeArc's own 4x4 does), as
/// far as half the free RAM (`available`, less what other 4x4 streams reserved) allows. With a
/// RAM limit: within an eighth of it, at most 1.5 GiB. (It used to be a fixed 1.5 GiB of buffers,
/// at most 16: 3 blocks at a time for 256 MB blocks, which held Batman: Arkham City's bcm
/// archive at 2.8 cores for 420 s.)
fn four_by_four_workers(block: u64, inner: &[Stage], available: u64, limit: Option<u64>, threads: usize) -> usize {
    let per = per_block_memory(block, inner).max(1);
    let budget = match limit {
        Some(l) => (l / 8).min(3 << 29),
        None => available / 2,
    };
    (budget / per).clamp(1, threads.max(1) as u64) as usize
}

/// One 4x4 block through the inner chain; it must decode to exactly `size` bytes.
fn decode_block(inner: &[Stage], data: Vec<u8>, size: usize, ctx: &Ctx) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(size);
    let mut s = build(inner, Box::new(io::Cursor::new(data)), size as u64, ctx)?;
    s.read_to_end(&mut out).context("decoding a 4x4 block")?;
    ensure!(out.len() == size, "4x4 block decoded to {} bytes, expected {size}", out.len());
    Ok(out)
}

/// Fills `buf`; false at a clean end of stream before the first byte.
fn read_full(input: &mut Stream, buf: &mut [u8]) -> io::Result<bool> {
    let mut got = 0;
    while got < buf.len() {
        match input.read(&mut buf[got..])? {
            0 if got == 0 => return Ok(false),
            0 => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "stream cut short")),
            n => got += n,
        }
    }
    Ok(true)
}

/// Picks cls-host-x86.exe or cls-host-x64.exe to match the DLL's architecture.
fn cls_host_for(dll: &Path, host_dir: &Path) -> Result<PathBuf> {
    let arch = pe_machine(dll).with_context(|| format!("read PE header of {}", dll.display()))?;
    let (name, native) = match arch {
        0x14c => ("cls-host-x86.exe", cfg!(target_arch = "x86")),
        0x8664 => ("cls-host-x64.exe", cfg!(target_arch = "x86_64")),
        m => bail!("{}: unsupported machine type {m:#x}", dll.display()),
    };
    let mut names = vec![name];
    if native {
        names.push("cls-host.exe");
    }
    helper_exe(&names, host_dir).with_context(|| format!("needed for {}", dll.display()))
}

/// Finds a helper program (cls-host-*, unarc-host-*) next to arc-unpack.exe, or in
/// ARC_UNPACK_HOST_DIR when set.
pub fn helper_exe(names: &[&str], host_dir: &Path) -> Result<PathBuf> {
    crate::helpers::find(names, host_dir)
}

pub fn pe_machine(path: &Path) -> Result<u16> {
    use std::io::Seek;
    let mut f = std::fs::File::open(path)?;
    let mut dos = [0u8; 64];
    f.read_exact(&mut dos)?;
    anyhow::ensure!(&dos[..2] == b"MZ", "not a PE file");
    let off = u32::from_le_bytes(dos[0x3c..0x40].try_into().unwrap()) as u64;
    f.seek(io::SeekFrom::Start(off))?;
    let mut pe = [0u8; 6];
    f.read_exact(&mut pe)?;
    anyhow::ensure!(&pe[..4] == b"PE\0\0", "bad PE signature");
    Ok(u16::from_le_bytes([pe[4], pe[5]]))
}

#[derive(Clone)]
struct ExtSpec {
    label: String,
    argv: Vec<String>,
    /// None: a fresh empty folder per process (see `config::External::workdir`).
    workdir: Option<PathBuf>,
    input_file: bool,
    output_file: bool,
    header: bool,
    performance_cores: bool,
    /// Position among this chain's codec processes in decode order (0 reads the archive).
    rank: usize,
    /// The output file the decoder creates in its working folder (see `config::External`).
    datafile: Option<String>,
}

fn command(spec: &ExtSpec, input_path: &Path, output_path: &Path, workdir: &Path, tune: bool) -> Command {
    let sub = |a: &str| a.replace("{input}", &input_path.to_string_lossy()).replace("{output}", &output_path.to_string_lossy());
    let mut cmd = Command::new(sub(&spec.argv[0]));
    cmd.args(spec.argv[1..].iter().map(|a| sub(a)));
    if !workdir.as_os_str().is_empty() {
        cmd.current_dir(workdir);
    }
    apply_child_flags(&mut cmd, tune);
    cmd
}

/// No console window for codec processes; above-normal priority when tuning.
pub fn apply_child_flags(cmd: &mut Command, tune: bool) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let prio = if tune { crate::perf::PRIORITY_FLAGS } else { 0 };
        cmd.creation_flags(CREATE_NO_WINDOW | prio);
    }
    #[cfg(not(windows))]
    let _ = (cmd, tune);
}

struct Spawned {
    child: Arc<Mutex<Child>>,
    stdin: Option<std::process::ChildStdin>,
    stdout: Option<ChildStdout>,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
}

fn spawn_child(spec: &ExtSpec, mut cmd: Command, ctx: &Ctx) -> Result<Spawned> {
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| {
        let what = format!("{}: failed to start {:?}", spec.label, spec.argv[0]);
        match crate::helpers::missing_hint(Path::new(&spec.argv[0])) {
            Some(hint) if e.kind() == std::io::ErrorKind::NotFound => anyhow::anyhow!("{what}: {hint}"),
            _ => anyhow::Error::new(e).context(what),
        }
    })?;
    let job = crate::perf::CpuJob::track(&child);
    if ctx.tune {
        crate::perf::tune_child(&child, spec.performance_cores);
        crate::perf::register_stage(child.id(), spec.rank);
    }
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let tail = Arc::new(Mutex::new(Vec::new()));
    if let Some(mut err) = child.stderr.take() {
        let tail = tail.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = err.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let mut t = tail.lock().unwrap();
                t.extend_from_slice(&buf[..n]);
                if t.len() > 8192 {
                    let cut = t.len() - 4096;
                    t.drain(..cut);
                }
            }
        });
    }
    let child = Arc::new(Mutex::new(child));
    ctx.reg.children.lock().unwrap().push(ChildInfo { label: spec.label.clone(), child: child.clone(), started: Instant::now(), job });
    Ok(Spawned { child, stdin, stdout, stderr_tail: tail })
}

fn exit_error(label: &str, status: std::process::ExitStatus, tail: &Mutex<Vec<u8>>) -> anyhow::Error {
    let t = tail.lock().unwrap();
    let msg = String::from_utf8_lossy(&t);
    let msg = msg.trim();
    if msg.is_empty() { anyhow!("{label} exited with {status}") } else { anyhow!("{label} exited with {status}: {msg}") }
}

/// Copies `input` into a child's stdin. A child that stops reading early (BrokenPipe) is not an
/// error by itself: if its output comes up short, the size/CRC checks downstream catch it.
fn feed(mut input: Stream, mut stdin: impl Write) -> Result<()> {
    let mut buf = vec![0u8; IO_BUF];
    loop {
        let n = input.read(&mut buf).context("reading stage input")?;
        if n == 0 {
            return Ok(());
        }
        match stdin.write_all(&buf[..n]) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
            Err(e) => return Err(e).context("writing to codec stdin"),
        }
    }
}

/// Child stdout that checks the exit status when it reaches EOF.
struct ChildOut {
    out: ChildStdout,
    child: Arc<Mutex<Child>>,
    label: String,
    tail: Arc<Mutex<Vec<u8>>>,
    checked: bool,
    reg: Registry,
    /// The process's own working folder, removed with this stream.
    _own_dir: Option<RemoveDir>,
}

impl Read for ChildOut {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.out.read(buf)?;
        if n == 0 && !buf.is_empty() && !self.checked {
            self.checked = true;
            let status = self.child.lock().unwrap().wait()?;
            if !status.success() {
                let msg = if self.reg.is_aborted() {
                    format!("{} stopped after another stage failed", self.label)
                } else {
                    format!("{:#}", exit_error(&self.label, status, &self.tail))
                };
                self.reg.kill_all();
                return Err(io::Error::other(msg));
            }
        }
        Ok(n)
    }
}

/// Debugging aid: `ARC_UNPACK_TEE_XTOOL=<file>` also writes xtool's input stream to a file, so
/// xtool can be replayed on its own afterwards.
struct Tee {
    inner: Stream,
    copy: io::BufWriter<std::fs::File>,
}

impl Read for Tee {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n == 0 {
            self.copy.flush()?;
        } else {
            self.copy.write_all(&buf[..n])?;
        }
        Ok(n)
    }
}

/// Benchmarking aids: a copy of a codec's input, so the codec can be replayed alone.
/// `ARC_UNPACK_TEE_XTOOL=<file>`: xtool's input. `ARC_UNPACK_TEE_DIR=<dir>`: every codec's
/// input, as `<dir>\<stage rank>-<label>-<id>.in` (rank 0 = the first decoder of the chain);
/// `ARC_UNPACK_TEE_ONLY=<text>` limits that to codecs whose label contains the text.
fn tee_if_requested(spec: &ExtSpec, input: Stream) -> Result<Stream> {
    let is_xtool = Path::new(&spec.argv[0]).file_name().is_some_and(|n| n.to_string_lossy().to_ascii_lowercase().starts_with("xtool"));
    // `ARC_UNPACK_TEE_ONLY=<text>`: only codecs whose label contains it.
    let wanted = std::env::var("ARC_UNPACK_TEE_ONLY").map_or(true, |only| spec.label.contains(only.as_str()));
    let path = match (std::env::var_os("ARC_UNPACK_TEE_DIR"), std::env::var_os("ARC_UNPACK_TEE_XTOOL")) {
        (Some(_), _) if !wanted => return Ok(input),
        (Some(dir), _) => {
            let name: String = spec.label.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
            PathBuf::from(dir).join(format!("{}-{}-{}-{}.in", spec.rank, name, std::process::id(), unique()))
        }
        (None, Some(path)) if is_xtool => PathBuf::from(path),
        _ => return Ok(input),
    };
    let file = std::fs::File::create(&path).with_context(|| format!("create {}", path.display()))?;
    Ok(Box::new(Tee { inner: input, copy: io::BufWriter::with_capacity(IO_BUF, file) }))
}

fn external(spec: ExtSpec, input: Stream, ctx: &Ctx) -> Result<Stream> {
    let mut input = tee_if_requested(&spec, input)?;
    if !spec.header && !spec.input_file && !spec.output_file {
        // xtool restores one chunk at a time: give whole chunks to several xtool processes.
        let workers = crate::xtsplit::workers();
        if workers > 1 && crate::xtsplit::is_stream_decode(&spec.argv) {
            match crate::xtsplit::probe(input)? {
                crate::xtsplit::Plan::Split { header, rest } => return xtool_split(spec, header, rest, workers, ctx),
                crate::xtsplit::Plan::Single { reason, replay } => {
                    eprintln!("note: {}: not split ({reason}); one xtool decodes it", spec.label);
                    input = replay;
                }
            }
        }
        // Fast path: stdin -> codec -> stdout, read directly by the next stage.
        let own_dir = own_workdir(&spec, ctx)?;
        let mut cmd = command(&spec, Path::new(""), Path::new(""), workdir_of(&spec, &own_dir), ctx.tune);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
        let sp = spawn_child(&spec, cmd, ctx)?;
        let stdin = sp.stdin.unwrap();
        ctx.reg.worker(format!("feed {}", spec.label), move || feed(input, stdin));
        return Ok(Box::new(ChildOut {
            out: sp.stdout.unwrap(),
            child: sp.child,
            label: spec.label,
            tail: sp.stderr_tail,
            checked: false,
            reg: ctx.reg.clone(),
            _own_dir: own_dir,
        }));
    }

    let (mut w, r) = pipe(8);
    let ctx2 = ctx.clone();
    ctx.reg.worker(spec.label.clone(), move || {
        let mut input = input;
        if spec.header {
            // arc.ini `header=1`: one flag byte before the payload (C_External.cpp DeCompress).
            let mut flag = [0u8; 1];
            if input.read(&mut flag)? == 0 {
                w.finish()?;
                return Ok(());
            }
            match flag[0] {
                0 => {
                    io::copy(&mut input, &mut w).context("copying stored stage data")?;
                    w.finish()?;
                    return Ok(());
                }
                1 => {}
                // Archives from FreeArc versions without the flag: the byte is data.
                b => input = Box::new(io::Cursor::new(vec![b]).chain(input)),
            }
        }
        run_process(&spec, input, &mut w, &ctx2)?;
        w.finish()?;
        Ok(())
    });
    Ok(Box::new(r))
}

/// Decodes an xtool stream with up to `workers` xtool processes (see `xtsplit`).
fn xtool_split(spec: ExtSpec, header: Vec<u8>, rest: Stream, workers: usize, ctx: &Ctx) -> Result<Stream> {
    let (base, ctx2) = (spec.clone(), ctx.clone());
    let spawn = move |_chunk: u64| -> Result<crate::xtsplit::Worker> {
        let mut spec = base.clone();
        spec.label = format!("{} (split)", base.label);
        let own_dir = own_workdir(&spec, &ctx2)?;
        let mut cmd = command(&spec, Path::new(""), Path::new(""), workdir_of(&spec, &own_dir), ctx2.tune);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
        let sp = spawn_child(&spec, cmd, &ctx2)?;
        let (label, tail) = (spec.label.clone(), sp.stderr_tail.clone());
        Ok(crate::xtsplit::Worker {
            stdin: sp.stdin.unwrap(),
            stdout: sp.stdout.unwrap(),
            child: sp.child,
            exit_error: Box::new(move |status| exit_error(&label, status, &tail)),
            keep: Box::new(own_dir),
        })
    };
    let (reg, reg2) = (ctx.reg.clone(), ctx.reg.clone());
    crate::xtsplit::run(header, rest, workers, spawn, move || reg.kill_all(), move |label, f| reg2.worker(label, f), &spec.label)
}

/// The extension written right after a placeholder in a command ("{output}.mp3" -> ".mp3").
fn placeholder_ext(argv: &[String], ph: &str) -> String {
    argv.iter()
        .find_map(|a| a.find(ph).map(|i| a[i + ph.len()..].chars().take_while(|c| *c == '.' || c.is_ascii_alphanumeric()).collect()))
        .unwrap_or_default()
}

/// Runs one codec process to completion, writing its output into `w`. Handles every combination
/// of stdin/temp-file input and stdout/temp-file output.
fn run_process(spec: &ExtSpec, mut input: Stream, w: &mut PipeWriter, ctx: &Ctx) -> Result<()> {
    let id = format!("{}-{}", std::process::id(), unique());
    let tin = ctx.temp_dir.join(format!("arcunp-{id}.in"));
    let tout = ctx.temp_dir.join(format!("arcunp-{id}.out"));
    // "{input}.mpz" in the command: the file is the placeholder's path plus that extension.
    let with_ext = |base: &Path, ph: &str| {
        let ext = placeholder_ext(&spec.argv, ph);
        PathBuf::from(format!("{}{ext}", base.display()))
    };
    let own_dir = own_workdir(spec, ctx)?;
    let in_file = with_ext(&tin, "{input}");
    let out_file = match &spec.datafile {
        Some(name) => workdir_of(spec, &own_dir).join(name),
        None => with_ext(&tout, "{output}"),
    };
    let _cleanup = Cleanup(vec![tin.clone(), tout.clone(), in_file.clone(), out_file.clone()]);
    if spec.input_file {
        let mut f = io::BufWriter::with_capacity(IO_BUF, std::fs::File::create(&in_file).context("create input temp file")?);
        io::copy(&mut input, &mut f).context("writing codec input temp file")?;
        f.flush()?;
    }
    let mut cmd = command(spec, &tin, &tout, workdir_of(spec, &own_dir), ctx.tune);
    cmd.stdin(if spec.input_file { Stdio::null() } else { Stdio::piped() });
    cmd.stdout(if spec.output_file { Stdio::null() } else { Stdio::piped() });
    let sp = spawn_child(spec, cmd, ctx)?;
    let feeder = sp.stdin.map(|s| std::thread::spawn(move || feed(input, s)));
    if let Some(mut out) = sp.stdout {
        io::copy(&mut out, w).context("reading codec stdout")?;
    }
    let status = sp.child.lock().unwrap().wait()?;
    if let Some(f) = feeder {
        f.join().map_err(|_| anyhow!("feeder panicked"))??;
    }
    if !status.success() {
        return Err(exit_error(&spec.label, status, &sp.stderr_tail));
    }
    if spec.output_file {
        let mut f = std::fs::File::open(&out_file).context("codec did not create its output file")?;
        io::copy(&mut f, w)?;
    }
    Ok(())
}

struct Cleanup(Vec<PathBuf>);
impl Drop for Cleanup {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

struct RemoveDir(PathBuf);
impl Drop for RemoveDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Like FreeArc, an external process without a configured working folder gets its own empty
/// one: some codecs (the Razor stdio build) keep a fixed-name temp file there and crash when
/// two processes share it.
fn own_workdir(spec: &ExtSpec, ctx: &Ctx) -> Result<Option<RemoveDir>> {
    if spec.workdir.is_some() {
        return Ok(None);
    }
    let dir = ctx.temp_dir.join(format!("arcunp-{}-{}.cwd", std::process::id(), unique()));
    std::fs::create_dir_all(&dir).with_context(|| format!("create working folder {}", dir.display()))?;
    Ok(Some(RemoveDir(dir)))
}

fn workdir_of<'a>(spec: &'a ExtSpec, own: &'a Option<RemoveDir>) -> &'a Path {
    own.as_ref().map(|d| d.0.as_path()).or(spec.workdir.as_deref()).unwrap_or(Path::new(""))
}

pub(crate) fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Codec, Config};

    #[test]
    fn four_by_four_workers_follow_free_ram() {
        const MB: u64 = 1 << 20;
        const GB: u64 = 1 << 30;
        let cfg = Config::default();
        let native = match &cfg.resolve("4x4:b256mb:lzma:64mb").unwrap()[0].codec {
            Codec::FourByFour(inner) => inner.clone(),
            c => panic!("{c:?}"),
        };
        let foreign = vec![crate::config::Stage {
            method: crate::method::parse_chain("bcm").remove(0),
            codec: Codec::Cls { dll: "cls-bcm.dll".into(), method: "bcm".into(), performance_cores: false },
        }];
        // Batman: Arkham City's 4x4:b256mb:bcm with 24 GB free on 32 threads: 1.5 GB per block,
        // 8 at once (it was 3).
        assert_eq!(four_by_four_workers(256 * MB, &foreign, 24 * GB, None, 32), 8);
        // Small blocks: one per CPU (it was at most 16).
        assert_eq!(four_by_four_workers(16 * MB, &foreign, 24 * GB, None, 32), 32);
        // Native LZMA needs less per block.
        assert_eq!(four_by_four_workers(256 * MB, &native, 24 * GB, None, 32), 16);
        // Little free RAM: still one; a RAM limit keeps its old, smaller budget.
        assert_eq!(four_by_four_workers(256 * MB, &foreign, 512 * MB, None, 32), 1);
        assert_eq!(four_by_four_workers(64 * MB, &foreign, 24 * GB, Some(8 * GB), 32), 2);
    }

    #[test]
    fn four_by_four_resolves_its_inner_method() {
        let cfg = Config::default();
        let st = cfg.resolve("4x4:b16mb:t4:lzma:1mb").unwrap();
        match &st[0].codec {
            Codec::FourByFour(inner) => assert_eq!(inner[0].method.full(), "lzma:1mb"),
            c => panic!("{c:?}"),
        }
        // Tornado has no decoder here: the block still goes to unarc.dll.
        assert!(!cfg.missing("4x4:tor:16mb:c3").is_empty());
        assert!(cfg.missing("4x4:b128mb:storing").is_empty());
    }

    #[test]
    fn four_by_four_blocks_come_out_in_order() {
        let mut data = 0i32.to_le_bytes().to_vec();
        let mut expect = Vec::new();
        for i in 0..40u8 {
            let block: Vec<u8> = (0..(1000 + i as usize * 37)).map(|j| (j as u8).wrapping_mul(i)).collect();
            // Every third block stored as is (original size -1), the rest through the inner method.
            let orig = if i % 3 == 0 { -1 } else { block.len() as i32 };
            data.extend_from_slice(&orig.to_le_bytes());
            data.extend_from_slice(&(block.len() as i32).to_le_bytes());
            data.extend_from_slice(&block);
            expect.extend_from_slice(&block);
        }
        let stages = Config::default().resolve("4x4:b1mb:storing").unwrap();
        let ctx = Ctx { temp_dir: std::env::temp_dir(), host_dir: std::env::temp_dir(), tune: false, reg: Registry::default() };
        let mut out = Vec::new();
        build(&stages, Box::new(io::Cursor::new(data)), expect.len() as u64, &ctx).unwrap().read_to_end(&mut out).unwrap();
        assert!(ctx.reg.join().is_empty());
        assert_eq!(out, expect);
    }
}
