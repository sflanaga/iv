use std::fs;
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Instant;
use winit::event_loop::EventLoopProxy;

use crate::loader::UserEvent;

const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "bmp", "tga", "tiff", "tif", "webp", "ico", "pnm", "pbm",
    "pgm", "ppm", "pam", "dds", "hdr", "exr", "ff", "qoi",
];

fn is_image_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

pub fn spawn_file_scanner(
    paths: Vec<PathBuf>,
    file_list: Option<PathBuf>,
    recursive: bool,
    follow_links: bool,
    files_arc: Arc<RwLock<Vec<PathBuf>>>,
    proxy: EventLoopProxy<UserEvent>,
) {
    thread::spawn(move || {
        log::info!("Starting background image scan...");
        let start_time = Instant::now();
        let mut count = 0;

        let should_process = |p: &PathBuf| -> bool {
            if !follow_links {
                if let Ok(meta) = fs::symlink_metadata(p) {
                    if meta.file_type().is_symlink() {
                        return false;
                    }
                }
            }
            true
        };

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
                                if !should_process(&p) { continue; }

                                if p.is_file() {
                                    if is_image_file(&p) {
                                        {
                                            let mut guard = files_arc.write().unwrap();
                                            guard.push(p);
                                        }
                                        count += 1;
                                        // Batch updates slightly? For now, every file is safe but maybe noisy.
                                        // Let's send update every 100 items or so, or if it's the first item.
                                        if count == 1 || count % 100 == 0 {
                                             let _ = proxy.send_event(UserEvent::FileListUpdated);
                                        }
                                    }
                                } else {
                                    // Try split whitespace
                                    for sub in trimmed.split_whitespace() {
                                        let sub_p = PathBuf::from(sub);
                                        if !should_process(&sub_p) { continue; }

                                        if sub_p.is_file() && is_image_file(&sub_p) {
                                            {
                                                let mut guard = files_arc.write().unwrap();
                                                guard.push(sub_p);
                                            }
                                            count += 1;
                                            if count == 1 || count % 100 == 0 {
                                                let _ = proxy.send_event(UserEvent::FileListUpdated);
                                            }
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
            if !should_process(&path) { continue; }

            if path.is_dir() {
                scan_dir(&path, recursive, follow_links, &files_arc, &proxy, &mut count);
            } else if path.is_file() && is_image_file(&path) {
                {
                    let mut guard = files_arc.write().unwrap();
                    guard.push(path.clone());
                }
                count += 1;
                if count == 1 || count % 100 == 0 {
                    let _ = proxy.send_event(UserEvent::FileListUpdated);
                }
            }
        }
        
        // Final update to ensure we didn't miss the last batch
        let _ = proxy.send_event(UserEvent::FileListUpdated);
        
        log::info!(
            "Scan complete in {:.2}s. Found {} images.",
            start_time.elapsed().as_secs_f64(),
            count
        );
    });
}

fn scan_dir(
    dir: &Path, 
    recursive: bool, 
    follow_links: bool,
    files_arc: &Arc<RwLock<Vec<PathBuf>>>, 
    proxy: &EventLoopProxy<UserEvent>,
    count: &mut usize
) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut files = Vec::new();
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
            files.push(p);
        } else if recursive && p.is_dir() {
            subdirs.push(p);
        }
    }
    
    // Sort files in this directory
    files.sort();
    
    if !files.is_empty() {
        {
            let mut guard = files_arc.write().unwrap();
            guard.extend(files);
        }
        *count = files_arc.read().unwrap().len();
        let _ = proxy.send_event(UserEvent::FileListUpdated);
        
        log::info!("Scanning {:?}... (total {} images)", dir, *count);
    }

    if recursive {
        subdirs.sort();
        for sub in subdirs {
            scan_dir(&sub, true, follow_links, files_arc, proxy, count);
        }
    }
}
