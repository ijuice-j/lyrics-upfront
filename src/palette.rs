//! Card theme from album art -- a port of Spotify Lyrics Overlay's app/palette.py.
//!
//! The cover only supplies a hue: the plate is forced near-black and the accent
//! bright, so no cover can make the lyrics hard to read. Swatches are scored by
//! population x saturation^3 (Android Palette style). Greyscale covers fall back
//! to the neutral theme.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    pub plate: [f32; 3],  // 0..1 sRGB
    pub accent: [f32; 3], // 0..1 sRGB
}

const QUANT_LEVELS: u32 = 12;
const SATURATION_EXPONENT: f64 = 3.0;
const MIN_POPULATION: f64 = 0.001;
const MIN_USABLE_LIGHTNESS: f64 = 0.12;
const MAX_USABLE_LIGHTNESS: f64 = 0.90;
const MIN_USABLE_SATURATION: f64 = 0.15;
const GREYSCALE_SATURATION: f64 = 0.10;
const PLATE_LIGHTNESS: f64 = 0.058;
const PLATE_MAX_SATURATION: f64 = 0.42;
const ACCENT_MIN_LIGHTNESS: f64 = 0.58;
const ACCENT_MAX_LIGHTNESS: f64 = 0.76;
const ACCENT_MIN_SATURATION: f64 = 0.45;

pub const NEUTRAL: Theme = Theme {
    plate: [12.0 / 255.0, 13.0 / 255.0, 18.0 / 255.0],
    accent: [150.0 / 255.0, 158.0 / 255.0, 178.0 / 255.0],
};

/// `pixels` is RGB triples (any order of rows), already flattened onto black.
pub fn theme_from_rgb(pixels: &[[u8; 3]]) -> Theme {
    if pixels.is_empty() {
        return NEUTRAL;
    }
    let step = 256 / QUANT_LEVELS;
    // Insertion-ordered buckets so ties break the same way as Python's dict.
    let mut index = std::collections::HashMap::new();
    let mut buckets: Vec<[u64; 4]> = Vec::new();
    for &[r, g, b] in pixels {
        let key = (r as u32 / step, g as u32 / step, b as u32 / step);
        let i = *index.entry(key).or_insert_with(|| {
            buckets.push([0; 4]);
            buckets.len() - 1
        });
        let e = &mut buckets[i];
        e[0] += 1;
        e[1] += r as u64;
        e[2] += g as u64;
        e[3] += b as u64;
    }
    let total = pixels.len() as f64;
    let candidates: Vec<((f64, f64, f64), f64)> = buckets
        .iter()
        .map(|&[n, rs, gs, bs]| {
            let n_f = n as f64;
            let hls = rgb_to_hls(rs as f64 / n_f / 255.0, gs as f64 / n_f / 255.0, bs as f64 / n_f / 255.0);
            (hls, n_f / total)
        })
        .collect();

    let peak = candidates.iter().map(|(h, _)| h.2).fold(0.0, f64::max);
    if peak < GREYSCALE_SATURATION {
        return NEUTRAL;
    }
    let best = candidates
        .iter()
        .filter(|((_, l, s), pop)| {
            (MIN_USABLE_LIGHTNESS..=MAX_USABLE_LIGHTNESS).contains(l) && *s >= MIN_USABLE_SATURATION && *pop >= MIN_POPULATION
        })
        .fold(None::<&((f64, f64, f64), f64)>, |best, c| {
            let score = c.1 * c.0 .2.powf(SATURATION_EXPONENT);
            match best {
                Some(b) if b.1 * b.0 .2.powf(SATURATION_EXPONENT) >= score => Some(b),
                _ => Some(c),
            }
        });
    let Some(&((h, l, s), _)) = best else { return NEUTRAL };

    let accent = hls_to_rgb(h, l.clamp(ACCENT_MIN_LIGHTNESS, ACCENT_MAX_LIGHTNESS), s.max(ACCENT_MIN_SATURATION));
    let plate = hls_to_rgb(h, PLATE_LIGHTNESS, s.min(PLATE_MAX_SATURATION));
    // Round to 8-bit like the Python version, so both apps produce the same colours.
    let q = |c: (f64, f64, f64)| [c.0, c.1, c.2].map(|v| (v * 255.0).round() as f32 / 255.0);
    Theme { plate: q(plate), accent: q(accent) }
}

fn rgb_to_hls(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let maxc = r.max(g).max(b);
    let minc = r.min(g).min(b);
    let sumc = maxc + minc;
    let rangec = maxc - minc;
    let l = sumc / 2.0;
    if minc == maxc {
        return (0.0, l, 0.0);
    }
    let s = if l <= 0.5 { rangec / sumc } else { rangec / (2.0 - maxc - minc) };
    let rc = (maxc - r) / rangec;
    let gc = (maxc - g) / rangec;
    let bc = (maxc - b) / rangec;
    let h = if r == maxc {
        bc - gc
    } else if g == maxc {
        2.0 + rc - bc
    } else {
        4.0 + gc - rc
    };
    ((h / 6.0).rem_euclid(1.0), l, s)
}

fn hls_to_rgb(h: f64, l: f64, s: f64) -> (f64, f64, f64) {
    if s == 0.0 {
        return (l, l, l);
    }
    let m2 = if l <= 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let m1 = 2.0 * l - m2;
    (v(m1, m2, h + 1.0 / 3.0), v(m1, m2, h), v(m1, m2, h - 1.0 / 3.0))
}

fn v(m1: f64, m2: f64, hue: f64) -> f64 {
    let hue = hue.rem_euclid(1.0);
    if hue < 1.0 / 6.0 {
        m1 + (m2 - m1) * hue * 6.0
    } else if hue < 0.5 {
        m2
    } else if hue < 2.0 / 3.0 {
        m1 + (m2 - m1) * (2.0 / 3.0 - hue) * 6.0
    } else {
        m1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn red_cover_matches_python() {
        // app/palette.py on a solid rgb(190,40,60) cover gives plate #1a0709-ish
        // and accent from hue 352deg; check hue family and darkness rules.
        let t = theme_from_rgb(&vec![[190, 40, 60]; 64 * 64]);
        assert!(t.plate.iter().all(|&c| c < 0.15));
        assert!(t.accent[0] > t.accent[1] && t.accent[0] > t.accent[2]);
    }

    #[test]
    fn grey_cover_is_neutral() {
        assert_eq!(theme_from_rgb(&vec![[120, 120, 120]; 100]), NEUTRAL);
    }
}
