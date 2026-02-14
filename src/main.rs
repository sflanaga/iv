use clap::Parser;
use image::GenericImageView;
use softbuffer::Surface;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const ZOOM_FACTOR: f32 = 0.25;
const PREFETCH_COUNT: usize = 3;
const BG_COLOR: [u8; 4] = [31, 31, 31, 255]; // ~0.12 * 255
const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "bmp", "tga", "tiff", "tif", "webp", "ico", "pnm", "pbm",
    "pgm", "ppm", "pam", "dds", "hdr", "exr", "ff", "qoi",
];

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "iv", about = "A simple image viewer")]
struct Cli {
    /// Files or directories to view
    #[arg(required = true)]
    paths: Vec<PathBuf>,

    /// Memory budget for image cache (e.g. 512MB, 2GB). Default: 10% of RAM.
    #[arg(short, long)]
    memory: Option<String>,

    /// Recurse into subdirectories
    #[arg(short, long)]
    recursive: bool,

    /// Initial delay in ms before key-hold repeat begins (default: 500)
    #[arg(long, default_value = "500")]
    initial_delay: u64,

    /// Key-hold repeat interval in milliseconds for navigation (default: 35)
    #[arg(long, default_value = "35")]
    repeat_delay: u64,
}

fn parse_memory_budget(s: &str) -> u64 {
    let s = s.trim().to_uppercase();
    if let Some(num) = s.strip_suffix("GB") {
        num.trim().parse::<f64>().unwrap_or(1.0) as u64 * 1024 * 1024 * 1024
    } else if let Some(num) = s.strip_suffix("MB") {
        num.trim().parse::<f64>().unwrap_or(512.0) as u64 * 1024 * 1024
    } else {
        s.parse::<f64>().unwrap_or(512.0) as u64 * 1024 * 1024
    }
}

fn default_memory_budget() -> u64 {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    sys.total_memory() / 10
}

// ---------------------------------------------------------------------------
// File scanning
// ---------------------------------------------------------------------------

fn is_image_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

fn collect_images(paths: &[PathBuf], recursive: bool) -> Vec<PathBuf> {
    let mut result = Vec::new();
    for path in paths {
        if path.is_dir() {
            scan_dir(path, recursive, &mut result);
        } else if path.is_file() && is_image_file(path) {
            result.push(path.clone());
        }
    }
    result
}

fn scan_dir(dir: &Path, recursive: bool, result: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut files = Vec::new();
    let mut subdirs = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let p = entry.path();
        if p.is_file() && is_image_file(&p) {
            files.push(p);
        } else if recursive && p.is_dir() {
            subdirs.push(p);
        }
    }
    files.sort();
    result.extend(files);
    if recursive {
        subdirs.sort();
        for sub in subdirs {
            scan_dir(&sub, true, result);
        }
    }
}

// ---------------------------------------------------------------------------
// Decoded image data (CPU side, before GPU upload)
// ---------------------------------------------------------------------------

struct DecodedImage {
    rgba_bytes: Vec<u8>,
    width: u32,
    height: u32,
    file_size: u64,
    format_name: String,
}

impl DecodedImage {
    fn mem_size(&self) -> u64 {
        self.rgba_bytes.len() as u64
    }
}

fn decode_image(path: &Path) -> Result<DecodedImage, String> {
    let file_size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let img = image::open(path).map_err(|e| format!("{}", e))?;
    let (width, height) = img.dimensions();
    let format_name = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("unknown")
        .to_uppercase();
    let rgba = img.to_rgba8();
    Ok(DecodedImage {
        rgba_bytes: rgba.into_raw(),
        width,
        height,
        file_size,
        format_name,
    })
}

// ---------------------------------------------------------------------------
// LRU Image Cache (thread-safe, CPU-decoded images)
// ---------------------------------------------------------------------------

struct CacheEntry {
    decoded: Arc<DecodedImage>,
}

struct ImageCache {
    map: HashMap<usize, CacheEntry>,
    order: VecDeque<usize>,  // front = least recently used
    used_bytes: u64,
    budget: u64,
    pending: HashSet<usize>,
}

impl ImageCache {
    fn new(budget: u64) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            used_bytes: 0,
            budget,
            pending: HashSet::new(),
        }
    }

    fn get(&mut self, idx: usize) -> Option<Arc<DecodedImage>> {
        if self.map.contains_key(&idx) {
            self.order.retain(|&i| i != idx);
            self.order.push_back(idx);
            Some(Arc::clone(&self.map[&idx].decoded))
        } else {
            None
        }
    }

    fn insert(&mut self, idx: usize, decoded: DecodedImage) {
        let mem = decoded.mem_size();
        let arc = Arc::new(decoded);
        // Evict until we have room
        while self.used_bytes + mem > self.budget && !self.order.is_empty() {
            if let Some(evict_idx) = self.order.pop_front() {
                if let Some(entry) = self.map.remove(&evict_idx) {
                    self.used_bytes -= entry.decoded.mem_size();
                }
            }
        }
        self.used_bytes += mem;
        self.map.insert(idx, CacheEntry { decoded: arc });
        self.order.retain(|&i| i != idx);
        self.order.push_back(idx);
        self.pending.remove(&idx);
    }

    fn contains(&self, idx: usize) -> bool {
        self.map.contains_key(&idx)
    }

    fn is_pending(&self, idx: usize) -> bool {
        self.pending.contains(&idx)
    }

    fn mark_pending(&mut self, idx: usize) {
        self.pending.insert(idx);
    }
}

type SharedCache = Arc<Mutex<ImageCache>>;

// ---------------------------------------------------------------------------
// Background loader
// ---------------------------------------------------------------------------

fn request_load(cache: &SharedCache, files: &[PathBuf], idx: usize) {
    let mut c = cache.lock().unwrap();
    if c.contains(idx) || c.is_pending(idx) || idx >= files.len() {
        return;
    }
    c.mark_pending(idx);
    let cache2 = Arc::clone(cache);
    let path = files[idx].clone();
    thread::spawn(move || {
        let result = decode_image(&path);
        let mut c = cache2.lock().unwrap();
        c.pending.remove(&idx);
        if let Ok(decoded) = result {
            c.insert(idx, decoded);
        }
    });
}

fn prefetch(cache: &SharedCache, files: &[PathBuf], current: usize) {
    // Prefetch forward
    for i in 1..=PREFETCH_COUNT {
        let idx = current + i;
        if idx < files.len() {
            request_load(cache, files, idx);
        }
    }
    // Prefetch one backward
    if current > 0 {
        request_load(cache, files, current - 1);
    }
}

// ---------------------------------------------------------------------------
// Tiny software text renderer (built-in bitmap font, no GPU text needed)
// ---------------------------------------------------------------------------

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
fn rgb(r: u8, g: u8, b: u8) -> u32 {
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
fn draw_text(buf: &mut [u32], stride: u32, buf_h: u32, text: &str, px: i32, py: i32, scale: u32, color: (u8, u8, u8, u8)) -> i32 {
    let mut x = px;
    for ch in text.chars() {
        draw_char(buf, stride, buf_h, ch, x, py, scale, color);
        x += (6 * scale) as i32; // 5 pixels + 1 spacing
    }
    x
}

/// Fill a rectangle with a color (with alpha blending).
fn fill_rect(buf: &mut [u32], stride: u32, buf_h: u32, rx: i32, ry: i32, rw: u32, rh: u32, color: (u8, u8, u8, u8)) {
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

// ---------------------------------------------------------------------------
// Viewer state
// ---------------------------------------------------------------------------

struct ViewerState {
    files: Vec<PathBuf>,
    cache: SharedCache,
    current_index: usize,
    current_decoded: Option<Arc<DecodedImage>>,
    error_message: Option<String>,

    zoom: f32, // 0.0 = fit to window
    offset_x: f32,
    offset_y: f32,
    show_info: bool,
    is_fullscreen: bool,
    dragging: bool,
    drag_start: (f64, f64),
    drag_offset_start: (f32, f32),
    needs_load: bool,
    mouse_pos: (f64, f64),

    // Key-hold repeat state
    initial_delay: f64,
    repeat_delay: f64,
    nav_hold_timer: f64,
    nav_past_initial: bool,
    last_frame: Instant,

    // Track which keys are currently held
    keys_down: HashSet<NamedKey>,
    chars_down: HashSet<char>,

    // Track keys that were just pressed this frame
    keys_pressed: HashSet<NamedKey>,
    chars_pressed: HashSet<char>,

    // Mouse wheel accumulator for this frame
    wheel_y: f32,
}

impl ViewerState {
    fn new(
        files: Vec<PathBuf>,
        cache: SharedCache,
        initial_delay: f64,
        repeat_delay: f64,
    ) -> Self {
        Self {
            files,
            cache,
            current_index: 0,
            current_decoded: None,
            error_message: None,
            zoom: 0.0,
            offset_x: 0.0,
            offset_y: 0.0,
            show_info: false,
            is_fullscreen: false,
            dragging: false,
            drag_start: (0.0, 0.0),
            drag_offset_start: (0.0, 0.0),
            needs_load: true,
            mouse_pos: (0.0, 0.0),
            initial_delay,
            repeat_delay,
            nav_hold_timer: 0.0,
            nav_past_initial: false,
            last_frame: Instant::now(),
            keys_down: HashSet::new(),
            chars_down: HashSet::new(),
            keys_pressed: HashSet::new(),
            chars_pressed: HashSet::new(),
            wheel_y: 0.0,
        }
    }

    fn is_key_pressed_named(&self, k: NamedKey) -> bool {
        self.keys_pressed.contains(&k)
    }

    fn is_char_pressed(&self, c: char) -> bool {
        self.chars_pressed.contains(&c)
    }

    fn is_key_down_named(&self, k: NamedKey) -> bool {
        self.keys_down.contains(&k)
    }

    fn is_char_down(&self, c: char) -> bool {
        self.chars_down.contains(&c)
    }

    /// Run the per-frame logic: input handling, cache polling, etc.
    /// Returns true if the app should quit.
    fn update(&mut self, window: &Window) -> bool {
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f64();
        self.last_frame = now;

        // ------------------------------------------------------------------
        // Quit
        // ------------------------------------------------------------------
        if self.is_key_pressed_named(NamedKey::Escape)
            || self.is_char_pressed('q')
            || self.is_char_pressed('e')
        {
            return true;
        }

        // ------------------------------------------------------------------
        // Navigation
        // ------------------------------------------------------------------
        let mut nav = 0i32;
        let fwd_down = self.is_key_down_named(NamedKey::ArrowRight)
            || self.is_key_down_named(NamedKey::Space)
            || self.is_char_down('l');
        let bwd_down = self.is_key_down_named(NamedKey::ArrowLeft)
            || self.is_char_down('h');
        let fwd_pressed = self.is_key_pressed_named(NamedKey::ArrowRight)
            || self.is_key_pressed_named(NamedKey::Space)
            || self.is_char_pressed('l');
        let bwd_pressed = self.is_key_pressed_named(NamedKey::ArrowLeft)
            || self.is_char_pressed('h');

        if fwd_pressed || bwd_pressed {
            nav = if fwd_pressed { 1 } else { -1 };
            self.nav_hold_timer = 0.0;
            self.nav_past_initial = false;
        } else if fwd_down || bwd_down {
            self.nav_hold_timer += dt;
            if !self.nav_past_initial {
                if self.nav_hold_timer >= self.initial_delay {
                    nav = if fwd_down { 1 } else { -1 };
                    self.nav_hold_timer = 0.0;
                    self.nav_past_initial = true;
                }
            } else if self.nav_hold_timer >= self.repeat_delay {
                nav = if fwd_down { 1 } else { -1 };
                self.nav_hold_timer -= self.repeat_delay;
            }
        } else {
            self.nav_hold_timer = 0.0;
            self.nav_past_initial = false;
        }

        if nav != 0 {
            let new_idx = (self.current_index as i64 + nav as i64)
                .clamp(0, self.files.len() as i64 - 1) as usize;
            if new_idx != self.current_index {
                self.current_index = new_idx;
                self.needs_load = true;
                self.zoom = 0.0;
                self.offset_x = 0.0;
                self.offset_y = 0.0;
                self.error_message = None;
            }
            prefetch(&self.cache, &self.files, self.current_index);
        }

        // ------------------------------------------------------------------
        // Toggle info
        // ------------------------------------------------------------------
        if self.is_char_pressed('i') {
            self.show_info = !self.show_info;
        }

        // ------------------------------------------------------------------
        // Fullscreen toggle
        // ------------------------------------------------------------------
        if self.is_char_pressed('f') {
            self.is_fullscreen = !self.is_fullscreen;
            if self.is_fullscreen {
                window.set_fullscreen(Some(Fullscreen::Borderless(None)));
            } else {
                window.set_fullscreen(None);
            }
            self.zoom = 0.0;
            self.offset_x = 0.0;
            self.offset_y = 0.0;
        }

        // ------------------------------------------------------------------
        // Zoom: r = fit to window, z = 1:1
        // ------------------------------------------------------------------
        if self.is_char_pressed('r') {
            self.zoom = 0.0;
            self.offset_x = 0.0;
            self.offset_y = 0.0;
        }
        if self.is_char_pressed('z') {
            if self.zoom == 1.0 {
                self.zoom = 0.0;
            } else {
                self.zoom = 1.0;
            }
            self.offset_x = 0.0;
            self.offset_y = 0.0;
        }

        // ------------------------------------------------------------------
        // Zoom in/out with = / - / mouse wheel
        // ------------------------------------------------------------------
        let zoom_in = self.is_char_pressed('=') || self.is_char_pressed('+');
        let zoom_out = self.is_char_pressed('-');
        let wheel = self.wheel_y;
        let zoom_delta = if zoom_in {
            ZOOM_FACTOR
        } else if zoom_out {
            -ZOOM_FACTOR
        } else if wheel.abs() > 0.1 {
            wheel.signum() * ZOOM_FACTOR
        } else {
            0.0
        };

        if zoom_delta != 0.0 {
            if let Some(ref dec) = self.current_decoded {
                let size = window.inner_size();
                let sw = size.width as f32;
                let sh = size.height as f32;
                let old_zoom = if self.zoom == 0.0 {
                    fit_scale(dec.width as f32, dec.height as f32, sw, sh)
                } else {
                    self.zoom
                };
                let new_zoom = (old_zoom + zoom_delta).max(0.01);

                // Zoom toward mouse position (or image center if mouse outside window)
                let (mx, my) = (self.mouse_pos.0 as f32, self.mouse_pos.1 as f32);
                let anchor_x = if mx >= 0.0 && mx <= sw { mx } else { sw / 2.0 };
                let anchor_y = if my >= 0.0 && my <= sh { my } else { sh / 2.0 };

                // Image point under anchor before zoom
                let img_w = dec.width as f32;
                let img_h = dec.height as f32;
                let old_dw = img_w * old_zoom;
                let old_dh = img_h * old_zoom;
                let old_x0 = (sw - old_dw) / 2.0 + self.offset_x;
                let old_y0 = (sh - old_dh) / 2.0 + self.offset_y;
                let img_px = (anchor_x - old_x0) / old_zoom;
                let img_py = (anchor_y - old_y0) / old_zoom;

                // Where that image point ends up after zoom
                let new_dw = img_w * new_zoom;
                let new_dh = img_h * new_zoom;
                let new_x0 = (sw - new_dw) / 2.0;
                let new_y0 = (sh - new_dh) / 2.0;
                self.offset_x = anchor_x - new_x0 - img_px * new_zoom;
                self.offset_y = anchor_y - new_y0 - img_py * new_zoom;

                self.zoom = new_zoom;
            }
        }

        // ------------------------------------------------------------------
        // Load current image if needed
        // ------------------------------------------------------------------
        if self.needs_load {
            self.current_decoded = None;

            request_load(&self.cache, &self.files, self.current_index);

            let maybe = {
                let mut c = self.cache.lock().unwrap();
                c.get(self.current_index)
            };
            if let Some(dec) = maybe {
                self.current_decoded = Some(dec);
                self.needs_load = false;
                self.error_message = None;
            }
        }

        // Try to pick up a cached image that was loading in the background
        if self.needs_load && self.error_message.is_none() {
            let maybe = {
                let mut c = self.cache.lock().unwrap();
                c.get(self.current_index)
            };
            match maybe {
                Some(dec) => {
                    self.current_decoded = Some(dec);
                    self.needs_load = false;
                }
                None => {
                    // Check if it's no longer pending (i.e. failed)
                    let c = self.cache.lock().unwrap();
                    if !c.is_pending(self.current_index) && !c.contains(self.current_index) {
                        drop(c);
                        match decode_image(&self.files[self.current_index]) {
                            Ok(decoded) => {
                                let arc = {
                                    let mut c = self.cache.lock().unwrap();
                                    c.insert(self.current_index, decoded);
                                    c.get(self.current_index).unwrap()
                                };
                                self.current_decoded = Some(arc);
                                self.needs_load = false;
                            }
                            Err(e) => {
                                self.error_message = Some(format!(
                                    "Could not load: {} {}",
                                    self.files[self.current_index].display(),
                                    e
                                ));
                                self.needs_load = false;
                            }
                        }
                    }
                }
            }
        }

        // Clear per-frame input state
        self.keys_pressed.clear();
        self.chars_pressed.clear();
        self.wheel_y = 0.0;

        false
    }

    /// Render into the softbuffer framebuffer (u32 per pixel, 0x00RRGGBB).
    fn render(&self, frame: &mut [u32], fb_w: u32, fb_h: u32) {
        // Clear to background color
        let bg = rgb(BG_COLOR[0], BG_COLOR[1], BG_COLOR[2]);
        frame.fill(bg);

        let sw = fb_w as f32;
        let sh = fb_h as f32;

        if let Some(ref dec) = self.current_decoded {
            let img_w = dec.width as f32;
            let img_h = dec.height as f32;

            let scale = if self.zoom == 0.0 {
                fit_scale(img_w, img_h, sw, sh)
            } else {
                self.zoom
            };

            let draw_w = img_w * scale;
            let draw_h = img_h * scale;
            let x0 = (sw - draw_w) / 2.0 + self.offset_x;
            let y0 = (sh - draw_h) / 2.0 + self.offset_y;

            blit_scaled(
                frame, fb_w, fb_h,
                &dec.rgba_bytes, dec.width, dec.height,
                x0, y0, scale,
            );

            // Info overlay
            if self.show_info {
                let display_zoom = if self.zoom == 0.0 {
                    fit_scale(img_w, img_h, sw, sh) * 100.0
                } else {
                    self.zoom * 100.0
                };
                let raw_size = (dec.width as u64) * (dec.height as u64) * 4;
                let ratio = if raw_size > 0 {
                    dec.file_size as f64 / raw_size as f64
                } else {
                    0.0
                };
                let line1 = format!(
                    "[{}/{}]",
                    self.current_index + 1,
                    self.files.len(),
                );
                let line2 = format!(
                    "{}",
                    self.files[self.current_index].display(),
                );
                let line3 = format!(
                    "{}x{} | {} | {:.1} KB | ratio {:.2} | zoom {:.0}%",
                    dec.width,
                    dec.height,
                    dec.format_name,
                    dec.file_size as f64 / 1024.0,
                    ratio,
                    display_zoom,
                );
                let text_scale: u32 = 2;
                let line_h = (7 * text_scale + 4) as i32;
                let bar_h = (line_h * 3 + 8) as u32;
                fill_rect(frame, fb_w, fb_h, 0, 0, fb_w, bar_h, (0, 0, 0, 178));
                let white = (255, 255, 255, 255);
                draw_text(frame, fb_w, fb_h, &line1, 10, 4, text_scale, white);
                draw_text(frame, fb_w, fb_h, &line2, 10, 4 + line_h, text_scale, white);
                draw_text(frame, fb_w, fb_h, &line3, 10, 4 + line_h * 2, text_scale, white);
            }
        } else if let Some(ref err) = self.error_message {
            let text_scale: u32 = 2;
            draw_text(frame, fb_w, fb_h, err, 20, fb_h as i32 / 2, text_scale, (255, 80, 80, 255));
        } else if self.needs_load {
            let text_scale: u32 = 2;
            let tx = (fb_w as i32) / 2 - 30;
            draw_text(frame, fb_w, fb_h, "Loading...", tx, fb_h as i32 / 2, text_scale, (255, 255, 255, 255));
        }
    }
}

// ---------------------------------------------------------------------------
// Scaled blit (nearest-neighbor for simplicity and speed)
// ---------------------------------------------------------------------------

fn blit_scaled(
    dst: &mut [u32], dst_w: u32, dst_h: u32,
    src: &[u8], src_w: u32, src_h: u32,
    x0: f32, y0: f32, scale: f32,
) {
    // Determine destination pixel range (clipped to framebuffer)
    let draw_w = src_w as f32 * scale;
    let draw_h = src_h as f32 * scale;
    let dx_start = (x0.max(0.0)) as u32;
    let dy_start = (y0.max(0.0)) as u32;
    let dx_end = ((x0 + draw_w).ceil() as u32).min(dst_w);
    let dy_end = ((y0 + draw_h).ceil() as u32).min(dst_h);

    let inv_scale = 1.0 / scale;

    for dy in dy_start..dy_end {
        let sy = ((dy as f32 - y0) * inv_scale) as u32;
        if sy >= src_h {
            continue;
        }
        let src_row = (sy * src_w) as usize;
        let dst_row = (dy * dst_w) as usize;
        for dx in dx_start..dx_end {
            let sx = ((dx as f32 - x0) * inv_scale) as u32;
            if sx >= src_w {
                continue;
            }
            let si = (src_row + sx as usize) * 4;
            let di = dst_row + dx as usize;
            // Source is RGBA u8, destination is 0x00RRGGBB u32
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

// ---------------------------------------------------------------------------
// Application handler (winit 0.30 style)
// ---------------------------------------------------------------------------

struct App {
    state: ViewerState,
    window: Option<Arc<Window>>,
    context: Option<softbuffer::Context<Arc<Window>>>,
    surface: Option<Surface<Arc<Window>, Arc<Window>>>,
    next_redraw: Option<Instant>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("iv")
            .with_inner_size(LogicalSize::new(1280u32, 720u32));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        let context = softbuffer::Context::new(Arc::clone(&window)).expect("create context");
        let surface = Surface::new(&context, Arc::clone(&window)).expect("create surface");

        self.window = Some(window);
        self.context = Some(context);
        self.surface = Some(surface);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }

            WindowEvent::Resized(PhysicalSize { width, height }) => {
                let w = width.max(1);
                let h = height.max(1);
                if let Some(ref mut surface) = self.surface {
                    let _ = surface.resize(
                        std::num::NonZeroU32::new(w).unwrap(),
                        std::num::NonZeroU32::new(h).unwrap(),
                    );
                }
                if let Some(ref window) = self.window {
                    window.request_redraw();
                }
            }

            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = event.state == ElementState::Pressed;
                match &event.logical_key {
                    Key::Named(named) => {
                        if pressed {
                            if !event.repeat {
                                self.state.keys_pressed.insert(*named);
                            }
                            self.state.keys_down.insert(*named);
                        } else {
                            self.state.keys_down.remove(named);
                        }
                    }
                    Key::Character(s) => {
                        if let Some(c) = s.chars().next() {
                            let c = c.to_ascii_lowercase();
                            if pressed {
                                if !event.repeat {
                                    self.state.chars_pressed.insert(c);
                                }
                                self.state.chars_down.insert(c);
                            } else {
                                self.state.chars_down.remove(&c);
                            }
                        }
                    }
                    _ => {}
                }
                if let Some(ref window) = self.window {
                    window.request_redraw();
                }
            }

            WindowEvent::MouseInput { state, button, .. } => {
                if button == MouseButton::Left {
                    if state == ElementState::Pressed {
                        self.state.dragging = true;
                        self.state.drag_start = self.state.mouse_pos;
                        self.state.drag_offset_start =
                            (self.state.offset_x, self.state.offset_y);
                    } else {
                        self.state.dragging = false;
                    }
                }
                if let Some(ref window) = self.window {
                    window.request_redraw();
                }
            }

            WindowEvent::CursorMoved {
                position: PhysicalPosition { x, y },
                ..
            } => {
                self.state.mouse_pos = (x, y);
                if self.state.dragging {
                    self.state.offset_x = self.state.drag_offset_start.0
                        + (x as f32 - self.state.drag_start.0 as f32);
                    self.state.offset_y = self.state.drag_offset_start.1
                        + (y as f32 - self.state.drag_start.1 as f32);
                    if let Some(ref window) = self.window {
                        window.request_redraw();
                    }
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let y = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(PhysicalPosition { y, .. }) => y as f32 / 40.0,
                };
                self.state.wheel_y += y;
                if let Some(ref window) = self.window {
                    window.request_redraw();
                }
            }

            WindowEvent::RedrawRequested => {
                let window = self.window.as_ref().unwrap();
                let quit = self.state.update(window);
                if quit {
                    event_loop.exit();
                    return;
                }

                if let Some(ref mut surface) = self.surface {
                    let size = window.inner_size();
                    let fb_w = size.width.max(1);
                    let fb_h = size.height.max(1);
                    if let Ok(mut buffer) = surface.buffer_mut() {
                        self.state.render(&mut buffer, fb_w, fb_h);
                        let _ = buffer.present();
                    }
                }

                // Schedule next redraw only if continuous updates are needed
                let nav_keys_held = self.state.is_key_down_named(NamedKey::ArrowRight)
                    || self.state.is_key_down_named(NamedKey::ArrowLeft)
                    || self.state.is_key_down_named(NamedKey::Space)
                    || self.state.is_char_down('l')
                    || self.state.is_char_down('h');

                if nav_keys_held {
                    let delay_ms = if !self.state.nav_past_initial {
                        (self.state.initial_delay * 1000.0) as u64
                    } else {
                        (self.state.repeat_delay * 1000.0) as u64
                    };
                    self.next_redraw = Some(Instant::now() + Duration::from_millis(delay_ms.max(1)));
                } else if self.state.needs_load {
                    self.next_redraw = Some(Instant::now() + Duration::from_millis(16));
                } else {
                    self.next_redraw = None;
                }
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(when) = self.next_redraw {
            if Instant::now() >= when {
                self.next_redraw = None;
                if let Some(ref window) = self.window {
                    window.request_redraw();
                }
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(when));
            }
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let cli = Cli::parse();

    let budget = match &cli.memory {
        Some(s) => parse_memory_budget(s),
        None => default_memory_budget(),
    };

    let files = collect_images(&cli.paths, cli.recursive);
    if files.is_empty() {
        eprintln!("No image files found.");
        return;
    }

    let cache: SharedCache = Arc::new(Mutex::new(ImageCache::new(budget)));

    // Kick off initial loads
    request_load(&cache, &files, 0);
    prefetch(&cache, &files, 0);

    let initial_delay = cli.initial_delay as f64 / 1000.0;
    let repeat_delay = cli.repeat_delay as f64 / 1000.0;

    let state = ViewerState::new(files, cache, initial_delay, repeat_delay);

    let event_loop = EventLoop::new().expect("create event loop");
    let mut app = App {
        state,
        window: None,
        context: None,
        surface: None,
        next_redraw: None,
    };

    event_loop.run_app(&mut app).expect("run event loop");
}

fn fit_scale(img_w: f32, img_h: f32, win_w: f32, win_h: f32) -> f32 {
    (win_w / img_w).min(win_h / img_h)
}
