//! The checksum of FitGirl's modified FreeArc unarc.dll (the same build in every FitGirl repack):
//! reflected CRC-32 with init 0xFFFFFFFF and a final NOT, exactly like zlib's CRC-32, but with the
//! reflected polynomial 0x0895171B instead of 0xEDB88320. unarc.dll derives that polynomial at run
//! time from 0xEDB88320 and a key in its .rdata (reverse-engineered: FUN_61090b44). It covers file
//! data, control blocks (header, directory, footer) and descriptors alike.

/// Reflected (LSB-first) polynomial, as used in the table recurrence.
pub const FG_POLY: u32 = 0x0895_171B;

const fn make_tables() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut r = i as u32;
        let mut k = 0;
        while k < 8 {
            r = (r >> 1) ^ (FG_POLY & (r & 1).wrapping_neg());
            k += 1;
        }
        t[0][i] = r;
        i += 1;
    }
    let mut s = 1;
    while s < 8 {
        let mut i = 0;
        while i < 256 {
            let p = t[s - 1][i];
            t[s][i] = (p >> 8) ^ t[0][(p & 0xff) as usize];
            i += 1;
        }
        s += 1;
    }
    t
}

static T: [[u32; 256]; 8] = make_tables();

/// Streaming hasher with the same shape as `crc32fast::Hasher`.
#[derive(Clone)]
pub struct Hasher(u32);

impl Default for Hasher {
    fn default() -> Self {
        Self::new()
    }
}

impl Hasher {
    pub fn new() -> Self {
        Hasher(!0)
    }

    pub fn update(&mut self, data: &[u8]) {
        let mut c = self.0;
        let mut chunks = data.chunks_exact(8);
        for ch in &mut chunks {
            let lo = u32::from_le_bytes([ch[0], ch[1], ch[2], ch[3]]) ^ c;
            let hi = u32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]);
            c = T[7][(lo & 0xff) as usize]
                ^ T[6][((lo >> 8) & 0xff) as usize]
                ^ T[5][((lo >> 16) & 0xff) as usize]
                ^ T[4][(lo >> 24) as usize]
                ^ T[3][(hi & 0xff) as usize]
                ^ T[2][((hi >> 8) & 0xff) as usize]
                ^ T[1][((hi >> 16) & 0xff) as usize]
                ^ T[0][(hi >> 24) as usize];
        }
        for &b in chunks.remainder() {
            c = (c >> 8) ^ T[0][((c ^ b as u32) & 0xff) as usize];
        }
        self.0 = c;
    }

    pub fn finalize(self) -> u32 {
        !self.0
    }
}

pub fn hash(data: &[u8]) -> u32 {
    let mut h = Hasher::new();
    h.update(data);
    h.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bitwise(data: &[u8]) -> u32 {
        let mut c = !0u32;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = (c >> 1) ^ (FG_POLY & (c & 1).wrapping_neg());
            }
        }
        !c
    }

    #[test]
    fn vectors() {
        assert_eq!(hash(b""), 0x0000_0000);
        assert_eq!(hash(b"a"), 0xF6A5_E1A4);
        assert_eq!(hash(b"123456789"), 0xF9CD_CB20);
        assert_eq!(hash(&[0xFF; 32]), 0xF7FA_67B7);
        assert_eq!(hash(&vec![0u8; 1 << 20]), 0xFF34_35BA);
        // FreeArc header block "ArC\x01" 00 00 06 07: stored CRC in every FitGirl archive.
        assert_eq!(hash(&[0x41, 0x72, 0x43, 0x01, 0x00, 0x00, 0x06, 0x07]), 0xF664_DCE3);
        // Its descriptor (everything but the trailing descriptor CRC) -> stored f6d0be5a.
        let desc = [
            0x41, 0x72, 0x43, 0x01, 0x02, b's', b't', b'o', b'r', b'i', b'n', b'g', 0x00, 0x10, 0x10, 0xe3, 0xdc,
            0x64, 0xf6,
        ];
        assert_eq!(hash(&desc), 0xF6D0_BE5A);
        assert_eq!(T[0][128], FG_POLY);
        assert_eq!(T[0][1], 0x029F_6AC5);
    }

    #[test]
    fn slicing_matches_bitwise_and_streaming() {
        let data: Vec<u8> = (0..100_003u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        let want = bitwise(&data);
        assert_eq!(hash(&data), want);
        for split in [1usize, 3, 7, 8, 9, 4096, 65_537] {
            let mut h = Hasher::new();
            for c in data.chunks(split) {
                h.update(c);
            }
            assert_eq!(h.finalize(), want, "split {split}");
        }
    }
}
