//! Parallel restore of precomp 0.4.3 streams (precomp.inside).
//!
//! Older repacks (Pro Evolution Soccer 2017, Far Cry 3, Call of Duty: Black Ops, Farming
//! Simulator 17) restore precompressed data with precomp.inside: CLS-precomp.dll hands the
//! stream to the repack's patched precomp.exe (precomp 0.4.3), which restores one record after
//! another on one core (PES 2017: 745 s at 0.8 cores).
//!
//! The stream is "PCF" 0.4.3 without on-the-fly compression: a header (magic, version, a 0 byte,
//! the input's name and a NUL), then records until the end. (PES 2017's starts
//! `PCF 00 04 03 00 $$arcdatafile$$.tmp`: the external `precomp` command line made it.) A depth-0 record's output depends
//! only on its own bytes (precomp-cpp's decompress_file: zlib, bzip2 and the image libraries are
//! set up per record, penalty bytes are relative to the record's own output, and a recursion
//! record holds its whole nested stream; the layer FitGirl added to precomp.exe flushes its
//! buffer at every record end, reverse-engineered). So this cuts the stream between records into
//! parts, each the header plus whole records, restores every part with its own plain
//! `precomp.exe -r` in its own empty folder, several at once, and passes the outputs on in order.
//! The patched exe acts as plain precomp 0.4.3 for paths that don't start with "null" or "out"
//! (its hooks key on those names), and then needs no shared objects or administrator rights.
//!
//! Raw records are passed on as they are. A part whose output doesn't come out at the size its
//! records say goes through precomp.inside after all, and so does the rest of a stream from a
//! record this doesn't know (another type, an end marker, a stream cut short): precomp.inside then
//! sees exactly what it would have seen, from a record boundary on.

use crate::stage::Stream;
use anyhow::{Context, Result, anyhow};
use std::collections::{BTreeMap, VecDeque};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// The precomp.exe precomp.inside drives: precomp 0.4.3 (MinGW, 2012) with FitGirl's IPC layer
/// appended.
const PRECOMP_043: (u64, &str) = (1_187_840, "a5f0564caadc1b936e5c30c390fbd32b63665bbdc5e272f24f2995dabd9ff245");
/// How the stream is cut.
#[derive(Clone, Copy)]
pub struct Sizes {
    /// PCF bytes per part.
    pub part: u64,
    /// A raw record at least this big is passed on by us instead of going through a part.
    pub raw_alone: u64,
}

impl Default for Sizes {
    fn default() -> Sizes {
        Sizes { part: 32 << 20, raw_alone: 1 << 20 }
    }
}
/// Free disk space needed next to the parts; with less, the stream goes to precomp.inside whole.
pub const MIN_FREE: u64 = 8 << 30;

/// Set after a part failed: later streams go to precomp.inside whole, as before.
static DISABLED: AtomicBool = AtomicBool::new(false);

/// precomp processes at once: `ARC_UNPACK_PRECOMP_WORKERS`, else half the hardware threads (2 to
/// 12). 1 = don't split.
pub fn workers() -> usize {
    if DISABLED.load(Ordering::Relaxed) {
        return 1;
    }
    if let Some(n) = std::env::var("ARC_UNPACK_PRECOMP_WORKERS").ok().and_then(|v| v.trim().parse::<usize>().ok()) {
        return n.max(1);
    }
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    (threads / 2).clamp(2, 12)
}

/// The precomp.exe next to `dll`, when `dll` is precomp.inside's CLS-precomp.dll and the exe is
/// the known build.
pub fn known_precomp(dll: &Path) -> Option<PathBuf> {
    if !dll.file_name()?.to_string_lossy().eq_ignore_ascii_case("cls-precomp.dll") {
        return None;
    }
    let exe = dll.with_file_name("precomp.exe");
    let len = std::fs::metadata(&exe).ok()?.len();
    if len != PRECOMP_043.0 {
        return None;
    }
    static SEEN: Mutex<Vec<(PathBuf, bool)>> = Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap();
    let ok = match seen.iter().find(|(p, _)| *p == exe) {
        Some(&(_, ok)) => ok,
        None => {
            let ok = crate::tools::sha256_file(&exe).is_ok_and(|h| crate::extract::hex(&h) == PRECOMP_043.1);
            seen.push((exe.clone(), ok));
            ok
        }
    };
    ok.then_some(exe)
}

pub enum Plan {
    /// A PCF 0.4.3 stream: its header and the records after it.
    Split { header: Vec<u8>, rest: Stream },
    /// Something else: precomp.inside restores it. The bytes already read are put back in front.
    Single { reason: String, replay: Stream },
}

fn read_exact_into(input: &mut Stream, buf: &mut Vec<u8>, n: usize) -> io::Result<bool> {
    let start = buf.len();
    buf.resize(start + n, 0);
    let mut got = 0;
    while got < n {
        match input.read(&mut buf[start + got..])? {
            0 => {
                buf.truncate(start + got);
                return Ok(false);
            }
            k => got += k,
        }
    }
    Ok(true)
}

/// Reads the stream header and decides whether to split.
pub fn probe(mut input: Stream) -> Result<Plan> {
    let mut got = Vec::new();
    let verdict = (|| -> io::Result<std::result::Result<(), String>> {
        if !read_exact_into(&mut input, &mut got, 7)? || &got[..3] != b"PCF" {
            return Ok(Err("not a precomp stream".into()));
        }
        if got[3..6] != [0, 4, 3] {
            return Ok(Err(format!("a precomp {}.{}.{} stream", got[3], got[4], got[5])));
        }
        if got[6] != 0 {
            return Ok(Err("compressed on the fly".into()));
        }
        loop {
            if got.len() > 7 + 4096 {
                return Ok(Err("an implausible file name in its header".into()));
            }
            if !read_exact_into(&mut input, &mut got, 1)? {
                return Ok(Err("it ends inside its header".into()));
            }
            if got.last() == Some(&0) {
                return Ok(Ok(()));
            }
        }
    })()?;
    Ok(match verdict {
        Ok(()) => Plan::Split { header: got, rest: input },
        Err(reason) => Plan::Single { reason, replay: Box::new(io::Cursor::new(got).chain(input)) },
    })
}

/// A record's leading fields, read into `Fields::bytes`, and what follows them.
#[derive(Debug, PartialEq)]
struct Head {
    /// Bytes of the record after its leading fields.
    data: u64,
    /// What precomp writes for the record.
    out: u64,
    /// A raw record (`data` bytes copied to the output as they are).
    raw: bool,
    /// precomp's own work files while it restores the record: a recursion record copies its
    /// nested stream to a temp file and restores that into another (precomp-cpp's
    /// recursion_decompress), next to the part's own files.
    work: u64,
}

enum Stop {
    /// The stream ended before the record's first byte.
    End,
    /// A record this doesn't take apart, or one cut short.
    Refuse(String),
    Io(io::Error),
}

impl From<io::Error> for Stop {
    fn from(e: io::Error) -> Stop {
        if e.kind() == io::ErrorKind::UnexpectedEof { Stop::Refuse("the stream ends inside a record".into()) } else { Stop::Io(e) }
    }
}

/// Reads a record's leading fields, keeping the bytes.
struct Fields<'a> {
    input: &'a mut Stream,
    bytes: Vec<u8>,
}

impl Fields<'_> {
    fn take(&mut self, n: u64) -> std::result::Result<&[u8], Stop> {
        let n = usize::try_from(n).map_err(|_| Stop::Refuse("a field too long".into()))?;
        if n > 64 << 20 {
            return Err(Stop::Refuse(format!("a {n}-byte field")));
        }
        let start = self.bytes.len();
        if !read_exact_into(self.input, &mut self.bytes, n)? {
            return Err(Stop::Refuse("the stream ends inside a record".into()));
        }
        Ok(&self.bytes[start..])
    }

    /// A big-endian number of `n` bytes.
    fn u(&mut self, n: u64) -> std::result::Result<u64, Stop> {
        Ok(self.take(n)?.iter().fold(0u64, |v, &b| (v << 8) | b as u64))
    }

    /// A 4-byte length (an int in precomp: negative ones are refused).
    fn len32(&mut self) -> std::result::Result<u64, Stop> {
        let v = self.u(4)?;
        if v > i32::MAX as u64 {
            return Err(Stop::Refuse("a negative length".into()));
        }
        Ok(v)
    }

    fn skip(&mut self, n: u64) -> std::result::Result<(), Stop> {
        self.take(n).map(|_| ())
    }
}

/// One depth-0 record's leading fields, as precomp 0.4.3's decompress_file reads them (the
/// same walk as the source study's pcfwalk.py).
fn parse(f: &mut Fields) -> std::result::Result<Head, Stop> {
    let mut first = Vec::with_capacity(1);
    if !read_exact_into(f.input, &mut first, 1)? {
        return Err(Stop::End);
    }
    f.bytes.extend_from_slice(&first);
    let h1 = first[0];
    if h1 == 0 {
        let n = f.u(8)?;
        if n == 0 {
            // precomp stops reading here.
            return Err(Stop::Refuse("an end marker".into()));
        }
        return Ok(Head { data: n, out: n, raw: true, work: 0 });
    }
    let t = f.u(1)? as u8;
    let penalty = h1 & 2 != 0;
    let recursion = h1 & 128 != 0;
    let pen = |f: &mut Fields| -> std::result::Result<(), Stop> {
        if penalty {
            let n = f.u(4)?;
            f.skip(n)?;
        }
        Ok(())
    };
    // (original length, stored length)
    let lens = |f: &mut Fields| -> std::result::Result<(u64, u64), Stop> { Ok((f.len32()?, f.len32()?)) };
    let nested = |f: &mut Fields, stored: u64| -> std::result::Result<u64, Stop> { if recursion { f.u(8) } else { Ok(stored) } };
    let head = |data: u64, out: u64| -> std::result::Result<Head, Stop> { Ok(Head { data, out, raw: false, work: 0 }) };
    // A recursion record: its nested stream (`data`) and that restored (`size`) in temp files.
    let deep = |h: std::result::Result<Head, Stop>, size: u64| h.map(|mut h| {
        if recursion {
            h.work = h.data + size;
        }
        h
    });
    match t {
        // PDF (FlateDecode), possibly an image restored with a BMP header.
        0 => {
            f.skip(1)?;
            let hl = f.u(3)?;
            f.skip(hl)?;
            pen(f)?;
            let (rec, dec) = lens(f)?;
            let bmp = h1 >> 6;
            if bmp == 3 {
                return Err(Stop::Refuse("a PDF record with image mode 3".into()));
            }
            let mut width = 0u64;
            if bmp != 0 {
                let b = f.take(if bmp == 1 { 54 + 1024 } else { 54 })?;
                width = u32::from_le_bytes(b[18..22].try_into().unwrap()) as u64;
                if bmp == 2 {
                    width *= 3;
                }
            }
            // def_part_skip: a pad of 4 - width % 4 bytes after every completed line.
            let data = if bmp == 0 || width % 4 == 0 { dec } else { dec + (dec / width) * (4 - width % 4) };
            head(data, 12 + hl + rec)
        }
        // ZIP, GZip
        1 | 2 => {
            f.skip(1)?;
            let hl = f.u(3)?;
            f.skip(hl)?;
            pen(f)?;
            let (rec, dec) = lens(f)?;
            let data = nested(f, dec)?;
            deep(head(data, if t == 1 { 4 } else { 2 } + hl + rec), dec)
        }
        // PNG
        3 => {
            f.skip(3)?;
            pen(f)?;
            let (rec, dec) = lens(f)?;
            head(dec, 6 + rec)
        }
        // PNG with several IDAT chunks: the restore re-inserts a 12-byte chunk header per split.
        4 => {
            f.skip(3)?;
            let cnt = f.u(2)? + 1;
            let mut ln = vec![(f.u(4)?.wrapping_sub(2)) & 0xFFFF_FFFF];
            for _ in 1..cnt {
                f.skip(4)?;
                ln.push(f.u(4)?);
            }
            pen(f)?;
            let (rec, dec) = lens(f)?;
            let (mut remaining, mut i, mut extra) = (rec, 0usize, 0u64);
            loop {
                if i as u64 >= cnt {
                    return Err(Stop::Refuse("a PNG record that runs past its chunk table".into()));
                }
                if remaining + 2 > ln[i] {
                    if remaining < ln[i] || i as u64 + 1 >= cnt {
                        return Err(Stop::Refuse("a PNG record with an unusual chunk table".into()));
                    }
                    remaining -= ln[i];
                    extra += 1;
                    i += 1;
                } else {
                    break;
                }
            }
            head(dec, 6 + rec + 12 * extra)
        }
        // GIF
        5 => {
            let n = f.u(4)?;
            f.skip(n)?;
            pen(f)?;
            let (rec, dec) = lens(f)?;
            head(dec, rec)
        }
        // JPG (no penalty bytes)
        6 => {
            let (rec, dec) = lens(f)?;
            head(dec, rec)
        }
        // SWF
        7 => {
            f.skip(1 + 1 + 4 + 2)?;
            pen(f)?;
            let (rec, dec) = lens(f)?;
            let data = nested(f, dec)?;
            deep(head(data, 10 + rec), dec)
        }
        // Base64 (no penalty bytes): the first length is what is stored, the second the encoding.
        8 => {
            let hl = f.u(2)?;
            f.skip(hl)?;
            let lc = f.u(2)?;
            f.skip(match (h1 >> 2) & 3 {
                2 => lc,
                1 => 2,
                _ => 1,
            })?;
            let (stored, encoded) = lens(f)?;
            let data = nested(f, stored)?;
            deep(head(data, hl + encoded), stored)
        }
        // bZip2, brute force zLib, raw zLib
        9 | 254 | 255 => {
            f.skip(if t == 255 { 3 } else { 1 })?;
            pen(f)?;
            let (rec, dec) = lens(f)?;
            let data = nested(f, dec)?;
            deep(head(data, if t == 255 { 2 } else { 0 } + rec), dec)
        }
        _ => Err(Stop::Refuse(format!("record type {t}"))),
    }
}

/// Starts `precomp -r -o<output> <pcf>` in `dir` and returns the process (registered with the
/// block, so it is stopped with the block).
pub type Spawn = Box<dyn Fn(&Path, &Path, &Path) -> Result<Arc<Mutex<Child>>> + Send + Sync>;
/// precomp.inside on a whole PCF stream.
pub type Single = Box<dyn Fn(Stream) -> Result<Stream> + Send + Sync>;

enum Kind {
    /// `<dir>\part.pcf`, restored into `<dir>\result.bin`.
    Pcf,
    /// `<dir>\raw.bin` is the output as it is.
    Raw,
    /// The rest of the stream, for precomp.inside.
    Rest(Option<Stream>),
}

enum State {
    Waiting,
    Done,
    Failed(String),
}

struct Part {
    dir: PathBuf,
    kind: Kind,
    /// Its output bytes, from its records.
    out: u64,
    /// The bytes it takes in the temp folder (its part file and its result).
    disk: u64,
    state: State,
}

#[derive(Default)]
struct Shared {
    parts: BTreeMap<u64, Part>,
    /// Pcf parts waiting for a process.
    queue: VecDeque<u64>,
    /// How many parts there are, once the reader is done.
    total: Option<u64>,
    /// Parts written but not yet passed on, and their bytes in the temp folder.
    in_flight: usize,
    staged: u64,
    failed: Option<String>,
}

struct Coord {
    s: Mutex<Shared>,
    cv: Condvar,
    /// The block's stop flag (set when the install is cancelled, stalls or another stage fails):
    /// a process killed then must not be taken for a failed part.
    aborted: Arc<AtomicBool>,
}

impl Coord {
    fn fail(&self, msg: String) {
        let mut s = self.s.lock().unwrap();
        if s.failed.is_none() {
            s.failed = Some(msg);
        }
        self.cv.notify_all();
    }

    fn aborted(&self) -> bool {
        self.aborted.load(Ordering::Relaxed)
    }

    /// Waits for a change, at most a moment, and turns the block's stop flag into a failure.
    fn wait<'a>(&self, s: std::sync::MutexGuard<'a, Shared>) -> std::sync::MutexGuard<'a, Shared> {
        let mut s = self.cv.wait_timeout(s, Duration::from_millis(200)).unwrap().0;
        if self.aborted() && s.failed.is_none() {
            s.failed = Some("stopped".into());
            self.cv.notify_all();
        }
        s
    }

    fn add(&self, k: u64, part: Part) {
        let mut s = self.s.lock().unwrap();
        if matches!(part.kind, Kind::Pcf) {
            s.queue.push_back(k);
        }
        s.in_flight += 1;
        s.staged += part.disk;
        s.parts.insert(k, part);
        self.cv.notify_all();
    }

    /// Waits until another part may be written (`limits`: parts, temp bytes); false once the
    /// block stops. One part is always allowed, so this can't wait for itself.
    fn room(&self, limits: (usize, u64)) -> bool {
        let mut s = self.s.lock().unwrap();
        while (s.in_flight >= limits.0 || s.staged >= limits.1) && s.in_flight > 0 && s.failed.is_none() {
            s = self.wait(s);
        }
        if self.aborted() && s.failed.is_none() {
            s.failed = Some("stopped".into());
            self.cv.notify_all();
        }
        s.failed.is_none()
    }

    /// A new part folder, or None once the block has stopped (then nothing is created any more,
    /// and the reader just ends).
    fn new_dir(&self, root: &Path, k: u64) -> Result<Option<PathBuf>> {
        let s = self.s.lock().unwrap();
        if s.failed.is_some() {
            return Ok(None);
        }
        let dir = root.join(k.to_string());
        std::fs::create_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
        Ok(Some(dir))
    }
}

/// The parts' folder, removed when its last user (the reader, a process slot, the output) is gone.
struct RootGuard(PathBuf);

impl Drop for RootGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn copy_n(input: &mut Stream, out: &mut impl Write, mut n: u64) -> io::Result<()> {
    let mut buf = vec![0u8; (1 << 20).min(n.max(1) as usize)];
    while n > 0 {
        let k = (n as usize).min(buf.len());
        let got = input.read(&mut buf[..k])?;
        if got == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the precomp stream ends inside a record"));
        }
        out.write_all(&buf[..got])?;
        n -= got as u64;
    }
    Ok(())
}

/// Restores `rest` (the records after `header`) with up to `workers` precomp processes started
/// by `spawn`, in parts under a new folder in `temp_dir`; `single` is precomp.inside, for what
/// those can't do. `aborted` is the block's stop flag; `run_thread` starts the block's threads.
#[allow(clippy::too_many_arguments)]
pub fn run(
    header: Vec<u8>,
    rest: Stream,
    workers: usize,
    sizes: Sizes,
    temp_dir: &Path,
    spawn: Spawn,
    single: Single,
    aborted: Arc<AtomicBool>,
    run_thread: impl Fn(String, Box<dyn FnOnce() -> Result<()> + Send>),
    label: &str,
) -> Result<Stream> {
    let root = std::path::absolute(temp_dir.join(format!("arcunp-pcf-{}-{}", std::process::id(), crate::stage::unique())))?;
    std::fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;
    let guard = Arc::new(RootGuard(root.clone()));
    let coord = Arc::new(Coord { s: Mutex::new(Shared::default()), cv: Condvar::new(), aborted });
    // Parts ahead of the output, and their bytes in the temp folder: a quarter of its free
    // space, 1 to 8 GiB (the game is being written to the same drive, maybe).
    let free = crate::guard::free_space(&root).unwrap_or(16 << 30);
    let limits = (2 * workers + 2, (free / 4).clamp(1 << 30, 8 << 30));

    // The reader: cuts the stream into parts.
    {
        let (coord, root, owned, guard) = (coord.clone(), root.clone(), label.to_string(), guard.clone());
        let reader = move || -> Result<()> {
            let _guard = guard;
            let r = read_parts(header, rest, sizes, &root, &coord, limits, &owned);
            if let Err(e) = &r {
                coord.fail(format!("{e:#}"));
            }
            r
        };
        run_thread(format!("{label} (split)"), Box::new(reader));
    }
    // The precomp processes.
    let spawn = Arc::new(spawn);
    for w in 0..workers {
        let (coord, spawn, guard) = (coord.clone(), spawn.clone(), guard.clone());
        std::thread::Builder::new()
            .name(format!("{label} (split) {w}"))
            .spawn(move || {
                let _guard = guard;
                restore_parts(&coord, &**spawn)
            })
            .expect("spawn thread");
    }
    Ok(Box::new(Output { coord, single, _root: guard, next: 0, cur: None }))
}

fn read_parts(header: Vec<u8>, mut input: Stream, sizes: Sizes, root: &Path, coord: &Coord, limits: (usize, u64), label: &str) -> Result<()> {
    let mut k = 0u64;
    // The part being written: (folder, file, PCF bytes, output bytes, precomp's work files).
    type Cur = Option<(PathBuf, BufWriter<std::fs::File>, u64, u64, u64)>;
    let mut cur: Cur = None;
    let finish = |cur: &mut Cur, k: &mut u64| -> Result<()> {
        if let Some((dir, mut w, len, out, work)) = cur.take() {
            w.flush()?;
            drop(w);
            coord.add(*k, Part { dir, kind: Kind::Pcf, out, disk: len + out + work, state: State::Waiting });
            *k += 1;
        }
        Ok(())
    };
    loop {
        if !coord.room(limits) {
            return Ok(());
        }
        let mut f = Fields { input: &mut input, bytes: Vec::with_capacity(64) };
        match parse(&mut f) {
            Ok(h) if h.raw && h.data >= sizes.raw_alone => {
                // Passed on as it is, in pieces within the temp budget (its output is its bytes).
                finish(&mut cur, &mut k)?;
                let mut left = h.data;
                while left > 0 {
                    if left != h.data && !coord.room(limits) {
                        return Ok(());
                    }
                    let n = left.min(sizes.part.max(1));
                    let Some(dir) = coord.new_dir(root, k)? else { return Ok(()) };
                    let mut w = BufWriter::with_capacity(1 << 20, std::fs::File::create(dir.join("raw.bin"))?);
                    copy_n(&mut input, &mut w, n)?;
                    w.flush()?;
                    drop(w);
                    coord.add(k, Part { dir, kind: Kind::Raw, out: n, disk: n, state: State::Done });
                    k += 1;
                    left -= n;
                }
            }
            Ok(h) => {
                let bytes = std::mem::take(&mut f.bytes);
                if cur.is_none() {
                    let Some(dir) = coord.new_dir(root, k)? else { return Ok(()) };
                    let mut w = BufWriter::with_capacity(1 << 20, std::fs::File::create(dir.join("part.pcf"))?);
                    w.write_all(&header)?;
                    cur = Some((dir, w, header.len() as u64, 0, 0));
                }
                let (_, w, len, out, work) = cur.as_mut().unwrap();
                w.write_all(&bytes)?;
                copy_n(&mut input, w, h.data)?;
                *len += bytes.len() as u64 + h.data;
                *out += h.out;
                // (One record's work files at a time: each goes before the next.)
                *work = (*work).max(h.work);
                if *len >= sizes.part {
                    finish(&mut cur, &mut k)?;
                }
            }
            Err(Stop::End) => break,
            Err(Stop::Io(e)) => return Err(e).context("reading the precomp stream"),
            Err(Stop::Refuse(why)) => {
                // From this record on, precomp.inside restores what is left, as it would have.
                let bytes = std::mem::take(&mut f.bytes);
                finish(&mut cur, &mut k)?;
                eprintln!("note: {label}: precomp.inside restores the rest from {why} on");
                let rest: Stream = Box::new(io::Cursor::new([header.clone(), bytes].concat()).chain(input));
                coord.add(k, Part { dir: root.join(k.to_string()), kind: Kind::Rest(Some(rest)), out: 0, disk: 0, state: State::Done });
                k += 1;
                let mut s = coord.s.lock().unwrap();
                s.total = Some(k);
                coord.cv.notify_all();
                return Ok(());
            }
        }
    }
    finish(&mut cur, &mut k)?;
    let mut s = coord.s.lock().unwrap();
    s.total = Some(k);
    coord.cv.notify_all();
    Ok(())
}

/// One precomp process slot: restores queued parts until there are none.
fn restore_parts(coord: &Coord, spawn: &(dyn Fn(&Path, &Path, &Path) -> Result<Arc<Mutex<Child>>> + Send + Sync)) {
    loop {
        let (k, dir, out) = {
            let mut s = coord.s.lock().unwrap();
            loop {
                if s.failed.is_some() || coord.aborted() {
                    return;
                }
                if let Some(k) = s.queue.pop_front() {
                    let p = &s.parts[&k];
                    break (k, p.dir.clone(), p.out);
                }
                if s.total.is_some() {
                    return;
                }
                s = coord.wait(s);
            }
        };
        let result = restore(&dir, out, spawn, coord);
        let mut s = coord.s.lock().unwrap();
        if let Some(p) = s.parts.get_mut(&k) {
            p.state = match result {
                Ok(()) => State::Done,
                Err(e) => State::Failed(format!("{e:#}")),
            };
        }
        coord.cv.notify_all();
    }
}

fn restore(dir: &Path, expected: u64, spawn: &(dyn Fn(&Path, &Path, &Path) -> Result<Arc<Mutex<Child>>> + Send + Sync), coord: &Coord) -> Result<()> {
    let (pcf, result) = (dir.join("part.pcf"), dir.join("result.bin"));
    let child = spawn(dir, &pcf, &result)?;
    // (The lock is held only for a moment, so the block can still stop the process.)
    let status = loop {
        if let Some(st) = child.lock().unwrap().try_wait()? {
            break st;
        }
        if coord.s.lock().unwrap().failed.is_some() || coord.aborted() {
            // Gone before its slot lets go of the folder, so its files can be removed.
            let mut c = child.lock().unwrap();
            let _ = c.kill();
            let _ = c.wait();
            coord.fail("stopped".into());
            return Err(anyhow!("stopped"));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // Killed with the rest of the block: that's not a part that failed.
    if coord.aborted() {
        coord.fail("stopped".into());
        return Err(anyhow!("stopped"));
    }
    // precomp exits with 0 even after "Error recompressing data!": the size tells.
    let got = std::fs::metadata(&result).map(|m| m.len()).unwrap_or(0);
    if !status.success() || got != expected {
        return Err(anyhow!("precomp restored {got} of {expected} bytes ({status})"));
    }
    Ok(())
}

/// The restored stream, part by part in order.
struct Output {
    coord: Arc<Coord>,
    single: Single,
    _root: Arc<RootGuard>,
    next: u64,
    /// The part being passed on: its reader, its folder, what it should come to, what it has
    /// come to, and its bytes in the temp folder.
    cur: Option<(Box<dyn Read + Send>, PathBuf, Option<u64>, u64, u64)>,
}

impl Read for Output {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if let Some((r, dir, expected, got, disk)) = &mut self.cur {
                let n = r.read(buf)?;
                if n > 0 {
                    *got += n as u64;
                    return Ok(n);
                }
                if let Some(e) = expected
                    && *got != *e
                {
                    let msg = format!("precomp part {} came to {got} bytes, expected {e}", self.next);
                    self.coord.fail(msg.clone());
                    return Err(io::Error::other(msg));
                }
                let _ = std::fs::remove_dir_all(&*dir);
                let disk = *disk;
                self.cur = None;
                self.next += 1;
                let mut s = self.coord.s.lock().unwrap();
                s.in_flight -= 1;
                s.staged -= disk;
                self.coord.cv.notify_all();
                continue;
            }
            let mut s = self.coord.s.lock().unwrap();
            loop {
                if let Some(f) = &s.failed {
                    return Err(io::Error::other(f.clone()));
                }
                if s.total.is_some_and(|t| self.next >= t) {
                    return Ok(0);
                }
                let Some(p) = s.parts.get_mut(&self.next) else {
                    s = self.coord.wait(s);
                    continue;
                };
                let (dir, disk) = (p.dir.clone(), p.disk);
                match (&mut p.kind, &p.state) {
                    (_, State::Waiting) => {
                        s = self.coord.wait(s);
                        continue;
                    }
                    (Kind::Pcf, State::Done) => {
                        let f = std::fs::File::open(dir.join("result.bin"))?;
                        self.cur = Some((Box::new(io::BufReader::with_capacity(1 << 20, f)), dir, Some(p.out), 0, disk));
                    }
                    (Kind::Raw, _) => {
                        let f = std::fs::File::open(dir.join("raw.bin"))?;
                        self.cur = Some((Box::new(io::BufReader::with_capacity(1 << 20, f)), dir, Some(p.out), 0, disk));
                    }
                    (Kind::Pcf, State::Failed(why)) => {
                        if self.coord.aborted() {
                            return Err(io::Error::other("stopped"));
                        }
                        // This part through precomp.inside, and the rest of this install too.
                        DISABLED.store(true, Ordering::Relaxed);
                        eprintln!("note: precomp part {} failed ({why}); precomp.inside restores it", self.next);
                        let (out, f) = (p.out, std::fs::File::open(dir.join("part.pcf"))?);
                        drop(s);
                        let r = (self.single)(Box::new(io::BufReader::with_capacity(1 << 20, f))).map_err(|e| io::Error::other(format!("{e:#}")))?;
                        self.cur = Some((r, dir, Some(out), 0, disk));
                    }
                    (Kind::Rest(rest), _) => {
                        if self.coord.aborted() {
                            return Err(io::Error::other("stopped"));
                        }
                        let rest = rest.take().ok_or_else(|| io::Error::other("the rest of the precomp stream was taken twice"))?;
                        drop(s);
                        let r = (self.single)(rest).map_err(|e| io::Error::other(format!("{e:#}")))?;
                        self.cur = Some((r, dir, None, 0, disk));
                    }
                }
                break;
            }
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        // Stops the reader and the processes if the stream wasn't read to the end.
        {
            let mut s = self.coord.s.lock().unwrap();
            if s.failed.is_none() && !s.total.is_some_and(|t| self.next >= t) {
                s.failed = Some("stopped".into());
            }
            self.coord.cv.notify_all();
        }
        // (The folder goes with the last of the reader, the process slots and this.)
        self.cur = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn be(v: u64, n: usize) -> Vec<u8> {
        v.to_be_bytes()[8 - n..].to_vec()
    }

    /// Records of every type (structure only), with what they hold: the source study's self-test.
    fn synthetic() -> (Vec<u8>, Vec<(u64, bool)>) {
        let mut s = Vec::new();
        let mut expect = Vec::new();
        let mut rec = |bytes: Vec<u8>, out: u64, raw: bool, s: &mut Vec<u8>| {
            s.extend(bytes);
            expect.push((out, raw));
        };
        rec([vec![0], be(5, 8), b"abcde".to_vec()].concat(), 5, true, &mut s);
        rec([vec![1 | 2 | (9 << 2), 1, 0x7F], be(3, 3), b"hdr".to_vec(), be(5, 4), b"ppppp".to_vec(), be(100, 4), be(7, 4), vec![b'z'; 7]].concat(), 4 + 3 + 100, false, &mut s);
        let nested = [vec![0], be(4, 8), b"wxyz".to_vec()].concat();
        rec([vec![1 | 128, 2, 0x7F], be(2, 3), b"gh".to_vec(), be(50, 4), be(4, 4), be(nested.len() as u64, 8), nested].concat(), 2 + 2 + 50, false, &mut s);
        rec([vec![1, 3, 0x7F, 0x78, 0x9d], be(40, 4), be(9, 4), vec![b'q'; 9]].concat(), 46, false, &mut s);
        rec([vec![1, 4, 0x7F, 0x78, 0x9d], be(2, 2), be(20, 4), be(0, 4), be(20, 4), be(0, 4), be(20, 4), be(50, 4), be(6, 4), vec![b'm'; 6]].concat(), 6 + 50 + 12 * 2, false, &mut s);
        rec([vec![1, 5], be(2, 4), b"dd".to_vec(), be(30, 4), be(8, 4), vec![b'g'; 8]].concat(), 30, false, &mut s);
        rec([vec![1, 6], be(70, 4), be(11, 4), vec![b'j'; 11]].concat(), 70, false, &mut s);
        rec([vec![1, 7, 0x7F, 0x0a], be(99, 4), vec![0x78, 0x9d], be(60, 4), be(3, 4), vec![b's'; 3]].concat(), 70, false, &mut s);
        rec([vec![1 | (2 << 2), 8], be(4, 2), b"Con:".to_vec(), be(3, 2), vec![0x4c, 0x4c, 0x10], be(6, 4), be(10, 4), vec![b'b'; 6]].concat(), 4 + 10, false, &mut s);
        rec([vec![1, 9, 9], be(25, 4), be(5, 4), vec![b'x'; 5]].concat(), 25, false, &mut s);
        rec([vec![1, 254, 0x7F], be(25, 4), be(5, 4), vec![b'x'; 5]].concat(), 25, false, &mut s);
        rec([vec![1, 255, 0x7F, 0x78, 0x9c], be(25, 4), be(5, 4), vec![b'x'; 5]].concat(), 27, false, &mut s);
        // PDF with a 24-bit image 3 px wide: 9 bytes per line, 3 pad bytes per line, 2 lines.
        let mut bmp = vec![0u8; 54];
        bmp[18..22].copy_from_slice(&3u32.to_le_bytes());
        rec([vec![1 | (2 << 6), 0, 0x7F], be(1, 3), b" ".to_vec(), be(33, 4), be(18, 4), bmp, vec![b'p'; 18 + 2 * 3]].concat(), 12 + 1 + 33, false, &mut s);
        (s, expect)
    }

    #[test]
    fn records_of_every_type() {
        let (s, expect) = synthetic();
        let mut input: Stream = Box::new(io::Cursor::new(s));
        let mut got = Vec::new();
        loop {
            let mut f = Fields { input: &mut input, bytes: Vec::new() };
            match parse(&mut f) {
                Ok(h) => {
                    copy_n(&mut input, &mut io::sink(), h.data).unwrap();
                    got.push((h.out, h.raw));
                }
                Err(Stop::End) => break,
                Err(Stop::Refuse(why)) => panic!("refused: {why}"),
                Err(Stop::Io(e)) => panic!("{e}"),
            }
        }
        assert_eq!(got, expect);
        // Another type, an end marker, a record cut short: not taken apart.
        for bad in [vec![1u8, 77, 0], [vec![0u8], be(0, 8)].concat(), vec![1u8, 6, 0, 0]] {
            let mut input: Stream = Box::new(io::Cursor::new(bad));
            let mut f = Fields { input: &mut input, bytes: Vec::new() };
            assert!(matches!(parse(&mut f), Err(Stop::Refuse(_))));
        }
    }

    #[test]
    fn stream_headers() {
        let ok = b"PCF\x00\x04\x03\x00game.bin\x00rest".to_vec();
        match probe(Box::new(io::Cursor::new(ok))).unwrap() {
            Plan::Split { header, mut rest } => {
                assert_eq!(header, b"PCF\x00\x04\x03\x00game.bin\x00");
                let mut r = Vec::new();
                rest.read_to_end(&mut r).unwrap();
                assert_eq!(r, b"rest");
            }
            Plan::Single { reason, .. } => panic!("{reason}"),
        }
        // Another version or on-the-fly compression: the whole stream goes on unchanged.
        for other in [b"PCF\x00\x04\x08\x00x\x00data".to_vec(), b"PCF\x00\x04\x03\x01x\x00data".to_vec(), b"XTL0abc".to_vec()] {
            match probe(Box::new(io::Cursor::new(other.clone()))).unwrap() {
                Plan::Single { mut replay, .. } => {
                    let mut r = Vec::new();
                    replay.read_to_end(&mut r).unwrap();
                    assert_eq!(r, other);
                }
                Plan::Split { .. } => panic!("split"),
            }
        }
    }

    /// The real precomp: PCF_TEST=<pcf>|<original>|<precomp.exe>. Restores the stream in parts
    /// (part 1's process is made to fail, so precomp "inside" takes it: here a plain precomp on
    /// the whole part) and compares with the original.
    #[test]
    #[ignore]
    fn real_precomp_parts() {
        let v = std::env::var("PCF_TEST").unwrap();
        let [pcf, orig, exe]: [&str; 3] = v.split('|').collect::<Vec<_>>().try_into().unwrap();
        let exe = PathBuf::from(exe);
        let tmp = tempfile::tempdir().unwrap();
        let input: Stream = Box::new(std::fs::File::open(pcf).unwrap());
        let Plan::Split { header, rest } = probe(input).unwrap() else { panic!("not split") };
        let exe2 = exe.clone();
        let spawn: Spawn = Box::new(move |dir, pcf, out| {
            let fail = dir.file_name().is_some_and(|n| n == "1");
            let mut cmd = if fail {
                let mut c = std::process::Command::new("cmd");
                c.args(["/c", "exit 0"]);
                c
            } else {
                let mut c = std::process::Command::new(&exe2);
                c.arg("-r").arg(format!("-o{}", out.display())).arg(pcf);
                c
            };
            cmd.current_dir(dir).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null());
            Ok(Arc::new(Mutex::new(cmd.spawn()?)))
        });
        let (single_dir, exe3) = (tmp.path().join("single"), exe.clone());
        std::fs::create_dir(&single_dir).unwrap();
        let single: Single = Box::new(move |mut s: Stream| {
            let d = single_dir.join(crate::stage::unique().to_string());
            std::fs::create_dir(&d)?;
            let mut f = std::fs::File::create(d.join("in.pcf"))?;
            io::copy(&mut s, &mut f)?;
            drop(f);
            let st = std::process::Command::new(&exe3).arg("-r").arg(format!("-o{}", d.join("out.bin").display())).arg(d.join("in.pcf")).current_dir(&d).stdout(std::process::Stdio::null()).status()?;
            assert!(st.success());
            Ok(Box::new(std::fs::File::open(d.join("out.bin"))?) as Stream)
        });
        let threads = |label: String, f: Box<dyn FnOnce() -> Result<()> + Send>| {
            std::thread::Builder::new().name(label).spawn(move || f().unwrap()).unwrap();
        };
        // Small parts, so there are several, and raw records of their own.
        let sizes = Sizes { part: 512 << 10, raw_alone: 64 << 10 };
        let mut out = run(header, rest, 3, sizes, tmp.path(), spawn, single, Arc::new(AtomicBool::new(false)), threads, "precomp").unwrap();
        let mut got = Vec::new();
        out.read_to_end(&mut got).unwrap();
        let want = std::fs::read(orig).unwrap();
        println!("restored {} bytes, original {}", got.len(), want.len());
        assert!(got == want);
    }
}
