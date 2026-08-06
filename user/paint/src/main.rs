//! A drawing program for Kestrel.
//!
//! The first program to put pixels on the screen rather than characters. It
//! asks the kernel for a window, reads the pointer's position inside it, and
//! pushes a frame back each pass.
//!
//! Everything is in a fixed array: there is no allocator for programs, so the
//! canvas size is a constant and the whole picture lives in the binary's BSS.

#![no_std]
#![no_main]

const WIDTH: usize = 240;
const HEIGHT: usize = 160;
/// Room along the top for the colour swatches.
const PALETTE_HEIGHT: usize = 28;

const PALETTE: [u32; 10] = [
    0x1B1B1B, // near black
    0xFFFFFF, // white
    0xE05252, // red
    0xE08A3C, // orange
    0xE0C93C, // yellow
    0x4CAF50, // green
    0x2BA098, // teal
    0x3C8DBC, // blue
    0x9B59B6, // purple
    0xE0709B, // pink
];

const CANVAS: u32 = 0xF4F6F8;

/// Zero-initialised on purpose. A non-zero initialiser would put half a
/// megabyte of literal pixels in the binary's `.data`; zeroed, it lands in
/// `.bss`, which the loader satisfies with pages that arrive blank. The canvas
/// is painted white by `clear_canvas` at startup instead.
static mut PIXELS: [u32; WIDTH * HEIGHT] = [0; WIDTH * HEIGHT];

fn pixels() -> &'static mut [u32; WIDTH * HEIGHT] {
    unsafe { &mut *core::ptr::addr_of_mut!(PIXELS) }
}

fn clear_canvas() {
    let buffer = pixels();
    for y in PALETTE_HEIGHT..HEIGHT {
        for x in 0..WIDTH {
            buffer[y * WIDTH + x] = CANVAS;
        }
    }
}

fn draw_palette(selected: usize, brush: usize) {
    let buffer = pixels();
    let swatch = WIDTH / PALETTE.len();

    for (index, colour) in PALETTE.iter().enumerate() {
        let start = index * swatch;
        let end = if index == PALETTE.len() - 1 { WIDTH } else { start + swatch };

        for y in 0..PALETTE_HEIGHT {
            for x in start..end {
                // The chosen colour is marked with a light border rather than
                // a separate indicator, which would need somewhere to put it.
                let edge = index == selected
                    && (y < 3 || y >= PALETTE_HEIGHT - 3 || x < start + 3 || x >= end - 3);
                buffer[y * WIDTH + x] = if edge { 0xFFFFFF } else { *colour };
            }
        }
    }

    // Brush size, shown as a dot in the corner of the first swatch.
    for y in 0..brush.min(PALETTE_HEIGHT) {
        for x in 0..brush.min(swatch) {
            buffer[y * WIDTH + x] = 0xFFFFFF;
        }
    }
}

/// A filled circle, so a stroke has soft ends rather than square ones.
fn dab(cx: usize, cy: usize, radius: usize, colour: u32) {
    let buffer = pixels();
    let r = radius as isize;

    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy > r * r {
                continue;
            }
            let x = cx as isize + dx;
            let y = cy as isize + dy;

            // Never paint over the palette: it is the only control there is.
            if x < 0 || y < PALETTE_HEIGHT as isize || x >= WIDTH as isize || y >= HEIGHT as isize {
                continue;
            }
            buffer[y as usize * WIDTH + x as usize] = colour;
        }
    }
}

/// Join two dabs, so a fast drag draws a line rather than a dotted trail.
fn stroke(from: (usize, usize), to: (usize, usize), radius: usize, colour: u32) {
    let (x0, y0) = (from.0 as isize, from.1 as isize);
    let (x1, y1) = (to.0 as isize, to.1 as isize);

    let steps = (x1 - x0).abs().max((y1 - y0).abs()).max(1);
    for step in 0..=steps {
        let x = x0 + (x1 - x0) * step / steps;
        let y = y0 + (y1 - y0) * step / steps;
        dab(x as usize, y as usize, radius, colour);
    }
}

fn run() {
    if !kestrel::surface(WIDTH, HEIGHT, "Paint") {
        kestrel::write_line("paint: no desktop to draw in - run 'desktop' first");
        return;
    }

    kestrel::write_line("paint: click the swatches to pick a colour");
    kestrel::write_line("       [ and ] change the brush, c clears, s saves, q quits");

    let mut colour = 0usize;
    let mut brush = 3usize;
    let mut last: Option<(usize, usize)> = None;

    clear_canvas();
    draw_palette(colour, brush);

    loop {
        while let Some(key) = kestrel::read_key() {
            match key {
                b'q' | b'Q' => return,
                b'c' | b'C' => clear_canvas(),
                b'[' => brush = brush.saturating_sub(1).max(1),
                b']' => brush = (brush + 1).min(20),
                b's' | b'S' => {
                    // Raw pixels, not an image format: there is no decoder on
                    // this system to read one back with anyway.
                    let bytes = unsafe {
                        core::slice::from_raw_parts(
                            pixels().as_ptr() as *const u8,
                            WIDTH * HEIGHT * 4,
                        )
                    };
                    match kestrel::store("/disk/picture.raw", bytes) {
                        kestrel::FAILED => kestrel::write_line("could not save"),
                        _ => kestrel::write_line("saved to /disk/picture.raw"),
                    }
                }
                _ => {}
            }
        }

        if let Some(p) = kestrel::pointer() {
            if p.left {
                if p.y < PALETTE_HEIGHT {
                    // Picking a colour, not drawing.
                    let swatch = WIDTH / PALETTE.len();
                    colour = (p.x / swatch).min(PALETTE.len() - 1);
                    last = None;
                } else {
                    match last {
                        Some(previous) => stroke(previous, (p.x, p.y), brush, PALETTE[colour]),
                        None => dab(p.x, p.y, brush, PALETTE[colour]),
                    }
                    last = Some((p.x, p.y));
                }
            } else {
                // Lifting the button ends the stroke, so the next one does not
                // draw a line from wherever this one finished.
                last = None;
            }
        } else {
            last = None;
        }

        draw_palette(colour, brush);
        kestrel::blit(pixels());
        kestrel::sleep(16);
    }
}

kestrel::main!(run);

