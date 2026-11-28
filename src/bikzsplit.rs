//! Parallel decoding of FitGirl's cls-zstd.dll streams.
//!
//! Despite its name, cls-zstd.dll (one build, in Dark Souls Remastered, Far Cry 3, Left 4 Dead 2,
//! Mad Max, Rocksmith 2014, Civilization 6, Sleeping Dogs and Spintires) is a Bink video
//! recompressor (reverse-engineered): it restores one .bik/.bk2 file at a time with a fixed 2 or 6
//! decoder threads, about 1.4 cores, so a solid group of videos decodes on little more than one
//! core (Rocksmith 2014: 1.9 GB in 113 s). Each video is a block that depends on nothing before or
//! after it: the DLL sets up every model and buffer per block and keeps no other state, reads no
//! settings and writes no temp files. This cuts the stream between blocks, decodes groups of whole
//! coded blocks with several copies of the DLL (each in its own codec host), and passes the
//! outputs on in order. Stored blocks are copied here, as the DLL copies them.
//!
//! Stream: `A R J 02`, then blocks, each starting with a type byte t (t & 3):
//! - 0: 4 MiB stored;
//! - 1: two more bytes, len = (t | b1 << 8 | b2 << 16) >> 2, then len stored bytes;
//! - 2: one coded video: `A R J 01`, n (u8), n bytes, m (u24 LE), m bytes of side information,
//!   H bytes of Bink header, then the frames' chunks; H and the chunk sizes come from the side
//!   information's range coder (`read_coded`), so a block's length is known from its first few KB;
//! - 3: the end (the DLL reads nothing after it).
//!
//! Each process gets `A R J 02`, its blocks and the end byte (without it, the DLL never returns).
//! A coded block is handed out only once the bytes after it start a block as they should; from
//! anything unexpected on (or a block too big to hold), the rest of the stream goes to one
//! cls-zstd, which then sees exactly what it would have seen from that boundary on. The FitGirl
//! CRC of every file is the final check.

use crate::stage::Stream;
use anyhow::{Context, Result, anyhow, bail};
use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// The cls-zstd.dll every known setup ships (UPX-packed, 32-bit, 2017).
const CLS_ZSTD: (u64, &str) = (85_504, "fb0b85e32eac59200a807f1ff51b7f1a865faa65e9dfbe39c75a8dcaa5be3fd3");
const MAGIC: &[u8; 4] = b"ARJ\x02";
const CODED: &[u8; 4] = b"ARJ\x01";
const END: u8 = 3;
/// Probabilities per size model, and models: 21 decoder-thread slots and the header size.
const MODEL: usize = 69;
const NMODELS: usize = 22;
/// A coded block bigger than this isn't held in memory (nor one bigger than the memory budget,
/// `budget()`): the rest goes to one cls-zstd. Dark Souls Remastered's first video is 295 MB.
const BLOCK_MAX: u64 = 1 << 30;
/// Input bytes of whole coded blocks given to one process. The first groups are smaller, so a
/// short stream starts at once.
const GROUP: usize = 32 << 20;
const FIRST_GROUP: usize = 4 << 20;

/// Set after a split decode failed: later streams go to one cls-zstd, as before.
static DISABLED: AtomicBool = AtomicBool::new(false);

/// cls-zstd processes at once: `ARC_UNPACK_BINK_WORKERS`, else half the hardware threads (2 to 16)
/// as far as free memory allows. 1 = don't split.
pub fn workers() -> usize {
    if DISABLED.load(Ordering::Relaxed) {
        return 1;
    }
    if let Some(n) = std::env::var("ARC_UNPACK_BINK_WORKERS").ok().and_then(|v| v.trim().parse::<usize>().ok()) {
        return n.max(1);
    }
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    // Each takes about 1.4 cores and 20-60 MB, plus its share of the blocks held here.
    let (free, _) = crate::guard::memory();
    let free = crate::memlimit::get().map_or(free, |limit| free.min(limit));
    let fit = if free > 0 { (free / 2 / (256 << 20)) as usize } else { 2 };
    (threads / 2).clamp(2, 16).min(fit.max(1))
}

/// Whether `dll` is the known cls-zstd.dll.
pub fn known(dll: &Path) -> bool {
    dll.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("cls-zstd.dll"))
        && std::fs::metadata(dll).is_ok_and(|m| m.len() == CLS_ZSTD.0)
        && crate::stage::file_sha256(dll).as_deref() == Some(CLS_ZSTD.1)
}

/// Blocks being decoded plus output not yet passed on, before no more groups are handed out
/// (the group in turn always goes): 1 GiB, at most a quarter of the free RAM or an eighth of a
/// RAM limit, at least 256 MiB.
fn budget() -> u64 {
    let (free, _) = crate::guard::memory();
    let mut b = (1u64 << 30).min(free / 4);
    if let Some(limit) = crate::memlimit::get() {
        b = b.min(limit / 8);
    }
    b.max(256 << 20)
}

enum Plan {
    /// A cls-zstd stream: the blocks after its magic.
    Split { rest: Stream },
    /// Something else: one cls-zstd decodes it. The bytes already read are put back in front.
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

/// Reads the stream's magic and first block start and decides whether to split.
fn probe(mut input: Stream) -> Result<Plan> {
    let mut got = Vec::with_capacity(9);
    let reason = if !read_exact_into(&mut input, &mut got, 5)? {
        Some("too short")
    } else if &got[..4] != MAGIC {
        Some("not a cls-zstd stream")
    } else if got[4] & 3 == 2 && !(read_exact_into(&mut input, &mut got, 4)? && &got[5..9] == CODED) {
        Some("its first block isn't one this knows")
    } else {
        None
    };
    Ok(match reason {
        Some(r) => Plan::Single { reason: r.into(), replay: Box::new(io::Cursor::new(got).chain(input)) },
        None => Plan::Split { rest: Box::new(io::Cursor::new(got[4..].to_vec()).chain(input)) },
    })
}

/// The DLL's binary range decoder (16-bit probabilities of a 0, adapted by 1/64; big-endian
/// start; bytes past the end read as 0).
struct Rc<'a> {
    b: &'a [u8],
    pos: usize,
    code: u32,
    range: u32,
}

impl<'a> Rc<'a> {
    fn new(b: &'a [u8]) -> Rc<'a> {
        let code = (0..4).fold(0u32, |c, i| c << 8 | u32::from(b.get(i).copied().unwrap_or(0)));
        Rc { b, pos: 4, code, range: u32::MAX }
    }

    fn bit(&mut self, p: &mut u16) -> u32 {
        while self.range < 1 << 24 {
            self.code = self.code << 8 | u32::from(self.b.get(self.pos).copied().unwrap_or(0));
            self.pos += 1;
            self.range <<= 8;
        }
        let bound = ((u64::from(self.range) * u64::from(*p)) >> 16) as u32;
        if self.code < bound {
            self.range = bound;
            *p += (0xFFFF - *p) >> 6;
            0
        } else {
            self.code -= bound;
            self.range -= bound;
            *p -= *p >> 6;
            1
        }
    }

    /// Bytes read past the end.
    fn over(&self) -> usize {
        self.pos.saturating_sub(self.b.len())
    }
}

/// A signed size (the DLL's FUN_100195c0): 0, or +-(2^n + mantissa), n from a 5-bit tree, the
/// mantissa's n bits LSB first under a 5-bit context; the sign under a 2-bit history.
fn value(rc: &mut Rc, m: &mut [u16; MODEL], ctx: &mut u8) -> i32 {
    if rc.bit(&mut m[0]) == 1 {
        return 0;
    }
    let b = rc.bit(&mut m[1 + usize::from(*ctx)]);
    *ctx = (b as u8 + 2 * *ctx) & 3;
    let mut node = 1usize;
    for _ in 0..5 {
        node = node * 2 + rc.bit(&mut m[5 + node]) as usize;
    }
    let n = node & 31;
    let (mut mant, mut c) = (0u32, 1usize);
    for i in 0..n {
        let bit = rc.bit(&mut m[0x25 + c]);
        c = if c < 16 { c * 2 } else { c & 0x1E } + bit as usize;
        mant |= bit << i;
    }
    let v = (1u32 << n).wrapping_add(mant) as i32;
    if b == 1 { v.wrapping_neg() } else { v }
}

/// The 44-byte Bink header from a coded block's header blob (the DLL's FUN_100207c0, first
/// part): 6-bit symbols until a 0, then 44 bytes of 8 bits under a 2-bit history.
fn bink_header(blob: &[u8]) -> Option<[u8; 44]> {
    let mut rc = Rc::new(blob);
    let mut p = [0x8000u16; 0x44];
    loop {
        let mut node = 1usize;
        for _ in 0..6 {
            node = node * 2 + rc.bit(&mut p[node]) as usize;
        }
        if node & 0x3F == 0 {
            break;
        }
        if rc.over() > 8 {
            return None;
        }
    }
    let mut out = [0u8; 44];
    let mut ctx = 0usize;
    for o in &mut out {
        for _ in 0..8 {
            let bit = rc.bit(&mut p[0x40 + ctx]);
            *o = *o << 1 | bit as u8;
            ctx = (2 * ctx + bit as usize) & 3;
        }
    }
    (rc.over() <= 8).then_some(out)
}

/// The input ending inside a block: the DLL would wait forever at its end, so it's an error.
fn cut_short() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "cls-zstd stream cut short")
}

/// Reads the rest of a coded block whose first 5 bytes (type byte, `A R J 01`) are in `blk`,
/// appending it to `blk`. Err(reason) for anything that doesn't add up, or a block of more than
/// `max` bytes (`blk` then holds every byte read).
fn read_coded(input: &mut Stream, blk: &mut Vec<u8>, max: u64) -> io::Result<std::result::Result<(), String>> {
    let cut = || Err(cut_short());
    if !read_exact_into(input, blk, 1)? {
        return cut();
    }
    let n = usize::from(blk[5]);
    if !read_exact_into(input, blk, n + 3)? {
        return cut();
    }
    let at = blk.len() - 3;
    let m = usize::from(blk[at]) | usize::from(blk[at + 1]) << 8 | usize::from(blk[at + 2]) << 16;
    if !read_exact_into(input, blk, m)? {
        return cut();
    }
    let side = blk[blk.len() - m..].to_vec();
    let mut rc = Rc::new(&side);
    let mut probs = [[0x8000u16; MODEL]; NMODELS];
    let mut ctx = [0u8; NMODELS];
    // (The 44-byte Bink header is range-coded in it, so it can be shorter: Far Cry 3 has 41.)
    let h = value(&mut rc, &mut probs[21], &mut ctx[21]);
    if !(1..1 << 26).contains(&h) {
        return Ok(Err(format!("a coded block's header size is {h}")));
    }
    let at = blk.len();
    if !read_exact_into(input, blk, h as usize)? {
        return cut();
    }
    let Some(head) = bink_header(&blk[at..]) else {
        return Ok(Err("a coded block's Bink header doesn't decode".into()));
    };
    let slots = if &head[..3] == b"KB2" { 2 } else { 6 };
    let frames = u64::from(u32::from_le_bytes(head[8..12].try_into().unwrap()));
    if frames > 1 << 24 {
        return Ok(Err(format!("a coded block has {frames} frames")));
    }
    // A priming round and one per frame: per decoder slot a size change; -1 ends the block.
    let mut acc = [0i32; 6];
    let mut total = 0u64;
    'rounds: for _ in 0..=frames {
        for j in 0..slots {
            acc[j] = acc[j].wrapping_add(value(&mut rc, &mut probs[j], &mut ctx[j]));
            if acc[j] == -1 {
                break 'rounds;
            }
            if acc[j] < 0 {
                return Ok(Err(format!("a coded block has a chunk of {} bytes", acc[j])));
            }
            total += acc[j] as u64;
        }
        if rc.over() > 8 {
            return Ok(Err("a coded block's sizes run past its side information".into()));
        }
    }
    if blk.len() as u64 + total > max {
        return Ok(Err(format!("a coded block of {} MB is more than this holds", (blk.len() as u64 + total) >> 20)));
    }
    blk.reserve(total as usize);
    let got = input.by_ref().take(total).read_to_end(blk)?;
    if (got as u64) < total {
        return cut();
    }
    Ok(Ok(()))
}

/// Reads the start of the next block into `head` (cleared first): its type byte, and for a coded
/// block the `A R J 01` after it. Ok(false) at the end of the input; Err(reason) for a coded
/// type byte followed by something else (`head` then holds what was read).
fn next_head(input: &mut Stream, head: &mut Vec<u8>) -> io::Result<std::result::Result<bool, String>> {
    head.clear();
    if !read_exact_into(input, head, 1)? {
        return Ok(Ok(false));
    }
    if head[0] & 3 == 2 {
        if !read_exact_into(input, head, 4)? {
            return Err(cut_short());
        }
        if &head[1..5] != CODED {
            return Ok(Err("a coded block doesn't start as one".into()));
        }
    }
    Ok(Ok(true))
}

/// Writes a process's whole input and closes it.
pub type Feed = Box<dyn FnOnce(&[u8]) -> io::Result<()> + Send>;

/// One cls-zstd process.
pub struct Worker {
    pub input: Feed,
    pub output: Box<dyn Read + Send>,
    /// Waits for the process to end: the error for a failed exit.
    pub wait: Box<dyn FnOnce() -> Result<()> + Send>,
}

/// Decodes a stream with one cls-zstd (the rest of a stream that isn't split).
pub type Single = Box<dyn Fn(Stream) -> Result<Stream> + Send + Sync>;

#[derive(Default)]
struct Shared {
    running: usize,
    /// Each item's output, once done.
    done: BTreeMap<u64, Vec<u8>>,
    /// The last item: the rest of the stream, for one cls-zstd.
    rest: Option<(u64, Stream)>,
    next_out: u64,
    /// Input being decoded, plus output done or expected but not yet passed on.
    pending: u64,
    items: Option<u64>,
    failed: Option<String>,
}

/// The block's stop flags (the codec registry's).
#[derive(Default, Clone)]
pub struct Stop {
    /// The block is being torn down (cancelled, stalled, or another stage failed).
    pub aborted: Arc<AtomicBool>,
    /// It was torn down because it made no progress.
    pub stalled: Arc<AtomicBool>,
}

struct Coord {
    s: Mutex<Shared>,
    cv: Condvar,
    stop: Stop,
}

impl Coord {
    fn fail(&self, msg: String) {
        let mut s = self.s.lock().unwrap();
        if s.failed.is_none() {
            s.failed = Some(msg);
        }
        self.cv.notify_all();
    }

    /// A block being torn down stops the split too, so it starts no process after the others
    /// were killed.
    fn check(&self, s: &mut Shared) {
        if self.stop.aborted.load(Ordering::Relaxed) && s.failed.is_none() {
            s.failed = Some("stopped".into());
            self.cv.notify_all();
        }
    }

    /// Waits for a change, or 200 ms (the stop flag is only set, never signalled).
    fn wait<'a>(&self, s: MutexGuard<'a, Shared>) -> MutexGuard<'a, Shared> {
        let mut s = self.cv.wait_timeout(s, Duration::from_millis(200)).unwrap().0;
        self.check(&mut s);
        s
    }
}

/// Decodes a cls-zstd stream with one process per group of whole coded blocks, at most
/// `max_workers` at a time, started by `spawn`; stored blocks are passed on here, and from
/// anything unexpected on (a stream of another kind: all of it) the rest goes to `single`.
/// `abort` stops every process of the block (on any failure); `stop` says it was stopped.
/// The stream is read on its own thread only, so this returns at once.
#[allow(clippy::too_many_arguments)]
pub fn run(
    input: Stream,
    max_workers: usize,
    spawn: impl Fn(u64) -> Result<Worker> + Send + 'static,
    single: Single,
    stop: Stop,
    abort: impl Fn() + Send + Sync + 'static,
    run_thread: impl Fn(String, Box<dyn FnOnce() -> Result<()> + Send>),
    label: &str,
) -> Result<Stream> {
    let coord = Arc::new(Coord { s: Mutex::new(Shared::default()), cv: Condvar::new(), stop });
    let abort: Arc<dyn Fn() + Send + Sync> = Arc::new(abort);
    let (coord2, abort2, label2) = (coord.clone(), abort.clone(), label.to_string());
    let budget = budget();
    let dispatcher = move || -> Result<()> {
        let r = match probe(input) {
            Ok(Plan::Split { rest }) => dispatch(rest, max_workers, budget, budget.min(BLOCK_MAX), &spawn, &coord2, &abort2, &label2),
            Ok(Plan::Single { reason, replay }) => {
                eprintln!("note: {label2}: not split ({reason}); one cls-zstd decodes it");
                let mut s = coord2.s.lock().unwrap();
                s.rest = Some((0, replay));
                s.items = Some(1);
                coord2.cv.notify_all();
                Ok(())
            }
            Err(e) => Err(e),
        };
        if let Err(e) = &r {
            coord2.fail(format!("{e:#}"));
            abort2();
        }
        r
    };
    run_thread(format!("{label} (split)"), Box::new(dispatcher));
    Ok(Box::new(Output { coord, single, cur: None }))
}

#[allow(clippy::too_many_arguments)]
fn dispatch(
    mut input: Stream,
    max_workers: usize,
    budget: u64,
    block_max: u64,
    spawn: &dyn Fn(u64) -> Result<Worker>,
    coord: &Arc<Coord>,
    abort: &Arc<dyn Fn() + Send + Sync>,
    label: &str,
) -> Result<()> {
    let mut item = 0u64;
    // The group being gathered, as its process input: the magic, then whole blocks.
    let mut group: Vec<u8> = MAGIC.to_vec();
    let mut limit = FIRST_GROUP;
    // Hands out the group with the end byte, and starts a fresh buffer (so a far bigger video
    // doesn't keep its room).
    let flush = |group: &mut Vec<u8>, item: &mut u64| -> Result<()> {
        if group.len() == MAGIC.len() {
            return Ok(());
        }
        group.push(END);
        let data = std::mem::replace(group, MAGIC.to_vec());
        // Output estimate: Bink files come out about a fifth bigger.
        let (inlen, est) = (data.len() as u64, data.len() as u64 * 5 / 4);
        {
            let mut s = coord.s.lock().unwrap();
            loop {
                coord.check(&mut s);
                if let Some(f) = &s.failed {
                    bail!("{f}");
                }
                let room = s.pending + inlen + est <= budget || *item == s.next_out;
                if room && s.running < max_workers {
                    s.running += 1;
                    s.pending += inlen + est;
                    break;
                }
                s = coord.wait(s);
            }
        }
        let wk = spawn(*item)?;
        let n = *item;
        let (coord3, abort3, mut output, wait) = (coord.clone(), abort.clone(), wk.output, wk.wait);
        std::thread::Builder::new()
            .name(format!("{label} blocks {n}"))
            .spawn(move || {
                let r = (|| -> Result<Vec<u8>> {
                    let mut out = Vec::new();
                    output.read_to_end(&mut out).context("reading cls-zstd's output")?;
                    drop(output);
                    wait()?;
                    Ok(out)
                })();
                match r {
                    Ok(out) => {
                        let mut s = coord3.s.lock().unwrap();
                        s.pending = s.pending - est + out.len() as u64;
                        s.running -= 1;
                        s.done.insert(n, out);
                        coord3.cv.notify_all();
                    }
                    // Killed with the rest of the block (stopped, or another stage failed): that
                    // says nothing about the split, whose own failure sets this before it stops
                    // the block. A copy that made no progress until the watchdog stopped the
                    // block may be one this split fed wrongly: later streams go to one copy.
                    Err(_) if coord3.stop.aborted.load(Ordering::Relaxed) => {
                        if coord3.stop.stalled.load(Ordering::Relaxed) {
                            DISABLED.store(true, Ordering::Relaxed);
                        }
                        coord3.fail("stopped".into());
                    }
                    Err(e) => {
                        DISABLED.store(true, Ordering::Relaxed);
                        coord3.fail(format!("{e:#}"));
                        abort3();
                    }
                }
            })
            .expect("spawn thread");
        // Its input from its own thread, so the next group is gathered meanwhile.
        let (coord3, abort3, feed) = (coord.clone(), abort.clone(), wk.input);
        std::thread::Builder::new()
            .name(format!("{label} blocks {n} in"))
            .spawn(move || match feed(&data) {
                Ok(()) => {
                    let mut s = coord3.s.lock().unwrap();
                    s.pending -= inlen;
                    coord3.cv.notify_all();
                }
                // The process died or stopped reading: the output side reports its exit status
                // (a clean exit with input left over comes out short, and the file CRCs catch
                // that, as in `feed`).
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                    let mut s = coord3.s.lock().unwrap();
                    s.pending -= inlen;
                    coord3.cv.notify_all();
                }
                Err(e) => {
                    coord3.fail(format!("writing to cls-zstd: {e}"));
                    abort3();
                }
            })
            .expect("spawn thread");
        *item += 1;
        Ok(())
    };
    // Passes output on as it is (stored blocks), after the group before it.
    let emit = |data: Vec<u8>, item: &mut u64| -> Result<()> {
        let mut s = coord.s.lock().unwrap();
        coord.check(&mut s);
        while s.pending > 0 && s.pending + data.len() as u64 > budget && *item != s.next_out && s.failed.is_none() {
            s = coord.wait(s);
        }
        if let Some(f) = &s.failed {
            bail!("{f}");
        }
        s.pending += data.len() as u64;
        s.done.insert(*item, data);
        coord.cv.notify_all();
        *item += 1;
        Ok(())
    };
    let mut head = Vec::with_capacity(5);
    let mut blocks = 0u64;
    let mut why = match next_head(&mut input, &mut head)? {
        Ok(true) => None,
        Ok(false) => bail!("cls-zstd stream ended without its end marker"),
        Err(e) => Some(e),
    };
    // `head`: the start of the next block, checked (or, with `why`, what didn't check out).
    let fallback = loop {
        if let Some(why) = why.take() {
            break Some((why, std::mem::take(&mut head)));
        }
        match head[0] & 3 {
            END => break None,
            2 => {
                let mut blk = std::mem::take(&mut head);
                if let Err(e) = read_coded(&mut input, &mut blk, block_max)? {
                    break Some((e, blk));
                }
                match next_head(&mut input, &mut head)? {
                    Ok(true) => blocks += 1,
                    Ok(false) => bail!("cls-zstd stream ended without its end marker"),
                    Err(e) => {
                        blk.append(&mut head);
                        break Some((e, blk));
                    }
                }
                if group.len() == MAGIC.len() {
                    // (Room for the whole group and its end byte at once.)
                    group.reserve_exact(limit.max(blk.len()) + 1);
                }
                group.append(&mut blk);
                if group.len() >= limit {
                    flush(&mut group, &mut item)?;
                    limit = (limit * 2).min(GROUP);
                }
            }
            t => {
                let len = if t == 0 { 0x40_0000 } else {
                    let mut b = std::mem::take(&mut head);
                    if !read_exact_into(&mut input, &mut b, 2)? {
                        return Err(cut_short().into());
                    }
                    (usize::from(b[0]) | usize::from(b[1]) << 8 | usize::from(b[2]) << 16) >> 2
                };
                let mut data = Vec::new();
                if !read_exact_into(&mut input, &mut data, len)? {
                    return Err(cut_short().into());
                }
                flush(&mut group, &mut item)?;
                emit(data, &mut item)?;
                match next_head(&mut input, &mut head)? {
                    Ok(true) => {}
                    Ok(false) => bail!("cls-zstd stream ended without its end marker"),
                    Err(e) => why = Some(e),
                }
            }
        }
    };
    flush(&mut group, &mut item)?;
    let mut s = coord.s.lock().unwrap();
    if let Some((why, bytes)) = fallback {
        // One cls-zstd from this boundary on, as if the stream started here.
        eprintln!("note: {label}: not split from video {} on ({why}); one cls-zstd decodes the rest", blocks + 1);
        let mut pre = MAGIC.to_vec();
        pre.extend_from_slice(&bytes);
        s.rest = Some((item, Box::new(io::Cursor::new(pre).chain(input))));
        item += 1;
    }
    // (After the end byte the DLL reads nothing: neither does this.)
    s.items = Some(item);
    coord.cv.notify_all();
    Ok(())
}

/// The decoded stream, item by item in order.
struct Output {
    coord: Arc<Coord>,
    single: Single,
    cur: Option<Box<dyn Read + Send>>,
}

impl Read for Output {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if let Some(r) = &mut self.cur {
                let n = r.read(buf)?;
                if n > 0 {
                    return Ok(n);
                }
                self.cur = None;
                let mut s = self.coord.s.lock().unwrap();
                s.next_out += 1;
                self.coord.cv.notify_all();
                continue;
            }
            let mut s = self.coord.s.lock().unwrap();
            loop {
                self.coord.check(&mut s);
                if let Some(f) = &s.failed {
                    return Err(io::Error::other(f.clone()));
                }
                let next = s.next_out;
                if s.items.is_some_and(|t| next >= t) {
                    return Ok(0);
                }
                if let Some(out) = s.done.remove(&next) {
                    s.pending -= out.len() as u64;
                    self.coord.cv.notify_all();
                    drop(s);
                    self.cur = Some(Box::new(io::Cursor::new(out)));
                    break;
                }
                if s.rest.as_ref().is_some_and(|(i, _)| *i == next) {
                    let (_, rest) = s.rest.take().unwrap();
                    drop(s);
                    let r = (self.single)(rest).map_err(|e| io::Error::other(format!("{e:#}")))?;
                    self.cur = Some(r);
                    break;
                }
                s = self.coord.wait(s);
            }
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        // Stops the dispatcher if the stream wasn't read to the end.
        let mut s = self.coord.s.lock().unwrap();
        if s.failed.is_none() && !s.items.is_some_and(|t| s.next_out >= t) {
            s.failed = Some("stopped".into());
        }
        self.coord.cv.notify_all();
    }
}

/// Walks a whole cls-zstd stream (for tests and checks): the number of coded and stored blocks,
/// or where it stops making sense.
pub fn walk(mut input: Stream) -> Result<(u64, u64)> {
    let mut got = Vec::new();
    if !read_exact_into(&mut input, &mut got, 4)? || &got[..] != MAGIC {
        bail!("not a cls-zstd stream");
    }
    let (mut coded, mut stored, mut head) = (0u64, 0u64, Vec::new());
    loop {
        match next_head(&mut input, &mut head)? {
            Ok(true) => {}
            Ok(false) => bail!("no end marker after {coded} videos"),
            Err(e) => bail!("after {coded} videos: {e}"),
        }
        match head[0] & 3 {
            END => return Ok((coded, stored)),
            2 => {
                let mut blk = std::mem::take(&mut head);
                read_coded(&mut input, &mut blk, BLOCK_MAX)?.map_err(|e| anyhow!("video {coded}: {e}"))?;
                coded += 1;
            }
            t => {
                let len = if t == 0 { 0x40_0000 } else {
                    if !read_exact_into(&mut input, &mut head, 2)? {
                        return Err(cut_short().into());
                    }
                    (usize::from(head[0]) | usize::from(head[1]) << 8 | usize::from(head[2]) << 16) >> 2
                };
                let n = io::copy(&mut input.by_ref().take(len as u64), &mut io::sink())?;
                if n < len as u64 {
                    return Err(cut_short().into());
                }
                stored += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The range encoder matching `Rc` (LZMA's, without its leading zero byte).
    struct Enc {
        low: u64,
        range: u32,
        cache: u8,
        size: u64,
        out: Vec<u8>,
    }

    impl Enc {
        fn new() -> Enc {
            Enc { low: 0, range: u32::MAX, cache: 0, size: 1, out: Vec::new() }
        }
        fn shift_low(&mut self) {
            if self.low < 0xFF00_0000 || self.low >= 1 << 32 {
                let carry = (self.low >> 32) as u8;
                let mut c = self.cache;
                loop {
                    self.out.push(c.wrapping_add(carry));
                    c = 0xFF;
                    self.size -= 1;
                    if self.size == 0 {
                        break;
                    }
                }
                self.cache = (self.low >> 24) as u8;
            }
            self.size += 1;
            self.low = (self.low & 0x00FF_FFFF) << 8;
        }
        fn bit(&mut self, p: &mut u16, bit: u32) {
            let bound = ((u64::from(self.range) * u64::from(*p)) >> 16) as u32;
            if bit == 0 {
                self.range = bound;
                *p += (0xFFFF - *p) >> 6;
            } else {
                self.low += u64::from(bound);
                self.range -= bound;
                *p -= *p >> 6;
            }
            while self.range < 1 << 24 {
                self.range <<= 8;
                self.shift_low();
            }
        }
        fn finish(mut self) -> Vec<u8> {
            for _ in 0..5 {
                self.shift_low();
            }
            self.out.remove(0);
            self.out
        }
        fn value(&mut self, m: &mut [u16; MODEL], ctx: &mut u8, v: i32) {
            if v == 0 {
                return self.bit(&mut m[0], 1);
            }
            self.bit(&mut m[0], 0);
            let b = u32::from(v < 0);
            self.bit(&mut m[1 + usize::from(*ctx)], b);
            *ctx = (b as u8 + 2 * *ctx) & 3;
            let a = v.unsigned_abs();
            let n = 31 - a.leading_zeros() as usize;
            let mut node = 1usize;
            for i in (0..5).rev() {
                let bit = (n >> i) as u32 & 1;
                self.bit(&mut m[5 + node], bit);
                node = node * 2 + bit as usize;
            }
            let mut c = 1usize;
            for i in 0..n {
                let bit = (a >> i) & 1;
                self.bit(&mut m[0x25 + c], bit);
                c = if c < 16 { c * 2 } else { c & 0x1E } + bit as usize;
            }
        }
    }

    /// A coded block: a Bink header with `frames` (KB2 or BIK) and chunks of the given sizes per
    /// round and slot (the last round may be short), then -1.
    fn coded(kb2: bool, frames: u32, chunks: &[Vec<i32>], fill: u8) -> Vec<u8> {
        // As the DLL reads them: a size per slot and round, frames + 1 rounds or up to a -1.
        let slots = if kb2 { 2 } else { 6 };
        for (r, round) in chunks.iter().enumerate() {
            let end = round.iter().position(|&z| z == -1);
            assert!(end.is_none_or(|e| e + 1 == round.len() && r + 1 == chunks.len()), "-1 only at the end");
            assert!(round.len() == slots || end.is_some(), "round {r}: {slots} sizes");
        }
        let ended = chunks.last().is_some_and(|r| r.last() == Some(&-1));
        assert!(if ended { chunks.len() as u32 <= frames + 1 } else { chunks.len() as u32 == frames + 1 });
        let mut head = [0u8; 44];
        head[..4].copy_from_slice(if kb2 { b"KB2g" } else { b"BIKi" });
        head[8..12].copy_from_slice(&frames.to_le_bytes());
        for (i, b) in head.iter_mut().enumerate().skip(12) {
            // (fill 0: a header that codes to fewer than its 44 bytes, as real ones can.)
            *b = if fill == 0 { 0 } else { (i as u8).wrapping_mul(37) ^ fill };
        }
        let mut e = Enc::new();
        let mut p = [0x8000u16; 0x44];
        let mut node = 1;
        for _ in 0..6 {
            e.bit(&mut p[node], 0);
            node *= 2;
        }
        let mut ctx = 0usize;
        for &byte in &head {
            for i in (0..8).rev() {
                let bit = u32::from(byte >> i & 1);
                e.bit(&mut p[0x40 + ctx], bit);
                ctx = (2 * ctx + bit as usize) & 3;
            }
        }
        let blob = e.finish();
        assert!(fill != 0 || blob.len() < 44, "a short header blob");
        assert_eq!(bink_header(&blob).unwrap(), head);
        let mut e = Enc::new();
        let mut probs = [[0x8000u16; MODEL]; NMODELS];
        let mut cx = [0u8; NMODELS];
        e.value(&mut probs[21], &mut cx[21], blob.len() as i32);
        let mut acc = [0i32; 6];
        let mut data = Vec::new();
        for round in chunks {
            for (j, &size) in round.iter().enumerate() {
                e.value(&mut probs[j], &mut cx[j], size.wrapping_sub(acc[j]));
                acc[j] = size;
                if size == -1 {
                    break;
                }
                data.extend((0..size).map(|k| (k as u8) ^ fill));
            }
        }
        let side = e.finish();
        let mut b = vec![2u8];
        b.extend_from_slice(CODED);
        b.push(2);
        b.extend_from_slice(&[0x11, 0x22]);
        b.extend_from_slice(&(side.len() as u32).to_le_bytes()[..3]);
        b.extend_from_slice(&side);
        b.extend_from_slice(&blob);
        b.extend_from_slice(&data);
        b
    }

    fn stored(data: &[u8]) -> Vec<u8> {
        let v = (data.len() as u32) << 2 | 1;
        let mut b = v.to_le_bytes()[..3].to_vec();
        b.extend_from_slice(data);
        b
    }

    /// A stand-in for the DLL: stored blocks as they are, each coded block as its bytes reversed,
    /// and anything it doesn't know as "?" and the rest.
    fn fake_decode(mut input: Stream) -> Vec<u8> {
        let mut out = Vec::new();
        let mut got = Vec::new();
        if !read_exact_into(&mut input, &mut got, 4).unwrap() || &got[..] != MAGIC {
            return b"not a stream".to_vec();
        }
        let mut head = Vec::new();
        loop {
            match next_head(&mut input, &mut head).unwrap() {
                Ok(true) => {}
                Ok(false) => return out,
                Err(_) => {
                    out.push(b'?');
                    input.read_to_end(&mut out).unwrap();
                    return out;
                }
            }
            match head[0] & 3 {
                END => return out,
                2 => {
                    let mut blk = std::mem::take(&mut head);
                    match read_coded(&mut input, &mut blk, BLOCK_MAX).unwrap() {
                        Ok(()) => out.extend(blk.iter().rev()),
                        Err(_) => {
                            out.push(b'?');
                            input.read_to_end(&mut out).unwrap();
                            return out;
                        }
                    }
                }
                t => {
                    let len = if t == 0 { 0x40_0000 } else {
                        read_exact_into(&mut input, &mut head, 2).unwrap();
                        (usize::from(head[0]) | usize::from(head[1]) << 8 | usize::from(head[2]) << 16) >> 2
                    };
                    let mut d = Vec::new();
                    read_exact_into(&mut input, &mut d, len).unwrap();
                    out.extend(d);
                }
            }
        }
    }

    fn split(data: &[u8], workers: usize) -> (Result<Vec<u8>>, u64) {
        split_stop(data, workers, Stop::default())
    }

    fn split_stop(data: &[u8], workers: usize, stop: Stop) -> (Result<Vec<u8>>, u64) {
        let spawned = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let sp = spawned.clone();
        let spawn = move |_| -> Result<Worker> {
            sp.fetch_add(1, Ordering::Relaxed);
            let (w, r) = crate::pipe::pipe(4);
            let (mut ow, or) = crate::pipe::pipe(4);
            let t = std::thread::spawn(move || {
                let out = fake_decode(Box::new(r));
                ow.write_all(&out).unwrap();
                ow.finish().unwrap();
            });
            Ok(Worker {
                input: Box::new(move |d| {
                    let mut w = w;
                    w.write_all(d)?;
                    w.finish()
                }),
                output: Box::new(or),
                wait: Box::new(move || t.join().map_err(|_| anyhow!("stand-in panicked"))),
            })
        };
        let single: Single = Box::new(|s| Ok(Box::new(io::Cursor::new(fake_decode(s))) as Stream));
        let r = (|| {
            let input: Stream = Box::new(io::Cursor::new(data.to_vec()));
            let mut out = run(input, workers, spawn, single, stop, || {}, |_, f| {
                std::thread::spawn(f);
            }, "test")?;
            let mut v = Vec::new();
            out.read_to_end(&mut v)?;
            Ok(v)
        })();
        (r, spawned.load(Ordering::Relaxed))
    }

    #[test]
    fn sizes_round_trip() {
        let mut e = Enc::new();
        let mut m = [0x8000u16; MODEL];
        let mut ctx = 0u8;
        let vals = [0, 1, -1, 2, 3, 44, -45, 1000, 65_535, 1 << 20, -(1 << 20), i32::MAX, i32::MIN + 1, 7, 0, 0, 12_345_678];
        for &v in &vals {
            e.value(&mut m, &mut ctx, v);
        }
        let bytes = e.finish();
        let mut rc = Rc::new(&bytes);
        let (mut m, mut ctx) = ([0x8000u16; MODEL], 0u8);
        for &v in &vals {
            assert_eq!(value(&mut rc, &mut m, &mut ctx), v);
        }
    }

    #[test]
    fn coded_block_lengths() {
        for (kb2, frames, chunks) in [
            (true, 2, vec![vec![10, 3], vec![12, 0], vec![-1]]),
            (false, 1, vec![vec![5, 0, 7, 1, 1, 9], vec![6, 0, 7, 2, -1]]),
            (true, 0, vec![vec![4, 4]]),
        ] {
            for fill in [0x5A, 0] {
                let blk = coded(kb2, frames, &chunks, fill);
                let mut input: Stream = Box::new(io::Cursor::new([blk.clone(), vec![3, 9, 9]].concat()));
                let mut got = Vec::new();
                read_exact_into(&mut input, &mut got, 5).unwrap();
                read_coded(&mut input, &mut got, BLOCK_MAX).unwrap().unwrap();
                assert_eq!(got, blk);
                let mut after = Vec::new();
                input.read_to_end(&mut after).unwrap();
                assert_eq!(after, [3, 9, 9]);
            }
        }
    }

    #[test]
    fn split_output_is_the_whole_decode() {
        let mut s = MAGIC.to_vec();
        for i in 0..40u8 {
            let (kb2, size) = (i % 3 == 0, 1000 + i32::from(i) * 12_000);
            let rounds = if kb2 {
                vec![vec![size, 7], vec![size / 2, 9], vec![1, -1]]
            } else {
                vec![vec![size, 7, 0, 1, 2, 3], vec![size / 2, 9, 0, 0, 0, 0], vec![-1]]
            };
            s.extend(coded(kb2, 2, &rounds, i));
            if i % 7 == 0 {
                s.extend(stored(&[i; 300]));
            }
        }
        s.push(END);
        let whole = fake_decode(Box::new(io::Cursor::new(s.clone())));
        for w in [1, 3, 8] {
            let (got, spawned) = split(&s, w);
            assert!(got.unwrap() == whole, "{w} workers");
            assert!(spawned > 1);
        }
        assert_eq!(walk(Box::new(io::Cursor::new(s.clone()))).unwrap(), (40, 6));
        // Without the end byte: an error, never a short stream.
        let (got, _) = split(&s[..s.len() - 1], 3);
        assert!(got.is_err());
        // A coded type byte without its magic: the stream up to it is split, the rest goes to one
        // cls-zstd, which sees it from that boundary on.
        let mut bad = MAGIC.to_vec();
        bad.extend(coded(true, 1, &[vec![50, 50], vec![-1]], 1));
        bad.extend(coded(true, 1, &[vec![60, 60], vec![-1]], 2));
        let at = bad.len();
        bad.extend(coded(true, 1, &[vec![70, 70], vec![-1]], 3));
        bad[at + 2] = b'X';
        bad.extend(coded(true, 1, &[vec![80, 80], vec![-1]], 4));
        bad.push(END);
        let whole = fake_decode(Box::new(io::Cursor::new(bad.clone())));
        assert!(whole.contains(&b'?'));
        let (got, _) = split(&bad, 4);
        assert!(got.unwrap() == whole);
        // Not a stream of this kind: all of it to one cls-zstd, every byte kept.
        let other = b"ARJ\x01xyz".to_vec();
        let (got, spawned) = split(&other, 3);
        assert_eq!(got.unwrap(), fake_decode(Box::new(io::Cursor::new(other))));
        assert_eq!(spawned, 0);
        match probe(Box::new(io::Cursor::new(b"ARJ\x01xyz".to_vec()))).unwrap() {
            Plan::Single { mut replay, .. } => {
                let mut v = Vec::new();
                replay.read_to_end(&mut v).unwrap();
                assert_eq!(v, b"ARJ\x01xyz");
            }
            Plan::Split { .. } => panic!("split"),
        }
    }

    #[test]
    fn a_stopped_block_starts_nothing() {
        let mut s = MAGIC.to_vec();
        for i in 0..10u8 {
            s.extend(coded(true, 1, &[vec![60_000, 70_000], vec![-1]], i));
        }
        s.push(END);
        let stop = Stop::default();
        stop.aborted.store(true, Ordering::Relaxed);
        let (got, spawned) = split_stop(&s, 4, stop);
        assert!(got.is_err(), "an error, never a short stream");
        assert_eq!(spawned, 0);
    }

    /// A captured cls-zstd input (`ARC_UNPACK_TEE_DIR`) walked to its end marker:
    /// `BIKZ_TEST=<capture>` (and `BIKZ_VIDEOS=<count>` to check the number of videos).
    #[test]
    #[ignore]
    fn real_stream_walk() {
        let path = std::env::var("BIKZ_TEST").expect("BIKZ_TEST=<captured cls-zstd input>");
        let f = std::fs::File::open(&path).unwrap();
        let (coded, stored) = walk(Box::new(io::BufReader::with_capacity(1 << 20, f))).unwrap();
        eprintln!("{path}: {coded} videos, {stored} stored blocks");
        if let Some(n) = std::env::var("BIKZ_VIDEOS").ok().map(|v| v.trim().parse::<u64>().expect("BIKZ_VIDEOS=<count>")) {
            assert_eq!(coded, n);
        }
    }
}
