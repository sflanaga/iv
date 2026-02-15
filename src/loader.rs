use image::GenericImageView;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread;
use winit::event_loop::EventLoopProxy;

// ---------------------------------------------------------------------------
// Decoded image data (CPU side, before GPU upload)
// ---------------------------------------------------------------------------

pub struct DecodedImage {
    pub rgba_bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub file_size: u64,
    pub format_name: String,
}

impl DecodedImage {
    pub fn mem_size(&self) -> u64 {
        self.rgba_bytes.len() as u64
    }
}

fn decode_image(path: &Path, target_size: Option<(u32, u32)>) -> Result<DecodedImage, String> {
    let file_size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let img_result = image::open(path);
    
    match img_result {
        Ok(img) => {
            let format_name = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("unknown")
                .to_uppercase();

            let final_img = if let Some((w, h)) = target_size {
                img.thumbnail(w, h)
            } else {
                img
            };
            
            let (f_width, f_height) = final_img.dimensions();
            let rgba = final_img.to_rgba8();
            
            Ok(DecodedImage {
                rgba_bytes: rgba.into_raw(),
                width: f_width,
                height: f_height,
                file_size,
                format_name,
            })
        }
        Err(e) => Err(format!("{}", e)),
    }
}

// ---------------------------------------------------------------------------
// Cache state (shared between UI and worker threads via Mutex + Condvar)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ViewMode {
    Single,
    Grid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkType {
    Full,
    Thumbnail,
}

pub struct CacheState {
    pub current_idx: usize,
    pub mode: ViewMode,
    
    // Caches
    pub images: HashMap<usize, Arc<DecodedImage>>,
    pub thumbnails: HashMap<usize, Arc<DecodedImage>>,
    
    // Work tracking
    pub in_progress: HashSet<(usize, WorkType)>,
    pub errors: HashMap<usize, String>, // Full load errors
    pub thumbnail_errors: HashSet<usize>, // Thumbnail load errors (simple set)
    
    // Budget
    pub used_bytes: u64,
    pub budget: u64,
    pub file_count: usize,
    
    /// Indices that were decoded but couldn't be kept (cache full, too far).
    pub saturated: HashSet<usize>,
}

pub type SharedState = Arc<(Mutex<CacheState>, Condvar)>;

impl CacheState {
    pub fn new(budget: u64, file_count: usize) -> Self {
        Self {
            current_idx: 0,
            mode: ViewMode::Single,
            images: HashMap::new(),
            thumbnails: HashMap::new(),
            in_progress: HashSet::new(),
            errors: HashMap::new(),
            thumbnail_errors: HashSet::new(),
            used_bytes: 0,
            budget,
            file_count,
            saturated: HashSet::new(),
        }
    }

    pub fn set_current_idx(&mut self, idx: usize) {
        if idx != self.current_idx {
            self.current_idx = idx;
            self.saturated.clear();
        }
    }
    
    pub fn set_mode(&mut self, mode: ViewMode) {
        self.mode = mode;
        // Should we clear in_progress or caches? 
        // For now, keep them. Transition might be smoother.
    }

    pub fn get(&self, idx: usize) -> Option<Arc<DecodedImage>> {
        self.images.get(&idx).cloned()
    }
    
    pub fn get_thumbnail(&self, idx: usize) -> Option<Arc<DecodedImage>> {
        self.thumbnails.get(&idx).cloned()
    }

    /// Average decoded image size in bytes (fallback: ~8 MB).
    fn avg_image_size(&self) -> u64 {
        if self.images.is_empty() {
            8 * 1024 * 1024
        } else {
            self.used_bytes.max(1) / self.images.len().max(1) as u64
        }
    }

    pub fn is_available(&self, idx: usize, wtype: WorkType) -> bool {
        if idx >= self.file_count { return false; }
        if self.in_progress.contains(&(idx, wtype)) { return false; }
        
        match wtype {
            WorkType::Full => {
                !self.images.contains_key(&idx) 
                && !self.errors.contains_key(&idx)
                && !self.saturated.contains(&idx)
            },
            WorkType::Thumbnail => {
                !self.thumbnails.contains_key(&idx)
                && !self.thumbnail_errors.contains(&idx)
            }
        }
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
    pub fn find_work(&self) -> Option<(usize, WorkType)> {
        match self.mode {
            ViewMode::Single => self.find_work_single(),
            ViewMode::Grid => self.find_work_grid(),
        }
    }

    fn find_work_single(&self) -> Option<(usize, WorkType)> {
        // Always prioritize current_idx full load
        if self.is_available(self.current_idx, WorkType::Full) {
            return Some((self.current_idx, WorkType::Full));
        }

        // Standard prefetch logic for single view
        let avg = self.avg_image_size();
        let pending_bytes = self.in_progress.iter()
            .filter(|(_, t)| *t == WorkType::Full)
            .count() as u64 * avg;
            
        let predicted_usage = self.used_bytes + pending_bytes + avg;
        let over_budget = predicted_usage > self.budget;
        
        let farthest_dist = if over_budget {
            self.get_farthest_cached().map(|(_, d)| d).unwrap_or(0)
        } else {
            usize::MAX
        };

        const MAX_SCAN: usize = 2000; 
        
        let mut fwd_dist = 1;
        let mut bwd_dist = 1;
        let mut stop_fwd = false;
        let mut stop_bwd = false;

        let check_candidate = |idx: usize| -> Option<(usize, WorkType)> {
            if self.saturated.contains(&idx) {
                return None;
            }
            if self.is_available(idx, WorkType::Full) {
                let dist = if idx >= self.current_idx { idx - self.current_idx } else { self.current_idx - idx };
                if !over_budget || dist < farthest_dist {
                     return Some((idx, WorkType::Full));
                }
            }
            None
        };

        // 1. Immediate neighbors
        if fwd_dist < self.file_count {
            let idx = self.current_idx + fwd_dist;
            if idx < self.file_count {
                if let Some(res) = check_candidate(idx) { return Some(res); }
            }
            fwd_dist += 1;
        }
        
        if bwd_dist <= self.current_idx {
            let idx = self.current_idx - bwd_dist;
             if let Some(res) = check_candidate(idx) { return Some(res); }
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
                    } else if let Some(res) = check_candidate(idx) {
                        return Some(res);
                    } else if self.is_available(idx, WorkType::Full) {
                        stop_fwd = true;
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
                    } else if let Some(res) = check_candidate(idx) {
                        return Some(res);
                    } else if self.is_available(idx, WorkType::Full) {
                        stop_bwd = true;
                    }
                    bwd_dist += 1;
                }
            }
        }
        
        None
    }

    fn find_work_grid(&self) -> Option<(usize, WorkType)> {
        // Grid mode: Fill thumbnails spiraling out from current_index.
        // We want to prioritize the visible area which is centered on current_index.
        // We check backward (upper half) first to encourage top-to-bottom filling.
        
        // Scan ALL files.
        // We use the file_count as the limit to ensure we cover the entire list
        // regardless of where current_idx is.
        let limit = self.file_count;

        for i in 0..limit {
            // 1. Backward (current - i)
            // We check this first to prioritize the "top" of the view (reading order)
            if i > 0 {
                if let Some(bwd) = self.current_idx.checked_sub(i) {
                    if self.is_available(bwd, WorkType::Thumbnail) {
                        return Some((bwd, WorkType::Thumbnail));
                    }
                }
            }

            // 2. Forward (current + i)
            let fwd = self.current_idx + i;
            // Only check if within bounds. 
            // If fwd is out of bounds, we still continue the loop because 'bwd' might still be valid
            // (e.g. if we are at the end of the list, we need to scan backwards to 0).
            if fwd < self.file_count {
                if self.is_available(fwd, WorkType::Thumbnail) {
                    return Some((fwd, WorkType::Thumbnail));
                }
            }
        }

        None
    }

    /// Insert a decoded image. 
    pub fn insert(&mut self, idx: usize, decoded: DecodedImage, wtype: WorkType) {
        match wtype {
            WorkType::Full => {
                // Budget check only for full images for now
                if idx != self.current_idx && self.used_bytes + decoded.mem_size() > self.budget {
                    let my_dist = if idx >= self.current_idx { idx - self.current_idx } else { self.current_idx - idx };
                    let farthest_dist = self.images.keys()
                        .filter(|&&i| i != self.current_idx)
                        .map(|&i| if i >= self.current_idx { i - self.current_idx } else { self.current_idx - i })
                        .max()
                        .unwrap_or(0);
                    
                    if my_dist >= farthest_dist {
                        self.saturated.insert(idx);
                        return;
                    }
                }
                
                if let Some(old) = self.images.remove(&idx) {
                    self.used_bytes -= old.mem_size();
                }
                self.used_bytes += decoded.mem_size();
                self.images.insert(idx, Arc::new(decoded));
                self.evict_distant();
            },
            WorkType::Thumbnail => {
                // Thumbnails are small and kept separate for now (ignoring budget, or managed separately?)
                // A thumbnail is ~200x200x4 = 160KB. 1000 thumbnails = 160MB. 
                // We should probably limit them too, but let's assume they fit for now.
                self.thumbnails.insert(idx, Arc::new(decoded));
                
                // Optional: evict very far thumbnails if memory is tight?
                // For now, let's keep them to ensure smooth scrolling.
            }
        }
    }

    fn evict_distant(&mut self) {
        while self.used_bytes > self.budget && self.images.len() > 1 {
            let farthest = self.images.keys()
                .filter(|&&idx| idx != self.current_idx)
                .max_by_key(|&&idx| {
                    if idx >= self.current_idx { idx - self.current_idx } else { self.current_idx - idx }
                })
                .copied();
            
            match farthest {
                Some(evict_idx) => {
                    if let Some(img) = self.images.remove(&evict_idx) {
                        self.used_bytes -= img.mem_size();
                    }
                }
                None => break,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// User event for waking the UI from worker threads
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum UserEvent {
    ImageReady(usize),
    ThumbnailReady(usize),
    FileListUpdated,
}

// ---------------------------------------------------------------------------
// Background decode workers
// ---------------------------------------------------------------------------

pub fn spawn_decode_workers(
    shared: SharedState,
    files: Arc<RwLock<Vec<PathBuf>>>,
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
                let (idx, wtype) = {
                    let (lock, cvar) = &*shared;
                    let mut state = lock.lock().unwrap();
                    loop {
                        if let Some((idx, wtype)) = state.find_work() {
                            state.in_progress.insert((idx, wtype));
                            break (idx, wtype);
                        }
                        state = cvar.wait(state).unwrap();
                    }
                };

                let path_opt = {
                    let guard = files.read().unwrap();
                    if idx < guard.len() {
                        Some(guard[idx].clone())
                    } else {
                        None
                    }
                };

                if let Some(path) = path_opt {
                    // Decide size
                    let target_size = match wtype {
                        WorkType::Full => None,
                        WorkType::Thumbnail => Some((200, 200)), // Fixed thumbnail size
                    };

                    let result = decode_image(&path, target_size);

                    {
                        let (lock, cvar) = &*shared;
                        let mut state = lock.lock().unwrap();
                        state.in_progress.remove(&(idx, wtype));
                        
                        match result {
                            Ok(decoded) => {
                                state.insert(idx, decoded, wtype);
                            }
                            Err(e) => {
                                match wtype {
                                    WorkType::Full => {
                                        state.errors.insert(idx, format!("{}: {}", path.display(), e));
                                    }
                                    WorkType::Thumbnail => {
                                        state.thumbnail_errors.insert(idx);
                                    }
                                }
                            }
                        }
                        cvar.notify_all();
                    }

                    match wtype {
                        WorkType::Full => { let _ = proxy.send_event(UserEvent::ImageReady(idx)); },
                        WorkType::Thumbnail => { let _ = proxy.send_event(UserEvent::ThumbnailReady(idx)); },
                    }
                } else {
                    let (lock, _) = &*shared;
                    let mut state = lock.lock().unwrap();
                    state.in_progress.remove(&(idx, wtype));
                }
            }
        });
    }
}
