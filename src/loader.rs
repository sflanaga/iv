use image::GenericImageView;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread;
use std::time::Instant;
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

pub struct CacheState {
    pub current_idx: usize,
    pub images: HashMap<usize, Arc<DecodedImage>>,
    pub in_progress: HashSet<usize>,
    pub errors: HashMap<usize, String>,
    pub used_bytes: u64,
    pub budget: u64,
    pub file_count: usize,
    /// Indices that were decoded but couldn't be kept (cache full, too far).
    /// Cleared when current_idx changes so they can be re-evaluated.
    pub saturated: HashSet<usize>,
}

pub type SharedState = Arc<(Mutex<CacheState>, Condvar)>;

impl CacheState {
    pub fn new(budget: u64, file_count: usize) -> Self {
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

    pub fn set_current_idx(&mut self, idx: usize) {
        if idx != self.current_idx {
            self.current_idx = idx;
            self.saturated.clear();
        }
    }

    pub fn get(&self, idx: usize) -> Option<Arc<DecodedImage>> {
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

    pub fn is_available(&self, idx: usize) -> bool {
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
    pub fn find_work(&self) -> Option<usize> {
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
    pub fn insert(&mut self, idx: usize, decoded: DecodedImage) {
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
pub enum UserEvent {
    ImageReady(usize),
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
                // We must hold read lock on files just long enough to get the path
                let path_opt = {
                    let guard = files.read().unwrap();
                    if idx < guard.len() {
                        Some(guard[idx].clone())
                    } else {
                        None
                    }
                };

                if let Some(path) = path_opt {
                    let t0 = Instant::now();
                    let result = decode_image(&path);
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
                                    path.file_name().unwrap_or_default().to_string_lossy(),
                                    secs * 1000.0,
                                    mbps,
                                );
                                state.insert(idx, decoded);
                            }
                            Err(e) => {
                                log::warn!(
                                    "[decode] idx={} file={} FAILED: {}",
                                    idx,
                                    path.file_name().unwrap_or_default().to_string_lossy(),
                                    e,
                                );
                                state.errors.insert(
                                    idx,
                                    format!("{}: {}", path.display(), e),
                                );
                            }
                        }
                        cvar.notify_all();
                    }

                    // Wake the UI
                    let _ = proxy.send_event(UserEvent::ImageReady(idx));
                } else {
                    // Invalid index? Should not happen if CacheState is synced.
                    // Just clear it from in_progress
                    let (lock, _) = &*shared;
                    let mut state = lock.lock().unwrap();
                    state.in_progress.remove(&idx);
                }
            }
        });
    }
}
