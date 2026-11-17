//! Capture post-processing shared by both capture tools: black-box
//! redaction of excluded windows, non-black
//! sampling on a ~48x48 grid, downscale to `max_width` with .NET rounding,
//! PNG encoding, sha256.

use base64::Engine as _;
use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
use image::{ImageEncoder, RgbImage, imageops};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::UiError;
use super::render::{num, ps_int, round_dp};

/// A captured bitmap: top-down rows of BGRA pixels (GDI DIB layout).
#[derive(Debug, Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

impl Frame {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            bgra: vec![0; width as usize * height as usize * 4],
        }
    }

    fn px(&self, x: u32, y: u32) -> (u8, u8, u8) {
        let i = (y as usize * self.width as usize + x as usize) * 4;
        (self.bgra[i + 2], self.bgra[i + 1], self.bgra[i])
    }

    /// `NonBlack`: share of sampled pixels with R+G+B > 30, 3 decimals.
    pub fn non_black_ratio(&self) -> f64 {
        let sx = ps_int(f64::from(self.width) / 48.0).max(1) as usize;
        let sy = ps_int(f64::from(self.height) / 48.0).max(1) as usize;
        let (mut tot, mut nb) = (0u64, 0u64);
        for y in (0..self.height).step_by(sy) {
            for x in (0..self.width).step_by(sx) {
                let (r, g, b) = self.px(x, y);
                tot += 1;
                if u32::from(r) + u32::from(g) + u32::from(b) > 30 {
                    nb += 1;
                }
            }
        }
        round_dp(nb as f64 / tot.max(1) as f64, 3)
    }

    /// `FillRectangle(Brushes.Black, rect)` clipped to the frame.
    pub fn fill_black(&mut self, x: i64, y: i64, w: i64, h: i64) {
        let x0 = x.clamp(0, i64::from(self.width)) as usize;
        let y0 = y.clamp(0, i64::from(self.height)) as usize;
        let x1 = (x + w).clamp(0, i64::from(self.width)) as usize;
        let y1 = (y + h).clamp(0, i64::from(self.height)) as usize;
        let stride = self.width as usize * 4;
        for row in y0..y1 {
            for col in x0..x1 {
                let i = row * stride + col * 4;
                self.bgra[i..i + 3].fill(0);
                self.bgra[i + 3] = 255;
            }
        }
    }

    fn to_rgb(&self) -> RgbImage {
        let mut out = Vec::with_capacity(self.width as usize * self.height as usize * 3);
        for p in self.bgra.as_chunks::<4>().0 {
            out.extend_from_slice(&[p[2], p[1], p[0]]);
        }
        RgbImage::from_raw(self.width, self.height, out).expect("buffer matches dimensions")
    }
}

/// `Rectangle.IntersectsWith` (zero-sized rectangles never intersect).
pub fn intersects(a: (i64, i64, i64, i64), b: (i64, i64, i64, i64)) -> bool {
    b.0 < a.0 + a.2 && a.0 < b.0 + b.2 && b.1 < a.1 + a.3 && a.1 < b.1 + b.3
}

/// The capture result of `Finish`; returns the `capture` object (without
/// `capture_id`) and the PNG as base64.
pub fn finish(
    frame: &Frame,
    max_width: u32,
    method: &str,
    origin_x: i64,
    origin_y: i64,
    non_black: f64,
) -> Result<(Value, String), UiError> {
    let mut scale = 1.0;
    let rgb = frame.to_rgb();
    let out = if frame.width > max_width {
        scale = round_dp(f64::from(max_width) / f64::from(frame.width), 4);
        let w = ps_int(f64::from(frame.width) * scale).max(1) as u32;
        let h = ps_int(f64::from(frame.height) * scale).max(1) as u32;
        imageops::resize(&rgb, w, h, imageops::FilterType::Triangle)
    } else {
        rgb
    };
    let mut png = Vec::new();
    PngEncoder::new_with_quality(&mut png, CompressionType::Fast, PngFilter::Adaptive)
        .write_image(
            out.as_raw(),
            out.width(),
            out.height(),
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|e| UiError::new("capture_backend_failed", format!("PNG encoding: {e}")))?;
    let sha = crate::audit::hex(&Sha256::digest(&png));
    let capture = json!({
        "method": method,
        "source_width": frame.width,
        "source_height": frame.height,
        "width": out.width(),
        "height": out.height(),
        "scale": num(scale),
        "origin_x": origin_x,
        "origin_y": origin_y,
        "bytes": png.len(),
        "sha256": sha,
        "non_black_ratio": num(non_black),
    });
    Ok((
        capture,
        base64::engine::general_purpose::STANDARD.encode(&png),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(w: u32, h: u32) -> Frame {
        let mut f = Frame::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                f.bgra[i] = (x % 256) as u8;
                f.bgra[i + 1] = (y % 256) as u8;
                f.bgra[i + 2] = 200;
                f.bgra[i + 3] = 255;
            }
        }
        f
    }

    #[test]
    fn non_black_and_redaction() {
        let mut f = gradient(480, 240);
        assert_eq!(f.non_black_ratio(), 1.0);
        f.fill_black(-10, -10, 250, 500);
        let r = f.non_black_ratio();
        assert!((0.45..0.55).contains(&r), "{r}");
        assert_eq!(Frame::new(10, 10).non_black_ratio(), 0.0);
    }

    #[test]
    fn finish_downscales_like_dotnet() {
        let f = gradient(2560, 1440);
        let (cap, b64) = finish(&f, 1280, "bitblt", -2560, -1440, 1.0).unwrap();
        assert_eq!(cap["width"], 1280);
        assert_eq!(cap["height"], 720);
        assert_eq!(cap["scale"], json!(0.5));
        assert_eq!(cap["source_width"], 2560);
        assert_eq!(cap["origin_x"], -2560);
        assert_eq!(cap["non_black_ratio"], json!(1));
        let png = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        assert_eq!(cap["bytes"], png.len());
        assert_eq!(&png[1..4], b"PNG");
        let img = image::load_from_memory(&png).unwrap();
        assert_eq!((img.width(), img.height()), (1280, 720));

        let (cap, _) = finish(&gradient(1921, 1081), 1280, "printwindow", 0, 0, 0.5).unwrap();
        assert_eq!(cap["scale"], json!(0.6663));
        assert_eq!(cap["width"], 1280); // [int](1921*0.6663)=1280
        assert_eq!(cap["height"], 720); // [int](1081*0.6663)=720.27 -> 720
        let (cap, _) = finish(&gradient(800, 600), 1280, "printwindow", 5, 6, 0.987).unwrap();
        assert_eq!(cap["scale"], json!(1));
        assert_eq!(cap["width"], 800);
        assert_eq!(cap["non_black_ratio"], json!(0.987));
    }

    #[test]
    fn rectangle_intersection() {
        assert!(intersects((0, 0, 10, 10), (9, 9, 5, 5)));
        assert!(!intersects((0, 0, 10, 10), (10, 0, 5, 5)));
        // .NET Rectangle.IntersectsWith: an empty rect strictly inside counts.
        assert!(intersects((0, 0, 10, 10), (3, 3, 0, 0)));
        assert!(!intersects((0, 0, 10, 10), (0, 0, 0, 0)));
        assert!(intersects((0, 0, 2560, 1440), (-100, -100, 200, 200)));
    }
}
