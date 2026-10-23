use image::{DynamicImage, RgbaImage};

pub type Palette = [[u8; 3]; 3];

pub const NEUTRAL_PALETTE: Palette = [[180; 3], [210; 3], [240; 3]];

#[derive(Clone, Copy, Default)]
struct Bucket {
    r: f64,
    g: f64,
    b: f64,
    weight: f64,
}

pub(crate) fn palette_for(image: &DynamicImage) -> Palette {
    palette_from_rgba(image)
}

#[cfg(test)]
pub(crate) fn palette_from_png(data: &[u8]) -> Palette {
    let Ok(image) = image::load_from_memory_with_format(data, image::ImageFormat::Png) else {
        return NEUTRAL_PALETTE;
    };
    palette_for(&image)
}

fn palette_from_rgba(image: &DynamicImage) -> Palette {
    let image: RgbaImage = image.to_rgba8();
    let width = image.width();
    let height = image.height();
    if width == 0 || height == 0 {
        return NEUTRAL_PALETTE;
    }

    let mut bins = [Bucket::default(); 4096];
    let step_x = (width / 96).max(1) as usize;
    let step_y = (height / 96).max(1) as usize;
    for y in (0..height as usize).step_by(step_y) {
        for x in (0..width as usize).step_by(step_x) {
            let pixel = image.get_pixel(x as u32, y as u32).0;
            if pixel[3] < 192 {
                continue;
            }

            let r = f64::from(pixel[0]) / 255.0;
            let g = f64::from(pixel[1]) / 255.0;
            let b = f64::from(pixel[2]) / 255.0;
            let value = r.max(g).max(b);
            let low = r.min(g).min(b);
            if value < 0.18 {
                continue;
            }

            let saturation = (value - low) / value;
            if saturation < 0.22 {
                continue;
            }

            let weight = saturation * saturation * value * value;
            let bucket = &mut bins[(usize::from(pixel[0] >> 4) << 8)
                | (usize::from(pixel[1] >> 4) << 4)
                | usize::from(pixel[2] >> 4)];
            bucket.r += r * weight;
            bucket.g += g * weight;
            bucket.b += b * weight;
            bucket.weight += weight;
        }
    }

    let mut result = [[0; 3]; 3];
    for index in 0..result.len() {
        let mut best = None;
        let mut best_score = 0.0;
        for (bucket_index, bucket) in bins.iter().enumerate() {
            if bucket.weight == 0.0 {
                continue;
            }

            let r = bucket.r / bucket.weight;
            let g = bucket.g / bucket.weight;
            let b = bucket.b / bucket.weight;
            let value = r.max(g).max(b);
            let mut distance = 1.0_f64;
            for previous in &result[..index] {
                let previous_value = previous[0].max(previous[1]).max(previous[2]);
                let hue_distance = (r / value - f64::from(previous[0]) / f64::from(previous_value))
                    .powi(2)
                    + (g / value - f64::from(previous[1]) / f64::from(previous_value)).powi(2)
                    + (b / value - f64::from(previous[2]) / f64::from(previous_value)).powi(2);
                distance = distance.min(hue_distance);
            }
            let score = bucket.weight.sqrt() * distance;
            if score > best_score {
                best = Some(bucket_index);
                best_score = score;
            }
        }

        let Some(best) = best else {
            if index == 0 {
                return NEUTRAL_PALETTE;
            }
            result[index] = result[index - 1];
            continue;
        };

        let bucket = bins[best];
        let r = bucket.r / bucket.weight;
        let g = bucket.g / bucket.weight;
        let b = bucket.b / bucket.weight;
        let scale = 1.0_f64.max(0.88 / r.max(g).max(b));
        result[index] = [
            (r * scale * 255.0).round() as u8,
            (g * scale * 255.0).round() as u8,
            (b * scale * 255.0).round() as u8,
        ];
    }
    result
}

/// Interpolates the three extracted accents from `t = 0.0` through `t = 1.0`.
pub fn palette_at(palette: &Palette, t: f64) -> [u8; 3] {
    let t = if t.is_nan() {
        0.0
    } else {
        t.clamp(0.0, 1.0) * 2.0
    };
    let index = (t as usize).min(1);
    let fraction = t - index as f64;
    let mut output = [0; 3];
    for channel in 0..3 {
        output[channel] = (f64::from(palette[index][channel]) * (1.0 - fraction)
            + f64::from(palette[index + 1][channel]) * fraction)
            .round()
            .clamp(0.0, 255.0) as u8;
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgba, RgbaImage};
    use std::io::Cursor;

    fn png(image: RgbaImage) -> Vec<u8> {
        let mut output = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image)
            .write_to(&mut output, ImageFormat::Png)
            .unwrap();
        output.into_inner()
    }

    #[test]
    fn clamps_and_interpolates_palette() {
        let palette = [[10, 20, 30], [100, 110, 120], [200, 210, 220]];
        assert_eq!(palette_at(&palette, -1.0), palette[0]);
        assert_eq!(palette_at(&palette, 0.25), [55, 65, 75]);
        assert_eq!(palette_at(&palette, 0.75), [150, 160, 170]);
        assert_eq!(palette_at(&palette, 2.0), palette[2]);
    }

    #[test]
    fn neutral_artwork_uses_fallback_palette() {
        let image = RgbaImage::from_pixel(8, 8, Rgba([100, 100, 100, 255]));
        assert_eq!(palette_from_png(&png(image)), NEUTRAL_PALETTE);
    }

    #[test]
    fn colorful_artwork_produces_bright_accents() {
        let image = RgbaImage::from_fn(16, 16, |x, _| {
            if x < 8 {
                Rgba([220, 20, 20, 255])
            } else {
                Rgba([20, 80, 230, 255])
            }
        });
        let colors = palette_from_png(&png(image));
        assert_ne!(colors, NEUTRAL_PALETTE);
        assert!(colors.iter().flatten().any(|channel| *channel > 200));
    }
}
