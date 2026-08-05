//! Kestrel's mark, described once and drawn by both the kernel and the asset
//! generator.
//!
//! The shape is defined as a function from a point to a colour rather than as
//! stored pixels, so it stays crisp at any size — from a 16-pixel icon to a
//! boot splash on a 1280x800 framebuffer.
//!
//! No square roots: distances are compared squared, so this needs no floating
//! point library and works in the kernel as-is.

#![no_std]

/// Colours, as 0xRRGGBB.
pub const BACKDROP_TOP: u32 = 0x1B2B44;
pub const BACKDROP_BOTTOM: u32 = 0x0C1626;
pub const WING: u32 = 0x7AC7FF;
pub const WING_SHADE: u32 = 0x4A93D6;
pub const BEAK: u32 = 0xF2B441;

/// What a point of the mark is made of.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Backdrop,
    Wing,
    WingShade,
    Beak,
}

type Point = (f32, f32);

/// Squared distance from `p` to the segment `a`..`b`, and how far along it the
/// nearest point lies.
fn segment(p: Point, a: Point, b: Point) -> (f32, f32) {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let length_squared = dx * dx + dy * dy;

    let mut t = if length_squared <= 1e-9 {
        0.0
    } else {
        ((p.0 - a.0) * dx + (p.1 - a.1) * dy) / length_squared
    };
    if t < 0.0 {
        t = 0.0;
    }
    if t > 1.0 {
        t = 1.0;
    }

    let (ex, ey) = (p.0 - (a.0 + dx * t), p.1 - (a.1 + dy * t));
    (t, ex * ex + ey * ey)
}

/// A stroke whose half-width tapers from `start` to `end`.
///
/// Tapered strokes are what give the wings and tail their shape: a falcon in a
/// stoop is all swept lines narrowing to points.
fn stroke(p: Point, a: Point, b: Point, start: f32, end: f32) -> bool {
    let (t, distance_squared) = segment(p, a, b);
    let width = start + (end - start) * t;
    distance_squared <= width * width
}

fn circle(p: Point, centre: Point, radius: f32) -> bool {
    let (dx, dy) = (p.0 - centre.0, p.1 - centre.1);
    dx * dx + dy * dy <= radius * radius
}

/// Which part of the mark covers `(u, v)`, both in 0..1.
pub fn part(u: f32, v: f32) -> Part {
    let p = (u, v);

    // The head, and a beak jutting from it.
    if circle(p, (0.50, 0.30), 0.085) {
        return Part::Wing;
    }
    if stroke(p, (0.50, 0.245), (0.50, 0.175), 0.045, 0.004) {
        return Part::Beak;
    }

    // Body tapering to a tail.
    if stroke(p, (0.50, 0.33), (0.50, 0.83), 0.072, 0.012) {
        return Part::Wing;
    }

    // Wings, swept back and down. Two strokes each, so the leading edge has
    // some sweep instead of reading as one straight bar.
    if stroke(p, (0.52, 0.34), (0.90, 0.50), 0.075, 0.010)
        || stroke(p, (0.52, 0.40), (0.78, 0.60), 0.055, 0.008)
    {
        return Part::Wing;
    }

    if stroke(p, (0.48, 0.34), (0.10, 0.50), 0.075, 0.010)
        || stroke(p, (0.48, 0.40), (0.22, 0.60), 0.055, 0.008)
    {
        // The trailing wing is shaded, which reads as depth at small sizes.
        return Part::WingShade;
    }

    Part::Backdrop
}

/// Is `(u, v)` inside the rounded-square badge?
pub fn in_badge(u: f32, v: f32, radius: f32) -> bool {
    let inset = |value: f32| -> f32 {
        if value < radius {
            radius - value
        } else if value > 1.0 - radius {
            value - (1.0 - radius)
        } else {
            0.0
        }
    };

    let dx = inset(u);
    let dy = inset(v);
    dx * dx + dy * dy <= radius * radius
}

pub fn blend(a: u32, b: u32, t: f32) -> u32 {
    let channel = |shift: u32| {
        let x = ((a >> shift) & 0xFF) as f32;
        let y = ((b >> shift) & 0xFF) as f32;
        ((x + (y - x) * t) as u32) & 0xFF
    };
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

/// Colour of one point, badge included.
pub fn colour(u: f32, v: f32) -> u32 {
    if !in_badge(u, v, 0.22) {
        return BACKDROP_BOTTOM;
    }

    match part(u, v) {
        Part::Wing => WING,
        Part::WingShade => WING_SHADE,
        Part::Beak => BEAK,
        Part::Backdrop => blend(BACKDROP_TOP, BACKDROP_BOTTOM, v),
    }
}
