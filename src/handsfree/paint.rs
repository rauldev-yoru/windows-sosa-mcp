//! Pixel painting of the hands-free overlay (portable, unit-tested).
//!
//! Buffers are top-down premultiplied BGRA as `u32` (`A<<24|R<<16|G<<8|B`),
//! the format `UpdateLayeredWindow(ULW_ALPHA)` expects. Edge gradient #E64F8CFF ->
//! #E6B05CFF -> #E62ED6C8 along the edge, an opacity mask fading
//! FF -> 90 (at 25 %) -> 00 away from the screen border; label pill
//! #CC101422 with a 1 px #AA7F9CFF border and white text.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

type Rgba = (f64, f64, f64, f64);

const C1: Rgba = (0x4F as f64, 0x8C as f64, 0xFF as f64, 0xE6 as f64 / 255.0);
const C2: Rgba = (0xB0 as f64, 0x5C as f64, 0xFF as f64, 0xE6 as f64 / 255.0);
const C3: Rgba = (0x2E as f64, 0xD6 as f64, 0xC8 as f64, 0xE6 as f64 / 255.0);

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

fn lerp_c(a: Rgba, b: Rgba, t: f64) -> Rgba {
    (
        lerp(a.0, b.0, t),
        lerp(a.1, b.1, t),
        lerp(a.2, b.2, t),
        lerp(a.3, b.3, t),
    )
}

/// Straight colour + alpha (0..1) -> premultiplied BGRA.
pub fn premul(r: f64, g: f64, b: f64, a: f64) -> u32 {
    let a = a.clamp(0.0, 1.0);
    let ch = |c: f64| ((c * a).round().clamp(0.0, 255.0)) as u32;
    (((a * 255.0).round() as u32) << 24) | (ch(r) << 16) | (ch(g) << 8) | ch(b)
}

fn along(t: f64) -> Rgba {
    if t <= 0.5 {
        lerp_c(C1, C2, t / 0.5)
    } else {
        lerp_c(C2, C3, (t - 0.5) / 0.5)
    }
}

/// Opacity mask: 1.0 at the screen border, 0x90/255 at 25 %, 0 inside.
pub fn fade(d: f64) -> f64 {
    let mid = 0x90 as f64 / 255.0;
    if d <= 0.25 {
        lerp(1.0, mid, d / 0.25)
    } else {
        lerp(mid, 0.0, ((d - 0.25) / 0.75).min(1.0))
    }
}

/// One glow strip (`w` x `h`) for `edge`.
pub fn edge_pixels(w: usize, h: usize, edge: Edge) -> Vec<u32> {
    let mut out = vec![0u32; w * h];
    let n = |v: usize, len: usize| {
        if len <= 1 {
            0.0
        } else {
            v as f64 / (len - 1) as f64
        }
    };
    for y in 0..h {
        for x in 0..w {
            let (t, d) = match edge {
                Edge::Top => (n(x, w), n(y, h)),
                Edge::Bottom => (n(x, w), 1.0 - n(y, h)),
                Edge::Left => (n(y, h), n(x, w)),
                Edge::Right => (n(y, h), 1.0 - n(x, w)),
            };
            let c = along(t);
            out[y * w + x] = premul(c.0, c.1, c.2, c.3 * fade(d));
        }
    }
    out
}

/// Signed distance from (px,py) to a rounded rectangle [0,w]x[0,h].
fn rounded_dist(px: f64, py: f64, w: f64, h: f64, r: f64) -> f64 {
    let (cx, cy) = (w / 2.0, h / 2.0);
    let qx = (px - cx).abs() - (cx - r);
    let qy = (py - cy).abs() - (cy - r);
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
}

/// The label pill: `coverage` is the anti-aliased text mask (0..255) of
/// the same size, drawn white over the pill.
pub fn label_pixels(w: usize, h: usize, radius: f64, border: f64, coverage: &[u8]) -> Vec<u32> {
    let mut out = vec![0u32; w * h];
    for y in 0..h {
        for x in 0..w {
            let d = rounded_dist(x as f64 + 0.5, y as f64 + 0.5, w as f64, h as f64, radius);
            let outer = (0.5 - d).clamp(0.0, 1.0);
            let inner = (0.5 - (d + border)).clamp(0.0, 1.0);
            // Premultiplied components (0..255 scale) of fill + border.
            let fa = 0xCC as f64 / 255.0 * inner;
            let ba = 0xAA as f64 / 255.0 * (outer - inner);
            let mut a = fa + ba;
            let mut r = 0x10 as f64 * fa + 0x7F as f64 * ba;
            let mut g = 0x14 as f64 * fa + 0x9C as f64 * ba;
            let mut b = 0x22 as f64 * fa + 0xFF as f64 * ba;
            let c = coverage.get(y * w + x).copied().unwrap_or(0) as f64 / 255.0 * outer;
            if c > 0.0 {
                r = 255.0 * c + r * (1.0 - c);
                g = 255.0 * c + g * (1.0 - c);
                b = 255.0 * c + b * (1.0 - c);
                a = c + a * (1.0 - c);
            }
            let q = |v: f64| v.round().clamp(0.0, 255.0) as u32;
            out[y * w + x] = (q(a * 255.0) << 24) | (q(r) << 16) | (q(g) << 8) | q(b);
        }
    }
    out
}

/// Pulsing opacity 0.55 <-> 1.0, 1200 ms each way (WPF DoubleAnimation
/// with AutoReverse, linear), as a `SourceConstantAlpha`.
pub fn pulse_alpha(elapsed_ms: u64) -> u8 {
    let p = (elapsed_ms % 2400) as f64;
    let t = if p < 1200.0 {
        p / 1200.0
    } else {
        (2400.0 - p) / 1200.0
    };
    (lerp(0.55, 1.0, t) * 255.0).round() as u8
}

/// Glow thickness in physical pixels for a monitor DPI (`glow_px` is DIPs).
pub fn scaled(px: u32, dpi: u32) -> i32 {
    ((px as f64) * (dpi.max(96) as f64) / 96.0).round() as i32
}

/// The four strips of one monitor: (x, y, w, h, edge).
pub fn strips(x: i32, y: i32, w: i32, h: i32, glow: i32) -> [(i32, i32, i32, i32, Edge); 4] {
    let g = glow.min(w / 2).min(h / 2).max(1);
    [
        (x, y, w, g, Edge::Top),
        (x, y + h - g, w, g, Edge::Bottom),
        (x, y, g, h, Edge::Left),
        (x + w - g, y, g, h, Edge::Right),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha(p: u32) -> u32 {
        p >> 24
    }

    #[test]
    fn edges_fade_away_from_the_border() {
        let top = edge_pixels(10, 44, Edge::Top);
        assert_eq!(alpha(top[0]), 0xE6);
        assert_eq!(alpha(top[43 * 10]), 0);
        let bottom = edge_pixels(10, 44, Edge::Bottom);
        assert_eq!(alpha(bottom[43 * 10]), 0xE6);
        assert_eq!(alpha(bottom[0]), 0);
        let left = edge_pixels(44, 10, Edge::Left);
        assert_eq!(alpha(left[0]), 0xE6);
        assert_eq!(alpha(left[43]), 0);
        let right = edge_pixels(44, 10, Edge::Right);
        assert_eq!(alpha(right[43]), 0xE6);
        // Gradient along the edge: blue start, teal end (premultiplied).
        assert_eq!(
            top[0] & 0xFF,
            (0xFF as f64 * 0xE6 as f64 / 255.0).round() as u32
        );
        let end = top[9];
        assert_eq!(
            (end >> 16) & 0xFF,
            (0x2E as f64 * 0xE6 as f64 / 255.0).round() as u32
        );
        // Premultiplied: no channel exceeds alpha.
        for p in top.iter().chain(&left) {
            let a = alpha(*p);
            assert!((p >> 16 & 0xFF) <= a && (p >> 8 & 0xFF) <= a && (p & 0xFF) <= a);
        }
        assert!((fade(0.25) - 0x90 as f64 / 255.0).abs() < 1e-9);
    }

    #[test]
    fn label_has_rounded_corners_and_white_text() {
        let (w, h) = (100, 34);
        let mut cov = vec![0u8; w * h];
        cov[17 * w + 50] = 255;
        let px = label_pixels(w, h, 16.0, 1.0, &cov);
        assert_eq!(alpha(px[0]), 0, "corner is transparent");
        assert_eq!(px[17 * w + 50], 0xFFFF_FFFF, "text is opaque white");
        let mid = px[17 * w + 20];
        assert_eq!(alpha(mid), 0xCC);
        let top_border = px[50];
        assert!(alpha(top_border) >= 0x80, "{top_border:x}");
    }

    #[test]
    fn pulse_and_geometry() {
        assert_eq!(pulse_alpha(0), 140);
        assert_eq!(pulse_alpha(1200), 255);
        assert_eq!(pulse_alpha(2400), 140);
        assert_eq!(pulse_alpha(600), pulse_alpha(1800));
        assert_eq!(scaled(44, 96), 44);
        assert_eq!(scaled(44, 144), 66);
        let s = strips(-2560, -1440, 2560, 1440, 44);
        assert_eq!(s[1], (-2560, -44, 2560, 44, Edge::Bottom));
        assert_eq!(s[3], (-44, -1440, 44, 1440, Edge::Right));
    }
}
