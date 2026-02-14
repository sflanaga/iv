use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};
use softbuffer::Surface;

use crate::loader::UserEvent;
use crate::ui::state::ViewerState;

pub mod render;
pub mod state;

// ---------------------------------------------------------------------------
// Application handler (winit 0.30 style)
// ---------------------------------------------------------------------------

pub struct App {
    pub state: ViewerState,
    pub window: Option<Arc<Window>>,
    pub context: Option<softbuffer::Context<Arc<Window>>>,
    pub surface: Option<Surface<Arc<Window>, Arc<Window>>>,
    pub next_redraw: Option<Instant>,
}

impl App {
    pub fn new(state: ViewerState) -> Self {
        Self {
            state,
            window: None,
            context: None,
            surface: None,
            next_redraw: None,
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("iv")
            .with_inner_size(LogicalSize::new(1280u32, 720u32));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        let context = softbuffer::Context::new(Arc::clone(&window)).expect("create context");
        let surface = Surface::new(&context, Arc::clone(&window)).expect("create surface");

        window.request_redraw();
        self.window = Some(window);
        self.context = Some(context);
        self.surface = Some(surface);
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::ImageReady(idx) => {
                // If the ready image is the one we want to display
                if idx == self.state.current_index {
                    let (lock, _) = &*self.state.shared;
                    let state = lock.lock().unwrap();
                    if let Some(img) = state.get(idx) {
                        drop(state);
                        self.state.current_decoded = Some(img);
                        self.state.displayed_index = idx;
                        self.state.error_message = None;
                    } else if let Some(err) = state.errors.get(&idx) {
                        let msg = format!("Could not load: {}", err);
                        drop(state);
                        self.state.error_message = Some(msg);
                        self.state.current_decoded = None; // clear old image on error? or keep it? 
                        // Let's clear it so the error is visible on black background, 
                        // or we could overlay error. For now, clear to match old behavior for errors.
                        self.state.displayed_index = idx;
                    }
                    if let Some(ref window) = self.window {
                        window.request_redraw();
                    }
                }
            }
            UserEvent::FileListUpdated => {
                // Update the file count in CacheState so workers know they can look further
                let (lock, cvar) = &*self.state.shared;
                let mut state = lock.lock().unwrap();
                
                let files_guard = self.state.files.read().unwrap();
                state.file_count = files_guard.len();
                drop(files_guard);
                
                // Wake up workers to check for new work (e.g. current_index might now be valid)
                cvar.notify_all();
                
                if let Some(ref window) = self.window {
                    window.request_redraw();
                }
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }

            WindowEvent::Resized(PhysicalSize { width, height }) => {
                let w = width.max(1);
                let h = height.max(1);
                if let Some(ref mut surface) = self.surface {
                    let _ = surface.resize(
                        std::num::NonZeroU32::new(w).unwrap(),
                        std::num::NonZeroU32::new(h).unwrap(),
                    );
                }
                if let Some(ref window) = self.window {
                    window.request_redraw();
                }
            }

            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = event.state == ElementState::Pressed;
                match &event.logical_key {
                    Key::Named(named) => {
                        if pressed {
                            if !event.repeat {
                                self.state.keys_pressed.insert(*named);
                            }
                            self.state.keys_down.insert(*named);
                        } else {
                            self.state.keys_down.remove(named);
                        }
                    }
                    Key::Character(s) => {
                        if let Some(c) = s.chars().next() {
                            let c = c.to_ascii_lowercase();
                            if pressed {
                                if !event.repeat {
                                    self.state.chars_pressed.insert(c);
                                }
                                self.state.chars_down.insert(c);
                            } else {
                                self.state.chars_down.remove(&c);
                            }
                        }
                    }
                    _ => {}
                }
                if let Some(ref window) = self.window {
                    window.request_redraw();
                }
            }

            WindowEvent::MouseInput { state, button, .. } => {
                if button == MouseButton::Left {
                    if state == ElementState::Pressed {
                        self.state.dragging = true;
                        self.state.drag_start = self.state.mouse_pos;
                        self.state.drag_offset_start =
                            (self.state.offset_x, self.state.offset_y);
                    } else {
                        self.state.dragging = false;
                    }
                }
                if let Some(ref window) = self.window {
                    window.request_redraw();
                }
            }

            WindowEvent::CursorMoved {
                position: PhysicalPosition { x, y },
                ..
            } => {
                self.state.mouse_pos = (x, y);
                if self.state.dragging {
                    self.state.offset_x = self.state.drag_offset_start.0
                        + (x as f32 - self.state.drag_start.0 as f32);
                    self.state.offset_y = self.state.drag_offset_start.1
                        + (y as f32 - self.state.drag_start.1 as f32);
                    if let Some(ref window) = self.window {
                        window.request_redraw();
                    }
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let y = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(PhysicalPosition { y, .. }) => y as f32 / 40.0,
                };
                self.state.wheel_y += y;
                if let Some(ref window) = self.window {
                    window.request_redraw();
                }
            }

            WindowEvent::RedrawRequested => {
                let window = self.window.as_ref().unwrap();
                let quit = self.state.update(window);
                if quit {
                    event_loop.exit();
                    return;
                }

                if let Some(ref mut surface) = self.surface {
                    let size = window.inner_size();
                    let fb_w = size.width.max(1);
                    let fb_h = size.height.max(1);
                    if let Ok(mut buffer) = surface.buffer_mut() {
                        self.state.render(&mut buffer, fb_w, fb_h);
                        let _ = buffer.present();
                    }
                }

                // Schedule next redraw only for key-hold repeat
                let nav_keys_held = self.state.is_key_down_named(NamedKey::ArrowRight)
                    || self.state.is_key_down_named(NamedKey::ArrowLeft)
                    || self.state.is_key_down_named(NamedKey::Space)
                    || self.state.is_char_down('l')
                    || self.state.is_char_down('h');

                if nav_keys_held {
                    let delay_ms = if !self.state.nav_past_initial {
                        (self.state.initial_delay * 1000.0) as u64
                    } else {
                        (self.state.repeat_delay * 1000.0) as u64
                    };
                    self.next_redraw = Some(Instant::now() + Duration::from_millis(delay_ms.max(1)));
                } else {
                    self.next_redraw = None;
                }
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(when) = self.next_redraw {
            if Instant::now() >= when {
                self.next_redraw = None;
                if let Some(ref window) = self.window {
                    window.request_redraw();
                }
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(when));
            }
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }
}
