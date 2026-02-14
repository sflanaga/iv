use clap::Parser;
use image::GenericImageView;
use softbuffer::Surface;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const ZOOM_FACTOR: f32 = 0.25;
const BG_COLOR: [u8; 4] = [31, 31, 31, 255]; // ~0.12 * 255
const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "bmp", "tga", "tiff", "tif", "webp", "ico", "pnm", "pbm",
    "pgm", "ppm", "pam", "dds", "hdr", "exr", "ff", "qoi",
];

const HELP_KEYS: &str = "\
Key Bindings:
  Esc / q       : Quit
  Left / h      : Previous image
  Right / l     : Next image
  Space         : Next image
  f             : Toggle fullscreen
  i             : Toggle info overlay
  ?             : Toggle help overlay
  r / R         : Rotate 90° CCW / CW
  m             : Mark current file (write path to output)
  z             : Toggle zoom (1:1 / Fit)
  + / - / Wheel : Zoom in / out
  Home          : Go to first image
  End           : Go to last image
";

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "iv", about = "A simple image viewer", after_help = HELP_KEYS)]
struct Cli {
    /// Files or directories to view
    #[arg(required_unless_present = "file_list")]
    paths: Vec<PathBuf>,

    /// Load file list from a text file (one path per line)
    #[arg(short = 'L', long, value_name = "FILE")]
    file_list: Option<PathBuf>,

    /// Output file for marked images (appends path). Defaults to stdout if not set.
    #[arg(short = 'o', long, value_name = "FILE")]
    marked_file_output: Option<PathBuf>,

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

fn collect_images(paths: &[PathBuf], file_list: Option<&PathBuf>, recursive: bool) -> Vec<PathBuf> {
    let mut result = Vec::new();
    
    // 1. Read from file list if provided
    if let Some(list_path) = file_list {
        if let Ok(file) = fs::File::open(list_path) {
            let reader = io::BufReader::new(file);
            for line in reader.lines() {
                if let Ok(l) = line {
                    // Split by tab first
                    for tab_part in l.split('\t') {
                        // Then by double-space (common column separator)
                        for part in tab_part.split("  ") {
                            let trimmed = part.trim();
                            if trimmed.is_empty() { continue; }
                            
                            let p = PathBuf::from(trimmed);
                            if p.is_file() {
                                if is_image_file(&p) {
                                    result.push(p);
                                }
                            } else {
                                // If the chunk is not a file, maybe it's multiple single-space separated files?
                                // Only try this if the chunk itself isn't a valid path, 
                                // to avoid breaking "Copy of file.jpg".
                                for sub in trimmed.split_whitespace() {
                                    let sub_p = PathBuf::from(sub);
                                    if sub_p.is_file() && is_image_file(&sub_p) {
                                        result.push(sub_p);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // 2. Scan explicit paths
    for path in paths {
        if path.is_dir() {
            scan_dir(path, recursive, &mut result);
        } else if path.is_file() && is_image_file(path) {
            result.push(path.clone());
        }
    }
    
    // Remove duplicates? Maybe not needed if user wants specific order.
    // But if we mixed directories and files, order might be weird.
    // Let's keep it simple: just append.
    
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
// Cache state (shared between UI and worker threads via Mutex + Condvar)
// ---------------------------------------------------------------------------

struct CacheState {
    current_idx: usize,
    images: HashMap<usize, Arc<DecodedImage>>,
    in_progress: HashSet<usize>,
    errors: HashMap<usize, String>,
    used_bytes: u64,
    budget: u64,
    file_count: usize,
    /// Indices that were decoded but couldn't be kept (cache full, too far).
    /// Cleared when current_idx changes so they can be re-evaluated.
    saturated: HashSet<usize>,
}

type SharedState = Arc<(Mutex<CacheState>, Condvar)>;

impl CacheState {
    fn new(budget: u64, file_count: usize) -> Self {
        Self {
            current_idx: 0,
            images: HashMap::new(),
            in_progress: HashSet::new(),
            errors: HashMap::new(),
            used_bytes: 0,
            budget,
            file_count,
            saturated: HashSet::new(),
        }
    }

    fn set_current_idx(&mut self, idx: usize) {
        if idx != self.current_idx {
            self.current_idx = idx;
            self.saturated.clear();
        }
    }

    fn get(&self, idx: usize) -> Option<Arc<DecodedImage>> {
        self.images.get(&idx).cloned()
    }

    /// Average decoded image size in bytes (fallback: ~8 MB).
    fn avg_image_size(&self) -> u64 {
        if self.images.is_empty() {
            8 * 1024 * 1024
        } else {
            self.used_bytes / self.images.len() as u64
        }
    }

    fn is_available(&self, idx: usize) -> bool {
        idx < self.file_count
            && !self.images.contains_key(&idx)
            && !self.in_progress.contains(&idx)
            && !self.errors.contains_key(&idx)
            && !self.saturated.contains(&idx)
    }

    fn get_farthest_cached(&self) -> Option<(usize, usize)> {
        self.images.keys()
            .filter(|&&i| i != self.current_idx)
            .map(|&i| {
                let dist = if i >= self.current_idx { i - self.current_idx } else { self.current_idx - i };
                (i, dist)
            })
            .max_by_key(|&(_, d)| d)
    }

    /// Find the nearest un-cached, non-in-progress index to current_idx.
    /// Prioritizes forward direction (2:1 ratio) to support read-ahead.
    fn find_work(&self) -> Option<usize> {
        // Always prioritize current_idx regardless of budget
        if self.current_idx < self.file_count
            && !self.images.contains_key(&self.current_idx)
            && !self.in_progress.contains(&self.current_idx)
            && !self.errors.contains_key(&self.current_idx)
        {
            return Some(self.current_idx);
        }

        let avg = self.avg_image_size();
        let pending_bytes = self.in_progress.len() as u64 * avg;
        let predicted_usage = self.used_bytes + pending_bytes + avg;
        let over_budget = predicted_usage > self.budget;
        
        // If over budget, we can only schedule if the new item is "closer" 
        // than the farthest item we currently have (which would be evicted).
        let farthest_dist = if over_budget {
            self.get_farthest_cached().map(|(_, d)| d).unwrap_or(0)
        } else {
            usize::MAX
        };

        // Search pattern:
        // 1. Immediate neighbors (+1, -1)
        // 2. Then 2 forward, 1 backward, repeated.
        
        let mut fwd_dist = 1;
        let mut bwd_dist = 1;
        
        let mut stop_fwd = false;
        let mut stop_bwd = false;

        // Max scan distance to prevent scanning the whole drive if cache is tiny
        const MAX_SCAN: usize = 2000; 

        // Helper to check if a candidate is valid to schedule
        let check_candidate = |idx: usize| -> Option<usize> {
            if self.saturated.contains(&idx) {
                // If it was saturated before, it won't fit now unless budget changed/moved
                // But we clear saturated on move.
                return None; // Stop search signal handled by caller via return check?
                // Actually saturated means "too far/big".
            }
            if self.is_available(idx) {
                let dist = if idx >= self.current_idx { idx - self.current_idx } else { self.current_idx - idx };
                if !over_budget || dist < farthest_dist {
                     return Some(idx);
                }
            }
            None
        };

        // 1. Immediate neighbors
        // Check +1
        if fwd_dist < self.file_count && !stop_fwd {
            let idx = self.current_idx + fwd_dist;
            if idx < self.file_count {
                if self.saturated.contains(&idx) {
                    stop_fwd = true;
                    log::debug!("[find_work] Stop FWD at saturated idx={}", idx);
                } else if let Some(found) = check_candidate(idx) {
                    return Some(found);
                } else if self.is_available(idx) {
                    stop_fwd = true;
                    log::debug!(
                        "[find_work] Stop FWD at idx={} (dist={} over_budget={})", 
                        idx, 
                        if idx >= self.current_idx { idx - self.current_idx } else { self.current_idx - idx },
                        over_budget
                    );
                }
            } else {
                stop_fwd = true;
            }
            fwd_dist += 1;
        }
        
        // Check -1
        if bwd_dist <= self.current_idx && !stop_bwd {
            let idx = self.current_idx - bwd_dist;
            if self.saturated.contains(&idx) {
                stop_bwd = true;
                log::debug!("[find_work] Stop BWD at saturated idx={}", idx);
            } else if let Some(found) = check_candidate(idx) {
                return Some(found);
            } else if self.is_available(idx) {
                 stop_bwd = true;
                 log::debug!(
                    "[find_work] Stop BWD at idx={} (dist={} over_budget={})", 
                    idx, 
                    if idx >= self.current_idx { idx - self.current_idx } else { self.current_idx - idx },
                    over_budget
                );
            }
            bwd_dist += 1;
        }

        // 2. Loop with bias
        while (!stop_fwd && fwd_dist < MAX_SCAN) || (!stop_bwd && bwd_dist < MAX_SCAN) {
             // 2 Forward
            for _ in 0..2 {
                if stop_fwd { break; }
                let idx = self.current_idx + fwd_dist;
                if idx >= self.file_count {
                    stop_fwd = true;
                } else {
                    if self.saturated.contains(&idx) {
                        stop_fwd = true;
                        log::debug!("[find_work] Stop FWD at saturated idx={}", idx);
                    } else if let Some(found) = check_candidate(idx) {
                        return Some(found);
                    } else if self.is_available(idx) {
                        stop_fwd = true;
                         log::debug!(
                            "[find_work] Stop FWD at idx={} (dist={} over_budget={})", 
                            idx, 
                            if idx >= self.current_idx { idx - self.current_idx } else { self.current_idx - idx },
                            over_budget
                        );
                    }
                }
                fwd_dist += 1;
            }

            // 1 Backward
            if !stop_bwd {
                if bwd_dist > self.current_idx {
                     stop_bwd = true;
                } else {
                    let idx = self.current_idx - bwd_dist;
                    if self.saturated.contains(&idx) {
                        stop_bwd = true;
                        log::debug!("[find_work] Stop BWD at saturated idx={}", idx);
                    } else if let Some(found) = check_candidate(idx) {
                        return Some(found);
                    } else if self.is_available(idx) {
                        stop_bwd = true;
                         log::debug!(
                            "[find_work] Stop BWD at idx={} (dist={} over_budget={})", 
                            idx, 
                            if idx >= self.current_idx { idx - self.current_idx } else { self.current_idx - idx },
                            over_budget
                        );
                    }
                    bwd_dist += 1;
                }
            }
        }
        
        None
    }

    /// Insert a decoded image, then evict distant images if over budget.
    /// If the new image would be the farthest and over budget, skip it
    /// (it would be immediately evicted) and mark it saturated.
    fn insert(&mut self, idx: usize, decoded: DecodedImage) {
        if idx != self.current_idx && self.used_bytes + decoded.mem_size() > self.budget {
            let my_dist = if idx >= self.current_idx {
                idx - self.current_idx
            } else {
                self.current_idx - idx
            };
            let farthest_cached_dist = self.images.keys()
                .filter(|&&i| i != self.current_idx)
                .map(|&i| if i >= self.current_idx { i - self.current_idx } else { self.current_idx - i })
                .max()
                .unwrap_or(0);
            
            // If we are farther than the farthest thing we have, we can't fit.
            if my_dist >= farthest_cached_dist {
                log::debug!(
                    "[saturated] idx={} dist={} (farthest_cached_dist={}) - skipping insert",
                    idx, my_dist, farthest_cached_dist,
                );
                self.saturated.insert(idx);
                return;
            }
        }
        // Handle re-insertion: subtract old bytes first
        if let Some(old) = self.images.remove(&idx) {
            self.used_bytes -= old.mem_size();
        }
        self.used_bytes += decoded.mem_size();
        log::debug!(
            "[insert] idx={} size={:.1}MB used={:.1}/{:.1}MB",
            idx,
            decoded.mem_size() as f64 / (1024.0 * 1024.0),
            self.used_bytes as f64 / (1024.0 * 1024.0),
            self.budget as f64 / (1024.0 * 1024.0)
        );
        self.images.insert(idx, Arc::new(decoded));
        self.evict_distant();
    }

    /// Remove images farthest from current_idx until under budget.
    /// Never evicts the current_idx image.
    fn evict_distant(&mut self) {
        while self.used_bytes > self.budget && self.images.len() > 1 {
            let farthest = self.images.keys()
                .filter(|&&idx| idx != self.current_idx)
                .max_by_key(|&&idx| {
                    if idx >= self.current_idx {
                        idx - self.current_idx
                    } else {
                        self.current_idx - idx
                    }
                })
                .copied();
            match farthest {
                Some(evict_idx) => {
                    if let Some(img) = self.images.remove(&evict_idx) {
                        log::debug!(
                            "[evict] idx={} dist={} freed={:.1}MB",
                            evict_idx,
                            if evict_idx >= self.current_idx { evict_idx - self.current_idx } else { self.current_idx - evict_idx },
                            img.mem_size() as f64 / (1024.0 * 1024.0),
                        );
                        self.used_bytes -= img.mem_size();
                    }
                }
                None => break, // only current_idx remains
            }
        }
    }
}

// ---------------------------------------------------------------------------
// User event for waking the UI from worker threads
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum UserEvent {
    ImageReady(usize),
}

// ---------------------------------------------------------------------------
// Background decode workers
// ---------------------------------------------------------------------------

fn spawn_decode_workers(
    shared: SharedState,
    files: Arc<Vec<PathBuf>>,
    proxy: EventLoopProxy<UserEvent>,
    num_threads: usize,
) {
    for _ in 0..num_threads {
        let shared = Arc::clone(&shared);
        let files = Arc::clone(&files);
        let proxy = proxy.clone();
        thread::spawn(move || {
            loop {
                // Wait for work
                let idx = {
                    let (lock, cvar) = &*shared;
                    let mut state = lock.lock().unwrap();
                    loop {
                        if let Some(idx) = state.find_work() {
                            state.in_progress.insert(idx);
                            log::debug!("[schedule] Worker picked up idx={}", idx);
                            break idx;
                        }
                        state = cvar.wait(state).unwrap();
                    }
                };

                // Decode (no lock held — this is the slow part)
                let t0 = Instant::now();
                let result = decode_image(&files[idx]);
                let elapsed = t0.elapsed();

                // Insert result and wake other workers
                {
                    let (lock, cvar) = &*shared;
                    let mut state = lock.lock().unwrap();
                    state.in_progress.remove(&idx);
                    match result {
                        Ok(decoded) => {
                            let bytes = decoded.rgba_bytes.len() as f64;
                            let secs = elapsed.as_secs_f64();
                            let mbps = if secs > 0.0 { bytes / secs / (1024.0 * 1024.0) } else { 0.0 };
                            log::debug!(
                                "[decode] idx={} file={} {:.1}ms {:.1} MB/s",
                                idx,
                                files[idx].file_name().unwrap_or_default().to_string_lossy(),
                                secs * 1000.0,
                                mbps,
                            );
                            state.insert(idx, decoded);
                        }
                        Err(e) => {
                            log::warn!(
                                "[decode] idx={} file={} FAILED: {}",
                                idx,
                                files[idx].file_name().unwrap_or_default().to_string_lossy(),
                                e,
                            );
                            state.errors.insert(
                                idx,
                                format!("{}: {}", files[idx].display(), e),
                            );
                        }
                    }
                    cvar.notify_all();
                }

                // Wake the UI
                let _ = proxy.send_event(UserEvent::ImageReady(idx));
            }
        });
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
    files: Arc<Vec<PathBuf>>,
    shared: SharedState,
    current_index: usize,
    /// The index of the image currently stored in `current_decoded`.
    /// May differ from `current_index` if we are waiting for a load.
    displayed_index: usize,
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

    // Feature states
    marked_file_output: Option<PathBuf>,
    show_help: bool,
    rotation: u8, // 0=0, 1=90, 2=180, 3=270 (CW)
}

impl ViewerState {
    fn new(
        files: Arc<Vec<PathBuf>>,
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
        let mut explicit_target: Option<usize> = None;

        // Home / End
        if self.is_key_pressed_named(NamedKey::Home) {
            explicit_target = Some(0);
        } else if self.is_key_pressed_named(NamedKey::End) {
             explicit_target = Some(self.files.len().saturating_sub(1));
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

            // If we have an explicit target (Home/End), we might want to allow jumping 
            // even if loading? For now let's keep it consistent: wait for load unless it's a huge jump?
            // Actually, if we are loading index 5 and user hits Home (0), we should probably just go.
            // But the architecture "poll cache" relies on current_index being the target.
            // If we change current_index to 0, the worker logic will switch to 0.
            // The only issue is the "loading overlay" logic which checks displayed vs current.
            // If we change current to 0, and displayed is still 4 (old), it will show loading overlay.
            // That is fine.
            
            // However, the block below says "if is_loading { don't advance }".
            // We should probably allow changing target if it's an explicit jump, 
            // OR if we are just scrolling. 
            // But the problem with "if is_loading" is that it is the mechanism to POLL the cache.
            // If we skip it, we update current_index, but we don't necessarily wait for the OLD one.
            // We want to wait for the NEW one.
            
            // The logic:
            // if is_loading { check if the WANTED image appeared }
            // else { calculate NEW WANTED image }
            
            // If we have an explicit target, that IS the new wanted image.
            // So we should update current_index to it.
            
            // But we can only do that if we are NOT waiting for the PREVIOUS move to finish?
            // No, we can interrupt.
            
            // Let's see: 
            // The existing code uses `is_loading` to BLOCK navigation inputs (`nav != 0`).
            // It says "Don't advance yet".
            // This is to prevent skipping over images too fast or getting out of sync?
            // Or mostly to ensure we see the image before moving to the next.
            
            // If I hit Home, I definitely want to move.
            // So I should arguably bypass the `is_loading` check for `explicit_target`.
            
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
                    (self.current_index as i64 + nav as i64)
                        .clamp(0, self.files.len() as i64 - 1) as usize
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

        // Clear per-frame input state
        self.keys_pressed.clear();
        self.chars_pressed.clear();
        self.wheel_y = 0.0;

        false
    }

    fn mark_current_file(&self) {
        if self.current_index < self.files.len() {
            let path = &self.files[self.current_index];
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
    fn render(&self, frame: &mut [u32], fb_w: u32, fb_h: u32) {
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
                let line4 = {
                    let (lock, _) = &*self.shared;
                    let cs = lock.lock().unwrap();
                    let cached = cs.images.len();
                    let used_mb = cs.used_bytes as f64 / (1024.0 * 1024.0);
                    let budget_mb = cs.budget as f64 / (1024.0 * 1024.0);
                    format!(
                        "cache: {}/{} images | {:.0}/{:.0} MB",
                        cached, self.files.len(), used_mb, budget_mb,
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

// ---------------------------------------------------------------------------
// Scaled blit (nearest-neighbor) with rotation
// ---------------------------------------------------------------------------

fn blit_scaled_rotated(
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

impl ApplicationHandler<UserEvent> for App {
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

        window.request_redraw();
        self.window = Some(window);
        self.context = Some(context);
        self.surface = Some(surface);
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::ImageReady(idx) => {
                // If the ready image is the one we want to display
                if idx == self.state.current_index {
                    let (lock, _) = &*self.state.shared;
                    let state = lock.lock().unwrap();
                    if let Some(img) = state.get(idx) {
                        drop(state);
                        self.state.current_decoded = Some(img);
                        self.state.displayed_index = idx;
                        self.state.error_message = None;
                    } else if let Some(err) = state.errors.get(&idx) {
                        let msg = format!("Could not load: {}", err);
                        drop(state);
                        self.state.error_message = Some(msg);
                        self.state.current_decoded = None; // clear old image on error? or keep it? 
                        // Let's clear it so the error is visible on black background, 
                        // or we could overlay error. For now, clear to match old behavior for errors.
                        self.state.displayed_index = idx;
                    }
                    if let Some(ref window) = self.window {
                        window.request_redraw();
                    }
                }
            }
        }
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

                // Schedule next redraw only for key-hold repeat
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
    env_logger::init();
    let cli = Cli::parse();

    let budget = match &cli.memory {
        Some(s) => parse_memory_budget(s),
        None => default_memory_budget(),
    };

    let files = collect_images(&cli.paths, cli.file_list.as_ref(), cli.recursive);
    if files.is_empty() {
        log::error!("No image files found.");
        return;
    }

    let files = Arc::new(files);
    let file_count = files.len();

    let shared: SharedState = Arc::new((
        Mutex::new(CacheState::new(budget, file_count)),
        Condvar::new(),
    ));

    let num_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(4, 16);

    let event_loop = EventLoop::<UserEvent>::with_user_event().build().expect("create event loop");
    let proxy = event_loop.create_proxy();

    // Spawn workers — they immediately start decoding from index 0 outward
    spawn_decode_workers(Arc::clone(&shared), Arc::clone(&files), proxy, num_threads);
    {
        let (_, cvar) = &*shared;
        cvar.notify_all();
    }

    let initial_delay = cli.initial_delay as f64 / 1000.0;
    let repeat_delay = cli.repeat_delay as f64 / 1000.0;

    let state = ViewerState::new(
        files, 
        shared, 
        initial_delay, 
        repeat_delay, 
        cli.marked_file_output
    );

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
