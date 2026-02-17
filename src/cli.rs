use clap::Parser;
use sysinfo;

pub const HELP_KEYS: &str = "\
Key Bindings:
  Esc / q       : Quit
  Left / h      : Previous image
  Right / l     : Next image
  Space         : Next image
  f             : Toggle fullscreen
  s             : Cycle font size
  t             : Toggle thumbnail view
  i             : Toggle info overlay
  M             : Dump metadata to stdout
  ?             : Toggle help overlay
  r / R         : Rotate 90° CCW / CW
  m             : Mark current file (write path to output)
  z             : Toggle zoom (1:1 / Fit)
  + / - / Wheel : Zoom in / out
  Home          : Go to first image
  End           : Go to last image
";

#[derive(Parser)]
#[command(name = "iv", about = "A simple image viewer", after_help = HELP_KEYS)]
pub struct Cli {
    /// Files or directories to view
    #[arg(required_unless_present = "file_list")]
    pub paths: Vec<std::path::PathBuf>,

    /// Load file list from a text file (one path per line)
    #[arg(short = 'L', long, value_name = "FILE")]
    pub file_list: Option<std::path::PathBuf>,

    /// Output file for marked images (appends path). Defaults to stdout if not set.
    #[arg(short = 'o', long, value_name = "FILE")]
    pub marked_file_output: Option<std::path::PathBuf>,

    /// Memory budget for image cache (e.g. 512MB, 2GB). Default: 10% of RAM.
    #[arg(short, long)]
    pub memory: Option<String>,

    /// Recurse into subdirectories
    #[arg(short, long)]
    pub recursive: bool,

    /// Follow symbolic links (default: false)
    #[arg(long)]
    pub follow_links: bool,

    /// Find duplicates / similar images
    #[arg(short = 'D', long)]
    pub find_duplicates: bool,

    /// Similarity threshold for duplicates (0-64, default: 2). Lower = stricter.
    #[arg(long, default_value = "2")]
    pub threshold: u32,

    /// Dump duplicates to the specified file and exit (requires -D)
    #[arg(long, value_name = "FILE")]
    pub dump: Option<std::path::PathBuf>,

    /// Initial delay in ms before key-hold repeat begins (default: 500)
    #[arg(long, default_value = "500")]
    pub initial_delay: u64,

    /// Key-hold repeat interval in milliseconds for navigation (default: 35)
    #[arg(long, default_value = "35")]
    pub repeat_delay: u64,

    /// Initial font size scaling factor (default: 2)
    #[arg(long, default_value = "2")]
    pub font_size: u32,
}

pub fn parse_memory_budget(s: &str) -> u64 {
    let s = s.trim().to_uppercase();
    if let Some(num) = s.strip_suffix("GB") {
        num.trim().parse::<f64>().unwrap_or(1.0) as u64 * 1024 * 1024 * 1024
    } else if let Some(num) = s.strip_suffix("MB") {
        num.trim().parse::<f64>().unwrap_or(512.0) as u64 * 1024 * 1024
    } else {
        s.parse::<f64>().unwrap_or(512.0) as u64 * 1024 * 1024
    }
}

pub fn default_memory_budget() -> u64 {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    sys.total_memory() / 10
}
