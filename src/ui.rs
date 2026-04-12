use crate::core::{
    ModelManager, RecorderState, StatusCallback, copy_and_paste_text, copy_text_to_clipboard,
    ensure_model_cached, has_accessibility_permission, is_launch_at_login_enabled,
    request_accessibility_permission_if_needed, set_launch_at_login, transcribe_wav_file,
};
#[cfg(target_os = "macos")]
use objc2::MainThreadMarker;
#[cfg(target_os = "macos")]
use objc2_app_kit::NSImage;
#[cfg(target_os = "macos")]
use objc2_foundation::NSString;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem};
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
    let menu_proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = menu_proxy.send_event(UserEvent::MenuEvent(event));
    }));

    let mut app = WhisperingMvpApp::new(event_loop.create_proxy());
    event_loop.run_app(&mut app)?;
    Ok(())
}

enum UserEvent {
    TrayIconEvent(TrayIconEvent),
    MenuEvent(MenuEvent),
    WorkerEvent(WorkerEvent),
}

enum WorkerEvent {
    Status(String),
    Success(String),
    PasteFailed { transcript: String, error: String },
    Failed(String),
}

#[derive(Clone, Copy)]
enum TrayVisualState {
    Idle,
    Recording,
    Transcribing(usize),
}

pub struct WhisperingMvpApp {
    proxy: EventLoopProxy<UserEvent>,
    tray_icon: Option<TrayIcon>,
    copy_last_transcript_item: Option<MenuItem>,
    launch_at_login_item: Option<CheckMenuItem>,
    status_item: Option<MenuItem>,
    quit_item: Option<MenuItem>,
    recorder: RecorderState,
    model_manager: ModelManager,
    last_transcript: String,
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
            copy_last_transcript_item: None,
            launch_at_login_item: None,
            status_item: None,
            quit_item: None,
            recorder: RecorderState::new(),
            model_manager: ModelManager::new(),
            last_transcript: String::new(),
            status,
            is_transcribing: false,
            spinner_phase: 0,
            last_spinner_tick: Instant::now(),
        }
    }

    fn build_tray_icon(
        &self,
    ) -> UiResult<(TrayIcon, MenuItem, CheckMenuItem, MenuItem, MenuItem)> {
        let menu = Menu::new();
        let copy_last_transcript_item = MenuItem::new(
            copy_last_transcript_menu_text(&self.last_transcript),
            !self.last_transcript.trim().is_empty(),
            None,
        );
        let launch_at_login_item = CheckMenuItem::new(
            "Spouštět po startu systému",
            true,
            is_launch_at_login_enabled(),
            None,
        );
        let status_item = MenuItem::new(status_menu_text(&self.status), false, None);
        let quit_item = MenuItem::new("Quit", true, None);
        menu.append(&status_item)?;
        menu.append(&copy_last_transcript_item)?;
        menu.append(&launch_at_login_item)?;
        menu.append(&quit_item)?;

        let tray_icon = TrayIconBuilder::new()
            .with_tooltip(self.tooltip())
            .with_icon(icon_for_state(TrayVisualState::Idle))
            .with_menu(Box::new(menu))
            .with_icon_as_template(true)
            .with_menu_on_left_click(false)
            .with_menu_on_right_click(true)
            .build()?;

        Ok((
            tray_icon,
            copy_last_transcript_item,
            launch_at_login_item,
            status_item,
            quit_item,
        ))
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
        if let Some(status_item) = self.status_item.as_ref() {
            status_item.set_text(status_menu_text(&self.status));
        }
        if let Some(copy_last_transcript_item) = self.copy_last_transcript_item.as_ref() {
            copy_last_transcript_item
                .set_text(copy_last_transcript_menu_text(&self.last_transcript));
            copy_last_transcript_item.set_enabled(!self.last_transcript.trim().is_empty());
        }
        if apply_macos_symbol(tray_icon, state)
        {
            return;
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
                    let status_callback = status_callback(proxy.clone());
                    let file_path = recording.file_path;
                    thread::spawn(move || {
                        let event = match transcribe_wav_file(
                            &model_manager,
                            &file_path,
                            Some(&status_callback),
                        ) {
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
                let status_callback = status_callback(self.proxy.clone());
                thread::spawn(move || {
                    if let Err(err) = model_manager.preload_whisper(Some(&status_callback)) {
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
            Ok((
                tray_icon,
                copy_last_transcript_item,
                launch_at_login_item,
                status_item,
                quit_item,
            )) => {
                self.tray_icon = Some(tray_icon);
                self.copy_last_transcript_item = Some(copy_last_transcript_item);
                self.launch_at_login_item = Some(launch_at_login_item);
                self.status_item = Some(status_item);
                self.quit_item = Some(quit_item);
                self.refresh_tray(TrayVisualState::Idle);

                let status_callback = status_callback(self.proxy.clone());
                thread::spawn(move || {
                    if let Err(err) = ensure_model_cached(Some(&status_callback)) {
                        status_callback(format!("Model download failed: {err}"));
                    }
                });

                if !request_accessibility_permission_if_needed() {
                    self.set_status(
                        "Accessibility permission is needed for auto-paste. macOS settings were opened.",
                    );
                }
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

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        self.model_manager.unload_if_idle();

        match event {
            UserEvent::TrayIconEvent(TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }) => self.toggle_recording(),
            UserEvent::TrayIconEvent(_) => {}
            UserEvent::MenuEvent(event) => {
                if self
                    .copy_last_transcript_item
                    .as_ref()
                    .is_some_and(|item| event.id == *item.id())
                {
                    match copy_text_to_clipboard(&self.last_transcript) {
                        Ok(()) => self.set_status("Last transcript copied to clipboard."),
                        Err(err) => {
                            self.set_status(format!("Failed to copy last transcript: {err}"))
                        }
                    }
                    return;
                }
                if self
                    .launch_at_login_item
                    .as_ref()
                    .is_some_and(|item| event.id == *item.id())
                {
                    let enabled = self
                        .launch_at_login_item
                        .as_ref()
                        .map(|item| item.is_checked())
                        .unwrap_or(false);
                    match set_launch_at_login(enabled) {
                        Ok(()) => {
                            self.set_status(if enabled {
                                "Autostart enabled."
                            } else {
                                "Autostart disabled."
                            });
                        }
                        Err(err) => {
                            if let Some(item) = self.launch_at_login_item.as_ref() {
                                item.set_checked(!enabled);
                            }
                            self.set_status(format!("Failed to update autostart: {err}"));
                        }
                    }
                    return;
                }
                if self.quit_item.as_ref().is_some_and(|item| event.id == *item.id()) {
                    event_loop.exit();
                }
            }
            UserEvent::WorkerEvent(event) => {
                match event {
                    WorkerEvent::Status(status) => {
                        self.set_status(status);
                    }
                    WorkerEvent::Success(transcript) => {
                        self.is_transcribing = false;
                        self.spinner_phase = 0;
                        self.last_transcript = transcript.clone();
                        let preview = preview_text(&transcript);
                        self.set_status(format!("Transcript pasted. {preview}"));
                    }
                    WorkerEvent::PasteFailed { transcript, error } => {
                        self.is_transcribing = false;
                        self.spinner_phase = 0;
                        self.last_transcript = transcript.clone();
                        let preview = preview_text(&transcript);
                        self.set_status(format!("Transcript ready but paste failed: {error}. {preview}"));
                    }
                    WorkerEvent::Failed(error) => {
                        self.is_transcribing = false;
                        self.spinner_phase = 0;
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

fn status_menu_text(status: &str) -> String {
    let compact = status.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut text = compact.chars().take(80).collect::<String>();
    if compact.chars().count() > 80 {
        text.push_str("...");
    }
    format!("Status: {text}")
}

fn copy_last_transcript_menu_text(transcript: &str) -> String {
    let compact = transcript.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        return "Zkopírovat poslední přepis: —".to_string();
    }
    format!("Zkopírovat poslední přepis: {compact}")
}

fn status_callback(proxy: EventLoopProxy<UserEvent>) -> StatusCallback {
    Arc::new(move |status| {
        let _ = proxy.send_event(UserEvent::WorkerEvent(WorkerEvent::Status(status)));
    })
}

fn icon_for_state(state: TrayVisualState) -> Icon {
    match state {
        TrayVisualState::Idle => load_microphone_icon(),
        TrayVisualState::Recording => draw_checkmark_icon(),
        TrayVisualState::Transcribing(phase) => draw_spinner_icon(phase),
    }
}

fn load_microphone_icon() -> Icon {
    let image = image::load_from_memory_with_format(
        include_bytes!("../assets/AppIcon.appiconset/icon_32x32.png"),
        image::ImageFormat::Png,
    )
    .expect("embedded microphone icon should decode")
    .into_rgba8();
    let (width, height) = image.dimensions();
    Icon::from_rgba(image.into_raw(), width, height).expect("valid embedded tray icon")
}

#[cfg(target_os = "macos")]
fn apply_macos_symbol(tray_icon: &TrayIcon, state: TrayVisualState) -> bool {
    let Some(status_item) = tray_icon.ns_status_item() else {
        return false;
    };
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let Some(button) = status_item.button(mtm) else {
        return false;
    };

    let (symbol_name, description) = match state {
        TrayVisualState::Idle => ("mic.fill", "Microphone"),
        TrayVisualState::Recording => ("checkmark", "Stop recording"),
        TrayVisualState::Transcribing(_) => return false,
    };
    let symbol_name = NSString::from_str(symbol_name);
    let description = NSString::from_str(description);
    let Some(image) =
        NSImage::imageWithSystemSymbolName_accessibilityDescription(&symbol_name, Some(&description))
    else {
        return false;
    };

    image.setTemplate(true);
    button.setImage(Some(&image));
    true
}

#[cfg(not(target_os = "macos"))]
fn apply_macos_symbol(_tray_icon: &TrayIcon, _state: TrayVisualState) -> bool {
    false
}

fn draw_checkmark_icon() -> Icon {
    let width = 32;
    let height = 32;
    let mut rgba = vec![0u8; width * height * 4];

    for offset in 0..6 {
        let x = 8 + offset;
        let y = 17 + offset;
        draw_stroke(&mut rgba, width, x, y, 2);
    }

    for offset in 0..12 {
        let x = 13 + offset;
        let y = 22 - offset;
        draw_stroke(&mut rgba, width, x, y, 2);
    }

    Icon::from_rgba(rgba, width as u32, height as u32).expect("valid checkmark tray icon")
}

fn draw_spinner_icon(phase: usize) -> Icon {
    let width = 32;
    let height = 32;
    let mut rgba = vec![0u8; width * height * 4];
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

        draw_filled_circle(&mut rgba, width, height, dot_x, dot_y, dot_radius, alpha);
    }

    Icon::from_rgba(rgba, width as u32, height as u32).expect("valid spinner tray icon")
}

fn draw_stroke(rgba: &mut [u8], width: usize, x: usize, y: usize, radius: usize) {
    let start_x = x.saturating_sub(radius);
    let start_y = y.saturating_sub(radius);
    let end_x = (x + radius).min(width - 1);
    let end_y = (y + radius).min((rgba.len() / 4 / width).saturating_sub(1));

    for py in start_y..=end_y {
        for px in start_x..=end_x {
            set_pixel(rgba, width, px, py, 0, 0, 0, 255);
        }
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
