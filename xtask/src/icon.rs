//! Generates Kestrel's icon from the shared mark description.

use std::io;
use std::path::Path;

/// Write the icon as a PNG.
///
/// Rendered at four times the requested size and averaged down. The shape is
/// all diagonals, and without that the edges stair-step badly — most visibly
/// at 16 pixels, where the icon is mostly edge.
pub fn write(path: &Path, size: u32) -> io::Result<()> {
    const SCALE: u32 = 4;

    let mut pixels = Vec::with_capacity((size * size * 3) as usize);
    let large = (size * SCALE) as f32;

    for y in 0..size {
        for x in 0..size {
            let (mut red, mut green, mut blue) = (0u32, 0u32, 0u32);

            for sub_y in 0..SCALE {
                for sub_x in 0..SCALE {
                    let u = ((x * SCALE + sub_x) as f32 + 0.5) / large;
                    let v = ((y * SCALE + sub_y) as f32 + 0.5) / large;

                    let colour = logo::colour(u, v);
                    red += (colour >> 16) & 0xFF;
                    green += (colour >> 8) & 0xFF;
                    blue += colour & 0xFF;
                }
            }

            let samples = SCALE * SCALE;
            pixels.push((red / samples) as u8);
            pixels.push((green / samples) as u8);
            pixels.push((blue / samples) as u8);
        }
    }

    crate::png::write_rgb(path, size as usize, size as usize, &pixels)
}
