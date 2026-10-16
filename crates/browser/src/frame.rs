//! Rendered page frames.

use anyhow::{Context as _, Result};
use base64::Engine as _;

/// One painted frame: tightly packed BGRA pixels, as Chromium's off-screen
/// renderer produces them (and as GPUI's image atlas expects them).
#[derive(Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Frame({}x{})", self.width, self.height)
    }
}

impl Frame {
    pub fn new(width: u32, height: u32, bgra: Vec<u8>) -> Option<Self> {
        (bgra.len() == width as usize * height as usize * 4).then_some(Self { width, height, bgra })
    }

    /// Encodes the frame as PNG, scaled down so neither side exceeds
    /// `max_side` pixels (screenshots for the agent do not need Retina
    /// resolution, and every pixel costs tokens).
    pub fn to_png(&self, max_side: u32) -> Result<Vec<u8>> {
        let mut rgba = self.bgra.clone();
        for pixel in rgba.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        let image = image::RgbaImage::from_raw(self.width, self.height, rgba)
            .context("frame buffer has the wrong size")?;
        let longest = self.width.max(self.height);
        let image = if max_side > 0 && longest > max_side {
            let scale = max_side as f64 / longest as f64;
            image::imageops::resize(
                &image,
                ((self.width as f64 * scale).round() as u32).max(1),
                ((self.height as f64 * scale).round() as u32).max(1),
                image::imageops::FilterType::Triangle,
            )
        } else {
            image
        };
        let mut png = Vec::new();
        image
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .context("encoding PNG")?;
        Ok(png)
    }

    /// The frame as a `data:image/png;base64,…` URL.
    pub fn to_png_data_url(&self, max_side: u32) -> Result<String> {
        let png = self.to_png(max_side)?;
        Ok(format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(png)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_mismatched_buffers() {
        assert!(Frame::new(2, 2, vec![0; 15]).is_none());
        assert!(Frame::new(2, 2, vec![0; 16]).is_some());
    }

    #[test]
    fn png_swaps_channels_and_downscales() {
        // One blue pixel in BGRA is (255, 0, 0, 255).
        let frame = Frame::new(400, 200, [255u8, 0, 0, 255].repeat(400 * 200)).unwrap();
        let png = frame.to_png(100).unwrap();
        let decoded = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(decoded.dimensions(), (100, 50));
        assert_eq!(decoded.get_pixel(10, 10).0, [0, 0, 255, 255]);
        assert!(frame.to_png_data_url(0).unwrap().starts_with("data:image/png;base64,"));
    }
}
