//! The once-a-second beep: 4x the carrier frequency at twice its level, so a first difference makes it
//! stand out about 60x in energy over the carrier.

use crate::pyfmt;

/// Onset times (seconds, rounded to 0.1 ms, offset by `start`) of the beep in mono samples.
pub fn find_beeps(samples: &[i16], rate: u32, start: f64) -> Vec<f64> {
    find_beeps_with(samples, rate, start, 1.0, 0.3)
}

pub fn find_beeps_with(samples: &[i16], rate: u32, start: f64, block_ms: f64, gap_s: f64) -> Vec<f64> {
    let n = ((rate as f64 * block_ms / 1000.0) as usize).max(1);
    let mut energies: Vec<i64> = Vec::with_capacity(samples.len() / n + 1);
    let mut prev = samples.first().copied().unwrap_or(0) as i64;
    let mut i = 0;
    while i + n <= samples.len() {
        let mut e = 0i64;
        for &s in &samples[i..i + n] {
            let d = s as i64 - prev;
            e += d * d;
            prev = s as i64;
        }
        energies.push(e);
        i += n;
    }
    let max = energies.iter().copied().max().unwrap_or(0);
    if max == 0 {
        return vec![];
    }
    let thresh = max as f64 / 8.0;
    let (mut onsets, mut last) = (vec![], -1e9f64);
    for (k, &e) in energies.iter().enumerate() {
        let t = (k * n) as f64 / rate as f64;
        let loud = e as f64 > thresh;
        if loud && t - last > gap_s {
            onsets.push(pyfmt::round_to(start + t, 4));
        }
        if loud {
            last = t;
        }
    }
    onsets
}
