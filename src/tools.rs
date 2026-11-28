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
//! - FitGirl's run.exe: `run " " <url> <list> <N>` runs the list's command lines N at a time
//!   (0 = one per CPU). Farming Simulator 22 passes N=4 for 2,155 independent lines. When N is
//!   below the CPU count and no two lines name the same file, the original runs with N=0.
//! - HDiffPatch's hpatchz 4.8.0 (x5n.exe), patching a folder in place (`x5n -s-2g -f . d .`): by
//!   default it reads every unchanged file in full only to checksum it before keeping it. The
//!   original runs with `-C-new` added, which skips that read; the patched files are the same
//!   (the bytes come from the diff). Source: HDiffPatch, dir_patch / new_dir_output.c.
//! - delzo (Far Cry 3's Dunia recompressor): `delzo r -t16 X.fgu X.dat X.fat` recompresses
//!   independent chunks in batches with a full wait after each. Here: the .fgu is cut into chunk
//!   ranges, each restored by a copy of the original at once, the .dat pieces joined in order (see
//!   `delzo_parallel`).

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
    RunExe,
    X5nDirPatch,
    DelzoRestore,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Role::Xor => "xor",
            Role::Lz4Cli194 => "lz4-1.9.4",
            Role::X2n => "x2n",
            Role::FgpackSegLzma => "fgpack-seg-lzma",
            Role::FreeArcUnarc => "freearc-unarc-0.67",
            Role::RunExe => "fitgirl-run",
            Role::X5nDirPatch => "hpatchz-4.8.0",
            Role::DelzoRestore => "delzo-0.0.4",
        }
    }

    /// Whether its job touches only the files its command line names, so that steps running it
    /// may run beside each other (install.rs `overlap_paths`). Not FreeArc's unarc (it writes an
    /// archive's files), run.exe (it runs a whole list) or x5n (it patches a folder).
    pub fn touches_only_named_files(self) -> bool {
        matches!(self, Role::Xor | Role::Lz4Cli194 | Role::X2n | Role::FgpackSegLzma | Role::DelzoRestore)
    }

    /// Leading command-line words that are a mode, not a file (delzo's `r`, Cricket fgpack's `e`).
    pub fn mode_words(self) -> usize {
        matches!(self, Role::DelzoRestore | Role::FgpackSegLzma) as usize
    }

    /// The original must stay in its own folder (renamed): FreeArc's console unarc finds arc.ini
    /// and its cls-*.dll codecs next to itself.
    pub fn keeps_its_folder(self) -> bool {
        // (FitGirl's run.exe checks "Made by FitGirl.txt" next to itself and runs its lines there.)
        matches!(self, Role::FreeArcUnarc | Role::RunExe)
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
    (81920, "3243562d5d0685b57bc237b24f254d1d18d6abcb1f8ddac2698c2d9208c76247", Role::RunExe),
    (436736, "234168f765294a5db867163e639c9288f04f976976bdf80da29fbba9eae39917", Role::X5nDirPatch),
    (98816, "aa511243239695e06dc959a6f8c5fe002d1101400b751125b98ba120c560f765", Role::DelzoRestore),
];

/// Repack tools that touch only the files named on their command line: no fixed-name temp
/// files, no other files read or written. Steps that run them can run beside each other when
/// they share no file (see install.rs, `overlap_paths`). The stand-ins above qualify too.
/// - raw2hif (FitGirl's reflate tool, also shipped as nz.exe): `raw2hif d9 X.unp X.hif9 X.raw`;
///   no temp-file or GetTempPath imports.
/// - hpatchz 2.5.3 (x5.exe; HDiffPatch, open source): `x5 old diff new`.
/// - xdelta3 3.0.8 / 3.0.11 (x.exe; open source): `x -f -d -s src delta out`.
/// - oggvorbis2fsb5 (fsb.exe; open source): `fsb in out`.
/// - Not RTPatch's x3.exe (Unravel Two): the files it patches are named only inside its patch
///   file, and a target another step holds open becomes a reboot-pending rename (MoveFileEx
///   DELAY_UNTIL_REBOOT) with the new file missing. (Its CPU pinning is only the MSVC CRT's FDIV
///   test, which restores the mask, and its temp names carry the PID.)
/// - Farming Simulator 22's .NET shapes tool (fgpack.exe, 24,576 B; reverse-engineered:
///   File.Open(in), File.Create(out), a keystream XOR): `fgpack in out`.
/// - LZMA SDK 15.12 and 19.00 lzma.exe (LzmaAlone, open source; Kerbal Space Program's and
///   RimWorld's fgpack.exe / fgpack2.exe; CreateFileW is their only file import):
///   `lzma e -d19 ... in out`.
/// (size, SHA-256, how many leading arguments are mode words, not files)
const OVERLAP_SAFE: &[(u64, &str, usize)] = &[
    (74240, "c6ae9b328578ee6b43021b6ec90fbaf152c7cdba0085fda72ab51b9259a5b361", 1),
    (159232, "342b579afeff5651e9c0c57a6d2e785f119655f170d106dd01dfb97162efe385", 0),
    (145408, "0eb2633f7414b79a39cc93bd9f36fa286a17905492235fe72a882d916b3fa30d", 0),
    (317952, "09763ca90c09a5f815a94527399b1f2a88685e9de462e2ff1bc5648d320e707a", 0),
    (548804, "9bf8d067de9448e521afe1f8108caa0f85b4b7c7933641efd44bc43533920565", 0),
    (331264, "d1f00ff506d77a3f219c497e444306f7949c0a9b9e029f15bb862aa521fd26f8", 0),
    (1153277, "98d7c11a27dc7b0da976d5b01374e0359d92b2d3b5439c5d26bad70ec618d811", 0),
    (24576, "eb9a914ff781bc2f088e70d1b99faca73d98a96325a4a154cf52b3b52a4f860a", 0),
    (97280, "5c7ecad4da5b60aca44594b29e8b6529dec2489d5e05e1a5f59af345b9b43045", 1),
    (108544, "a95222984f60e3f5bea4099cab85868946523391b0e50d7f6dcc5e866d0b0dbd", 1),
];

/// If `path` is a tool in `OVERLAP_SAFE` or a stand-in listed in `map`: how many leading
/// arguments are not file names (raw2hif's `d9`, lzma's `e`). None for any other program.
pub fn named_files_tool(path: &Path, map: Option<&Path>) -> Option<usize> {
    if let Some(e) = map.and_then(|m| lookup(m, path)) {
        return e.role.touches_only_named_files().then_some(e.role.mode_words());
    }
    let meta = std::fs::metadata(path).ok()?;
    if !OVERLAP_SAFE.iter().any(|k| k.0 == meta.len()) {
        return None;
    }
    let d = crate::extract::hex(&sha256_file(path).ok()?);
    OVERLAP_SAFE.iter().find(|k| k.0 == meta.len() && k.1 == d).map(|k| k.2)
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

pub(crate) fn sha256_file(path: &Path) -> std::io::Result<[u8; 32]> {
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
    if len >= (2u64 << 30) - (64 << 20) {
        return Err(LeftToOriginal(format!("input of {len} bytes: near the tool's 2 GB limit")).into());
    }
    if ends.len() < 2 {
        return Err(LeftToOriginal(format!("{} segment(s): nothing to split", ends.len())).into());
    }
    let cuts = balanced_cuts(&ends, len, ends.len().min(cpus.max(1)));
    if cuts.len() < 2 {
        return Err(LeftToOriginal("one piece only".into()).into());
    }
    if let Some(free) = crate::guard::free_space(input.parent().unwrap_or(Path::new(".")))
        && free <= 2 * len + (1 << 30)
    {
        return Err(LeftToOriginal("not enough free space for the pieces".into()).into());
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

// ---------------------------------------------------------------------------------------------
// FitGirl's run.exe (see the module doc).

/// Whether `diff` is an HDiffPatch directory diff ("HDIFF19&..." and a NUL byte, then the varints
/// oldPathIsDir and newPathIsDir, both 1).
pub fn is_dir_diff(diff: &Path) -> bool {
    let mut head = [0u8; 64];
    let n = std::fs::File::open(diff).and_then(|mut f| f.read(&mut head)).unwrap_or(0);
    let head = &head[..n];
    if !head.starts_with(b"HDIFF19&") {
        return false;
    }
    let Some(nul) = head.iter().position(|&b| b == 0) else { return false };
    head.get(nul + 1..nul + 3) == Some(&[1, 1][..])
}

/// A piece of a run.exe list line as cmd sees it.
#[derive(Debug, PartialEq)]
enum Tok {
    Word(String),
    /// `&&` between two commands.
    And,
}

/// One list line split the way cmd splits it, or None for what this doesn't follow (a single
/// `&`, `|`, `^`, brackets). run.exe's `@` prefix goes; in `cmd /c "..."` cmd drops the first
/// quote and the last one; then words end at blanks (and at `+` when `plus`: `copy /b a+b c`)
/// outside quotes, with the quotes removed, and `&&` separates commands. `2>&1` stays a word.
fn list_tokens(line: &str, plus: bool) -> Option<Vec<Tok>> {
    let line = line.trim().trim_start_matches('@').trim_start();
    let mut toks = Vec::new();
    let mut rest = line.to_string();
    let lower = line.to_ascii_lowercase();
    for prefix in ["cmd /c ", "cmd.exe /c "] {
        if lower.starts_with(prefix) {
            toks.push(Tok::Word(line[..prefix.len() - 4].trim().to_string()));
            toks.push(Tok::Word("/c".into()));
            let mut r = line[prefix.len()..].trim_start().to_string();
            if r.starts_with('"') {
                r.remove(0);
                if let Some(i) = r.rfind('"') {
                    r.remove(i);
                }
            }
            rest = r;
            break;
        }
    }
    let (mut word, mut quoted, mut any) = (String::new(), false, false);
    let mut chars = rest.chars().peekable();
    let end = |word: &mut String, any: &mut bool, toks: &mut Vec<Tok>| {
        if *any {
            toks.push(Tok::Word(std::mem::take(word)));
            *any = false;
        }
    };
    while let Some(c) = chars.next() {
        if c == '"' {
            quoted = !quoted;
            any = true;
            continue;
        }
        if quoted {
            word.push(c);
            continue;
        }
        match c {
            c if c.is_whitespace() => end(&mut word, &mut any, &mut toks),
            '+' if plus => end(&mut word, &mut any, &mut toks),
            '&' if chars.peek() == Some(&'&') => {
                chars.next();
                end(&mut word, &mut any, &mut toks);
                toks.push(Tok::And);
            }
            // `2>&1`, `1>&2`: a handle, part of the redirection.
            '&' if word.ends_with('>') => word.push(c),
            '&' | '|' | '^' | '(' | ')' => return None,
            _ => {
                word.push(c);
                any = true;
            }
        }
    }
    end(&mut word, &mut any, &mut toks);
    Some(toks)
}

/// The programs (`*.exe` words) the command lines of a run.exe list run.
pub fn run_list_programs(list: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in run_list_lines(list) {
        for t in list_tokens(line, false).unwrap_or_default() {
            if let Tok::Word(w) = t
                && w.to_ascii_lowercase().ends_with(".exe")
                && !w.eq_ignore_ascii_case("cmd.exe")
                && !out.iter().any(|o| o.eq_ignore_ascii_case(&w))
            {
                out.push(w);
            }
        }
    }
    out
}

/// The lines run.exe runs (fgets in file order; empty, '#' and ';' lines skipped).
fn run_list_lines(list: &str) -> impl Iterator<Item = &str> {
    list.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('#') && !l.starts_with(';'))
}

/// How many words after a program (as a run.exe list names it) are mode words, not files
/// (lzma's `e`; see `named_files_tool`).
pub type ModeWords<'a> = &'a dyn Fn(&str) -> usize;

/// The files one command line of a run.exe list names, or None if it can't be told (then the
/// list is left as it is). Each command (the line's first, and each after `&&`) must be a
/// program (`*.exe`, which `run_list_programs` vets) or one of cmd's del/copy/move/ren/echo;
/// anything else (md, rd, cd, call, for, a .bat) makes it None, as do variables (run.exe
/// expands them) and wildcards. Every other word is a file, except a program's mode words and
/// switches; a redirection names the files on both sides (`base.dat>nul`, `echo 1>a.log`), but
/// not nul or a handle (`2>&1`).
fn run_line_files(line: &str, modes: ModeWords) -> Option<Vec<String>> {
    const BUILTINS: [&str; 7] = ["del", "erase", "move", "copy", "ren", "rename", "echo"];
    if line.contains(['%', '*', '?']) {
        return None;
    }
    let (mut out, mut mode_words, mut at_command, mut echo, mut target) = (Vec::new(), 0usize, true, false, false);
    for t in list_tokens(line, true)? {
        let w = match t {
            Tok::And => {
                (at_command, mode_words, echo, target) = (true, 0, false, false);
                continue;
            }
            Tok::Word(w) => w,
        };
        // The word after an operator standing alone (`> log.txt`, `2> err.txt`) is its file.
        if target && !at_command {
            target = false;
            if !w.contains(['>', '<']) {
                if !w.eq_ignore_ascii_case("nul") {
                    out.push(w);
                }
                continue;
            }
        }
        let lw = w.to_ascii_lowercase();
        if at_command {
            // (`cmd /c`: the command comes after it.)
            if lw == "cmd" || lw == "cmd.exe" || lw == "/c" {
                continue;
            }
            at_command = false;
            if lw.ends_with(".exe") && !lw.contains(['>', '<']) {
                mode_words = modes(&w);
                continue;
            }
            if BUILTINS.contains(&lw.as_str()) {
                echo = lw == "echo";
                continue;
            }
            return None;
        }
        if lw.contains(['>', '<']) {
            target = lw.ends_with(['>', '<']);
            let parts: Vec<&str> = w.split(['>', '<']).collect();
            for part in parts {
                let part = part.trim_start_matches('&');
                let lp = part.to_ascii_lowercase();
                // A handle number (`2>`, `>&1`), nul, or echo's text before the `>`.
                if part.is_empty() || lp == "nul" || part.chars().all(|c| c.is_ascii_digit()) {
                    continue;
                }
                if [".exe", ".bat", ".cmd", ".com"].iter().any(|e| lp.ends_with(e)) {
                    return None;
                }
                out.push(part.to_string());
            }
            continue;
        }
        if echo {
            // echo's own words are text, not files.
            continue;
        }
        if mode_words > 0 && !w.starts_with('-') && !w.starts_with('/') {
            mode_words -= 1;
            continue;
        }
        if w.starts_with('-') || (w.starts_with('/') && w.len() <= 3) {
            continue;
        }
        out.push(w);
    }
    Some(out)
}

/// Whether the command lines of a run.exe list touch disjoint sets of files (`run_line_files`;
/// the programs themselves must touch only those files: see `run_list_programs`). Two lines
/// share a file when they name the same file name, or the same path, or one names a folder the
/// other names something in.
pub fn run_list_lines_independent(list: &str, cwd: &Path, modes: ModeWords) -> bool {
    use std::collections::HashSet;
    let (mut names, mut paths, mut folders) = (HashSet::new(), HashSet::new(), HashSet::new());
    // Every path as run.exe's lines see it (relative to `cwd`, `.` and `..` resolved).
    let norm = |w: &str| {
        let p = std::path::absolute(cwd.join(w.replace('/', "\\"))).unwrap_or_else(|_| cwd.join(w));
        p.to_string_lossy().to_ascii_lowercase().trim_end_matches('\\').to_string()
    };
    // The programs count as files of every line: a line that deletes or replaces one (or its
    // folder) collides with the lines running it.
    for prog in run_list_programs(list) {
        let p = norm(&prog);
        let mut up = p.as_str();
        while let Some((parent, _)) = up.rsplit_once('\\') {
            folders.insert(parent.to_string());
            up = parent;
        }
        names.insert(p.rsplit('\\').next().unwrap_or("").to_string());
        paths.insert(p);
    }
    for line in run_list_lines(list) {
        let Some(files) = run_line_files(line, modes) else { return false };
        let mine: Vec<String> = files.iter().map(|w| norm(w)).collect();
        for p in &mine {
            let name = p.rsplit('\\').next().unwrap_or("");
            let mut up = p.as_str();
            let mut inside_one = false;
            while let Some((parent, _)) = up.rsplit_once('\\') {
                inside_one |= paths.contains(parent);
                up = parent;
            }
            if names.contains(name) || paths.contains(p) || folders.contains(p) || inside_one {
                return false;
            }
        }
        for p in mine {
            let mut up = p.as_str();
            while let Some((parent, _)) = up.rsplit_once('\\') {
                folders.insert(parent.to_string());
                up = parent;
            }
            names.insert(p.rsplit('\\').next().unwrap_or("").to_string());
            paths.insert(p);
        }
    }
    true
}

/// A run.exe list (of independent lines) with the lines whose existing files (relative to
/// `cwd`, run.exe's working folder) are largest first: run.exe starts its lines in file order,
/// so a big one near the end runs alone at the end (Kerbal Space Program: 222 LZMA re-encodes,
/// the last stretch at 15% CPU). None if the order stays the same, or if a line is too long
/// for run.exe's 4,096-byte line buffer.
pub fn run_list_largest_first(list: &[u8], cwd: &Path, modes: ModeWords) -> Option<Vec<u8>> {
    let text: String = list.iter().map(|&b| b as char).collect();
    let lines: Vec<&str> = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).filter(|l| !l.is_empty()).collect();
    if lines.iter().any(|l| l.len() >= 4094) {
        return None;
    }
    let size = |l: &str| -> u64 {
        if run_list_lines(l).next().is_none() {
            return 0;
        }
        run_line_files(l, modes)
            .unwrap_or_default()
            .iter()
            .filter_map(|f| {
                let p = Path::new(f);
                std::fs::metadata(if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) }).ok().filter(|m| m.is_file())
            })
            .map(|m| m.len())
            .sum()
    };
    let mut order: Vec<(usize, u64)> = lines.iter().enumerate().map(|(i, l)| (i, size(l))).collect();
    order.sort_by_key(|&(i, s)| (std::cmp::Reverse(s), i));
    if order.iter().enumerate().all(|(k, &(i, _))| k == i) {
        return None;
    }
    let mut out = Vec::with_capacity(list.len() + 2 * lines.len());
    for (i, _) in order {
        out.extend(lines[i].chars().map(|c| c as u8));
        out.extend(b"\r\n");
    }
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// delzo.exe ("DELZORec v0.0.4.0", Far Cry 3): `delzo r [-tN] X.fgu X.dat X.fat` rebuilds a Dunia
// .dat/.fat from FitGirl's .fgu. Reverse-engineered: the .fgu is a 32-byte header ("DERE",
// max chunk size, n, n*8, u64 .fat length, u64 original .dat length), n type bytes, n x {u32
// packed, u32 unpacked}, the .fat verbatim, then the chunks' data. `r` writes the .fat as it is
// and each chunk recompressed (type 0 stored, 1 LZO1X-999 level 9 for .fat version 9, 2 LZ4-HC
// level 9), back to back. Each output chunk depends only on (type, .fat version, its bytes). It
// runs N chunks on N new threads, waits for all, writes them, and goes on: a batch lasts as long
// as its largest chunk, so Far Cry 3 got about 3 of 16 threads busy for 408 s.

/// A stand-in declining a job it could do but that isn't worth it (small, no room): the
/// original runs, and the log doesn't call it a failure.
#[derive(Debug)]
pub struct LeftToOriginal(pub String);

impl std::fmt::Display for LeftToOriginal {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LeftToOriginal {}

/// The parts of a delzo .fgu.
struct Fgu {
    max_chunk: u32,
    types: Vec<u8>,
    sizes: Vec<(u32, u32)>,
    fat: Vec<u8>,
    data_start: u64,
    dat_len: u64,
}

fn read_fgu(path: &Path) -> Result<Fgu> {
    let mut f = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut h = [0u8; 32];
    f.read_exact(&mut h)?;
    let w = |i: usize| u32::from_le_bytes(h[4 * i..4 * i + 4].try_into().unwrap());
    ensure!(w(0) == 0x4552_4544, "not a delzo .fgu");
    let n = w(2) as usize;
    ensure!(w(3) as usize == n * 8 && w(5) == 0, "unexpected .fgu header");
    let mut types = vec![0u8; n];
    f.read_exact(&mut types)?;
    let mut raw = vec![0u8; n * 8];
    f.read_exact(&mut raw)?;
    let sizes = raw.chunks_exact(8).map(|c| (u32::from_le_bytes(c[..4].try_into().unwrap()), u32::from_le_bytes(c[4..].try_into().unwrap()))).collect();
    let mut fat = vec![0u8; w(4) as usize];
    f.read_exact(&mut fat)?;
    let data_start = 32 + n as u64 * 9 + fat.len() as u64;
    Ok(Fgu { max_chunk: w(1), types, sizes, fat, data_start, dat_len: w(6) as u64 | (w(7) as u64) << 32 })
}

/// What delzo reads for chunk `i`, and what it writes for it.
fn delzo_chunk(fgu: &Fgu, i: usize) -> (u64, u64) {
    let (packed, unpacked) = fgu.sizes[i];
    let read = if unpacked != 0 { unpacked } else { packed } as u64;
    (read, if fgu.types[i] == 0 { read } else { packed as u64 })
}

/// `delzo r <fgu> <dat> <fat>` with the original tool, in pieces at once (CpuSlots). Only for
/// .fat version 9 with chunk types 0-2 whose sizes add up to the original .dat length; anything
/// else is an error, and the caller runs the original on the whole file.
pub fn delzo_parallel(fgu_path: &Path, dat: &Path, fat_path: &Path, original: &Path, cpus: usize) -> Result<String> {
    let fgu = read_fgu(fgu_path)?;
    let n = fgu.types.len();
    ensure!(fgu.fat.len() >= 8 && u32::from_le_bytes(fgu.fat[4..8].try_into().unwrap()) == 9, "not a version 9 .fat");
    ensure!(fgu.types.iter().all(|&t| t <= 2), "a chunk type delzo doesn't know");
    let chunks: Vec<(u64, u64)> = (0..n).map(|i| delzo_chunk(&fgu, i)).collect();
    let (read, out): (u64, u64) = chunks.iter().fold((0, 0), |a, c| (a.0 + c.0, a.1 + c.1));
    ensure!(out == fgu.dat_len, "chunk sizes add up to {out}, the .dat was {}", fgu.dat_len);
    ensure!(std::fs::metadata(fgu_path)?.len() == fgu.data_start + read, "the .fgu's data doesn't match its tables");
    if read < 32 << 20 || n < 64 {
        return Err(LeftToOriginal("small".into()).into());
    }
    if let Some(free) = crate::guard::free_space(dat.parent().unwrap_or(Path::new(".")))
        && free <= 2 * read + out + (1 << 30)
    {
        return Err(LeftToOriginal("not enough free space for the pieces".into()).into());
    }
    // Contiguous chunk ranges of about equal input size.
    let k = cpus.clamp(1, 64).min(n);
    let mut ranges = Vec::new();
    let (mut start, mut acc) = (0usize, 0u64);
    for (i, c) in chunks.iter().enumerate() {
        acc += c.0;
        if acc >= read * (ranges.len() as u64 + 1) / k as u64 && ranges.len() + 1 < k {
            ranges.push(start..i + 1);
            start = i + 1;
        }
    }
    if start < n {
        ranges.push(start..n);
    }
    let piece = |p: &Path, i: usize| p.with_file_name(format!("{}.arc-unpack-piece{i}", p.file_name().unwrap_or_default().to_string_lossy()));
    let pieces: Vec<[PathBuf; 3]> = (0..ranges.len()).map(|i| [piece(fgu_path, i), piece(dat, i), piece(fat_path, i)]).collect();
    let cleanup = || {
        for p in pieces.iter().flatten() {
            let _ = std::fs::remove_file(p);
        }
    };
    let result = (|| -> Result<()> {
        let mut src = std::fs::File::open(fgu_path)?;
        use std::io::{Seek, SeekFrom};
        src.seek(SeekFrom::Start(fgu.data_start))?;
        for (r, [pin, _, _]) in ranges.iter().zip(&pieces) {
            let (r_read, r_out) = chunks[r.clone()].iter().fold((0, 0), |a, c| (a.0 + c.0, a.1 + c.1));
            let nr = r.len() as u32;
            let mut w = std::io::BufWriter::with_capacity(4 << 20, std::fs::File::create(pin)?);
            for v in [0x4552_4544, fgu.max_chunk, nr, nr * 8, fgu.fat.len() as u32, 0, r_out as u32, (r_out >> 32) as u32] {
                w.write_all(&u32::to_le_bytes(v))?;
            }
            w.write_all(&fgu.types[r.clone()])?;
            for &(p, u) in &fgu.sizes[r.clone()] {
                w.write_all(&p.to_le_bytes())?;
                w.write_all(&u.to_le_bytes())?;
            }
            w.write_all(&fgu.fat)?;
            ensure!(std::io::copy(&mut (&mut src).take(r_read), &mut w)? == r_read, "short read of {}", fgu_path.display());
            w.flush()?;
        }
        let slots = CpuSlots::open(cpus);
        let failed: Vec<String> = std::thread::scope(|s| {
            let jobs: Vec<_> = pieces
                .iter()
                .map(|[pin, pdat, pfat]| {
                    let slots = &slots;
                    s.spawn(move || -> std::result::Result<(), String> {
                        let _slot = slots.take();
                        let st = std::process::Command::new(original)
                            .arg("r")
                            .arg("-t1")
                            .args([pin, pdat, pfat])
                            .stdin(std::process::Stdio::null())
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .status()
                            .map_err(|e| e.to_string())?;
                        if st.success() { Ok(()) } else { Err(format!("exit {:?}", st.code())) }
                    })
                })
                .collect();
            jobs.into_iter().filter_map(|j| j.join().unwrap_or_else(|_| Err("panicked".into())).err()).collect()
        });
        ensure!(failed.is_empty(), "a piece failed: {}", failed.join(", "));
        // Every piece wrote the whole .fat and exactly its chunks.
        for (r, [_, pdat, pfat]) in ranges.iter().zip(&pieces) {
            let want = chunks[r.clone()].iter().map(|c| c.1).sum::<u64>();
            ensure!(std::fs::metadata(pdat)?.len() == want, "a piece's .dat is not {want} bytes");
            ensure!(std::fs::read(pfat)? == fgu.fat, "a piece's .fat differs");
        }
        let mut out = std::io::BufWriter::with_capacity(4 << 20, std::fs::File::create(dat)?);
        for [_, pdat, _] in &pieces {
            std::io::copy(&mut std::fs::File::open(pdat)?, &mut out)?;
        }
        out.flush()?;
        // (In verify mode the original has written it already: left alone when it's the same.)
        if std::fs::read(fat_path).ok().as_deref() != Some(&fgu.fat[..]) {
            std::fs::write(fat_path, &fgu.fat)?;
        }
        Ok(())
    })();
    cleanup();
    if let Err(e) = result {
        // (Only the .dat: the original, run next, writes the .fat again, and in verify mode
        // the .fat is the original's own.)
        let _ = std::fs::remove_file(dat);
        return Err(e);
    }
    Ok(format!("{n} chunks, {} MB, in {} pieces", read >> 20, ranges.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hdiffpatch_directory_diffs() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("new.x5n");
        let with = |flags: [u8; 2]| {
            let mut h = b"HDIFF19&zstd&fadler64".to_vec();
            h.push(0);
            h.extend(flags);
            h.extend([7u8; 20]);
            std::fs::write(&f, &h).unwrap();
            is_dir_diff(&f)
        };
        assert!(with([1, 1]), "folder to folder");
        assert!(!with([0, 1]), "file to folder");
        std::fs::write(&f, b"HDIFF13&zstd").unwrap();
        assert!(!is_dir_diff(&f), "a single-file diff");
        // Only the tools that touch just their named files may run beside other steps.
        assert!(Role::X2n.touches_only_named_files() && !Role::FreeArcUnarc.touches_only_named_files());
        assert!(!Role::RunExe.touches_only_named_files() && !Role::X5nDirPatch.touches_only_named_files());
    }

    #[test]
    fn run_lists_with_independent_lines() {
        let here = Path::new(r"C:\run\cwd");
        // Farming Simulator 22's fitgirl01.txt (after fart): each line has its own NNNN.* files.
        let t = r"C:\run\tmp";
        let line = |n: &str| format!("cmd /c \"\"{t}\\work\\fgpack.exe\" {n}.fgu {n}.fgr_&&del {n}.fgu&&\"{t}\\work\\x.exe\" -f -d -s {n}.fgr_ {n}.shapes.x {n}.shapes&&del {n}.fgr_ {n}.shapes.x\"\r\n");
        let fs22: String = ["0001", "0002", "0003"].iter().map(|n| line(n)).collect();
        let none = |_: &str| 0;
        assert!(run_list_lines_independent(&fs22, here, &none));
        assert_eq!(run_list_programs(&fs22), vec![format!("{t}\\work\\fgpack.exe"), format!("{t}\\work\\x.exe")]);
        // A line that works on another line's output must keep the list's own order and count.
        let chained = format!("{}cmd /c copy /b 0001.shapes+0002.shapes all.shapes\r\n", fs22);
        assert!(!run_list_lines_independent(&chained, here, &none));
        // Redirection targets are files (Batman: Arkham Asylum's `echo 123>N.test`); nul isn't.
        assert!(run_list_lines_independent("cmd /c \"echo 123>1.test\"\r\ncmd /c \"echo 123>2.test\"\r\n", here, &none));
        assert!(!run_list_lines_independent("cmd /c \"echo 1>a.log\"\r\ncmd /c \"echo 2>>a.log\"\r\n", here, &none));
        let cricket = "cmd /c \"\"w\\fgpack.exe\" e 04.fgpack 04.fgpack_ >nul 2>&1&&del 04.fgpack\"\r\ncmd /c \"\"w\\fgpack.exe\" e 21.fgpack 21.fgpack_ >nul 2>&1&&del 21.fgpack\"\r\n";
        // ("e" is fgpack's mode word: as a file name, every line would share it.)
        let fgpack = |w: &str| usize::from(w.to_ascii_lowercase().ends_with("fgpack.exe"));
        assert!(run_list_lines_independent(cricket, here, &fgpack));
        assert!(!run_list_lines_independent(cricket, here, &none));
        // run.exe expands variables: what such a line touches isn't known here.
        assert!(!run_list_lines_independent("tool.exe %TEMP%\\a b\r\n", here, &none));
        // Nor folder commands, wildcards, a single & or a glued program.
        let x = |rest: &str| format!("cmd /c \"\"w\\x.exe\" -f -d -s {rest}\"\r\n");
        assert!(!run_list_lines_independent(&format!("cmd /c \"md out\"\r\n{}", x("1.bin 1.x out\\1.bin")), here, &none));
        assert!(!run_list_lines_independent(&x("*.bin 1.x 1.out"), here, &none));
        assert!(!run_list_lines_independent(&format!("{}cmd /c \"del a.bin&del b.bin\"\r\n", x("1.bin 1.x 1.out")), here, &none));
        assert!(!run_list_lines_independent("cmd /c \"w\\tool.exe>nul a b\"\r\n", here, &none));
        // A folder one line names and a file in it another line names.
        assert!(!run_list_lines_independent(&format!("{}{}", x("1.bin 1.x out\\1.bin"), x("2.bin 2.x out")), here, &none));
        assert!(!run_list_lines_independent(&format!("{}{}", x("1.bin 1.x base.dat>nul"), x("base.dat 2.x 2.out")), here, &none));
        // A quoted path with a blank is one word (a user name with a space).
        let spaced = "cmd /c \"\"C:\\Users\\John Smith\\tmp\\work\\x.exe\" -f -d -s 1.fgr 1.x 1.out&&del 1.fgr\"\r\ncmd /c \"\"C:\\Users\\John Smith\\tmp\\work\\x.exe\" -f -d -s 2.fgr 2.x 2.out&&del 2.fgr\"\r\n";
        assert!(run_list_lines_independent(spaced, here, &none));
        assert_eq!(run_list_programs(spaced), vec!["C:\\Users\\John Smith\\tmp\\work\\x.exe".to_string()]);
        // A redirection target after a blank, a folder written another way, a program a line
        // deletes, and cmd.exe written out.
        assert!(!run_list_lines_independent(&format!("{}cmd /c \"echo 1 >> C:\\run\\cwd\\1.out\"\r\n", x("1.bin 1.x 1.out")), here, &none));
        assert!(!run_list_lines_independent(&format!("{}{}", x("1.bin 1.x C:\\run\\cwd\\out"), x("2.bin 2.x out\\2.bin")), here, &none));
        assert!(!run_list_lines_independent(&format!("{}cmd /c \"del w\\x.exe\"\r\n", x("1.bin 1.x 1.out")), here, &none));
        let cmd_exe = "CMD.EXE /c \"\"w\\x.exe\" -f 1.a 1.b\"\r\ncmd.exe /C \"\"w\\x.exe\" -f 2.a 2.b\"\r\n";
        assert_eq!(run_list_programs(cmd_exe), vec!["w\\x.exe".to_string()]);
        assert!(run_list_lines_independent(cmd_exe, here, &none));
    }

    #[test]
    fn run_lists_largest_first() {
        let dir = tempfile::tempdir().unwrap();
        for (n, len) in [("001", 10usize), ("002", 300), ("003", 20)] {
            std::fs::write(dir.path().join(format!("{n}.fgu")), vec![0u8; len]).unwrap();
        }
        let line = |n: &str| format!("cmd /c \"\"w\\fgpack2.exe\" e -d19 {n}.fgu {n}.fgr&&del {n}.fgu\"");
        let list = format!("{}\r\n# note\r\n{}\r\n{}", line("001"), line("002"), line("003"));
        let lzma = |_: &str| 1;
        assert!(run_list_lines_independent(&list, dir.path(), &lzma));
        let sorted = run_list_largest_first(list.as_bytes(), dir.path(), &lzma).unwrap();
        assert_eq!(String::from_utf8(sorted).unwrap(), format!("{}\r\n{}\r\n{}\r\n# note\r\n", line("002"), line("003"), line("001")));
        // Already in order: nothing to do.
        let list = format!("{}\r\n{}\r\n", line("002"), line("001"));
        assert!(run_list_largest_first(list.as_bytes(), dir.path(), &lzma).is_none());
    }

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
