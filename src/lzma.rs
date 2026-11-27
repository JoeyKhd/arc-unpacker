//! Built-in LZMA decoding for FreeArc's "lzma:..." method (raw LZMA1 stream, no header).

use crate::method::{Method, parse_size};
use anyhow::{Context, Result};
use std::io::{BufRead, Read, Write};

#[derive(Debug, Clone, Copy)]
pub struct LzmaParams {
    pub dict: u32,
    pub lc: u32,
    pub lp: u32,
    pub pb: u32,
}

impl LzmaParams {
    /// Reads dictionary size ("d1m" or a bare "64m") and lc/lp/pb from the method params.
    /// Encoder-only options (mf, fb, mc, a, ...) are ignored.
    pub fn from_method(m: &Method) -> Self {
        let mut p = LzmaParams { dict: 64 << 20, lc: 3, lp: 0, pb: 2 };
        for param in &m.params {
            let lower = param.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("lc").and_then(|v| v.parse().ok()) {
                p.lc = v;
            } else if let Some(v) = lower.strip_prefix("lp").and_then(|v| v.parse().ok()) {
                p.lp = v;
            } else if let Some(v) = lower.strip_prefix("pb").and_then(|v| v.parse().ok()) {
                p.pb = v;
            } else if let Some(v) = lower.strip_prefix('d').and_then(parse_size).or_else(|| parse_size(&lower)) {
                p.dict = v.clamp(4096, u32::MAX as u64) as u32;
            }
        }
        p
    }

    #[cfg(test)]
    fn header(&self) -> [u8; 5] {
        let props = ((self.pb * 5 + self.lp) * 9 + self.lc) as u8;
        let d = self.dict.to_le_bytes();
        [props, d[0], d[1], d[2], d[3]]
    }
}

/// Decodes a raw LZMA1 stream. With `unpacked = None` the stream must carry an end marker.
/// (lzma-rust2: FS17 setup-fitgirl-03, 809 MB at lc8:lp3:pb3, in 10 s instead of lzma-rs's 15 s.)
pub fn decode(params: &LzmaParams, input: &mut impl BufRead, output: &mut impl Write, unpacked: Option<u64>) -> Result<()> {
    let mut r = lzma_rust2::LzmaReader::new(input, unpacked.unwrap_or(u64::MAX), params.lc, params.lp, params.pb, params.dict, None)
        .map_err(|e| anyhow::anyhow!("lzma: {e}"))?;
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context("lzma decode failed"),
        };
        output.write_all(&buf[..n])?;
        total += n as u64;
    }
    if let Some(want) = unpacked {
        anyhow::ensure!(total == want, "lzma decode failed: {total} of {want} bytes");
    }
    Ok(())
}

/// The previous decoder (lzma-rs), kept to check the new one against.
#[cfg(test)]
fn decode_reference(params: &LzmaParams, input: &mut impl BufRead, output: &mut impl Write, unpacked: Option<u64>) -> Result<()> {
    let header = params.header();
    let mut stream = std::io::BufReader::with_capacity(1 << 20, (&header[..]).chain(input));
    let opts = lzma_rs::decompress::Options {
        unpacked_size: lzma_rs::decompress::UnpackedSize::UseProvided(unpacked),
        memlimit: None,
        allow_incomplete: false,
    };
    lzma_rs::lzma_decompress_with_options(&mut stream, output, &opts)
        .map_err(|e| anyhow::anyhow!("{e:?}"))
        .context("lzma decode failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_previous_decoder() {
        // Mixed data, compressed by lzma-rs (lc3 lp0 pb2, 13-byte .lzma header in front).
        let mut data = Vec::new();
        let mut x = 7u64;
        while data.len() < 3 << 20 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            match x >> 62 {
                0 => data.extend(std::iter::repeat_n((x >> 8) as u8, (x >> 20) as usize % 5000)),
                1 => data.extend(b"lzma raw stream test ".iter().cycle().take((x >> 24) as usize % 3000)),
                _ => data.extend((0..(x >> 30) as usize % 2000).map(|i| (x >> (i % 50)) as u8)),
            }
        }
        let mut packed = Vec::new();
        lzma_rs::lzma_compress(&mut &data[..], &mut packed).unwrap();
        let raw = &packed[13..];
        let params = LzmaParams { dict: u32::from_le_bytes(packed[1..5].try_into().unwrap()), lc: 3, lp: 0, pb: 2 };
        let (mut ours, mut theirs) = (Vec::new(), Vec::new());
        decode(&params, &mut &raw[..], &mut ours, Some(data.len() as u64)).unwrap();
        decode_reference(&params, &mut &raw[..], &mut theirs, Some(data.len() as u64)).unwrap();
        assert!(ours == data && theirs == data);
        // lzma-rs writes an end marker: an unknown size works too, and a wrong one fails.
        let mut ours = Vec::new();
        decode(&params, &mut &raw[..], &mut ours, None).unwrap();
        assert!(ours == data);
        assert!(decode(&params, &mut &raw[..], &mut Vec::new(), Some(data.len() as u64 + 1)).is_err());
    }
}
