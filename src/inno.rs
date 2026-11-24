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

#[derive(Debug, Clone)]
pub struct SetupFile {
    /// Destination as written in the installer, e.g. `{tmp}\arc.ini`.
    pub dest: String,
    pub size: u64,
    location: Option<usize>,
}

#[derive(Debug)]
pub struct Setup {
    pub version: String,
    pub files: Vec<SetupFile>,
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
fn header_counts(b: &[u8]) -> Result<(usize, usize)> {
    let mut r = Reader { b, o: 0 };
    // 27 binary strings (app name ... close_applications_filter), 3 ANSI texts, compiled code.
    for _ in 0..31 {
        r.bstr()?;
    }
    let counts: Vec<u32> = (0..16).map(|_| r.u32()).collect::<Result<_>>()?;
    // language, message, permission, type, component, task, directory, FILE, DATA, icon, ...
    Ok((counts[7] as usize, counts[8] as usize))
}

fn parse_file_entry(r: &mut Reader, n_data: usize) -> Result<SetupFile> {
    let _source = r.wstr()?;
    let dest = r.wstr()?;
    for _ in 0..8 {
        r.wstr()?; // font, assembly, components, tasks, languages, check, after/before install
    }
    r.raw(20)?; // windows version range
    let location = r.u32()?;
    r.u32()?; // attributes
    r.u64()?; // external size
    r.raw(2)?; // permission index
    r.u32()?; // 32 option flags
    let kind = r.u8()?;
    ensure!(kind <= 2, "bad file type");
    ensure!(!dest.is_empty(), "empty destination");
    let location = if location == u32::MAX { None } else { Some(location as usize) };
    ensure!(location.is_none_or(|l| l < n_data), "bad data location");
    Ok(SetupFile { dest, size: 0, location })
}

/// Finds the file-entry table by trying every plausible start ("{" as the first character of
/// a destination after an empty source) and keeping the one where all entries parse.
fn find_file_entries(b: &[u8], n_files: usize, n_data: usize) -> Result<Vec<SetupFile>> {
    let mut o = 0;
    while let Some(p) = b[o + 1..].windows(2).position(|w| w == b"{\0").map(|p| p + o + 1) {
        o = p;
        let Some(start) = p.checked_sub(8) else { continue };
        if u32_at(b, start)? != 0 || !(2..2048).contains(&u32_at(b, start + 4)?) {
            continue;
        }
        let mut r = Reader { b, o: start };
        let parsed: Result<Vec<SetupFile>> = (0..n_files).map(|_| parse_file_entry(&mut r, n_data)).collect();
        if let Ok(files) = parsed
            && files.iter().all(|f| f.dest.contains('\\') || f.dest.contains('}'))
        {
            return Ok(files);
        }
    }
    bail!("file entries not found (unsupported Inno Setup variant?)")
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
        let (n_files, n_data) = header_counts(&b1)?;
        let data = parse_data_entries(&b2, n_data)?;
        let mut files = find_file_entries(&b1, n_files, n_data)?;
        for f in &mut files {
            f.size = f.location.map(|l| data[l].size).unwrap_or(0);
        }
        Ok(Setup { version, files, data, data_offset, blob })
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

    /// Extracts every embedded file under `out_dir`, mapping `{tmp}\x` to `out_dir\tmp\x`.
    /// Returns (destination, path) pairs. Fails on any SHA-1 mismatch.
    pub fn extract_all(&self, out_dir: &Path) -> Result<Vec<(String, PathBuf)>> {
        let mut order: Vec<&SetupFile> = self.files.iter().filter(|f| f.location.is_some()).collect();
        order.sort_by_key(|f| {
            let d = &self.data[f.location.unwrap()];
            (d.chunk_offset, d.file_offset)
        });
        let mut cache: Option<(u32, Vec<u8>)> = None;
        let mut written = Vec::new();
        for f in order {
            let d = &self.data[f.location.unwrap()];
            if cache.as_ref().is_none_or(|(off, _)| *off != d.chunk_offset) {
                cache = Some((d.chunk_offset, self.chunk(d)?));
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
            let path = out_dir.join(safe_dest(&f.dest)?);
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
fn safe_dest(dest: &str) -> Result<PathBuf> {
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
