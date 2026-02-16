use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Instant;
use winit::event_loop::EventLoopProxy;
use image_hasher::{HasherConfig, ImageHash};
use image::ImageReader;
use rayon::prelude::*;

use crate::files::is_image_file;
use crate::loader::UserEvent;

#[derive(Clone, Debug)]
pub struct DuplicateInfo {
    pub original_path: PathBuf,
    pub distance: u32,
    pub is_original: bool,
}

struct SeenImage {
    path: PathBuf,
    hash: ImageHash,
}

pub fn spawn_dedupe_scanner(
    paths: Vec<PathBuf>,
    recursive: bool,
    follow_links: bool,
    threshold: u32,
    files_arc: Arc<RwLock<Vec<PathBuf>>>,
    dupe_info_arc: Arc<RwLock<HashMap<PathBuf, DuplicateInfo>>>,
    proxy: EventLoopProxy<UserEvent>,
) {
    thread::spawn(move || {
        log::info!("Starting background duplicate scan (threshold: {})...", threshold);
        let start_time = Instant::now();
        
        // We will collect all files first, then process them.
        let mut all_files = Vec::new();
        for path in paths {
            if path.is_dir() {
                collect_files(&path, recursive, follow_links, &mut all_files);
            } else if path.is_file() {
                if is_image_file(&path) {
                     all_files.push(path);
                }
            }
        }
        
        // Sort files to ensure deterministic order (alphabetical)
        // This ensures the "original" is always the first one alphabetically.
        all_files.sort();
        
        log::info!("Found {} candidates. Hashing and comparing...", all_files.len());
        
        let hasher_config = HasherConfig::new(); // immutable config
        let mut seen: Vec<SeenImage> = Vec::new();
        let mut displayed_count = 0;
        
        // We keep track of which "seen" images have already been "exposed" to the UI.
        let mut exposed_indices: Vec<bool> = Vec::new();
        
        // Process in chunks to allow progressive UI updates while using parallelism
        let chunk_size = 100;
        
        for chunk in all_files.chunks(chunk_size) {
            // 1. Parallel Load & Hash
            // We use rayon to process this chunk in parallel.
            // The order is preserved in the output vector.
            let results: Vec<Option<ImageHash>> = chunk.par_iter()
                .map(|path| {
                    let hasher = hasher_config.to_hasher();
                    match ImageReader::open(path) {
                        Ok(reader) => match reader.decode() {
                            Ok(img) => Some(hasher.hash_image(&img)),
                            Err(_) => None, 
                        },
                        Err(_) => None,
                    }
                })
                .collect();

            // 2. Serial Deduplication
            // We must process comparison sequentially to maintain deterministic "original" detection.
            let mut chunk_updates = Vec::new();
            let mut info_updates = Vec::new();

            for (i, hash_opt) in results.into_iter().enumerate() {
                let path = &chunk[i];
                let hash = if let Some(h) = hash_opt {
                    h
                } else {
                    continue;
                };

                let mut found_match = false;
                let mut match_index = 0;
                let mut dist = 0;
                
                // Compare against all previously seen images
                for (idx, seen_img) in seen.iter().enumerate() {
                    let d = hash.dist(&seen_img.hash);
                    if d <= threshold {
                        found_match = true;
                        match_index = idx;
                        dist = d;
                        break;
                    }
                }
                
                if found_match {
                    // It's a duplicate of seen[match_index]
                    let original = &seen[match_index];
                    
                    // If the "original" hasn't been shown yet, show it now
                    if !exposed_indices[match_index] {
                        chunk_updates.push(original.path.clone());
                        info_updates.push((original.path.clone(), DuplicateInfo {
                            original_path: original.path.clone(),
                            distance: 0,
                            is_original: true,
                        }));
                        exposed_indices[match_index] = true;
                    }
                    
                    // Show the current duplicate
                    chunk_updates.push(path.clone());
                    info_updates.push((path.clone(), DuplicateInfo {
                        original_path: original.path.clone(),
                        distance: dist,
                        is_original: false,
                    }));
                    
                } else {
                    // New unique image
                    seen.push(SeenImage {
                        path: path.clone(),
                        hash,
                    });
                    exposed_indices.push(false);
                }
            }
            
            // 3. Batch Update UI
            if !chunk_updates.is_empty() {
                // Update duplicate info map first
                let mut info_guard = dupe_info_arc.write().unwrap();
                for (p, info) in info_updates {
                    info_guard.insert(p, info);
                }
                drop(info_guard);

                let mut guard = files_arc.write().unwrap();
                guard.extend(chunk_updates);
                displayed_count = guard.len();
                drop(guard); // drop lock before sending event
                
                let _ = proxy.send_event(UserEvent::FileListUpdated);
            }
        }

        log::info!(
            "Dedupe scan complete in {:.2}s. Found {} duplicates among {} files.",
            start_time.elapsed().as_secs_f64(),
            displayed_count,
            seen.len()
        );
    });
}

fn collect_files(
    dir: &Path, 
    recursive: bool, 
    follow_links: bool,
    dest: &mut Vec<PathBuf>
) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut subdirs = Vec::new();
    
    for entry in entries.filter_map(|e| e.ok()) {
        let ft = if let Ok(ft) = entry.file_type() {
            ft
        } else {
            continue;
        };

        if ft.is_symlink() && !follow_links {
            continue;
        }

        let p = entry.path();
        if p.is_file() && is_image_file(&p) {
            dest.push(p);
        } else if recursive && p.is_dir() {
            subdirs.push(p);
        }
    }
    
    // Sort to ensure deterministic order
    subdirs.sort();
    
    if recursive {
        for sub in subdirs {
            collect_files(&sub, true, follow_links, dest);
        }
    }
}
