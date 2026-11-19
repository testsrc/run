//! Reads the frame code from a picture: samples 9 points in each code cell, thresholds luma between the
//! darkest and brightest cells, and tries the 8 orientations; only the right one passes sync and CRC.
//! If none does, it crops black borders and tries again.

use crate::code::{bit_cell, decode_bits, Orientation, COLS, NBITS, PATCH_ROW, ROWS};
use crate::frame::Pixels;
use crate::pyfmt;

/// x, y, width, height of the picture inside the capture.
pub type Box4 = [i64; 4];

#[derive(Clone, Debug, PartialEq)]
pub struct Read {
    pub code: Option<u32>,
    pub orient: Option<Orientation>,
    pub bbox: Option<Box4>,
    /// Mean RGB of each patch in row 15, when asked for.
    pub rgb: Option<Vec<[i64; 3]>>,
}

impl Read {
    fn none() -> Self {
        Read { code: None, orient: None, bbox: None, rgb: None }
    }

    /// The event fields, as the reference implementation writes them: {"code": null} when nothing was read.
    pub fn to_json(&self) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        m.insert("code".into(), self.code.map_or(serde_json::Value::Null, |c| c.into()));
        if let (Some(o), Some(b)) = (self.orient, self.bbox) {
            m.insert("orient".into(), o.name().into());
            m.insert("box".into(), b.to_vec().into());
        }
        if let Some(rgb) = &self.rgb {
            m.insert("rgb".into(), rgb.iter().map(|c| c.to_vec()).collect::<Vec<_>>().into());
        }
        m.into()
    }
}

/// Mean RGB of 3x3 points around a cell centre, mapped through an orientation.
fn sample(fr: &dyn Pixels, bx: Box4, o: Orientation, col: usize, row: usize) -> [f64; 3] {
    const SPREAD: f64 = 0.25;
    let [x0, y0, bw, bh] = bx;
    let (w, h) = (fr.width() as i64, fr.height() as i64);
    let mut s = [0i64; 3];
    for dv in [-SPREAD, 0.0, SPREAD] {
        for du in [-SPREAD, 0.0, SPREAD] {
            let (u, v) = ((col as f64 + 0.5 + du) / COLS as f64, (row as f64 + 0.5 + dv) / ROWS as f64);
            let (x, y) = o.map(u, v);
            // Python's int() truncates toward zero, like `as`
            let px = ((x0 as f64 + x * bw as f64) as i64).max(0).min(w - 1);
            let py = ((y0 as f64 + y * bh as f64) as i64).max(0).min(h - 1);
            let c = fr.rgb(px as usize, py as usize);
            for i in 0..3 {
                s[i] += c[i];
            }
        }
    }
    s.map(|v| v as f64 / 9.0)
}

fn luma(c: [f64; 3]) -> f64 {
    0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2]
}

fn try_read(fr: &dyn Pixels, bx: Box4, o: Orientation) -> Option<u32> {
    let lum: Vec<f64> = (0..NBITS).map(|i| {
        let (col, row) = bit_cell(i);
        luma(sample(fr, bx, o, col, row))
    }).collect();
    let lo = lum.iter().cloned().fold(f64::INFINITY, f64::min);
    let hi = lum.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    if hi - lo < 40.0 {
        return None;
    }
    let t = (lo + hi) / 2.0;
    let bits: Vec<u8> = lum.iter().map(|&v| (v > t) as u8).collect();
    decode_bits(&bits)
}

/// Bounding box of the non-black area, for letterboxed or pillarboxed captures.
pub fn find_picture(fr: &dyn Pixels) -> Box4 {
    const THRESH: i64 = 24;
    let (w, h) = (fr.width(), fr.height());
    let dark_row = |y: usize| (0..w).step_by(4).all(|x| *fr.rgb(x, y).iter().max().unwrap() < THRESH);
    let dark_col = |x: usize| (0..h).step_by(4).all(|y| *fr.rgb(x, y).iter().max().unwrap() < THRESH);
    let (mut top, mut bottom, mut left, mut right) = (0usize, h - 1, 0usize, w - 1);
    while top < bottom && dark_row(top) {
        top += 1;
    }
    while bottom > top && dark_row(bottom) {
        bottom -= 1;
    }
    while left < right && dark_col(left) {
        left += 1;
    }
    while right > left && dark_col(right) {
        right -= 1;
    }
    [left as i64, top as i64, (right - left + 1) as i64, (bottom - top + 1) as i64]
}

/// Reads frame codes; remembers the orientation and crop that worked last, and tries them first.
#[derive(Clone, Debug)]
pub struct Reader {
    bbox: Option<Box4>,
    orient: Orientation,
}

impl Default for Reader {
    fn default() -> Self {
        Self::new()
    }
}

impl Reader {
    pub fn new() -> Self {
        Reader { bbox: None, orient: Orientation::None }
    }

    pub fn read(&mut self, fr: &dyn Pixels, patches: bool) -> Read {
        let full = [0, 0, fr.width() as i64, fr.height() as i64];
        let mut bx = self.bbox.unwrap_or(full);
        let names: Vec<Orientation> = std::iter::once(self.orient)
            .chain(Orientation::ALL.into_iter().filter(|&o| o != self.orient))
            .collect();
        for attempt in 0..2 {
            for &o in &names {
                if let Some(code) = try_read(fr, bx, o) {
                    self.bbox = Some(bx);
                    self.orient = o;
                    let rgb = patches.then(|| {
                        (0..COLS).map(|col| sample(fr, bx, o, col, PATCH_ROW).map(pyfmt::round)).collect()
                    });
                    return Read { code: Some(code), orient: Some(o), bbox: Some(bx), rgb };
                }
            }
            if attempt == 0 {
                let crop = find_picture(fr);
                if crop == bx {
                    break;
                }
                bx = crop;
            }
        }
        Read::none()
    }
}
