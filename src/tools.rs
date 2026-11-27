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
//! - Cricket 19's fgpack.exe: `fgpack e <in> <out>` LZMA-encodes independent segments one after
//!   the other on one core. Here: the input is cut at segment boundaries and the pieces are
//!   encoded by copies of the original tool at once, their outputs joined in order (see
//!   `fgpack_parallel`).
//! - FreeArc 0.67's console unarc (Prototype's fgpack.exe, Max Payne 3's and Cricket's
//!   fgpack2.exe): `x -o+ <archive>` extracts into the current folder, one block after the other.
//!   Here: arc-unpack's own engine extracts it (all cores, every file CRC-checked), with the
//!   codecs next to the tool; anything it can't do falls back to the original, which rewrites
//!   every file anyway (-o+).

use anyhow::{Context, Result, bail, ensure};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// What a known tool is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Xor,
    Lz4Cli194,
    X2n,
    FgpackSegLzma,
    FreeArcUnarc,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Role::Xor => "xor",
            Role::Lz4Cli194 => "lz4-1.9.4",
            Role::X2n => "x2n",
            Role::FgpackSegLzma => "fgpack-seg-lzma",
            Role::FreeArcUnarc => "freearc-unarc-0.67",
        }
    }

    /// The original must stay in its own folder (renamed): FreeArc's console unarc finds arc.ini
    /// and its cls-*.dll codecs next to itself.
    pub fn keeps_its_folder(self) -> bool {
        matches!(self, Role::FreeArcUnarc)
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
    (205312, "f6594b8f7773a9223bc7f1c5c3ecf9971c4988ee34db6321a13673511cdde665", Role::FgpackSegLzma),
    (333312, "09ea4478b379a1839e21896a8b3fd82b4adf81c1b2b387795abefe5df3e57549", Role::FreeArcUnarc),
];

/// Repack tools that touch only the files named on their command line: no fixed-name temp
/// files, no other files read or written. Steps that run them can run beside each other when
/// they share no file (see install.rs, `overlap_paths`). The stand-ins above qualify too.
/// - raw2hif (FitGirl's reflate tool, also shipped as nz.exe): `raw2hif d9 X.unp X.hif9 X.raw`;
///   no temp-file or GetTempPath imports.
/// - hpatchz 2.5.3 (x5.exe; HDiffPatch, open source): `x5 old diff new`.
/// - xdelta3 3.0.8 / 3.0.11 (x.exe; open source): `x -f -d -s src delta out`.
/// - oggvorbis2fsb5 (fsb.exe; open source): `fsb in out`.
const OVERLAP_SAFE: &[(u64, &str)] = &[
    (74240, "c6ae9b328578ee6b43021b6ec90fbaf152c7cdba0085fda72ab51b9259a5b361"),
    (159232, "342b579afeff5651e9c0c57a6d2e785f119655f170d106dd01dfb97162efe385"),
    (145408, "0eb2633f7414b79a39cc93bd9f36fa286a17905492235fe72a882d916b3fa30d"),
    (317952, "09763ca90c09a5f815a94527399b1f2a88685e9de462e2ff1bc5648d320e707a"),
    (548804, "9bf8d067de9448e521afe1f8108caa0f85b4b7c7933641efd44bc43533920565"),
    (331264, "d1f00ff506d77a3f219c497e444306f7949c0a9b9e029f15bb862aa521fd26f8"),
    (1153277, "98d7c11a27dc7b0da976d5b01374e0359d92b2d3b5439c5d26bad70ec618d811"),
];

/// If `path` is a tool in `OVERLAP_SAFE` or a stand-in listed in `map`: how many leading
/// arguments are not file names (raw2hif's mode word, `d9`). None for any other program.
pub fn named_files_tool(path: &Path, map: Option<&Path>) -> Option<usize> {
    if map.is_some_and(|m| lookup(m, path).is_some()) {
        return Some(0);
    }
    let meta = std::fs::metadata(path).ok()?;
    if !OVERLAP_SAFE.iter().any(|k| k.0 == meta.len()) {
        return None;
    }
    let d = crate::extract::hex(&sha256_file(path).ok()?);
    let i = OVERLAP_SAFE.iter().position(|k| k.0 == meta.len() && k.1 == d)?;
    Some(if i == 0 { 1 } else { 0 })
}

/// The name ending of an original kept in its own folder (see `Role::keeps_its_folder`).
const ORIGINAL_SUFFIX: &str = ".arc-unpack-original.exe";

/// The environment variable that tells a stand-in which arc-unpack started it (the FreeArc
/// stand-in extracts with it).
pub const SELF_ENV: &str = "ARC_UNPACK_SELF";

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
            let name = p.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
            if !ft.is_file() || !name.ends_with(".exe") || name.ends_with(ORIGINAL_SUFFIX) || known.contains(&p) {
                continue;
            }
            let Some(role) = identify(&p) else { continue };
            let original = if role.keeps_its_folder() {
                p.with_file_name(format!("{}{ORIGINAL_SUFFIX}", p.file_stem().unwrap_or_default().to_string_lossy()))
            } else {
                std::fs::create_dir_all(originals)?;
                originals.join(format!("{}-{}", done.len() + known.len(), p.file_name().unwrap_or_default().to_string_lossy()))
            };
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

// ---------------------------------------------------------------------------------------------
// Cricket 19's fgpack.exe (205,312 B): `fgpack e <in> <out>`. Reverse-engineered (main
// FUN_00401000): the input is a sequence of segments [u32 LE n][n bytes], read until the 4-byte
// header can't be read; each segment is encoded by a NEW LZMA SDK 4.x encoder (d 8 KB, lc3 lp0
// pb2, a2, fb128, bt4, no end marker) and written as [u32 csize][13-byte .lzma header + data].
// Nothing carries from one segment to the next, so the output is the concatenation of the
// segments' outputs, and the file can be cut at segment boundaries, the pieces encoded by copies
// of the original tool at once, and their outputs joined in order. It runs on one core and
// Cricket's lists give it GB-sized files, 8-10 at a time on a 32-thread PC.

/// Where the segments of an `fgpack e` input end, and the file length. A header whose length
/// runs past the end, and a tail too short for a header, are left to the last piece (the tool
/// handles them the same way whether it sees the whole file or that piece).
pub fn fgpack_segment_ends(path: &Path) -> Result<(Vec<u64>, u64)> {
    use std::io::{Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let mut ends = Vec::new();
    let mut pos = 0u64;
    let mut head = [0u8; 4];
    while pos + 4 <= len {
        f.seek(SeekFrom::Start(pos))?;
        f.read_exact(&mut head)?;
        let end = pos + 4 + u32::from_le_bytes(head) as u64;
        if end > len {
            break;
        }
        ends.push(end);
        pos = end;
    }
    Ok((ends, len))
}

/// Up to `k` contiguous pieces of about equal size, cut at segment ends: the end offset of each
/// piece (the last is `len`).
pub fn balanced_cuts(ends: &[u64], len: u64, k: usize) -> Vec<u64> {
    let mut cuts = Vec::new();
    let mut last = 0u64;
    for i in 1..k.max(1) as u64 {
        let target = len * i / k as u64;
        // The first segment end at or after the target, if it leaves room for later pieces.
        if let Some(&c) = ends.iter().find(|&&e| e >= target && e > last && e < len) {
            if cuts.last() != Some(&c) {
                cuts.push(c);
                last = c;
            }
        }
    }
    cuts.push(len);
    cuts
}

/// `fgpack e <input> <output>` with the original tool `original`, piece by piece in parallel (at
/// most `cpus` copies at once machine-wide, through a semaphore shared with the other stand-ins
/// Cricket's lists start at the same time). An error means nothing usable was written: the caller
/// runs the original on the whole file. Returns a summary for the log.
pub fn fgpack_parallel(input: &Path, output: &Path, original: &Path, cpus: usize) -> Result<String> {
    let (ends, len) = fgpack_segment_ends(input)?;
    // The tool keeps 32-bit file offsets: past 2 GB it is the original's business.
    ensure!(len < (2u64 << 30) - (64 << 20), "input of {len} bytes: near the tool's 2 GB limit");
    ensure!(ends.len() >= 2, "{} segment(s): nothing to split", ends.len());
    let cuts = balanced_cuts(&ends, len, ends.len().min(cpus.max(1)));
    ensure!(cuts.len() >= 2, "one piece only");
    if let Some(free) = crate::guard::free_space(input.parent().unwrap_or(Path::new("."))) {
        ensure!(free > 2 * len + (1 << 30), "not enough free space for the pieces");
    }
    let piece = |p: &Path, i: usize| p.with_file_name(format!("{}.arc-unpack-piece{i}", p.file_name().unwrap_or_default().to_string_lossy()));
    let pieces: Vec<(PathBuf, PathBuf)> = (0..cuts.len()).map(|i| (piece(input, i), piece(output, i))).collect();
    let cleanup = || {
        for (a, b) in &pieces {
            let _ = std::fs::remove_file(a);
            let _ = std::fs::remove_file(b);
        }
    };
    let result = (|| -> Result<()> {
        let mut src = std::fs::File::open(input)?;
        let mut start = 0u64;
        for (&end, (pin, _)) in cuts.iter().zip(&pieces) {
            let mut dst = std::io::BufWriter::with_capacity(4 << 20, std::fs::File::create(pin)?);
            let n = std::io::copy(&mut (&mut src).take(end - start), &mut dst)?;
            ensure!(n == end - start, "short read of {}", input.display());
            dst.flush()?;
            start = end;
        }
        let slots = CpuSlots::open(cpus);
        let failed: Vec<String> = std::thread::scope(|s| {
            let jobs: Vec<_> = pieces
                .iter()
                .map(|(pin, pout)| {
                    let slots = &slots;
                    s.spawn(move || -> std::result::Result<(), String> {
                        let _slot = slots.take();
                        let st = std::process::Command::new(original)
                            .arg("e")
                            .arg(pin)
                            .arg(pout)
                            .stdin(std::process::Stdio::null())
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .status()
                            .map_err(|e| e.to_string())?;
                        if st.success() && pout.is_file() { Ok(()) } else { Err(format!("exit {:?}", st.code())) }
                    })
                })
                .collect();
            jobs.into_iter().filter_map(|j| j.join().unwrap_or_else(|_| Err("panicked".into())).err()).collect()
        });
        ensure!(failed.is_empty(), "a piece failed: {}", failed.join(", "));
        let mut out = std::io::BufWriter::with_capacity(4 << 20, std::fs::File::create(output)?);
        for (_, pout) in &pieces {
            std::io::copy(&mut std::fs::File::open(pout)?, &mut out)?;
        }
        out.flush()?;
        Ok(())
    })();
    cleanup();
    if let Err(e) = result {
        let _ = std::fs::remove_file(output);
        return Err(e);
    }
    let mut sizes: Vec<u64> = ends.iter().scan(0u64, |p, &e| { let s = e - *p; *p = e; Some(s) }).collect();
    sizes.sort_unstable();
    Ok(format!(
        "{} segments (min {} / median {} / max {} bytes) in {} pieces",
        ends.len(),
        sizes[0],
        sizes[sizes.len() / 2],
        sizes[sizes.len() - 1],
        cuts.len()
    ))
}

/// A machine-wide count of CPUs for the stand-ins' own child processes (a named semaphore), so
/// that ten stand-ins started at once don't start ten times as many tools as there are cores.
struct CpuSlots(#[cfg(windows)] windows_sys::Win32::Foundation::HANDLE);

// SAFETY: a semaphore handle can be used from any thread.
unsafe impl Sync for CpuSlots {}

struct CpuSlot<'a>(&'a CpuSlots);

impl CpuSlots {
    fn open(cpus: usize) -> CpuSlots {
        #[cfg(windows)]
        {
            let name: Vec<u16> = "Local\\arc-unpack-tool-cpu".encode_utf16().chain([0]).collect();
            let n = cpus.clamp(1, 1024) as i32;
            CpuSlots(unsafe { windows_sys::Win32::System::Threading::CreateSemaphoreW(std::ptr::null(), n, n, name.as_ptr()) })
        }
        #[cfg(not(windows))]
        {
            let _ = cpus;
            CpuSlots()
        }
    }

    fn take(&self) -> CpuSlot<'_> {
        #[cfg(windows)]
        if !self.0.is_null() {
            unsafe { windows_sys::Win32::System::Threading::WaitForSingleObject(self.0, u32::MAX) };
        }
        CpuSlot(self)
    }
}

impl Drop for CpuSlot<'_> {
    fn drop(&mut self) {
        #[cfg(windows)]
        if !(self.0).0.is_null() {
            unsafe { windows_sys::Win32::System::Threading::ReleaseSemaphore((self.0).0, 1, std::ptr::null_mut()) };
        }
    }
}

impl Drop for CpuSlots {
    fn drop(&mut self) {
        #[cfg(windows)]
        if !self.0.is_null() {
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
        }
    }
}

// ---------------------------------------------------------------------------------------------
// FreeArc 0.67's console unarc (see the module doc).

/// `x -o+ <archive>` into `out` with arc-unpack's engine: the arc-unpack program in SELF_ENV,
/// with the codecs (and arc.ini, if any) next to the `original` tool. An error means: run the
/// original (which rewrites every file anyway). Returns a note for the log.
pub fn freearc_extract(archive: &Path, out: &Path, original: &Path) -> Result<String> {
    let cli = std::env::var_os(SELF_ENV).map(PathBuf::from).filter(|p| p.is_file()).context("arc-unpack's program not known")?;
    let dir = original.parent().context("the tool's folder")?;
    let ini = dir.join("arc.ini");
    let toml = if ini.is_file() {
        crate::arcini::to_toml(&ini)?
    } else {
        // Without arc.ini FreeArc finds cls-<method>.dll next to itself; so does codec_dir.
        format!("codec_dir = {}\n", toml::Value::String(dir.to_string_lossy().into_owned()))
    };
    let cfg = std::env::temp_dir().join(format!("arc-unpack-freearc-{}.toml", std::process::id()));
    std::fs::write(&cfg, toml)?;
    let run = std::process::Command::new(&cli)
        .arg("extract")
        .arg("-c")
        .arg(&cfg)
        .arg("-o")
        .arg(out)
        .arg("--overwrite")
        .arg("-j")
        .arg("4")
        .arg(archive)
        .stdin(std::process::Stdio::null())
        .output();
    let _ = std::fs::remove_file(&cfg);
    let o = run?;
    let text = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    let last = text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
    ensure!(o.status.success(), "arc-unpack extract failed: {last}");
    Ok(format!("arc-unpack's engine: {last}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fgpack_inputs_are_cut_at_segment_ends() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("22.fgpack");
        // Segments of 10, 0, 5 and 20 bytes, then a 2-byte tail the tool ignores.
        let mut data = Vec::new();
        for n in [10u32, 0, 5, 20] {
            data.extend(n.to_le_bytes());
            data.extend(std::iter::repeat_n(n as u8, n as usize));
        }
        data.extend([1, 2]);
        std::fs::write(&f, &data).unwrap();
        let (ends, len) = fgpack_segment_ends(&f).unwrap();
        assert_eq!((ends.clone(), len), (vec![14, 18, 27, 51], 53));
        assert_eq!(balanced_cuts(&ends, len, 2), vec![27, 53]);
        assert_eq!(balanced_cuts(&ends, len, 4), vec![14, 27, 51, 53]);
        assert_eq!(balanced_cuts(&ends, len, 1), vec![53]);
        // A header longer than what is left: that segment and the rest stay in the last piece.
        let mut data = 10u32.to_le_bytes().to_vec();
        data.extend([0u8; 10]);
        data.extend(1000u32.to_le_bytes());
        data.extend([9, 9, 9]);
        std::fs::write(&f, &data).unwrap();
        let (ends, len) = fgpack_segment_ends(&f).unwrap();
        assert_eq!((ends.clone(), len), (vec![14], 21));
        assert_eq!(balanced_cuts(&ends, len, 2), vec![14, 21]);
    }

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
