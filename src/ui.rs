use crate::core::{
    LOG_PATH, ModelDownloadCallback, ModelDownloadProgress, ModelManager, ProgressCallback,
    RecorderState, StatusCallback, copy_and_paste_text, copy_text_to_clipboard, ensure_model_cached,
    has_accessibility_permission, is_launch_at_login_enabled,
    request_accessibility_permission_if_needed, set_launch_at_login, transcribe_wav_file,
};
use crate::bubble::{Bubble, BubbleState};
use crate::hotkey::{FnTap, HotkeyMonitor, install_fn_tap_monitor};
use crate::icons::{draw_checkmark_icon, draw_progress_icon, load_microphone_icon};
use crate::languages::LANGUAGES;
use crate::settings::Settings;
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
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
#[cfg(target_os = "macos")]
use tray_icon::menu::ContextMenu;

/// Pastel palette offered under "Barva ikony" for the idle microphone icon.
/// Tuple is (id stored in settings, Czech label, sRGB red, green, blue).
pub const ICON_COLORS: &[(&str, &str, f64, f64, f64)] = &[
    ("coral", "Korálová", 1.0, 0.604, 0.635),
    ("peach", "Broskvová", 1.0, 0.718, 0.698),
    ("yellow", "Žlutá", 0.992, 0.894, 0.651),
    ("mint", "Mátová", 0.710, 0.918, 0.843),
    ("sky", "Nebeská", 0.635, 0.824, 1.0),
    ("lavender", "Levandulová", 0.780, 0.698, 0.871),
    ("pink", "Růžová", 1.0, 0.718, 0.871),
];
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
    ModelDownload(ModelDownloadProgress),
    ModelDownloadDone,
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
    /// Kept so we can reach the native NSMenu (e.g. to style the Status item).
    menu: Option<Menu>,
    copy_last_transcript_item: Option<MenuItem>,
    launch_at_login_item: Option<CheckMenuItem>,
    status_item: Option<MenuItem>,
    /// Idle-icon color picker items, paired with the color id they select
    /// (empty id = default monochrome).
    icon_color_items: Vec<(String, CheckMenuItem)>,
    /// Language picker items, paired with the whisper language code they select.
    language_items: Vec<(String, CheckMenuItem)>,
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
    /// True while the startup model download is streaming progress into the
    /// bubble, so we know to take the bubble down once it finishes.
    downloading_model: bool,
    /// Persisted user preferences (idle icon color, transcription language).
    settings: Settings,
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
            menu: None,
            icon_color_items: Vec::new(),
            language_items: Vec::new(),
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
            downloading_model: false,
            settings: Settings::load(),
        }
    }

    fn build_tray_icon(&mut self) -> UiResult<()> {
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
        // Enabled so clicking it opens the log; it's re-styled to look faded via
        // a gray attributed title (see `apply_status_item_title`).
        let status_item = MenuItem::new(status_menu_text(&self.status), true, None);
        let quit_item = MenuItem::new("Quit", true, None);

        let (icon_color_menu, icon_color_items) = self.build_icon_color_menu()?;
        let (language_menu, language_items) = self.build_language_menu()?;

        menu.append(&status_item)?;
        menu.append(&copy_last_transcript_item)?;
        menu.append(&language_menu)?;
        menu.append(&icon_color_menu)?;
        menu.append(&launch_at_login_item)?;
        menu.append(&quit_item)?;

        let tray_icon = TrayIconBuilder::new()
            .with_tooltip(self.tooltip())
            .with_icon(icon_for_state(TrayVisualState::Idle))
            .with_menu(Box::new(menu.clone()))
            .with_icon_as_template(true)
            .with_menu_on_left_click(false)
            .with_menu_on_right_click(true)
            .build()?;

        self.tray_icon = Some(tray_icon);
        self.menu = Some(menu);
        self.copy_last_transcript_item = Some(copy_last_transcript_item);
        self.launch_at_login_item = Some(launch_at_login_item);
        self.status_item = Some(status_item);
        self.icon_color_items = icon_color_items;
        self.language_items = language_items;
        self.quit_item = Some(quit_item);
        Ok(())
    }

    /// Build the "Barva ikony" submenu: a default (monochrome) entry plus the
    /// pastel palette, with the current selection checked.
    fn build_icon_color_menu(&self) -> UiResult<(Submenu, Vec<(String, CheckMenuItem)>)> {
        let submenu = Submenu::new("Barva ikony", true);
        let mut items: Vec<(String, CheckMenuItem)> = Vec::new();

        let default_item = CheckMenuItem::new(
            "Výchozí (automatická)",
            true,
            self.settings.icon_color.is_empty(),
            None,
        );
        submenu.append(&default_item)?;
        items.push((String::new(), default_item));

        for (id, label, ..) in ICON_COLORS {
            let item = CheckMenuItem::new(*label, true, self.settings.icon_color == *id, None);
            submenu.append(&item)?;
            items.push(((*id).to_string(), item));
        }

        Ok((submenu, items))
    }

    /// Apply the picked idle-icon color: persist it, sync the check marks, and
    /// repaint the tray.
    fn select_icon_color(&mut self, color_id: String) {
        for (id, item) in &self.icon_color_items {
            item.set_checked(*id == color_id);
        }
        self.settings.icon_color = color_id;
        self.settings.save();
        let state = self.current_visual_state();
        self.refresh_tray(state);
    }

    /// sRGB components of the selected idle-icon color, or `None` for the
    /// default monochrome template.
    fn idle_color(&self) -> Option<(f64, f64, f64)> {
        ICON_COLORS
            .iter()
            .find(|(id, ..)| *id == self.settings.icon_color)
            .map(|(_, _, r, g, b)| (*r, *g, *b))
    }

    /// Build the "Jazyk přepisu" submenu: Czech and English pinned on top, a
    /// separator, then every other whisper language sorted by name.
    fn build_language_menu(&self) -> UiResult<(Submenu, Vec<(String, CheckMenuItem)>)> {
        let submenu = Submenu::new("Jazyk přepisu", true);
        let mut items: Vec<(String, CheckMenuItem)> = Vec::new();

        let pinned = ["cs", "en"];
        for code in pinned {
            let label = crate::languages::label_for(code);
            let item = CheckMenuItem::new(label, true, self.settings.language == code, None);
            submenu.append(&item)?;
            items.push((code.to_string(), item));
        }

        submenu.append(&PredefinedMenuItem::separator())?;

        let mut rest: Vec<(&str, &str)> = LANGUAGES
            .iter()
            .copied()
            .filter(|(code, _)| !pinned.contains(code))
            .collect();
        rest.sort_by(|a, b| a.1.cmp(b.1));
        for (code, label) in rest {
            let item = CheckMenuItem::new(label, true, self.settings.language == code, None);
            submenu.append(&item)?;
            items.push((code.to_string(), item));
        }

        Ok((submenu, items))
    }

    /// Apply the picked transcription language: persist it and sync check marks.
    fn select_language(&mut self, code: String) {
        for (c, item) in &self.language_items {
            item.set_checked(*c == code);
        }
        let label = crate::languages::label_for(&code).to_string();
        self.settings.language = code;
        self.settings.save();
        self.set_status(format!("Jazyk přepisu: {label}"));
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
        self.apply_status_item_title(&status_menu_text(&self.status));
        if let Some(copy_last_transcript_item) = self.copy_last_transcript_item.as_ref() {
            copy_last_transcript_item
                .set_text(copy_last_transcript_menu_text(&self.last_transcript));
            copy_last_transcript_item.set_enabled(!self.last_transcript.trim().is_empty());
        }
        if apply_macos_symbol(tray_icon, state, self.idle_color()) {
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
        } else if self.downloading_model {
            // "Skrýt dialog": only dismiss the popup; the download keeps running
            // in the background.
            self.hide_bubble();
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

    /// Set the Status item's title. On macOS we draw it with a gray attributed
    /// title so it keeps the "faded" look of a disabled item while still being
    /// clickable (clicking it opens the log).
    #[cfg(target_os = "macos")]
    #[allow(deprecated)]
    fn apply_status_item_title(&self, text: &str) {
        use cocoa::base::{id, nil};
        use cocoa::foundation::NSString;
        use objc::{class, msg_send, sel, sel_impl};

        let fallback = || {
            if let Some(item) = self.status_item.as_ref() {
                item.set_text(text);
            }
        };

        let Some(menu) = self.menu.as_ref() else {
            return fallback();
        };
        let ns_menu = ContextMenu::ns_menu(menu) as id;
        if ns_menu.is_null() {
            return fallback();
        }
        unsafe {
            // The Status item is appended first, so it lives at index 0.
            let item: id = msg_send![ns_menu, itemAtIndex: 0isize];
            if item.is_null() {
                return fallback();
            }
            let s = NSString::alloc(nil).init_str(text);
            let color: id = msg_send![class!(NSColor), disabledControlTextColor];
            // NSForegroundColorAttributeName's legacy underlying value is "NSColor".
            let key = NSString::alloc(nil).init_str("NSColor");
            let attrs: id =
                msg_send![class!(NSDictionary), dictionaryWithObject: color forKey: key];
            let attr: id = msg_send![class!(NSAttributedString), alloc];
            let attr: id = msg_send![attr, initWithString: s attributes: attrs];
            let _: () = msg_send![item, setAttributedTitle: attr];
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn apply_status_item_title(&self, text: &str) {
        if let Some(item) = self.status_item.as_ref() {
            item.set_text(text);
        }
    }

    fn open_log(&mut self) {
        if let Err(err) = std::process::Command::new("open").arg(LOG_PATH).spawn() {
            self.set_status(format!("Nepodařilo se otevřít log: {err}"));
        }
    }

    fn toggle_submit_after_transcription(&mut self) {
        let enabled = !self.submit_after_transcription.load(Ordering::SeqCst);
        self.submit_after_transcription
            .store(enabled, Ordering::SeqCst);
        self.update_bubble(BubbleState::Transcribing { submit: enabled });
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
                    self.update_bubble(BubbleState::Transcribing { submit: false });
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
                    let language = self.settings.language.clone();
                    thread::spawn(move || {
                        let transcription_started = Instant::now();
                        let event = match transcribe_wav_file(
                            &model_manager,
                            &file_path,
                            &language,
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
            Ok(()) => {
                self.refresh_tray(TrayVisualState::Idle);

                let cancel_proxy = self.proxy.clone();
                self.bubble = Some(Bubble::new(Box::new(move || {
                    let _ = cancel_proxy.send_event(UserEvent::Cancel);
                })));

                let status_callback = status_callback(self.proxy.clone());
                let download_callback = download_progress_callback(self.proxy.clone());
                let done_proxy = self.proxy.clone();
                thread::spawn(move || {
                    match ensure_model_cached(Some(&status_callback), Some(&download_callback)) {
                        Ok(()) => {
                            let _ = done_proxy
                                .send_event(UserEvent::WorkerEvent(WorkerEvent::ModelDownloadDone));
                        }
                        Err(err) => {
                            let _ = done_proxy
                                .send_event(UserEvent::WorkerEvent(WorkerEvent::ModelDownloadDone));
                            status_callback(format!("Model download failed: {err}"));
                        }
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
                    .status_item
                    .as_ref()
                    .is_some_and(|item| event.id == *item.id())
                {
                    self.open_log();
                    return;
                }
                if let Some((color_id, _)) = self
                    .icon_color_items
                    .iter()
                    .find(|(_, item)| event.id == *item.id())
                {
                    let color_id = color_id.clone();
                    self.select_icon_color(color_id);
                    return;
                }
                if let Some((code, _)) = self
                    .language_items
                    .iter()
                    .find(|(_, item)| event.id == *item.id())
                {
                    let code = code.clone();
                    self.select_language(code);
                    return;
                }
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
                WorkerEvent::ModelDownload(progress) => {
                    // A recording/transcription in flight owns the bubble; don't
                    // steal it for the background download.
                    if !self.recorder.is_recording() && !self.is_transcribing {
                        let state = BubbleState::DownloadingModel {
                            fraction: progress.fraction,
                            detail: download_detail_text(&progress),
                        };
                        if self.downloading_model {
                            self.update_bubble(state);
                        } else {
                            self.show_bubble(state);
                        }
                    }
                    self.downloading_model = true;
                }
                WorkerEvent::ModelDownloadDone => {
                    if self.downloading_model {
                        self.downloading_model = false;
                        if !self.recorder.is_recording() && !self.is_transcribing {
                            self.hide_bubble();
                        }
                    }
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

fn download_progress_callback(proxy: EventLoopProxy<UserEvent>) -> ModelDownloadCallback {
    Arc::new(move |progress| {
        let _ = proxy.send_event(UserEvent::WorkerEvent(WorkerEvent::ModelDownload(progress)));
    })
}

/// Second-line text for the download bubble, e.g. "45 % · zbývá 1:30" or, when
/// the total size is unknown, "120 MB staženo".
fn download_detail_text(progress: &ModelDownloadProgress) -> String {
    match progress.fraction {
        Some(fraction) => {
            let percent = (fraction * 100.0).round() as u32;
            match &progress.eta {
                Some(eta) => format!("{percent} % · zbývá {eta}"),
                None => format!("{percent} %"),
            }
        }
        None => format!(
            "{:.0} MB staženo",
            progress.downloaded_bytes as f64 / 1_048_576.0
        ),
    }
}

fn icon_for_state(state: TrayVisualState) -> Icon {
    match state {
        TrayVisualState::Idle => load_microphone_icon(),
        TrayVisualState::Recording => draw_checkmark_icon(),
        TrayVisualState::Transcribing { progress, submit } => draw_progress_icon(progress, submit),
    }
}

#[cfg(target_os = "macos")]
fn apply_macos_symbol(
    tray_icon: &TrayIcon,
    state: TrayVisualState,
    idle_color: Option<(f64, f64, f64)>,
) -> bool {
    use objc2_app_kit::{NSColor, NSImageSymbolConfiguration};

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

    // Tint the idle microphone with the chosen pastel color; everything else
    // stays a monochrome menu-bar template that adapts to light/dark.
    if let (TrayVisualState::Idle, Some((r, g, b))) = (state, idle_color) {
        let color = NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0);
        let config = NSImageSymbolConfiguration::configurationWithHierarchicalColor(&color);
        if let Some(colored) = image.imageWithSymbolConfiguration(&config) {
            colored.setTemplate(false);
            button.setImage(Some(&colored));
            return true;
        }
    }

    image.setTemplate(true);
    button.setImage(Some(&image));
    true
}

#[cfg(not(target_os = "macos"))]
fn apply_macos_symbol(
    _tray_icon: &TrayIcon,
    _state: TrayVisualState,
    _idle_color: Option<(f64, f64, f64)>,
) -> bool {
    false
}
