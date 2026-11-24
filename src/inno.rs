//! Read-only extractor for Inno Setup 5.5.x (Unicode) single-file installers, so `install` can
//! get a repack's codecs out of `setup.exe` without running it.
//!
//! Layouts follow innoextract's documentation of the format (loader/offsets, stream/block,
//! stream/chunk, setup/header, setup/file, setup/data, stream/exefilter). Every extracted file
//! is verified against the SHA-1 stored in the installer.

use anyhow::{Context, Result, bail, ensure};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Offset table magic for Inno Setup 5.1.5 and later.
const MAGIC_515: &[u8; 12] = b"rDlPtS\xcd\xe6\xd7{\x0b*";
const DATA_ENTRY_SIZE: usize = 74; // 5.5.x layout
const FLAG_CALL_FILTER: u16 = 1 << 4;
const FLAG_ENCRYPTED: u16 = 1 << 6;
const FLAG_COMPRESSED: u16 = 1 << 7;
const FILE_TYPE_UNINSTALLER: u8 = 1;
/// A file entry whose 10 strings are all empty: 10 length prefixes + the fixed 43-byte tail.
const EMPTY_FILE_ENTRY: usize = 10 * 4 + 43;

#[derive(Debug, Clone)]
pub struct SetupFile {
    /// Destination as written in the installer, e.g. `{tmp}\arc.ini`.
    pub dest: String,
    pub size: u64,
    /// Inno conditions, as written in the script (empty = unconditional).
    pub components: String,
    pub tasks: String,
    pub languages: String,
    pub check: String,
    location: Option<usize>,
}

impl SetupFile {
    /// `{app}\x\y` -> `x\y` for files the installer puts in the game folder.
    pub fn app_relative(&self) -> Option<&str> {
        let rest = self.dest.strip_prefix("{app}")?;
        let rest = rest.trim_start_matches(['\\', '/']);
        (!rest.is_empty() && !rest.ends_with(['\\', '/'])).then_some(rest)
    }

    /// Whether an embedded file can be written at all (directory entries and files the
    /// installer downloads or finds elsewhere cannot).
    pub fn has_data(&self) -> bool {
        self.location.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct SetupType {
    pub name: String,
    pub custom: bool,
}

#[derive(Debug, Clone)]
pub struct Component {
    pub name: String,
    pub description: String,
    /// Comma-separated setup types that include this component.
    pub types: String,
    pub languages: String,
    pub fixed: bool,
    pub exclusive: bool,
}

/// A shortcut from the installer's `[Icons]` section, e.g. `{commondesktop}\Game` ->
/// `{app}\Game.exe`.
#[derive(Debug, Clone)]
pub struct Icon {
    pub name: String,
    pub filename: String,
    pub parameters: String,
    pub working_dir: String,
    pub components: String,
    pub tasks: String,
    pub languages: String,
}

/// 5.5.x icon entries: name, file, parameters, working dir, icon file, comment, six condition
/// strings and the AppUserModelID, then a fixed 32-byte tail.
fn parse_icons(b: &[u8], start: usize, n: usize) -> Result<Vec<Icon>> {
    let mut r = Reader { b, o: start };
    let mut out = Vec::new();
    for _ in 0..n {
        let mut s: Vec<String> = (0..13).map(|_| r.wstr()).collect::<Result<_>>()?;
        r.raw(20 + 4 + 4 + 1 + 2 + 1)?;
        ensure!(!s[0].is_empty() && !s[1].is_empty(), "not an icon entry");
        let take = |s: &mut Vec<String>, i: usize| std::mem::take(&mut s[i]);
        out.push(Icon {
            name: take(&mut s, 0),
            filename: take(&mut s, 1),
            parameters: take(&mut s, 2),
            working_dir: take(&mut s, 3),
            components: take(&mut s, 6),
            tasks: take(&mut s, 7),
            languages: take(&mut s, 8),
        });
    }
    Ok(out)
}

#[derive(Debug)]
pub struct Setup {
    pub version: String,
    pub files: Vec<SetupFile>,
    /// Setup types (e.g. "full", "custom"); the first one is the default.
    pub types: Vec<SetupType>,
    pub components: Vec<Component>,
    /// Shortcuts the installer creates (`[Icons]`).
    pub icons: Vec<Icon>,
    /// Compiled `[Code]` section (empty if the installer has none).
    pub code: Vec<u8>,
    data: Vec<DataEntry>,
    data_offset: u64,
    blob: Vec<u8>,
}

#[derive(Debug, Clone)]
struct DataEntry {
    chunk_offset: u32,
    file_offset: u64,
    size: u64,
    chunk_size: u64,
    sha1: [u8; 20],
    flags: u16,
}

fn u32_at(b: &[u8], o: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(b.get(o..o + 4).context("truncated")?.try_into().unwrap()))
}
fn u64_at(b: &[u8], o: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(b.get(o..o + 8).context("truncated")?.try_into().unwrap()))
}

struct Reader<'a> {
    b: &'a [u8],
    o: usize,
}

impl Reader<'_> {
    fn u8(&mut self) -> Result<u8> {
        let v = *self.b.get(self.o).context("truncated")?;
        self.o += 1;
        Ok(v)
    }
    fn u16(&mut self) -> Result<u16> {
        let v = u16::from_le_bytes(self.b.get(self.o..self.o + 2).context("truncated")?.try_into().unwrap());
        self.o += 2;
        Ok(v)
    }
    fn u32(&mut self) -> Result<u32> {
        let v = u32_at(self.b, self.o)?;
        self.o += 4;
        Ok(v)
    }
    fn u64(&mut self) -> Result<u64> {
        let v = u64_at(self.b, self.o)?;
        self.o += 8;
        Ok(v)
    }
    fn raw(&mut self, n: usize) -> Result<&[u8]> {
        let v = self.b.get(self.o..self.o + n).context("truncated")?;
        self.o += n;
        Ok(v)
    }
    fn bstr(&mut self) -> Result<&[u8]> {
        let n = self.u32()? as usize;
        ensure!(n <= self.b.len() - self.o, "string past end");
        self.raw(n)
    }
    fn wstr(&mut self) -> Result<String> {
        let s = self.bstr()?;
        ensure!(s.len() % 2 == 0, "odd UTF-16 length");
        let w: Vec<u16> = s.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16(&w).context("bad UTF-16")
    }
}

/// Decodes an LZMA1 stream that starts with Inno's 5-byte header (props + dict size) and has
/// neither a stored size nor necessarily an end marker.
fn lzma1(data: &[u8]) -> Result<Vec<u8>> {
    ensure!(data.len() >= 5, "LZMA stream too short");
    let opts = lzma_rs::decompress::Options {
        unpacked_size: lzma_rs::decompress::UnpackedSize::ReadFromHeader,
        memlimit: None,
        allow_incomplete: true,
    };
    let mut stream = lzma_rs::decompress::Stream::new_with_options(&opts, Vec::new());
    stream.write_all(&data[..5])?;
    stream.write_all(&u64::MAX.to_le_bytes())?; // "size unknown"
    stream.write_all(&data[5..])?;
    stream.finish().map_err(|e| anyhow::anyhow!("LZMA: {e:?}"))
}

/// setup-0 block: crc32(4) | stored_size(4) | compressed(1) | [crc32(4) + <=4096 bytes]*
fn read_block(blob: &[u8], pos: usize) -> Result<(Vec<u8>, usize)> {
    let hdr = blob.get(pos..pos + 9).context("block header past end")?;
    ensure!(crc32fast::hash(&hdr[4..]) == u32_at(hdr, 0)?, "block header CRC mismatch");
    let stored = u32_at(hdr, 4)? as usize;
    let compressed = hdr[8] != 0;
    let raw = blob.get(pos + 9..pos + 9 + stored).context("block past end")?;
    let mut data = Vec::with_capacity(stored);
    let mut o = 0;
    while o < raw.len() {
        let crc = u32_at(raw, o)?;
        let end = (o + 4 + 4096).min(raw.len());
        let piece = &raw[o + 4..end];
        ensure!(crc32fast::hash(piece) == crc, "block chunk CRC mismatch");
        data.extend_from_slice(piece);
        o = end;
    }
    let data = if compressed { lzma1(&data)? } else { data };
    Ok((data, pos + 9 + stored))
}

/// File and data-entry counts from the 5.5.x setup header.
/// Entry counts from the 5.5.x setup header: language, message, permission, type, component,
/// task, directory, file, data, icon, ini, registry, delete, uninstall delete, run, uninstall run.
fn header_counts(b: &[u8]) -> Result<Vec<usize>> {
    let mut r = Reader { b, o: 0 };
    // 27 binary strings (app name ... close_applications_filter), 3 ANSI texts, compiled code.
    for _ in 0..31 {
        r.bstr()?;
    }
    (0..16).map(|_| r.u32().map(|v| v as usize)).collect()
}

/// The 31st header string: the compiled `[Code]` section (Pascal Script bytecode).
fn compiled_code(b: &[u8]) -> Result<Vec<u8>> {
    let mut r = Reader { b, o: 0 };
    for _ in 0..30 {
        r.bstr()?;
    }
    Ok(r.bstr()?.to_vec())
}

fn parse_file_entry(r: &mut Reader, n_data: usize) -> Result<SetupFile> {
    let _source = r.wstr()?;
    let dest = r.wstr()?;
    r.wstr()?; // install font name
    r.wstr()?; // strong assembly name
    let components = r.wstr()?;
    let tasks = r.wstr()?;
    let languages = r.wstr()?;
    let check = r.wstr()?;
    r.wstr()?; // after install
    r.wstr()?; // before install
    r.raw(20)?; // windows version range
    let location = r.u32()?;
    r.u32()?; // attributes
    r.u64()?; // external size
    r.raw(2)?; // permission index
    r.u32()?; // 32 option flags
    let kind = r.u8()?;
    ensure!(kind <= 2, "bad file type");
    // Only the uninstaller's own entry (type 1) has no destination; it is never extracted.
    ensure!(!dest.is_empty() || kind == FILE_TYPE_UNINSTALLER, "empty destination");
    let location = if location == u32::MAX || dest.is_empty() { None } else { Some(location as usize) };
    ensure!(location.is_none_or(|l| l < n_data), "bad data location");
    Ok(SetupFile { dest, size: 0, components, tasks, languages, check, location })
}

/// Finds the file-entry table by trying every plausible start ("{" as the first character of
/// a destination after an empty source) and keeping the one where all entries parse.
/// Returns the entries and the offset right after the table (where the icon entries start).
fn find_file_entries(b: &[u8], n_files: usize, n_data: usize) -> Result<(Vec<SetupFile>, usize)> {
    let parse_from = |start: usize| -> Result<(Vec<SetupFile>, usize)> {
        let mut r = Reader { b, o: start };
        let files = (0..n_files).map(|_| parse_file_entry(&mut r, n_data)).collect::<Result<_>>()?;
        Ok((files, r.o))
    };
    // Only the uninstaller's entry may lack a destination (parse_file_entry enforces that).
    let plausible = |f: &SetupFile| f.dest.is_empty() || f.dest.contains('\\') || f.dest.contains('}');
    let mut o = 0;
    while let Some(p) = b[o + 1..].windows(2).position(|w| w == b"{\0").map(|p| p + o + 1) {
        o = p;
        let Some(start) = p.checked_sub(8) else { continue };
        if u32_at(b, start)? != 0 || !(2..2048).contains(&u32_at(b, start + 4)?) {
            continue;
        }
        // The uninstaller's own entry (all strings empty) usually comes first, and the anchor
        // above can't see it. Without it the table would end one entry too late, inside the
        // icon entries, and produce a bogus file.
        if let Some(prev) = start.checked_sub(EMPTY_FILE_ENTRY)
            && b[prev..prev + 40].iter().all(|&x| x == 0)
            && b[start - 1] == FILE_TYPE_UNINSTALLER
            && let Ok(found) = parse_from(prev)
            && found.0.iter().all(plausible)
        {
            return Ok(found);
        }
        if let Ok(found) = parse_from(start)
            && found.0.iter().all(plausible)
        {
            return Ok(found);
        }
    }
    bail!("file entries not found (unsupported Inno Setup variant?)")
}

fn parse_type_entry(r: &mut Reader) -> Result<SetupType> {
    let name = r.wstr()?;
    for _ in 0..3 {
        r.wstr()?; // description, languages, check
    }
    r.raw(20)?; // windows version range
    let flags = r.u8()?;
    let kind = r.u8()?;
    r.u64()?; // size
    ensure!(!name.is_empty() && kind <= 3 && flags <= 1, "not a type entry");
    Ok(SetupType { name, custom: flags & 1 != 0 })
}

fn parse_component_entry(r: &mut Reader) -> Result<Component> {
    let name = r.wstr()?;
    let description = r.wstr()?;
    let types = r.wstr()?;
    let languages = r.wstr()?;
    r.wstr()?; // check
    r.u64()?; // extra disk space
    let level = r.u32()?;
    let used = r.u8()?;
    r.raw(20)?; // windows version range
    let flags = r.u8()?;
    r.u64()?; // size
    ensure!(!name.is_empty() && level < 16 && used <= 1 && flags < 32, "not a component entry");
    Ok(Component { name, description, types, languages, fixed: flags & 1 != 0, exclusive: flags & 8 != 0 })
}

/// Finds the type and component tables (they are adjacent) by trying every start offset and
/// keeping the first where all entries parse and every component's types are known types.
fn find_component_tables(b: &[u8], n_types: usize, n_components: usize) -> Result<(Vec<SetupType>, Vec<Component>)> {
    if n_types == 0 {
        return Ok((Vec::new(), Vec::new()));
    }
    for start in 0..b.len().saturating_sub(8) {
        // A type entry starts with a non-empty UTF-16 name.
        if !(2..256).contains(&u32_at(b, start).unwrap_or(0)) {
            continue;
        }
        let mut r = Reader { b, o: start };
        let Ok(types) = (0..n_types).map(|_| parse_type_entry(&mut r)).collect::<Result<Vec<_>>>() else { continue };
        let Ok(components) = (0..n_components).map(|_| parse_component_entry(&mut r)).collect::<Result<Vec<_>>>() else {
            continue;
        };
        let known = |list: &str| list.split(',').map(str::trim).filter(|t| !t.is_empty()).all(|t| types.iter().any(|x| x.name.eq_ignore_ascii_case(t)));
        if types.iter().all(|t| t.name.chars().all(|c| !c.is_control())) && components.iter().all(|c| known(&c.types)) {
            return Ok((types, components));
        }
    }
    bail!("setup types/components not found")
}

fn parse_data_entries(b: &[u8], n: usize) -> Result<Vec<DataEntry>> {
    ensure!(b.len() == n * DATA_ENTRY_SIZE, "data block is {} bytes, expected {}x{}", b.len(), n, DATA_ENTRY_SIZE);
    b.chunks_exact(DATA_ENTRY_SIZE)
        .map(|c| {
            let mut r = Reader { b: c, o: 0 };
            r.u32()?; // first slice
            r.u32()?; // last slice
            let chunk_offset = r.u32()?;
            let file_offset = r.u64()?;
            let size = r.u64()?;
            let chunk_size = r.u64()?;
            let sha1: [u8; 20] = r.raw(20)?.try_into().unwrap();
            r.u64()?; // timestamp
            r.u32()?; // version ms
            r.u32()?; // version ls
            let flags = r.u16()?;
            Ok(DataEntry { chunk_offset, file_offset, size, chunk_size, sha1, flags })
        })
        .collect()
}

/// Inno Setup 5.3.9+ x86 call-instruction filter (undone on extraction).
fn decode_call_filter(data: &mut [u8]) {
    let n = data.len();
    let mut i = 0;
    while i < n {
        let b = data[i];
        i += 1;
        if b != 0xE8 && b != 0xE9 {
            continue;
        }
        if 0x10000 - ((i - 1) % 0x10000) < 5 {
            continue;
        }
        if i + 4 > n {
            break;
        }
        i += 4;
        if data[i - 1] == 0x00 || data[i - 1] == 0xFF {
            let rel = (data[i - 4] as u32) | (data[i - 3] as u32) << 8 | (data[i - 2] as u32) << 16;
            let rel = rel.wrapping_sub((i as u32) & 0xFF_FFFF);
            data[i - 4] = rel as u8;
            data[i - 3] = (rel >> 8) as u8;
            data[i - 2] = (rel >> 16) as u8;
            if rel & 0x80_0000 != 0 {
                data[i - 1] = !data[i - 1];
            }
        }
    }
}

impl Setup {
    pub fn open(exe: &Path) -> Result<Setup> {
        let blob = std::fs::read(exe).with_context(|| format!("read {}", exe.display()))?;
        let m = blob.windows(12).position(|w| w == MAGIC_515).context("not an Inno Setup 5.1.5+ installer")?;
        let vals: Vec<u32> = (0..8).map(|k| u32_at(&blob, m + 12 + 4 * k)).collect::<Result<_>>()?;
        ensure!(crc32fast::hash(&blob[m..m + 40]) == vals[7], "installer offset table CRC mismatch");
        let (header_offset, data_offset) = (vals[5] as usize, vals[6] as u64);
        let id = blob.get(header_offset..header_offset + 64).context("header past end")?;
        let version = String::from_utf8_lossy(id.split(|&b| b == 0).next().unwrap_or_default()).into_owned();
        if !(version.contains("(5.5.") && version.contains("(u)")) {
            bail!("unsupported installer version '{version}' (only Inno Setup 5.5.x Unicode is supported)");
        }
        let (b1, end1) = read_block(&blob, header_offset + 64)?;
        let (b2, _) = read_block(&blob, end1)?;
        let counts = header_counts(&b1)?;
        let (n_files, n_data) = (counts[7], counts[8]);
        let data = parse_data_entries(&b2, n_data)?;
        let (mut files, files_end) = find_file_entries(&b1, n_files, n_data)?;
        // Shortcuts follow the files; only needed for "create desktop shortcut", so optional.
        let icons = parse_icons(&b1, files_end, counts[9]).unwrap_or_default();
        for f in &mut files {
            f.size = f.location.map(|l| data[l].size).unwrap_or(0);
        }
        // Types and components only matter for running the install script; don't fail on them.
        let (types, components) = find_component_tables(&b1, counts[3], counts[4]).unwrap_or_default();
        let code = compiled_code(&b1)?;
        Ok(Setup { version, files, types, components, icons, code, data, data_offset, blob })
    }

    fn chunk(&self, d: &DataEntry) -> Result<Vec<u8>> {
        let start = self.data_offset as usize + d.chunk_offset as usize;
        ensure!(self.blob.get(start..start + 4) == Some(b"zlb\x1a"), "bad chunk magic");
        let raw = self.blob.get(start + 4..start + 4 + d.chunk_size as usize).context("chunk past end")?;
        ensure!(d.flags & FLAG_ENCRYPTED == 0, "encrypted installer chunks are not supported");
        if d.flags & FLAG_COMPRESSED == 0 {
            return Ok(raw.to_vec());
        }
        match raw.first().copied() {
            Some(p) if p <= 40 => {
                let mut out = Vec::new();
                lzma_rs::lzma2_decompress(&mut &raw[1..], &mut out).map_err(|e| anyhow::anyhow!("LZMA2: {e:?}"))?;
                Ok(out)
            }
            Some(0x78) => bail!("zlib-compressed installers are not supported"),
            Some(_) => lzma1(raw),
            None => bail!("empty chunk"),
        }
    }

    /// One embedded file's bytes (SHA-1 verified). `cache` keeps the last decompressed chunk.
    fn content(&self, f: &SetupFile, cache: &mut Option<(u32, Vec<u8>)>) -> Result<Vec<u8>> {
        let d = &self.data[f.location.context("file has no embedded data")?];
        if cache.as_ref().is_none_or(|(off, _)| *off != d.chunk_offset) {
            *cache = Some((d.chunk_offset, self.chunk(d)?));
        }
        let chunk = &cache.as_ref().unwrap().1;
        let mut content = chunk
            .get(d.file_offset as usize..(d.file_offset + d.size) as usize)
            .with_context(|| format!("{}: chunk too short", f.dest))?
            .to_vec();
        if d.flags & FLAG_CALL_FILTER != 0 {
            decode_call_filter(&mut content);
        }
        let digest = sha1_smol::Sha1::from(&content).digest().bytes();
        ensure!(digest == d.sha1, "{}: SHA-1 mismatch", f.dest);
        Ok(content)
    }

    /// Reads one embedded file into memory.
    pub fn read(&self, f: &SetupFile) -> Result<Vec<u8>> {
        self.content(f, &mut None)
    }

    /// Extracts every embedded file under `out_dir`, mapping `{tmp}\x` to `out_dir\tmp\x`.
    /// Returns (destination, path) pairs. Fails on any SHA-1 mismatch.
    pub fn extract_all(&self, out_dir: &Path) -> Result<Vec<(String, PathBuf)>> {
        self.extract_with(|f| Ok(Some(out_dir.join(safe_dest(&f.dest)?))))
    }

    /// Extracts the embedded files `place` maps to a path (None = skip). Fails on any SHA-1
    /// mismatch.
    pub fn extract_with(&self, mut place: impl FnMut(&SetupFile) -> Result<Option<PathBuf>>) -> Result<Vec<(String, PathBuf)>> {
        let mut order: Vec<(&SetupFile, PathBuf)> = Vec::new();
        for f in self.files.iter().filter(|f| f.location.is_some()) {
            if let Some(path) = place(f)? {
                order.push((f, path));
            }
        }
        order.sort_by_key(|(f, _)| {
            let d = &self.data[f.location.unwrap()];
            (d.chunk_offset, d.file_offset)
        });
        let mut cache: Option<(u32, Vec<u8>)> = None;
        let mut written = Vec::new();
        for (f, path) in order {
            let content = self.content(f, &mut cache)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, &content).with_context(|| format!("write {}", path.display()))?;
            written.push((f.dest.clone(), path));
        }
        Ok(written)
    }
}

/// `{tmp}\cls-srep.dll` -> `tmp\cls-srep.dll`, refusing anything that could escape.
pub fn safe_dest(dest: &str) -> Result<PathBuf> {
    let mut p = PathBuf::new();
    for part in dest.split(['\\', '/']) {
        if part.is_empty() || part == "." {
            continue;
        }
        ensure!(part != ".." && !part.contains(':'), "unsafe path in installer: {dest}");
        p.push(part.replace(['{', '}'], ""));
    }
    ensure!(!p.as_os_str().is_empty(), "empty path in installer");
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_filter_roundtrip_of_known_bytes() {
        // E8 followed by an address whose high byte is 00 gets rebased by its position.
        let mut d = vec![0xE8, 0x10, 0x00, 0x00, 0x00, 0x90];
        decode_call_filter(&mut d);
        // rel = 0x10 - 5 = 0x0B
        assert_eq!(d, vec![0xE8, 0x0B, 0x00, 0x00, 0x00, 0x90]);
    }

    #[test]
    fn dest_mapping() {
        assert_eq!(safe_dest(r"{tmp}\cls-srep.dll").unwrap(), Path::new("tmp").join("cls-srep.dll"));
        assert!(safe_dest(r"{app}\..\x").is_err());
    }

    fn wstr(out: &mut Vec<u8>, s: &str) {
        let w: Vec<u8> = s.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
        out.extend_from_slice(&(w.len() as u32).to_le_bytes());
        out.extend_from_slice(&w);
    }

    /// One 5.5.x file entry: source, dest, font, assembly, 6 condition strings, fixed tail.
    fn entry(out: &mut Vec<u8>, dest: &str, components: &str, location: u32, kind: u8) {
        for s in ["", dest, "", "", components, "", "", "", "", ""] {
            wstr(out, s);
        }
        out.extend_from_slice(&[0u8; 20]); // windows versions
        out.extend_from_slice(&location.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // attributes
        out.extend_from_slice(&0u64.to_le_bytes()); // external size
        out.extend_from_slice(&[0xFF, 0xFF]); // permission
        out.extend_from_slice(&0u32.to_le_bytes()); // options
        out.push(kind);
    }

    #[test]
    fn file_table_includes_the_uninstaller_entry() {
        let mut b = vec![7u8; 33]; // whatever precedes the table
        entry(&mut b, "", "", u32::MAX, FILE_TYPE_UNINSTALLER);
        entry(&mut b, r"{tmp}\a.dll", "", 0, 0);
        entry(&mut b, r"{app}\_Redist\b.addon", "bonus", 1, 0);
        // An icon entry follows; it must not be read as a fourth file.
        for s in [r"{group}\Game", r"{app}\Game.exe", "", "", "", "", "", "", "", ""] {
            wstr(&mut b, s);
        }
        b.extend_from_slice(&[0u8; 64]);

        let (files, _) = find_file_entries(&b, 3, 2).unwrap();
        let dests: Vec<&str> = files.iter().map(|f| f.dest.as_str()).collect();
        assert_eq!(dests, ["", r"{tmp}\a.dll", r"{app}\_Redist\b.addon"]);
        assert!(!files[0].has_data());
        assert_eq!(files[0].app_relative(), None);
        assert_eq!(files[2].app_relative(), Some(r"_Redist\b.addon"));
        assert_eq!(files[2].components, "bonus");
    }

    /// Runs against a real installer when ARC_UNPACK_TEST_SETUP points at one.
    #[test]
    fn real_installer_if_available() {
        let Some(exe) = std::env::var_os("ARC_UNPACK_TEST_SETUP") else { return };
        let setup = Setup::open(Path::new(&exe)).unwrap();
        let dir = std::env::temp_dir().join(format!("arcunp-inno-{}", std::process::id()));
        let files = setup.extract_all(&dir).unwrap();
        assert!(!files.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
