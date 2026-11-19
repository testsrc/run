//! Pictures the reader can sample: packed RGB (any channel order, bytes per pixel and stride), and planar
//! 4:2:0 YUV shown through a given matrix and range. Only the sampled pixels are converted.

use crate::colour::{ycc_to_rgb, Matrix, Range};

pub trait Pixels {
    fn width(&self) -> usize;
    fn height(&self) -> usize;
    /// RGB of the pixel at (x, y), 0..=255 per channel.
    fn rgb(&self, x: usize, y: usize) -> [i64; 3];
}

pub struct RgbFrame<'a> {
    data: &'a [u8],
    w: usize,
    h: usize,
    bpp: usize,
    stride: usize,
    idx: [usize; 3],
}

impl<'a> RgbFrame<'a> {
    /// Packed RGB, 3 bytes per pixel, rows back to back.
    pub fn new(data: &'a [u8], width: usize, height: usize) -> Self {
        Self::with_layout(data, width, height, 3, width * 3, "rgb")
    }

    /// `order` names the channel in each byte position, for example "bgr" or "rgbx" (with bpp 4).
    pub fn with_layout(data: &'a [u8], width: usize, height: usize, bpp: usize, stride: usize, order: &str) -> Self {
        let pos = |c| order.find(c).expect("channel order needs r, g and b");
        Self { data, w: width, h: height, bpp, stride, idx: [pos('r'), pos('g'), pos('b')] }
    }
}

impl Pixels for RgbFrame<'_> {
    fn width(&self) -> usize {
        self.w
    }
    fn height(&self) -> usize {
        self.h
    }
    fn rgb(&self, x: usize, y: usize) -> [i64; 3] {
        let o = y * self.stride + x * self.bpp;
        self.idx.map(|i| self.data[o + i] as i64)
    }
}

/// I420: the Y plane, then Cb and Cr at half width and height (rounded up), packed one after the other.
pub struct YuvFrame<'a> {
    data: &'a [u8],
    w: usize,
    h: usize,
    matrix: Matrix,
    range: Range,
    cw: usize,
    uo: usize,
    vo: usize,
}

impl<'a> YuvFrame<'a> {
    pub fn new(data: &'a [u8], width: usize, height: usize, matrix: Matrix, range: Range) -> Self {
        let cw = width.div_ceil(2);
        let uo = width * height;
        let vo = uo + cw * height.div_ceil(2);
        Self { data, w: width, h: height, matrix, range, cw, uo, vo }
    }

    pub fn size(width: usize, height: usize) -> usize {
        width * height + 2 * width.div_ceil(2) * height.div_ceil(2)
    }
}

impl Pixels for YuvFrame<'_> {
    fn width(&self) -> usize {
        self.w
    }
    fn height(&self) -> usize {
        self.h
    }
    fn rgb(&self, x: usize, y: usize) -> [i64; 3] {
        let c = (y / 2) * self.cw + x / 2;
        let d = self.data;
        ycc_to_rgb([d[y * self.w + x] as f64, d[self.uo + c] as f64, d[self.vo + c] as f64], self.matrix, self.range)
    }
}
