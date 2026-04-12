#[cfg(target_os = "macos")]
use accessibility_sys::{
    AXIsProcessTrusted, AXIsProcessTrustedWithOptions, kAXTrustedCheckOptionPrompt,
};
use arboard::Clipboard;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, Stream};
#[cfg(target_os = "macos")]
use core_foundation::base::TCFType;
#[cfg(target_os = "macos")]
use core_foundation::boolean::CFBoolean;
#[cfg(target_os = "macos")]
use core_foundation::dictionary::CFDictionary;
#[cfg(target_os = "macos")]
use core_foundation::string::CFString;
use enigo::{Direction, Enigo, Key, Keyboard, Settings};
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};
use thiserror::Error;
use transcribe_rs::TranscriptionEngine;
use transcribe_rs::engines::whisper::{WhisperEngine, WhisperInferenceParams};

pub const MODEL_URL: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo.bin";
pub const MODEL_FILENAME: &str = "ggml-large-v3-turbo.bin";
pub const LANGUAGE: &str = "cs";

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
                    eprintln!("Failed to build input stream: {err}");
                    return;
                }
            };

            if let Err(err) = stream.play() {
                eprintln!("Failed to start stream: {err}");
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
    let err_fn = |err| eprintln!("Audio stream error: {err}");

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
    Whisper(WhisperEngine),
}

impl Engine {
    fn unload(&mut self) {
        let Self::Whisper(engine) = self;
        engine.unload_model();
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
            let mut engine = WhisperEngine::new();
            engine
                .load_model(&model_path)
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

    pub fn preload_whisper(&self) -> Result<()> {
        let started_at = Instant::now();
        let model_path = ensure_model_available()?;
        println!(
            "[preload] starting Whisper preload from {}",
            model_path.display()
        );

        let (_, loaded_now) = self.get_or_load_whisper(model_path)?;
        let elapsed = started_at.elapsed();

        if loaded_now {
            println!(
                "[preload] Whisper model loaded in {:.2}s",
                elapsed.as_secs_f32()
            );
        } else {
            println!(
                "[preload] Whisper model already warm (checked in {:.2}s)",
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
    unsafe { AXIsProcessTrusted() }

    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

#[cfg(target_os = "macos")]
fn prompt_accessibility_permission() {
    let key = unsafe { CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt) };
    let value = CFBoolean::true_value();
    let options = CFDictionary::from_CFType_pairs(&[(key.as_CFType(), value.as_CFType())]);

    unsafe {
        let _ = AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef());
    }

    if let Err(err) =
        Command::new("open").arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility").status()
    {
        eprintln!("[accessibility] failed to open System Settings: {err}");
    }
}

#[cfg(not(target_os = "macos"))]
fn prompt_accessibility_permission() {}

pub fn copy_and_paste_text(text: &str) -> Result<()> {
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

    thread::sleep(Duration::from_millis(100));

    if let Some(original_clipboard) = original_clipboard
        && let Err(err) = clipboard.set_text(original_clipboard)
    {
        eprintln!("[clipboard] failed to restore original clipboard text: {err}");
    }

    Ok(())
}

pub fn transcribe_wav_file(model_manager: &ModelManager, file_path: &PathBuf) -> Result<String> {
    let total_started_at = Instant::now();
    println!("[transcribe] reading audio from {}", file_path.display());
    let audio_data = std::fs::read(file_path)?;

    let sample_extract_started_at = Instant::now();
    let samples = extract_whisper_samples_from_wav(audio_data)?;
    println!(
        "[transcribe] prepared {} samples in {:.2}s",
        samples.len(),
        sample_extract_started_at.elapsed().as_secs_f32()
    );
    if samples.is_empty() {
        println!(
            "[transcribe] no samples found, finishing in {:.2}s",
            total_started_at.elapsed().as_secs_f32()
        );
        return Ok(String::new());
    }

    let model_ready_started_at = Instant::now();
    let model_path = ensure_model_available()?;
    let (engine_arc, loaded_now) = model_manager.get_or_load_whisper(model_path.clone())?;
    println!(
        "[transcribe] model {} in {:.2}s ({})",
        if loaded_now {
            "loaded on demand"
        } else {
            "was already warm"
        },
        model_ready_started_at.elapsed().as_secs_f32(),
        model_path.display()
    );

    let mut params = WhisperInferenceParams::default();
    params.language = Some(LANGUAGE.to_string());
    params.print_special = false;
    params.print_progress = false;
    params.print_realtime = false;
    params.print_timestamps = false;
    params.suppress_blank = true;
    params.suppress_non_speech_tokens = true;
    params.no_speech_thold = 0.2;

    let mut engine_guard = engine_arc
        .lock()
        .map_err(|e| AppError::Message(format!("Engine mutex poisoned: {e}")))?;
    let engine = engine_guard
        .as_mut()
        .ok_or_else(|| AppError::Message("Whisper model is not loaded.".into()))?;
    let Engine::Whisper(whisper_engine) = engine;

    let inference_started_at = Instant::now();
    let result = whisper_engine
        .transcribe_samples(samples, Some(params))
        .map_err(|e| AppError::Message(format!("Transcription failed: {e}")))?;
    let transcript = result.text.trim().to_string();

    println!(
        "[transcribe] inference finished in {:.2}s, transcript chars={}, total {:.2}s",
        inference_started_at.elapsed().as_secs_f32(),
        transcript.len(),
        total_started_at.elapsed().as_secs_f32()
    );

    Ok(transcript)
}

fn ensure_model_available() -> Result<PathBuf> {
    let model_path = cache_model_path()?;

    if let Ok(metadata) = std::fs::metadata(&model_path)
        && metadata.len() > 0
    {
        println!("[model] using cached model {}", model_path.display());
        return Ok(model_path);
    }

    let cache_dir = model_path
        .parent()
        .ok_or_else(|| AppError::Message("Model cache path is missing a parent directory.".into()))?;
    std::fs::create_dir_all(cache_dir)?;

    let partial_path = cache_dir.join(format!("{MODEL_FILENAME}.partial"));
    if partial_path.exists() {
        std::fs::remove_file(&partial_path)?;
    }

    let started_at = Instant::now();
    println!(
        "[model] downloading Whisper model from {} to {}",
        MODEL_URL,
        model_path.display()
    );

    let status = Command::new("curl")
        .args(["-L", "--fail", "--progress-bar", MODEL_URL, "-o"])
        .arg(&partial_path)
        .status()
        .map_err(|err| AppError::Message(format!("Failed to start curl for model download: {err}")))?;

    if !status.success() {
        let _ = std::fs::remove_file(&partial_path);
        return Err(AppError::Message(format!(
            "Model download failed with status {status}."
        )));
    }

    std::fs::rename(&partial_path, &model_path)?;
    println!(
        "[model] download finished in {:.2}s: {}",
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
