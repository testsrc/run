//! Frame code layout v1. The bottom three rows of an 18-row grid (22 columns, full frame width) carry:
//! row 15 colour and range patches, rows 16-17 a 44-bit code: sync(8) frame(24) version(4) crc8(8).

pub const VERSION: u32 = 1;
pub const COLS: usize = 22;
pub const ROWS: usize = 18;
pub const PATCH_ROW: usize = 15;
pub const CODE_ROWS: [usize; 2] = [16, 17];
pub const SYNC: u64 = 0xB2;
pub const NBITS: usize = 44;

/// Row 15, one RGB value per column: 75% bars (2 columns each), range greys, 100% primaries.
pub const PATCHES: [[u8; 3]; COLS] = [
    [191, 191, 191], [191, 191, 191], [191, 191, 0], [191, 191, 0], [0, 191, 191], [0, 191, 191],
    [0, 191, 0], [0, 191, 0], [191, 0, 191], [191, 0, 191], [191, 0, 0], [191, 0, 0], [0, 0, 191], [0, 0, 191],
    [0, 0, 0], [16, 16, 16], [128, 128, 128], [239, 239, 239], [255, 255, 255],
    [255, 0, 0], [0, 255, 0], [0, 0, 255],
];

pub fn crc8(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &b in data {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 { (crc << 1) ^ 0x07 } else { crc << 1 };
        }
    }
    crc
}

fn crc_of(frame: u64, version: u64) -> u8 {
    crc8(&[(frame >> 16) as u8, (frame >> 8) as u8, frame as u8, (version << 4) as u8])
}

/// The 44 bits, most significant first, for a frame number (taken modulo 2^24).
pub fn encode(frame: u64) -> [u8; NBITS] {
    let f = frame & 0xFF_FFFF;
    let word = (SYNC << 36) | (f << 12) | ((VERSION as u64) << 8) | crc_of(f, VERSION as u64) as u64;
    std::array::from_fn(|i| ((word >> (NBITS - 1 - i)) & 1) as u8)
}

pub fn decode_bits(bits: &[u8]) -> Option<u32> {
    let word = bits.iter().fold(0u64, |w, &b| (w << 1) | b as u64);
    let (sync, f, ver, crc) = (word >> 36, (word >> 12) & 0xFF_FFFF, (word >> 8) & 15, (word & 255) as u8);
    if sync != SYNC || ver != VERSION as u64 || crc_of(f, ver) != crc {
        return None;
    }
    Some(f as u32)
}

/// (column, row) of code bit `i`.
pub fn bit_cell(i: usize) -> (usize, usize) {
    (i % COLS, CODE_ROWS[i / COLS])
}

/// How the player showed the picture. Order and names as the reference implementation tries them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Orientation {
    None,
    Rotated90Cw,
    Rotated180,
    Rotated90Ccw,
    Mirrored,
    UpsideDown,
    Transposed,
    AntiTransposed,
}

impl Orientation {
    pub const ALL: [Orientation; 8] = [
        Self::None, Self::Rotated90Cw, Self::Rotated180, Self::Rotated90Ccw,
        Self::Mirrored, Self::UpsideDown, Self::Transposed, Self::AntiTransposed,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Rotated90Cw => "rotated 90 cw",
            Self::Rotated180 => "rotated 180",
            Self::Rotated90Ccw => "rotated 90 ccw",
            Self::Mirrored => "mirrored",
            Self::UpsideDown => "upside down (vflip)",
            Self::Transposed => "transposed",
            Self::AntiTransposed => "anti-transposed",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|o| o.name() == s)
    }

    /// Where a cell at normalised (u, v) of the picture lands in the displayed frame.
    pub fn map(self, u: f64, v: f64) -> (f64, f64) {
        match self {
            Self::None => (u, v),
            Self::Rotated90Cw => (1.0 - v, u),
            Self::Rotated180 => (1.0 - u, 1.0 - v),
            Self::Rotated90Ccw => (v, 1.0 - u),
            Self::Mirrored => (1.0 - u, v),
            Self::UpsideDown => (u, 1.0 - v),
            Self::Transposed => (v, u),
            Self::AntiTransposed => (1.0 - v, 1.0 - u),
        }
    }

    /// Width and height trade places.
    pub fn turned(self) -> bool {
        matches!(self, Self::Rotated90Cw | Self::Rotated90Ccw | Self::Transposed | Self::AntiTransposed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_check_value() {
        assert_eq!(crc8(b"123456789"), 0xF4);
        assert_eq!(crc8(b""), 0);
    }

    #[test]
    fn roundtrip_and_single_bit_errors() {
        for n in [0u64, 1, 179, 65535, (1 << 24) - 1] {
            let bits = encode(n);
            assert_eq!(decode_bits(&bits), Some(n as u32));
            for i in 0..NBITS {
                let mut b = bits;
                b[i] ^= 1;
                assert_eq!(decode_bits(&b), None, "frame {n}, bit {i}");
            }
        }
        assert_eq!(encode(1 << 24), encode(0));
    }

    #[test]
    fn names_roundtrip() {
        for o in Orientation::ALL {
            assert_eq!(Orientation::from_name(o.name()), Some(o));
        }
    }
}
