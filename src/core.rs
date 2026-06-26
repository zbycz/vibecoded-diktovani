use crate::whisper::WhisperModel;
#[cfg(target_os = "macos")]
use accessibility_sys::{
    AXIsProcessTrusted, AXIsProcessTrustedWithOptions, kAXTrustedCheckOptionPrompt,
};
use arboard::Clipboard;
#[cfg(target_os = "macos")]
use core_foundation::base::TCFType;
#[cfg(target_os = "macos")]
use core_foundation::boolean::CFBoolean;
#[cfg(target_os = "macos")]
use core_foundation::dictionary::CFDictionary;
#[cfg(target_os = "macos")]
use core_foundation::string::CFString;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, Stream};
use enigo::{Direction, Enigo, Key, Keyboard, Settings};
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};
use thiserror::Error;

fn log_timestamp_origin() -> &'static Mutex<Instant> {
    static START: OnceLock<Mutex<Instant>> = OnceLock::new();
    START.get_or_init(|| Mutex::new(Instant::now()))
}

fn set_ts_origin(origin: Instant) {
    let mut start = log_timestamp_origin()
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    *start = origin;
}

fn ts() -> f32 {
    let start = log_timestamp_origin()
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    start.elapsed().as_secs_f32()
}

pub const MODEL_URL: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo.bin";
pub const MODEL_FILENAME: &str = "ggml-large-v3-turbo.bin";
pub const APP_IDENTIFIER: &str = "com.example.diktovani";
pub const LANGUAGE: &str = "cs";
/// Where stdout/stderr are mirrored (see `main::redirect_output_to_log`); the
/// Status menu item opens this file.
pub const LOG_PATH: &str = "/tmp/diktovani.log";

#[derive(Debug, Error)]
pub enum AppError {
    #[error("{0}")]
    Message(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("WAV error: {0}")]
    Wav(#[from] hound::Error),
}

pub type Result<T> = std::result::Result<T, AppError>;
pub type StatusCallback = Arc<dyn Fn(String) + Send + Sync + 'static>;
pub type ProgressCallback = Arc<dyn Fn(u8) + Send + Sync + 'static>;
/// Structured model-download progress, used to drive the popup bubble's progress
/// bar and ETA line. Emitted in addition to the human-readable status string.
pub type ModelDownloadCallback = Arc<dyn Fn(ModelDownloadProgress) + Send + Sync + 'static>;

#[derive(Clone, Debug)]
pub struct ModelDownloadProgress {
    /// Completed fraction in 0.0..=1.0 when the total size is known.
    pub fraction: Option<f64>,
    pub downloaded_bytes: u64,
    /// Human ETA like "1:30" or "12s", or `None` when it can't be estimated yet.
    pub eta: Option<String>,
}

#[derive(Debug)]
pub struct AudioRecording {
    pub file_path: PathBuf,
    pub sample_rate: u32,
    pub channels: u16,
    pub duration_seconds: f32,
}

#[derive(Debug)]
enum RecorderCmd {
    Start(mpsc::Sender<Result<()>>),
    Stop(mpsc::Sender<Result<()>>),
    Shutdown,
}

struct ProgressiveWavWriter {
    writer: BufWriter<File>,
    sample_rate: u32,
    channels: u16,
    bytes_per_sample: u16,
    data_chunk_size_pos: u64,
    riff_chunk_size_pos: u64,
    samples_written: u64,
    last_header_update: Instant,
}

impl ProgressiveWavWriter {
    fn new(file_path: &PathBuf, sample_rate: u32, channels: u16) -> Result<Self> {
        let file = File::create(file_path)?;
        let mut writer = BufWriter::new(file);
        let bits_per_sample = 32u16;
        let bytes_per_sample = bits_per_sample / 8;

        writer.write_all(b"RIFF")?;
        let riff_chunk_size_pos = writer.stream_position()?;
        writer.write_all(&0u32.to_le_bytes())?;
        writer.write_all(b"WAVE")?;

        writer.write_all(b"fmt ")?;
        writer.write_all(&16u32.to_le_bytes())?;
        writer.write_all(&3u16.to_le_bytes())?;
        writer.write_all(&channels.to_le_bytes())?;
        writer.write_all(&sample_rate.to_le_bytes())?;
        writer
            .write_all(&(sample_rate * channels as u32 * bytes_per_sample as u32).to_le_bytes())?;
        writer.write_all(&(channels * bytes_per_sample).to_le_bytes())?;
        writer.write_all(&bits_per_sample.to_le_bytes())?;

        writer.write_all(b"data")?;
        let data_chunk_size_pos = writer.stream_position()?;
        writer.write_all(&0u32.to_le_bytes())?;
        writer.flush()?;

        Ok(Self {
            writer,
            sample_rate,
            channels,
            bytes_per_sample,
            data_chunk_size_pos,
            riff_chunk_size_pos,
            samples_written: 0,
            last_header_update: Instant::now(),
        })
    }

    fn write_samples_f32(&mut self, samples: &[f32]) -> Result<()> {
        for sample in samples {
            self.writer.write_all(&sample.to_le_bytes())?;
        }
        self.samples_written += samples.len() as u64;
        if self.last_header_update.elapsed().as_secs() >= 1 {
            self.update_headers()?;
            self.last_header_update = Instant::now();
        }
        Ok(())
    }

    fn write_samples_i16(&mut self, samples: &[i16]) -> Result<()> {
        for sample in samples {
            let value = *sample as f32 / i16::MAX as f32;
            self.writer.write_all(&value.to_le_bytes())?;
        }
        self.samples_written += samples.len() as u64;
        if self.last_header_update.elapsed().as_secs() >= 1 {
            self.update_headers()?;
            self.last_header_update = Instant::now();
        }
        Ok(())
    }

    fn write_samples_u16(&mut self, samples: &[u16]) -> Result<()> {
        for sample in samples {
            let value = (*sample as f32 / u16::MAX as f32) * 2.0 - 1.0;
            self.writer.write_all(&value.to_le_bytes())?;
        }
        self.samples_written += samples.len() as u64;
        if self.last_header_update.elapsed().as_secs() >= 1 {
            self.update_headers()?;
            self.last_header_update = Instant::now();
        }
        Ok(())
    }

    fn finalize(&mut self) -> Result<()> {
        self.update_headers()?;
        self.writer.flush()?;
        Ok(())
    }

    fn duration_seconds(&self) -> f32 {
        self.samples_written as f32 / (self.sample_rate as f32 * self.channels as f32)
    }

    fn update_headers(&mut self) -> Result<()> {
        let current_pos = self.writer.stream_position()?;
        let data_size = self.samples_written * self.bytes_per_sample as u64;
        let file_size = 36 + data_size;

        self.writer
            .seek(SeekFrom::Start(self.riff_chunk_size_pos))?;
        self.writer.write_all(&(file_size as u32).to_le_bytes())?;
        self.writer
            .seek(SeekFrom::Start(self.data_chunk_size_pos))?;
        self.writer.write_all(&(data_size as u32).to_le_bytes())?;
        self.writer.seek(SeekFrom::Start(current_pos))?;
        self.writer.flush()?;
        Ok(())
    }
}

pub struct RecorderState {
    cmd_tx: Option<mpsc::Sender<RecorderCmd>>,
    worker_handle: Option<JoinHandle<()>>,
    writer: Option<Arc<Mutex<ProgressiveWavWriter>>>,
    is_recording: Arc<AtomicBool>,
    sample_rate: u32,
    channels: u16,
    file_path: Option<PathBuf>,
    recording_started_at: Option<Instant>,
}

impl RecorderState {
    pub fn new() -> Self {
        Self {
            cmd_tx: None,
            worker_handle: None,
            writer: None,
            is_recording: Arc::new(AtomicBool::new(false)),
            sample_rate: 0,
            channels: 0,
            file_path: None,
            recording_started_at: None,
        }
    }

    pub fn start_new_recording(&mut self) -> Result<()> {
        self.close_session()?;

        let file_path = std::env::temp_dir().join(format!(
            "whispering-mvp-{}.wav",
            std::time::UNIX_EPOCH
                .elapsed()
                .map_err(|e| AppError::Message(format!("Failed to create temp filename: {e}")))?
                .as_millis()
        ));

        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| AppError::Message("No default input device available.".into()))?;

        let config = get_optimal_config(&device, Some(16000))?;
        let sample_format = config.sample_format();
        let sample_rate = config.sample_rate().0;
        let channels = config.channels();

        let writer = Arc::new(Mutex::new(ProgressiveWavWriter::new(
            &file_path,
            sample_rate,
            channels,
        )?));
        let stream_config = cpal::StreamConfig {
            channels,
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        self.is_recording = Arc::new(AtomicBool::new(false));
        let is_recording = self.is_recording.clone();
        let writer_clone = writer.clone();
        let (cmd_tx, cmd_rx) = mpsc::channel();

        let worker = thread::spawn(move || {
            let stream = match build_input_stream(
                &device,
                &stream_config,
                sample_format,
                is_recording.clone(),
                writer_clone,
            ) {
                Ok(stream) => stream,
                Err(err) => {
                    eprintln!("[{:.1}s] Failed to build input stream: {err}", ts());
                    return;
                }
            };

            if let Err(err) = stream.play() {
                eprintln!("[{:.1}s] Failed to start stream: {err}", ts());
                return;
            }

            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    RecorderCmd::Start(reply_tx) => {
                        is_recording.store(true, Ordering::Relaxed);
                        let _ = reply_tx.send(Ok(()));
                    }
                    RecorderCmd::Stop(reply_tx) => {
                        is_recording.store(false, Ordering::Relaxed);
                        let _ = reply_tx.send(Ok(()));
                    }
                    RecorderCmd::Shutdown => break,
                }
            }
        });

        self.cmd_tx = Some(cmd_tx);
        self.worker_handle = Some(worker);
        self.writer = Some(writer);
        self.sample_rate = sample_rate;
        self.channels = channels;
        self.file_path = Some(file_path);

        let tx = self
            .cmd_tx
            .as_ref()
            .ok_or_else(|| AppError::Message("Recording session failed to initialize.".into()))?;
        let (reply_tx, reply_rx) = mpsc::channel();
        tx.send(RecorderCmd::Start(reply_tx))
            .map_err(|e| AppError::Message(format!("Failed to send start command: {e}")))?;
        reply_rx.recv().map_err(|e| {
            AppError::Message(format!("Failed to receive start confirmation: {e}"))
        })??;

        let recording_started_at = Instant::now();
        self.recording_started_at = Some(recording_started_at);
        set_ts_origin(recording_started_at);
        println!("[{:.1}s] [record] started recording", ts());

        Ok(())
    }

    pub fn stop_recording(&mut self) -> Result<AudioRecording> {
        let tx = self
            .cmd_tx
            .as_ref()
            .ok_or_else(|| AppError::Message("No active recording session.".into()))?;
        let (reply_tx, reply_rx) = mpsc::channel();
        tx.send(RecorderCmd::Stop(reply_tx))
            .map_err(|e| AppError::Message(format!("Failed to send stop command: {e}")))?;
        reply_rx.recv().map_err(|e| {
            AppError::Message(format!("Failed to receive stop confirmation: {e}"))
        })??;

        let writer = self
            .writer
            .as_ref()
            .ok_or_else(|| AppError::Message("Missing WAV writer.".into()))?;
        let mut writer = writer
            .lock()
            .map_err(|e| AppError::Message(format!("Failed to lock WAV writer: {e}")))?;
        writer.finalize()?;
        let duration_seconds = writer.duration_seconds();
        drop(writer);

        let recording = AudioRecording {
            file_path: self
                .file_path
                .clone()
                .ok_or_else(|| AppError::Message("Missing recording file path.".into()))?,
            sample_rate: self.sample_rate,
            channels: self.channels,
            duration_seconds,
        };

        self.close_session()?;

        let rec_duration = self
            .recording_started_at
            .take()
            .map(|t| t.elapsed().as_secs_f32());
        if let Some(secs) = rec_duration {
            println!(
                "[{:.1}s] [record] stopped recording after {:.2}s",
                ts(),
                secs
            );
        }

        Ok(recording)
    }

    pub fn close_session(&mut self) -> Result<()> {
        if let Some(tx) = self.cmd_tx.take() {
            let _ = tx.send(RecorderCmd::Shutdown);
        }
        if let Some(handle) = self.worker_handle.take() {
            let _ = handle.join();
        }
        self.writer = None;
        self.sample_rate = 0;
        self.channels = 0;
        self.file_path = None;
        self.is_recording.store(false, Ordering::Relaxed);
        Ok(())
    }

    pub fn is_recording(&self) -> bool {
        self.is_recording.load(Ordering::Acquire)
    }
}

impl Drop for RecorderState {
    fn drop(&mut self) {
        let _ = self.close_session();
    }
}

fn get_optimal_config(
    device: &Device,
    preferred_sample_rate: Option<u32>,
) -> Result<cpal::SupportedStreamConfig> {
    let target_sample_rate = preferred_sample_rate.unwrap_or(16000);
    let configs: Vec<_> = device
        .supported_input_configs()
        .map_err(|e| AppError::Message(format!("Failed to enumerate input configs: {e}")))?
        .collect();
    if configs.is_empty() {
        return Err(AppError::Message(
            "No supported input configurations were found.".into(),
        ));
    }

    let supported_formats = [SampleFormat::F32, SampleFormat::I16, SampleFormat::U16];
    let compatible_configs: Vec<_> = configs
        .iter()
        .filter(|config| supported_formats.contains(&config.sample_format()))
        .collect();
    if compatible_configs.is_empty() {
        return Err(AppError::Message(
            "No compatible microphone formats were found.".into(),
        ));
    }

    for config in &compatible_configs {
        if config.channels() == 1 {
            let min_rate = config.min_sample_rate().0;
            let max_rate = config.max_sample_rate().0;
            if min_rate <= target_sample_rate && max_rate >= target_sample_rate {
                return Ok(config.with_sample_rate(cpal::SampleRate(target_sample_rate)));
            }
        }
    }

    for config in &compatible_configs {
        let min_rate = config.min_sample_rate().0;
        let max_rate = config.max_sample_rate().0;
        if min_rate <= target_sample_rate && max_rate >= target_sample_rate {
            return Ok(config.with_sample_rate(cpal::SampleRate(target_sample_rate)));
        }
    }

    let mut best_config = None;
    let mut best_diff = u32::MAX;
    for config in &compatible_configs {
        if config.channels() == 1 {
            let min_rate = config.min_sample_rate().0;
            let max_rate = config.max_sample_rate().0;
            let closest_rate = if target_sample_rate < min_rate {
                min_rate
            } else if target_sample_rate > max_rate {
                max_rate
            } else {
                target_sample_rate
            };
            let diff = (closest_rate as i32 - target_sample_rate as i32).unsigned_abs();
            if diff < best_diff {
                best_diff = diff;
                best_config = Some(config.with_sample_rate(cpal::SampleRate(closest_rate)));
            }
        }
    }

    best_config.ok_or_else(|| AppError::Message("Failed to choose a microphone config.".into()))
}

fn build_input_stream(
    device: &Device,
    config: &cpal::StreamConfig,
    sample_format: SampleFormat,
    is_recording: Arc<AtomicBool>,
    writer: Arc<Mutex<ProgressiveWavWriter>>,
) -> Result<Stream> {
    let err_fn = |err| eprintln!("[{:.1}s] Audio stream error: {err}", ts());

    let stream = match sample_format {
        SampleFormat::F32 => {
            let is_recording = is_recording.clone();
            let writer = writer.clone();
            device.build_input_stream(
                config,
                move |data: &[f32], _| {
                    if is_recording.load(Ordering::Relaxed)
                        && let Ok(mut writer) = writer.lock()
                    {
                        let _ = writer.write_samples_f32(data);
                    }
                },
                err_fn,
                None,
            )
        }
        SampleFormat::I16 => {
            let is_recording = is_recording.clone();
            let writer = writer.clone();
            device.build_input_stream(
                config,
                move |data: &[i16], _| {
                    if is_recording.load(Ordering::Relaxed)
                        && let Ok(mut writer) = writer.lock()
                    {
                        let _ = writer.write_samples_i16(data);
                    }
                },
                err_fn,
                None,
            )
        }
        SampleFormat::U16 => {
            let is_recording = is_recording.clone();
            let writer = writer.clone();
            device.build_input_stream(
                config,
                move |data: &[u16], _| {
                    if is_recording.load(Ordering::Relaxed)
                        && let Ok(mut writer) = writer.lock()
                    {
                        let _ = writer.write_samples_u16(data);
                    }
                },
                err_fn,
                None,
            )
        }
        _ => {
            return Err(AppError::Message(format!(
                "Unsupported sample format: {sample_format:?}"
            )));
        }
    }
    .map_err(|e| AppError::Message(format!("Failed to build input stream: {e}")))?;

    Ok(stream)
}

enum Engine {
    Whisper(WhisperModel),
}

impl Engine {
    fn unload(&mut self) {
        // WhisperModel drops its resources when the Engine is dropped.
    }
}

#[derive(Clone)]
pub struct ModelManager {
    engine: Arc<Mutex<Option<Engine>>>,
    current_model_path: Arc<Mutex<Option<PathBuf>>>,
    last_activity: Arc<Mutex<SystemTime>>,
    idle_timeout: Duration,
}

impl ModelManager {
    pub fn new() -> Self {
        Self {
            engine: Arc::new(Mutex::new(None)),
            current_model_path: Arc::new(Mutex::new(None)),
            last_activity: Arc::new(Mutex::new(SystemTime::now())),
            idle_timeout: Duration::from_secs(5 * 60),
        }
    }

    fn get_or_load_whisper(
        &self,
        model_path: PathBuf,
    ) -> Result<(Arc<Mutex<Option<Engine>>>, bool)> {
        let mut engine_guard = self
            .engine
            .lock()
            .map_err(|e| AppError::Message(format!("Engine mutex poisoned: {e}")))?;
        let mut current_path_guard = self
            .current_model_path
            .lock()
            .map_err(|e| AppError::Message(format!("Model path mutex poisoned: {e}")))?;

        let needs_load = match (&*engine_guard, &*current_path_guard) {
            (None, _) => true,
            (Some(_), Some(path)) if path != &model_path => {
                if let Some(mut engine) = engine_guard.take() {
                    engine.unload();
                }
                true
            }
            _ => false,
        };

        if needs_load {
            let engine = WhisperModel::load(&model_path)
                .map_err(|e| AppError::Message(format!("Failed to load Whisper model: {e}")))?;
            *engine_guard = Some(Engine::Whisper(engine));
            *current_path_guard = Some(model_path);
        }

        let mut last_activity_guard = self
            .last_activity
            .lock()
            .map_err(|e| AppError::Message(format!("Last activity mutex poisoned: {e}")))?;
        *last_activity_guard = SystemTime::now();

        Ok((self.engine.clone(), needs_load))
    }

    pub fn preload_whisper(&self, status_callback: Option<&StatusCallback>) -> Result<()> {
        let started_at = Instant::now();
        let model_path = ensure_model_available(status_callback, None)?;
        println!(
            "[{:.1}s] [preload] starting Whisper preload from {}",
            ts(),
            model_path.display()
        );

        let (_, loaded_now) = self.get_or_load_whisper(model_path)?;
        emit_status(status_callback, "Model ready.");
        let elapsed = started_at.elapsed();

        if loaded_now {
            println!(
                "[{:.1}s] [preload] Whisper model loaded in {:.2}s",
                ts(),
                elapsed.as_secs_f32()
            );
        } else {
            println!(
                "[{:.1}s] [preload] Whisper model already warm (checked in {:.2}s)",
                ts(),
                elapsed.as_secs_f32()
            );
        }

        Ok(())
    }

    pub fn unload_if_idle(&self) {
        let Ok(last_activity) = self.last_activity.lock() else {
            return;
        };
        let elapsed = SystemTime::now()
            .duration_since(*last_activity)
            .unwrap_or(Duration::ZERO);
        drop(last_activity);

        if elapsed <= self.idle_timeout {
            return;
        }

        if let Ok(mut engine_guard) = self.engine.lock() {
            if let Some(mut engine) = engine_guard.take() {
                engine.unload();
            }
        }
        if let Ok(mut current_model_path) = self.current_model_path.lock() {
            *current_model_path = None;
        }
    }
}

pub fn has_accessibility_permission() -> bool {
    #[cfg(target_os = "macos")]
    unsafe {
        AXIsProcessTrusted()
    }

    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

pub fn request_accessibility_permission_if_needed() -> bool {
    if has_accessibility_permission() {
        return true;
    }

    prompt_accessibility_permission();
    false
}

#[cfg(target_os = "macos")]
fn prompt_accessibility_permission() {
    let key = unsafe { CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt) };
    let value = CFBoolean::true_value();
    let options = CFDictionary::from_CFType_pairs(&[(key.as_CFType(), value.as_CFType())]);

    unsafe {
        let _ = AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef());
    }

    if let Err(err) = Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
        .status()
    {
        eprintln!(
            "[{:.1}s] [accessibility] failed to open System Settings: {err}",
            ts()
        );
    }
}

#[cfg(not(target_os = "macos"))]
fn prompt_accessibility_permission() {}

pub fn copy_and_paste_text(text: &str, submit_with_enter: bool) -> Result<()> {
    if text.trim().is_empty() {
        return Ok(());
    }

    if !has_accessibility_permission() {
        prompt_accessibility_permission();
        return Err(AppError::Message(
            "Accessibility permission is required for auto-paste. Enable Diktovani in System Settings > Privacy & Security > Accessibility.".into(),
        ));
    }

    let mut clipboard = Clipboard::new()
        .map_err(|e| AppError::Message(format!("Failed to access clipboard: {e}")))?;
    let original_clipboard = clipboard.get_text().ok();

    clipboard
        .set_text(text.to_string())
        .map_err(|e| AppError::Message(format!("Failed to write transcript to clipboard: {e}")))?;

    thread::sleep(Duration::from_millis(50));

    let mut enigo = Enigo::new(&Settings::default())
        .map_err(|e| AppError::Message(format!("Failed to initialize input automation: {e}")))?;

    #[cfg(target_os = "macos")]
    let (modifier, v_key) = (Key::Meta, Key::Other(9));
    #[cfg(target_os = "windows")]
    let (modifier, v_key) = (Key::Control, Key::Other(0x56));
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let (modifier, v_key) = (Key::Control, Key::Unicode('v'));

    enigo
        .key(modifier, Direction::Press)
        .map_err(|e| AppError::Message(format!("Failed to press modifier key: {e}")))?;
    enigo
        .key(v_key, Direction::Press)
        .map_err(|e| AppError::Message(format!("Failed to press paste key: {e}")))?;
    enigo
        .key(v_key, Direction::Release)
        .map_err(|e| AppError::Message(format!("Failed to release paste key: {e}")))?;
    enigo
        .key(modifier, Direction::Release)
        .map_err(|e| AppError::Message(format!("Failed to release modifier key: {e}")))?;

    if submit_with_enter {
        // The paste (Cmd+V) is asynchronous: the target app inserts the text on
        // its own schedule. Slower/web/Electron apps can take >100ms, and if we
        // press Return before the text has landed it submits an empty field (or
        // nothing). Wait long enough that the paste has reliably committed.
        thread::sleep(Duration::from_millis(250));
        enigo
            .key(Key::Return, Direction::Press)
            .map_err(|e| AppError::Message(format!("Failed to press Return key: {e}")))?;
        enigo
            .key(Key::Return, Direction::Release)
            .map_err(|e| AppError::Message(format!("Failed to release Return key: {e}")))?;
    }

    thread::sleep(Duration::from_millis(100));

    if let Some(original_clipboard) = original_clipboard
        && let Err(err) = clipboard.set_text(original_clipboard)
    {
        eprintln!(
            "[{:.1}s] [clipboard] failed to restore original clipboard text: {err}",
            ts()
        );
    }

    Ok(())
}

pub fn copy_text_to_clipboard(text: &str) -> Result<()> {
    if text.trim().is_empty() {
        return Ok(());
    }

    let mut clipboard = Clipboard::new()
        .map_err(|e| AppError::Message(format!("Failed to access clipboard: {e}")))?;
    clipboard
        .set_text(text.to_string())
        .map_err(|e| AppError::Message(format!("Failed to write transcript to clipboard: {e}")))?;
    Ok(())
}

pub fn is_launch_at_login_enabled() -> bool {
    #[cfg(target_os = "macos")]
    {
        launch_agent_plist_path()
            .map(|path| path.exists())
            .unwrap_or(false)
    }

    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

pub fn set_launch_at_login(enabled: bool) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let plist_path = launch_agent_plist_path()?;
        let launch_agents_dir = plist_path.parent().ok_or_else(|| {
            AppError::Message("LaunchAgent path is missing a parent directory.".into())
        })?;

        if enabled {
            std::fs::create_dir_all(launch_agents_dir)?;

            let executable = std::env::current_exe().map_err(|err| {
                AppError::Message(format!("Failed to resolve current executable: {err}"))
            })?;
            let plist_contents = launch_agent_plist_contents(&executable);
            let tmp_path = plist_path.with_extension("plist.tmp");
            std::fs::write(&tmp_path, plist_contents)?;
            std::fs::rename(&tmp_path, &plist_path)?;

            let _ = Command::new("launchctl")
                .args(["unload", "-w"])
                .arg(&plist_path)
                .status();
            let status = Command::new("launchctl")
                .args(["load", "-w"])
                .arg(&plist_path)
                .status()
                .map_err(|err| AppError::Message(format!("Failed to run launchctl load: {err}")))?;
            if !status.success() {
                return Err(AppError::Message(format!(
                    "launchctl load failed with status {status}."
                )));
            }
        } else {
            if plist_path.exists() {
                let _ = Command::new("launchctl")
                    .args(["unload", "-w"])
                    .arg(&plist_path)
                    .status();
                std::fs::remove_file(&plist_path)?;
            }
        }

        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = enabled;
        Err(AppError::Message(
            "Launch at login is only implemented for macOS.".into(),
        ))
    }
}

pub fn ensure_model_cached(
    status_callback: Option<&StatusCallback>,
    download_callback: Option<&ModelDownloadCallback>,
) -> Result<()> {
    let model_path = ensure_model_available(status_callback, download_callback)?;
    println!(
        "[{:.1}s] [model] cache ready {}",
        ts(),
        model_path.display()
    );
    Ok(())
}

pub fn transcribe_wav_file(
    model_manager: &ModelManager,
    file_path: &PathBuf,
    status_callback: Option<&StatusCallback>,
    progress_callback: Option<&ProgressCallback>,
) -> Result<String> {
    let total_started_at = Instant::now();
    println!(
        "[{:.1}s] [transcribe] reading audio from {}",
        ts(),
        file_path.display()
    );
    let audio_data = std::fs::read(file_path)?;

    let sample_extract_started_at = Instant::now();
    let samples = extract_whisper_samples_from_wav(audio_data)?;
    println!(
        "[{:.1}s] [transcribe] prepared {} samples in {:.2}s",
        ts(),
        samples.len(),
        sample_extract_started_at.elapsed().as_secs_f32()
    );
    if samples.is_empty() {
        println!(
            "[{:.1}s] [transcribe] no samples found, finishing in {:.2}s",
            ts(),
            total_started_at.elapsed().as_secs_f32()
        );
        return Ok(String::new());
    }

    let model_ready_started_at = Instant::now();
    let model_path = ensure_model_available(status_callback, None)?;
    let (engine_arc, loaded_now) = model_manager.get_or_load_whisper(model_path.clone())?;
    emit_status(status_callback, "Model ready. Transcribing...");
    println!(
        "[{:.1}s] [transcribe] model {} in {:.2}s ({})",
        ts(),
        if loaded_now {
            "loaded on demand"
        } else {
            "was already warm"
        },
        model_ready_started_at.elapsed().as_secs_f32(),
        model_path.display()
    );

    // whisper's native progress callback only fires once per 30-second chunk –
    // short recordings always give just 0% and 100%.  Instead, run a timer
    // thread that fires every 200 ms with a time-based estimate, and skip the
    // whisper callback entirely.
    let progress_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ticker_handle: Option<thread::JoinHandle<()>> = progress_callback.map(|cb| {
        let cb = cb.clone();
        let done = progress_done.clone();
        // Empirical formula (M1, large-v3-turbo): inference ≈ 2.6s + audio_secs / 12
        let audio_secs = samples.len() as f32 / 16000.0;
        let estimated_secs = 3.0 + audio_secs / 12.0;
        thread::spawn(move || {
            let started = Instant::now();
            loop {
                thread::sleep(Duration::from_millis(200));
                if done.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let elapsed = started.elapsed().as_secs_f32();
                let pct = ((elapsed / estimated_secs) * 100.0).clamp(0.0, 75.0) as u8;
                println!("[{:.1}s] [transcribe] progress ~{}%", ts(), pct);
                cb(pct);
            }
        })
    });

    let engine_guard = engine_arc
        .lock()
        .map_err(|e| AppError::Message(format!("Engine mutex poisoned: {e}")))?;
    let engine = engine_guard
        .as_ref()
        .ok_or_else(|| AppError::Message("Whisper model is not loaded.".into()))?;
    let Engine::Whisper(whisper_model) = engine;

    let inference_started_at = Instant::now();
    let raw = whisper_model
        .transcribe(&samples, Some(LANGUAGE), None)
        .map_err(|e| AppError::Message(format!("Transcription failed: {e}")))?;
    let transcript = strip_trailing_subtitle_credit(raw.trim());

    progress_done.store(true, std::sync::atomic::Ordering::Relaxed);
    if let Some(h) = ticker_handle {
        let _ = h.join();
    }
    if let Some(cb) = progress_callback {
        println!("[{:.1}s] [transcribe] progress 100%", ts());
        cb(100);
    }

    println!(
        "[{:.1}s] [transcribe] inference finished in {:.2}s, transcript chars={}, total {:.2}s",
        ts(),
        inference_started_at.elapsed().as_secs_f32(),
        transcript.len(),
        total_started_at.elapsed().as_secs_f32()
    );

    Ok(transcript)
}

fn ensure_model_available(
    status_callback: Option<&StatusCallback>,
    download_callback: Option<&ModelDownloadCallback>,
) -> Result<PathBuf> {
    let _download_guard = model_download_lock()
        .lock()
        .map_err(|err| AppError::Message(format!("Model download mutex poisoned: {err}")))?;
    let model_path = cache_model_path()?;

    if let Ok(metadata) = std::fs::metadata(&model_path)
        && metadata.len() > 0
    {
        println!(
            "[{:.1}s] [model] using cached model {}",
            ts(),
            model_path.display()
        );
        return Ok(model_path);
    }

    let cache_dir = model_path.parent().ok_or_else(|| {
        AppError::Message("Model cache path is missing a parent directory.".into())
    })?;
    std::fs::create_dir_all(cache_dir)?;

    let partial_path = cache_dir.join(format!("{MODEL_FILENAME}.partial"));
    if partial_path.exists() {
        std::fs::remove_file(&partial_path)?;
    }

    let started_at = Instant::now();
    println!(
        "[{:.1}s] [model] downloading Whisper model from {} to {}",
        ts(),
        MODEL_URL,
        model_path.display()
    );
    emit_status(status_callback, "Downloading model: 0% · ETA --");

    if let Err(err) = download_model_with_progress(&partial_path, status_callback, download_callback)
    {
        let _ = std::fs::remove_file(&partial_path);
        return Err(err);
    }

    std::fs::rename(&partial_path, &model_path)?;
    emit_status(status_callback, "Model download complete.");
    println!(
        "[{:.1}s] [model] download finished in {:.2}s: {}",
        ts(),
        started_at.elapsed().as_secs_f32(),
        model_path.display()
    );

    Ok(model_path)
}

fn cache_model_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| {
        AppError::Message("HOME is not set, cannot resolve ~/.cache/diktovani.".into())
    })?;

    Ok(PathBuf::from(home)
        .join(".cache")
        .join("diktovani")
        .join(MODEL_FILENAME))
}

/// Append one row to `timings.csv` in the model cache directory, recording how
/// long the audio was, how long transcription took, and how many bytes the
/// resulting transcript had. Best-effort: failures are logged, never returned.
pub fn append_transcription_log(
    audio_seconds: f32,
    transcription_seconds: f32,
    transcript_bytes: usize,
) {
    let Ok(model_path) = cache_model_path() else {
        return;
    };
    let Some(dir) = model_path.parent() else {
        return;
    };
    let csv_path = dir.join("timings.csv");

    let write_header = !csv_path.exists();
    let mut file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&csv_path)
    {
        Ok(file) => file,
        Err(err) => {
            eprintln!("[timings] failed to open {}: {err}", csv_path.display());
            return;
        }
    };

    use std::io::Write;
    if write_header
        && let Err(err) = writeln!(file, "audio_seconds,transcription_seconds,transcript_bytes")
    {
        eprintln!("[timings] failed to write header: {err}");
        return;
    }
    if let Err(err) = writeln!(
        file,
        "{audio_seconds:.3},{transcription_seconds:.3},{transcript_bytes}"
    ) {
        eprintln!("[timings] failed to write row: {err}");
    }
}

fn model_download_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn emit_status(status_callback: Option<&StatusCallback>, status: impl Into<String>) {
    if let Some(callback) = status_callback {
        callback(status.into());
    }
}

fn download_model_with_progress(
    destination: &PathBuf,
    status_callback: Option<&StatusCallback>,
    download_callback: Option<&ModelDownloadCallback>,
) -> Result<()> {
    let response = ureq::get(MODEL_URL)
        .call()
        .map_err(|err| AppError::Message(format!("Model download request failed: {err}")))?;
    let total_bytes = response
        .header("Content-Length")
        .and_then(|value| value.parse::<u64>().ok());

    let mut reader = response.into_reader();
    let mut writer = BufWriter::new(File::create(destination)?);
    let mut buffer = [0u8; 256 * 1024];
    let started_at = Instant::now();
    let mut last_report = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .unwrap_or_else(Instant::now);
    let mut downloaded = 0u64;

    loop {
        let bytes_read = reader.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }

        writer.write_all(&buffer[..bytes_read])?;
        downloaded += bytes_read as u64;

        if last_report.elapsed() >= Duration::from_millis(250) {
            let elapsed = started_at.elapsed();
            emit_status(
                status_callback,
                format_download_progress(downloaded, total_bytes, elapsed),
            );
            emit_download_progress(download_callback, downloaded, total_bytes, elapsed);
            last_report = Instant::now();
        }
    }

    writer.flush()?;
    let elapsed = started_at.elapsed();
    emit_status(
        status_callback,
        format_download_progress(downloaded, total_bytes, elapsed),
    );
    emit_download_progress(download_callback, downloaded, total_bytes, elapsed);
    Ok(())
}

fn emit_download_progress(
    download_callback: Option<&ModelDownloadCallback>,
    downloaded: u64,
    total_bytes: Option<u64>,
    elapsed: Duration,
) {
    let Some(callback) = download_callback else {
        return;
    };

    let fraction = total_bytes.and_then(|total| {
        (total > 0).then(|| (downloaded as f64 / total as f64).clamp(0.0, 1.0))
    });

    let speed = downloaded as f64 / elapsed.as_secs_f64().max(0.001);
    let eta = total_bytes.and_then(|total| {
        let remaining = total.saturating_sub(downloaded) as f64;
        (speed > 0.0 && downloaded > 0)
            .then(|| format_eta(Duration::from_secs_f64(remaining / speed)))
    });

    callback(ModelDownloadProgress {
        fraction,
        downloaded_bytes: downloaded,
        eta,
    });
}

fn format_download_progress(
    downloaded: u64,
    total_bytes: Option<u64>,
    elapsed: Duration,
) -> String {
    match total_bytes {
        Some(total_bytes) if total_bytes > 0 => {
            let progress = (downloaded as f64 / total_bytes as f64 * 100.0).clamp(0.0, 100.0);
            let speed = downloaded as f64 / elapsed.as_secs_f64().max(0.001);
            let remaining_bytes = total_bytes.saturating_sub(downloaded) as f64;
            let eta = if speed > 0.0 {
                format_eta(Duration::from_secs_f64(remaining_bytes / speed))
            } else {
                "--".into()
            };
            format!("Downloading model: {:.0}% · ETA {eta}", progress)
        }
        _ => format!(
            "Downloading model: {:.1} MB",
            downloaded as f64 / 1_048_576.0
        ),
    }
}

fn format_eta(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let minutes = seconds / 60;
    let seconds = seconds % 60;
    if minutes > 0 {
        format!("{minutes}:{seconds:02}")
    } else {
        format!("{seconds}s")
    }
}

fn strip_trailing_subtitle_credit(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let without_trailing_sentence_punctuation =
        trimmed.trim_end_matches(|ch: char| matches!(ch, '.' | '!' | '?') || ch.is_whitespace());

    let sentence_start = without_trailing_sentence_punctuation
        .char_indices()
        .rev()
        .find(|(_, ch)| matches!(ch, '.' | '!' | '?' | '\n'))
        .map(|(index, ch)| index + ch.len_utf8())
        .unwrap_or(0);

    let last_sentence_lower = without_trailing_sentence_punctuation[sentence_start..]
        .trim_start()
        .to_lowercase();
    if !last_sentence_lower.contains("titulky vytvořil ") {
        return trimmed.to_string();
    }

    trimmed[..sentence_start].trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::strip_trailing_subtitle_credit;

    #[test]
    fn strips_credit_with_trailing_period() {
        assert_eq!(
            strip_trailing_subtitle_credit("Ahoj. Titulky vytvořil JohnyX."),
            "Ahoj."
        );
    }

    #[test]
    fn strips_credit_without_trailing_period() {
        assert_eq!(
            strip_trailing_subtitle_credit("Ahoj. Titulky vytvořil JohnyX"),
            "Ahoj."
        );
    }

    #[test]
    fn strips_credit_when_it_is_only_sentence() {
        assert_eq!(
            strip_trailing_subtitle_credit("Titulky vytvořil JohnyX."),
            ""
        );
    }

    #[test]
    fn leaves_other_last_sentence_untouched() {
        assert_eq!(
            strip_trailing_subtitle_credit("Ahoj. To je vsechno."),
            "Ahoj. To je vsechno."
        );
    }

    #[test]
    fn strips_credit_case_insensitively() {
        assert_eq!(
            strip_trailing_subtitle_credit("Ahoj.\nTITULKY VYTVOŘIL JohnyX"),
            "Ahoj."
        );
    }
}

#[cfg(target_os = "macos")]
fn launch_agent_plist_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| AppError::Message("HOME is not set, cannot resolve LaunchAgents.".into()))?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{APP_IDENTIFIER}.plist")))
}

#[cfg(target_os = "macos")]
fn launch_agent_plist_contents(executable: &PathBuf) -> String {
    let executable = xml_escape(&executable.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{APP_IDENTIFIER}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{executable}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <false/>
</dict>
</plist>
"#
    )
}

#[cfg(target_os = "macos")]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn extract_whisper_samples_from_wav(audio_data: Vec<u8>) -> Result<Vec<f32>> {
    let cursor = std::io::Cursor::new(audio_data);
    let mut reader = hound::WavReader::new(cursor)?;
    let spec = reader.spec();
    let channels = spec.channels as usize;

    let samples_f32 = match spec.sample_format {
        hound::SampleFormat::Int => match spec.bits_per_sample {
            16 => reader
                .samples::<i16>()
                .map(|sample| sample.map(|value| value as f32 / 32768.0))
                .collect::<std::result::Result<Vec<_>, _>>()?,
            32 => reader
                .samples::<i32>()
                .map(|sample| sample.map(|value| value as f32 / 2147483648.0))
                .collect::<std::result::Result<Vec<_>, _>>()?,
            other => {
                return Err(AppError::Message(format!(
                    "Unsupported WAV bit depth: {other}"
                )));
            }
        },
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<std::result::Result<Vec<_>, _>>()?,
    };

    let mono_samples = if channels <= 1 {
        samples_f32
    } else {
        samples_f32
            .chunks_exact(channels)
            .map(|chunk| chunk.iter().sum::<f32>() / channels as f32)
            .collect()
    };

    if spec.sample_rate == 16000 {
        return Ok(mono_samples);
    }

    let resample_ratio = 16000.0 / spec.sample_rate as f64;
    if resample_ratio > 8.0 {
        return Err(AppError::Message(format!(
            "Input sample rate {} Hz is too low for resampling.",
            spec.sample_rate
        )));
    }

    let chunk_size = 1024;
    let params = SincInterpolationParameters {
        sinc_len: 64,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 128,
        window: WindowFunction::BlackmanHarris2,
    };
    let mut resampler = SincFixedIn::<f32>::new(resample_ratio, 8.0, params, chunk_size, 1)
        .map_err(|e| AppError::Message(format!("Failed to create resampler: {e}")))?;

    let expected_output_len = (mono_samples.len() as f64 * resample_ratio).round() as usize;
    let mut output_samples = Vec::with_capacity(expected_output_len);
    let mut input_pos = 0;

    while input_pos < mono_samples.len() {
        let end_pos = (input_pos + chunk_size).min(mono_samples.len());
        let mut chunk = mono_samples[input_pos..end_pos].to_vec();
        if chunk.len() < chunk_size {
            chunk.resize(chunk_size, 0.0);
        }
        let waves_out = resampler
            .process(&vec![chunk], None)
            .map_err(|e| AppError::Message(format!("Resampling failed: {e}")))?;
        output_samples.extend_from_slice(&waves_out[0]);
        input_pos += chunk_size;
    }

    output_samples.truncate(expected_output_len);
    Ok(output_samples)
}
