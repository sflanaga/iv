mod cli;
mod files;
mod loader;
mod ui;

use clap::Parser;
use std::sync::{Arc, Condvar, Mutex, RwLock};
use winit::event_loop::EventLoop;

use crate::cli::{parse_memory_budget, default_memory_budget, Cli};
use crate::files::spawn_file_scanner;
use crate::loader::{spawn_decode_workers, CacheState, SharedState, UserEvent};
use crate::ui::state::ViewerState;
use crate::ui::App;

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();

    let budget = match &cli.memory {
        Some(s) => parse_memory_budget(s),
        None => default_memory_budget(),
    };

    // Shared file list, initially empty. Populated by background scanner.
    let files = Arc::new(RwLock::new(Vec::new()));
    
    // Initial file count is 0. Will be updated via UserEvent::FileListUpdated.
    let shared: SharedState = Arc::new((
        Mutex::new(CacheState::new(budget, 0)),
        Condvar::new(),
    ));

    let num_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(4, 16);

    let event_loop = EventLoop::<UserEvent>::with_user_event().build().expect("create event loop");
    let proxy = event_loop.create_proxy();

    // Spawn file scanner (producer)
    spawn_file_scanner(
        cli.paths.clone(),
        cli.file_list.clone(),
        cli.recursive,
        Arc::clone(&files),
        proxy.clone(),
    );

    // Spawn decode workers (consumers)
    spawn_decode_workers(Arc::clone(&shared), Arc::clone(&files), proxy, num_threads);

    let initial_delay = cli.initial_delay as f64 / 1000.0;
    let repeat_delay = cli.repeat_delay as f64 / 1000.0;

    let state = ViewerState::new(
        files, 
        shared, 
        initial_delay, 
        repeat_delay, 
        cli.marked_file_output
    );

    let mut app = App::new(state);

    event_loop.run_app(&mut app).expect("run event loop");
}
