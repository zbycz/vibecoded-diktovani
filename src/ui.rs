use crate::core::{
    ModelManager, ProgressCallback, RecorderState, StatusCallback, copy_and_paste_text,
    copy_text_to_clipboard, ensure_model_cached, has_accessibility_permission,
    is_launch_at_login_enabled, request_accessibility_permission_if_needed, set_launch_at_login,
    transcribe_wav_file,
};
use crate::bubble::{Bubble, BubbleState};
use crate::hotkey::{FnTap, HotkeyMonitor, install_fn_tap_monitor};
use crate::icons::{draw_checkmark_icon, draw_progress_icon, load_microphone_icon};
#[cfg(target_os = "macos")]
use objc2::MainThreadMarker;
#[cfg(target_os = "macos")]
use objc2_app_kit::NSImage;
#[cfg(target_os = "macos")]
use objc2_foundation::NSString;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
    FnTap(FnTap),
    Cancel,
    WorkerEvent(WorkerEvent),
}

enum WorkerEvent {
    Status(String),
    TranscriptionProgress(u8),
    Success(String),
    PasteFailed { transcript: String, error: String },
    Failed(String),
    Cancelled,
}

#[derive(Clone, Copy)]
enum TrayVisualState {
    Idle,
    Recording,
    Transcribing { progress: u8, submit: bool }, // progress 0–100, submit = auto-send with Enter
}

pub struct WhisperingMvpApp {
    proxy: EventLoopProxy<UserEvent>,
    tray_icon: Option<TrayIcon>,
    copy_last_transcript_item: Option<MenuItem>,
    launch_at_login_item: Option<CheckMenuItem>,
    status_item: Option<MenuItem>,
    quit_item: Option<MenuItem>,
    hotkey_monitor: Option<HotkeyMonitor>,
    waiting_for_accessibility_hotkey: bool,
    recorder: RecorderState,
    model_manager: ModelManager,
    last_transcript: String,
    status: String,
    is_transcribing: bool,
    transcription_progress: u8,
    /// When toggled on during transcription, the finished transcript is pasted
    /// and immediately submitted with Enter. Shared with the worker thread so it
    /// can be flipped while transcription is already running.
    submit_after_transcription: Arc<AtomicBool>,
    /// Set when the user cancels the in-progress transcription. A fresh handle is
    /// created per transcription so a cancelled (still-running) worker can never
    /// paste, even if a new recording is started right after.
    cancel_flag: Arc<AtomicBool>,
    bubble: Option<Bubble>,
}

impl WhisperingMvpApp {
    fn new(proxy: EventLoopProxy<UserEvent>) -> Self {
        let has_accessibility = has_accessibility_permission();
        let status = if has_accessibility {
            "Ready. Left click the microphone in the menu bar or double-press Fn/Globe to dictate."
                .to_string()
        } else {
            "Ready. Left click to dictate. Enable Accessibility for Diktovani to turn on auto-paste and the Fn/Globe hotkey."
                .to_string()
        };

        Self {
            proxy,
            tray_icon: None,
            copy_last_transcript_item: None,
            launch_at_login_item: None,
            status_item: None,
            quit_item: None,
            hotkey_monitor: None,
            waiting_for_accessibility_hotkey: !has_accessibility,
            recorder: RecorderState::new(),
            model_manager: ModelManager::new(),
            last_transcript: String::new(),
            status,
            is_transcribing: false,
            transcription_progress: 0,
            submit_after_transcription: Arc::new(AtomicBool::new(false)),
            cancel_flag: Arc::new(AtomicBool::new(false)),
            bubble: None,
        }
    }

    fn build_tray_icon(&self) -> UiResult<(TrayIcon, MenuItem, CheckMenuItem, MenuItem, MenuItem)> {
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
        if apply_macos_symbol(tray_icon, state) {
            return;
        }
        if let Err(err) = tray_icon.set_icon_with_as_template(Some(icon_for_state(state)), true) {
            eprintln!("[tray] failed to update icon: {err}");
        }
    }

    fn current_visual_state(&self) -> TrayVisualState {
        if self.is_transcribing {
            TrayVisualState::Transcribing {
                progress: self.transcription_progress,
                submit: self.submit_after_transcription.load(Ordering::SeqCst),
            }
        } else if self.recorder.is_recording() {
            TrayVisualState::Recording
        } else {
            TrayVisualState::Idle
        }
    }

    fn set_status(&mut self, status: impl Into<String>) {
        self.status = status.into();
        let state = self.current_visual_state();
        self.refresh_tray(state);
    }

    /// Tray left click: start/stop recording when idle, or toggle "submit with
    /// Enter" while a transcription is in progress.
    fn handle_primary_action(&mut self) {
        if self.is_transcribing {
            self.toggle_submit_after_transcription();
        } else {
            self.toggle_recording();
        }
    }

    /// Fn/Globe hotkey. A *double* tap starts recording from idle. A *single*
    /// tap stops an in-progress recording, or toggles submit-with-Enter while a
    /// transcription is running. A single tap while idle is ignored so the Fn
    /// key keeps working normally.
    fn handle_fn_tap(&mut self, tap: FnTap) {
        if self.is_transcribing {
            if tap == FnTap::Single {
                self.toggle_submit_after_transcription();
            }
        } else if self.recorder.is_recording() {
            if tap == FnTap::Single {
                self.toggle_recording();
            }
        } else if tap == FnTap::Double {
            self.toggle_recording();
        }
    }

    /// "Zrušit" from the bubble (or, in future, a key): throw away the current
    /// recording, or mark the running transcription so its result is discarded.
    fn cancel_current(&mut self) {
        if self.recorder.is_recording() {
            match self.recorder.stop_recording() {
                Ok(recording) => {
                    if let Err(err) = std::fs::remove_file(&recording.file_path) {
                        eprintln!(
                            "[recording] failed to remove cancelled temp file {}: {err}",
                            recording.file_path.display()
                        );
                    }
                }
                Err(err) => eprintln!("[recording] cancel stop failed: {err}"),
            }
            self.hide_bubble();
            self.set_status("Nahrávání zrušeno.");
        } else if self.is_transcribing {
            self.cancel_flag.store(true, Ordering::SeqCst);
            self.is_transcribing = false;
            self.transcription_progress = 0;
            self.submit_after_transcription.store(false, Ordering::SeqCst);
            self.hide_bubble();
            self.set_status("Přepis zrušen.");
        }
    }

    fn show_bubble(&mut self, state: BubbleState) {
        let anchor = self.status_item_screen_rect();
        if let Some(bubble) = self.bubble.as_ref() {
            match anchor {
                Some(rect) => bubble.show(state, rect),
                None => bubble.show(state, (0.0, 0.0, 0.0, 0.0)),
            }
        }
    }

    fn update_bubble(&mut self, state: BubbleState) {
        if let Some(bubble) = self.bubble.as_ref() {
            bubble.update(state);
        }
    }

    fn hide_bubble(&mut self) {
        if let Some(bubble) = self.bubble.as_ref() {
            bubble.hide();
        }
    }

    /// Screen rect of the menu-bar icon (origin bottom-left), used to anchor the
    /// popup bubble directly under it.
    #[cfg(target_os = "macos")]
    fn status_item_screen_rect(&self) -> Option<(f64, f64, f64, f64)> {
        use objc2::runtime::AnyObject;
        let tray_icon = self.tray_icon.as_ref()?;
        let status_item = tray_icon.ns_status_item()?;
        let mtm = MainThreadMarker::new()?;
        let button = status_item.button(mtm)?;
        unsafe {
            let window: *mut AnyObject = objc2::msg_send![&*button, window];
            if window.is_null() {
                return None;
            }
            let frame: objc2_foundation::NSRect = objc2::msg_send![window, frame];
            Some((
                frame.origin.x,
                frame.origin.y,
                frame.size.width,
                frame.size.height,
            ))
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn status_item_screen_rect(&self) -> Option<(f64, f64, f64, f64)> {
        None
    }

    fn toggle_submit_after_transcription(&mut self) {
        let enabled = !self.submit_after_transcription.load(Ordering::SeqCst);
        self.submit_after_transcription
            .store(enabled, Ordering::SeqCst);
        if enabled {
            self.set_status("Po přepisu se text vloží a odešle (Enter).");
        } else {
            self.set_status("Po přepisu se text jen vloží.");
        }
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
                    self.transcription_progress = 0;
                    self.submit_after_transcription.store(false, Ordering::SeqCst);
                    self.cancel_flag = Arc::new(AtomicBool::new(false));
                    self.update_bubble(BubbleState::Transcribing);
                    self.set_status(format!(
                        "Recording stopped ({:.1}s, {} Hz, {} ch). Transcribing...",
                        recording.duration_seconds, recording.sample_rate, recording.channels
                    ));

                    let proxy = self.proxy.clone();
                    let model_manager = self.model_manager.clone();
                    let status_callback = status_callback(proxy.clone());
                    let progress_cb = progress_callback(proxy.clone());
                    let file_path = recording.file_path;
                    let submit_flag = self.submit_after_transcription.clone();
                    let cancel_flag = self.cancel_flag.clone();
                    let audio_seconds = recording.duration_seconds;
                    thread::spawn(move || {
                        let transcription_started = Instant::now();
                        let event = match transcribe_wav_file(
                            &model_manager,
                            &file_path,
                            Some(&status_callback),
                            Some(&progress_cb),
                        ) {
                            Ok(transcript) => {
                                println!("{transcript}");
                                crate::core::append_transcription_log(
                                    audio_seconds,
                                    transcription_started.elapsed().as_secs_f32(),
                                    transcript.len(),
                                );
                                if cancel_flag.load(Ordering::SeqCst) {
                                    WorkerEvent::Cancelled
                                } else {
                                    let submit = submit_flag.load(Ordering::SeqCst);
                                    match copy_and_paste_text(&transcript, submit) {
                                        Ok(()) => WorkerEvent::Success(transcript),
                                        Err(err) => WorkerEvent::PasteFailed {
                                            transcript,
                                            error: err.to_string(),
                                        },
                                    }
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
                self.show_bubble(BubbleState::Recording);
                self.set_status("Recording... left click again to stop.");
            }
            Err(err) => {
                self.set_status(format!("Start failed: {err}"));
            }
        }
    }

    fn install_hotkey_monitor(&mut self) {
        if self.hotkey_monitor.is_some() {
            self.waiting_for_accessibility_hotkey = false;
            return;
        }

        let proxy = self.proxy.clone();
        match install_fn_tap_monitor(move |tap| {
            let _ = proxy.send_event(UserEvent::FnTap(tap));
        }) {
            Ok(monitor) => {
                self.hotkey_monitor = Some(monitor);
                self.waiting_for_accessibility_hotkey = false;
            }
            Err(err) => {
                eprintln!("[hotkey] {err}");
            }
        }
    }

    fn refresh_accessibility_hotkey(&mut self) {
        if !self.waiting_for_accessibility_hotkey || self.hotkey_monitor.is_some() {
            return;
        }

        if !has_accessibility_permission() {
            return;
        }

        self.install_hotkey_monitor();
        if self.hotkey_monitor.is_some() && !self.recorder.is_recording() && !self.is_transcribing {
            self.set_status(
                "Accessibility granted. Left click the microphone or double-press Fn/Globe to dictate.",
            );
        }
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

                let cancel_proxy = self.proxy.clone();
                self.bubble = Some(Bubble::new(Box::new(move || {
                    let _ = cancel_proxy.send_event(UserEvent::Cancel);
                })));

                let status_callback = status_callback(self.proxy.clone());
                thread::spawn(move || {
                    if let Err(err) = ensure_model_cached(Some(&status_callback)) {
                        status_callback(format!("Model download failed: {err}"));
                    }
                });

                if !request_accessibility_permission_if_needed() {
                    self.waiting_for_accessibility_hotkey = true;
                    self.set_status(
                        "Accessibility permission is needed for auto-paste and the Fn/Globe hotkey. macOS settings were opened.",
                    );
                }
                if has_accessibility_permission() {
                    self.install_hotkey_monitor();
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
            UserEvent::FnTap(tap) => self.handle_fn_tap(tap),
            UserEvent::Cancel => self.cancel_current(),
            UserEvent::TrayIconEvent(TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }) => self.handle_primary_action(),
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
                if self
                    .quit_item
                    .as_ref()
                    .is_some_and(|item| event.id == *item.id())
                {
                    event_loop.exit();
                }
            }
            UserEvent::WorkerEvent(event) => match event {
                WorkerEvent::Status(status) => {
                    self.set_status(status);
                }
                WorkerEvent::TranscriptionProgress(p) => {
                    self.transcription_progress = p;
                    let state = self.current_visual_state();
                    self.refresh_tray(state);
                }
                WorkerEvent::Success(transcript) => {
                    self.is_transcribing = false;
                    self.transcription_progress = 0;
                    self.submit_after_transcription.store(false, Ordering::SeqCst);
                    self.hide_bubble();
                    self.last_transcript = transcript.clone();
                    let preview = preview_text(&transcript);
                    self.set_status(format!("Transcript pasted. {preview}"));
                }
                WorkerEvent::PasteFailed { transcript, error } => {
                    self.is_transcribing = false;
                    self.transcription_progress = 0;
                    self.submit_after_transcription.store(false, Ordering::SeqCst);
                    self.hide_bubble();
                    self.last_transcript = transcript.clone();
                    let preview = preview_text(&transcript);
                    self.set_status(format!(
                        "Transcript ready but paste failed: {error}. {preview}"
                    ));
                }
                WorkerEvent::Failed(error) => {
                    self.is_transcribing = false;
                    self.transcription_progress = 0;
                    self.submit_after_transcription.store(false, Ordering::SeqCst);
                    self.hide_bubble();
                    self.set_status(format!("Transcription failed: {error}"));
                }
                WorkerEvent::Cancelled => {
                    self.is_transcribing = false;
                    self.transcription_progress = 0;
                    self.submit_after_transcription.store(false, Ordering::SeqCst);
                    self.hide_bubble();
                    self.set_status("Přepis zrušen.");
                }
            },
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.model_manager.unload_if_idle();
        self.refresh_accessibility_hotkey();

        event_loop.set_control_flow(ControlFlow::WaitUntil(
            Instant::now() + Duration::from_secs(1),
        ));
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

fn progress_callback(proxy: EventLoopProxy<UserEvent>) -> ProgressCallback {
    Arc::new(move |pct| {
        let _ = proxy.send_event(UserEvent::WorkerEvent(WorkerEvent::TranscriptionProgress(
            pct,
        )));
    })
}

fn icon_for_state(state: TrayVisualState) -> Icon {
    match state {
        TrayVisualState::Idle => load_microphone_icon(),
        TrayVisualState::Recording => draw_checkmark_icon(),
        TrayVisualState::Transcribing { progress, submit } => draw_progress_icon(progress, submit),
    }
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
        TrayVisualState::Transcribing { .. } => return false,
    };
    let symbol_name = NSString::from_str(symbol_name);
    let description = NSString::from_str(description);
    let Some(image) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &symbol_name,
        Some(&description),
    ) else {
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
