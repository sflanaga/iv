use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

#[derive(Debug)]
struct ScannedImage {
    path: PathBuf,
    hash: ImageHash,
    width: u32,
    height: u32,
}

pub fn run_headless_dedupe(
    paths: Vec<PathBuf>,
    recursive: bool,
    follow_links: bool,
    threshold: u32,
    output_path: PathBuf,
) {
    let mut all_files = Vec::new();
    for path in &paths {
        if path.is_dir() {
            collect_files(path, recursive, follow_links, &mut all_files);
        } else if path.is_file() {
            if is_image_file(path) {
                all_files.push(path.clone());
            }
        }
    }
    
    // Sort for deterministic behavior
    all_files.sort();

    let total_files = all_files.len();
    eprintln!("Found {} candidates. Hashing...", total_files);

    let counter = Arc::new(AtomicUsize::new(0));
    let stop_signal = Arc::new(AtomicBool::new(false));
    
    // Spawn ticker thread
    let ticker_counter = Arc::clone(&counter);
    let ticker_stop = Arc::clone(&stop_signal);
    
    let ticker_handle = thread::spawn(move || {
        while !ticker_stop.load(Ordering::Relaxed) {
            let current = ticker_counter.load(Ordering::Relaxed);
            eprint!("\rScanning: {} / {}", current, total_files);
            // Flush stderr to ensure line updates
            use std::io::Write;
            let _ = std::io::stderr().flush();
            thread::sleep(std::time::Duration::from_secs(1));
        }
        // One final print to clear/finalize line
        eprintln!("\rScanning: {} / {} - Done.", ticker_counter.load(Ordering::Relaxed), total_files);
    });

    let hasher_config = HasherConfig::new();
    let scanned: Vec<ScannedImage> = all_files.par_iter()
        .filter_map(|path| {
            let res = {
                let hasher = hasher_config.to_hasher();
                 match ImageReader::open(path) {
                    Ok(reader) => match reader.decode() {
                        Ok(img) => {
                            let hash = hasher.hash_image(&img);
                            Some(ScannedImage {
                                path: path.clone(),
                                hash,
                                width: img.width(),
                                height: img.height(),
                            })
                        },
                        Err(_) => None, 
                    },
                    Err(_) => None,
                }
            };
            counter.fetch_add(1, Ordering::Relaxed);
            res
        })
        .collect();
    
    // Stop ticker
    stop_signal.store(true, Ordering::Relaxed);
    let _ = ticker_handle.join();
        
    eprintln!("Hashed {} images. Clustering...", scanned.len());

    // Clustering
    let mut clusters: Vec<Vec<ScannedImage>> = Vec::new();
    
    for img in scanned {
        let mut match_index = None;
        for (i, cluster) in clusters.iter().enumerate() {
            // Compare with the first one (representative)
            if img.hash.dist(&cluster[0].hash) <= threshold {
                match_index = Some(i);
                break;
            }
        }
        
        if let Some(i) = match_index {
            clusters[i].push(img);
        } else {
            clusters.push(vec![img]);
        }
    }
    
    eprintln!("Found {} clusters. Writing output to {}...", clusters.len(), output_path.display());

    // Post-process and write to file
    use std::io::Write;
    let mut file = match fs::File::create(&output_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Error creating output file: {}", e);
            return;
        }
    };

    // Header
    let now = chrono::Local::now();
    writeln!(file, "Duplicate Scan Report").unwrap();
    writeln!(file, "Time: {}", now.format("%Y-%m-%d %H:%M:%S")).unwrap();
    writeln!(file, "Scanned Directories:").unwrap();
    for p in &paths {
        writeln!(file, "  - {}", p.display()).unwrap();
    }
    writeln!(file, "Total Files Scanned: {}", total_files).unwrap();
    writeln!(file, "Threshold: {}", threshold).unwrap();
    writeln!(file, "--------------------------------------------------").unwrap();

    for mut cluster in clusters {
        // Only interested in duplicates (cluster size > 1)
        if cluster.len() > 1 {
            // Find best original: max pixels, then alphabetical path
            // We want to sort such that index 0 is the best.
            cluster.sort_by(|a, b| {
                let pixels_a = a.width as u64 * a.height as u64;
                let pixels_b = b.width as u64 * b.height as u64;
                
                if pixels_a != pixels_b {
                    return pixels_b.cmp(&pixels_a); // Descending resolution
                }
                a.path.cmp(&b.path) // Ascending path for deterministic tie-break
            });
            
            let original = &cluster[0];
            writeln!(file, "# {}", original.path.display()).unwrap();
            
            for i in 1..cluster.len() {
                let dup = &cluster[i];
                let dist = dup.hash.dist(&original.hash);
                writeln!(file, "D {} {}", dist, dup.path.display()).unwrap();
            }
        }
    }
    eprintln!("Done.");
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
