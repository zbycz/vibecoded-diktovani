//! Quick test: transcribe a WAV file and observe progress callback frequency.
//! Usage: cargo run --example test_progress -- <path-to.wav>

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::thread;

fn main() {
    let wav_path = std::env::args().nth(1).expect("Usage: test_progress <wav-file>");
    let model_path = format!("{}/.cache/diktovani/ggml-large-v3-turbo.bin",
        std::env::var("HOME").unwrap());

    println!("[0.0s] loading model from {}", model_path);
    let start = Instant::now();

    let mut ctx_params = whisper_rs::WhisperContextParameters::default();
    ctx_params.use_gpu = true;
    let ctx = whisper_rs::WhisperContext::new_with_params(&model_path, ctx_params)
        .expect("failed to load model");
    println!("[{:.1}s] model loaded", start.elapsed().as_secs_f32());

    let audio_data = std::fs::read(&wav_path).expect("failed to read wav");
    let samples = extract_samples(&audio_data);
    let audio_secs = samples.len() as f32 / 16000.0;
    println!("[{:.1}s] {} samples = {:.1}s of audio", start.elapsed().as_secs_f32(), samples.len(), audio_secs);

    let mut state = ctx.create_state().unwrap();
    let mut params = whisper_rs::FullParams::new(whisper_rs::SamplingStrategy::BeamSearch {
        beam_size: 3,
        patience: -1.0,
    });
    params.set_language(Some("cs"));
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);

    println!("[{:.1}s] starting inference (no progress callback - set_progress_callback_safe has dangling-ptr bug in whisper-rs 0.13.2)...", start.elapsed().as_secs_f32());
    state.full(params, &samples).unwrap();
    println!("[{:.1}s] inference done in {:.1}s",
        start.elapsed().as_secs_f32(),
        start.elapsed().as_secs_f32() - 2.5);

    let n = state.full_n_segments().unwrap();
    let mut text = String::new();
    for i in 0..n { text.push_str(&state.full_get_segment_text(i).unwrap()); }
    println!("Transcript: {}", text.trim());
}

fn extract_samples(data: &[u8]) -> Vec<f32> {
    // Skip 44-byte WAV header, read i16 LE samples, convert to f32
    if data.len() < 44 { return vec![]; }
    let pcm = &data[44..];
    pcm.chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
        .collect()
}
