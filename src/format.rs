//! FreeArc container parsing: footer -> control blocks -> directory -> data blocks + file entries.
//!
//! Layout (verified against real archives, see README):
//!   [header block "ArC\x01" + version][descriptor]
//!   [data block 0][data block 1]...            (no descriptors of their own)
//!   [directory block][descriptor]
//!   [footer block][descriptor]                 (footer descriptor ends exactly at EOF)
//!
//! Every descriptor is: "ArC\x01", type (varint), compressor (C string),
//! origsize (varint), compsize (varint), crc (u32), descriptor crc (u32).
//! The block's bytes sit immediately *before* its descriptor.
//!
//! The footer lists the control blocks (header, directories) with positions stored as
//! `footer_data_pos - pos`. A directory lists its data blocks with positions stored as
//! `dir_data_pos - pos`.
//!
//! Stock FreeArc uses standard CRC-32 everywhere. Some repacks ship a modified unarc with a
//! different checksum, so `Archive::standard_crc` records whether the directory blocks' CRCs
//! match standard CRC-32; file CRCs are only verified when they do.

use anyhow::{Context, Result, bail, ensure};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const SIGNATURE: &[u8; 4] = b"ArC\x01";

pub const BLOCK_DIR: u64 = 3;
pub const BLOCK_FOOTER: u64 = 4;

/// Cursor over an in-memory block with FreeArc's primitive decoders.
pub struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(self.remaining() >= n, "block truncated (wanted {n} bytes at offset {})", self.pos);
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    /// FreeArc variable-length integer. The number of trailing 1-bits in the first byte gives
    /// the length: `xxxxxxx0` = 1 byte (value >> 1), `xxxxxx01` = 2 bytes (>> 2), ...,
    /// `x1111111` = 8 bytes (>> 8), `11111111` = marker followed by a plain 8-byte value.
    pub fn varint(&mut self) -> Result<u64> {
        let first = *self.buf.get(self.pos).context("block truncated (varint)")?;
        let len = first.trailing_ones() as usize + 1;
        if len == 9 {
            let b = self.take(9)?;
            return Ok(u64::from_le_bytes(b[1..9].try_into().unwrap()));
        }
        let b = self.take(len)?;
        let mut v = 0u64;
        for (i, &byte) in b.iter().enumerate() {
            v |= (byte as u64) << (8 * i);
        }
        Ok(v >> len)
    }

    pub fn cstr(&mut self) -> Result<String> {
        let rest = &self.buf[self.pos..];
        let end = rest.iter().position(|&b| b == 0).context("unterminated string")?;
        let s = String::from_utf8_lossy(&rest[..end]).into_owned();
        self.pos += end + 1;
        Ok(s)
    }

    pub fn u32le(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn skip(&mut self, n: usize) -> Result<()> {
        self.take(n).map(|_| ())
    }
}

#[derive(Debug, Clone)]
pub struct ControlBlock {
    pub kind: u64,
    pub method: String,
    pub pos: u64,
    pub origsize: u64,
    pub compsize: u64,
    pub crc: u32,
}

#[derive(Debug, Clone)]
pub struct DataBlock {
    pub method: String,
    pub pos: u64,
    pub compsize: u64,
    /// Indices into `Archive::entries`, in stream order.
    pub first_entry: usize,
    pub entry_count: usize,
    /// Sum of file sizes = size of the fully decoded block.
    pub origsize: u64,
}

impl DataBlock {
    pub fn entries(&self) -> std::ops::Range<usize> {
        self.first_entry..self.first_entry + self.entry_count
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    /// Relative path with '/' separators, as stored in the archive (untrusted).
    pub path: String,
    pub size: u64,
    pub mtime: u32,
    pub is_dir: bool,
    pub crc: u32,
}

#[derive(Debug)]
pub struct Archive {
    pub path: PathBuf,
    pub len: u64,
    pub blocks: Vec<DataBlock>,
    pub entries: Vec<Entry>,
    /// True when the archive's own block CRCs are standard CRC-32 (so file CRCs can be trusted).
    pub standard_crc: bool,
}

impl Archive {
    pub fn name(&self) -> String {
        self.path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    }

    pub fn open(path: &Path) -> Result<Archive> {
        let mut f = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let len = f.metadata()?.len();

        let footer = find_footer(&mut f, len).with_context(|| format!("{}: no FreeArc footer", path.display()))?;
        let footer_bytes = read_control_block(&mut f, &footer)?;
        let footer_pos = footer.pos;

        let mut c = Cursor::new(&footer_bytes);
        let n = c.varint()?;
        let mut controls = Vec::new();
        for _ in 0..n {
            let kind = c.varint()?;
            let method = c.cstr()?;
            let rel = c.varint()?;
            let origsize = c.varint()?;
            let compsize = c.varint()?;
            let crc = c.u32le()?;
            let pos = footer_pos.checked_sub(rel).context("control block position before start of file")?;
            ensure!(pos + compsize <= len, "control block past end of file");
            controls.push(ControlBlock { kind, method, pos, origsize, compsize, crc });
        }

        let mut archive =
            Archive { path: path.to_path_buf(), len, blocks: Vec::new(), entries: Vec::new(), standard_crc: true };
        let mut saw_dir = false;
        for cb in controls.iter().filter(|c| c.kind == BLOCK_DIR) {
            saw_dir = true;
            let bytes = read_control_block(&mut f, cb)?;
            if crc32fast::hash(&bytes) != cb.crc {
                archive.standard_crc = false;
            }
            parse_directory(&bytes, cb.pos, &mut archive)
                .with_context(|| format!("{}: bad directory block at {}", path.display(), cb.pos))?;
        }
        ensure!(saw_dir, "{}: footer lists no directory block", path.display());
        Ok(archive)
    }

    pub fn total_size(&self) -> u64 {
        self.entries.iter().filter(|e| !e.is_dir).map(|e| e.size).sum()
    }
}

/// Scan the tail of the file backwards for the footer descriptor.
fn find_footer(f: &mut File, len: u64) -> Result<ControlBlock> {
    let tail_len = len.min(1 << 16);
    f.seek(SeekFrom::Start(len - tail_len))?;
    let mut tail = vec![0u8; tail_len as usize];
    f.read_exact(&mut tail)?;
    let base = len - tail_len;

    let mut fallback = None;
    let mut end = tail.len();
    while let Some(p) = find_last(&tail[..end], SIGNATURE) {
        end = p;
        let Ok((desc, desc_end)) = parse_descriptor(&tail[p..]) else { continue };
        if desc.kind != BLOCK_FOOTER || !is_method_name(&desc.method) {
            continue;
        }
        let desc_pos = base + p as u64;
        let Some(pos) = desc_pos.checked_sub(desc.compsize) else { continue };
        let found = ControlBlock { pos, ..desc };
        if desc_pos + desc_end as u64 == len {
            return Ok(found);
        }
        fallback.get_or_insert(found);
    }
    fallback.context("footer descriptor not found in the last 64 KiB")
}

fn find_last(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).rposition(|w| w == needle)
}

fn is_method_name(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| (0x20..0x7f).contains(&b))
}

/// Parses a descriptor starting at the signature. Returns the block info (pos = 0) and its length.
fn parse_descriptor(buf: &[u8]) -> Result<(ControlBlock, usize)> {
    let mut c = Cursor::new(buf);
    ensure!(c.take(4)? == SIGNATURE, "no signature");
    let kind = c.varint()?;
    let method = c.cstr()?;
    let origsize = c.varint()?;
    let compsize = c.varint()?;
    let crc = c.u32le()?;
    let _desc_crc = c.u32le()?;
    Ok((ControlBlock { kind, method, pos: 0, origsize, compsize, crc }, c.pos))
}

fn read_control_block(f: &mut File, cb: &ControlBlock) -> Result<Vec<u8>> {
    f.seek(SeekFrom::Start(cb.pos))?;
    let mut raw = vec![0u8; usize::try_from(cb.compsize)?];
    f.read_exact(&mut raw)?;
    let chain = crate::method::parse_chain(&cb.method);
    match chain.as_slice() {
        [m] if m.name == "storing" => Ok(raw),
        [m] if m.name == "lzma" => {
            let params = crate::lzma::LzmaParams::from_method(m);
            let mut out = Vec::with_capacity(cb.origsize as usize);
            crate::lzma::decode(&params, &mut &raw[..], &mut out, Some(cb.origsize))?;
            ensure!(out.len() as u64 == cb.origsize, "control block decoded to wrong size");
            Ok(out)
        }
        _ => bail!("control block uses unsupported method '{}'", cb.method),
    }
}

fn parse_directory(bytes: &[u8], dir_pos: u64, archive: &mut Archive) -> Result<()> {
    let mut c = Cursor::new(bytes);
    let nblocks = c.varint()? as usize;
    let mut files_per_block = Vec::with_capacity(nblocks);
    for _ in 0..nblocks {
        files_per_block.push(c.varint()? as usize);
    }
    let mut methods = Vec::with_capacity(nblocks);
    for _ in 0..nblocks {
        methods.push(c.cstr()?);
    }
    let mut positions = Vec::with_capacity(nblocks);
    for _ in 0..nblocks {
        let rel = c.varint()?;
        positions.push(dir_pos.checked_sub(rel).context("data block position before start of file")?);
    }
    let mut compsizes = Vec::with_capacity(nblocks);
    for _ in 0..nblocks {
        compsizes.push(c.varint()?);
    }

    let ndirs = c.varint()? as usize;
    let mut dirs = Vec::with_capacity(ndirs);
    for _ in 0..ndirs {
        dirs.push(c.cstr()?);
    }

    let nfiles: usize = files_per_block.iter().sum();
    let mut names = Vec::with_capacity(nfiles);
    for _ in 0..nfiles {
        names.push(c.cstr()?);
    }
    let mut dir_no = Vec::with_capacity(nfiles);
    for _ in 0..nfiles {
        dir_no.push(c.varint()? as usize);
    }
    let mut sizes = Vec::with_capacity(nfiles);
    for _ in 0..nfiles {
        sizes.push(c.varint()?);
    }
    let mut times = Vec::with_capacity(nfiles);
    for _ in 0..nfiles {
        times.push(c.u32le()?);
    }
    let mut isdir = Vec::with_capacity(nfiles);
    for _ in 0..nfiles {
        isdir.push(c.u8()? != 0);
    }
    let mut crcs = Vec::with_capacity(nfiles);
    for _ in 0..nfiles {
        crcs.push(c.u32le()?);
    }
    // Optional tagged fields: (tag, size, payload)* terminated by tag 0. Best effort.
    while c.remaining() > 0 {
        let tag = c.varint()?;
        if tag == 0 {
            break;
        }
        let size = c.varint()? as usize;
        c.skip(size)?;
    }

    let base = archive.entries.len();
    for i in 0..nfiles {
        let dir = dirs.get(dir_no[i]).with_context(|| format!("file {i} has bad directory index"))?;
        let path = if dir.is_empty() { names[i].clone() } else { format!("{dir}/{}", names[i]) };
        archive.entries.push(Entry { path, size: sizes[i], mtime: times[i], is_dir: isdir[i], crc: crcs[i] });
    }

    let mut first = base;
    for b in 0..nblocks {
        let range = first..first + files_per_block[b];
        let origsize = archive.entries[range.clone()].iter().filter(|e| !e.is_dir).map(|e| e.size).sum();
        ensure!(
            positions[b] + compsizes[b] <= dir_pos,
            "data block {b} ({}..+{}) overlaps the directory at {dir_pos}",
            positions[b],
            compsizes[b]
        );
        archive.blocks.push(DataBlock {
            method: methods[b].clone(),
            pos: positions[b],
            compsize: compsizes[b],
            first_entry: first,
            entry_count: files_per_block[b],
            origsize,
        });
        first = range.end;
    }
    Ok(())
}

/// Encoder for FreeArc varints (used by tests and the synthetic-archive writer).
pub fn encode_varint(v: u64) -> Vec<u8> {
    for len in 1..=8u32 {
        if v < (1u64 << (7 * len)) {
            let tag = (1u64 << (len - 1)) - 1; // len-1 trailing ones
            let x = (v << len) | tag;
            return x.to_le_bytes()[..len as usize].to_vec();
        }
    }
    let mut out = vec![0xFF];
    out.extend_from_slice(&v.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(v: u64) -> Vec<u8> {
        crate::format::encode_varint(v)
    }

    #[test]
    fn varint_roundtrip_all_lengths() {
        for v in [0u64, 1, 127, 128, 16383, 16384, (1 << 21) - 1, 1 << 21, 1 << 28, 36_554_469_692, (1 << 56) - 1, 1 << 56, u64::MAX] {
            let b = enc(v);
            let mut c = Cursor::new(&b);
            assert_eq!(c.varint().unwrap(), v, "value {v}");
            assert_eq!(c.remaining(), 0, "value {v} consumed wrong length");
        }
    }

    #[test]
    fn varint_matches_real_bytes() {
        // From a real archive's footer: 83 e5 3c = 498864, 55 06 = 405, b5 08 = 557.
        let mut c = Cursor::new(&[0x83, 0xe5, 0x3c, 0x55, 0x06, 0xb5, 0x08, 0x10]);
        assert_eq!(c.varint().unwrap(), 498864);
        assert_eq!(c.varint().unwrap(), 405);
        assert_eq!(c.varint().unwrap(), 557);
        assert_eq!(c.varint().unwrap(), 8);
    }
}
