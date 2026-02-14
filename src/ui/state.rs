use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Instant;
use winit::window::{Fullscreen, Window};
use winit::keyboard::NamedKey;

use crate::cli::HELP_KEYS;
use crate::loader::{DecodedImage, SharedState};
use crate::ui::render::{
    blit_scaled_rotated, draw_text, fill_rect, fit_scale, rgb, BG_COLOR,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const ZOOM_FACTOR: f32 = 0.25;

// ---------------------------------------------------------------------------
// Viewer state
// ---------------------------------------------------------------------------

pub struct ViewerState {
    pub files: Arc<RwLock<Vec<PathBuf>>>,
    pub shared: SharedState,
    pub current_index: usize,
    /// The index of the image currently stored in `current_decoded`.
    /// May differ from `current_index` if we are waiting for a load.
    pub displayed_index: usize,
    pub current_decoded: Option<Arc<DecodedImage>>,
    pub error_message: Option<String>,

    pub zoom: f32, // 0.0 = fit to window
    pub offset_x: f32,
    pub offset_y: f32,
    pub show_info: bool,
    pub is_fullscreen: bool,
    pub dragging: bool,
    pub drag_start: (f64, f64),
    pub drag_offset_start: (f32, f32),
    pub mouse_pos: (f64, f64),

    // Key-hold repeat state
    pub initial_delay: f64,
    pub repeat_delay: f64,
    pub nav_hold_timer: f64,
    pub nav_past_initial: bool,
    pub last_frame: Instant,

    // Track which keys are currently held
    pub keys_down: HashSet<NamedKey>,
    pub chars_down: HashSet<char>,

    // Track keys that were just pressed this frame
    pub keys_pressed: HashSet<NamedKey>,
    pub chars_pressed: HashSet<char>,

    // Mouse wheel accumulator for this frame
    pub wheel_y: f32,

    // Feature states
    pub marked_file_output: Option<PathBuf>,
    pub show_help: bool,
    pub rotation: u8, // 0=0, 1=90, 2=180, 3=270 (CW)
}

impl ViewerState {
    pub fn new(
        files: Arc<RwLock<Vec<PathBuf>>>,
        shared: SharedState,
        initial_delay: f64,
        repeat_delay: f64,
        marked_file_output: Option<PathBuf>,
    ) -> Self {
        Self {
            files,
            shared,
            current_index: 0,
            displayed_index: 0,
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
            marked_file_output,
            show_help: false,
            rotation: 0,
        }
    }

    pub fn is_key_pressed_named(&self, k: NamedKey) -> bool {
        self.keys_pressed.contains(&k)
    }

    pub fn is_char_pressed(&self, c: char) -> bool {
        self.chars_pressed.contains(&c)
    }

    pub fn is_key_down_named(&self, k: NamedKey) -> bool {
        self.keys_down.contains(&k)
    }

    pub fn is_char_down(&self, c: char) -> bool {
        self.chars_down.contains(&c)
    }

    /// Run the per-frame logic: input handling, cache polling, etc.
    /// Returns true if the app should quit.
    pub fn update(&mut self, window: &Window) -> bool {
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
        let mut explicit_target: Option<usize> = None;

        let files_guard = self.files.read().unwrap();
        let files_len = files_guard.len();
        drop(files_guard); // Drop lock early

        // Home / End
        if self.is_key_pressed_named(NamedKey::Home) {
            explicit_target = Some(0);
        } else if self.is_key_pressed_named(NamedKey::End) {
             explicit_target = Some(files_len.saturating_sub(1));
        }

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

        if nav != 0 || explicit_target.is_some() {
            // If current image is still loading, don't advance — poll cache instead.
            // Loading means we are waiting for the image at `current_index` to be ready.
            let is_loading = self.displayed_index != self.current_index
                || (self.current_decoded.is_none() && self.error_message.is_none());

            // If we have an explicit target (Home/End), we bypass loading check
            if is_loading && explicit_target.is_none() {
                let (lock, cvar) = &*self.shared;
                let mut state = lock.lock().unwrap();
                // Ensure workers know our actual position
                state.set_current_idx(self.current_index);
                if let Some(img) = state.get(self.current_index) {
                    self.current_decoded = Some(img);
                    self.displayed_index = self.current_index;
                } else if let Some(err) = state.errors.get(&self.current_index) {
                    self.error_message = Some(format!("Could not load: {}", err));
                    self.current_decoded = None;
                    self.displayed_index = self.current_index;
                }
                cvar.notify_all();
                // Don't advance yet — wait for current image
            } else {
                let new_idx = if let Some(t) = explicit_target {
                    t
                } else {
                    if files_len == 0 {
                        0
                    } else {
                        (self.current_index as i64 + nav as i64)
                            .clamp(0, files_len as i64 - 1) as usize
                    }
                };

                if new_idx != self.current_index {
                    log::debug!(
                        "[nav] move {} -> {} (cache_hit={})",
                        self.current_index,
                        new_idx,
                        self.shared.0.lock().unwrap().images.contains_key(&new_idx)
                    );
                    self.current_index = new_idx;
                    self.error_message = None;
                    self.zoom = 0.0;
                    self.offset_x = 0.0;
                    self.offset_y = 0.0;

                    // Update shared state and wake workers
                    let (lock, cvar) = &*self.shared;
                    let mut state = lock.lock().unwrap();
                    state.set_current_idx(new_idx);
                    if let Some(img) = state.get(new_idx) {
                        self.current_decoded = Some(img);
                        self.displayed_index = new_idx;
                    } else if let Some(err) = state.errors.get(&new_idx) {
                        self.error_message = Some(format!("Could not load: {}", err));
                        self.current_decoded = None;
                        self.displayed_index = new_idx;
                    } else {
                        // Not in cache yet.
                        // Keep current_decoded (old image) and displayed_index (old index)
                        // to show the overlay.
                    }
                    cvar.notify_all();
                }
            }
        }

        // ------------------------------------------------------------------
        // Toggle info
        // ------------------------------------------------------------------
        if self.is_char_pressed('i') {
            self.show_info = !self.show_info;
        }

        // ------------------------------------------------------------------
        // Toggle help
        // ------------------------------------------------------------------
        if self.is_char_pressed('?') {
            self.show_help = !self.show_help;
        }

        // ------------------------------------------------------------------
        // Mark file
        // ------------------------------------------------------------------
        if self.is_char_pressed('m') {
            self.mark_current_file();
        }

        // ------------------------------------------------------------------
        // Rotate
        // ------------------------------------------------------------------
        if self.is_char_pressed('r') {
            self.rotation = (self.rotation + 1) % 4;
            self.zoom = 0.0; // Reset zoom on rotate for simplicity
            self.offset_x = 0.0;
            self.offset_y = 0.0;
        }
        if self.is_char_pressed('R') {
             self.rotation = (self.rotation + 3) % 4;
             self.zoom = 0.0;
             self.offset_x = 0.0;
             self.offset_y = 0.0;
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
        // Zoom: z = 1:1 toggle (was 'z') -- wait code says 'r' is fit to window in comments? 
        // No, loop above 'r' is rotate.
        // Old code:
        // if self.is_char_pressed('r') { zoom=0.0; ... } -> This was probably remnants or re-use.
        // But 'r' is rotate now.
        // Let's check original code logic again.
        // "Zoom: r = fit to window, z = 1:1" comment in old code.
        // But we added rotation on 'r'.
        // So 'r' now rotates.
        // 'z' toggles zoom.
        // Let's remove 'r' for reset zoom, or bind it to something else? 
        // Or just let 'z' handle it.
        // ------------------------------------------------------------------
        
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

        // Clear per-frame input state
        self.keys_pressed.clear();
        self.chars_pressed.clear();
        self.wheel_y = 0.0;

        false
    }

    fn mark_current_file(&self) {
        let files_guard = self.files.read().unwrap();
        if self.current_index < files_guard.len() {
            let path = &files_guard[self.current_index];
            if let Some(ref out_path) = self.marked_file_output {
                // Append to file
                match fs::OpenOptions::new().create(true).append(true).open(out_path) {
                    Ok(mut file) => {
                        if let Err(e) = writeln!(file, "{}", path.display()) {
                            log::error!("Failed to write to mark file: {}", e);
                        }
                    }
                    Err(e) => {
                        log::error!("Failed to open mark file: {}", e);
                    }
                }
            } else {
                // Write to stdout
                println!("{}", path.display());
            }
        }
    }

    /// Render into the softbuffer framebuffer (u32 per pixel, 0x00RRGGBB).
    pub fn render(&self, frame: &mut [u32], fb_w: u32, fb_h: u32) {
        // Clear to background color
        let bg = rgb(BG_COLOR[0], BG_COLOR[1], BG_COLOR[2]);
        frame.fill(bg);

        let sw = fb_w as f32;
        let sh = fb_h as f32;

        if let Some(ref dec) = self.current_decoded {
            // Adjust dimensions for rotation
            let (img_w, img_h) = if self.rotation % 2 == 1 {
                (dec.height as f32, dec.width as f32)
            } else {
                (dec.width as f32, dec.height as f32)
            };

            let scale = if self.zoom == 0.0 {
                fit_scale(img_w, img_h, sw, sh)
            } else {
                self.zoom
            };

            let draw_w = img_w * scale;
            let draw_h = img_h * scale;
            let x0 = (sw - draw_w) / 2.0 + self.offset_x;
            let y0 = (sh - draw_h) / 2.0 + self.offset_y;

            blit_scaled_rotated(
                frame, fb_w, fb_h,
                &dec.rgba_bytes, dec.width, dec.height,
                x0, y0, scale,
                self.rotation,
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
                
                let files_guard = self.files.read().unwrap();
                let files_len = files_guard.len();
                let filename = if self.current_index < files_len {
                    files_guard[self.current_index].display().to_string()
                } else {
                    "Loading...".to_string()
                };
                
                let line1 = format!(
                    "[{}/{}]",
                    self.current_index + 1,
                    files_len,
                );
                let line2 = format!(
                    "{}",
                    filename,
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
                let line4 = {
                    let (lock, _) = &*self.shared;
                    let cs = lock.lock().unwrap();
                    let cached = cs.images.len();
                    let used_mb = cs.used_bytes as f64 / (1024.0 * 1024.0);
                    let budget_mb = cs.budget as f64 / (1024.0 * 1024.0);
                    format!(
                        "cache: {}/{} images | {:.0}/{:.0} MB",
                        cached, files_len, used_mb, budget_mb,
                    )
                };
                let text_scale: u32 = 2;
                let line_h = (7 * text_scale + 4) as i32;
                let bar_h = (line_h * 4 + 8) as u32;
                fill_rect(frame, fb_w, fb_h, 0, 0, fb_w, bar_h, (0, 0, 0, 178));
                let white = (255, 255, 255, 255);
                draw_text(frame, fb_w, fb_h, &line1, 10, 4, text_scale, white);
                draw_text(frame, fb_w, fb_h, &line2, 10, 4 + line_h, text_scale, white);
                draw_text(frame, fb_w, fb_h, &line3, 10, 4 + line_h * 2, text_scale, white);
                draw_text(frame, fb_w, fb_h, &line4, 10, 4 + line_h * 3, text_scale, white);
            }
        }

        // Check for Error or Loading state overlays
        if let Some(ref err) = self.error_message {
            let text_scale: u32 = 2;
            draw_text(frame, fb_w, fb_h, err, 20, fb_h as i32 / 2, text_scale, (255, 80, 80, 255));
        } else if self.displayed_index != self.current_index || self.current_decoded.is_none() {
             // ... existing loading log ...
            let text_scale: u32 = 2;
            let tx = (fb_w as i32) / 2 - 30;
            // Draw a semi-transparent box behind "Loading..." if we have an image under it
            if self.current_decoded.is_some() {
                 fill_rect(frame, fb_w, fb_h, tx - 10, fb_h as i32 / 2 - 10, 140, 40, (0, 0, 0, 128));
            }
            draw_text(frame, fb_w, fb_h, "Loading...", tx, fb_h as i32 / 2, text_scale, (255, 255, 255, 255));
        }

        // Help Overlay
        if self.show_help {
            fill_rect(frame, fb_w, fb_h, 0, 0, fb_w, fb_h, (0, 0, 0, 200));
            let text_scale = 2;
            let mut y = 20;
            for line in HELP_KEYS.lines() {
                draw_text(frame, fb_w, fb_h, line, 20, y, text_scale, (255, 255, 255, 255));
                y += 24;
            }
        }
    }
}
