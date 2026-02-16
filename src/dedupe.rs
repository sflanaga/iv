use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Instant;
use winit::event_loop::EventLoopProxy;
use image_hasher::{HasherConfig, ImageHash};
use image::ImageReader;

use crate::files::is_image_file;
use crate::loader::UserEvent;

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
    proxy: EventLoopProxy<UserEvent>,
) {
    thread::spawn(move || {
        log::info!("Starting background duplicate scan (threshold: {})...", threshold);
        let start_time = Instant::now();
        
        // We will collect all files first, then process them.
        // Processing one by one is better for UI feedback, but we need something to compare against.
        // Strategy:
        // 1. Walk and find all image files.
        // 2. Hash them one by one.
        // 3. Compare new hash against all "seen" unique hashes.
        // 4. If match:
        //      If this is the first match for this unique hash, add the ORIGINAL to the display list too.
        //      Add the NEW one to the display list.
        
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
        
        let hasher = HasherConfig::new().to_hasher();
        let mut seen: Vec<SeenImage> = Vec::new();
        let mut displayed_count = 0;
        
        // We keep track of which "seen" images have already been "exposed" to the UI.
        // If SeenImage A matches new Image B, and A hasn't been shown yet, we show A then B.
        let mut exposed_indices: Vec<bool> = Vec::new();

        for (i, path) in all_files.into_iter().enumerate() {
            // Load and hash
            // We use image crate to open. 
            // Note: This duplicates some logic from loader.rs but we need the raw image for hashing.
            // For speed, we might want to resize before hashing if the image is huge, 
            // but image_hasher might handle that.
            
            // Log progress occasionally
            if i % 50 == 0 {
                log::debug!("Processed {} files...", i);
            }

            let img = match ImageReader::open(&path) {
                Ok(reader) => match reader.decode() {
                    Ok(img) => img,
                    Err(_) => continue, 
                },
                Err(_) => continue,
            };
            
            let hash = hasher.hash_image(&img);
            
            let mut found_match = false;
            let mut match_index = 0;
            
            for (idx, seen_img) in seen.iter().enumerate() {
                if hash.dist(&seen_img.hash) <= threshold {
                    found_match = true;
                    match_index = idx;
                    break;
                }
            }
            
            if found_match {
                // It's a duplicate of seen[match_index]
                let mut updates = Vec::new();
                
                // If the "original" hasn't been shown yet, show it now
                if !exposed_indices[match_index] {
                    updates.push(seen[match_index].path.clone());
                    exposed_indices[match_index] = true;
                }
                
                // Show the current duplicate
                updates.push(path.clone());
                
                // Push to shared state
                if !updates.is_empty() {
                    let mut guard = files_arc.write().unwrap();
                    guard.extend(updates);
                    displayed_count = guard.len();
                }
                
                let _ = proxy.send_event(UserEvent::FileListUpdated);
                
            } else {
                // New unique image
                seen.push(SeenImage {
                    path,
                    hash,
                });
                exposed_indices.push(false);
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
