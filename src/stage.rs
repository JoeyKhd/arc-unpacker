//! Turns a resolved method chain into running decode stages connected by pipes.
//! Every stage runs concurrently: built-ins on their own thread, codecs as child processes.

use crate::config::{Codec, Stage};
use crate::pipe::{PipeWriter, pipe};
use anyhow::{Context, Result, anyhow, bail};
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
                };
                rank += 1;
                external(spec, stream, ctx)?
            }
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
                };
                rank += 1;
                external(spec, stream, ctx)?
            }
        };
    }
    Ok(stream)
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
    let mut child = cmd.spawn().with_context(|| format!("{}: failed to start {:?}", spec.label, spec.argv[0]))?;
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
/// input, as `<dir>\<stage rank>-<label>-<id>.in` (rank 0 = the first decoder of the chain).
fn tee_if_requested(spec: &ExtSpec, input: Stream) -> Result<Stream> {
    let is_xtool = Path::new(&spec.argv[0]).file_name().is_some_and(|n| n.to_string_lossy().to_ascii_lowercase().starts_with("xtool"));
    let path = match (std::env::var_os("ARC_UNPACK_TEE_DIR"), std::env::var_os("ARC_UNPACK_TEE_XTOOL")) {
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

/// Runs one codec process to completion, writing its output into `w`. Handles every combination
/// of stdin/temp-file input and stdout/temp-file output.
fn run_process(spec: &ExtSpec, mut input: Stream, w: &mut PipeWriter, ctx: &Ctx) -> Result<()> {
    let id = format!("{}-{}", std::process::id(), unique());
    let tin = ctx.temp_dir.join(format!("arcunp-{id}.in"));
    let tout = ctx.temp_dir.join(format!("arcunp-{id}.out"));
    let _cleanup = Cleanup(vec![tin.clone(), tout.clone()]);
    let own_dir = own_workdir(spec, ctx)?;
    if spec.input_file {
        let mut f = io::BufWriter::with_capacity(IO_BUF, std::fs::File::create(&tin).context("create input temp file")?);
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
        let mut f = std::fs::File::open(&tout).context("codec did not create its output file")?;
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
