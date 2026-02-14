mod cli;
mod files;
mod loader;
mod ui;

use clap::Parser;
use std::sync::{Arc, Condvar, Mutex};
use winit::event_loop::EventLoop;

use crate::cli::{parse_memory_budget, default_memory_budget, Cli};
use crate::files::collect_images;
use crate::loader::{spawn_decode_workers, CacheState, SharedState, UserEvent};
use crate::ui::state::ViewerState;
use crate::ui::App;

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    env_logger::init();
    let cli = Cli::parse();

    let budget = match &cli.memory {
        Some(s) => parse_memory_budget(s),
        None => default_memory_budget(),
    };

    let files = collect_images(&cli.paths, cli.file_list.as_ref(), cli.recursive);
    if files.is_empty() {
        log::error!("No image files found.");
        return;
    }

    let files = Arc::new(files);
    let file_count = files.len();

    let shared: SharedState = Arc::new((
        Mutex::new(CacheState::new(budget, file_count)),
        Condvar::new(),
    ));

    let num_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(4, 16);

    let event_loop = EventLoop::<UserEvent>::with_user_event().build().expect("create event loop");
    let proxy = event_loop.create_proxy();

    // Spawn workers — they immediately start decoding from index 0 outward
    spawn_decode_workers(Arc::clone(&shared), Arc::clone(&files), proxy, num_threads);
    {
        let (_, cvar) = &*shared;
        cvar.notify_all();
    }

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
