use std::env;
use std::fs;
use std::path::Path;

fn rounded_rect(x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, radius: f32) -> bool {
    if x < x0 || x > x1 || y < y0 || y > y1 {
        return false;
    }
    let cx = x.clamp(x0 + radius, x1 - radius);
    let cy = y.clamp(y0 + radius, y1 - radius);
    let dx = x - cx;
    let dy = y - cy;
    dx * dx + dy * dy <= radius * radius
}

/// Base app icon: blue rounded square with a white speech bubble.
/// When `badge` is set, a red dot is drawn in the bottom-right corner.
fn draw_icon(size: u32, badge: bool) -> Vec<u8> {
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    let s = size as f32;
    let radius = s * 0.22;
    let inset = s * 0.06;
    let max = s - inset;

    let bx0 = s * 0.24;
    let bx1 = s * 0.76;
    let by0 = s * 0.28;
    let by1 = s * 0.62;

    for y in 0..size {
        for x in 0..size {
            let fx = x as f32 + 0.5;
            let fy = y as f32 + 0.5;

            let inside = rounded_rect(fx, fy, inset, inset, max, max, radius);
            let bubble = rounded_rect(fx, fy, bx0, by0, bx1, by1, s * 0.10);
            let tail = (fy > by1 && fy < by1 + s * 0.12)
                && fx > s * 0.30
                && fx < s * 0.48
                && (fx - s * 0.30) < (s * 0.48 - fx) * 1.6;

            let mut px = if inside {
                if bubble || tail {
                    (255u8, 255u8, 255u8, 255u8)
                } else {
                    (26, 115, 232, 255)
                }
            } else {
                (0, 0, 0, 0)
            };

            if badge {
                let cx = s * 0.78;
                let cy = s * 0.78;
                let r = s * 0.24;
                let dx = fx - cx;
                let dy = fy - cy;
                let d = (dx * dx + dy * dy).sqrt();
                if d <= r {
                    px = if d > r - s * 0.06 {
                        (255, 255, 255, 255) // white ring for contrast
                    } else {
                        (234, 67, 53, 255) // Google red
                    };
                }
            }

            data.push(px.0);
            data.push(px.1);
            data.push(px.2);
            data.push(px.3);
        }
    }
    data
}

/// Small transparent overlay: a red dot with a thin white ring.
fn draw_overlay(size: u32) -> Vec<u8> {
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    let s = size as f32;
    let cx = s / 2.0;
    let cy = s / 2.0;
    let r = s * 0.46;

    for y in 0..size {
        for x in 0..size {
            let fx = x as f32 + 0.5;
            let fy = y as f32 + 0.5;
            let d = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
            let px = if d <= r - s * 0.12 {
                (234u8, 67u8, 53u8, 255u8)
            } else if d <= r {
                (255, 255, 255, 255)
            } else {
                (0, 0, 0, 0)
            };
            data.push(px.0);
            data.push(px.1);
            data.push(px.2);
            data.push(px.3);
        }
    }
    data
}

/// 3x5 pixel glyphs for digits 0-9, row-major, used to draw the unread
/// counter into the tray icon at runtime.
fn emit_digits() -> String {
    const ROWS: [&str; 50] = [
        "111", "101", "101", "101", "111", // 0
        "010", "110", "010", "010", "111", // 1
        "111", "001", "111", "100", "111", // 2
        "111", "001", "111", "001", "111", // 3
        "101", "101", "111", "001", "001", // 4
        "111", "100", "111", "001", "111", // 5
        "111", "100", "111", "101", "111", // 6
        "111", "001", "001", "001", "001", // 7
        "111", "101", "111", "101", "111", // 8
        "111", "101", "111", "001", "111", // 9
    ];
    let mut out = String::from("pub const DIGIT_GLYPHS: [[u8; 15]; 10] = [");
    for digit in 0..10 {
        out.push_str("\n    [");
        for (i, ch) in ROWS[digit * 5..digit * 5 + 5].concat().bytes().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(if ch == b'1' { "1u8" } else { "0u8" });
        }
        out.push_str("],");
    }
    out.push_str("\n];\n");
    out
}

fn emit(name: &str, data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 4 + name.len() + 64);
    out.push_str("pub const ");
    out.push_str(name);
    out.push_str(": &[u8] = &[");
    for (i, b) in data.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&b.to_string());
    }
    out.push_str("];\n");
    out
}

fn main() {
    let out_dir = env::var_os("OUT_DIR").expect("OUT_DIR not set");
    let dest_path = Path::new(&out_dir).join("icon_data.rs");

    let base = draw_icon(32, false);
    let badge = draw_icon(32, true);
    let overlay = draw_overlay(16);

    let mut src = String::new();
    src.push_str(&emit("ICON_RGBA_32", &base));
    src.push_str(&emit("ICON_BADGE_32", &badge));
    src.push_str(&emit("ICON_OVERLAY_16", &overlay));
    src.push_str(&emit_digits());
    src.push_str("pub const ICON_SIZE: u32 = 32;\npub const OVERLAY_SIZE: u32 = 16;\n");

    fs::write(&dest_path, src).expect("Failed to write icon data");
}
