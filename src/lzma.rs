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

    fn header(&self) -> [u8; 5] {
        let props = ((self.pb * 5 + self.lp) * 9 + self.lc) as u8;
        let d = self.dict.to_le_bytes();
        [props, d[0], d[1], d[2], d[3]]
    }
}

/// Decodes a raw LZMA1 stream. With `unpacked = None` the stream must carry an end marker.
pub fn decode(params: &LzmaParams, input: &mut impl BufRead, output: &mut impl Write, unpacked: Option<u64>) -> Result<()> {
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
