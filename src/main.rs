use clap::Parser;
use image::GenericImageView;
use macroquad::prelude::*;
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const ZOOM_FACTOR: f32 = 0.25;
const PREFETCH_COUNT: usize = 3;
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
    pending: std::collections::HashSet<usize>,
}

impl ImageCache {
    fn new(budget: u64) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            used_bytes: 0,
            budget,
            pending: std::collections::HashSet::new(),
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
// GPU texture helper
// ---------------------------------------------------------------------------

fn upload_texture(decoded: &DecodedImage) -> Texture2D {
    let tex = Texture2D::from_rgba8(
        decoded.width as u16,
        decoded.height as u16,
        &decoded.rgba_bytes,
    );
    tex.set_filter(FilterMode::Linear);
    tex
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn window_conf() -> Conf {
    Conf {
        window_title: "iv".to_string(),
        window_resizable: true,
        platform: miniquad::conf::Platform {
            swap_interval: Some(1), // vsync — prevents busy-loop rendering
            ..Default::default()
        },
        ..Default::default()
    }
}

#[macroquad::main(window_conf)]
async fn main() {
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

    let mut current_index: usize = 0;
    let mut current_texture: Option<Texture2D> = None;
    let mut current_decoded: Option<Arc<DecodedImage>> = None;
    let mut error_message: Option<String> = None;

    let mut zoom: f32 = 0.0; // 0.0 means "fit to window"
    let mut offset_x: f32 = 0.0;
    let mut offset_y: f32 = 0.0;
    let mut show_info = false;
    let mut is_fullscreen = false;
    let mut dragging = false;
    let mut drag_start = (0.0f32, 0.0f32);
    let mut drag_offset_start = (0.0f32, 0.0f32);
    let mut needs_load = true;
    let initial_delay = cli.initial_delay as f64 / 1000.0;
    let repeat_delay = cli.repeat_delay as f64 / 1000.0;
    let mut nav_hold_timer: f64 = 0.0;
    let mut nav_past_initial = false;

    // Kick off initial loads
    request_load(&cache, &files, 0);
    prefetch(&cache, &files, 0);

    loop {
        // ------------------------------------------------------------------
        // Input handling
        // ------------------------------------------------------------------

        // Quit
        if is_key_pressed(KeyCode::Escape)
            || is_key_pressed(KeyCode::Q)
            || is_key_pressed(KeyCode::E)
        {
            break;
        }

        // Navigation
        let mut nav = 0i32;
        let fwd_down = is_key_down(KeyCode::Right)
            || is_key_down(KeyCode::Space)
            || is_key_down(KeyCode::L);
        let bwd_down = is_key_down(KeyCode::Left) || is_key_down(KeyCode::H);
        let fwd_pressed = is_key_pressed(KeyCode::Right)
            || is_key_pressed(KeyCode::Space)
            || is_key_pressed(KeyCode::L);
        let bwd_pressed = is_key_pressed(KeyCode::Left) || is_key_pressed(KeyCode::H);

        if fwd_pressed || bwd_pressed {
            nav = if fwd_pressed { 1 } else { -1 };
            nav_hold_timer = 0.0;
            nav_past_initial = false;
        } else if fwd_down || bwd_down {
            nav_hold_timer += get_frame_time() as f64;
            if !nav_past_initial {
                if nav_hold_timer >= initial_delay {
                    nav = if fwd_down { 1 } else { -1 };
                    nav_hold_timer = 0.0;
                    nav_past_initial = true;
                }
            } else if nav_hold_timer >= repeat_delay {
                nav = if fwd_down { 1 } else { -1 };
                nav_hold_timer -= repeat_delay;
            }
        } else {
            nav_hold_timer = 0.0;
            nav_past_initial = false;
        }
        if nav != 0 {
            let new_idx = (current_index as i64 + nav as i64)
                .clamp(0, files.len() as i64 - 1) as usize;
            if new_idx != current_index {
                current_index = new_idx;
                needs_load = true;
                zoom = 0.0; // reset to fit
                offset_x = 0.0;
                offset_y = 0.0;
                error_message = None;
            }
            prefetch(&cache, &files, current_index);
        }

        // Toggle info
        if is_key_pressed(KeyCode::I) {
            show_info = !show_info;
        }

        // Fullscreen toggle
        if is_key_pressed(KeyCode::F) {
            is_fullscreen = !is_fullscreen;
            set_fullscreen(is_fullscreen);
            zoom = 0.0;
            offset_x = 0.0;
            offset_y = 0.0;
        }

        // Zoom: r = fit to window, z = 1:1
        if is_key_pressed(KeyCode::R) {
            zoom = 0.0;
            offset_x = 0.0;
            offset_y = 0.0;
        }
        if is_key_pressed(KeyCode::Z) {
            if zoom == 1.0 {
                zoom = 0.0; // back to fit-to-window
            } else {
                zoom = 1.0; // 1:1 pixel
            }
            offset_x = 0.0;
            offset_y = 0.0;
        }

        // Zoom in/out with = / -
        let zoom_in = is_key_pressed(KeyCode::Equal);
        let zoom_out = is_key_pressed(KeyCode::Minus);
        let wheel = mouse_wheel().1;
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
            if let Some(ref dec) = current_decoded {
                let sw = screen_width();
                let sh = screen_height();
                let old_zoom = if zoom == 0.0 {
                    fit_scale(dec.width as f32, dec.height as f32, sw, sh)
                } else {
                    zoom
                };
                let new_zoom = (old_zoom + zoom_delta).max(0.01);

                // Zoom toward mouse position (or image center if mouse outside window)
                let (mx, my) = mouse_position();
                let anchor_x = if mx >= 0.0 && mx <= sw { mx } else { sw / 2.0 };
                let anchor_y = if my >= 0.0 && my <= sh { my } else { sh / 2.0 };

                // Image point under anchor before zoom
                let img_w = dec.width as f32;
                let img_h = dec.height as f32;
                let old_dw = img_w * old_zoom;
                let old_dh = img_h * old_zoom;
                let old_x0 = (sw - old_dw) / 2.0 + offset_x;
                let old_y0 = (sh - old_dh) / 2.0 + offset_y;
                let img_px = (anchor_x - old_x0) / old_zoom;
                let img_py = (anchor_y - old_y0) / old_zoom;

                // Where that image point ends up after zoom
                let new_dw = img_w * new_zoom;
                let new_dh = img_h * new_zoom;
                let new_x0 = (sw - new_dw) / 2.0;
                let new_y0 = (sh - new_dh) / 2.0;
                offset_x = anchor_x - new_x0 - img_px * new_zoom;
                offset_y = anchor_y - new_y0 - img_py * new_zoom;

                zoom = new_zoom;
            }
        }

        // Mouse drag to pan
        if is_mouse_button_pressed(MouseButton::Left) {
            dragging = true;
            let (mx, my) = mouse_position();
            drag_start = (mx, my);
            drag_offset_start = (offset_x, offset_y);
        }
        if is_mouse_button_released(MouseButton::Left) {
            dragging = false;
        }
        if dragging {
            let (mx, my) = mouse_position();
            offset_x = drag_offset_start.0 + (mx - drag_start.0);
            offset_y = drag_offset_start.1 + (my - drag_start.1);
        }

        // ------------------------------------------------------------------
        // Load current image if needed
        // ------------------------------------------------------------------

        if needs_load {
            current_texture = None;
            current_decoded = None;

            request_load(&cache, &files, current_index);

            let maybe = {
                let mut c = cache.lock().unwrap();
                c.get(current_index)
            };
            if let Some(dec) = maybe {
                current_texture = Some(upload_texture(&dec));
                current_decoded = Some(dec);
                needs_load = false;
                error_message = None;
            }
            // If not in cache yet, we'll try again next frame
        }

        // Try to pick up a cached image that was loading in the background
        if needs_load && error_message.is_none() {
            let maybe = {
                let mut c = cache.lock().unwrap();
                c.get(current_index)
            };
            match maybe {
                Some(dec) => {
                    current_texture = Some(upload_texture(&dec));
                    current_decoded = Some(dec);
                    needs_load = false;
                }
                None => {
                    // Check if it's no longer pending (i.e. failed)
                    let c = cache.lock().unwrap();
                    if !c.is_pending(current_index) && !c.contains(current_index) {
                        // Try a synchronous decode as fallback
                        drop(c);
                        match decode_image(&files[current_index]) {
                            Ok(decoded) => {
                                let arc = {
                                    let mut c = cache.lock().unwrap();
                                    c.insert(current_index, decoded);
                                    c.get(current_index).unwrap()
                                };
                                current_texture = Some(upload_texture(&arc));
                                current_decoded = Some(arc);
                                needs_load = false;
                            }
                            Err(e) => {
                                error_message = Some(format!(
                                    "Could not load: {}\n{}",
                                    files[current_index].display(),
                                    e
                                ));
                                needs_load = false;
                            }
                        }
                    }
                }
            }
        }

        // ------------------------------------------------------------------
        // Render
        // ------------------------------------------------------------------

        clear_background(Color::new(0.12, 0.12, 0.12, 1.0));

        let sw = screen_width();
        let sh = screen_height();

        if let (Some(ref tex), Some(ref dec)) = (&current_texture, &current_decoded) {
            let img_w = dec.width as f32;
            let img_h = dec.height as f32;

            let scale = if zoom == 0.0 {
                fit_scale(img_w, img_h, sw, sh)
            } else {
                zoom
            };

            let draw_w = img_w * scale;
            let draw_h = img_h * scale;
            let x = (sw - draw_w) / 2.0 + offset_x;
            let y = (sh - draw_h) / 2.0 + offset_y;

            draw_texture_ex(
                tex,
                x,
                y,
                WHITE,
                DrawTextureParams {
                    dest_size: Some(Vec2::new(draw_w, draw_h)),
                    ..Default::default()
                },
            );

            // Info overlay
            if show_info {
                let display_zoom = if zoom == 0.0 {
                    fit_scale(img_w, img_h, sw, sh) * 100.0
                } else {
                    zoom * 100.0
                };
                let raw_size = (dec.width as u64) * (dec.height as u64) * 4;
                let ratio = if raw_size > 0 {
                    dec.file_size as f64 / raw_size as f64
                } else {
                    0.0
                };
                let line1 = format!(
                    "[{}/{}]",
                    current_index + 1,
                    files.len(),
                );
                let line2 = format!(
                    "{}",
                    files[current_index].display(),
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
                let font_size = 30.0;
                let line_h = font_size + 4.0;
                let bar_h = line_h * 3.0 + 8.0;
                draw_rectangle(0.0, 0.0, sw, bar_h, Color::new(0.0, 0.0, 0.0, 0.7));
                draw_text(&line1, 10.0, line_h, font_size, WHITE);
                draw_text(&line2, 10.0, line_h * 2.0, font_size, WHITE);
                draw_text(&line3, 10.0, line_h * 3.0, font_size, WHITE);
            }
        } else if let Some(ref err) = error_message {
            draw_text(err, 20.0, sh / 2.0, 24.0, RED);
        } else if needs_load {
            draw_text("Loading...", sw / 2.0 - 50.0, sh / 2.0, 24.0, WHITE);
        }

        next_frame().await;
    }
}

fn fit_scale(img_w: f32, img_h: f32, win_w: f32, win_h: f32) -> f32 {
    (win_w / img_w).min(win_h / img_h)
}
