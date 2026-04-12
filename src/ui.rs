use crate::core::{
    ModelManager, RecorderState, copy_and_paste_text, has_accessibility_permission,
    transcribe_wav_file,
};
use std::thread;
use std::time::{Duration, Instant};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use winit::application::ApplicationHandler;
use winit::event::StartCause;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};

type UiResult<T> = Result<T, Box<dyn std::error::Error>>;

pub fn run() -> UiResult<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let tray_proxy = event_loop.create_proxy();
    TrayIconEvent::set_event_handler(Some(move |event| {
        let _ = tray_proxy.send_event(UserEvent::TrayIconEvent(event));
    }));

    let mut app = WhisperingMvpApp::new(event_loop.create_proxy());
    event_loop.run_app(&mut app)?;
    Ok(())
}

enum UserEvent {
    TrayIconEvent(TrayIconEvent),
    WorkerEvent(WorkerEvent),
}

enum WorkerEvent {
    Success(String),
    PasteFailed { transcript: String, error: String },
    Failed(String),
}

enum TrayVisualState {
    Idle,
    Recording,
    Transcribing(usize),
}

pub struct WhisperingMvpApp {
    proxy: EventLoopProxy<UserEvent>,
    tray_icon: Option<TrayIcon>,
    recorder: RecorderState,
    model_manager: ModelManager,
    status: String,
    is_transcribing: bool,
    spinner_phase: usize,
    last_spinner_tick: Instant,
}

impl WhisperingMvpApp {
    fn new(proxy: EventLoopProxy<UserEvent>) -> Self {
        let status = if has_accessibility_permission() {
            "Ready. Left click the microphone in the menu bar to dictate.".to_string()
        } else {
            "Ready. Left click to dictate. Auto-paste will ask for Accessibility permission when needed.".to_string()
        };

        Self {
            proxy,
            tray_icon: None,
            recorder: RecorderState::new(),
            model_manager: ModelManager::new(),
            status,
            is_transcribing: false,
            spinner_phase: 0,
            last_spinner_tick: Instant::now(),
        }
    }

    fn build_tray_icon(&self) -> UiResult<TrayIcon> {
        Ok(TrayIconBuilder::new()
            .with_tooltip(self.tooltip())
            .with_icon(icon_for_state(TrayVisualState::Idle))
            .with_icon_as_template(true)
            .with_menu_on_left_click(false)
            .with_menu_on_right_click(false)
            .build()?)
    }

    fn tooltip(&self) -> String {
        format!("Diktovani\n{}", self.status)
    }

    fn refresh_tray(&mut self, state: TrayVisualState) {
        let Some(tray_icon) = self.tray_icon.as_ref() else {
            return;
        };

        if let Err(err) = tray_icon.set_tooltip(Some(self.tooltip())) {
            eprintln!("[tray] failed to update tooltip: {err}");
        }
        if let Err(err) = tray_icon.set_icon_with_as_template(Some(icon_for_state(state)), true) {
            eprintln!("[tray] failed to update icon: {err}");
        }
    }

    fn set_status(&mut self, status: impl Into<String>) {
        self.status = status.into();
        let state = if self.is_transcribing {
            TrayVisualState::Transcribing(self.spinner_phase)
        } else if self.recorder.is_recording() {
            TrayVisualState::Recording
        } else {
            TrayVisualState::Idle
        };
        self.refresh_tray(state);
    }

    fn toggle_recording(&mut self) {
        if self.is_transcribing {
            self.set_status("Wait for the current transcription to finish.");
            return;
        }

        if self.recorder.is_recording() {
            match self.recorder.stop_recording() {
                Ok(recording) => {
                    self.is_transcribing = true;
                    self.spinner_phase = 0;
                    self.last_spinner_tick = Instant::now();
                    self.set_status(format!(
                        "Recording stopped ({:.1}s, {} Hz, {} ch). Transcribing...",
                        recording.duration_seconds, recording.sample_rate, recording.channels
                    ));

                    let proxy = self.proxy.clone();
                    let model_manager = self.model_manager.clone();
                    let file_path = recording.file_path;
                    thread::spawn(move || {
                        let event = match transcribe_wav_file(&model_manager, &file_path) {
                            Ok(transcript) => {
                                println!("{transcript}");
                                match copy_and_paste_text(&transcript) {
                                    Ok(()) => WorkerEvent::Success(transcript),
                                    Err(err) => WorkerEvent::PasteFailed {
                                        transcript,
                                        error: err.to_string(),
                                    },
                                }
                            }
                            Err(err) => WorkerEvent::Failed(err.to_string()),
                        };

                        if let Err(err) = std::fs::remove_file(&file_path) {
                            eprintln!(
                                "[recording] failed to remove temp file {}: {err}",
                                file_path.display()
                            );
                        }
                        let _ = proxy.send_event(UserEvent::WorkerEvent(event));
                    });
                }
                Err(err) => {
                    self.set_status(format!("Stop failed: {err}"));
                }
            }
            return;
        }

        match self.recorder.start_new_recording() {
            Ok(()) => {
                let model_manager = self.model_manager.clone();
                thread::spawn(move || {
                    if let Err(err) = model_manager.preload_whisper() {
                        eprintln!("[preload] failed: {err}");
                    }
                });
                self.set_status("Recording... left click again to stop.");
            }
            Err(err) => {
                self.set_status(format!("Start failed: {err}"));
            }
        }
    }

    fn tick_spinner(&mut self) {
        if !self.is_transcribing || self.last_spinner_tick.elapsed() < Duration::from_millis(100) {
            return;
        }

        self.spinner_phase = (self.spinner_phase + 1) % 12;
        self.last_spinner_tick = Instant::now();
        self.refresh_tray(TrayVisualState::Transcribing(self.spinner_phase));
    }
}

impl ApplicationHandler<UserEvent> for WhisperingMvpApp {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _window_id: winit::window::WindowId,
        _event: winit::event::WindowEvent,
    ) {
    }

    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        if cause != StartCause::Init {
            return;
        }

        match self.build_tray_icon() {
            Ok(tray_icon) => {
                self.tray_icon = Some(tray_icon);
                self.refresh_tray(TrayVisualState::Idle);
            }
            Err(err) => {
                eprintln!("[tray] failed to create tray icon: {err}");
            }
        }

        #[cfg(target_os = "macos")]
        unsafe {
            let run_loop = core_foundation_sys::runloop::CFRunLoopGetMain();
            core_foundation_sys::runloop::CFRunLoopWakeUp(run_loop);
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        self.model_manager.unload_if_idle();

        match event {
            UserEvent::TrayIconEvent(TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }) => self.toggle_recording(),
            UserEvent::TrayIconEvent(_) => {}
            UserEvent::WorkerEvent(event) => {
                self.is_transcribing = false;
                self.spinner_phase = 0;
                match event {
                    WorkerEvent::Success(transcript) => {
                        let preview = preview_text(&transcript);
                        self.set_status(format!("Transcript pasted. {preview}"));
                    }
                    WorkerEvent::PasteFailed { transcript, error } => {
                        let preview = preview_text(&transcript);
                        self.set_status(format!("Transcript ready but paste failed: {error}. {preview}"));
                    }
                    WorkerEvent::Failed(error) => {
                        self.set_status(format!("Transcription failed: {error}"));
                    }
                }
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.model_manager.unload_if_idle();
        self.tick_spinner();

        let next_tick = if self.is_transcribing {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(1)
        };
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + next_tick));
    }
}

fn preview_text(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "Transcript was empty.".to_string();
    }

    let mut preview = trimmed.chars().take(60).collect::<String>();
    if trimmed.chars().count() > 60 {
        preview.push_str("...");
    }
    format!("Last transcript: {preview}")
}

fn icon_for_state(state: TrayVisualState) -> Icon {
    let width = 32;
    let height = 32;
    let mut rgba = vec![0u8; width * height * 4];

    match state {
        TrayVisualState::Idle => draw_microphone(&mut rgba, width, height, false),
        TrayVisualState::Recording => draw_microphone(&mut rgba, width, height, true),
        TrayVisualState::Transcribing(phase) => draw_spinner(&mut rgba, width, height, phase),
    }

    Icon::from_rgba(rgba, width as u32, height as u32).expect("valid tray icon")
}

fn draw_microphone(rgba: &mut [u8], width: usize, height: usize, filled: bool) {
    for y in 0..height as i32 {
        for x in 0..width as i32 {
            let dx = x - 16;
            let dy = y - 11;
            let head = dx * dx + dy * dy <= 36;
            let body = (13..=19).contains(&x) && (11..=19).contains(&y);
            let stem = (15..=17).contains(&x) && (20..=25).contains(&y);
            let base = (11..=21).contains(&x) && (25..=27).contains(&y);

            if head || body || stem || base {
                if !filled && y < 18 && x > 13 && x < 19 && head {
                    continue;
                }
                set_pixel(rgba, width, x as usize, y as usize, 0, 0, 0, 255);
            }
        }
    }
}

fn draw_spinner(rgba: &mut [u8], width: usize, height: usize, phase: usize) {
    let center_x = 16.0f32;
    let center_y = 16.0f32;
    let radius = 10.0f32;
    let dot_radius = 2.4f32;

    for index in 0..12 {
        let angle = std::f32::consts::TAU * index as f32 / 12.0 - std::f32::consts::FRAC_PI_2;
        let dot_x = center_x + radius * angle.cos();
        let dot_y = center_y + radius * angle.sin();
        let intensity_step = (12 + index + phase - 1) % 12;
        let alpha = ((intensity_step + 1) as f32 / 12.0 * 255.0) as u8;

        draw_filled_circle(rgba, width, height, dot_x, dot_y, dot_radius, alpha);
    }
}

fn draw_filled_circle(
    rgba: &mut [u8],
    width: usize,
    height: usize,
    center_x: f32,
    center_y: f32,
    radius: f32,
    alpha: u8,
) {
    let min_x = (center_x - radius).floor().max(0.0) as usize;
    let max_x = (center_x + radius).ceil().min((width - 1) as f32) as usize;
    let min_y = (center_y - radius).floor().max(0.0) as usize;
    let max_y = (center_y + radius).ceil().min((height - 1) as f32) as usize;

    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let dx = x as f32 - center_x;
            let dy = y as f32 - center_y;
            if dx * dx + dy * dy <= radius * radius {
                set_pixel(rgba, width, x, y, 0, 0, 0, alpha);
            }
        }
    }
}

fn set_pixel(rgba: &mut [u8], width: usize, x: usize, y: usize, r: u8, g: u8, b: u8, a: u8) {
    let index = (y * width + x) * 4;
    rgba[index] = r;
    rgba[index + 1] = g;
    rgba[index + 2] = b;
    rgba[index + 3] = a;
}
