// Actually, render functions usually take primitive types or simple structs.
// The `ViewerState::render` method in main.rs calls these primitives.
// So `render.rs` should just expose the primitives.
// AND `ViewerState::render` logic itself (the high level one) should probably belong to `ViewerState` in `state.rs` or be a separate "renderer" struct.
// The original code had `ViewerState::render`.
// If I put `ViewerState` in `state.rs`, it can have a `render` method that calls functions in `render.rs`.
// So `render.rs` will contain the primitives.

// Constants
pub const BG_COLOR: [u8; 4] = [31, 31, 31, 255]; // ~0.12 * 255

// 5x7 bitmap font covering ASCII 32..127. Each glyph is 5 columns × 7 rows
// packed into 5 bytes (one byte per column, LSB = top row).
static FONT_5X7: [[u8; 5]; 96] = {
    let mut f = [[0u8; 5]; 96];
    // space
    f[0]  = [0x00, 0x00, 0x00, 0x00, 0x00];
    // !
    f[1]  = [0x00, 0x00, 0x5F, 0x00, 0x00];
    // "
    f[2]  = [0x00, 0x07, 0x00, 0x07, 0x00];
    // #
    f[3]  = [0x14, 0x7F, 0x14, 0x7F, 0x14];
    // $
    f[4]  = [0x24, 0x2A, 0x7F, 0x2A, 0x12];
    // %
    f[5]  = [0x23, 0x13, 0x08, 0x64, 0x62];
    // &
    f[6]  = [0x36, 0x49, 0x55, 0x22, 0x50];
    // '
    f[7]  = [0x00, 0x05, 0x03, 0x00, 0x00];
    // (
    f[8]  = [0x00, 0x1C, 0x22, 0x41, 0x00];
    // )
    f[9]  = [0x00, 0x41, 0x22, 0x1C, 0x00];
    // *
    f[10] = [0x14, 0x08, 0x3E, 0x08, 0x14];
    // +
    f[11] = [0x08, 0x08, 0x3E, 0x08, 0x08];
    // ,
    f[12] = [0x00, 0x50, 0x30, 0x00, 0x00];
    // -
    f[13] = [0x08, 0x08, 0x08, 0x08, 0x08];
    // .
    f[14] = [0x00, 0x60, 0x60, 0x00, 0x00];
    // /
    f[15] = [0x20, 0x10, 0x08, 0x04, 0x02];
    // 0
    f[16] = [0x3E, 0x51, 0x49, 0x45, 0x3E];
    // 1
    f[17] = [0x00, 0x42, 0x7F, 0x40, 0x00];
    // 2
    f[18] = [0x42, 0x61, 0x51, 0x49, 0x46];
    // 3
    f[19] = [0x21, 0x41, 0x45, 0x4B, 0x31];
    // 4
    f[20] = [0x18, 0x14, 0x12, 0x7F, 0x10];
    // 5
    f[21] = [0x27, 0x45, 0x45, 0x45, 0x39];
    // 6
    f[22] = [0x3C, 0x4A, 0x49, 0x49, 0x30];
    // 7
    f[23] = [0x01, 0x71, 0x09, 0x05, 0x03];
    // 8
    f[24] = [0x36, 0x49, 0x49, 0x49, 0x36];
    // 9
    f[25] = [0x06, 0x49, 0x49, 0x29, 0x1E];
    // :
    f[26] = [0x00, 0x36, 0x36, 0x00, 0x00];
    // ;
    f[27] = [0x00, 0x56, 0x36, 0x00, 0x00];
    // <
    f[28] = [0x08, 0x14, 0x22, 0x41, 0x00];
    // =
    f[29] = [0x14, 0x14, 0x14, 0x14, 0x14];
    // >
    f[30] = [0x00, 0x41, 0x22, 0x14, 0x08];
    // ?
    f[31] = [0x02, 0x01, 0x51, 0x09, 0x06];
    // @
    f[32] = [0x3E, 0x41, 0x5D, 0x55, 0x1E];
    // A
    f[33] = [0x7E, 0x11, 0x11, 0x11, 0x7E];
    // B
    f[34] = [0x7F, 0x49, 0x49, 0x49, 0x36];
    // C
    f[35] = [0x3E, 0x41, 0x41, 0x41, 0x22];
    // D
    f[36] = [0x7F, 0x41, 0x41, 0x22, 0x1C];
    // E
    f[37] = [0x7F, 0x49, 0x49, 0x49, 0x41];
    // F
    f[38] = [0x7F, 0x09, 0x09, 0x09, 0x01];
    // G
    f[39] = [0x3E, 0x41, 0x49, 0x49, 0x7A];
    // H
    f[40] = [0x7F, 0x08, 0x08, 0x08, 0x7F];
    // I
    f[41] = [0x00, 0x41, 0x7F, 0x41, 0x00];
    // J
    f[42] = [0x20, 0x40, 0x41, 0x3F, 0x01];
    // K
    f[43] = [0x7F, 0x08, 0x14, 0x22, 0x41];
    // L
    f[44] = [0x7F, 0x40, 0x40, 0x40, 0x40];
    // M
    f[45] = [0x7F, 0x02, 0x0C, 0x02, 0x7F];
    // N
    f[46] = [0x7F, 0x04, 0x08, 0x10, 0x7F];
    // O
    f[47] = [0x3E, 0x41, 0x41, 0x41, 0x3E];
    // P
    f[48] = [0x7F, 0x09, 0x09, 0x09, 0x06];
    // Q
    f[49] = [0x3E, 0x41, 0x51, 0x21, 0x5E];
    // R
    f[50] = [0x7F, 0x09, 0x19, 0x29, 0x46];
    // S
    f[51] = [0x46, 0x49, 0x49, 0x49, 0x31];
    // T
    f[52] = [0x01, 0x01, 0x7F, 0x01, 0x01];
    // U
    f[53] = [0x3F, 0x40, 0x40, 0x40, 0x3F];
    // V
    f[54] = [0x1F, 0x20, 0x40, 0x20, 0x1F];
    // W
    f[55] = [0x3F, 0x40, 0x38, 0x40, 0x3F];
    // X
    f[56] = [0x63, 0x14, 0x08, 0x14, 0x63];
    // Y
    f[57] = [0x07, 0x08, 0x70, 0x08, 0x07];
    // Z
    f[58] = [0x61, 0x51, 0x49, 0x45, 0x43];
    // [
    f[59] = [0x00, 0x7F, 0x41, 0x41, 0x00];
    // backslash
    f[60] = [0x02, 0x04, 0x08, 0x10, 0x20];
    // ]
    f[61] = [0x00, 0x41, 0x41, 0x7F, 0x00];
    // ^
    f[62] = [0x04, 0x02, 0x01, 0x02, 0x04];
    // _
    f[63] = [0x40, 0x40, 0x40, 0x40, 0x40];
    // `
    f[64] = [0x00, 0x01, 0x02, 0x04, 0x00];
    // a
    f[65] = [0x20, 0x54, 0x54, 0x54, 0x78];
    // b
    f[66] = [0x7F, 0x48, 0x44, 0x44, 0x38];
    // c
    f[67] = [0x38, 0x44, 0x44, 0x44, 0x20];
    // d
    f[68] = [0x38, 0x44, 0x44, 0x48, 0x7F];
    // e
    f[69] = [0x38, 0x54, 0x54, 0x54, 0x18];
    // f
    f[70] = [0x08, 0x7E, 0x09, 0x01, 0x02];
    // g
    f[71] = [0x0C, 0x52, 0x52, 0x52, 0x3E];
    // h
    f[72] = [0x7F, 0x08, 0x04, 0x04, 0x78];
    // i
    f[73] = [0x00, 0x44, 0x7D, 0x40, 0x00];
    // j
    f[74] = [0x20, 0x40, 0x44, 0x3D, 0x00];
    // k
    f[75] = [0x7F, 0x10, 0x28, 0x44, 0x00];
    // l
    f[76] = [0x00, 0x41, 0x7F, 0x40, 0x00];
    // m
    f[77] = [0x7C, 0x04, 0x18, 0x04, 0x78];
    // n
    f[78] = [0x7C, 0x08, 0x04, 0x04, 0x78];
    // o
    f[79] = [0x38, 0x44, 0x44, 0x44, 0x38];
    // p
    f[80] = [0x7C, 0x14, 0x14, 0x14, 0x08];
    // q
    f[81] = [0x08, 0x14, 0x14, 0x18, 0x7C];
    // r
    f[82] = [0x7C, 0x08, 0x04, 0x04, 0x08];
    // s
    f[83] = [0x48, 0x54, 0x54, 0x54, 0x20];
    // t
    f[84] = [0x04, 0x3F, 0x44, 0x40, 0x20];
    // u
    f[85] = [0x3C, 0x40, 0x40, 0x20, 0x7C];
    // v
    f[86] = [0x1C, 0x20, 0x40, 0x20, 0x1C];
    // w
    f[87] = [0x3C, 0x40, 0x30, 0x40, 0x3C];
    // x
    f[88] = [0x44, 0x28, 0x10, 0x28, 0x44];
    // y
    f[89] = [0x0C, 0x50, 0x50, 0x50, 0x3C];
    // z
    f[90] = [0x44, 0x64, 0x54, 0x4C, 0x44];
    // {
    f[91] = [0x00, 0x08, 0x36, 0x41, 0x00];
    // |
    f[92] = [0x00, 0x00, 0x7F, 0x00, 0x00];
    // }
    f[93] = [0x00, 0x41, 0x36, 0x08, 0x00];
    // ~
    f[94] = [0x10, 0x08, 0x08, 0x10, 0x08];
    // DEL (blank)
    f[95] = [0x00, 0x00, 0x00, 0x00, 0x00];
    f
};

/// Pack RGB into softbuffer u32 format: 0x00RRGGBB.
pub fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) << 16 | (g as u32) << 8 | b as u32
}

/// Unpack softbuffer u32 into (r, g, b).
fn unpack_rgb(v: u32) -> (u8, u8, u8) {
    ((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

/// Draw one character at (px, py) with the given scale into a u32 pixel buffer.
/// `stride` is the framebuffer width in pixels.
fn draw_char(buf: &mut [u32], stride: u32, buf_h: u32, ch: char, px: i32, py: i32, scale: u32, color: (u8, u8, u8, u8)) {
    let idx = (ch as u32).wrapping_sub(32) as usize;
    if idx >= 96 {
        return;
    }
    let glyph = &FONT_5X7[idx];
    let a = color.3 as u32;
    for col in 0..5u32 {
        let bits = glyph[col as usize];
        for row in 0..7u32 {
            if bits & (1 << row) != 0 {
                for sy in 0..scale {
                    for sx in 0..scale {
                        let x = px + (col * scale + sx) as i32;
                        let y = py + (row * scale + sy) as i32;
                        if x >= 0 && y >= 0 && (x as u32) < stride && (y as u32) < buf_h {
                            let off = (y as u32 * stride + x as u32) as usize;
                            let (dr, dg, db) = unpack_rgb(buf[off]);
                            let r = ((color.0 as u32 * a + dr as u32 * (255 - a)) / 255) as u8;
                            let g = ((color.1 as u32 * a + dg as u32 * (255 - a)) / 255) as u8;
                            let b = ((color.2 as u32 * a + db as u32 * (255 - a)) / 255) as u8;
                            buf[off] = rgb(r, g, b);
                        }
                    }
                }
            }
        }
    }
}

/// Draw a string. Returns the x position after the last character.
pub fn draw_text(buf: &mut [u32], stride: u32, buf_h: u32, text: &str, px: i32, py: i32, scale: u32, color: (u8, u8, u8, u8)) -> i32 {
    let mut x = px;
    for ch in text.chars() {
        draw_char(buf, stride, buf_h, ch, x, py, scale, color);
        x += (6 * scale) as i32; // 5 pixels + 1 spacing
    }
    x
}

/// Fill a rectangle with a color (with alpha blending).
pub fn fill_rect(buf: &mut [u32], stride: u32, buf_h: u32, rx: i32, ry: i32, rw: u32, rh: u32, color: (u8, u8, u8, u8)) {
    let a = color.3 as u32;
    for row in 0..rh {
        let y = ry + row as i32;
        if y < 0 || y as u32 >= buf_h {
            continue;
        }
        for col in 0..rw {
            let x = rx + col as i32;
            if x < 0 || x as u32 >= stride {
                continue;
            }
            let off = (y as u32 * stride + x as u32) as usize;
            let (dr, dg, db) = unpack_rgb(buf[off]);
            let r = ((color.0 as u32 * a + dr as u32 * (255 - a)) / 255) as u8;
            let g = ((color.1 as u32 * a + dg as u32 * (255 - a)) / 255) as u8;
            let b = ((color.2 as u32 * a + db as u32 * (255 - a)) / 255) as u8;
            buf[off] = rgb(r, g, b);
        }
    }
}

pub fn fit_scale(img_w: f32, img_h: f32, win_w: f32, win_h: f32) -> f32 {
    (win_w / img_w).min(win_h / img_h)
}

pub fn blit_scaled_rotated(
    dst: &mut [u32], dst_w: u32, dst_h: u32,
    src: &[u8], src_w: u32, src_h: u32,
    x0: f32, y0: f32, scale: f32,
    rotation: u8,
) {
    let (draw_w, draw_h) = if rotation % 2 == 1 {
        (src_h as f32 * scale, src_w as f32 * scale)
    } else {
        (src_w as f32 * scale, src_h as f32 * scale)
    };

    let dx_start = (x0.max(0.0)) as u32;
    let dy_start = (y0.max(0.0)) as u32;
    let dx_end = ((x0 + draw_w).ceil() as u32).min(dst_w);
    let dy_end = ((y0 + draw_h).ceil() as u32).min(dst_h);

    let inv_scale = 1.0 / scale;

    for dy in dy_start..dy_end {
        let vy = (dy as f32 - y0) * inv_scale;
        for dx in dx_start..dx_end {
            let vx = (dx as f32 - x0) * inv_scale;

            // Map (vx, vy) back to source coordinates based on rotation
            // Source dims are (src_w, src_h)
            // (vx, vy) are in the rotated space (0..draw_w/scale, 0..draw_h/scale)
            let (sx, sy) = match rotation {
                0 => (vx as u32, vy as u32),
                1 => ((src_w as f32 - 1.0 - vy) as u32, vx as u32), // 90 CCW
                2 => ((src_w as f32 - 1.0 - vx) as u32, (src_h as f32 - 1.0 - vy) as u32), // 180
                3 => (vy as u32, (src_h as f32 - 1.0 - vx) as u32), // 270 CCW (90 CW)
                _ => (vx as u32, vy as u32),
            };

            if sx >= src_w || sy >= src_h {
                continue;
            }

            let si = (sy as usize * src_w as usize + sx as usize) * 4;
            let di = dy as usize * dst_w as usize + dx as usize;

            // ... pixel copy ...
            let sa = src[si + 3] as u32;
            if sa == 255 {
                dst[di] = rgb(src[si], src[si + 1], src[si + 2]);
            } else if sa > 0 {
                let inv = 255 - sa;
                let (dr, dg, db) = unpack_rgb(dst[di]);
                let r = ((src[si] as u32 * sa + dr as u32 * inv) / 255) as u8;
                let g = ((src[si + 1] as u32 * sa + dg as u32 * inv) / 255) as u8;
                let b = ((src[si + 2] as u32 * sa + db as u32 * inv) / 255) as u8;
                dst[di] = rgb(r, g, b);
            }
        }
    }
}
