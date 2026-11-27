//! Faster stand-ins for repack tools whose exact behaviour we know from reverse engineering
//! them, recognised by their SHA-256 (FitGirl ships the same tool under different names: her
//! XOR tool is xx.exe in one repack and x.exe in another).
//!
//! Before the install script runs a program, arc-unpack replaces each known tool in its own
//! temporary folder with `arc-unpack-tool.exe` and moves the original aside
//! ([`substitute`]). The stand-in does the work itself for the exact command lines it
//! understands and runs the original for anything else, so the output is the original's in
//! every case. With ARC_UNPACK_TOOL_VERIFY=1 it runs both and compares (see the tool binary).
//!
//! Tools:
//! - Luigi Auriemma's "Xor 0.2" (xx.exe): `xx <in> <out> 0x<hex key>`. It XORs the file with the
//!   key, repeated, 8 KB at a time, byte by byte. Here: in big buffers.
//! - The LZ4 1.9.4 command-line tool (a MinGW build, as fgpack3.exe/lz4.exe): `lz4 -12 <in>
//!   <out>`. It compresses 4 MB blocks one after the other on one core; the blocks are
//!   independent, so here they are compressed on all cores with the same library (lz4-sys
//!   bundles LZ4 1.9.4), making the same calls per block.
//! - FitGirl's x2n.exe patcher: `x2n <target> <data> <map>` writes runs of data into the target
//!   at the map's offsets, three system calls per run of at most 256 bytes. Here: the target
//!   mapped into memory, the data read in bulk (see `x2n_apply`).

use anyhow::{Context, Result, bail, ensure};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// What a known tool is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Xor,
    Lz4Cli194,
    X2n,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Role::Xor => "xor",
            Role::Lz4Cli194 => "lz4-1.9.4",
            Role::X2n => "x2n",
        }
    }

    pub fn from_name(s: &str) -> Option<Role> {
        KNOWN.iter().map(|k| k.2).find(|r| r.name() == s)
    }
}

/// (size, SHA-256, role) of the tools we stand in for.
const KNOWN: &[(u64, &str, Role)] = &[
    (10752, "d363eff713456c2249a9a7197cb0e2fd6b189a83ad7fb3a3d3386517b35a8ccc", Role::Xor),
    (304640, "f0eb4df3daedbba4764afca1e18a9c4b716df0917f51382c99e120c9733e87f6", Role::Lz4Cli194),
    (7680, "62410f076016456299e3cb5b3311b8a8d66fecd299dde9e7b6815fd581036428", Role::X2n),
];

/// The stand-in program's file name (shipped next to arc-unpack or embedded in it).
pub const PROGRAM: &str = "arc-unpack-tool.exe";
/// The list of stand-ins, in arc-unpack's run folder (the parent of its temporary folder).
pub const MAP_FILE: &str = "tool-standins.txt";
/// The environment variable that tells a stand-in where the list of stand-ins is.
pub const MAP_ENV: &str = "ARC_UNPACK_TOOL_MAP";
/// Set to 1: stand-ins run the original too and compare (results in `<map>.verify.log`).
pub const VERIFY_ENV: &str = "ARC_UNPACK_TOOL_VERIFY";

/// The role of the file at `path`, if it is a tool we know.
pub fn identify(path: &Path) -> Option<Role> {
    let len = std::fs::metadata(path).ok()?.len();
    let candidates: Vec<_> = KNOWN.iter().filter(|k| k.0 == len).collect();
    if candidates.is_empty() {
        return None;
    }
    let digest = crate::extract::hex(&sha256_file(path).ok()?);
    candidates.iter().find(|k| k.1 == digest).map(|k| k.2)
}

fn sha256_file(path: &Path) -> std::io::Result<[u8; 32]> {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().into())
}

/// One stand-in: where it is, what it stands in for, and where the original went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub role: Role,
    pub original: PathBuf,
}

/// Reads the list of stand-ins (`<path>\t<role>\t<original>` lines).
pub fn read_map(map: &Path) -> Vec<Entry> {
    let text = std::fs::read_to_string(map).unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            let mut p = l.split('\t');
            let (path, role, original) = (p.next()?, p.next()?, p.next()?);
            Some(Entry { path: PathBuf::from(path), role: Role::from_name(role)?, original: PathBuf::from(original) })
        })
        .collect()
}

/// The stand-in at `path` (compared case-insensitively, as Windows does).
pub fn lookup(map: &Path, path: &Path) -> Option<Entry> {
    let key = |p: &Path| p.to_string_lossy().replace('/', "\\").to_lowercase();
    let want = key(path);
    read_map(map).into_iter().find(|e| key(&e.path) == want)
}

/// Replaces the known tools under `dir` with copies of `stand_in`, moving each original into
/// `originals` and listing it in `map`. Tools already replaced are left alone. Returns what it
/// replaced, for the log.
pub fn substitute(dir: &Path, stand_in: &Path, originals: &Path, map: &Path) -> Result<Vec<String>> {
    let mut done = Vec::new();
    let known: Vec<PathBuf> = read_map(map).into_iter().map(|e| e.path).collect();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push(p);
                continue;
            }
            if !ft.is_file() || !p.extension().is_some_and(|x| x.eq_ignore_ascii_case("exe")) || known.contains(&p) {
                continue;
            }
            let Some(role) = identify(&p) else { continue };
            std::fs::create_dir_all(originals)?;
            let original = originals.join(format!("{}-{}", done.len() + known.len(), p.file_name().unwrap_or_default().to_string_lossy()));
            std::fs::rename(&p, &original).with_context(|| format!("move {} aside", p.display()))?;
            if let Err(e) = std::fs::copy(stand_in, &p) {
                let _ = std::fs::rename(&original, &p);
                return Err(e).with_context(|| format!("put the stand-in for {}", p.display()));
            }
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(map)?;
            writeln!(f, "{}\t{}\t{}", p.display(), role.name(), original.display())?;
            done.push(format!("{} ({})", p.file_name().unwrap_or_default().to_string_lossy(), role.name()));
        }
    }
    Ok(done)
}

// ---- Xor 0.2 -----------------------------------------------------------------------------

/// The key of `xx <in> <out> 0x<hex>`: hex digit pairs in string order. None for any other
/// form (the stand-in then runs the original).
pub fn xor_key(arg: &str) -> Option<Vec<u8>> {
    let hex = arg.strip_prefix("0x").or_else(|| arg.strip_prefix("0X"))?;
    if hex.is_empty() || hex.len() % 2 != 0 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok()).collect()
}

/// `out[i] = in[i] ^ key[i % key.len()]` over the whole file, as Xor 0.2 does.
pub fn xor_file(input: &Path, output: &Path, key: &[u8]) -> Result<()> {
    ensure!(!key.is_empty(), "empty key");
    let mut src = std::fs::File::open(input).with_context(|| format!("open {}", input.display()))?;
    let mut dst = std::fs::File::create(output).with_context(|| format!("create {}", output.display()))?;
    // A buffer whose size is a multiple of the key length keeps the key position at 0 per read.
    let size = (8 << 20) / key.len() * key.len();
    let mut buf = vec![0u8; size];
    loop {
        let mut n = 0;
        while n < buf.len() {
            let r = src.read(&mut buf[n..])?;
            if r == 0 {
                break;
            }
            n += r;
        }
        if n == 0 {
            break;
        }
        xor_in_place(&mut buf[..n], key);
        dst.write_all(&buf[..n])?;
        if n < buf.len() {
            break;
        }
    }
    dst.flush()?;
    Ok(())
}

fn xor_in_place(data: &mut [u8], key: &[u8]) {
    if key.len() == 8 {
        let k = u64::from_le_bytes(key.try_into().unwrap());
        let mut chunks = data.chunks_exact_mut(8);
        for c in &mut chunks {
            let v = u64::from_le_bytes((&*c).try_into().unwrap()) ^ k;
            c.copy_from_slice(&v.to_le_bytes());
        }
        for (i, b) in chunks.into_remainder().iter_mut().enumerate() {
            *b ^= key[i];
        }
    } else {
        for (i, b) in data.iter_mut().enumerate() {
            *b ^= key[i % key.len()];
        }
    }
}

// ---- LZ4 1.9.4 command-line tool ---------------------------------------------------------

/// The LZ4 tool's block size (its default -B7) and frame settings: independent blocks, a
/// content checksum, no block checksums, no content size, auto-flush.
const LZ4_BLOCK: usize = 4 << 20;

fn lz4_prefs(level: i32, block_size_id: lz4_sys::BlockSize) -> lz4_sys::LZ4FPreferences {
    lz4_sys::LZ4FPreferences {
        // (lz4-sys folds frameType, contentSize, dictID and blockChecksumFlag into `reserved`,
        // at the same offsets: all 0 = a normal frame, no content size, no block checksums.)
        frame_info: lz4_sys::LZ4FFrameInfo {
            block_size_id,
            block_mode: lz4_sys::BlockMode::Independent,
            content_checksum_flag: lz4_sys::ContentChecksum::ChecksumEnabled,
            reserved: [0; 5],
        },
        compression_level: level as u32,
        auto_flush: 1,
        favor_dec_speed: 0,
        reserved: [0; 3],
    }
}

/// An LZ4F compression context.
struct Cctx(lz4_sys::LZ4FCompressionContext);

impl Cctx {
    fn new() -> Result<Cctx> {
        let mut ctx = lz4_sys::LZ4FCompressionContext(std::ptr::null_mut());
        check(unsafe { lz4_sys::LZ4F_createCompressionContext(&mut ctx, lz4_sys::LZ4F_VERSION) })?;
        Ok(Cctx(ctx))
    }
}

impl Drop for Cctx {
    fn drop(&mut self) {
        unsafe { lz4_sys::LZ4F_freeCompressionContext(self.0) };
    }
}

fn check(code: usize) -> Result<usize> {
    if unsafe { lz4_sys::LZ4F_isError(code) } != 0 {
        let name = unsafe { std::ffi::CStr::from_ptr(lz4_sys::LZ4F_getErrorName(code)) };
        bail!("lz4: {}", name.to_string_lossy());
    }
    Ok(code)
}

fn begin(ctx: &Cctx, prefs: &lz4_sys::LZ4FPreferences, out: &mut Vec<u8>) -> Result<()> {
    let mut buf = vec![0u8; 32];
    let n = check(unsafe { lz4_sys::LZ4F_compressBegin(ctx.0, buf.as_mut_ptr(), buf.len(), prefs) })?;
    out.extend_from_slice(&buf[..n]);
    Ok(())
}

fn update(ctx: &Cctx, prefs: &lz4_sys::LZ4FPreferences, src: &[u8], stable: bool, out: &mut Vec<u8>) -> Result<()> {
    let bound = unsafe { lz4_sys::LZ4F_compressBound(src.len(), prefs) };
    let mut buf = vec![0u8; bound];
    let opts = lz4_sys::LZ4FCompressOptions { stable_src: stable as u32, reserved: [0; 3] };
    let n = check(unsafe { lz4_sys::LZ4F_compressUpdate(ctx.0, buf.as_mut_ptr(), buf.len(), src.as_ptr(), src.len(), &opts) })?;
    out.extend_from_slice(&buf[..n]);
    Ok(())
}

fn end(ctx: &Cctx, prefs: &lz4_sys::LZ4FPreferences, out: &mut Vec<u8>) -> Result<()> {
    let bound = unsafe { lz4_sys::LZ4F_compressBound(0, prefs) };
    let mut buf = vec![0u8; bound.max(16)];
    let opts = lz4_sys::LZ4FCompressOptions { stable_src: 0, reserved: [0; 3] };
    let n = check(unsafe { lz4_sys::LZ4F_compressEnd(ctx.0, buf.as_mut_ptr(), buf.len(), &opts) })?;
    out.extend_from_slice(&buf[..n]);
    Ok(())
}

/// LZ4F_optimalBSID: the smallest block size that holds `size` bytes, up to the requested one.
fn optimal_block_size(size: usize) -> lz4_sys::BlockSize {
    use lz4_sys::BlockSize::*;
    match size {
        n if n <= 64 << 10 => Max64KB,
        n if n <= 256 << 10 => Max256KB,
        n if n <= 1 << 20 => Max1MB,
        _ => Max4MB,
    }
}

/// The whole output of `lz4 -<level> <in> <out>` (LZ4 1.9.4's lz4io) for a file smaller than
/// one block: LZ4F_compressFrame (the optimal block size, one block).
fn lz4_single_block(data: &[u8], level: i32) -> Result<Vec<u8>> {
    let prefs = lz4_prefs(level, optimal_block_size(data.len()));
    let ctx = Cctx::new()?;
    let mut out = Vec::with_capacity(data.len() + 64);
    begin(&ctx, &prefs, &mut out)?;
    update(&ctx, &prefs, data, true, &mut out)?;
    end(&ctx, &prefs, &mut out)?;
    Ok(out)
}

/// One 4 MB block as lz4io's LZ4F_compressUpdate emits it (its size word and data), from a
/// context of its own: the blocks are independent, so this is what the tool's single context
/// produces for it too.
fn lz4_block(data: &[u8], level: i32) -> Result<Vec<u8>> {
    let prefs = lz4_prefs(level, lz4_sys::BlockSize::Max4MB);
    let ctx = Cctx::new()?;
    let mut header = Vec::new();
    begin(&ctx, &prefs, &mut header)?;
    let mut out = Vec::with_capacity(data.len() + 16);
    update(&ctx, &prefs, data, false, &mut out)?;
    Ok(out)
}

/// `lz4 -<level> <input> <output>` as the LZ4 1.9.4 tool writes it, with the blocks compressed on
/// up to `threads` cores.
pub fn lz4_compress_file(input: &Path, output: &Path, level: i32, threads: usize) -> Result<()> {
    let mut src = std::fs::File::open(input).with_context(|| format!("open {}", input.display()))?;
    let mut dst = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(output).with_context(|| format!("create {}", output.display()))?);
    let read_block = |src: &mut std::fs::File, buf: &mut Vec<u8>| -> std::io::Result<usize> {
        buf.resize(LZ4_BLOCK, 0);
        let mut n = 0;
        while n < LZ4_BLOCK {
            let r = src.read(&mut buf[n..])?;
            if r == 0 {
                break;
            }
            n += r;
        }
        buf.truncate(n);
        Ok(n)
    };
    let mut first = Vec::new();
    read_block(&mut src, &mut first)?;
    if first.len() < LZ4_BLOCK {
        dst.write_all(&lz4_single_block(&first, level)?)?;
        dst.flush()?;
        return Ok(());
    }
    // Several blocks: the header from LZ4F_compressBegin, every block, the end mark and the
    // XXH32 of the content (what LZ4F_compressEnd writes with a content checksum).
    let prefs = lz4_prefs(level, lz4_sys::BlockSize::Max4MB);
    let mut header = Vec::new();
    begin(&Cctx::new()?, &prefs, &mut header)?;
    dst.write_all(&header)?;
    let mut hasher = xxhash_rust::xxh32::Xxh32::new(0);
    let threads = threads.max(1);
    let mut pending = vec![first];
    loop {
        while pending.len() < threads * 2 {
            let mut b = Vec::new();
            if read_block(&mut src, &mut b)? == 0 {
                break;
            }
            let short = b.len() < LZ4_BLOCK;
            pending.push(b);
            if short {
                break;
            }
        }
        if pending.is_empty() {
            break;
        }
        let blocks = std::mem::take(&mut pending);
        for b in &blocks {
            hasher.update(b);
        }
        let compressed: Vec<Result<Vec<u8>>> = std::thread::scope(|s| {
            let per = blocks.len().div_ceil(threads);
            let handles: Vec<_> = blocks
                .chunks(per)
                .map(|group| s.spawn(move || group.iter().map(|b| lz4_block(b, level)).collect::<Vec<_>>()))
                .collect();
            handles.into_iter().flat_map(|h| h.join().unwrap_or_else(|_| vec![Err(anyhow::anyhow!("lz4 worker panicked"))])).collect()
        });
        for c in compressed {
            dst.write_all(&c?)?;
        }
        let last = blocks.last().is_some_and(|b| b.len() < LZ4_BLOCK);
        if last {
            break;
        }
    }
    dst.write_all(&[0, 0, 0, 0])?;
    dst.write_all(&hasher.digest().to_le_bytes())?;
    dst.flush()?;
    Ok(())
}

/// The output of the LZ4 1.9.4 tool's own loop (one context, block after block), for tests.
#[cfg(test)]
fn lz4_sequential(data: &[u8], level: i32) -> Result<Vec<u8>> {
    if data.len() < LZ4_BLOCK {
        return lz4_single_block(data, level);
    }
    let prefs = lz4_prefs(level, lz4_sys::BlockSize::Max4MB);
    let ctx = Cctx::new()?;
    let mut out = Vec::new();
    begin(&ctx, &prefs, &mut out)?;
    for b in data.chunks(LZ4_BLOCK) {
        update(&ctx, &prefs, b, false, &mut out)?;
    }
    end(&ctx, &prefs, &mut out)?;
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// x2n (FitGirl's patcher, 7,680 B; Rocksmith's audio.psarc, Max Payne 3's audio.rpf, RE2, ...):
// `x2n <target> <data> <map>`. Reverse-engineered (main FUN_00401240): data and map are read,
// the target is opened read/write (it must exist). The map is a u64 LE new size, then records of
// [7-byte LE offset][1-byte length, 0 = 256]; for each, in order, the offset is clamped to the
// new size and `length` bytes, read in sequence from data, are written there. At the end the
// target's length is set to the new size. The original makes three unbuffered system calls per
// record of at most 256 bytes; here the target is mapped into memory and data is read in bulk.

/// The records of an x2n map: (new size, [(offset, length)]). None for a map whose tail isn't a
/// whole number of records (the original's handling of a half record is its own).
fn x2n_records(map: &[u8]) -> Option<(u64, Vec<(u64, usize)>)> {
    if map.len() < 8 || (map.len() - 8) % 8 != 0 {
        return None;
    }
    let new_size = u64::from_le_bytes(map[..8].try_into().ok()?);
    let records = map[8..]
        .chunks_exact(8)
        .map(|r| {
            let mut off = [0u8; 8];
            off[..7].copy_from_slice(&r[..7]);
            let len = if r[7] == 0 { 256 } else { r[7] as usize };
            (u64::from_le_bytes(off).min(new_size), len)
        })
        .collect();
    Some((new_size, records))
}

/// An x2n map checked and ready to apply.
pub struct X2nPlan {
    new_size: u64,
    records: Vec<(u64, usize)>,
}

/// Reads and checks an x2n map against its data. An error here means: leave it to the original
/// (nothing has been touched).
pub fn x2n_plan(data: &Path, map: &Path) -> Result<X2nPlan> {
    let map = std::fs::read(map).with_context(|| format!("read {}", map.display()))?;
    let Some((new_size, records)) = x2n_records(&map) else { bail!("map of {} bytes: not whole records", map.len()) };
    let need: u64 = records.iter().map(|r| r.1 as u64).sum();
    let have = std::fs::metadata(data)?.len();
    // (The original writes stale buffer bytes after a short read; that case stays the original's.)
    ensure!(need <= have, "data has {have} bytes, the map takes {need}");
    Ok(X2nPlan { new_size, records })
}

/// Applies an x2n map to `target` (see above).
pub fn x2n_apply(target: &Path, data: &Path, map: &Path) -> Result<()> {
    let plan = x2n_plan(data, map)?;
    x2n_apply_plan(target, data, plan)
}

/// Applies a checked map. An error here may leave the target half patched.
pub fn x2n_apply_plan(target: &Path, data: &Path, plan: X2nPlan) -> Result<()> {
    let X2nPlan { new_size, records } = plan;
    let file = std::fs::OpenOptions::new().read(true).write(true).open(target).with_context(|| format!("open {}", target.display()))?;
    let end = records.iter().map(|&(o, n)| o + n as u64).max().unwrap_or(0);
    let len = file.metadata()?.len().max(end);
    if len > 0 && !records.is_empty() {
        file.set_len(len)?; // writes past the end extend the file, the gap zero-filled, as WriteFile does
        let mut map = Mapped::new(&file, len)?;
        let view = map.bytes();
        let mut data = std::io::BufReader::with_capacity(4 << 20, std::fs::File::open(data)?);
        for (off, n) in records {
            let off = off as usize;
            data.read_exact(&mut view[off..off + n])?;
        }
        map.flush()?;
    }
    file.set_len(new_size)?;
    Ok(())
}

/// A file mapped into memory, read/write.
struct Mapped {
    ptr: *mut u8,
    len: usize,
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
}

impl Mapped {
    #[cfg(windows)]
    fn new(file: &std::fs::File, len: u64) -> Result<Mapped> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, PAGE_READWRITE};
        let len_usize = usize::try_from(len)?;
        unsafe {
            let handle = CreateFileMappingW(file.as_raw_handle() as _, std::ptr::null(), PAGE_READWRITE, (len >> 32) as u32, len as u32, std::ptr::null());
            ensure!(!handle.is_null(), "CreateFileMapping: {}", std::io::Error::last_os_error());
            let view = MapViewOfFile(handle, FILE_MAP_WRITE, 0, 0, len_usize);
            if view.Value.is_null() {
                let e = std::io::Error::last_os_error();
                windows_sys::Win32::Foundation::CloseHandle(handle);
                bail!("MapViewOfFile: {e}");
            }
            Ok(Mapped { ptr: view.Value as *mut u8, len: len_usize, handle })
        }
    }

    #[cfg(not(windows))]
    fn new(_: &std::fs::File, _: u64) -> Result<Mapped> {
        bail!("x2n: Windows only")
    }

    fn bytes(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    fn flush(&self) -> Result<()> {
        #[cfg(windows)]
        unsafe {
            ensure!(windows_sys::Win32::System::Memory::FlushViewOfFile(self.ptr as _, 0) != 0, "FlushViewOfFile: {}", std::io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.ptr as _ });
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The original's loop, record by record (FUN_00401240), as the reference.
    fn x2n_reference(target: &mut Vec<u8>, data: &[u8], map: &[u8]) {
        let new_size = u64::from_le_bytes(map[..8].try_into().unwrap());
        let mut pos = 0usize;
        for r in map[8..].chunks_exact(8) {
            let mut off = [0u8; 8];
            off[..7].copy_from_slice(&r[..7]);
            let off = u64::from_le_bytes(off).min(new_size) as usize;
            let n = if r[7] == 0 { 256 } else { r[7] as usize };
            if target.len() < off + n {
                target.resize(off + n, 0);
            }
            target[off..off + n].copy_from_slice(&data[pos..pos + n]);
            pos += n;
        }
        target.resize(new_size as usize, 0);
    }

    #[cfg(windows)]
    #[test]
    fn x2n_matches_the_tools_own_loop() {
        let dir = tempfile::tempdir().unwrap();
        let old = sample(300_000, 5);
        let data = sample(200_000, 6);
        // Records: in order, overlapping, past the end, clamped, length 0 (= 256), the file
        // growing and shrinking.
        for (case, new_size) in [(0, 280_000u64), (1, 350_000), (2, 100)] {
            let mut map = new_size.to_le_bytes().to_vec();
            let mut used = 0usize;
            let mut x = 99u64 + case;
            while used + 256 <= data.len() {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let off = match x % 4 {
                    0 => (x >> 16) % 400_000, // may be past the new size: clamped
                    1 => used as u64,         // in order
                    _ => (x >> 20) % 300_000,
                };
                let len = (x >> 40) as u8; // 0 = 256
                map.extend_from_slice(&off.to_le_bytes()[..7]);
                map.push(len);
                used += if len == 0 { 256 } else { len as usize };
            }
            let (t, d, m) = (dir.path().join("t.bin"), dir.path().join("d.bin"), dir.path().join("m.bin"));
            std::fs::write(&t, &old).unwrap();
            std::fs::write(&d, &data).unwrap();
            std::fs::write(&m, &map).unwrap();
            x2n_apply(&t, &d, &m).unwrap();
            let mut want = old.clone();
            x2n_reference(&mut want, &data, &map);
            assert_eq!(std::fs::read(&t).unwrap(), want, "case {case}");
        }
        // A map that takes more data than there is, or ends in half a record: left to the
        // original, and the target untouched.
        let (t, d, m) = (dir.path().join("t2.bin"), dir.path().join("d2.bin"), dir.path().join("m2.bin"));
        std::fs::write(&t, b"untouched").unwrap();
        std::fs::write(&d, [1u8; 10]).unwrap();
        let mut map = 9u64.to_le_bytes().to_vec();
        map.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 20]);
        std::fs::write(&m, &map).unwrap();
        assert!(x2n_apply(&t, &d, &m).is_err());
        map.extend_from_slice(&[1, 2, 3]);
        std::fs::write(&m, &map).unwrap();
        assert!(x2n_apply(&t, &d, &m).is_err());
        assert_eq!(std::fs::read(&t).unwrap(), b"untouched");
    }

    fn sample(n: usize, seed: u64) -> Vec<u8> {
        // Compressible but varied: runs, text and pseudo-random stretches.
        let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let len = (x >> 40) as usize % 50_000 + 1;
            match (x >> 20) % 3 {
                0 => out.extend(std::iter::repeat_n((x >> 8) as u8, len)),
                1 => out.extend(b"the quick brown fox jumps over the lazy dog ".iter().cycle().take(len)),
                _ => {
                    for _ in 0..len {
                        x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                        out.push((x >> 33) as u8);
                    }
                }
            }
        }
        out.truncate(n);
        out
    }

    #[test]
    fn parallel_lz4_matches_the_tools_own_loop() {
        let dir = std::env::temp_dir().join(format!("arcunp-lz4-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (i, n) in [0usize, 1, 1000, 65536, 65537, LZ4_BLOCK - 1, LZ4_BLOCK, LZ4_BLOCK + 1, 3 * LZ4_BLOCK + 12345].into_iter().enumerate() {
            let data = sample(n, i as u64);
            let input = dir.join(format!("in{i}"));
            let output = dir.join(format!("out{i}"));
            std::fs::write(&input, &data).unwrap();
            lz4_compress_file(&input, &output, 12, 4).unwrap();
            assert_eq!(std::fs::read(&output).unwrap(), lz4_sequential(&data, 12).unwrap(), "size {n}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lz4_output_decompresses() {
        let data = sample(2 * LZ4_BLOCK + 99, 7);
        let dir = std::env::temp_dir().join(format!("arcunp-lz4d-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("in"), &data).unwrap();
        lz4_compress_file(&dir.join("in"), &dir.join("out"), 12, 3).unwrap();
        let packed = std::fs::read(dir.join("out")).unwrap();
        let mut ctx = lz4_sys::LZ4FDecompressionContext(std::ptr::null_mut());
        check(unsafe { lz4_sys::LZ4F_createDecompressionContext(&mut ctx, lz4_sys::LZ4F_VERSION) }).unwrap();
        let mut back = vec![0u8; data.len() + 1];
        let (mut src_pos, mut dst_pos) = (0usize, 0usize);
        loop {
            let mut src_len = packed.len() - src_pos;
            let mut dst_len = back.len() - dst_pos;
            let hint = check(unsafe {
                lz4_sys::LZ4F_decompress(ctx, back[dst_pos..].as_mut_ptr(), &mut dst_len, packed[src_pos..].as_ptr(), &mut src_len, std::ptr::null())
            })
            .unwrap();
            src_pos += src_len;
            dst_pos += dst_len;
            if hint == 0 || src_len == 0 && dst_len == 0 {
                break;
            }
        }
        unsafe { lz4_sys::LZ4F_freeDecompressionContext(ctx) };
        assert_eq!(src_pos, packed.len());
        assert_eq!(&back[..dst_pos], &data[..]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn xor_matches_the_definition() {
        let key = xor_key("0xDC45A69CD3724CAB").unwrap();
        assert_eq!(key, [0xDC, 0x45, 0xA6, 0x9C, 0xD3, 0x72, 0x4C, 0xAB]);
        let data = sample((8 << 20) + 13, 3);
        let dir = std::env::temp_dir().join(format!("arcunp-xor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("in"), &data).unwrap();
        xor_file(&dir.join("in"), &dir.join("out"), &key).unwrap();
        let out = std::fs::read(dir.join("out")).unwrap();
        assert!(out.iter().zip(&data).enumerate().all(|(i, (o, d))| *o == d ^ key[i % 8]));
        assert_eq!(out.len(), data.len());
        for bad in ["DC45", "0x", "0xABC", "0xZZ"] {
            assert!(xor_key(bad).is_none(), "{bad}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
