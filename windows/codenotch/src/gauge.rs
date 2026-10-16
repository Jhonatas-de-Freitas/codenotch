//! The tray icon as a gauge: a rounded square coloured by how close the limit is, with the
//! percentage written in it. Drawn pixel by pixel from a 3x5 digit font, so no font or image
//! crate is involved and the icon is crisp at any tray size.

use tauri::image::Image;

const N: usize = 32;

/// 3x5 digits, one entry per row, the low three bits as columns left to right.
const DIGITS: [[u8; 5]; 11] = [
    [0b111, 0b101, 0b101, 0b101, 0b111],
    [0b010, 0b110, 0b010, 0b010, 0b111],
    [0b111, 0b001, 0b111, 0b100, 0b111],
    [0b111, 0b001, 0b111, 0b001, 0b111],
    [0b101, 0b101, 0b111, 0b001, 0b001],
    [0b111, 0b100, 0b111, 0b001, 0b111],
    [0b111, 0b100, 0b111, 0b101, 0b111],
    [0b111, 0b001, 0b001, 0b001, 0b001],
    [0b111, 0b101, 0b111, 0b101, 0b111],
    [0b111, 0b101, 0b111, 0b001, 0b111],
    // index 10: a dash, for a reading that is not there
    [0b000, 0b000, 0b111, 0b000, 0b000],
];

/// Green under 50, amber under 75, orange under 90, red from there. No reading yet: the app's mark.
pub fn icon(pct: Option<u32>, blink: bool) -> Image<'static> {
    let Some(pct) = pct else {
        return crate::trayicon::app_mark()
            .unwrap_or_else(|| Image::new_owned(vec![0; N * N * 4], N as u32, N as u32));
    };
    let bg: [u8; 3] = match pct {
        0..=49 => [0x1F, 0x8F, 0x55],
        50..=74 => [0xB7, 0x7B, 0x00],
        75..=89 => [0xD0, 0x62, 0x10],
        _ => [0xC9, 0x2A, 0x2E],
    };
    // Blinking: white tile and coloured digits, the inverse of the steady icon
    let (tile, ink) = if blink { ([255, 255, 255], bg) } else { (bg, [255, 255, 255]) };
    let mut px = vec![0u8; N * N * 4];
    let radius = 7.0f64;
    for y in 0..N {
        for x in 0..N {
            // How far the pixel sits outside the straight edges, which is only nonzero in a corner
            let (fx, fy) = (x as f64 + 0.5, y as f64 + 0.5);
            let dx = (radius - fx).max(fx - (N as f64 - radius)).max(0.0);
            let dy = (radius - fy).max(fy - (N as f64 - radius)).max(0.0);
            if dx * dx + dy * dy <= radius * radius {
                let i = (y * N + x) * 4;
                px[i..i + 4].copy_from_slice(&[tile[0], tile[1], tile[2], 255]);
            }
        }
    }
    let digits: Vec<usize> = pct.min(100).to_string().bytes().map(|b| (b - b'0') as usize).collect();
    // The widest that fits: one or two digits at 4 px a cell, three at 3
    let scale = if digits.len() >= 3 { 3 } else { 4 };
    let gap = if scale == 3 { 2 } else { scale };
    let w = digits.len() * 3 * scale + (digits.len() - 1) * gap;
    let (x0, y0) = ((N - w) / 2, (N - 5 * scale) / 2);
    for (n, d) in digits.iter().enumerate() {
        for (row, bits) in DIGITS[*d].iter().enumerate() {
            for col in 0..3 {
                if (bits >> (2 - col)) & 1 == 0 {
                    continue;
                }
                for sy in 0..scale {
                    for sx in 0..scale {
                        let x = x0 + n * (3 * scale + gap) + col * scale + sx;
                        let y = y0 + row * scale + sy;
                        let i = (y * N + x) * 4;
                        px[i..i + 4].copy_from_slice(&[ink[0], ink[1], ink[2], 255]);
                    }
                }
            }
        }
    }
    Image::new_owned(px, N as u32, N as u32)
}

fn level_color(pct: u32) -> [u8; 3] {
    match pct {
        0..=49 => [0x34, 0xC4, 0x7C],
        50..=74 => [0xE8, 0xAA, 0x22],
        75..=89 => [0xF0, 0x80, 0x2A],
        _ => [0xEF, 0x4F, 0x55],
    }
}

fn put(px: &mut [u8], x: usize, y: usize, c: [u8; 3]) {
    let i = (y * N + x) * 4;
    px[i..i + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
}

/// Two characters (digits, or dashes) at 3 px a cell, 21 wide and 15 tall, from (x0, y0).
fn pair(px: &mut [u8], x0: usize, y0: usize, glyphs: [usize; 2], c: [u8; 3]) {
    for (n, g) in glyphs.iter().enumerate() {
        for (row, bits) in DIGITS[*g].iter().enumerate() {
            for col in 0..3 {
                if (bits >> (2 - col)) & 1 == 1 {
                    for sy in 0..3 {
                        for sx in 0..3 {
                            put(px, x0 + n * 12 + col * 3 + sx, y0 + row * 3 + sy, c);
                        }
                    }
                }
            }
        }
    }
}

/// Claude on the top row, Codex on the bottom, each as big as the icon allows: four numbers at once
/// came out unreadable at tray size, so the pair alternates between the session and the week and
/// a bar down one side says which is showing (left edge: session, right edge: week).
/// A reading of 100 is drawn as 99, in red, which is what a full limit looks like here.
pub fn quad(vals: [Option<u32>; 2], week: bool, blink: bool) -> Image<'static> {
    let mut px = vec![0u8; N * N * 4];
    let radius = 7.0f64;
    for y in 0..N {
        for x in 0..N {
            let (fx, fy) = (x as f64 + 0.5, y as f64 + 0.5);
            let dx = (radius - fx).max(fx - (N as f64 - radius)).max(0.0);
            let dy = (radius - fy).max(fy - (N as f64 - radius)).max(0.0);
            if dx * dx + dy * dy <= radius * radius {
                put(&mut px, x, y, if blink { [0xC9, 0x2A, 0x2E] } else { [0x1E, 0x1F, 0x22] });
            }
        }
    }
    for x in 4..N - 4 {
        put(&mut px, x, 16, [0x4A, 0x4D, 0x52]);
    }
    // Which reading this is: a bar on the left for the session, on the right for the week
    let bar_x = if week { 29 } else { 1 };
    for y in 6..N - 6 {
        for dx in 0..2 {
            put(&mut px, bar_x + dx, y, [0x7A, 0xA7, 0xFF]);
        }
    }
    for (v, y0) in vals.iter().zip([1usize, 17]) {
        let (glyphs, color) = match v {
            Some(p) => {
                let shown = (*p).min(99);
                ([(shown / 10) as usize, (shown % 10) as usize], level_color(if *p >= 99 { 100 } else { shown }))
            }
            None => ([10, 10], [0x80, 0x84, 0x8A]),
        };
        pair(&mut px, 5, y0, glyphs, if blink { [255, 255, 255] } else { color });
    }
    Image::new_owned(px, N as u32, N as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_size_of_number_fits_in_the_icon() {
        for p in [0, 7, 56, 99, 100] {
            let img = icon(Some(p), p % 2 == 0);
            assert_eq!((img.width(), img.height()), (32, 32));
        }
        let img = quad([Some(100), None], true, true);
        assert_eq!((img.width(), img.height()), (32, 32));
    }
}
