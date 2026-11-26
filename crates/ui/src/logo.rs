//! The Imperio logo, drawn in code.
//!
//! A screen with a dot in it, white on a rounded square in the accent colour
//! (red for Free, gold for Pro): the other computer's screen, and you on it.
//!
//! This file has no dependencies beyond the standard library, because it is
//! used twice: at run time for window icons, and by the build scripts (which
//! `#[path]`-include it) to give each program its icon in Explorer and on the
//! taskbar. `site/icon.svg` is the same drawing for the website; keep the two
//! in step.

#![allow(dead_code)]

/// The Free plan's accent, 0xRRGGBB. The icon inside the program file uses it.
pub const RED: u32 = 0xE6453B;

/// Distance from `(x, y)` to the edge of a rounded rectangle: negative inside.
fn rounded_rect(x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> f32 {
    let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    let (hx, hy) = ((x1 - x0) / 2.0 - r, (y1 - y0) / 2.0 - r);
    let (dx, dy) = ((x - cx).abs() - hx, (y - cy).abs() - hy);
    dx.max(0.0).hypot(dy.max(0.0)) + dx.max(dy).min(0.0) - r
}

/// Is `(x, y)`, on the 64-unit design grid, part of the white drawing?
/// `bold` is the version for very small icons: fewer, thicker parts.
fn white(x: f32, y: f32, bold: bool) -> bool {
    if bold {
        // The screen and the dot only; a stand would be a smudge at 16 pixels.
        let frame = rounded_rect(x, y, 11.0, 15.0, 53.0, 49.0, 7.0);
        let glass = rounded_rect(x, y, 17.5, 21.5, 46.5, 42.5, 2.0);
        let dot = (x - 32.0).hypot(y - 32.0) - 5.4;
        return (frame <= 0.0 && glass > 0.0) || dot <= 0.0;
    }
    let frame = rounded_rect(x, y, 13.0, 15.0, 51.0, 41.5, 6.0);
    let glass = rounded_rect(x, y, 17.6, 19.6, 46.4, 36.9, 2.2);
    let neck = rounded_rect(x, y, 29.6, 41.0, 34.4, 47.5, 0.0);
    let foot = rounded_rect(x, y, 22.5, 46.5, 41.5, 51.0, 2.25);
    let dot = (x - 32.0).hypot(y - 28.25) - 4.5;
    (frame <= 0.0 && glass > 0.0) || neck <= 0.0 || foot <= 0.0 || dot <= 0.0
}

/// The logo as `size` x `size` pixels, RGBA with straight alpha, top row first.
pub fn rgba(size: u32, accent: u32) -> Vec<u8> {
    let n = size as usize;
    let bold = size < 28;
    let (ar, ag, ab) = (((accent >> 16) & 0xFF) as f32, ((accent >> 8) & 0xFF) as f32, (accent & 0xFF) as f32);
    // Lighter at the top left, darker at the bottom right.
    let shade = |c: f32, t: f32| -> f32 {
        let light = c + (255.0 - c) * 0.14;
        let dark = c * 0.78;
        if t < 0.55 {
            light + (c - light) * (t / 0.55)
        } else {
            c + (dark - c) * ((t - 0.55) / 0.45)
        }
    };
    const SUB: usize = 6;
    let step = 64.0 / n as f32 / SUB as f32;
    let mut out = vec![0u8; n * n * 4];
    for py in 0..n {
        for px in 0..n {
            let (mut r, mut g, mut b, mut a) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for sy in 0..SUB {
                for sx in 0..SUB {
                    let x = (px * SUB + sx) as f32 * step + step / 2.0;
                    let y = (py * SUB + sy) as f32 * step + step / 2.0;
                    if rounded_rect(x, y, 4.0, 4.0, 60.0, 60.0, 15.0) > 0.0 {
                        continue;
                    }
                    a += 1.0;
                    if white(x, y, bold) {
                        r += 255.0;
                        g += 255.0;
                        b += 255.0;
                    } else {
                        let t = ((x - 4.0) + (y - 4.0)) / 112.0;
                        r += shade(ar, t);
                        g += shade(ag, t);
                        b += shade(ab, t);
                    }
                }
            }
            if a > 0.0 {
                let i = (py * n + px) * 4;
                out[i] = (r / a).round() as u8;
                out[i + 1] = (g / a).round() as u8;
                out[i + 2] = (b / a).round() as u8;
                out[i + 3] = (a * 255.0 / (SUB * SUB) as f32).round() as u8;
            }
        }
    }
    out
}

fn crc32(data: &[u8], mut crc: u32) -> u32 {
    crc = !crc;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// The logo as a PNG file (not compressed: these are small).
pub fn png(size: u32, accent: u32) -> Vec<u8> {
    let pixels = rgba(size, accent);
    let row = size as usize * 4;
    // Each row starts with a filter byte (0 = none).
    let mut raw = Vec::with_capacity(pixels.len() + size as usize);
    for line in pixels.chunks(row) {
        raw.push(0);
        raw.extend_from_slice(line);
    }
    // zlib stream of stored (uncompressed) blocks.
    let mut z = vec![0x78, 0x01];
    let mut blocks = raw.chunks(0xFFFF).peekable();
    while let Some(block) = blocks.next() {
        z.push(blocks.peek().is_none() as u8);
        z.extend_from_slice(&(block.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        z.extend_from_slice(block);
    }
    let (mut s1, mut s2) = (1u32, 0u32);
    for &byte in &raw {
        s1 = (s1 + byte as u32) % 65_521;
        s2 = (s2 + s1) % 65_521;
    }
    z.extend_from_slice(&((s2 << 16) | s1).to_be_bytes());

    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut chunk = |kind: &[u8; 4], body: &[u8]| {
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out.extend_from_slice(&crc32(body, crc32(kind, 0)).to_be_bytes());
    };
    let mut header = Vec::new();
    header.extend_from_slice(&size.to_be_bytes());
    header.extend_from_slice(&size.to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]); // 8 bits, RGBA
    chunk(b"IHDR", &header);
    chunk(b"IDAT", &z);
    chunk(b"IEND", &[]);
    out
}

/// One picture of an icon as Windows stores it: a bitmap without its file
/// header, bottom row first, followed by an (empty) one-bit mask.
fn icon_bitmap(size: u32, accent: u32) -> Vec<u8> {
    let pixels = rgba(size, accent);
    let n = size as usize;
    let mut out = Vec::new();
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(size as i32).to_le_bytes());
    out.extend_from_slice(&(size as i32 * 2).to_le_bytes()); // picture + mask
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0u8; 24]);
    for y in (0..n).rev() {
        for x in 0..n {
            let p = &pixels[(y * n + x) * 4..][..4];
            out.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
    }
    let mask_row = n.div_ceil(32) * 4;
    out.resize(out.len() + mask_row * n, 0);
    out
}

/// Sizes Windows asks for: title bars and the tray at 100 to 250 % scaling,
/// the taskbar, Alt+Tab, and Explorer's large icons.
const ICON_SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];

/// Every picture of the program icon: (size, data).
fn icon_pictures(accent: u32) -> Vec<(u32, Vec<u8>)> {
    ICON_SIZES
        .iter()
        .map(|&s| (s, if s >= 256 { png(s, accent) } else { icon_bitmap(s, accent) }))
        .collect()
}

/// One picture for `CreateIconFromResourceEx`.
pub fn icon_resource(size: u32, accent: u32) -> Vec<u8> {
    icon_bitmap(size, accent)
}

/// The logo as an `.ico` file.
pub fn ico(accent: u32) -> Vec<u8> {
    let pictures = icon_pictures(accent);
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(pictures.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * pictures.len();
    for (size, data) in &pictures {
        let dim = if *size >= 256 { 0 } else { *size as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0, 1, 0, 32, 0]);
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += data.len();
    }
    for (_, data) in &pictures {
        out.extend_from_slice(data);
    }
    out
}

/// Resource id of the program icon inside each `.exe` (the lowest id is the
/// one Explorer shows).
pub const ICON_RESOURCE_ID: u16 = 1;

/// A compiled resource file (`.res`) holding the program icon, which the
/// linker accepts as it is: no resource compiler is needed to build Imperio.
pub fn resource_file(accent: u32) -> Vec<u8> {
    const RT_ICON: u16 = 3;
    const RT_GROUP_ICON: u16 = 14;
    fn entry(out: &mut Vec<u8>, kind: u16, id: u16, data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&32u32.to_le_bytes());
        out.extend_from_slice(&[0xFF, 0xFF]);
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&[0xFF, 0xFF]);
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // data version
        out.extend_from_slice(&0x1030u16.to_le_bytes()); // moveable, pure, discardable
        out.extend_from_slice(&0x0409u16.to_le_bytes()); // language: English (US)
        out.extend_from_slice(&0u32.to_le_bytes()); // version
        out.extend_from_slice(&0u32.to_le_bytes()); // characteristics
        out.extend_from_slice(data);
        // Every entry starts on a multiple of four bytes.
        let padding = (4 - out.len() % 4) % 4;
        out.resize(out.len() + padding, 0);
    }
    let pictures = icon_pictures(accent);
    let mut out = Vec::new();
    // Every .res file opens with an empty entry.
    out.extend_from_slice(&[0, 0, 0, 0, 32, 0, 0, 0, 0xFF, 0xFF, 0, 0, 0xFF, 0xFF, 0, 0]);
    out.extend_from_slice(&[0u8; 16]);
    let mut group = Vec::new();
    group.extend_from_slice(&[0, 0, 1, 0]);
    group.extend_from_slice(&(pictures.len() as u16).to_le_bytes());
    for (i, (size, data)) in pictures.iter().enumerate() {
        let dim = if *size >= 256 { 0 } else { *size as u8 };
        group.extend_from_slice(&[dim, dim, 0, 0, 1, 0, 32, 0]);
        group.extend_from_slice(&(data.len() as u32).to_le_bytes());
        group.extend_from_slice(&(i as u16 + 1).to_le_bytes());
        entry(&mut out, RT_ICON, i as u16 + 1, data);
    }
    entry(&mut out, RT_GROUP_ICON, ICON_RESOURCE_ID, &group);
    out
}

/// For a build script: put the icon into the programs of this package.
pub fn embed_in_programs() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../crates/ui/src/logo.rs");
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("windows-msvc") {
        return;
    }
    let Some(dir) = std::env::var_os("OUT_DIR") else { return };
    let path = std::path::Path::new(&dir).join("imperio-icon.res");
    std::fs::write(&path, resource_file(RED)).expect("write the icon resource");
    println!("cargo:rustc-link-arg-bins={}", path.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_drawing_is_where_it_should_be() {
        for size in [16u32, 32, 64, 256] {
            let p = rgba(size, RED);
            let at = |x: u32, y: u32| -> [u8; 4] { p[((y * size + x) * 4) as usize..][..4].try_into().unwrap() };
            assert_eq!(at(0, 0)[3], 0, "corner is clear at {size}");
            // The dot sits a little higher in the larger drawing (which has a
            // stand under the screen) than in the one for very small icons.
            let centre = if size < 28 { at(size / 2, size / 2) } else { at(size / 2, size * 29 / 64) };
            assert!(centre.iter().all(|c| *c >= 235), "the dot is white at {size}: {centre:?}");
            let side = at(size * 6 / 64, size / 2);
            assert!(side[3] == 255 && side[0] > 200 && side[1] < 120, "the square is red at {size}: {side:?}");
        }
    }

    #[test]
    fn files_have_the_right_shape() {
        let png = png(16, RED);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        // The checksum of the standard test string.
        assert_eq!(crc32(b"123456789", 0), 0xCBF4_3926);
        let ico = ico(RED);
        assert_eq!(&ico[..6], &[0, 0, 1, 0, ICON_SIZES.len() as u8, 0]);
        // The last picture ends where the file ends.
        let last = 6 + 16 * (ICON_SIZES.len() - 1);
        let len = u32::from_le_bytes(ico[last + 8..last + 12].try_into().unwrap()) as usize;
        let off = u32::from_le_bytes(ico[last + 12..last + 16].try_into().unwrap()) as usize;
        assert_eq!(off + len, ico.len());
        assert_eq!(resource_file(RED).len() % 4, 0);
    }
}
