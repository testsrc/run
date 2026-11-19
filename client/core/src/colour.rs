//! YUV/RGB conversion with BT.601 or BT.709 in limited or full range, and the colour diagnosis: which
//! common decode (correct, range ignored, wrong matrix, swapped planes) best explains the measured patches.

use crate::code::PATCHES;
use crate::pyfmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Matrix {
    Bt601,
    Bt709,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Range {
    Limited,
    Full,
}

impl Matrix {
    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "bt601" => Some(Self::Bt601),
            "bt709" => Some(Self::Bt709),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Bt601 => "bt601",
            Self::Bt709 => "bt709",
        }
    }
    fn k(self) -> (f64, f64) {
        match self {
            Self::Bt601 => (0.299, 0.114),
            Self::Bt709 => (0.2126, 0.0722),
        }
    }
    fn other(self) -> Self {
        match self {
            Self::Bt601 => Self::Bt709,
            Self::Bt709 => Self::Bt601,
        }
    }
}

impl Range {
    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "limited" => Some(Self::Limited),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Limited => "limited",
            Self::Full => "full",
        }
    }
    fn other(self) -> Self {
        match self {
            Self::Limited => Self::Full,
            Self::Full => Self::Limited,
        }
    }
}

pub fn rgb_to_ycc(rgb: [f64; 3], matrix: Matrix, range: Range) -> [f64; 3] {
    let (kr, kb) = matrix.k();
    let (r, g, b) = (rgb[0] / 255.0, rgb[1] / 255.0, rgb[2] / 255.0);
    let y = kr * r + (1.0 - kr - kb) * g + kb * b;
    let (cb, cr) = ((b - y) / (2.0 * (1.0 - kb)), (r - y) / (2.0 * (1.0 - kr)));
    match range {
        Range::Limited => [16.0 + 219.0 * y, 128.0 + 224.0 * cb, 128.0 + 224.0 * cr],
        Range::Full => [255.0 * y, 128.0 + 255.0 * cb, 128.0 + 255.0 * cr],
    }
}

pub fn ycc_to_rgb(ycc: [f64; 3], matrix: Matrix, range: Range) -> [i64; 3] {
    let (kr, kb) = matrix.k();
    let [y8, cb8, cr8] = ycc;
    let (y, cb, cr) = match range {
        Range::Limited => ((y8 - 16.0) / 219.0, (cb8 - 128.0) / 224.0, (cr8 - 128.0) / 224.0),
        Range::Full => (y8 / 255.0, (cb8 - 128.0) / 255.0, (cr8 - 128.0) / 255.0),
    };
    let r = y + 2.0 * (1.0 - kr) * cr;
    let b = y + 2.0 * (1.0 - kb) * cb;
    let g = (y - kr * r - kb * b) / (1.0 - kr - kb);
    [r, g, b].map(|c| pyfmt::round(c * 255.0).clamp(0, 255))
}

/// What a player shows for each patch under the correct decode and common buggy ones, in a fixed order.
pub fn colour_hypotheses(matrix: Matrix, range: Range) -> Vec<(String, Vec<[i64; 3]>)> {
    let (om, or) = (matrix.other(), range.other());
    let enc: Vec<[f64; 3]> = PATCHES.iter().map(|p| rgb_to_ycc(p.map(f64::from), matrix, range)).collect();
    let all = |f: &dyn Fn(&[f64; 3]) -> [i64; 3]| enc.iter().map(f).collect::<Vec<_>>();
    let correct = all(&|e| ycc_to_rgb(*e, matrix, range));
    let rb = correct.iter().map(|c| [c[2], c[1], c[0]]).collect();
    vec![
        ("correct".into(), correct),
        (format!("range ignored ({} shown as {})", range.name(), or.name()), all(&|e| ycc_to_rgb(*e, matrix, or))),
        (format!("wrong matrix ({})", om.name()), all(&|e| ycc_to_rgb(*e, om, range))),
        ("wrong matrix and range".into(), all(&|e| ycc_to_rgb(*e, om, or))),
        ("Cb/Cr swapped".into(), all(&|e| ycc_to_rgb([e[0], e[2], e[1]], matrix, range))),
        ("R/B swapped".into(), rb),
    ]
}

/// Hypotheses ranked by mean absolute error against the measured patches, best first; errors rounded to 0.1.
pub fn diagnose_colour(measured: &[[i64; 3]], matrix: Matrix, range: Range) -> Vec<(f64, String)> {
    let mut scores: Vec<(f64, String)> = colour_hypotheses(matrix, range)
        .into_iter()
        .map(|(name, exp)| {
            let total: i64 = measured.iter().zip(&exp)
                .flat_map(|(m, e)| (0..3).map(move |i| (m[i] - e[i]).abs()))
                .sum();
            (pyfmt::round_to(total as f64 / (3 * exp.len()) as f64, 1), name)
        })
        .collect();
    // as Python sorts (error, name) tuples: -0.0 and 0.0 are equal, then the name decides
    scores.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.1.cmp(&b.1)));
    scores
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_patches() {
        for m in [Matrix::Bt601, Matrix::Bt709] {
            for r in [Range::Limited, Range::Full] {
                for p in PATCHES {
                    let got = ycc_to_rgb(rgb_to_ycc(p.map(f64::from), m, r), m, r);
                    assert_eq!(got, p.map(i64::from));
                }
            }
        }
    }

    #[test]
    fn each_hypothesis_is_its_own_best_match() {
        let hyp = colour_hypotheses(Matrix::Bt709, Range::Limited);
        for (name, shown) in &hyp {
            if name == "correct" || *shown != hyp[0].1 {
                assert_eq!(diagnose_colour(shown, Matrix::Bt709, Range::Limited)[0], (0.0, name.clone()));
            }
        }
    }
}
