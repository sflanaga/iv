# iv - Image Viewer

`iv` is a high-performance, lightweight image viewer written in Rust. It is designed for speed and efficiency, featuring intelligent read-ahead caching, progressive file scanning, and a keyboard-centric workflow.

Written by AI - just saying cause it's true.

## Features

- **Fast & Responsive**: Starts displaying images immediately while scanning for files in the background.
- **Intelligent Caching**: Prefetches images in your navigation direction (2:1 forward bias) to ensure instant page turns.
- **Resource Friendly**: Configurable memory budget for the image cache (default: 10% of system RAM).
- **Minimalist UI**: Software rendering with a clean, distraction-free interface.
- **Workflow Tools**:
    - **Mark Files**: Save paths of interesting images to a file or stdout for later processing.
    - **File Lists**: Load images from a text file (supports tab/space separated lists).
    - **Rotation**: Lossless visual rotation (90° steps).
- **Duplicate Finding**: Detects and groups similar images using perceptual hashing (pHash).
- **Extended Metadata**: Displays EXIF data (Date, Camera, ISO, GPS) and allows dumping to stdout.
- **Format Support**: Supports all common image formats (JPG, PNG, GIF, BMP, WebP, TIFF, etc.).

## Installation

Building from source requires a Rust toolchain.

```bash
cargo build --release
# Binary will be at ./target/release/iv
```

## Usage

```bash
iv [OPTIONS] [PATHS]...
```

### Examples

**View images in the current directory:**
```bash
iv .
```

**Recursively view a large photo collection:**
```bash
iv --recursive ~/Pictures/2024
```

**View specific files:**
```bash
iv img1.jpg img2.jpg
```

**Use a memory budget of 2GB for caching:**
```bash
iv --memory 2GB ~/Pictures
```

**Selection Workflow (Marking):**
Review images and save the paths of the ones you like to `selected.txt`:
```bash
iv -o selected.txt ~/Pictures
```
(Press `m` to mark the current file).

**Read from a file list:**
Useful for integrating with other tools like `find` or `fzf`.
```bash
find . -name "*.jpg" > list.txt
iv -L list.txt
```

**Find Duplicates (Visual Mode):**
Scan a directory for duplicate or similar images and review them in the grid view.
```bash
# Find exact or near-exact duplicates (default threshold: 2)
iv -D --recursive ~/Pictures

# Find similar images (looser threshold, e.g. resized or slightly edited)
iv -D --threshold 10 ~/Pictures
```

**Find Duplicates (Headless Dump):**
Scan for duplicates and write the results to a file without opening the UI.
```bash
iv -D --recursive --dump duplicates.txt ~/Pictures
```

**Custom Font Size:**
Start with a larger UI font size.
```bash
iv --font-size 3 ~/Pictures
```

### Key Bindings

| Key | Action |
| --- | --- |
| `Esc` / `q` | Quit |
| `Right` / `Space` / `l` | Next image |
| `Left` / `h` | Previous image |
| `Home` | Go to first image |
| `End` | Go to last image |
| `f` | Toggle fullscreen |
| `s` | Cycle font size |
| `t` | Toggle thumbnail view |
| `z` | Toggle zoom (1:1 / Fit) |
| `+` / `-` / `Wheel` | Zoom in / out |
| `r` | Rotate 90° Counter-Clockwise |
| `R` | Rotate 90° Clockwise |
| `m` | Mark current file (append path to output file) |
| `i` | Toggle info overlay |
| `M` | Dump metadata to stdout |
| `?` | Toggle help overlay |

## Configuration

`iv` accepts several command-line arguments to tune behavior:

- `-r, --recursive`: Search directories recursively.
- `-m, --memory <SIZE>`: Set cache memory limit (e.g., `512MB`, `4GB`).
- `--font-size <N>`: Initial font scale factor (default: 2).
- `--initial-delay <MS>`: Delay before key repeat starts (default: 500ms).
- `--repeat-delay <MS>`: Interval for key repeat (default: 35ms).
- `-D, --find-duplicates`: Enable duplicate finding mode.
- `--threshold <N>`: Similarity threshold for duplicates (0-64, default: 2).
- `--dump <FILE>`: Dump found duplicates to file and exit (headless).

Run `iv --help` for the full list of options.

## logging

You can enable logging to diagnose issues or watch the preloader in action:

```bash
RUST_LOG=debug iv .
```
```
