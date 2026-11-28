//! FitGirl's zstdrec (Resident Evil 2, Football Manager 2023), done inside arc-unpack.
//!
//! zstdrec stores the zstd frames of a game's data decompressed, and its decoder (`zstdrec.exe d`,
//! Free Pascal, reverse-engineered) compresses each one again with the repack's own libzstd.dll:
//! one `ZSTD_compress(dst, cap, src, size, level)` per record, with nothing carried between
//! records. It handles one group of records at a time with a barrier after each (groups hold the
//! frames of one 32 MB window of the original data), so it keeps about 6 cores busy and stops
//! reading its input while a group's last records compress, which stalls the stages before it.
//!
//! This reads the same stream and calls the same ZSTD_compress in the same libzstd.dll (the
//! output depends on the library's version, so never another zstd), on every core, keeping
//! several groups in flight, and writes the result in order.
//!
//! Stream (little-endian): groups until a count below 0. A group is `i32 count`; 0 means raw
//! data follows (`i32 n`, n bytes). Otherwise `count` x {`i8 level`, `i32 size`}, `i64 total`,
//! the records' data (`total` bytes, back to back), then `count + 1` gaps (`i32 n`, n bytes).
//! Its output: gap 0, record 0 compressed, gap 1, ..., gap `count`.

use crate::stage::Stream;
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

/// zstdrec.exe (Resident Evil 2; the same file in Football Manager 2023).
const ZSTDREC: (u64, &str) = (538_611, "d44e72517845076dfe232a523252e33f2775399a19776ca2285a60051450b552");
/// Record data read but not yet passed on, beyond which no new group is started (a group holds
/// at most 256 MB, or one bigger frame).
const BUDGET: u64 = 1 << 30;

/// The repack's libzstd.dll, when `argv` is the known `zstdrec.exe d` (both ends piped).
pub fn known(argv: &[String]) -> Option<PathBuf> {
    if std::env::var_os("ARC_UNPACK_NO_ZSTDREC").is_some() || argv.len() != 2 || argv[1] != "d" {
        return None;
    }
    let exe = Path::new(&argv[0]);
    if !exe.file_name()?.to_string_lossy().eq_ignore_ascii_case("zstdrec.exe") || std::fs::metadata(exe).ok()?.len() != ZSTDREC.0 {
        return None;
    }
    // (Even when it is missing: then the stage says so.)
    let dll = exe.with_file_name("libzstd.dll");
    crate::tools::sha256_file(exe).is_ok_and(|h| crate::extract::hex(&h) == ZSTDREC.1).then_some(dll)
}

type Compress = unsafe extern "C" fn(*mut u8, usize, *const u8, usize, i32) -> usize;
type IsError = unsafe extern "C" fn(usize) -> u32;
type Bound = unsafe extern "C" fn(usize) -> usize;

#[derive(Clone, Copy)]
struct Zstd {
    compress: Compress,
    is_error: Option<IsError>,
    bound: Option<Bound>,
}

/// Loads the library once per content (it stays loaded). The copy that is loaded lives outside
/// the run's temp folder: a loaded DLL can't be deleted or overwritten, and the next install
/// clears and refills that folder.
fn load(dll: &Path) -> Result<Zstd> {
    static LOADED: Mutex<Vec<(String, Zstd)>> = Mutex::new(Vec::new());
    let hash = crate::extract::hex(&crate::tools::sha256_file(dll).with_context(|| format!("read {}", dll.display()))?);
    let mut loaded = LOADED.lock().unwrap();
    if let Some((_, z)) = loaded.iter().find(|(h, _)| *h == hash) {
        return Ok(*z);
    }
    let dir = std::env::temp_dir().join("arc-unpack-libzstd").join(&hash[..16]);
    let copy = dir.join("libzstd.dll");
    if !crate::tools::sha256_file(&copy).is_ok_and(|h| crate::extract::hex(&h) == hash) {
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let part = dir.join(format!("libzstd.{}.part", std::process::id()));
        std::fs::copy(dll, &part).with_context(|| format!("copy {}", dll.display()))?;
        // (Another arc-unpack may have put it there first, and may have it loaded.)
        if std::fs::rename(&part, &copy).is_err() {
            let _ = std::fs::remove_file(&part);
        }
        ensure!(crate::tools::sha256_file(&copy).is_ok_and(|h| crate::extract::hex(&h) == hash), "{} is not a copy of {}", copy.display(), dll.display());
    }
    let z = load_new(&copy)?;
    loaded.push((hash, z));
    Ok(z)
}

#[cfg(windows)]
fn load_new(dll: &Path) -> Result<Zstd> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    let wide: Vec<u16> = dll.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: a NUL-terminated path; the module is never unloaded, so its functions stay valid.
    unsafe {
        let m = LoadLibraryW(wide.as_ptr());
        ensure!(!m.is_null(), "can't load {}", dll.display());
        let compress = GetProcAddress(m, c"ZSTD_compress".as_ptr().cast()).ok_or_else(|| anyhow!("{} has no ZSTD_compress", dll.display()))?;
        let is_error = GetProcAddress(m, c"ZSTD_isError".as_ptr().cast());
        let bound = GetProcAddress(m, c"ZSTD_compressBound".as_ptr().cast());
        Ok(Zstd {
            compress: std::mem::transmute::<unsafe extern "system" fn() -> isize, Compress>(compress),
            is_error: is_error.map(|f| std::mem::transmute::<unsafe extern "system" fn() -> isize, IsError>(f)),
            bound: bound.map(|f| std::mem::transmute::<unsafe extern "system" fn() -> isize, Bound>(f)),
        })
    }
}

#[cfg(not(windows))]
fn load_new(dll: &Path) -> Result<Zstd> {
    bail!("{}: libzstd.dll needs Windows", dll.display())
}

impl Zstd {
    /// One record, as zstdrec compresses it. The buffer has zstd's worst-case room: with only
    /// the record's size, a frame a few bytes smaller than its data fails (zstd's bit writer
    /// keeps a margin), where zstdrec.exe's buffer, the largest record that worker has seen,
    /// nearly always has room. When the call succeeds, the room doesn't change the bytes.
    fn record(&self, data: &[u8], level: i32) -> Result<Vec<u8>> {
        let len = data.len();
        // SAFETY: a pure function of its argument.
        let bound = self.bound.map(|f| unsafe { f(len) }).filter(|&b| b >= len && b < len.saturating_mul(2).max(1 << 20));
        let mut out = vec![0u8; bound.unwrap_or(len + len / 128 + 1024).max(len + 1024)];
        // SAFETY: both buffers are valid for the lengths given; ZSTD_compress is thread-safe.
        let n = unsafe { (self.compress)(out.as_mut_ptr(), out.len(), data.as_ptr(), len, level) };
        let error = match self.is_error {
            // SAFETY: as above.
            Some(f) => unsafe { f(n) != 0 },
            None => n > out.len(),
        };
        // (The encoder keeps only frames smaller than their data.)
        ensure!(!error && n < len.max(1), "libzstd failed on a {len}-byte record at level {level}");
        out.truncate(n);
        out.shrink_to_fit();
        Ok(out)
    }
}

enum Item {
    Bytes(Vec<u8>),
    /// A record's result, by id, and how much of the budget it holds.
    Record(u64, u64),
}

#[derive(Default)]
struct Shared {
    /// Records to compress: (id, level, data).
    jobs: VecDeque<(u64, i32, Vec<u8>)>,
    results: HashMap<u64, Vec<u8>>,
    /// The output, in order, as far as the stream has been read.
    items: VecDeque<Item>,
    /// Record bytes read but not yet passed on.
    pending: u64,
    read_all: bool,
    failed: Option<String>,
}

struct Coord {
    s: Mutex<Shared>,
    cv: Condvar,
}

impl Coord {
    /// Waits until `n` more bytes fit in the budget (or nothing else is held), and takes them.
    fn reserve(&self, n: u64) -> Result<()> {
        let mut s = self.s.lock().unwrap();
        while s.pending > 0 && s.pending + n > BUDGET && s.failed.is_none() {
            s = self.cv.wait(s).unwrap();
        }
        if let Some(f) = &s.failed {
            bail!("{f}");
        }
        s.pending += n;
        Ok(())
    }

    fn fail(&self, msg: String) {
        let mut s = self.s.lock().unwrap();
        if s.failed.is_none() {
            s.failed = Some(msg);
        }
        self.cv.notify_all();
    }
}

fn read_n(input: &mut Stream, n: usize) -> Result<Vec<u8>> {
    let mut b = vec![0u8; n];
    input.read_exact(&mut b).context("the zstdrec stream ends early")?;
    Ok(b)
}

fn i32_at(input: &mut Stream) -> Result<i32> {
    Ok(i32::from_le_bytes(read_n(input, 4)?.try_into().unwrap()))
}

/// A loaded libzstd.dll.
pub struct Library(Zstd);

/// Loads the repack's libzstd.dll (before any input is read, so the original can run instead).
pub fn open(dll: &Path) -> Result<Library> {
    load(dll).map(Library)
}

/// Restores a zstdrec stream with the library's ZSTD_compress on `workers` threads.
/// `run_thread` starts the block's threads; `report` gets each worker's CPU and wall seconds.
pub fn run(
    lib: &Library,
    input: Stream,
    workers: usize,
    run_thread: impl Fn(String, Box<dyn FnOnce() -> Result<()> + Send>),
    report: impl Fn(f64, f64) + Send + Sync + 'static,
) -> Stream {
    let z = lib.0;
    let coord = Arc::new(Coord { s: Mutex::new(Shared::default()), cv: Condvar::new() });
    {
        let coord = coord.clone();
        run_thread(
            "zstdrec (reader)".into(),
            Box::new(move || {
                let r = read_groups(input, &coord);
                if let Err(e) = &r {
                    coord.fail(format!("{e:#}"));
                }
                r
            }),
        );
    }
    let report = Arc::new(report);
    for w in 0..workers.max(1) {
        let (coord, report) = (coord.clone(), report.clone());
        std::thread::Builder::new()
            .name(format!("zstdrec {w}"))
            .spawn(move || {
                let (t0, c0) = (std::time::Instant::now(), crate::perf::thread_cpu_seconds());
                compress_jobs(&coord, z);
                if let (Some(c0), Some(c1)) = (c0, crate::perf::thread_cpu_seconds()) {
                    report(c1 - c0, t0.elapsed().as_secs_f64());
                }
            })
            .expect("spawn thread");
    }
    Box::new(Output { coord, cur: Vec::new(), off: 0, release: 0 })
}

fn read_groups(mut input: Stream, coord: &Coord) -> Result<()> {
    let mut id = 0u64;
    loop {
        let count = i32_at(&mut input)?;
        if count < 0 {
            break;
        }
        if count == 0 {
            let n = i32_at(&mut input)?;
            ensure!(n >= 0, "a negative raw length in the zstdrec stream");
            // In pieces, each within the budget: everything before it is already passed on or
            // ready to be, so the wait always ends.
            let mut left = n as usize;
            while left > 0 {
                let k = left.min(8 << 20);
                coord.reserve(k as u64)?;
                let data = read_n(&mut input, k)?;
                coord.s.lock().unwrap().items.push_back(Item::Bytes(data));
                coord.cv.notify_all();
                left -= k;
            }
            continue;
        }
        ensure!(count <= 1 << 20, "implausible zstdrec record count {count}");
        let table = read_n(&mut input, 5 * count as usize)?;
        let recs: Vec<(i32, u64)> = table
            .chunks_exact(5)
            .map(|c| (c[0] as i8 as i32, i32::from_le_bytes(c[1..5].try_into().unwrap())))
            .map(|(level, size)| (level, size.max(-1) as i64 as u64))
            .collect();
        ensure!(recs.iter().all(|&(_, size)| size <= i32::MAX as u64), "a negative record size in the zstdrec stream");
        let total = i64::from_le_bytes(read_n(&mut input, 8)?.try_into().unwrap());
        let sum: u64 = recs.iter().map(|r| r.1).sum();
        ensure!(total >= 0 && total as u64 == sum, "zstdrec group of {sum} bytes says {total}");
        // A new group only within the budget (or when nothing else is in flight).
        coord.reserve(sum)?;
        let first = id;
        for &(level, size) in &recs {
            let data = read_n(&mut input, size as usize)?;
            coord.s.lock().unwrap().jobs.push_back((id, level, data));
            coord.cv.notify_all();
            id += 1;
        }
        // (A group's gaps join the budget without waiting: its records can't be passed on
        // before them. They cover one 32 MB window of the original data.)
        let (mut items, mut gaps) = (Vec::with_capacity(2 * recs.len() + 1), 0u64);
        for (i, &(_, size)) in recs.iter().enumerate() {
            let n = i32_at(&mut input)?;
            ensure!(n >= 0, "a negative gap length in the zstdrec stream");
            items.push(Item::Bytes(read_n(&mut input, n as usize)?));
            items.push(Item::Record(first + i as u64, size));
            gaps += n as u64;
        }
        let n = i32_at(&mut input)?;
        ensure!(n >= 0, "a negative gap length in the zstdrec stream");
        items.push(Item::Bytes(read_n(&mut input, n as usize)?));
        gaps += n as u64;
        let mut s = coord.s.lock().unwrap();
        s.pending += gaps;
        s.items.extend(items);
        coord.cv.notify_all();
    }
    let mut s = coord.s.lock().unwrap();
    s.read_all = true;
    coord.cv.notify_all();
    Ok(())
}

fn compress_jobs(coord: &Coord, z: Zstd) {
    loop {
        let (id, level, data) = {
            let mut s = coord.s.lock().unwrap();
            loop {
                if s.failed.is_some() {
                    return;
                }
                if let Some(j) = s.jobs.pop_front() {
                    break j;
                }
                if s.read_all {
                    return;
                }
                s = coord.cv.wait(s).unwrap();
            }
        };
        match z.record(&data, level) {
            Ok(out) => {
                drop(data);
                coord.s.lock().unwrap().results.insert(id, out);
                coord.cv.notify_all();
            }
            Err(e) => return coord.fail(format!("{e:#}")),
        }
    }
}

/// The restored stream, in order.
struct Output {
    coord: Arc<Coord>,
    cur: Vec<u8>,
    off: usize,
    /// Budget bytes to give back once `cur` is passed on.
    release: u64,
}

impl Read for Output {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.off == self.cur.len() {
            let mut s = self.coord.s.lock().unwrap();
            if self.release > 0 {
                s.pending -= self.release;
                self.release = 0;
                self.coord.cv.notify_all();
            }
            loop {
                if let Some(f) = &s.failed {
                    return Err(io::Error::other(f.clone()));
                }
                match s.items.front() {
                    Some(Item::Bytes(_)) => {
                        let Some(Item::Bytes(b)) = s.items.pop_front() else { unreachable!() };
                        self.release = b.len() as u64;
                        self.cur = b;
                        break;
                    }
                    Some(&Item::Record(id, size)) => {
                        if let Some(out) = s.results.remove(&id) {
                            s.items.pop_front();
                            self.cur = out;
                            self.release = size;
                            break;
                        }
                    }
                    None if s.read_all && s.jobs.is_empty() => return Ok(0),
                    None => {}
                }
                s = self.coord.cv.wait(s).unwrap();
            }
            self.off = 0;
        }
        let n = buf.len().min(self.cur.len() - self.off);
        buf[..n].copy_from_slice(&self.cur[self.off..self.off + n]);
        self.off += n;
        Ok(n)
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        // Stops the reader and the workers if the stream wasn't read to the end.
        let mut s = self.coord.s.lock().unwrap();
        if s.failed.is_none() && !(s.read_all && s.items.is_empty()) {
            s.failed = Some("stopped".into());
        }
        self.coord.cv.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real zstdrec: ZSTDREC_TEST=<stream>|<original data>|<libzstd.dll>.
    #[test]
    #[ignore]
    fn real_libzstd() {
        let v = std::env::var("ZSTDREC_TEST").unwrap();
        let [stream, orig, dll]: [&str; 3] = v.split('|').collect::<Vec<_>>().try_into().unwrap();
        let threads = |label: String, f: Box<dyn FnOnce() -> Result<()> + Send>| {
            std::thread::Builder::new().name(label).spawn(move || f().unwrap()).unwrap();
        };
        let t = std::time::Instant::now();
        let input: Stream = Box::new(io::BufReader::new(std::fs::File::open(stream).unwrap()));
        let lib = open(Path::new(dll)).unwrap();
        let mut out = run(&lib, input, 8, threads, |_, _| {});
        let mut got = Vec::new();
        out.read_to_end(&mut got).unwrap();
        let want = std::fs::read(orig).unwrap();
        println!("restored {} bytes in {:.1} s, original {}", got.len(), t.elapsed().as_secs_f64(), want.len());
        assert!(got == want);
    }
}
