mod cache;
pub mod graphics;
mod palette;

pub use cache::Cache;
pub use graphics::Graphics;
pub use palette::{NEUTRAL_PALETTE, Palette, palette_at};

/// Normalized cover artwork and its one-time sampled palette.
#[derive(Clone, Debug)]
pub struct Image {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub palette: Palette,
}
