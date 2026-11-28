//! Parallel xtool decoding.
//!
//! xtool restores its stream one chunk (~400 MB) at a time, and a chunk only ends when its
//! slowest stream ends. With a few big streams per chunk (large PNGs rebuilt with preflate) most
//! of its threads wait: on Factorio's main archive xtool keeps about 5 of 32 threads busy and
//! takes 300 s. Chunks don't depend on each other (unless streams are deduplicated), so this
//! hands whole chunks to several xtool processes and puts their output back in order. xtool
//! still restores every stream itself, so the output is the same.
//!
//! Stream layout (the xtool builds repacks ship, magic "XTL0"): magic, 16 bytes, depth i32,
//! method (u8 length + bytes), resources (i32 count; per resource u8 name length, name, i32 size,
//! data), one zero byte, then chunks until a count of -1. A chunk: count i32, block i64, `count`
//! 18-byte stream headers (kind u8, old i32, new i32, resource i32, codec u8, option i32),
//! `block` bytes of stream data (the sum of `new`), per stream a u32 gap length and the gap,
//! then a trailing u32 gap length and gap. Its output is every gap and every stream's `old` bytes.
//!
//! The 2018 XTool builds (`OLD_XTOOL`, method `xt`) have another layout and the same per-record
//! barrier: see `run_old`.

use crate::stage::Stream;
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

const MAGIC: &[u8; 4] = b"XTL0";

/// The 2018 XTool builds (method `xt`, `xtool.exe d:precomp:t100p - -`; reverse-engineered):
/// 0.2 (Mad Max), 0.6 (Dark Souls Remastered), 0.9 x86 (Prototype, Max Payne 3, Spintires,
/// ETS2) and 0.9 x64 (Octopath, Crysis 3, Rocksmith, RE2, Stray, Civ 6, FS22, A Way Out, Dune).
pub const OLD_XTOOL: [&str; 4] = [
    "cfc5263f290c924c1c20fdac66e59c5a14d3166995fca8b0f139698e4a041f9d",
    "25e63404a25f54a28b5c9664a988739ae140b82cef977d5c99c5a282a2abedc7",
    "17dbbea161ec11e96d7ee9fcd63ada7716a2d27e5c0d8b8cce5f6de6b0c94ca9",
    "0bb81d315a5c73509ea33aded9ce932457a68c7a694c697ae5da1796c80b7a14",
];
const HEADER_LEN: usize = 18;
/// Kind bit of a stream that copies an earlier stream's data (xtool's deduplication). Such a
/// stream may refer to a chunk another worker restored, so these streams can't be split.
const DUPLICATED: u8 = 4;
/// Finished chunk output waiting for an earlier, slower chunk, before no more chunks are handed out.
const OUTPUT_BUDGET: u64 = 1 << 30;

/// Set after a split decode failed on the stream's format: later decodes (the script's retry of
/// the same archive) use one xtool, as before.
static DISABLED: AtomicBool = AtomicBool::new(false);

/// xtool processes at once: `ARC_UNPACK_XTOOL_WORKERS`, else one per 6 hardware threads (2 to 6)
/// as far as free memory allows. 1 = don't split.
pub fn workers() -> usize {
    if DISABLED.load(Ordering::Relaxed) {
        return 1;
    }
    if let Some(n) = std::env::var("ARC_UNPACK_XTOOL_WORKERS").ok().and_then(|v| v.trim().parse::<usize>().ok()) {
        return n.max(1);
    }
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    // Each xtool process takes about 1.5 GB with its chunk; keep a quarter of free memory spare.
    let (free, _) = crate::guard::memory();
    let free = crate::memlimit::get().map_or(free, |limit| free.min(limit));
    let fit = if free > 0 { (free * 3 / 4 / (2 << 30)) as usize } else { 2 };
    (threads / 6).clamp(2, 6).min(fit)
}

/// `xtool decode ... - -` reading stdin and writing stdout.
pub fn is_stream_decode(argv: &[String]) -> bool {
    let exe = std::path::Path::new(argv.first().map(String::as_str).unwrap_or(""))
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    exe.starts_with("xtool")
        && argv.get(1).is_some_and(|c| c.eq_ignore_ascii_case("decode"))
        && argv.len() >= 4
        && argv[argv.len() - 2] == "-"
        && argv[argv.len() - 1] == "-"
}

pub enum Plan {
    /// The stream has the expected layout: its header (sent to every worker) and the rest.
    Split { header: Vec<u8>, rest: Stream },
    /// Unknown layout: decode with one xtool. The bytes already read are put back in front.
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

fn le_i32(b: &[u8]) -> i32 {
    i32::from_le_bytes(b[..4].try_into().unwrap())
}

fn le_i64(b: &[u8]) -> i64 {
    i64::from_le_bytes(b[..8].try_into().unwrap())
}

/// Block bytes of 2018-XTool records given to one process: whole records, at least one (their
/// records are the encoder's 16 MB chunks, so one process each would mostly be start-up).
const OLD_GROUP: u64 = 64 << 20;

/// `xtool.exe d:precomp[:...] - -` of one of the 2018 builds (`OLD_XTOOL`), with no crilayla.dll
/// next to it (with one, those builds copy it to fixed crilayla<N>.dll names in their folder).
pub fn is_old_stream_decode(argv: &[String]) -> bool {
    let exe = std::path::Path::new(argv.first().map(String::as_str).unwrap_or(""));
    let name = exe.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    name.starts_with("xtool")
        && name.ends_with(".exe")
        && argv.len() >= 4
        && argv[1].to_ascii_lowercase().starts_with("d:precomp")
        && argv[argv.len() - 2] == "-"
        && argv[argv.len() - 1] == "-"
        && !exe.with_file_name("crilayla.dll").exists()
        && crate::stage::file_sha256(exe).is_some_and(|h| OLD_XTOOL.contains(&h.as_str()))
}

/// The 2018 XTool's options with its thread count set to `k` (`d:precomp:t100p` -> `d:precomp:t4`,
/// `d:precomp:c16mb,t100p` -> `d:precomp:c16mb,t4`, `d:precomp` -> `d:precomp:t4`). The count only
/// decides how many identical per-thread contexts take streams.
pub fn old_threads_arg(arg: &str, k: usize) -> String {
    let mut fields: Vec<String> = arg.split(':').map(str::to_string).collect();
    if fields.len() < 3 {
        fields.push(format!("t{k}"));
        return fields.join(":");
    }
    let mut opts: Vec<String> = fields[2].split(',').filter(|o| !o.is_empty()).map(str::to_string).collect();
    match opts.iter_mut().find(|o| o.starts_with(['t', 'T'])) {
        Some(t) => *t = format!("t{k}"),
        None => opts.push(format!("t{k}")),
    }
    fields[2] = opts.join(",");
    fields.join(":")
}

/// One 2018-XTool record's leading fields after its count (read into `buf`), and its block size.
/// count > 0: count x {u8 codec, u8 param, i32 size} (negative: |size| bytes kept, nothing
/// written), then i64 block = the sum of |size|.
fn old_record_head(input: &mut Stream, buf: &mut Vec<u8>, count: i32) -> io::Result<std::result::Result<u64, String>> {
    if !(1..=4_000_000).contains(&count) {
        return Ok(Err(format!("implausible stream count {count}")));
    }
    let at = buf.len();
    if !read_exact_into(input, buf, 6 * count as usize + 8)? {
        return Ok(Err("the stream ends inside a record".into()));
    }
    let heads = &buf[at..at + 6 * count as usize];
    let mut sum = 0u64;
    for h in heads.chunks_exact(6) {
        let size = le_i32(&h[2..]);
        if size == i32::MIN {
            return Ok(Err("a stream size out of range".into()));
        }
        sum += size.unsigned_abs() as u64;
    }
    let block = le_i64(&buf[at + 6 * count as usize..]);
    if block < 0 || block as u64 != sum {
        return Ok(Err(format!("a record block of {block} bytes for streams of {sum}")));
    }
    Ok(Ok(sum))
}

/// Reads a 2018-XTool stream's header (an i32 chunk size, no magic) and its first record, and
/// decides whether to split.
pub fn probe_old(mut input: Stream) -> Result<Plan> {
    let mut got = Vec::new();
    let ok = (|| -> io::Result<std::result::Result<(), String>> {
        if !read_exact_into(&mut input, &mut got, 8)? {
            return Ok(Err("too short for the 2018 XTool layout".into()));
        }
        let chunk = le_i32(&got);
        if !(65_536..=1 << 30).contains(&chunk) {
            return Ok(Err(format!("not the 2018 XTool layout (chunk size {chunk})")));
        }
        let count = le_i32(&got[4..]);
        if count < -1 {
            return Ok(Err(format!("not the 2018 XTool layout (count {count})")));
        }
        if count == 0 && !read_exact_into(&mut input, &mut got, 4)? {
            return Ok(Err("the stream ends inside a record".into()));
        }
        if count > 0
            && let Err(e) = old_record_head(&mut input, &mut got, count)?
        {
            return Ok(Err(e));
        }
        Ok(Ok(()))
    })()?;
    Ok(match ok {
        Ok(()) => {
            let rest = got.split_off(4);
            Plan::Split { header: got, rest: Box::new(io::Cursor::new(rest).chain(input)) }
        }
        Err(reason) => Plan::Single { reason, replay: Box::new(io::Cursor::new(got).chain(input)) },
    })
}

/// Reads the stream header and the first chunk's headers, and decides whether to split.
pub fn probe(mut input: Stream) -> Result<Plan> {
    let mut got = Vec::new();
    let ok = (|| -> io::Result<std::result::Result<usize, String>> {
        if !read_exact_into(&mut input, &mut got, 4 + 16 + 4 + 1)? || &got[..4] != MAGIC {
            return Ok(Err("not xtool's XTL0 stream layout".into()));
        }
        let depth = le_i32(&got[20..]);
        if !(1..=16).contains(&depth) {
            return Ok(Err("unexpected depth".into()));
        }
        let method_len = got[24] as usize;
        if !read_exact_into(&mut input, &mut got, method_len + 4)? {
            return Ok(Err("stream ends in its header".into()));
        }
        let resources = le_i32(&got[got.len() - 4..]);
        if !(0..=4096).contains(&resources) {
            return Ok(Err("unexpected resource count".into()));
        }
        for _ in 0..resources {
            if !read_exact_into(&mut input, &mut got, 1)? {
                return Ok(Err("stream ends in its resources".into()));
            }
            let name_len = got[got.len() - 1] as usize;
            if !read_exact_into(&mut input, &mut got, name_len + 4)? {
                return Ok(Err("stream ends in its resources".into()));
            }
            let size = le_i32(&got[got.len() - 4..]);
            if !(0..=(64 << 20)).contains(&size) || !read_exact_into(&mut input, &mut got, size as usize)? {
                return Ok(Err("unexpected resource size".into()));
            }
        }
        if !read_exact_into(&mut input, &mut got, 1)? || got[got.len() - 1] != 0 {
            return Ok(Err("the byte after the resources isn't 0".into()));
        }
        let header_len = got.len();
        // The first chunk must check out too.
        if !read_exact_into(&mut input, &mut got, 4)? {
            return Ok(Err("stream ends before its first chunk".into()));
        }
        let count = le_i32(&got[got.len() - 4..]);
        if !(0..=4_000_000).contains(&count) {
            return Ok(Err("implausible stream count in the first chunk".into()));
        }
        if !read_exact_into(&mut input, &mut got, 8 + HEADER_LEN * count as usize)? {
            return Ok(Err("stream ends in the first chunk's headers".into()));
        }
        let at = header_len + 4;
        let block = le_i64(&got[at..]);
        match check_headers(&got[at + 8..], block) {
            Ok(_) => Ok(Ok(header_len)),
            Err(e) => Ok(Err(format!("{e:#}"))),
        }
    })()
    .context("reading xtool's stream header")?;
    Ok(match ok {
        Ok(header_len) => {
            let first_chunk = got[header_len..].to_vec();
            got.truncate(header_len);
            Plan::Split { header: got, rest: Box::new(io::Cursor::new(first_chunk).chain(input)) }
        }
        Err(reason) => Plan::Single { reason, replay: Box::new(io::Cursor::new(got).chain(input)) },
    })
}

/// Validates a chunk's stream headers against its block size; returns the output size of the
/// streams (gaps come on top).
fn check_headers(headers: &[u8], block: i64) -> Result<u64> {
    let mut new_sum = 0i64;
    let mut old_sum = 0u64;
    for h in headers.chunks_exact(HEADER_LEN) {
        let (kind, old, new) = (h[0], le_i32(&h[1..]), le_i32(&h[5..]));
        ensure!(kind & DUPLICATED == 0, "a deduplicated stream (chunks depend on each other)");
        ensure!(old >= 0 && new >= 0, "negative stream size");
        new_sum += new as i64;
        old_sum += old as u64;
    }
    ensure!(new_sum == block, "stream sizes ({new_sum}) don't add up to the chunk ({block})");
    Ok(old_sum)
}

/// One xtool process (it decodes one chunk).
pub struct Worker {
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub child: Arc<Mutex<Child>>,
    /// Builds the error for a failed exit (with the process's stderr).
    pub exit_error: Box<dyn Fn(std::process::ExitStatus) -> anyhow::Error + Send + Sync>,
    /// Kept alive as long as the process (its working folder).
    pub keep: Box<dyn std::any::Any + Send>,
}

#[derive(Default)]
struct Shared {
    running: usize,
    /// Output size of each chunk handed out, once its gaps have been read.
    expected: BTreeMap<u64, u64>,
    done: BTreeMap<u64, Vec<u8>>,
    next_out: u64,
    /// Output bytes finished but not yet read, plus those expected from chunks in progress.
    pending: u64,
    chunks: Option<u64>,
    failed: Option<String>,
}

struct Coord {
    s: Mutex<Shared>,
    cv: Condvar,
}

impl Coord {
    fn fail(&self, msg: String) {
        let mut s = self.s.lock().unwrap();
        if s.failed.is_none() {
            s.failed = Some(msg);
        }
        self.cv.notify_all();
    }
}

/// Decodes `rest` (the stream after `header`) with one xtool process per chunk, at most
/// `max_workers` at a time, started by `spawn`. Each process gets the header, its chunk and the
/// end marker, so it reads its input to the end and exits (xtool reads ahead: a process kept
/// for several chunks would wait for input beyond its chunk). `abort` kills every process of
/// the block (on any failure).
pub fn run(
    header: Vec<u8>,
    rest: Stream,
    max_workers: usize,
    spawn: impl Fn(u64) -> Result<Worker> + Send + 'static,
    abort: impl Fn() + Send + Sync + 'static,
    run_thread: impl Fn(String, Box<dyn FnOnce() -> Result<()> + Send>),
    label: &str,
) -> Result<Stream> {
    let coord = Arc::new(Coord { s: Mutex::new(Shared::default()), cv: Condvar::new() });
    let abort: Arc<dyn Fn() + Send + Sync> = Arc::new(abort);
    let (coord2, abort2) = (coord.clone(), abort.clone());
    let label2 = label.to_string();
    let dispatcher = move || -> Result<()> {
        let mut input = rest;
        let r = (|| -> Result<()> {
            let mut chunk = 0u64;
            loop {
                let mut head = Vec::with_capacity(12);
                if !read_exact_into(&mut input, &mut head, 4)? {
                    bail!("xtool stream ended without its end marker");
                }
                let count = le_i32(&head);
                if count < 0 {
                    break;
                }
                ensure!(count <= 4_000_000, "implausible stream count {count}");
                ensure!(read_exact_into(&mut input, &mut head, 8 + HEADER_LEN * count as usize)?, "xtool stream cut short");
                let block = le_i64(&head[4..]);
                let old = check_headers(&head[12..], block).map_err(|e| {
                    DISABLED.store(true, Ordering::Relaxed);
                    anyhow!("can't split this xtool stream ({e:#}); the next attempt uses one xtool")
                })?;
                if count == 0 {
                    // No streams: the chunk's output is its gap, as it is.
                    let mut g = Vec::with_capacity(4);
                    ensure!(read_exact_into(&mut input, &mut g, 4)?, "xtool stream cut short");
                    let gap = u32::from_le_bytes(g[..4].try_into().unwrap()) as u64;
                    {
                        let mut s = coord2.s.lock().unwrap();
                        while s.pending + gap > OUTPUT_BUDGET && chunk != s.next_out && s.failed.is_none() {
                            s = coord2.cv.wait(s).unwrap();
                        }
                    }
                    let mut data = Vec::new();
                    ensure!(read_exact_into(&mut input, &mut data, gap as usize)?, "xtool stream cut short");
                    let mut s = coord2.s.lock().unwrap();
                    s.pending += gap;
                    s.done.insert(chunk, data);
                    coord2.cv.notify_all();
                    drop(s);
                    chunk += 1;
                    continue;
                }
                // A free process slot, within the output budget.
                {
                    let mut s = coord2.s.lock().unwrap();
                    loop {
                        if let Some(f) = &s.failed {
                            bail!("{f}");
                        }
                        let room = s.pending + old <= OUTPUT_BUDGET || chunk == s.next_out;
                        if room && s.running < max_workers {
                            s.running += 1;
                            s.pending += old;
                            break;
                        }
                        s = coord2.cv.wait(s).unwrap();
                    }
                }
                let wk = spawn(chunk)?;
                let mut stdin = wk.stdin;
                {
                    let (coord3, abort3, child, exit_error, keep) = (coord2.clone(), abort2.clone(), wk.child, wk.exit_error, wk.keep);
                    let mut stdout = wk.stdout;
                    std::thread::Builder::new()
                        .name(format!("{label2} chunk {chunk}"))
                        .spawn(move || {
                            if let Err(e) = collect(&mut stdout, chunk, &child, &*exit_error, &coord3) {
                                coord3.fail(format!("{e:#}"));
                                abort3();
                            }
                            drop(keep);
                        })
                        .expect("spawn thread");
                }
                // The stream data goes straight in: xtool reads all of it before writing output.
                stdin.write_all(&header).context("writing to xtool")?;
                stdin.write_all(&head).context("writing to xtool")?;
                copy_n(&mut input, &mut stdin, block as u64).context("passing a chunk to xtool")?;
                // xtool reads each gap only after writing the stream before it, so the rest is
                // written from another thread, while the next chunk is handed out.
                let mut tail = Vec::new();
                let mut out = old;
                for _ in 0..=count {
                    let at = tail.len();
                    ensure!(read_exact_into(&mut input, &mut tail, 4)?, "xtool stream cut short");
                    let gap = u32::from_le_bytes(tail[at..at + 4].try_into().unwrap()) as u64;
                    ensure!(read_exact_into(&mut input, &mut tail, gap as usize)?, "xtool stream cut short");
                    out += gap;
                }
                tail.extend_from_slice(&(-1i32).to_le_bytes());
                {
                    let mut s = coord2.s.lock().unwrap();
                    s.pending = s.pending + out - old;
                    s.expected.insert(chunk, out);
                    coord2.cv.notify_all();
                }
                let (coord3, abort3) = (coord2.clone(), abort2.clone());
                std::thread::Builder::new()
                    .name(format!("{label2} chunk {chunk} tail"))
                    .spawn(move || {
                        if let Err(e) = stdin.write_all(&tail) {
                            coord3.fail(format!("writing to xtool: {e}"));
                            abort3();
                        }
                        // Dropping stdin closes it: xtool sees the end of its input.
                    })
                    .expect("spawn thread");
                chunk += 1;
            }
            let mut s = coord2.s.lock().unwrap();
            s.chunks = Some(chunk);
            coord2.cv.notify_all();
            Ok(())
        })();
        if let Err(e) = &r {
            coord2.fail(format!("{e:#}"));
            abort2();
        }
        r
    };
    run_thread(format!("{label} (split)"), Box::new(dispatcher));
    Ok(Box::new(Output { coord, cur: Vec::new(), off: 0, ended: false }))
}

/// A 2018-XTool stream (`header` = its chunk size, then `rest`) restored by one process per group
/// of whole records, at most `max_workers` at a time. Its records don't refer to each other
/// (every codec context is reset per stream, no deduplication; reverse-engineered), but the
/// original reads nothing while a record's last streams restore, which stalls the stage before.
/// Each process gets the header, its records and the end marker (-1); a raw record (count 0) is
/// passed on as it is. This layout keeps no output sizes: the process's exit status and the
/// FitGirl CRC of every file are the checks.
pub fn run_old(
    header: Vec<u8>,
    rest: Stream,
    max_workers: usize,
    spawn: impl Fn(u64) -> Result<Worker> + Send + 'static,
    abort: impl Fn() + Send + Sync + 'static,
    run_thread: impl Fn(String, Box<dyn FnOnce() -> Result<()> + Send>),
    label: &str,
) -> Result<Stream> {
    let coord = Arc::new(Coord { s: Mutex::new(Shared::default()), cv: Condvar::new() });
    let abort: Arc<dyn Fn() + Send + Sync> = Arc::new(abort);
    let (coord2, abort2) = (coord.clone(), abort.clone());
    let label2 = label.to_string();
    let dispatcher = move || -> Result<()> {
        let mut input = rest;
        let r = (|| -> Result<()> {
            let mut item = 0u64;
            // The group being gathered: its process input and its output estimate.
            let mut group: Vec<u8> = Vec::new();
            let mut estimate = 0u64;
            let flush = |group: &mut Vec<u8>, estimate: &mut u64, item: &mut u64| -> Result<()> {
                if group.is_empty() {
                    return Ok(());
                }
                let mut data = Vec::with_capacity(header.len() + group.len() + 4);
                data.extend_from_slice(&header);
                data.append(group);
                data.extend_from_slice(&(-1i32).to_le_bytes());
                let est = std::mem::take(estimate);
                {
                    let mut s = coord2.s.lock().unwrap();
                    loop {
                        if let Some(f) = &s.failed {
                            bail!("{f}");
                        }
                        let room = s.pending + est <= OUTPUT_BUDGET || *item == s.next_out;
                        if room && s.running < max_workers {
                            s.running += 1;
                            s.pending += est;
                            break;
                        }
                        s = coord2.cv.wait(s).unwrap();
                    }
                }
                let wk = spawn(*item)?;
                let n = *item;
                let (coord3, abort3, child, exit_error, keep) = (coord2.clone(), abort2.clone(), wk.child, wk.exit_error, wk.keep);
                let mut stdout = wk.stdout;
                std::thread::Builder::new()
                    .name(format!("{label2} records {n}"))
                    .spawn(move || {
                        if let Err(e) = collect_old(&mut stdout, n, est, &child, &*exit_error, &coord3) {
                            coord3.fail(format!("{e:#}"));
                            abort3();
                        }
                        drop(keep);
                    })
                    .expect("spawn thread");
                // Its input from its own thread, so the next group is gathered meanwhile.
                let (coord3, abort3, mut stdin) = (coord2.clone(), abort2.clone(), wk.stdin);
                std::thread::Builder::new()
                    .name(format!("{label2} records {n} in"))
                    .spawn(move || {
                        if let Err(e) = stdin.write_all(&data) {
                            coord3.fail(format!("writing to xtool: {e}"));
                            abort3();
                        }
                    })
                    .expect("spawn thread");
                *item += 1;
                Ok(())
            };
            let mut block_in_group = 0u64;
            loop {
                let mut head = Vec::with_capacity(16);
                if !read_exact_into(&mut input, &mut head, 4)? {
                    bail!("xtool stream ended without its end marker");
                }
                let count = le_i32(&head);
                if count < 0 {
                    break;
                }
                if count == 0 {
                    // Raw bytes: the output as they are.
                    ensure!(read_exact_into(&mut input, &mut head, 4)?, "xtool stream cut short");
                    let len = le_i32(&head[4..]);
                    let mut data = Vec::new();
                    if len > 0 {
                        ensure!(read_exact_into(&mut input, &mut data, len as usize)?, "xtool stream cut short");
                    }
                    flush(&mut group, &mut estimate, &mut item)?;
                    block_in_group = 0;
                    let mut s = coord2.s.lock().unwrap();
                    while s.pending > 0 && s.pending + data.len() as u64 > OUTPUT_BUDGET && item != s.next_out && s.failed.is_none() {
                        s = coord2.cv.wait(s).unwrap();
                    }
                    s.pending += data.len() as u64;
                    s.done.insert(item, data);
                    coord2.cv.notify_all();
                    drop(s);
                    item += 1;
                    continue;
                }
                let block = match old_record_head(&mut input, &mut head, count)? {
                    Ok(b) => b,
                    Err(e) => {
                        DISABLED.store(true, Ordering::Relaxed);
                        bail!("can't split this xtool stream ({e}); the next attempt uses one xtool");
                    }
                };
                group.extend_from_slice(&head);
                copy_n(&mut input, &mut group, block).context("reading an xtool record")?;
                let mut gaps = 0u64;
                for _ in 0..=count {
                    let at = group.len();
                    ensure!(read_exact_into(&mut input, &mut group, 4)?, "xtool stream cut short");
                    let len = le_i32(&group[at..]);
                    if len > 0 {
                        ensure!(read_exact_into(&mut input, &mut group, len as usize)?, "xtool stream cut short");
                        gaps += len as u64;
                    }
                }
                estimate += block + gaps;
                block_in_group += block;
                if block_in_group >= OLD_GROUP {
                    flush(&mut group, &mut estimate, &mut item)?;
                    block_in_group = 0;
                }
            }
            flush(&mut group, &mut estimate, &mut item)?;
            let mut s = coord2.s.lock().unwrap();
            s.chunks = Some(item);
            coord2.cv.notify_all();
            Ok(())
        })();
        if let Err(e) = &r {
            coord2.fail(format!("{e:#}"));
            abort2();
        }
        r
    };
    run_thread(format!("{label} (split)"), Box::new(dispatcher));
    Ok(Box::new(Output { coord, cur: Vec::new(), off: 0, ended: false }))
}

/// Reads one group's xtool output to the end and checks xtool's exit status; `estimate` (held
/// in the output budget meanwhile) is replaced by the real size.
fn collect_old(
    stdout: &mut ChildStdout,
    item: u64,
    estimate: u64,
    child: &Mutex<Child>,
    exit_error: &(dyn Fn(std::process::ExitStatus) -> anyhow::Error + Send + Sync),
    coord: &Coord,
) -> Result<()> {
    let mut out = Vec::new();
    stdout.read_to_end(&mut out).context("reading xtool's output")?;
    let status = child.lock().unwrap().wait()?;
    if !status.success() {
        return Err(exit_error(status));
    }
    let mut s = coord.s.lock().unwrap();
    s.pending = s.pending - estimate + out.len() as u64;
    s.running -= 1;
    s.done.insert(item, out);
    coord.cv.notify_all();
    Ok(())
}

fn copy_n(input: &mut Stream, out: &mut impl Write, mut n: u64) -> io::Result<()> {
    let mut buf = vec![0u8; (1 << 20).min(n.max(1) as usize)];
    while n > 0 {
        let k = (n as usize).min(buf.len());
        let got = input.read(&mut buf[..k])?;
        if got == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "xtool stream cut short"));
        }
        out.write_all(&buf[..got])?;
        n -= got as u64;
    }
    Ok(())
}

/// Reads one chunk's xtool output to the end, checks its size and xtool's exit status.
fn collect(
    stdout: &mut ChildStdout,
    chunk: u64,
    child: &Mutex<Child>,
    exit_error: &(dyn Fn(std::process::ExitStatus) -> anyhow::Error + Send + Sync),
    coord: &Coord,
) -> Result<()> {
    let mut out = Vec::new();
    stdout.read_to_end(&mut out).context("reading xtool's output")?;
    let status = child.lock().unwrap().wait()?;
    if !status.success() {
        return Err(exit_error(status));
    }
    let mut s = coord.s.lock().unwrap();
    // The size is known once the chunk's gaps were read, which happens before xtool can finish.
    let expected = loop {
        if let Some(e) = s.expected.remove(&chunk) {
            break e;
        }
        if s.failed.is_some() {
            return Ok(());
        }
        s = coord.cv.wait(s).unwrap();
    };
    ensure!(out.len() as u64 == expected, "xtool restored {} bytes of chunk {chunk}, expected {expected}", out.len());
    s.running -= 1;
    s.done.insert(chunk, out);
    coord.cv.notify_all();
    Ok(())
}

/// The restored stream, chunk by chunk in order.
struct Output {
    coord: Arc<Coord>,
    cur: Vec<u8>,
    off: usize,
    ended: bool,
}

impl Read for Output {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.off == self.cur.len() {
            if self.ended {
                return Ok(0);
            }
            let mut s = self.coord.s.lock().unwrap();
            loop {
                if let Some(f) = &s.failed {
                    return Err(io::Error::other(f.clone()));
                }
                let next = s.next_out;
                if let Some(out) = s.done.remove(&next) {
                    s.next_out += 1;
                    s.pending -= out.len() as u64;
                    self.cur = out;
                    self.off = 0;
                    self.coord.cv.notify_all();
                    break;
                }
                if s.chunks == Some(next) {
                    self.ended = true;
                    return Ok(0);
                }
                s = self.coord.cv.wait(s).unwrap();
            }
        }
        let n = buf.len().min(self.cur.len() - self.off);
        buf[..n].copy_from_slice(&self.cur[self.off..self.off + n]);
        self.off += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(chunks: &[Vec<(u8, &[u8], usize)>]) -> Vec<u8> {
        // (kind, stored data, restored size) per stream; gaps of 1 byte before each stream.
        let mut s = Vec::new();
        s.extend_from_slice(MAGIC);
        s.extend_from_slice(&[7u8; 16]);
        s.extend_from_slice(&2i32.to_le_bytes());
        s.push(3);
        s.extend_from_slice(b"png");
        s.extend_from_slice(&1i32.to_le_bytes());
        s.push(1);
        s.extend_from_slice(b"k");
        s.extend_from_slice(&2i32.to_le_bytes());
        s.extend_from_slice(b"ab");
        s.push(0);
        for c in chunks {
            s.extend_from_slice(&(c.len() as i32).to_le_bytes());
            s.extend_from_slice(&(c.iter().map(|x| x.1.len() as i64).sum::<i64>()).to_le_bytes());
            for (kind, data, old) in c {
                s.push(*kind);
                s.extend_from_slice(&(*old as i32).to_le_bytes());
                s.extend_from_slice(&(data.len() as i32).to_le_bytes());
                s.extend_from_slice(&0i32.to_le_bytes());
                s.push(5);
                s.extend_from_slice(&3i32.to_le_bytes());
            }
            for (_, data, _) in c {
                s.extend_from_slice(data);
            }
            for _ in c {
                s.extend_from_slice(&1u32.to_le_bytes());
                s.push(b'g');
            }
            s.extend_from_slice(&0u32.to_le_bytes());
        }
        s.extend_from_slice(&(-1i32).to_le_bytes());
        s
    }

    #[test]
    fn splits_only_streams_it_understands() {
        let ok = stream(&[vec![], vec![(0, b"xyz", 10)], vec![(2, b"q", 4), (0, b"rr", 5)]]);
        match probe(Box::new(io::Cursor::new(ok.clone()))).unwrap() {
            Plan::Split { header, mut rest } => {
                let mut tail = Vec::new();
                rest.read_to_end(&mut tail).unwrap();
                assert_eq!([header, tail].concat(), ok, "header + rest must be the whole stream");
            }
            Plan::Single { reason, .. } => panic!("should split: {reason}"),
        }
        // Deduplicated streams, other layouts: one xtool, with every byte put back.
        for bad in [stream(&[vec![(DUPLICATED, b"x", 3)]]), b"XTL1 something else".to_vec(), b"XT".to_vec()] {
            match probe(Box::new(io::Cursor::new(bad.clone()))).unwrap() {
                Plan::Single { replay: mut s, .. } => {
                    let mut all = Vec::new();
                    s.read_to_end(&mut all).unwrap();
                    assert_eq!(all, bad);
                }
                Plan::Split { .. } => panic!("should not split"),
            }
        }
    }

    /// A 2018-XTool stream: chunk-size header, then records (stream sizes, gaps) and -1.
    fn old_stream(records: &[(&[i32], &[i32])]) -> Vec<u8> {
        let mut s = (16i32 << 20).to_le_bytes().to_vec();
        for (sizes, gaps) in records {
            s.extend_from_slice(&(sizes.len() as i32).to_le_bytes());
            if sizes.is_empty() {
                s.extend_from_slice(&gaps[0].to_le_bytes());
                s.extend(std::iter::repeat_n(9u8, gaps[0].max(0) as usize));
                continue;
            }
            for &z in *sizes {
                s.extend_from_slice(&[0, 96]);
                s.extend_from_slice(&z.to_le_bytes());
            }
            let block: i64 = sizes.iter().map(|z| z.unsigned_abs() as i64).sum();
            s.extend_from_slice(&block.to_le_bytes());
            s.extend(std::iter::repeat_n(1u8, block as usize));
            for &g in *gaps {
                s.extend_from_slice(&g.to_le_bytes());
                s.extend(std::iter::repeat_n(2u8, g.max(0) as usize));
            }
        }
        s.extend_from_slice(&(-1i32).to_le_bytes());
        s
    }

    #[test]
    fn old_xtool_streams() {
        // Records with a skipped stream (negative size), empty gaps (0, -3) and a raw record.
        let data = old_stream(&[(&[5, -3], &[1, 0, -3]), (&[], &[4]), (&[7], &[2, 2])]);
        match probe_old(Box::new(io::Cursor::new(data.clone()))).unwrap() {
            Plan::Split { header, mut rest } => {
                assert_eq!(header, (16i32 << 20).to_le_bytes());
                let mut r = Vec::new();
                rest.read_to_end(&mut r).unwrap();
                assert_eq!([header, r].concat(), data);
            }
            Plan::Single { reason, .. } => panic!("{reason}"),
        }
        // A first record whose block doesn't add up, or an XTL0 stream: one xtool, every byte kept.
        let mut bad = old_stream(&[(&[5, 3], &[1, 1, 1])]);
        bad[4 + 4 + 12] = 9;
        for s in [bad, b"XTL0 something".to_vec()] {
            match probe_old(Box::new(io::Cursor::new(s.clone()))).unwrap() {
                Plan::Single { mut replay, .. } => {
                    let mut r = Vec::new();
                    replay.read_to_end(&mut r).unwrap();
                    assert_eq!(r, s);
                }
                Plan::Split { .. } => panic!("split"),
            }
        }
        assert_eq!(old_threads_arg("d:precomp:t100p", 4), "d:precomp:t4");
        assert_eq!(old_threads_arg("d:precomp:c16mb,t100p", 3), "d:precomp:c16mb,t3");
        assert_eq!(old_threads_arg("d:precomp", 2), "d:precomp:t2");
        assert_eq!(old_threads_arg("d:precomp:c16mb", 2), "d:precomp:c16mb,t2");
    }

    #[test]
    fn xtool_decode_commands() {
        let v = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert!(is_stream_decode(&v(r"C:\x\xtool.exe decode -t100p - -")));
        assert!(!is_stream_decode(&v("xtool.exe decode -t100p in.tmp out.tmp")));
        assert!(!is_stream_decode(&v("xtool.exe d:precomp:t100p - -")));
        assert!(!is_stream_decode(&v("zstdrec.exe d")));
    }
}
