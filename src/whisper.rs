use std::path::Path;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

pub struct WhisperModel {
    context: WhisperContext,
}

impl WhisperModel {
    pub fn load(model_path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let mut ctx_params = WhisperContextParameters::default();
        ctx_params.use_gpu = true;
        let context = WhisperContext::new_with_params(
            model_path.to_str().ok_or("model path is not valid UTF-8")?,
            ctx_params,
        )?;
        Ok(Self { context })
    }

    /// Transcribe audio samples (16 kHz, mono, f32).
    ///
    /// `on_progress` receives values 0–100 reporting the fraction of audio
    /// whisper has decoded so far, emitted as decoding advances.
    pub fn transcribe(
        &self,
        samples: &[f32],
        language: Option<&str>,
        on_progress: Option<Box<dyn FnMut(u8) + 'static>>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let mut state = self.context.create_state()?;

        let mut params = FullParams::new(SamplingStrategy::BeamSearch {
            beam_size: 3,
            patience: -1.0,
        });
        params.set_language(language);
        params.set_translate(false);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_suppress_blank(true);
        params.set_suppress_non_speech_tokens(true);
        params.set_no_speech_thold(0.2);

        if let Some(mut progress_cb) = on_progress {
            params.set_progress_callback_safe(move |progress: i32| {
                progress_cb(progress.clamp(0, 100) as u8);
            });
        }

        state.full(params, samples)?;

        let n = state.full_n_segments()?;
        let mut text = String::new();
        for i in 0..n {
            text.push_str(&state.full_get_segment_text(i)?);
        }
        Ok(text)
    }
}
