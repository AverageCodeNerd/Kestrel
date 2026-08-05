//! Just enough PNG to turn QEMU's PPM screendumps into something viewable,
//! without pulling in an image crate.
//!
//! Pixel data is wrapped in a zlib stream made of *stored* (uncompressed)
//! deflate blocks, so no compressor is needed.

use std::io::{self, Write};
use std::path::Path;

pub fn ppm_to_png(ppm: &Path, png: &Path) -> io::Result<(usize, usize)> {
    let data = std::fs::read(ppm)?;
    let (width, height, pixels) = parse_ppm(&data)?;
    write_png(png, width, height, pixels)?;
    Ok((width, height))
}

/// Parse a binary (P6) PPM with 8 bits per channel.
fn parse_ppm(data: &[u8]) -> io::Result<(usize, usize, &[u8])> {
    let invalid = |msg: &str| io::Error::new(io::ErrorKind::InvalidData, msg.to_string());

    if !data.starts_with(b"P6") {
        return Err(invalid("not a binary PPM"));
    }

    let mut pos = 2;
    let mut fields = [0usize; 3]; // width, height, maxval
    for field in &mut fields {
        // Skip whitespace and full-line comments.
        loop {
            match data.get(pos) {
                Some(b'#') => while data.get(pos).is_some_and(|&b| b != b'\n') {
                    pos += 1;
                },
                Some(b) if b.is_ascii_whitespace() => pos += 1,
                _ => break,
            }
        }
        let start = pos;
        while data.get(pos).is_some_and(|b| b.is_ascii_digit()) {
            pos += 1;
        }
        if start == pos {
            return Err(invalid("malformed PPM header"));
        }
        *field = std::str::from_utf8(&data[start..pos])
            .unwrap()
            .parse()
            .map_err(|_| invalid("bad PPM number"))?;
    }

    if fields[2] != 255 {
        return Err(invalid("only 8-bit PPMs are supported"));
    }
    pos += 1; // single whitespace byte before the raster

    let (width, height) = (fields[0], fields[1]);
    let expected = width * height * 3;
    let pixels = data
        .get(pos..pos + expected)
        .ok_or_else(|| invalid("PPM raster is truncated"))?;

    Ok((width, height, pixels))
}

/// Write raw RGB pixels as a PNG.
pub fn write_rgb(path: &Path, width: usize, height: usize, pixels: &[u8]) -> io::Result<()> {
    write_png(path, width, height, pixels)
}

fn write_png(path: &Path, width: usize, height: usize, pixels: &[u8]) -> io::Result<()> {
    let mut out = std::fs::File::create(path)?;
    out.write_all(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])?;

    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(width as u32).to_be_bytes());
    ihdr.extend_from_slice(&(height as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB, no interlace
    chunk(&mut out, b"IHDR", &ihdr)?;

    // Each scanline is prefixed with filter type 0 (None).
    let mut raw = Vec::with_capacity(height * (1 + width * 3));
    for row in 0..height {
        raw.push(0);
        raw.extend_from_slice(&pixels[row * width * 3..(row + 1) * width * 3]);
    }

    chunk(&mut out, b"IDAT", &zlib_stored(&raw))?;
    chunk(&mut out, b"IEND", &[])?;
    Ok(())
}

fn chunk(out: &mut impl Write, kind: &[u8; 4], body: &[u8]) -> io::Result<()> {
    out.write_all(&(body.len() as u32).to_be_bytes())?;
    out.write_all(kind)?;
    out.write_all(body)?;

    // The CRC covers the chunk type and body, but not the length.
    let mut crc = crate::crc::Crc::new();
    crc.update(kind);
    crc.update(body);
    out.write_all(&crc.finish().to_be_bytes())
}

/// Wrap `data` in a zlib stream using uncompressed deflate blocks.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01]; // deflate, 32K window, no preset dict

    // Stored blocks carry a 16-bit length, so cap each at 65535 bytes.
    let mut chunks = data.chunks(0xFFFF).peekable();
    while let Some(block) = chunks.next() {
        let last = if chunks.peek().is_none() { 1 } else { 0 };
        out.push(last); // BFINAL, BTYPE=00
        out.extend_from_slice(&(block.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        out.extend_from_slice(block);
    }

    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

