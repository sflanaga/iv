use std::fs;
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};

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

pub fn collect_images(paths: &[PathBuf], file_list: Option<&PathBuf>, recursive: bool) -> Vec<PathBuf> {
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
